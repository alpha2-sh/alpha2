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

use crate::effect_cost::{ARITHMETIC_UNITS, EffectCost, FETCH_UNITS, TRANSCENDENTAL_UNITS};

/// Frames at most this cost (64 units per pixel over a megapixel) are not planned; well under a
/// millisecond on the slowest GPU measured.
pub(crate) const PLANNING_THRESHOLD_UNIT_PIXELS: f64 = 64.0 * 1_048_576.0;

/// The most bands a pass is drawn in.
pub(crate) const MAX_BANDS: u32 = 64;

/// Per covered pixel of a composite layer: texture read, minification taps and blend.
pub(crate) const COMPOSITE_LAYER_COST: EffectCost = EffectCost::fixed(4, 0);
/// Per covered pixel of a vector scene: coverage and paint reads.
pub(crate) const VECTOR_COST: EffectCost = EffectCost::fixed(2, 0);

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
    /// Bands the pass is drawn in, each in a command buffer of its own.
    pub(crate) bands: u32,
    /// Only the first band is drawn, to measure the pass.
    pub(crate) pilot: bool,
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
    /// Time of each remaining band of a banded pass.
    band_ms: f64,
    cost: FrameCost,
    /// Set once a graph was encoded.
    frame: bool,
    pilot: bool,
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

    /// Places every pass as a pilot: one band drawn of the most.
    pub(crate) const fn mark_pilot(&mut self) {
        self.pilot = true;
    }

    /// Charges a pass of `cost` over `pixels` and places it, at the rate measured for `pass` when
    /// that is slower than the device. A pass that is not `bandable` only gets its own command
    /// buffer.
    pub(crate) fn pass(
        &mut self,
        cost: EffectCost,
        pixels: u64,
        bandable: bool,
        pass: Option<alpha2_gpu::PassKey>,
    ) -> PassPlacement {
        self.cost.add(cost, pixels);
        if self.pilot && bandable {
            return PassPlacement {
                cut_before: false,
                bands: MAX_BANDS,
                pilot: true,
            };
        }
        let Some(plan) = &self.plan else {
            return PassPlacement {
                cut_before: false,
                bands: 1,
                pilot: false,
            };
        };
        let budget = plan.budget_ms;
        let rate = plan.rate.planning_rate(plan.ms_per_unit_pixel, pass);
        #[allow(clippy::cast_precision_loss)]
        let ms = cost.units() as f64 * pixels as f64 * rate;
        self.place(ms, budget, bandable)
    }

    fn place(&mut self, ms: f64, budget: f64, bandable: bool) -> PassPlacement {
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
            return PassPlacement {
                cut_before,
                bands,
                pilot: false,
            };
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
            pilot: false,
        }
    }

    pub(crate) fn next_band(&mut self) {
        self.finished_ms.push(self.current_ms);
        self.current_ms = self.band_ms;
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

    /// The device rate and budget the frame was planned at.
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

/// Draws of the calibration effect; the first warms the device and is not timed natively.
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

/// Scissor rows of band `band` of `bands` over `height` rows. Edges are on even rows so no 2x2
/// derivative quad straddles two bands.
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

    fn placed(cut_before: bool, bands: u32) -> PassPlacement {
        PassPlacement {
            cut_before,
            bands,
            pilot: false,
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
        assert_eq!(planner.pass(pass, 100, true, None).bands, 1);
        assert_eq!(planner.buffer_ms(), None);
        planner.plan(std::sync::Arc::default(), 1.0, 2_500.0);
        // 1000 ms each: two fit, the third starts a new buffer.
        assert!(!planner.pass(pass, 100, true, None).cut_before);
        assert!(!planner.pass(pass, 100, true, None).cut_before);
        assert!(planner.pass(pass, 100, true, None).cut_before);
        // 10,000 ms: four bands of 2,500 after a cut.
        let placement = planner.pass(pass, 1_000, true, None);
        assert_eq!(placement, placed(true, 4));
        for _ in 1..placement.bands {
            planner.next_band();
        }
        assert_eq!(planner.pass(pass, 1_000, false, None), placed(true, 1));
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
        assert_eq!(planner.pass(pass, 100, true, None), placed(false, 1));
        assert_eq!(
            planner.pass(pass, 100, true, Some(scattered)),
            placed(true, 4)
        );
    }

    #[test]
    fn a_pilot_draws_one_band_of_the_most_and_plans_nothing() {
        let mut planner = SubmissionPlanner::default();
        planner.mark_pilot();
        let pass = EffectCost::fixed(1, 0);
        assert_eq!(
            planner.pass(pass, 100, true, None),
            PassPlacement {
                cut_before: false,
                bands: MAX_BANDS,
                pilot: true
            }
        );
        assert_eq!(planner.buffer_ms(), None);
        assert_eq!(crate::submission_pilot::pilot_bands(1), 1);
        assert_eq!(crate::submission_pilot::pilot_bands(7), 3);
        assert_eq!(crate::submission_pilot::pilot_bands(4320), MAX_BANDS);
        for height in [1, 2, 3, 7, 100, 127, 128, 4320] {
            let (top, rows) = band_rows(0, crate::submission_pilot::pilot_bands(height), height);
            assert_eq!(top, 0);
            assert!(rows >= height.min(2), "{height}: {rows}");
        }
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
