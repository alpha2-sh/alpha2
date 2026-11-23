//! Splits one frame's GPU work into submissions that each stay under the watchdog budget.
//!
//! A GPU that cannot preempt a long draw resets the device when one submission exceeds the OS
//! timeout. Each pass's time is predicted from its cost ([`crate::effect_cost`]), covered pixels
//! and the device rate ([`alpha2_gpu::GpuRate`]); a pass that would push the command buffer past
//! [`alpha2_gpu::submission_budget_ms`] starts a new submission, and a single pass over budget
//! is drawn in scissored horizontal bands, one submission each.
//!
//! Frames below [`PLANNING_THRESHOLD_UNIT_PIXELS`] stay one command buffer and need no rate.
//! Splitting never changes pixels, only when the GPU starts each part.

use crate::composite::PixelRect;
use crate::effect_cost::{ARITHMETIC_UNITS, EffectCost, FETCH_UNITS, TRANSCENDENTAL_UNITS};

/// Frames at most this cost (64 units per pixel over a megapixel) are not planned; well under a
/// millisecond on the slowest GPU measured.
pub(crate) const PLANNING_THRESHOLD_UNIT_PIXELS: f64 = 64.0 * 1_048_576.0;

/// The most bands a pass is drawn in.
pub(crate) const MAX_BANDS: u32 = 64;

/// A frame's predicted cost in unit pixels per kind of work.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct FrameCost {
    /// Texture reads.
    pub fetches: f64,
    /// Transcendental calls.
    pub transcendentals: f64,
    /// Other operators and calls.
    pub arithmetic: f64,
}

impl FrameCost {
    /// Weighted sum the rate applies to.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn unit_pixels(&self) -> f64 {
        self.fetches * FETCH_UNITS as f64
            + self.transcendentals * TRANSCENDENTAL_UNITS as f64
            + self.arithmetic * ARITHMETIC_UNITS as f64
    }

    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn add(&mut self, cost: EffectCost, pixels: u64) {
        let pixels = pixels as f64;
        self.fetches += cost.fetches as f64 * pixels;
        self.transcendentals += cost.transcendentals as f64 * pixels;
        self.arithmetic += cost.arithmetic as f64 * pixels;
    }

    pub(crate) fn of(cost: EffectCost, pixels: u64) -> Self {
        let mut charge = Self::default();
        charge.add(cost, pixels);
        charge
    }

    pub(crate) fn add_cost(&mut self, other: Self) {
        self.fetches += other.fetches;
        self.transcendentals += other.transcendentals;
        self.arithmetic += other.arithmetic;
    }
}

/// How one frame was submitted, as the renderer planned it.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct FrameSubmissions {
    /// Queue submissions the frame went in.
    pub submissions: u32,
    /// Predicted GPU time in milliseconds, when planned.
    pub predicted_ms: Option<f64>,
    /// The frame's predicted cost.
    pub cost: FrameCost,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct PassPlacement {
    /// The current command buffer is finished before the pass.
    pub(crate) cut_before: bool,
    /// Pieces the pass is drawn in, each in a command buffer of its own.
    pub(crate) pieces: Pieces,
    pub(crate) pilot: Option<u32>,
}

/// A pass's region cut into `rows` bands, each cut into `columns` tiles, on even edges so no
/// 2x2 derivative quad straddles two pieces.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pieces {
    pub(crate) rows: u32,
    pub(crate) columns: u32,
}

impl Pieces {
    pub(crate) const ONE: Self = Self {
        rows: 1,
        columns: 1,
    };

    pub(crate) const fn count(self) -> u32 {
        self.rows.saturating_mul(self.columns)
    }

    pub(crate) fn rect(self, index: u32, region: PixelRect) -> PixelRect {
        let (y, height) = even_split(region.y, region.height, index / self.columns, self.rows);
        let (x, width) = even_split(region.x, region.width, index % self.columns, self.columns);
        PixelRect {
            x,
            y,
            width,
            height,
        }
    }
}

