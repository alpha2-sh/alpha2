use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use alpha2_gpu::CapabilityTier;

use crate::effect::EffectKey;
use crate::loop_bound::{BUDGET, MAX_LOOP_ITERATIONS, blank_comments, bound_for_loops};
use crate::uniform::{PARAM_SLOT_BYTES, RUNTIME_UNIFORM_WORDS, param_uniform_bytes};

use crate::{
    DiagnosticSeverity, EffectDef, EffectId, EffectKind, EffectResourceKind, InputSpec, ParamSpec,
    ParamType, ParamUnit, ParamValue, Renderer, ResourceSpec, ShaderDiagnostic, ShaderDiagnostics,
    UniformField, UniformLayout,
};

/// Effects a host may register. The built-ins, a fixed set the build
/// decides, do not count against it.
const MAX_REGISTERED_EFFECTS: usize = 256;
const MAX_WGSL_BODY_BYTES: usize = 32 * 1024;
const MAX_WGSL_NESTING: usize = 64;
/// Operator characters one statement may hold. A flat chain such as
/// `x+x+...+x` has no delimiters to nest, but it is as deep as it is long, and
/// naga lowers it by recursing once per operand: about 2,500 operands
/// overflow a 1 MiB stack (a Windows main thread, wasm32) and abort the
/// process instead of failing the registration. Engine bodies stay under 32.
const MAX_WGSL_STATEMENT_OPERATORS: usize = 256;
/// The identifier prefix of everything the generated wrapper declares.
const RESERVED_PREFIX: &str = "alpha2_";
const MAX_EFFECT_RESOURCES: usize = 8;
/// The most named inputs an effect declares. Its source pair, resources, and
/// inputs must also fit the sampled textures a device offers a shader stage
/// (16 on WebGL2-class limits), which registration checks per device.
pub const MAX_EFFECT_INPUTS: usize = 8;
/// The binding of an effect's first named input: past every resource slot a
/// definition may declare, so an input never moves another binding.
#[allow(clippy::cast_possible_truncation)]
const INPUT_BINDING_BASE: u32 = 5 + 2 * MAX_EFFECT_RESOURCES as u32;
/// A resource's `ResourceMetadata_<name>` uniform: `domain_min` and
/// `domain_max`, two `vec4f`.
const RESOURCE_METADATA_BYTES: usize = 32;

/// Scopes for every class of error that creating a shader module or a
/// pipeline can raise.
///
/// Validation alone is not enough: `wgpu` hands an error only to a scope of
/// its own class, and a backend compiler that rejects the code generated from
/// valid WGSL (Metal's for MSL, DXC or FXC for HLSL, a GL driver or `ANGLE`
/// for GLSL) reports an internal error, as a device out of memory reports its
/// own class. Uncaught, either goes to the uncaptured-error handler and the
/// effect registers with an invalid pipeline, which then invalidates every
/// submission that draws it.
struct CreationErrorScopes([wgpu::ErrorScopeGuard; 3]);

impl CreationErrorScopes {
    fn push(device: &wgpu::Device) -> Self {
        Self([
            device.push_error_scope(wgpu::ErrorFilter::Internal),
            device.push_error_scope(wgpu::ErrorFilter::OutOfMemory),
            device.push_error_scope(wgpu::ErrorFilter::Validation),
        ])
    }

    /// Closes the scopes now, in the reverse order of their pushes, and
    /// returns what they caught when the device answers.
    fn pop(self) -> ScopeErrors {
        let [internal, out_of_memory, validation] = self.0;
        let validation = validation.pop();
        let out_of_memory = out_of_memory.pop();
        let internal = internal.pop();
        Box::pin(async move {
            [validation.await, out_of_memory.await, internal.await]
                .into_iter()
                .flatten()
                .collect()
        })
    }
}

/// A device's answer still to come. Browser WebGPU answers on its own event
/// loop, so these are not `Send` there; natively they are.
#[cfg(not(target_arch = "wasm32"))]
type Answer<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;
#[cfg(target_arch = "wasm32")]
type Answer<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T>>>;

type ScopeErrors = Answer<Vec<wgpu::Error>>;

/// A runtime effect whose shader module and pipeline exist on the device,
/// with the device's verdict on them still to come: [`Renderer::compile_effect`]
/// made it, [`PendingEffect::compiled`] waits for the verdict, and
/// [`Renderer::register_compiled`] adds what passed.
///
/// It borrows nothing from the renderer, so waiting for a browser's shader
/// compiler (a GPU process round trip) leaves the renderer free to render,
/// to compile other effects, or to be dropped.
pub struct PendingEffect {
    compiled: CompiledEffect,
    compilation: Answer<wgpu::CompilationInfo>,
    shader_errors: ScopeErrors,
    pipeline_errors: ScopeErrors,
}

impl std::fmt::Debug for PendingEffect {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingEffect")
            .field("id", &self.compiled.id)
            .finish_non_exhaustive()
    }
}

impl PendingEffect {
    /// Waits for the device's verdict on the shader and the pipeline.
    ///
    /// # Errors
    /// Returns the shader's diagnostics when it failed to compile, else the
    /// errors creating its pipeline raised.
    pub async fn compiled(self) -> Result<CompiledEffect, ShaderDiagnostics> {
        let mut diagnostics = compilation_diagnostics(self.compilation.await);
        for error in self.shader_errors.await {
            diagnostics.messages.push(ShaderDiagnostic {
                severity: DiagnosticSeverity::Error,
                message: error.to_string(),
                line: None,
                column: None,
            });
        }
        // A pipeline made from a shader that failed fails too; the shader's own
        // diagnostics say why.
        if diagnostics
            .messages
            .iter()
            .any(|message| message.severity == DiagnosticSeverity::Error)
        {
            return Err(diagnostics);
        }
        let errors = self.pipeline_errors.await;
        if !errors.is_empty() {
            return Err(ShaderDiagnostics {
                messages: errors
                    .into_iter()
                    .map(|error| ShaderDiagnostic {
                        severity: DiagnosticSeverity::Error,
                        message: error.to_string(),
                        line: None,
                        column: None,
                    })
                    .collect(),
            });
        }
        Ok(self.compiled)
    }
}

