//! Cuts one frame's GPU work into submissions that each stay under the
//! watchdog's budget.
//!
//! A frame is one command buffer: its sources, every pass and the readback.
//! A GPU that cannot preempt a long draw resets the device when one
//! submission outruns the operating system's timeout, and the export's own
//! work is lost with it. So each pass's time is predicted from its cost
//! ([`crate::effect_cost`]), the pixels it covers and the device's rate
//! ([`alpha2_gpu::GpuRate`]), and a pass that would take the command buffer
//! past [`alpha2_gpu::submission_budget_ms`] starts a new one, each its own
//! queue submission. A single pass predicted above the budget is drawn in
//! horizontal bands, a scissor each, one submission per band.
//!
//! A frame predicted cheap is never planned: below
//! [`PLANNING_THRESHOLD_UNIT_PIXELS`] it is one command buffer, as it always
//! was, and needs no rate. No planned frame differs in its pixels either:
//! the cuts change only when the GPU starts each part.

use crate::effect_cost::{ARITHMETIC_UNITS, EffectCost, FETCH_UNITS, TRANSCENDENTAL_UNITS};

/// Frames predicted to cost at most this many unit pixels, 64 units per
/// pixel over a megapixel, are not planned: well under a millisecond on the
/// slowest GPU measured.
pub(crate) const PLANNING_THRESHOLD_UNIT_PIXELS: f64 = 64.0 * 1_048_576.0;

/// The most bands a pass is drawn in.
pub(crate) const MAX_BANDS: u32 = 64;

/// A composite layer, per covered pixel: its texture read, the
/// minification taps of a scaled-down layer, and the blend.
pub(crate) const COMPOSITE_LAYER_COST: EffectCost = EffectCost::fixed(4, 0);
/// A vector scene, per covered pixel: its coverage and paint reads.
pub(crate) const VECTOR_COST: EffectCost = EffectCost::fixed(2, 0);

/// A frame's predicted cost, in unit pixels per kind of work: the counts of
/// [`EffectCost`] times the pixels each pass covers.
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
    /// The weighted sum the rate applies to.
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
}

/// How one frame was submitted, as the renderer planned it.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct FrameSubmissions {
    /// Queue submissions the frame's command buffers went in.
    pub submissions: u32,
    /// The frame's predicted GPU time, in milliseconds, when it was planned.
    pub predicted_ms: Option<f64>,
    /// The frame's predicted cost.
    pub cost: FrameCost,
}

/// Where a pass goes: after a cut or not, and in how many bands.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct PassPlacement {
    /// The current command buffer is finished before the pass.
    pub(crate) cut_before: bool,
    /// Bands the pass is drawn in, each in a command buffer of its own.
    pub(crate) bands: u32,
}

/// The planning state of one renderer command encoder.
#[derive(Debug, Default)]
pub(crate) struct SubmissionPlanner {
    /// Milliseconds per unit pixel, and the budget, once the frame is planned.
    plan: Option<(f64, f64)>,
    /// Predicted milliseconds of the command buffer being recorded.
    current_ms: f64,
    /// Predicted milliseconds of each command buffer finished before it.
    finished_ms: Vec<f64>,
    /// The time of each band still to come of a banded pass.
    band_ms: f64,
    cost: FrameCost,
    /// A graph was encoded: this is a frame.
    frame: bool,
}

impl SubmissionPlanner {
    /// Plans the rest of the frame at `ms_per_unit_pixel` within `budget_ms`.
    pub(crate) fn plan(&mut self, ms_per_unit_pixel: f64, budget_ms: f64) {
        self.plan = Some((ms_per_unit_pixel, budget_ms.max(f64::MIN_POSITIVE)));
    }

    /// Marks the encoder as one frame's.
    pub(crate) const fn mark_frame(&mut self) {
        self.frame = true;
    }

