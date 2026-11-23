use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::submission_planner::Piece;
use crate::{
    EffectId, EffectKind, EffectParams, EffectResources, EffectRuntime, GraphError, ParamSpec,
    ParamType, ParamValue, RenderError, RenderGraph, RenderSize, RenderedTexture, Renderer,
    uniform::{PARAM_SLOT_BYTES, RUNTIME_UNIFORM_WORDS, UniformArenaLease, param_uniform_bytes},
};

impl Renderer {
    /// Draws a registered generator at `size`.
    ///
    /// # Errors
    /// [`RenderError`] for an unknown or non-generator effect or mismatched parameters;
    /// [`RenderError::MissingEffectResource`] when it declares a resource (use
    /// [`Renderer::render_generator_with_resources`]).
    pub fn render_generator(
        &self,
        id: &EffectId,
        size: RenderSize,
        params: &EffectParams,
        runtime: EffectRuntime,
    ) -> Result<RenderedTexture, RenderError> {
        self.render_generator_with_resources(id, size, params, &EffectResources::new(), runtime)
    }

    /// Draws a registered generator at `size` with named external resources.
    ///
    /// # Errors
    /// [`RenderError`] for an unknown or non-generator effect, mismatched parameters, or a resource
    /// not bound to a registered one of its kind.
    pub fn render_generator_with_resources(
        &self,
        id: &EffectId,
        size: RenderSize,
        params: &EffectParams,
        resources: &EffectResources,
        runtime: EffectRuntime,
    ) -> Result<RenderedTexture, RenderError> {
        if self
            .effect_definition(id)
            .ok_or(RenderError::UnknownEffect)?
            .kind
            != EffectKind::Generator
        {
            return Err(RenderError::InvalidGraph);
        }
        let mut graph = RenderGraph::new(size);
        let output = graph.add_generator_with_resources(
            id.clone(),
            params.clone(),
            resources.clone(),
            runtime,
        );
        graph
            .set_output(output)
            .map_err(GraphError::into_render_error)?;
        self.execute_graph(&graph)
            .map_err(GraphError::into_render_error)
    }