/// A runtime effect the device compiled, ready for
/// [`Renderer::register_compiled`] on the renderer that compiled it.
pub struct CompiledEffect {
    device: wgpu::Device,
    id: EffectId,
    effect: RegisteredEffect,
}

impl std::fmt::Debug for CompiledEffect {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CompiledEffect")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl CompiledEffect {
    /// The identity the effect registers under.
    #[must_use]
    pub const fn id(&self) -> &EffectId {
        &self.id
    }
}

#[derive(Debug, Default)]
pub(crate) struct EffectRegistry {
    entries: HashMap<EffectId, RegisteredEffect>,
    builtins: HashSet<EffectId>,
    internal: HashSet<EffectId>,
}

impl EffectRegistry {
    /// Refuses `id` when it is registered already or the host's share of the
    /// registry is full. A built-in is counted only once it is marked one, so
    /// the built-ins registering never fill it.
    fn check_room(&self, id: &EffectId) -> Result<(), ShaderDiagnostics> {
        if self.entries.contains_key(id) {
            return Err(ShaderDiagnostics::error(format!(
                "effect {} version {} is already registered",
                id.as_str(),
                id.version()
            )));
        }
        if self.entries.len() - self.builtins.len() >= MAX_REGISTERED_EFFECTS {
            return Err(ShaderDiagnostics::error(format!(
                "effect registry is full (maximum {MAX_REGISTERED_EFFECTS})"
            )));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct RegisteredEffect {
    pub(crate) definition: EffectDef,
    pub(crate) uniform_layout: UniformLayout,
    /// Whether the body can read the frame time ([`body_reads_time`]).
    reads_time: bool,
    /// Made when a host's effect registers, its verdict awaited then; a
    /// built-in's the first time it is drawn or warmed
    /// ([`Renderer::effect_pipeline`]).
    pipeline: std::sync::OnceLock<EffectPipeline>,
}

/// What a registered effect draws with on the device.
#[derive(Debug)]
pub(crate) struct EffectPipeline {
    _shader: wgpu::ShaderModule,
    pub(crate) bind_group_layout: wgpu::BindGroupLayout,
    pub(crate) pipeline: wgpu::RenderPipeline,
    pub(crate) sampler: wgpu::Sampler,
}

impl Renderer {
    /// Validates, compiles, and registers a runtime effect definition:
    /// [`Self::compile_effect`], [`PendingEffect::compiled`], then
    /// [`Self::register_compiled`].
    ///
    /// The returned future works on native and browser WebGPU backends because
    /// compilation diagnostics are asynchronous in the WebGPU API. It holds
    /// the renderer for as long as the device takes to answer; a host that
    /// must use the renderer meanwhile calls the three steps itself.
    ///
    /// # Errors
    /// Returns schema or WGSL diagnostics without mutating the registry.
    pub async fn register_effect(
        &mut self,
        definition: EffectDef,
    ) -> Result<EffectId, ShaderDiagnostics> {
        let compiled = self.compile_effect(definition)?.compiled().await?;
        self.register_compiled(compiled)
    }

    /// Validates a runtime effect definition and creates its shader module
    /// and pipeline, without waiting for the device's verdict on them.
    ///
    /// Everything is created, and every error scope pushed and popped, within
    /// this call, so registrations that overlap never share a scope.
    ///
    /// # Errors
    /// Returns schema diagnostics, or refuses an effect already registered or
    /// a full registry, without touching the device.
    pub fn compile_effect(
        &self,
        definition: EffectDef,
    ) -> Result<PendingEffect, ShaderDiagnostics> {
        let id = self.check_registration(&definition)?;
        let device = self.gpu.device();
        let shader_scope = CreationErrorScopes::push(device);
        let shader = effect_shader(device, &definition);
        let compilation: Answer<wgpu::CompilationInfo> = Box::pin(shader.get_compilation_info());
        let shader_errors = shader_scope.pop();
        let pipeline_scope = CreationErrorScopes::push(device);
        let pipeline = effect_pipeline(device, &definition, shader, self.intermediate_format());
        let pipeline_errors = pipeline_scope.pop();
        Ok(PendingEffect {
            compiled: CompiledEffect {
                device: device.clone(),
                id,
                effect: RegisteredEffect {
                    pipeline: std::sync::OnceLock::from(pipeline),
                    ..registered_effect(definition)
                },
            },
            compilation,
            shader_errors,
            pipeline_errors,
        })
    }

    /// Validates a built-in definition and registers it without touching the
    /// device: its shader module and pipeline are made the first time it is
    /// drawn or warmed ([`Self::warmup`]). Compiling every built-in at once
    /// would cost each renderer seconds on a software adapter, most of them for
    /// effects a project never draws; the engine's own shaders are checked on
    /// every backend before they ship.
    ///
    /// # Errors
    /// Returns schema diagnostics, or refuses an effect already registered or
    /// a full registry.
    pub(crate) fn register_lazily(
        &mut self,
        definition: EffectDef,
    ) -> Result<EffectId, ShaderDiagnostics> {
        let id = self.check_registration(&definition)?;
        self.effects
            .entries
            .insert(id.clone(), registered_effect(definition));
        Ok(id)
    }

    /// Everything registering `definition` checks before it touches the
    /// device, and the identity it registers under.
    fn check_registration(&self, definition: &EffectDef) -> Result<EffectId, ShaderDiagnostics> {
        validate_definition(definition)?;
        self.validate_parameter_uniform(definition)?;
        self.validate_texture_bindings(definition)?;
        if definition.tier_min == CapabilityTier::TierA
            && self.gpu.capabilities().tier == CapabilityTier::TierB
        {
            return Err(ShaderDiagnostics::error(
                "effect requires capability tier A on a tier B adapter",
            ));
        }
        let id = EffectId::new(definition.id.clone(), definition.version);
        self.effects.check_room(&id)?;
        Ok(id)
    }

    /// What a registered effect draws with, made now when it was not yet.
    pub(crate) fn effect_pipeline<'a>(&self, effect: &'a RegisteredEffect) -> &'a EffectPipeline {
        effect.pipeline.get_or_init(|| {
            let device = self.gpu.device();
            let shader = effect_shader(device, &effect.definition);
            effect_pipeline(
                device,
                &effect.definition,
                shader,
                self.intermediate_format(),
            )
        })
    }

    /// Adds an effect this renderer compiled.
    ///
    /// # Errors
    /// Refuses an effect compiled on another device, one registered while it
    /// compiled, and a registry that filled meanwhile.
    pub fn register_compiled(
        &mut self,
        compiled: CompiledEffect,
    ) -> Result<EffectId, ShaderDiagnostics> {
        if compiled.device != *self.gpu.device() {
            return Err(ShaderDiagnostics::error(format!(
                "effect {} version {} was compiled on another GPU device",
                compiled.id.as_str(),
                compiled.id.version()
            )));
        }
        self.effects.check_room(&compiled.id)?;
        self.effects
            .entries
            .insert(compiled.id.clone(), compiled.effect);
        Ok(compiled.id)
    }

    /// Refuses parameters whose uniform this device cannot bind.
    ///
    /// Every parameter is one 16-byte slot of a single uniform binding, and a
    /// device caps a binding's size (16 KiB on the WebGL2 limits every alpha2
    /// device asks for). The pipeline compiles past the cap; it is the bind
    /// group of each frame that fails, invalidating its submission, so the
    /// registration refuses it instead.
    fn validate_parameter_uniform(&self, definition: &EffectDef) -> Result<(), ShaderDiagnostics> {
        let limit = self.limits.max_uniform_buffer_binding_size;
        let bytes = u64::try_from(param_uniform_bytes(definition.params.len())).unwrap_or(u64::MAX);
        if bytes > limit {
            let most = limit / u64::try_from(PARAM_SLOT_BYTES).unwrap_or(u64::MAX);
            return Err(ShaderDiagnostics::error(format!(
                "effect declares {} parameters, a {bytes}-byte uniform past this device's \
                 {limit}-byte uniform binding limit (at most {most} parameters)",
                definition.params.len()
            )));
        }
        Ok(())
    }

    /// Refuses a definition whose textures this device cannot bind in one
    /// shader stage: the source pair, each resource, and each named input take
    /// one sampled texture, and a device caps them (16 on the WebGL2 limits
    /// every alpha2 device asks for). Past the cap the layout fails on the
    /// device; this says why before it is asked.
    fn validate_texture_bindings(&self, definition: &EffectDef) -> Result<(), ShaderDiagnostics> {
        let textures = 2 + definition.resources.len() + definition.inputs.len();
        let limit = usize::try_from(self.limits.max_sampled_textures_per_shader_stage)
            .unwrap_or(usize::MAX);
        if textures > limit {
            return Err(ShaderDiagnostics::error(format!(
                "effect samples {textures} textures (its source pair, {} resources, and {} \
                 named inputs), past this device's {limit} per shader stage",
                definition.resources.len(),
                definition.inputs.len()
            )));
        }
        Ok(())
    }

    /// Returns the generated host uniform layout for a registered effect.
    #[must_use]
    pub fn effect_uniform_layout(&self, id: &EffectId) -> Option<&UniformLayout> {
        self.effects
            .entries
            .get(id)
            .map(|entry| &entry.uniform_layout)
    }

    /// Returns the immutable definition for a registered effect.
    #[must_use]
    pub fn effect_definition(&self, id: &EffectId) -> Option<&EffectDef> {
        self.effects.entries.get(id).map(|entry| &entry.definition)
    }

    /// Whether what `id` draws can change with the frame time alone: whether
    /// its body, or for a built-in drawn as several passes the body of any
    /// of them, can read `time()` or `time_ticks()` ([`EffectRuntime::time`]). An
    /// effect that is not registered answers `true`.
    ///
    /// The answer comes from the WGSL itself, so it cannot disagree with the
    /// shader: a body that never names `time`, `time_ticks` or `runtime`, the
    /// uniform they read, cannot reach the frame time, and every other body is
    /// taken to read it. A caller may then pass any time to an effect that
    /// answers `false` (the timeline passes zero), so its result is found
    /// again in another frame from the same inputs and parameters.
    ///
    /// [`EffectRuntime::time`]: crate::EffectRuntime::time
    #[must_use]
    pub fn effect_reads_time(&self, id: &EffectId) -> bool {
        if let Some(passes) = crate::gaussian_blur::expanded_passes(id.as_str()) {
            return passes
                .iter()
                .any(|pass| self.effect_reads_time(&EffectId::from_static(pass, 1)));
        }
        self.effects
            .entries
            .get(id)
            .is_none_or(|entry| entry.reads_time)
    }

    /// Returns whether an exact effect version has been compiled.
    #[must_use]
    pub fn is_effect_registered(&self, id: &EffectId) -> bool {
        self.effects.entries.contains_key(id)
    }

    /// Returns whether an effect may be referenced by a project document.
    #[must_use]
    pub fn is_effect_project_visible(&self, id: &EffectId) -> bool {
        self.effects.entries.contains_key(id) && !self.effects.internal.contains(id)
    }

    /// The identity a project's reference to effect `id` version `version`
    /// draws with: the one it is registered under, when a project may
    /// reference it ([`Self::is_effect_project_visible`]). Looking it up and
    /// cloning it allocate nothing, so a frame names its effects without
    /// copying their text.
    #[must_use]
    pub fn project_effect_id(&self, id: &str, version: u32) -> Option<&EffectId> {
        let key: &dyn EffectKey = &(id, version);
        self.effects
            .entries
            .get_key_value(key)
            .map(|(id, _)| id)
            .filter(|id| !self.effects.internal.contains(*id))
    }

    /// Removes one compiled runtime effect and releases its GPU resources.
    pub fn unregister_effect(&mut self, id: &EffectId) -> bool {
        if self.effects.builtins.contains(id) {
            return false;
        }
        let removed = self.effects.entries.remove(id).is_some();
        if removed {
            self.clear_result_cache_for(crate::CacheInvalidation::Effects);
        }
        removed
    }

    /// Removes every host-registered runtime effect.
    pub fn clear_effects(&mut self) {
        self.effects
            .entries
            .retain(|id, _| self.effects.builtins.contains(id));
        self.clear_result_cache_for(crate::CacheInvalidation::Effects);
    }

    /// Confirms that all requested effect versions are registered, and makes
    /// the pipelines of the built-ins among them (and of the passes a built-in
    /// draws as) not drawn yet, so a project's first frame does not wait for
    /// their compiler. A host's effect compiled when it registered.
    ///
    /// # Errors
    /// Returns a diagnostic naming the first unregistered effect.
    pub fn warmup(&self, effects: &[EffectId]) -> Result<(), ShaderDiagnostics> {
        for effect in effects {
            let Some(registered) = self.effects.get(effect) else {
                return Err(ShaderDiagnostics::error(format!(
                    "effect {} version {} is not registered",
                    effect.as_str(),
                    effect.version()
                )));
            };
            self.effect_pipeline(registered);
            for pass in crate::gaussian_blur::expanded_passes(effect.as_str()).unwrap_or_default() {
                if let Some(registered) = self.effects.get(&EffectId::from_static(pass, 1)) {
                    self.effect_pipeline(registered);
                }
            }
        }
        Ok(())
    }
}

impl EffectRegistry {
    pub(crate) fn get(&self, id: &EffectId) -> Option<&RegisteredEffect> {
        self.entries.get(id)
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn mark_builtin(&mut self, id: EffectId) {
        debug_assert!(self.entries.contains_key(&id));
        self.builtins.insert(id);
    }

    pub(crate) fn mark_internal(&mut self, id: EffectId) {
        debug_assert!(self.entries.contains_key(&id));
        self.internal.insert(id);
    }
}

/// A registry entry for `definition`, its pipeline not made yet.
fn registered_effect(definition: EffectDef) -> RegisteredEffect {
    RegisteredEffect {
        uniform_layout: uniform_layout(&definition.params),
        reads_time: body_reads_time(&definition.wgsl_body),
        definition,
        pipeline: std::sync::OnceLock::new(),
    }
}

fn effect_shader(device: &wgpu::Device, definition: &EffectDef) -> wgpu::ShaderModule {
    device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(&definition.id),
        source: wgpu::ShaderSource::Wgsl(build_shader(definition).into()),
    })
}

