use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{
    EffectId, EffectKind, EffectParams, EffectResources, EffectRuntime, GraphError, ParamSpec,
    ParamType, ParamValue, RenderError, RenderGraph, RenderSize, RenderedTexture, Renderer,
    uniform::{
        PARAM_SLOT_BYTES, RUNTIME_UNIFORM_WORDS, UniformArenaLease, f32_bytes, param_uniform_bytes,
    },
};

impl Renderer {
    /// Draws a registered generator at `size`.
    ///
    /// # Errors
    /// Returns [`RenderError`] when the generator is unknown, is not a
    /// generator, or supplied parameters do not match the registered schema;
    /// [`RenderError::MissingEffectResource`] when it declares a resource,
    /// which [`Renderer::render_generator_with_resources`] binds.
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
    /// Returns [`RenderError`] when the generator is unknown, is not a
    /// generator, supplied parameters do not match the registered schema, or
    /// a declared resource is not bound to a registered one of its kind.
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

    #[allow(clippy::too_many_lines)]
    pub(crate) fn encode_effect_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
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
        let registered = self.effects.get(id).ok_or(RenderError::UnknownEffect)?;
        let drawn = self.effect_pipeline(registered);
        let source_b = match (registered.definition.kind, source_b) {
            (EffectKind::Transition, Some(source)) => source,
            (EffectKind::Transition, None) => return Err(RenderError::MissingEffectInput),
            (EffectKind::Effect | EffectKind::Generator, _) => source_a,
        };
        validate_runtime(runtime)?;
        let parameter_bytes = encode_params(&registered.definition.params, params)?;
        if resources.iter().any(|(name, _)| {
            !registered
                .definition
                .resources
                .iter()
                .any(|spec| spec.name == name)
        }) {
            return Err(RenderError::InvalidEffectParams);
        }
        let parameter_uniform = uniforms.write(&parameter_bytes);
        let runtime_uniform = uniforms.write(&runtime_bytes(
            size,
            runtime,
            self.quality_controls().max_filter_samples,
        ));
        let mut entries = vec![
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(source_a),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(source_b),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(&drawn.sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: uniforms.binding(parameter_uniform),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: uniforms.binding(runtime_uniform),
            },
        ];
        for (index, spec) in registered.definition.resources.iter().enumerate() {
            let id = resources
                .get(&spec.name)
                .ok_or(RenderError::MissingEffectResource)?;
            let resource = self
                .effect_resources
                .get(id)
                .ok_or(RenderError::MissingEffectResource)?;
            if resource.kind != spec.kind {
                return Err(RenderError::EffectResourceKindMismatch);
            }
            entries.push(wgpu::BindGroupEntry {
                binding: u32::try_from(index).unwrap_or(0) * 2 + 5,
                resource: wgpu::BindingResource::TextureView(&resource.view),
            });
            entries.push(wgpu::BindGroupEntry {
                binding: u32::try_from(index).unwrap_or(0) * 2 + 6,
                resource: resource.metadata.as_entire_binding(),
            });
        }
        // As for resources: a name the definition does not declare is a bad setting, and a
        // declared one left unbound a missing input.
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
        let bind_group = self
            .gpu
            .device()
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("alpha2 runtime effect bind group"),
                layout: &drawn.bind_group_layout,
                entries: &entries,
            });
        // One reusable probe, never one per pass: a query set per pass exhausts driver counter
        // buffers (Metal), and the failed allocation silently drops the whole submission.
        let timing = self.pass_timing_epoch();
        if timing.is_some() {
            // Advance a prior timestamp mapping first, so one slow map cannot latch `busy`
            // forever on explicitly polled backends.
            let _ = self.gpu.device().poll(wgpu::PollType::Poll);
        }
        let probe = timing
            .and_then(|epoch| Some((epoch, self.timestamp_probe()?)))
            .filter(|(_, probe)| probe.acquire());
        encode_pass(
            encoder,
            &drawn.pipeline,
            &bind_group,
            output,
            probe.map(|(_, probe)| &probe.queries),
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
    /// The timing run the pass was encoded in.
    epoch: u64,
    /// What the pass drew: an effect, a transition, or a generator.
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
    /// The graph node kind the pass draws, which names its timing.
    pub(crate) kind: crate::RenderNodeKind,
    pub(crate) source_a: &'a wgpu::TextureView,
    pub(crate) source_b: Option<&'a wgpu::TextureView>,
    /// The named inputs the node binds, by the names it gives them.
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
) -> Result<Vec<u8>, RenderError> {
    if supplied
        .names()
        .any(|name| !schema.iter().any(|param| param.name == name))
    {
        return Err(RenderError::InvalidEffectParams);
    }
    let mut bytes = vec![0; param_uniform_bytes(schema.len())];
    for (spec, slot) in schema
        .iter()
        .zip(bytes.as_chunks_mut::<PARAM_SLOT_BYTES>().0)
    {
        let value = supplied.get(&spec.name).unwrap_or(&spec.default);
        encode_value(spec, value, slot)?;
    }
    Ok(bytes)
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
            output.copy_from_slice(&f32_bytes(*values));
        }
        (ParamType::Color, ParamValue::Color(values))
            if values
                .iter()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value)) =>
        {
            output.copy_from_slice(&f32_bytes(*values));
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
    {
        return Err(RenderError::InvalidEffectParameter);
    }
    Ok(())
}