    /// Charges a pass of `cost` per pixel over `pixels` and places it; one
    /// that is not `bandable` is never cut into bands, only given a command
    /// buffer of its own.
    pub(crate) fn pass(&mut self, cost: EffectCost, pixels: u64, bandable: bool) -> PassPlacement {
        self.cost.add(cost, pixels);
        let Some((rate, budget)) = self.plan else {
            return PassPlacement {
                cut_before: false,
                bands: 1,
            };
        };
        #[allow(clippy::cast_precision_loss)]
        let ms = cost.units() as f64 * pixels as f64 * rate;
        if ms > budget {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let bands = if bandable {
                ((ms / budget).ceil() as u32).clamp(1, MAX_BANDS)
            } else {
                1
            };
            let cut_before = self.current_ms > 0.0;
            if cut_before {
                self.finished_ms.push(self.current_ms);
            }
            self.band_ms = ms / f64::from(bands);
            self.current_ms = self.band_ms;
            return PassPlacement { cut_before, bands };
        }
        let cut_before = self.current_ms > 0.0 && self.current_ms + ms > budget;
        if cut_before {
            self.finished_ms.push(self.current_ms);
            self.current_ms = 0.0;
        }
        self.current_ms += ms;
        PassPlacement {
            cut_before,
            bands: 1,
        }
    }

    /// Accounts for the cut before the next band of a banded pass.
    pub(crate) fn next_band(&mut self) {
        self.finished_ms.push(self.current_ms);
        self.current_ms = self.band_ms;
    }

    /// Whether the frame is planned.
    pub(crate) const fn is_planned(&self) -> bool {
        self.plan.is_some()
    }

    /// Whether the encoder recorded a frame.
    pub(crate) const fn is_frame(&self) -> bool {
        self.frame
    }

    /// The predicted cost charged so far.
    pub(crate) const fn cost(&self) -> FrameCost {
        self.cost
    }

    /// Each command buffer's predicted milliseconds, the last one included,
    /// when the frame was planned.
    pub(crate) fn buffer_ms(&self) -> Option<Vec<f64>> {
        self.plan.map(|_| {
            let mut buffers = self.finished_ms.clone();
            buffers.push(self.current_ms);
            buffers
        })
    }
}

/// The side of the square the calibration draw covers.
const CALIBRATION_SIDE: u32 = 512;

/// The calibration draw: 64 iterations of one offset texture read and one
/// sine, so its cost is near what heavy effects spend per unit.
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

impl crate::Renderer {
    /// Plans the frame `encoder` records when `cost` (the graph's passes,
    /// predicted) is past [`PLANNING_THRESHOLD_UNIT_PIXELS`]: at the device's
    /// rate, which the first such frame calibrates. A browser cannot wait for
    /// the calibration, so its frames meanwhile assume
    /// [`alpha2_gpu::UNCALIBRATED_MS_PER_UNIT_PIXEL`].
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
        if rate.ms_per_unit_pixel().is_none()
            && rate.begin_calibration()
            && self.calibrate().is_err()
        {
            rate.abandon_calibration();
        }
        let ms_per_unit_pixel = rate
            .ms_per_unit_pixel()
            .unwrap_or(alpha2_gpu::UNCALIBRATED_MS_PER_UNIT_PIXEL);
        encoder.planner().plan(
            ms_per_unit_pixel,
            f64::from(alpha2_gpu::submission_budget_ms()),
        );
    }

    /// Draws the calibration effect twice over a 512 x 512 target, each in a
    /// submission of its own, and seeds the device's rate when they complete.
    ///
    /// Natively each draw is waited for, and the second, on a warm device,
    /// alone is timed: completions seen in one poll arrive together, and the
    /// clock would give the second none of their span. A browser cannot wait,
    /// so there the two draws' summed span, halved, seeds it: as safe, since
    /// the first pays for the cold device.
    fn calibrate(&self) -> Result<(), crate::RenderError> {
        let effect = self
            .calibration
            .get_or_init(|| crate::registry::registered_effect(calibration_definition()));
        let id = crate::EffectId::new(effect.definition.id.clone(), effect.definition.version);
        let size = crate::RenderSize::new(CALIBRATION_SIDE, CALIBRATION_SIDE)?;
        let format = self.intermediate_format();
        let params = crate::EffectParams::new();
        let resources = crate::EffectResources::new();
        let mut first = None;
        let mut last = None;
        for _ in 0..2 {
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
            self.gpu
                .device()
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(submitted.index.clone()),
                    timeout: Some(self.gpu_wait_timeout),
                })
                .map_err(crate::error::poll_error)?;
            first.get_or_insert(submitted.sequences.start);
            last = Some(submitted);
        }
        let submitted = last.expect("two calibration draws");
        #[cfg(not(target_arch = "wasm32"))]
        let (sequences, draws) = (submitted.sequences.clone(), 1.0);
        #[cfg(target_arch = "wasm32")]
        let (sequences, draws) = (
            first.unwrap_or(submitted.sequences.start)..submitted.sequences.end,
            2.0,
        );
        #[allow(clippy::cast_precision_loss)]
        let unit_pixels =
            effect.cost.units() as f64 * f64::from(CALIBRATION_SIDE) * f64::from(CALIBRATION_SIDE);
        let clock = std::sync::Arc::clone(&self.submission_clock);
        let rate = std::sync::Arc::clone(self.gpu.rate());
        let seed = move || match clock.gpu_ms(sequences) {
            Some(ms) => rate.seed(ms / draws / unit_pixels),
            None => rate.abandon_calibration(),
        };
        // Natively both draws were waited for: their completions have run.
        #[cfg(not(target_arch = "wasm32"))]
        seed();
        #[cfg(target_arch = "wasm32")]
        self.gpu.on_submitted_work_done(seed);
        Ok(())
    }
}