fn effect_pipeline(
    device: &wgpu::Device,
    definition: &EffectDef,
    shader: wgpu::ShaderModule,
    format: wgpu::TextureFormat,
) -> EffectPipeline {
    let bind_group_layout = runtime_bind_group_layout(
        device,
        &definition.params,
        &definition.resources,
        &definition.inputs,
    );
    let pipeline = runtime_pipeline(device, &bind_group_layout, &shader, format);
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("alpha2 runtime effect sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    EffectPipeline {
        _shader: shader,
        bind_group_layout,
        pipeline,
        sampler,
    }
}

fn runtime_bind_group_layout(
    device: &wgpu::Device,
    params: &[ParamSpec],
    resources: &[ResourceSpec],
    inputs: &[InputSpec],
) -> wgpu::BindGroupLayout {
    let mut entries = vec![
        runtime_texture_entry(0, wgpu::TextureViewDimension::D2),
        runtime_texture_entry(1, wgpu::TextureViewDimension::D2),
        wgpu::BindGroupLayoutEntry {
            binding: 2,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        },
        // At the sizes the generated structs take and each pass writes: a pipeline whose
        // shader reads past them fails to compile instead of reading an arena's next slot.
        runtime_uniform_entry(3, param_uniform_bytes(params.len())),
        runtime_uniform_entry(4, RUNTIME_UNIFORM_WORDS * size_of::<f32>()),
    ];
    for (index, resource) in resources.iter().enumerate() {
        let binding = u32::try_from(index).unwrap_or(0) * 2 + 5;
        let dimension = match resource.kind {
            EffectResourceKind::Lut3d => wgpu::TextureViewDimension::D3,
        };
        entries.push(runtime_texture_entry(binding, dimension));
        entries.push(runtime_uniform_entry(binding + 1, RESOURCE_METADATA_BYTES));
    }
    for index in 0..inputs.len() {
        entries.push(runtime_texture_entry(
            input_binding(index),
            wgpu::TextureViewDimension::D2,
        ));
    }
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("alpha2 runtime effect layout"),
        entries: &entries,
    })
}