/// One piece of a pass: its scissor, `None` for the whole target, and whether it is the pass's
/// first piece, which alone may clear the target; later pieces load it.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Piece {
    pub(crate) scissor: Option<PixelRect>,
    pub(crate) first: bool,
}

impl Piece {
    pub(crate) const fn first(scissor: Option<PixelRect>) -> Self {
        Self {
            scissor,
            first: true,
        }
    }
}

fn even_split(start: u32, length: u32, part: u32, parts: u32) -> (u32, u32) {
    let end = start.saturating_add(length);
    let edge = |index: u32| -> u32 {
        if index == 0 {
            return start;
        }
        if index >= parts {
            return end;
        }
        let offset = u64::from(length) * u64::from(index) / u64::from(parts);
        let edge = start.saturating_add(u32::try_from(offset).unwrap_or(length)) & !1;
        edge.clamp(start, end)
    };
    let first = edge(part);
    (first, edge(part + 1).saturating_sub(first))
}

#[derive(Debug)]
struct Plan {
    rate: std::sync::Arc<alpha2_gpu::GpuRate>,
    ms_per_unit_pixel: f64,
    budget_ms: f64,
}

#[derive(Debug, Default)]
pub(crate) struct SubmissionPlanner {
    plan: Option<Plan>,
    /// Predicted milliseconds of the command buffer being recorded.
    current_ms: f64,
    /// Predicted milliseconds of each command buffer finished before it.
    finished_ms: Vec<f64>,
    current_effects: Vec<String>,
    finished_effects: Vec<Vec<String>>,
    placed_effect: Option<String>,
    /// Time of each remaining band of a banded pass.
    band_ms: f64,
    cost: FrameCost,
    /// Set once a graph was encoded.
    frame: bool,
    pilot_rows: Option<u32>,
}

impl SubmissionPlanner {
    pub(crate) fn plan(
        &mut self,
        rate: std::sync::Arc<alpha2_gpu::GpuRate>,
        ms_per_unit_pixel: f64,
        budget_ms: f64,
    ) {
        self.plan = Some(Plan {
            rate,
            ms_per_unit_pixel,
            budget_ms: budget_ms.max(f64::MIN_POSITIVE),
        });
    }

    pub(crate) const fn mark_frame(&mut self) {
        self.frame = true;
    }

    pub(crate) const fn mark_pilot(&mut self, rows: u32) {
        self.pilot_rows = Some(rows);
    }

    /// Charges a pass doing `work` and places it, at the rate measured for `pass` when that is
    /// slower than the device. A pass that is not `bandable` only gets its own command buffer.
    pub(crate) fn pass(
        &mut self,
        work: FrameCost,
        bandable: bool,
        pass: Option<alpha2_gpu::PassKey>,
    ) -> PassPlacement {
        self.cost.add_cost(work);
        if let (Some(rows), true) = (self.pilot_rows, bandable) {
            return PassPlacement {
                cut_before: false,
                pieces: Pieces::ONE,
                pilot: Some(rows),
            };
        }
        let Some(plan) = &self.plan else {
            return PassPlacement {
                cut_before: false,
                pieces: Pieces::ONE,
                pilot: None,
            };
        };
        let budget = plan.budget_ms;
        let rate = plan.rate.planning_rate(plan.ms_per_unit_pixel, pass);
        let ms = work.unit_pixels() * rate;
        self.place(ms, budget, bandable)
    }

    fn place(&mut self, ms: f64, budget: f64, bandable: bool) -> PassPlacement {
        if ms > budget {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let pieces = if bandable {
                Pieces {
                    rows: ((ms / budget).ceil() as u32).clamp(1, MAX_BANDS),
                    columns: 1,
                }
            } else {
                Pieces::ONE
            };
            let cut_before = self.current_ms > 0.0;
            if cut_before {
                self.finish_buffer();
            }
            self.band_ms = ms / f64::from(pieces.count());
            self.current_ms = self.band_ms;
            return PassPlacement {
                cut_before,
                pieces,
                pilot: None,
            };
        }
        let cut_before = self.current_ms > 0.0 && self.current_ms + ms > budget;
        if cut_before {
            self.finish_buffer();
            self.current_ms = 0.0;
        }
        self.current_ms += ms;
        PassPlacement {
            cut_before,
            pieces: Pieces::ONE,
            pilot: None,
        }
    }

