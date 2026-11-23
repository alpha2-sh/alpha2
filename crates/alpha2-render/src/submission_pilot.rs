//! Measures a heavy effect pass on the device, one band of it, before a frame first draws it.

use crate::submission_planner::PLANNING_THRESHOLD_UNIT_PIXELS;

const PILOT_DRAWS: u32 = 2;
const PILOT_PREDICTED_MS: f64 = 4.0;
const FEWEST_PILOT_ROWS: u32 = 2;

pub(crate) fn pilot_rows(height: u32, unit_pixels_per_row: f64, ms_per_unit_pixel: f64) -> u32 {
    let fewest = FEWEST_PILOT_ROWS;
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
    pub(crate) fn pilot_passes<'a>(
        &self,
        nodes: impl IntoIterator<
            Item = (
                &'a crate::graph::RenderNode<'a>,
                Option<&'a crate::Shared<crate::registry::RegisteredEffect>>,
            ),
        >,
        size: crate::RenderSize,
    ) {
        let pixels = u64::from(size.width) * u64::from(size.height);
        let mut piloted = Vec::new();
        for (node, registered) in nodes {
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
            let Some(registered) = registered else {
                continue;
            };
            #[expect(
                clippy::cast_precision_loss,
                reason = "a cost in units over a frame, far below 2^52"
            )]
            let unit_pixels = registered.cost.units() as f64 * pixels as f64;
            let key = alpha2_gpu::PassKey::new(registered.pass_id, pixels);
            if unit_pixels <= PLANNING_THRESHOLD_UNIT_PIXELS
                || piloted.contains(&key)
                || self.gpu.rate().pass_rate(key).is_some()
            {
                continue;
            }
            piloted.push(key);
            let piloted_pass = self.pilot(
                registered,
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
            if piloted_pass.is_err() {
                self.gpu.rate().record_pass_rate(key, 0.0);
            }
        }
    }

    fn pilot(
        &self,
        registered: &crate::registry::RegisteredEffect,
        key: alpha2_gpu::PassKey,
        pass: crate::runtime_effect::EffectPass<'_>,
    ) -> Result<(), crate::RenderError> {
        let size = pass.size;
        #[expect(
            clippy::cast_precision_loss,
            reason = "a cost in units over a frame, far below 2^52"
        )]
        let unit_pixels_per_row = registered.cost.units() as f64 * f64::from(size.width);
        let rows = self.pilot_rows(size.height, unit_pixels_per_row);
        let uniform_bytes = crate::prepared_frame::effect_uniform_capacity(
            registered.definition.params.len(),
            self.uniform_alignment(),
        );
        let format = self.intermediate_format();
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
        self.measure_pilot(
            key,
            unit_pixels_per_row * f64::from(rows),
            rows,
            uniform_bytes,
            &mut |encoder, uniforms| {
                self.encode_registered_pass(
                    encoder,
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
                    uniforms,
                )
                .map(drop)
            },
        )
    }

    pub(crate) fn pilot_kinds(
        &self,
        size: crate::RenderSize,
        work: &[(PassKind, crate::FrameCost)],
    ) {
        for (kind, work) in work {
            let key = kind.key(size.pixels());
            if work.unit_pixels() <= PLANNING_THRESHOLD_UNIT_PIXELS
                || self.gpu.rate().pass_rate(key).is_some()
            {
                continue;
            }
            let piloted = match kind {
                PassKind::Composite => self.pilot_composite(key, size),
                PassKind::Vector => continue,
                PassKind::Conversion => self.pilot_conversion(key, size),
                PassKind::Pack => self.pilot_pack(key, size),
            };
            if piloted.is_err() {
                self.gpu.rate().record_pass_rate(key, 0.0);
            }
        }
    }

    pub(crate) fn pilot_vector(
        &self,
        scene: &crate::vector::PreparedVector,
        size: crate::RenderSize,
    ) -> Result<(), crate::RenderError> {
        let key = PassKind::Vector.key(size.pixels());
        if scene.work().unit_pixels() <= PLANNING_THRESHOLD_UNIT_PIXELS
            || self.gpu.rate().pass_rate(key).is_some()
        {
            return Ok(());
        }
        let rows = self.pilot_rows(
            size.height,
            scene.work().unit_pixels() / f64::from(size.height),
        );
        let target = self.textures.acquire(
            self.gpu.device(),
            size,
            self.intermediate_format(),
            "alpha2 pilot target",
        );
        let measured = self.measure_pilot(
            key,
            scene.pilot_work(rows),
            rows,
            scene.uniform_bytes(self.uniform_alignment()),
            &mut |encoder, uniforms| {
                self.encode_vector(encoder, scene, size, target.view(), uniforms)
                    .map(drop)
            },
        );
        if measured.is_err() {
            self.gpu.rate().record_pass_rate(key, 0.0);
        }
        measured
    }

    fn pilot_composite(
        &self,
        key: alpha2_gpu::PassKey,
        size: crate::RenderSize,
    ) -> Result<(), crate::RenderError> {
        let format = self.intermediate_format();
        let source = self
            .textures
            .acquire(self.gpu.device(), size, format, "alpha2 pilot source");
        let target = self
            .textures
            .acquire(self.gpu.device(), size, format, "alpha2 pilot target");
        let visual = crate::LayerTransform {
            position: [0.5, 0.5],
            anchor: [0.5, 0.5],
            scale: [1.0, 1.0],
            rotation_degrees: 0.0,
            opacity: 0.5,
        };
        let prepared = self.prepare_composite(std::iter::once((visual, size)), size);
        let layers = [crate::CompositeLayer {
            texture: source.view(),
            visual,
            blend_mode: crate::BlendMode::Normal,
        }];
        let rows = self.pilot_rows(
            size.height,
            prepared.work().unit_pixels() / f64::from(size.height),
        );
        let uniform_bytes =
            crate::prepared_frame::composite_uniform_capacity(&prepared, self.uniform_alignment());
        self.measure_pilot(
            key,
            prepared.work().unit_pixels() * f64::from(rows) / f64::from(size.height),
            rows,
            uniform_bytes,
            &mut |encoder, uniforms| {
                self.encode_layers_pass(
                    encoder,
                    size,
                    &layers,
                    &prepared,
                    crate::composite::CompositeTarget {
                        background: None,
                        blend_space: self.blend_space,
                        output: target.texture(),
                        output_view: target.view(),
                    },
                    uniforms,
                )
                .map(drop)
            },
        )
    }

    fn pilot_conversion(
        &self,
        key: alpha2_gpu::PassKey,
        size: crate::RenderSize,
    ) -> Result<(), crate::RenderError> {
        let plane = |size: crate::RenderSize| {
            self.textures.acquire(
                self.gpu.device(),
                size,
                wgpu::TextureFormat::R8Unorm,
                "alpha2 pilot plane",
            )
        };
        let chroma_size = crate::RenderSize {
            width: size.width.div_ceil(2),
            height: size.height.div_ceil(2),
        };
        let (y, u, v) = (plane(size), plane(chroma_size), plane(chroma_size));
        let prepared = self.prepare_pilot_conversion(
            crate::GpuVideoPlanes {
                y: y.view(),
                chroma: u.view(),
                v: Some(v.view()),
                format: alpha2_media::YuvFormat::I420,
                size,
                alpha: 1.0,
                color: Some(alpha2_media::ColorMetadata::default()),
            },
            size,
        )?;
        let (_, pending) = prepared.into_parts();
        let pending = pending.ok_or(crate::RenderError::InvalidImageData)?;
        let frame = self.frame_lock.hold();
        let ticket = pending
            .begin(&frame)
            .ok_or(crate::RenderError::InvalidImageData)?;
        let work = ticket.work();
        let rows = self.pilot_rows(size.height, work.unit_pixels() / f64::from(size.height));
        let uniform_bytes = crate::prepared_frame::source_uniform_capacity(
            std::slice::from_ref(&ticket),
            self.uniform_alignment(),
        );
        self.measure_pilot(
            key,
            work.unit_pixels() * f64::from(rows) / f64::from(size.height),
            rows,
            uniform_bytes,
            &mut |encoder, uniforms| ticket.encode(self, encoder, uniforms),
        )
    }

    fn pilot_pack(
        &self,
        key: alpha2_gpu::PassKey,
        size: crate::RenderSize,
    ) -> Result<(), crate::RenderError> {
        let format = self.intermediate_format();
        let target = crate::RenderedTexture::pooled(
            size,
            format,
            self.textures
                .acquire(self.gpu.device(), size, format, "alpha2 pilot source"),
        );
        let chroma_size = crate::RenderSize {
            width: size.width.div_ceil(2),
            height: size.height.div_ceil(2),
        };
        let plane = |size| {
            self.textures.acquire(
                self.gpu.device(),
                size,
                wgpu::TextureFormat::R8Unorm,
                "alpha2 pilot plane",
            )
        };
        let (y, u, v) = (plane(size), plane(chroma_size), plane(chroma_size));
        let luma = crate::readback::pack_luma_work(size).unit_pixels();
        let chroma = crate::readback::pack_chroma_work(size).unit_pixels();
        let rows = self.pilot_rows(size.height, (luma + chroma) / f64::from(size.height));
        let drawn = luma * f64::from(rows.min(size.height)) / f64::from(size.height)
            + chroma * f64::from(rows.min(chroma_size.height)) / f64::from(chroma_size.height);
        self.measure_pilot(
            key,
            drawn,
            rows,
            crate::readback::YUV_PACK_UNIFORM_BYTES,
            &mut |encoder, uniforms| {
                self.encode_pilot_pack(&target, [y.view(), u.view(), v.view()], encoder, uniforms)
            },
        )
    }

    fn pilot_rows(&self, height: u32, unit_pixels_per_row: f64) -> u32 {
        pilot_rows(
            height,
            unit_pixels_per_row,
            self.gpu
                .rate()
                .ms_per_unit_pixel()
                .unwrap_or(alpha2_gpu::UNCALIBRATED_MS_PER_UNIT_PIXEL),
        )
    }

    fn measure_pilot(
        &self,
        key: alpha2_gpu::PassKey,
        unit_pixels: f64,
        rows: u32,
        uniform_bytes: u64,
        encode: &mut dyn FnMut(
            &mut crate::renderer::RenderEncoder,
            &mut crate::uniform::UniformArenaLease,
        ) -> Result<(), crate::RenderError>,
    ) -> Result<(), crate::RenderError> {
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
            let mut encoder = self.command_encoder("alpha2 pilot encoder");
            encoder.planner().mark_pilot(rows);
            let mut uniforms = self
                .uniform_buffers
                .acquire(self.gpu.device(), uniform_bytes);
            encode(&mut encoder, &mut uniforms)?;
            uniforms.flush(self.gpu.queue());
            let submitted = self.submit_measured([encoder.finish()], false)?;
            uniforms.recycle();
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
            self.gpu.rate().record_pass_rate(key, ms / unit_pixels);
        }
        #[cfg(target_arch = "wasm32")]
        self.gpu.rate().record_pass_rate(key, 0.0);
        #[cfg(target_arch = "wasm32")]
        if let Some((sequences, true)) = drawn {
            let clock = std::sync::Arc::clone(&self.submission_clock);
            let rate = std::sync::Arc::clone(self.gpu.rate());
            self.gpu.on_submitted_work_done(move || {
                if let Some(ms) = clock.gpu_ms(sequences) {
                    rate.record_pass_rate(key, ms / f64::from(PILOT_DRAWS) / unit_pixels);
                }
            });
        }
        Ok(())
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum PassKind {
    Composite,
    Vector,
    Conversion,
    Pack,
}