/// The binding of an effect's named input `index`, in declaration order.
pub(crate) fn input_binding(index: usize) -> u32 {
    INPUT_BINDING_BASE + u32::try_from(index).unwrap_or(0)
}

fn runtime_texture_entry(
    binding: u32,
    view_dimension: wgpu::TextureViewDimension,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension,
            multisampled: false,
        },
        count: None,
    }
}

fn runtime_uniform_entry(binding: u32, bytes: usize) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: u64::try_from(bytes)
                .ok()
                .and_then(std::num::NonZeroU64::new),
        },
        count: None,
    }
}

fn runtime_pipeline(
    device: &wgpu::Device,
    bind_group_layout: &wgpu::BindGroupLayout,
    shader: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("alpha2 runtime effect pipeline layout"),
        bind_group_layouts: &[Some(bind_group_layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("alpha2 runtime effect pipeline"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("alpha2_vs"),
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("alpha2_fs"),
            targets: &[Some(format.into())],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

pub(crate) fn uniform_layout(params: &[ParamSpec]) -> UniformLayout {
    let fields = params
        .iter()
        .enumerate()
        .map(|(index, param)| UniformField {
            name: param.name.clone(),
            offset: u32::try_from(index).expect("parameter count fits u32") * 16,
            size: 16,
        })
        .collect::<Vec<_>>();
    UniformLayout {
        size: u32::try_from(param_uniform_bytes(params.len())).expect("parameter uniform fits u32"),
        fields,
    }
}

/// Whether an effect body can read the frame time. The prelude gives it
/// only through `time()` and `time_ticks()`, which read the `runtime`
/// uniform, and WGSL has no other way to reach any of them than by naming it,
/// so a body in which none of the names occurs as a word cannot read it.
/// Everything else counts as reading it, comments and a local that shadows
/// the name included: a false answer would freeze a picture that should
/// change, a true one only redraws it.
///
/// A word here is a run of letters, digits, and underscores. Every character
/// that may follow an identifier in WGSL (space, punctuation, an operator,
/// a comment) ends one, so every `time`, `time_ticks` or `runtime` token is
/// such a word.
pub(crate) fn body_reads_time(body: &str) -> bool {
    body.split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .any(|word| matches!(word, "time" | "time_ticks" | "runtime"))
}

pub(crate) fn build_shader(definition: &EffectDef) -> String {
    let param_fields = if definition.params.is_empty() {
        "    _padding: vec4f,\n".to_owned()
    } else {
        definition
            .params
            .iter()
            .fold(String::new(), |mut output, param| {
                writeln!(
                    output,
                    "    {}: {},",
                    param.name,
                    wgsl_type(param.param_type)
                )
                .expect("writing to a String cannot fail");
                output
            })
    };
    let uniform_fields =
        (0..definition.params.len().max(1)).fold(String::new(), |mut output, index| {
            writeln!(output, "    slot_{index}: vec4u,").expect("writing to a String cannot fail");
            output
        });
    let param_values = if definition.params.is_empty() {
        "vec4f(0.0)".to_owned()
    } else {
        definition
            .params
            .iter()
            .enumerate()
            .map(|(index, param)| uniform_value(index, param.param_type))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let call = match definition.kind {
        EffectKind::Effect => "effect(input.uv, sample_src(input.uv))",
        EffectKind::Transition => "alpha2_transition(input.uv)",
        EffectKind::Generator => "generate(input.uv)",
    };
    let transition_wrapper = match definition.kind {
        EffectKind::Effect | EffectKind::Generator => "",
        EffectKind::Transition => {
            r"fn alpha2_transition(uv: vec2f) -> vec4f {
    if (progress() <= 0.0) { return from_color(uv); }
    if (progress() >= 1.0) { return to_color(uv); }
    return transition(uv);
}"
        }
    };
    let resource_declarations = resource_declarations(&definition.resources);
    let input_declarations = input_declarations(&definition.inputs);
    format!(
        r"@group(0) @binding(0) var source_a: texture_2d<f32>;
@group(0) @binding(1) var source_b: texture_2d<f32>;
@group(0) @binding(2) var source_sampler: sampler;
struct EffectParams {{
{param_fields}}};
struct EffectParamUniforms {{
{uniform_fields}}};
@group(0) @binding(3) var<uniform> param_uniforms: EffectParamUniforms;
var<private> params: EffectParams;
var<private> {BUDGET}: u32;
struct RuntimeValues {{ resolution_time: vec4f, transition: vec4f, ticks: vec4u, }};
@group(0) @binding(4) var<uniform> runtime: RuntimeValues;
{resource_declarations}{input_declarations}

fn sample_src(uv: vec2f) -> vec4f {{ return textureSampleLevel(source_a, source_sampler, uv, 0.0); }}
fn from_color(uv: vec2f) -> vec4f {{ return textureSampleLevel(source_a, source_sampler, uv, 0.0); }}
fn to_color(uv: vec2f) -> vec4f {{ return textureSampleLevel(source_b, source_sampler, uv, 0.0); }}
fn resolution() -> vec2f {{ return runtime.resolution_time.xy; }}
fn time() -> f32 {{ return runtime.resolution_time.z; }}
fn time_ticks() -> vec2u {{ return runtime.ticks.xy; }}
fn progress() -> f32 {{ return runtime.transition.x; }}
fn ratio() -> f32 {{ return runtime.transition.y; }}
fn max_filter_samples() -> f32 {{ return runtime.transition.z; }}
fn load_params() -> EffectParams {{ return EffectParams({param_values}); }}

{body}

{transition_wrapper}

struct VertexOutput {{ @builtin(position) position: vec4f, @location(0) uv: vec2f, }};
@vertex fn alpha2_vs(@builtin(vertex_index) index: u32) -> VertexOutput {{
    var p = array<vec2f, 3>(vec2f(-1.0, -1.0), vec2f(3.0, -1.0), vec2f(-1.0, 3.0));
    var u = array<vec2f, 3>(vec2f(0.0, 1.0), vec2f(2.0, 1.0), vec2f(0.0, -1.0));
    return VertexOutput(vec4f(p[index], 0.0, 1.0), u[index]);
}}
@fragment fn alpha2_fs(input: VertexOutput) -> @location(0) vec4f {{
    params = load_params();
    {BUDGET} = {MAX_LOOP_ITERATIONS}u;
    return {call};
}}
",
        // Registration validated the body, which includes delimiting its loops, so this
        // only falls back for a definition that skipped it.
        body =
            bound_for_loops(&definition.wgsl_body).unwrap_or_else(|_| definition.wgsl_body.clone()),
        resource_declarations = resource_declarations,
        input_declarations = input_declarations,
        transition_wrapper = transition_wrapper,
    )
}

fn resource_declarations(resources: &[ResourceSpec]) -> String {
    resources
        .iter()
        .enumerate()
        .map(|(index, resource)| match resource.kind {
            EffectResourceKind::Lut3d => format!(
                "struct ResourceMetadata_{} {{ domain_min: vec4f, domain_max: vec4f, }};\n\
                 @group(0) @binding({}) var resource_{}: texture_3d<f32>;\n\
                 @group(0) @binding({}) var<uniform> resource_{}_metadata: ResourceMetadata_{};\n\
                 fn sample_{}(rgb: vec3f) -> vec3f {{\n\
                     let edge = f32(textureDimensions(resource_{}).x);\n\
                     let normalized = clamp((rgb - resource_{}_metadata.domain_min.xyz) /\n\
                         (resource_{}_metadata.domain_max.xyz - resource_{}_metadata.domain_min.xyz),\n\
                         vec3f(0.0), vec3f(1.0));\n\
                     let coord = (normalized * (edge - 1.0) + 0.5) / edge;\n\
                     return textureSampleLevel(resource_{}, source_sampler, coord, 0.0).rgb;\n\
                 }}",
                resource.name,
                index * 2 + 5,
                resource.name,
                index * 2 + 6,
                resource.name,
                resource.name,
                resource.name,
                resource.name,
                resource.name,
                resource.name,
                resource.name,
                resource.name,
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What the wrapper declares for each named input: its texture, its sampling
/// helper, and its size. Empty without inputs, so an effect that declares none adds
/// nothing to its module.
fn input_declarations(inputs: &[InputSpec]) -> String {
    inputs
        .iter()
        .enumerate()
        .fold(String::new(), |mut output, (index, input)| {
            let name = &input.name;
            write!(
                output,
                "\n@group(0) @binding({binding}) var input_{name}: texture_2d<f32>;\n\
                 fn sample_{name}(uv: vec2f) -> vec4f {{ return textureSampleLevel(input_{name}, source_sampler, uv, 0.0); }}\n\
                 fn size_{name}() -> vec2f {{ return vec2f(textureDimensions(input_{name})); }}",
                binding = input_binding(index),
            )
            .expect("writing to a String cannot fail");
            output
        })
}

fn uniform_value(index: usize, param_type: ParamType) -> String {
    match param_type {
        ParamType::Float => format!("bitcast<f32>(param_uniforms.slot_{index}.x)"),
        ParamType::Vec2 => format!("bitcast<vec2f>(param_uniforms.slot_{index}.xy)"),
        ParamType::Vec4 | ParamType::Color => {
            format!("bitcast<vec4f>(param_uniforms.slot_{index})")
        }
        ParamType::Enum | ParamType::Bool => format!("param_uniforms.slot_{index}.x"),
    }
}

fn wgsl_type(param_type: ParamType) -> &'static str {
    match param_type {
        ParamType::Float => "f32",
        ParamType::Vec2 => "vec2f",
        ParamType::Vec4 | ParamType::Color => "vec4f",
        ParamType::Enum | ParamType::Bool => "u32",
    }
}

fn validate_definition(definition: &EffectDef) -> Result<(), ShaderDiagnostics> {
    if !valid_identifier(&definition.id) {
        return Err(ShaderDiagnostics::error(
            "effect id must contain only ASCII letters, digits, '-', '_', or '.'",
        ));
    }
    if definition.version == 0 {
        return Err(ShaderDiagnostics::error("effect version must be non-zero"));
    }
    validate_wgsl_body(&definition.wgsl_body)?;
    if !definition.inputs.is_empty() && definition.kind != EffectKind::Effect {
        return Err(ShaderDiagnostics::error(
            "only effects declare named inputs; a transition samples from and to",
        ));
    }
    if definition.inputs.len() > MAX_EFFECT_INPUTS {
        return Err(ShaderDiagnostics::error(format!(
            "effect declares more than {MAX_EFFECT_INPUTS} named inputs"
        )));
    }
    let mut names = std::collections::HashSet::new();
    for param in &definition.params {
        validate_param(param)?;
        if !names.insert(param.name.as_str()) {
            return Err(ShaderDiagnostics::error(format!(
                "duplicate parameter name {}",
                param.name
            )));
        }
    }
    if definition.resources.len() > MAX_EFFECT_RESOURCES {
        return Err(ShaderDiagnostics::error(format!(
            "effect declares more than {MAX_EFFECT_RESOURCES} resources"
        )));
    }
    let mut resource_names = std::collections::HashSet::new();
    // What the wrapper declares for each resource, at module scope beside its own names:
    // none may be one of those or another resource's.
    let mut declared = std::collections::HashSet::new();
    for resource in &definition.resources {
        let generated = resource_declarations_of(&resource.name);
        if !valid_wgsl_identifier(&resource.name)
            || names.contains(resource.name.as_str())
            || !resource_names.insert(resource.name.as_str())
            || !generated
                .iter()
                .all(|name| valid_wgsl_identifier(name) && declared.insert(name.clone()))
        {
            return Err(ShaderDiagnostics::error(format!(
                "invalid or duplicate effect resource name {}",
                resource.name
            )));
        }
    }
    // An input's names share the module scope with the resources'.
    let mut input_names = std::collections::HashSet::new();
    for input in &definition.inputs {
        let generated = input_declarations_of(&input.name);
        if !valid_wgsl_identifier(&input.name)
            || names.contains(input.name.as_str())
            || resource_names.contains(input.name.as_str())
            || !input_names.insert(input.name.as_str())
            || !generated
                .iter()
                .all(|name| valid_wgsl_identifier(name) && declared.insert(name.clone()))
        {
            return Err(ShaderDiagnostics::error(format!(
                "invalid or duplicate effect input name {}",
                input.name
            )));
        }
    }
    Ok(())
}

/// The module-scope names [`build_shader`] declares for a named input called
/// `name`. The name `src` is refused through them: `sample_src` is the
/// prelude's.
fn input_declarations_of(name: &str) -> [String; 3] {
    [
        format!("input_{name}"),
        format!("sample_{name}"),
        format!("size_{name}"),
    ]
}

/// The module-scope names [`build_shader`] declares for a resource named
/// `name`.
fn resource_declarations_of(name: &str) -> [String; 4] {
    [
        format!("resource_{name}"),
        format!("resource_{name}_metadata"),
        format!("sample_{name}"),
        format!("ResourceMetadata_{name}"),
    ]
}

fn validate_wgsl_body(body: &str) -> Result<(), ShaderDiagnostics> {
    if body.len() > MAX_WGSL_BODY_BYTES {
        return Err(ShaderDiagnostics::error(format!(
            "WGSL body exceeds {MAX_WGSL_BODY_BYTES} bytes"
        )));
    }
    if body
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(ShaderDiagnostics::error(
            "WGSL body contains unsupported control characters",
        ));
    }
    // Comments may say anything; only code is checked, for balance as for tokens.
    let code = blank_comments(body);
    if let Some(identifier) = identifiers(&code).find(|word| word.starts_with(RESERVED_PREFIX)) {
        return Err(ShaderDiagnostics::error(format!(
            "WGSL identifier {identifier} is reserved: names starting with {RESERVED_PREFIX} belong to the generated wrapper"
        )));
    }
    if code.split([';', '{', '}']).any(|statement| {
        statement.bytes().filter(|byte| is_operator(*byte)).count() > MAX_WGSL_STATEMENT_OPERATORS
    }) {
        return Err(ShaderDiagnostics::error(format!(
            "a WGSL statement has more than {MAX_WGSL_STATEMENT_OPERATORS} operators; split it with `let`"
        )));
    }
    let mut delimiters = Vec::new();
    for character in code.chars() {
        match character {
            '{' | '(' | '[' => {
                delimiters.push(character);
                if delimiters.len() > MAX_WGSL_NESTING {
                    return Err(ShaderDiagnostics::error(format!(
                        "WGSL nesting exceeds {MAX_WGSL_NESTING} levels"
                    )));
                }
            }
            '}' | ')' | ']' => {
                let matches = matches!(
                    (delimiters.pop(), character),
                    (Some('{'), '}') | (Some('('), ')') | (Some('['), ']')
                );
                if !matches {
                    return Err(ShaderDiagnostics::error(
                        "WGSL body has unbalanced delimiters",
                    ));
                }
            }
            _ => {}
        }
    }
    if !delimiters.is_empty() {
        return Err(ShaderDiagnostics::error(
            "WGSL body has unbalanced delimiters",
        ));
    }

    let forbidden = [
        "@binding",
        "@compute",
        "@group",
        "atomic",
        "enable",
        "loop",
        "override",
        "var<storage",
        "var<uniform",
        "var<workgroup",
        "while",
    ];
    for token in code.split(|character: char| {
        !(character.is_ascii_alphanumeric() || matches!(character, '_' | '@' | '<'))
    }) {
        if forbidden.contains(&token) {
            return Err(ShaderDiagnostics::error(format!(
                "WGSL token {token} is not allowed in a runtime effect"
            )));
        }
    }
    bound_for_loops(body)?;
    crate::call_budget::check_invocation_calls(&code)
}

/// The identifiers in `code`: words that start with a letter or `_`, so the
/// digits and suffixes of a number are not one.
fn identifiers(code: &str) -> impl Iterator<Item = &str> {
    code.split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .filter(|word| word.starts_with(|first: char| first.is_ascii_alphabetic() || first == '_'))
}

/// Counts every character that can make an expression one level deeper:
/// operators, member access, and indexing. It overcounts (the `.` of `1.0`,
/// the `<` of `vec4<f32>`), which only lowers the effective limit.
const fn is_operator(byte: u8) -> bool {
    matches!(
        byte,
        b'+' | b'-'
            | b'*'
            | b'/'
            | b'%'
            | b'&'
            | b'|'
            | b'^'
            | b'!'
            | b'~'
            | b'<'
            | b'>'
            | b'='
            | b'.'
            | b'['
    )
}

fn validate_param(param: &ParamSpec) -> Result<(), ShaderDiagnostics> {
    if !valid_wgsl_identifier(&param.name) {
        return Err(ShaderDiagnostics::error(format!(
            "invalid WGSL parameter name {}",
            param.name
        )));
    }
    let compatible = matches!(
        (&param.param_type, &param.default, &param.unit),
        (
            ParamType::Float,
            ParamValue::Float(_),
            ParamUnit::Normalized | ParamUnit::Degrees
        ) | (ParamType::Vec2, ParamValue::Vec2(_), ParamUnit::Normalized)
            | (ParamType::Vec4, ParamValue::Vec4(_), ParamUnit::Normalized)
            | (ParamType::Color, ParamValue::Color(_), ParamUnit::Color)
            | (ParamType::Enum, ParamValue::Enum(_), ParamUnit::Enum)
            | (ParamType::Bool, ParamValue::Bool(_), ParamUnit::Bool)
    );
    if !compatible {
        return Err(ShaderDiagnostics::error(format!(
            "parameter {} has incompatible type, default, and unit",
            param.name
        )));
    }
    if !finite_value(&param.default)
        || param.min.is_some_and(|value| !value.is_finite())
        || param.max.is_some_and(|value| !value.is_finite())
        || param
            .step
            .is_some_and(|value| !value.is_finite() || value <= 0.0)
        || matches!((param.min, param.max), (Some(min), Some(max)) if min > max)
    {
        return Err(ShaderDiagnostics::error(format!(
            "parameter {} has an invalid numeric range",
            param.name
        )));
    }
    if !matches!(
        param.param_type,
        ParamType::Float | ParamType::Vec2 | ParamType::Vec4
    ) && (param.min.is_some() || param.max.is_some() || param.step.is_some())
    {
        return Err(ShaderDiagnostics::error(format!(
            "only float, vec2, or vec4 parameter {} may declare a numeric range",
            param.name
        )));
    }
    if matches!(
        param.param_type,
        ParamType::Float | ParamType::Vec2 | ParamType::Vec4
    ) && !value_in_range(&param.default, param.min, param.max)
    {
        return Err(ShaderDiagnostics::error(format!(
            "parameter {} default is outside its numeric range",
            param.name
        )));
    }
    if let ParamValue::Color(values) = &param.default
        && !values.iter().all(|value| (0.0..=1.0).contains(value))
    {
        return Err(ShaderDiagnostics::error(format!(
            "color parameter {} default must be inside 0..=1",
            param.name
        )));
    }
    if param.param_type == ParamType::Enum && param.enum_values.is_empty() {
        return Err(ShaderDiagnostics::error(format!(
            "enum parameter {} requires named values",
            param.name
        )));
    }
    if let ParamValue::Enum(value) = &param.default
        && usize::try_from(*value).map_or(true, |index| index >= param.enum_values.len())
    {
        return Err(ShaderDiagnostics::error(format!(
            "enum parameter {} default is out of range",
            param.name
        )));
    }
    Ok(())
}

fn value_in_range(value: &ParamValue, min: Option<f32>, max: Option<f32>) -> bool {
    let in_range = |value: &f32| {
        min.is_none_or(|minimum| *value >= minimum) && max.is_none_or(|maximum| *value <= maximum)
    };
    match value {
        ParamValue::Float(value) => in_range(value),
        ParamValue::Vec2(values) => values.iter().all(in_range),
        ParamValue::Vec4(values) => values.iter().all(in_range),
        _ => true,
    }
}

fn finite_value(value: &ParamValue) -> bool {
    match value {
        ParamValue::Float(value) => value.is_finite(),
        ParamValue::Vec2(values) => values.iter().all(|value| value.is_finite()),
        ParamValue::Vec4(values) | ParamValue::Color(values) => {
            values.iter().all(|value| value.is_finite())
        }
        ParamValue::Enum(_) | ParamValue::Bool(_) => true,
    }
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_wgsl_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && !value.starts_with(RESERVED_PREFIX)
        && !matches!(
            value,
            "params"
                | "resolution"
                | "time"
                | "time_ticks"
                | "progress"
                | "ratio"
                | "sample_src"
                | "from_color"
                | "to_color"
                | "load_params"
                | "max_filter_samples"
                | "source_a"
                | "source_b"
                | "source_sampler"
                | "param_uniforms"
                | "runtime"
                | "EffectParams"
        )
}

fn compilation_diagnostics(info: wgpu::CompilationInfo) -> ShaderDiagnostics {
    ShaderDiagnostics {
        messages: info
            .messages
            .into_iter()
            .map(|message| {
                let severity = match message.message_type {
                    wgpu::CompilationMessageType::Error => DiagnosticSeverity::Error,
                    wgpu::CompilationMessageType::Warning => DiagnosticSeverity::Warning,
                    wgpu::CompilationMessageType::Info => DiagnosticSeverity::Info,
                };
                ShaderDiagnostic {
                    severity,
                    message: message.message,
                    line: message.location.map(|location| location.line_number),
                    column: message.location.map(|location| location.line_position),
                }
            })
            .collect(),
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