    pub(crate) fn next_band(&mut self) {
        self.finish_buffer();
        self.current_ms = self.band_ms;
        self.name_current_buffer();
    }

    fn finish_buffer(&mut self) {
        self.finished_ms.push(self.current_ms);
        self.finished_effects
            .push(std::mem::take(&mut self.current_effects));
    }

    pub(crate) fn name_placed(&mut self, effect: Option<&crate::EffectId>) {
        let unchanged = match (&self.placed_effect, effect) {
            (Some(name), Some(effect)) => names(name, effect),
            (None, None) => true,
            _ => false,
        };
        if !unchanged {
            self.placed_effect =
                effect.map(|effect| format!("{}@{}", effect.as_str(), effect.version()));
        }
        self.name_current_buffer();
    }

    fn name_current_buffer(&mut self) {
        if let Some(effect) = &self.placed_effect
            && !self.current_effects.contains(effect)
        {
            self.current_effects.push(effect.clone());
        }
    }

    pub(crate) fn buffer_effects(&self) -> Vec<Vec<String>> {
        let mut buffers = self.finished_effects.clone();
        buffers.push(self.current_effects.clone());
        buffers
    }

    pub(crate) const fn is_planned(&self) -> bool {
        self.plan.is_some()
    }

    pub(crate) const fn is_frame(&self) -> bool {
        self.frame
    }

    pub(crate) const fn cost(&self) -> FrameCost {
        self.cost
    }

    pub(crate) fn planned_at(&self) -> Option<(f64, f64)> {
        self.plan
            .as_ref()
            .map(|plan| (plan.ms_per_unit_pixel, plan.budget_ms))
    }

    /// Each command buffer's predicted milliseconds, when planned.
    pub(crate) fn buffer_ms(&self) -> Option<Vec<f64>> {
        self.plan.as_ref().map(|_| {
            let mut buffers = self.finished_ms.clone();
            buffers.push(self.current_ms);
            buffers
        })
    }
}

const CALIBRATION_SIDE: u32 = 512;

/// 64 iterations of an offset texture read and a sine, close to heavy effects' per-unit cost.
const CALIBRATION_BODY: &str = "\
fn effect(uv: vec2f, src: vec4f) -> vec4f {
    var acc = src;
    for (var i = 0u; i < 64u; i++) {
        let s = sample_src(fract(uv + vec2f(f32(i) * 0.0019, f32(i) * 0.0007)));
        acc = vec4f(sin(acc.r * 1.37 + s.g), acc.g, acc.b, acc.a);
    }
    return acc;
}
";

fn calibration_definition() -> crate::EffectDef {
    crate::EffectDef {
        id: "alpha2.internal.calibrate".into(),
        version: 1,
        kind: crate::EffectKind::Effect,
        inputs: Vec::new(),
        params: Vec::new(),
        resources: Vec::new(),
        wgsl_body: CALIBRATION_BODY.into(),
        tier_min: alpha2_gpu::CapabilityTier::TierB,
        license: None,
    }
}

const CALIBRATION_DRAWS: u32 = 4;

