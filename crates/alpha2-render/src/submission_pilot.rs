//! Measures a heavy effect pass on the device, one band of it, before a frame first draws it.

use crate::submission_planner::{MAX_BANDS, PLANNING_THRESHOLD_UNIT_PIXELS};

/// Draws of a pilot band; natively the faster is kept.
const PILOT_DRAWS: u32 = 2;

/// What the rows of a pilot are predicted to take at least, so submit and wait latency do not
/// dominate what it measures.
const PILOT_PREDICTED_MS: f64 = 4.0;

/// Rows a pilot of a pass of `unit_pixels_per_row` draws over `height` rows at `ms_per_unit_pixel`:
/// enough for [`PILOT_PREDICTED_MS`], at least one of the most bands, at most a quarter of them.
pub(crate) fn pilot_rows(height: u32, unit_pixels_per_row: f64, ms_per_unit_pixel: f64) -> u32 {
    let fewest = (height / MAX_BANDS).max(2);
    let most = (height / 4).max(fewest);
    let row_ms = unit_pixels_per_row * ms_per_unit_pixel;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to the rows of a frame"
    )]
    let rows = if row_ms > 0.0 {
        (PILOT_PREDICTED_MS / row_ms)
            .ceil()
            .clamp(f64::from(fewest), f64::from(most)) as u32
    } else {
        most
    };
    rows.clamp(1, height.max(1))
}