    /// Encodes a registered effect's pass, split into bands (each its own command buffer) when the
    /// plan says it exceeds a submission's budget.
    pub(crate) fn encode_effect_pass(
        &self,
        encoder: &mut crate::renderer::RenderEncoder,
        registered: Option<&crate::Shared<crate::registry::RegisteredEffect>>,
        pass: EffectPass<'_>,
        uniforms: &mut UniformArenaLease,
    ) -> Result<Option<PendingEffectTiming>, RenderError> {
        let registered = registered.ok_or(RenderError::UnknownEffect)?;
        self.encode_registered_pass(encoder, registered, pass, uniforms)
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn encode_registered_pass(
        &self,
        encoder: &mut crate::renderer::RenderEncoder,
        registered: &crate::registry::RegisteredEffect,
        pass: EffectPass<'_>,
        uniforms: &mut UniformArenaLease,
    ) -> Result<Option<PendingEffectTiming>, RenderError> {
        let EffectPass {
            id,
            kind,
            source_a,
            source_b,
            inputs,
            size,
            params,
            resources,
            runtime,
            output,
        } = pass;
        let drawn = self.effect_pipeline(registered)?;
        let source_b = match (registered.definition.kind, source_b) {
            (EffectKind::Transition, Some(source)) => source,
            (EffectKind::Transition, None) => return Err(RenderError::MissingEffectInput),
            (EffectKind::Effect | EffectKind::Generator, _) => source_a,
        };
        validate_runtime(runtime)?;
        validate_params(&registered.definition.params, params)?;
        if resources.iter().any(|(name, _)| {
            !registered
                .definition
                .resources
                .iter()
                .any(|spec| spec.name == name)
        }) {
            return Err(RenderError::InvalidEffectParams);
        }
        let parameter_uniform = uniforms.write_with(
            param_uniform_bytes(registered.definition.params.len()),
            |bytes| encode_params(&registered.definition.params, params, bytes),
        )?;
        let runtime_uniform = uniforms.write(bytemuck::cast_slice(&runtime_words(
            size,
            runtime,
            self.quality_controls().max_filter_samples,
        )))?;
        // Held until the bind group is made, whatever another thread registers meanwhile.
        let bound = registered
            .definition
            .resources
            .iter()
            .map(|spec| {
                let resource = resources
                    .get(&spec.name)
                    .and_then(|id| self.effect_resource(id))
                    .ok_or(RenderError::MissingEffectResource)?;
                if resource.kind == spec.kind {
                    Ok(resource)
                } else {
                    Err(RenderError::EffectResourceKindMismatch)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut entries = arrayvec::ArrayVec::<_, { crate::registry::MAX_EFFECT_BINDINGS }>::new();
        entries.extend([
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(source_a),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(source_b),
            },
            wgpu::BindGroupEntry {
                binding: crate::registry::PARAMS_BINDING,
                resource: uniforms.dynamic_binding(parameter_uniform.size()),
            },
            wgpu::BindGroupEntry {
                binding: crate::registry::RUNTIME_BINDING,
                resource: uniforms.dynamic_binding(runtime_uniform.size()),
            },
        ]);
        for (index, resource) in bound.iter().enumerate() {
            entries.push(wgpu::BindGroupEntry {
                binding: crate::registry::resource_binding(index),
                resource: wgpu::BindingResource::TextureView(&resource.view),
            });
            entries.push(wgpu::BindGroupEntry {
                binding: crate::registry::resource_binding(index) + 1,
                resource: resource.metadata.as_entire_binding(),
            });
        }
        // An undeclared name is a bad setting; a declared but unbound one is a missing input.
        if inputs.iter().any(|(name, _)| {
            !registered
                .definition
                .inputs
                .iter()
                .any(|spec| spec.name == *name)
        }) {
            return Err(RenderError::InvalidEffectParams);
        }
        for (index, spec) in registered.definition.inputs.iter().enumerate() {
            let (_, view) = inputs
                .iter()
                .find(|(name, _)| *name == spec.name)
                .ok_or(RenderError::MissingEffectInput)?;
            entries.push(wgpu::BindGroupEntry {
                binding: crate::registry::input_binding(index),
                resource: wgpu::BindingResource::TextureView(view),
            });
        }
        let bind_group = crate::bind_group::OffsetBindGroup {
            group: self.bind_group(&wgpu::BindGroupDescriptor {
                label: Some("alpha2 runtime effect bind group"),
                layout: &drawn.bind_group_layout,
                entries: &entries,
            }),
            offsets: [
                parameter_uniform.dynamic_offset(),
                runtime_uniform.dynamic_offset(),
            ],
        };
        let pixels = u64::from(size.width) * u64::from(size.height);
        let placement = encoder.place_pass(
            crate::FrameCost::of(registered.cost, pixels),
            crate::composite::PixelRect::whole(size),
            true,
            Some(alpha2_gpu::PassKey::new(registered.pass_id, pixels)),
            Some(id),
        );
        if placement.pilot.is_some() || placement.pieces.count() > 1 {
            // Pieces read only the pass's sources, never its target, so pixels match an undivided
            // pass.
            encoder.encode_pieces(
                placement,
                crate::composite::PixelRect::whole(size),
                |encoder, piece| {
                    encode_pass(
                        encoder,
                        &drawn.pipeline,
                        &bind_group,
                        &self.linear_sampler,
                        output,
                        None,
                        piece,
                    );
                },
            );
            return Ok(None);
        }
        // One reusable probe: a query set per pass exhausts Metal counter buffers, and the failed
        // allocation silently drops the whole submission.
        let timing = self.pass_timing_epoch();
        if timing.is_some() {
            // Advance a pending timestamp map first so one slow map cannot latch `busy` on polled
            // backends.
            let _ = self.gpu.device().poll(wgpu::PollType::Poll);
        }
        let probe = timing
            .and_then(|epoch| Some((epoch, self.timestamp_probe()?)))
            .filter(|(_, probe)| probe.acquire());
        encode_pass(
            encoder,
            &drawn.pipeline,
            &bind_group,
            &self.linear_sampler,
            output,
            probe.map(|(_, probe)| &probe.queries),
            Piece::first(None),
        );
        if let Some((_, probe)) = probe {
            encoder.resolve_query_set(&probe.queries, 0..2, &probe.resolve, 0);
            encoder.copy_buffer_to_buffer(&probe.resolve, 0, &probe.readback, 0, 16);
        }
        Ok(probe.map(|(epoch, probe)| PendingEffectTiming {
            epoch,
            label: kind.pass_label(),
            readback: probe.readback.clone(),
            busy: Arc::clone(&probe.busy),
            diagnostics: self.diagnostics.clone(),
            timestamp_period: self.gpu.queue().get_timestamp_period(),
            gpu: self.gpu.clone(),
            armed: true,
        }))
    }
}

pub(crate) struct PendingEffectTiming {
    epoch: u64,
    label: &'static str,
    readback: wgpu::Buffer,
    busy: Arc<AtomicBool>,
    diagnostics: crate::diagnostics::SharedDiagnostics,
    timestamp_period: f32,
    gpu: alpha2_gpu::GpuContext,
    armed: bool,
}

impl PendingEffectTiming {
    pub(crate) fn map_after_submit(mut self) {
        let mapped = self.readback.clone();
        let busy = Arc::clone(&self.busy);
        let diagnostics = self.diagnostics.clone();
        let timestamp_period = self.timestamp_period;
        let epoch = self.epoch;
        let label = self.label;
        self.gpu.map_async(
            &self.readback.slice(..),
            wgpu::MapMode::Read,
            move |result| {
                if result.is_ok() {
                    if let Ok(bytes) = mapped.slice(..).get_mapped_range() {
                        if bytes.len() >= 16 {
                            let start =
                                u64::from_ne_bytes(bytes[0..8].try_into().unwrap_or([0; 8]));
                            let end = u64::from_ne_bytes(bytes[8..16].try_into().unwrap_or([0; 8]));
                            let elapsed = end.saturating_sub(start);
                            #[allow(
                                clippy::cast_possible_truncation,
                                clippy::cast_precision_loss,
                                clippy::cast_sign_loss
                            )]
                            let nanoseconds = (elapsed as f64 * f64::from(timestamp_period)) as u64;
                            crate::diagnostics::record_pass(
                                &diagnostics,
                                epoch,
                                label,
                                std::time::Duration::from_nanos(nanoseconds),
                                crate::TimingSource::GpuTimestamp,
                            );
                        }
                        drop(bytes);
                    }
                    mapped.unmap();
                }
                busy.store(false, Ordering::Release);
            },
        );
        self.armed = false;
        let _ = self.gpu.device().poll(wgpu::PollType::Poll);
    }
}

impl Drop for PendingEffectTiming {
    fn drop(&mut self) {
        if self.armed {
            self.busy.store(false, Ordering::Release);
        }
    }
}

#[derive(Copy, Clone)]
pub(crate) struct EffectPass<'a> {
    pub(crate) id: &'a EffectId,
    /// The node kind, which names the pass's timing.
    pub(crate) kind: crate::RenderNodeKind,
    pub(crate) source_a: &'a wgpu::TextureView,
    pub(crate) source_b: Option<&'a wgpu::TextureView>,
    pub(crate) inputs: &'a [(&'a str, &'a wgpu::TextureView)],
    pub(crate) size: RenderSize,
    pub(crate) params: &'a EffectParams<'a>,
    pub(crate) resources: &'a EffectResources,
    pub(crate) runtime: EffectRuntime,
    pub(crate) output: &'a wgpu::TextureView,
}