/// The scissor rows of band `band` of `bands` over a target `height` rows
/// tall. Edges fall on even rows, so no 2x2 quad a derivative is taken over
/// straddles two bands.
pub(crate) fn band_rows(band: u32, bands: u32, height: u32) -> (u32, u32) {
    let edge = |index: u32| -> u32 {
        if index >= bands {
            return height;
        }
        let row = u64::from(height) * u64::from(index) / u64::from(bands);
        u32::try_from(row).unwrap_or(height) & !1
    };
    let top = edge(band);
    (top, edge(band + 1).saturating_sub(top))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_fill_a_buffer_up_to_the_budget_and_a_heavy_one_is_banded() {
        let mut planner = SubmissionPlanner::default();
        let pass = EffectCost {
            fetches: 0,
            transcendentals: 0,
            arithmetic: 10,
        };
        // Unplanned: every pass in the one buffer.
        assert_eq!(planner.pass(pass, 100, true).bands, 1);
        assert_eq!(planner.buffer_ms(), None);
        planner.plan(1.0, 2_500.0);
        // 1000 ms each: two fit, the third starts a new buffer.
        assert!(!planner.pass(pass, 100, true).cut_before);
        assert!(!planner.pass(pass, 100, true).cut_before);
        assert!(planner.pass(pass, 100, true).cut_before);
        // 10,000 ms: four bands of 2,500 after a cut.
        let placed = planner.pass(pass, 1_000, true);
        assert_eq!(
            placed,
            PassPlacement {
                cut_before: true,
                bands: 4
            }
        );
        for _ in 1..placed.bands {
            planner.next_band();
        }
        // A fixed pass is never banded: it gets a buffer of its own.
        assert_eq!(
            planner.pass(pass, 1_000, false),
            PassPlacement {
                cut_before: true,
                bands: 1
            }
        );
        assert_eq!(
            planner.buffer_ms(),
            Some(vec![
                2_000.0, 1_000.0, 2_500.0, 2_500.0, 2_500.0, 2_500.0, 10_000.0
            ])
        );
        assert!((planner.cost().unit_pixels() - 24_000.0).abs() < 1e-9);
    }

    #[test]
    fn bands_cover_every_row_once_on_even_edges() {
        for (bands, height) in [(1, 7), (3, 288), (7, 101), (64, 4320)] {
            let mut next = 0;
            for band in 0..bands {
                let (top, rows) = band_rows(band, bands, height);
                assert_eq!(top, next);
                assert_eq!(top % 2, 0);
                next = top + rows;
            }
            assert_eq!(next, height);
        }
    }
}