impl crate::Renderer {
    /// Plans the frame when `cost` exceeds [`PLANNING_THRESHOLD_UNIT_PIXELS`], at the device rate
    /// calibrated by the first such frame and again when frames disagree with it. A browser
    /// cannot wait for calibration, so it assumes [`alpha2_gpu::UNCALIBRATED_MS_PER_UNIT_PIXEL`]
    /// meanwhile.
    pub(crate) fn plan_submissions(
        &self,
        encoder: &mut crate::renderer::RenderEncoder,
        cost: FrameCost,
    ) {
        encoder.planner().mark_frame();
        if cost.unit_pixels() <= PLANNING_THRESHOLD_UNIT_PIXELS {
            return;
        }
        let rate = self.gpu.rate();
        if rate.begin_calibration() && self.calibrate().is_err() {
            rate.abandon_calibration();
        }
        let ms_per_unit_pixel = rate
            .ms_per_unit_pixel()
            .unwrap_or(alpha2_gpu::UNCALIBRATED_MS_PER_UNIT_PIXEL);
        encoder.planner().plan(
            std::sync::Arc::clone(rate),
            ms_per_unit_pixel,
            f64::from(alpha2_gpu::submission_budget_ms()),
        );
    }

    /// Draws the calibration effect [`CALIBRATION_DRAWS`] times at 512x512, one submission each,
    /// and seeds the device rate on completion.
    ///
    /// Natively each draw is waited for and the fastest timed one is kept. A browser cannot
    /// wait, so it uses the mean of the summed span, cold draw included.
    fn calibrate(&self) -> Result<(), crate::RenderError> {
        let effect = self
            .calibration
            .get_or_init(|| crate::registry::registered_effect(calibration_definition()));
        let id = crate::EffectId::new(effect.definition.id.clone(), effect.definition.version);
        let size = crate::RenderSize::new(CALIBRATION_SIDE, CALIBRATION_SIDE)?;
        let format = self.intermediate_format();
        let params = crate::EffectParams::new();
        let resources = crate::EffectResources::new();
        #[allow(clippy::cast_precision_loss)]
        let unit_pixels =
            effect.cost.units() as f64 * f64::from(CALIBRATION_SIDE) * f64::from(CALIBRATION_SIDE);
        #[cfg(not(target_arch = "wasm32"))]
        let mut fastest: Option<f64> = None;
        #[cfg(target_arch = "wasm32")]
        let mut sequences: Option<std::ops::Range<u64>> = None;
        for draw in 0..CALIBRATION_DRAWS {
            let mut encoder = self.command_encoder("alpha2 calibration encoder");
            let source =
                self.textures
                    .acquire(self.gpu.device(), size, format, "alpha2 calibration source");
            let target =
                self.textures
                    .acquire(self.gpu.device(), size, format, "alpha2 calibration target");
            let mut uniforms = self.uniform_buffers.acquire(self.gpu.device(), 4096);
            self.encode_registered_pass(
                &mut encoder,
                effect,
                crate::runtime_effect::EffectPass {
                    id: &id,
                    kind: crate::RenderNodeKind::Effect,
                    source_a: source.view(),
                    source_b: None,
                    inputs: &[],
                    size,
                    params: &params,
                    resources: &resources,
                    runtime: crate::EffectRuntime::default(),
                    output: target.view(),
                },
                &mut uniforms,
            )?;
            uniforms.flush(self.gpu.queue());
            let submitted = self.submit_measured([encoder.finish()], false)?;
            uniforms.recycle();
            #[cfg(not(target_arch = "wasm32"))]
            {
                submitted
                    .submission
                    .wait(&self.gpu, self.gpu_wait_timeout)?;
                if draw > 0
                    && let Some(ms) = self.submission_clock.gpu_ms(submitted.sequences.clone())
                {
                    fastest = Some(fastest.map_or(ms, |fastest| fastest.min(ms)));
                }
            }
            #[cfg(target_arch = "wasm32")]
            {
                let _ = draw;
                sequences = Some(sequences.map_or(submitted.sequences.clone(), |sequences| {
                    sequences.start..submitted.sequences.end
                }));
            }
        }
        let rate = std::sync::Arc::clone(self.gpu.rate());
        #[cfg(not(target_arch = "wasm32"))]
        match fastest {
            Some(ms) => rate.seed(ms / unit_pixels),
            None => rate.abandon_calibration(),
        }
        #[cfg(target_arch = "wasm32")]
        {
            let clock = std::sync::Arc::clone(&self.submission_clock);
            let sequences = sequences.expect("calibration draws");
            self.gpu
                .on_submitted_work_done(move || match clock.gpu_ms(sequences) {
                    Some(ms) => rate.seed(ms / f64::from(CALIBRATION_DRAWS) / unit_pixels),
                    None => rate.abandon_calibration(),
                });
        }
        Ok(())
    }
}