pub(crate) fn encode_params(
    schema: &[ParamSpec],
    supplied: &EffectParams,
    bytes: &mut [u8],
) -> Result<(), RenderError> {
    if supplied
        .names()
        .any(|name| !schema.iter().any(|param| param.name == name))
    {
        return Err(RenderError::InvalidEffectParams);
    }
    for (spec, slot) in schema
        .iter()
        .zip(bytes.as_chunks_mut::<PARAM_SLOT_BYTES>().0)
    {
        let value = supplied.get(&spec.name).unwrap_or(&spec.default);
        encode_value(spec, value, slot)?;
    }
    Ok(())
}

pub(crate) fn validate_params(
    schema: &[ParamSpec],
    supplied: &EffectParams,
) -> Result<(), RenderError> {
    if supplied
        .names()
        .any(|name| !schema.iter().any(|param| param.name == name))
    {
        return Err(RenderError::InvalidEffectParams);
    }
    for spec in schema {
        let value = supplied.get(&spec.name).unwrap_or(&spec.default);
        encode_value(spec, value, &mut [0; PARAM_SLOT_BYTES])?;
    }
    Ok(())
}

fn encode_value(
    spec: &ParamSpec,
    value: &ParamValue,
    output: &mut [u8],
) -> Result<(), RenderError> {
    match (spec.param_type, value) {
        (ParamType::Float, ParamValue::Float(value)) if valid_scalar(spec, *value) => {
            output[..4].copy_from_slice(&value.to_ne_bytes());
        }
        (ParamType::Vec2, ParamValue::Vec2(values))
            if values.iter().all(|value| valid_scalar(spec, *value)) =>
        {
            output[..4].copy_from_slice(&values[0].to_ne_bytes());
            output[4..8].copy_from_slice(&values[1].to_ne_bytes());
        }
        (ParamType::Vec4, ParamValue::Vec4(values))
            if values.iter().all(|value| valid_scalar(spec, *value)) =>
        {
            output.copy_from_slice(bytemuck::cast_slice(values));
        }
        (ParamType::Color, ParamValue::Color(values))
            if values
                .iter()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value)) =>
        {
            output.copy_from_slice(bytemuck::cast_slice(values));
        }
        (ParamType::Enum, ParamValue::Enum(value))
            if usize::try_from(*value).is_ok_and(|index| index < spec.enum_values.len()) =>
        {
            output[..4].copy_from_slice(&value.to_ne_bytes());
        }
        (ParamType::Bool, ParamValue::Bool(value)) => {
            output[..4].copy_from_slice(&u32::from(*value).to_ne_bytes());
        }
        _ => return Err(RenderError::InvalidEffectParams),
    }
    Ok(())
}