/// The runtime uniform: resolution, `time()` in seconds, progress, ratio and
/// the filter sample budget as floats, then the time's ticks as two words,
/// low first (`time_ticks()`), which tell apart every frame `time()` cannot.
#[allow(clippy::cast_precision_loss)]
fn runtime_bytes(size: RenderSize, runtime: EffectRuntime, max_filter_samples: u32) -> Vec<u8> {
    let ticks = runtime.time.ticks().cast_unsigned();
    // Rounded once, from the exact quotient.
    #[allow(clippy::cast_possible_truncation)]
    let seconds = (runtime.time.ticks() as f64 / alpha2_time::TICKS_PER_SECOND as f64) as f32;
    let values: [f32; 8] = [
        size.width as f32,
        size.height as f32,
        seconds,
        0.0,
        runtime.progress,
        runtime.ratio,
        max_filter_samples as f32,
        0.0,
    ];
    #[allow(clippy::cast_possible_truncation)]
    let words: [u32; RUNTIME_UNIFORM_WORDS - 8] = [ticks as u32, (ticks >> 32) as u32, 0, 0];
    let mut bytes = f32_bytes(values);
    bytes.extend(words.iter().flat_map(|word| word.to_ne_bytes()));
    bytes
}

fn encode_pass(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &wgpu::BindGroup,
    output: &wgpu::TextureView,
    timestamp_queries: Option<&wgpu::QuerySet>,
) {
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("alpha2 runtime effect pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: output,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
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
    pass.set_bind_group(0, bind_group, &[]);
    pass.draw(0..3, 0..1);
}

/// Reusable GPU timestamp instrumentation for effect passes.
///
/// Exactly one of these exists per renderer. Timing is diagnostic, so it is
/// sampled opportunistically: while one measurement is in flight the next pass
/// renders without timestamps and falls back to CPU submission timing rather
/// than allocating more driver resources.
#[derive(Debug)]
pub(crate) struct TimestampProbe {
    pub(crate) queries: wgpu::QuerySet,
    pub(crate) resolve: wgpu::Buffer,
    pub(crate) readback: wgpu::Buffer,
    pub(crate) busy: Arc<AtomicBool>,
}

impl TimestampProbe {
    /// Takes the probe for one pass, or reports that a read is still pending.
    pub(crate) fn acquire(&self) -> bool {
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

impl Renderer {
    /// Returns the shared probe, creating it at most once.
    fn timestamp_probe(&self) -> Option<&TimestampProbe> {
        self.timestamp_probe
            .get_or_init(|| {
                // Created at most once, and only if a caller asked for timing.
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