fn names(name: &str, effect: &crate::EffectId) -> bool {
    name.strip_prefix(effect.as_str())
        .and_then(|rest| rest.strip_prefix('@'))
        .is_some_and(|version| version.parse() == Ok(effect.version()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(cut_before: bool, rows: u32) -> PassPlacement {
        PassPlacement {
            cut_before,
            pieces: Pieces { rows, columns: 1 },
            pilot: None,
        }
    }

    #[test]
    fn passes_fill_a_buffer_up_to_the_budget_and_a_heavy_one_is_banded() {
        let mut planner = SubmissionPlanner::default();
        let pass = EffectCost {
            fetches: 0,
            transcendentals: 0,
            arithmetic: 10,
        };
        assert_eq!(
            planner.pass(FrameCost::of(pass, 100), true, None).pieces,
            Pieces::ONE
        );
        assert_eq!(planner.buffer_ms(), None);
        planner.plan(std::sync::Arc::default(), 1.0, 2_500.0);
        // 1000 ms each: two fit, the third starts a new buffer.
        assert!(
            !planner
                .pass(FrameCost::of(pass, 100), true, None)
                .cut_before
        );
        assert!(
            !planner
                .pass(FrameCost::of(pass, 100), true, None)
                .cut_before
        );
        assert!(
            planner
                .pass(FrameCost::of(pass, 100), true, None)
                .cut_before
        );
        // 10,000 ms: four bands of 2,500 after a cut.
        let placement = planner.pass(FrameCost::of(pass, 1_000), true, None);
        assert_eq!(placement, placed(true, 4));
        for _ in 1..placement.pieces.count() {
            planner.next_band();
        }
        assert_eq!(
            planner.pass(FrameCost::of(pass, 1_000), false, None),
            placed(true, 1)
        );
        assert_eq!(
            planner.buffer_ms(),
            Some(vec![
                2_000.0, 1_000.0, 2_500.0, 2_500.0, 2_500.0, 2_500.0, 10_000.0
            ])
        );
        assert!((planner.cost().unit_pixels() - 24_000.0).abs() < 1e-9);
        assert_eq!(planner.planned_at(), Some((1.0, 2_500.0)));
    }

    #[test]
    fn an_effect_placed_again_after_a_cut_names_the_new_buffer_once() {
        let mut planner = SubmissionPlanner::default();
        planner.plan(std::sync::Arc::default(), 1.0, 2_500.0);
        let pass = EffectCost {
            fetches: 0,
            transcendentals: 0,
            arithmetic: 10,
        };
        let blur = crate::EffectId::from_static("blur", 2);
        let _ = planner.pass(FrameCost::of(pass, 100), false, None);
        planner.name_placed(Some(&blur));
        let _ = planner.pass(FrameCost::of(pass, 100), false, None);
        planner.name_placed(Some(&blur));
        let _ = planner.pass(FrameCost::of(pass, 10_000), false, None);
        planner.name_placed(Some(&blur));
        let buffers = planner.buffer_effects();
        assert!(buffers.len() > 1, "{buffers:?}");
        for buffer in buffers {
            assert_eq!(buffer, ["blur@2"]);
        }
    }

    #[test]
    fn effect_names_follow_the_command_buffer_their_pass_is_encoded_into() {
        let mut planner = SubmissionPlanner::default();
        planner.plan(std::sync::Arc::default(), 1.0, 2_500.0);
        let pass = EffectCost {
            fetches: 0,
            transcendentals: 0,
            arithmetic: 10,
        };
        let blur = crate::EffectId::from_static("blur", 2);
        let grain = crate::EffectId::from_static("grain", 1);
        let _ = planner.pass(FrameCost::of(pass, 100), true, None);
        planner.name_placed(Some(&blur));
        let _ = planner.pass(FrameCost::of(pass, 100), false, None);
        planner.name_placed(None);
        let placement = planner.pass(FrameCost::of(pass, 1_000), true, None);
        assert_eq!(placement, placed(true, 4));
        planner.name_placed(Some(&grain));
        for _ in 1..placement.pieces.count() {
            planner.next_band();
        }
        let _ = planner.pass(FrameCost::of(pass, 100), true, None);
        planner.name_placed(Some(&blur));
        let names = |names: &[&str]| -> Vec<String> {
            names.iter().map(|name| (*name).to_owned()).collect()
        };
        assert_eq!(
            planner.buffer_effects(),
            vec![
                names(&["blur@2"]),
                names(&["grain@1"]),
                names(&["grain@1"]),
                names(&["grain@1"]),
                names(&["grain@1"]),
                names(&["blur@2"]),
            ]
        );
    }

    #[test]
    fn a_pass_measured_slower_than_the_device_is_banded_at_its_own_rate() {
        let rate = std::sync::Arc::new(alpha2_gpu::GpuRate::default());
        rate.seed(0.5);
        let scattered = alpha2_gpu::PassKey::new(1, 100);
        rate.record_pass_rate(scattered, 5.0);
        let mut planner = SubmissionPlanner::default();
        planner.plan(rate, 1.0, 2_500.0);
        let pass = EffectCost {
            fetches: 0,
            transcendentals: 0,
            arithmetic: 10,
        };
        assert_eq!(
            planner.pass(FrameCost::of(pass, 100), true, None),
            placed(false, 1)
        );
        assert_eq!(
            planner.pass(FrameCost::of(pass, 100), true, Some(scattered)),
            placed(true, 4)
        );
    }

    #[test]
    fn a_pilot_draws_its_rows_alone_and_plans_nothing() {
        let mut planner = SubmissionPlanner::default();
        planner.mark_pilot(6);
        let pass = EffectCost::fixed(1, 0);
        assert_eq!(
            planner.pass(FrameCost::of(pass, 100), true, None),
            PassPlacement {
                cut_before: false,
                pieces: Pieces::ONE,
                pilot: Some(6)
            }
        );
        assert_eq!(
            planner.pass(FrameCost::of(pass, 100), false, None),
            placed(false, 1)
        );
        assert_eq!(planner.buffer_ms(), None);
    }

    #[test]
    fn pieces_cover_every_pixel_of_their_region_once_on_even_edges() {
        for (pieces, region) in [
            (Pieces::ONE, (0, 0, 7, 7)),
            (
                Pieces {
                    rows: 3,
                    columns: 1,
                },
                (0, 0, 512, 288),
            ),
            (
                Pieces {
                    rows: 7,
                    columns: 1,
                },
                (0, 0, 64, 101),
            ),
            (
                Pieces {
                    rows: 64,
                    columns: 1,
                },
                (0, 0, 8, 4320),
            ),
            (
                Pieces {
                    rows: 4,
                    columns: 5,
                },
                (3, 9, 101, 77),
            ),
        ] {
            let region = PixelRect {
                x: region.0,
                y: region.1,
                width: region.2,
                height: region.3,
            };
            let mut covered = vec![0_u32; (region.width * region.height) as usize];
            for index in 0..pieces.count() {
                let piece = pieces.rect(index, region);
                for (edge, start, end) in [
                    (piece.x, region.x, region.x + region.width),
                    (piece.y, region.y, region.y + region.height),
                    (piece.x + piece.width, region.x, region.x + region.width),
                    (piece.y + piece.height, region.y, region.y + region.height),
                ] {
                    assert!(edge == start || edge == end || edge % 2 == 0, "{piece:?}");
                }
                for y in piece.y..piece.y + piece.height {
                    for x in piece.x..piece.x + piece.width {
                        covered[((y - region.y) * region.width + x - region.x) as usize] += 1;
                    }
                }
            }
            assert!(covered.iter().all(|count| *count == 1), "{pieces:?}");
        }
    }
}