impl PassKind {
    pub(crate) fn key(self, frame_pixels: u64) -> alpha2_gpu::PassKey {
        let id = match self {
            Self::Composite => u64::from_le_bytes(*b"composit"),
            Self::Vector => u64::from_le_bytes(*b"vectorsc"),
            Self::Conversion => u64::from_le_bytes(*b"convsrce"),
            Self::Pack => u64::from_le_bytes(*b"yuvpack_"),
        };
        alpha2_gpu::PassKey::new(id, frame_pixels)
    }
}

#[cfg(test)]
mod tests {
    use super::{PILOT_PREDICTED_MS, pilot_rows};

    #[test]
    fn a_pilot_draws_enough_rows_for_its_predicted_time_within_a_quarter_of_the_frame() {
        assert_eq!(pilot_rows(4320, 1.0, 1.0), 4);
        assert_eq!(pilot_rows(4320, 1.0, 100.0), 2, "two rows at the fewest");
        assert_eq!(pilot_rows(144, 0.1, 1.0), 36, "a quarter at most");
        assert_eq!(pilot_rows(1080, 1.0, PILOT_PREDICTED_MS / 32.0), 32);
        assert_eq!(pilot_rows(1080, 1.0, 0.0), 270);
        assert_eq!(pilot_rows(3, 1.0, 1.0), 2);
        assert_eq!(pilot_rows(1, 1.0, 1.0), 1);
    }
}