impl crate::Renderer {
    /// Draws a pilot of each effect pass among `nodes` that is heavy enough to be planned
    /// alone and that this device has not measured at `size`, keeping what each measured.
    pub(crate) fn pilot_passes<'a>(
        &self,
        nodes: impl IntoIterator<Item = &'a crate::graph::RenderNode<'a>>,
        size: crate::RenderSize,
    ) {
        let pixels = u64::from(size.width) * u64::from(size.height);
        let mut piloted = Vec::new();
        for node in nodes {
            let (effect, kind, params, resources, runtime) = match node {
                crate::graph::RenderNode::Effect {
                    effect,
                    params,
                    resources,
                    runtime,
                    ..
                } => (
                    effect,
                    crate::RenderNodeKind::Effect,
                    params,
                    resources,
                    runtime,
                ),
                crate::graph::RenderNode::Transition {
                    effect,
                    params,
                    resources,
                    runtime,
                    ..
                } => (
                    effect,
                    crate::RenderNodeKind::Transition,
                    params,
                    resources,
                    runtime,
                ),
                crate::graph::RenderNode::Generator {
                    effect,
                    params,
                    resources,
                    runtime,
                } => (
                    effect,
                    crate::RenderNodeKind::Generator,
                    params,
                    resources,
                    runtime,
                ),
                _ => continue,
            };
            let Some(registered) = self.effects().get(effect) else {
                continue;
            };
            #[allow(clippy::cast_precision_loss)]
            let unit_pixels = registered.cost.units() as f64 * pixels as f64;
            let key = alpha2_gpu::PassKey::new(registered.pass_id, pixels);
            if unit_pixels <= PLANNING_THRESHOLD_UNIT_PIXELS
                || piloted.contains(&key)
                || self.gpu.rate().pass_rate(key).is_some()
            {
                continue;
            }
            piloted.push(key);
            let _ = self.pilot(
                &registered,
                key,
                crate::runtime_effect::EffectPass {
                    id: effect,
                    kind,
                    source_a: &self.empty_input,
                    source_b: None,
                    inputs: &[],
                    size,
                    params,
                    resources,
                    runtime: *runtime,
                    output: &self.empty_input,
                },
            );
        }
    }

    /// Draws the first rows of `pass` over scratch textures [`PILOT_DRAWS`] times and records
    /// its rate under `key`. Natively each draw starts on an idle queue and is waited for. A
    /// browser records no slowdown at once and replaces it with the mean of both draws when the
    /// first started on an idle queue.
    fn pilot(
        &self,
        registered: &crate::registry::RegisteredEffect,
        key: alpha2_gpu::PassKey,
        pass: crate::runtime_effect::EffectPass<'_>,
    ) -> Result<(), crate::RenderError> {
        let size = pass.size;
        #[allow(clippy::cast_precision_loss)]
        let unit_pixels_per_row = registered.cost.units() as f64 * f64::from(size.width);
        let rows = pilot_rows(
            size.height,
            unit_pixels_per_row,
            self.gpu
                .rate()
                .ms_per_unit_pixel()
                .unwrap_or(alpha2_gpu::UNCALIBRATED_MS_PER_UNIT_PIXEL),
        );
        let band_unit_pixels = unit_pixels_per_row * f64::from(rows);
        let alignment = u64::from(self.limits.min_uniform_buffer_offset_alignment.max(1));
        let uniform_bytes = crate::graph_execute::effect_uniform_capacity(
            registered.definition.params.len(),
            alignment,
        );
        #[cfg(not(target_arch = "wasm32"))]
        self.gpu
            .device()
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(self.gpu_wait_timeout),
            })
            .map_err(crate::error::poll_error)?;
        #[cfg(not(target_arch = "wasm32"))]
        let mut fastest: Option<f64> = None;
        #[cfg(target_arch = "wasm32")]
        let mut drawn: Option<(std::ops::Range<u64>, bool)> = None;
        for _ in 0..PILOT_DRAWS {
            let submitted = self.draw_pilot(registered, pass, rows, uniform_bytes)?;
            #[cfg(not(target_arch = "wasm32"))]
            {
                submitted
                    .submission
                    .wait(&self.gpu, self.gpu_wait_timeout)?;
                if submitted.queue_idle
                    && let Some(ms) = self.submission_clock.gpu_ms(submitted.sequences.clone())
                {
                    fastest = Some(fastest.map_or(ms, |fastest| fastest.min(ms)));
                }
            }
            #[cfg(target_arch = "wasm32")]
            {
                drawn = Some(drawn.map_or(
                    (submitted.sequences.clone(), submitted.queue_idle),
                    |(sequences, idle)| (sequences.start..submitted.sequences.end, idle),
                ));
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(ms) = fastest {
            self.gpu.rate().record_pass_rate(key, ms / band_unit_pixels);
        }
        #[cfg(target_arch = "wasm32")]
        self.gpu.rate().record_pass_rate(key, 0.0);
        #[cfg(target_arch = "wasm32")]
        if let Some((sequences, true)) = drawn {
            let clock = std::sync::Arc::clone(&self.submission_clock);
            let rate = std::sync::Arc::clone(self.gpu.rate());
            self.gpu.on_submitted_work_done(move || {
                if let Some(ms) = clock.gpu_ms(sequences) {
                    rate.record_pass_rate(key, ms / f64::from(PILOT_DRAWS) / band_unit_pixels);
                }
            });
        }
        Ok(())
    }

    /// Draws the first `rows` rows of `pass` once over scratch textures and submits it.
    fn draw_pilot(
        &self,
        registered: &crate::registry::RegisteredEffect,
        pass: crate::runtime_effect::EffectPass<'_>,
        rows: u32,
        uniform_bytes: u64,
    ) -> Result<crate::renderer::MeasuredSubmit, crate::RenderError> {
        let format = self.intermediate_format();
        let mut encoder = self.command_encoder("alpha2 pilot encoder");
        encoder.planner().mark_pilot(rows);
        let scratch = |label| {
            self.textures
                .acquire(self.gpu.device(), pass.size, format, label)
        };
        let source_a =
            (pass.kind != crate::RenderNodeKind::Generator).then(|| scratch("alpha2 pilot source"));
        let source_b = (pass.kind == crate::RenderNodeKind::Transition)
            .then(|| scratch("alpha2 pilot source"));
        let inputs = registered
            .definition
            .inputs
            .iter()
            .map(|_| scratch("alpha2 pilot input"))
            .collect::<Vec<_>>();
        let input_views = registered
            .definition
            .inputs
            .iter()
            .zip(&inputs)
            .map(|(spec, texture)| (spec.name.as_str(), texture.view()))
            .collect::<Vec<_>>();
        let target = scratch("alpha2 pilot target");
        let mut uniforms = self
            .uniform_buffers
            .acquire(self.gpu.device(), uniform_bytes);
        self.encode_registered_pass(
            &mut encoder,
            registered,
            crate::runtime_effect::EffectPass {
                source_a: source_a
                    .as_ref()
                    .map_or(&self.empty_input, |texture| texture.view()),
                source_b: source_b.as_ref().map(crate::pool::TextureLease::view),
                inputs: &input_views,
                output: target.view(),
                ..pass
            },
            &mut uniforms,
        )?;
        uniforms.flush(self.gpu.queue());
        let submitted = self.submit_measured([encoder.finish()], false)?;
        uniforms.recycle();
        Ok(submitted)
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_BANDS, PILOT_PREDICTED_MS, pilot_rows};

    #[test]
    fn a_pilot_draws_enough_rows_for_its_predicted_time_within_a_quarter_of_the_frame() {
        assert_eq!(
            pilot_rows(4320, 1.0, 1.0),
            67,
            "one of the most bands at least"
        );
        assert_eq!(pilot_rows(144, 0.1, 1.0), 36, "a quarter at most");
        assert_eq!(pilot_rows(1080, 1.0, PILOT_PREDICTED_MS / 32.0), 32);
        assert_eq!(pilot_rows(1080, 1.0, 0.0), 270);
        assert_eq!(pilot_rows(3, 1.0, 1.0), 2);
        assert_eq!(pilot_rows(1, 1.0, 1.0), 1);
        assert!(pilot_rows(4320, 1.0, 1.0) >= 4320 / MAX_BANDS);
    }
}