fn valid_scalar(spec: &ParamSpec, value: f32) -> bool {
    value.is_finite()
        && spec.min.is_none_or(|minimum| value >= minimum)
        && spec.max.is_none_or(|maximum| value <= maximum)
}

pub(crate) fn validate_runtime(runtime: EffectRuntime) -> Result<(), RenderError> {
    if !runtime.progress.is_finite()
        || !(0.0..=1.0).contains(&runtime.progress)
        || !runtime.ratio.is_finite()
        || runtime.ratio <= 0.0
        || !runtime.scale.is_finite()
        || runtime.scale <= 0.0
    {
        return Err(RenderError::InvalidEffectParameter);
    }
    Ok(())
}

/// Runtime uniform: resolution, `time()` seconds, `render_scale()`, progress, ratio and filter
/// budget as floats, then the tick count as two words, low first (`time_ticks()`), and the render
/// scale in 16.16 fixed point (`render_scale_fixed()`).
#[allow(clippy::cast_precision_loss)]
fn runtime_words(
    size: RenderSize,
    runtime: EffectRuntime,
    max_filter_samples: u32,
) -> [u32; RUNTIME_UNIFORM_WORDS] {
    let ticks = runtime.time.ticks().cast_unsigned();
    // Rounded once from the exact quotient.
    #[allow(clippy::cast_possible_truncation)]
    let seconds = (runtime.time.ticks() as f64 / alpha2_time::TICKS_PER_SECOND as f64) as f32;
    let values: [f32; 8] = [
        size.width as f32,
        size.height as f32,
        seconds,
        runtime.scale,
        runtime.progress,
        runtime.ratio,
        max_filter_samples as f32,
        0.0,
    ];
    #[allow(clippy::cast_possible_truncation)]
    let ticks = [
        ticks as u32,
        (ticks >> 32) as u32,
        render_scale_fixed(runtime.scale),
        0,
    ];
    let mut words = [0; RUNTIME_UNIFORM_WORDS];
    for (word, value) in words
        .iter_mut()
        .zip(values.map(f32::to_bits).iter().chain(&ticks))
    {
        *word = *value;
    }
    words
}

/// `scale` in 16.16 fixed point, as `render_scale_fixed()` gives it.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn render_scale_fixed(scale: f32) -> u32 {
    (f64::from(scale) * 65_536.0).round() as u32
}

fn encode_pass(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &crate::bind_group::OffsetBindGroup<2>,
    sampler: &crate::bind_group::LinearSampler,
    output: &wgpu::TextureView,
    timestamp_queries: Option<&wgpu::QuerySet>,
    Piece { scissor, first }: Piece,
) {
    let load = if first {
        wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
    } else {
        wgpu::LoadOp::Load
    };
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("alpha2 runtime effect pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: output,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load,
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: timestamp_queries.map(|query_set| wgpu::RenderPassTimestampWrites {
            query_set,
            beginning_of_pass_write_index: Some(0),
            end_of_pass_write_index: Some(1),
        }),
        occlusion_query_set: None,
        multiview_mask: None,
    });
    pass.set_pipeline(pipeline);
    bind_group.set(&mut pass, 0);
    sampler.set(&mut pass);
    if let Some(scissor) = scissor {
        pass.set_scissor_rect(scissor.x, scissor.y, scissor.width, scissor.height);
    }
    pass.draw(0..3, 0..1);
}

/// Reusable GPU timestamp instrumentation for effect passes, one per renderer.
///
/// While one measurement is in flight, later passes fall back to CPU timing instead of
/// allocating more driver resources.
#[derive(Debug)]
pub(crate) struct TimestampProbe {
    pub(crate) queries: wgpu::QuerySet,
    pub(crate) resolve: wgpu::Buffer,
    pub(crate) readback: wgpu::Buffer,
    pub(crate) busy: Arc<AtomicBool>,
}

impl TimestampProbe {
    /// Takes the probe for one pass; `false` while a read is pending.
    pub(crate) fn acquire(&self) -> bool {
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

impl Renderer {
    fn timestamp_probe(&self) -> Option<&TimestampProbe> {
        self.timestamp_probe
            .get_or_init(|| {
                if !self.gpu.capabilities().timestamp_queries {
                    return None;
                }
                let queries = self
                    .gpu
                    .device()
                    .create_query_set(&wgpu::QuerySetDescriptor {
                        label: Some("alpha2 effect timestamps"),
                        ty: wgpu::QueryType::Timestamp,
                        count: 2,
                    });
                let resolve = self.gpu.device().create_buffer(&wgpu::BufferDescriptor {
                    label: Some("alpha2 effect timestamp resolve"),
                    size: 16,
                    usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                });
                let readback = self.gpu.device().create_buffer(&wgpu::BufferDescriptor {
                    label: Some("alpha2 effect timestamp readback"),
                    size: 16,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                Some(TimestampProbe {
                    queries,
                    resolve,
                    readback,
                    busy: Arc::new(AtomicBool::new(false)),
                })
            })
            .as_ref()
    }
}
