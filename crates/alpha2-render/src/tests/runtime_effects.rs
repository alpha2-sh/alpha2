//! Runtime effects: registration, shader diagnostics, loop budgets, parameters, uniform limits, and resources.

use super::*;

#[test]
fn runtime_effect_registration_compiles_author_wgsl_and_layout() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let definition = runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    );
    let id = pollster::block_on(renderer.register_effect(definition.clone()))
        .expect("valid runtime effect compiles");
    assert_eq!(id.as_str(), "test.tint");
    assert_eq!(id.version(), 1);
    assert_eq!(renderer.effect_definition(&id), Some(&definition));
    assert_eq!(
        renderer
            .effect_uniform_layout(&id)
            .expect("layout exists")
            .size,
        16
    );
    assert!(renderer.is_effect_registered(&id));
    assert!(pollster::block_on(renderer.register_effect(definition)).is_err());
}

#[test]
fn effects_compiled_side_by_side_register_in_any_order_and_a_duplicate_is_refused_at_commit() {
    // Compiling borrows the renderer only briefly, so a browser host compiles while it renders.
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let tint = |id: &str| EffectDef {
        id: id.to_owned(),
        ..runtime_tint_effect(
            "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
        )
    };
    let first = renderer
        .compile_effect(tint("test.first"))
        .expect("compiles");
    let second = renderer
        .compile_effect(tint("test.second"))
        .expect("compiles");
    let again = renderer
        .compile_effect(tint("test.first"))
        .expect("not yet registered");
    let second = renderer
        .register_compiled(pollster::block_on(second.compiled()).expect("valid"))
        .expect("registers");
    let first = renderer
        .register_compiled(pollster::block_on(first.compiled()).expect("valid"))
        .expect("registers");
    assert!(renderer.is_effect_registered(&first) && renderer.is_effect_registered(&second));
    let refused = renderer
        .register_compiled(pollster::block_on(again.compiled()).expect("valid"))
        .expect_err("registered while it compiled");
    assert!(refused.messages[0].message.contains("already registered"));
    // A broken shader's own diagnostics come back, not the error of the pipeline made from
    // it.
    let broken = renderer
        .compile_effect(EffectDef {
            id: "test.broken".into(),
            ..runtime_tint_effect(
                "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return no_such_value; }",
            )
        })
        .expect("the definition is valid");
    let diagnostics =
        pollster::block_on(broken.compiled()).expect_err("the shader does not compile");
    assert!(
        diagnostics
            .messages
            .iter()
            .any(|message| message.message.contains("no_such_value")),
        "{diagnostics:?}"
    );
    assert!(!renderer.is_effect_registered(&crate::EffectId::new("test.broken".to_owned(), 1)));
}

/// Renders a 1x1 runtime effect over black and returns its RGBA8 pixel.
fn render_runtime_effect_pixel(body: &str) -> [u8; 4] {
    let mut renderer = Renderer::new(gpu());
    let id = pollster::block_on(renderer.register_effect(runtime_tint_effect(body)))
        .expect("a bounded loop compiles");
    let size = RenderSize::new(1, 1).expect("size");
    let source = rgba_image_texture(&renderer, size, &[0, 0, 0, 255]);
    let view = source.create_view(&wgpu::TextureViewDescriptor::default());
    let target = renderer
        .render_effect(
            &id,
            &view,
            None,
            size,
            &EffectParams::new(),
            EffectRuntime::default(),
        )
        .expect("the effect finishes");
    let pixel = read_rgba8(&renderer, &target);
    pixel[..4].try_into().expect("an RGBA8 pixel")
}

fn assert_counted(pixel: [u8; 4], count: f32) {
    let expected = quantize_unorm8(count);
    assert!(
        pixel[..3].iter().all(|value| value.abs_diff(expected) <= 1),
        "{pixel:?} != {expected}"
    );
}

#[test]
fn a_runtime_for_loop_that_never_ends_is_bounded() {
    // Unbounded, the driver resets the GPU and the process loses its device. The 1024-iteration
    // budget is counted in steps of 1/2048: 0.5, which RGBA8 stores as 128.
    let _lock = gpu_test_lock();
    let pixel = render_runtime_effect_pixel(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f {
            var count = 0.0;
            // A loop with its own lower bound keeps it, and spends what it ran.
            for (var i = 0u; i < 3u; i++) { count += 1.0 / 256.0; }
            for (;;) { count += 1.0 / 2048.0; }
            return vec4f(count, count, count, 1.0);
        }",
    );
    assert_counted(pixel, 3.0 / 256.0 + 1021.0 / 2048.0);
}

#[test]
fn nested_loops_and_loops_in_called_functions_share_one_budget() {
    // A bound per loop would run 1024 x 1024 iterations (white). One budget per invocation stops
    // them all after 1024: one outer iteration, 1023 inner, then the outer condition fails.
    let _lock = gpu_test_lock();
    let nested = render_runtime_effect_pixel(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f {
            var count = 0.0;
            for (;;) { for (;;) { count += 1.0 / 2048.0; } }
            return vec4f(count, count, count, 1.0);
        }",
    );
    assert_counted(nested, 1023.0 / 2048.0);
    let called = render_runtime_effect_pixel(
        "fn taps() -> f32 {
            var count = 0.0;
            for (;;) { count += 1.0 / 2048.0; }
            return count;
        }
        fn effect(_uv: vec2f, src: vec4f) -> vec4f {
            var count = 0.0;
            for (var i = 0u; i < 4u; i++) { count += taps(); }
            return vec4f(count, count, count, 1.0);
        }",
    );
    assert_counted(called, 1023.0 / 2048.0);
}

#[test]
fn a_flat_operator_chain_fails_its_registration_not_the_process() {
    // naga lowers `x+x+...+x` recursively; on a 1 MiB stack (Windows main thread, wasm32) about
    // 2,500 operands abort the process, so the chain is refused before it reaches naga.
    let _lock = gpu_test_lock();
    let chain = |operands: usize| {
        format!(
            "fn effect(_uv: vec2f, src: vec4f) -> vec4f {{ let x = src.r; return vec4f({}); }}",
            vec!["x"; operands].join("+")
        )
    };
    let register = |body: String, stack: usize| {
        std::thread::Builder::new()
            .stack_size(stack)
            .spawn(move || {
                let mut renderer = Renderer::new(gpu());
                pollster::block_on(renderer.register_effect(runtime_tint_effect(&body)))
                    .map(drop)
                    .map_err(|diagnostics| format!("{diagnostics:?}"))
            })
            .expect("the registering thread starts")
            .join()
            .expect("registration returns instead of overflowing the stack")
    };
    let error = register(chain(16_000), 1 << 20).expect_err("a 16,000-operand chain is refused");
    assert!(error.contains("operators"), "{error}");
    // At the limit a release build needs well under 1 MiB (it overflows at about 2,500
    // operands); an unoptimized one has frames about 20 times larger.
    let stack = if cfg!(debug_assertions) { 8 } else { 1 } << 20;
    register(chain(256), stack).expect("a chain at the limit registers");
}

#[test]
fn runtime_effect_registration_returns_shader_diagnostics() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let definition = runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return definitely_invalid(src); }",
    );
    let diagnostics = pollster::block_on(renderer.register_effect(definition))
        .expect_err("invalid WGSL is rejected");
    assert_ne!(diagnostics.messages, [] as [crate::ShaderDiagnostic; 0]);
    assert!(
        diagnostics
            .messages
            .iter()
            .any(|message| message.severity == crate::DiagnosticSeverity::Error)
    );
}

#[test]
fn registered_effect_executes_with_named_parameters() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let definition = runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    );
    let id = pollster::block_on(renderer.register_effect(definition))
        .expect("valid runtime effect compiles");
    renderer
        .warmup(std::slice::from_ref(&id))
        .expect("effect is warm");
    let source = rgba_texture(&renderer, [100, 50, 25, 255]);
    let source_view = source.create_view(&wgpu::TextureViewDescriptor::default());
    let params = EffectParams::new().with("tint", ParamValue::Color([0.5, 1.0, 1.0, 1.0]));
    let size = RenderSize::new(1, 1).expect("non-zero size");
    let target = renderer
        .render_effect(
            &id,
            &source_view,
            None,
            size,
            &params,
            EffectRuntime {
                time: alpha2_time::MediaTime::ZERO,
                progress: 0.0,
                ratio: 1.0,
            },
        )
        .expect("registered effect renders");
    assert_rgba8_golden("runtime_effect", size, &read_rgba8(&renderer, &target), 1);
}

#[test]
fn resource_backed_lut_effect_executes_from_a_cube_resource() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let id = pollster::block_on(renderer.register_builtin_lut_3d())
        .expect("built-in LUT effect compiles");
    let data = crate::Lut3dData::from_cube(
        "LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n",
    )
    .expect("identity LUT parses");
    renderer
        .register_lut3d("identity", &data)
        .expect("identity LUT uploads");
    let source = rgba_texture(&renderer, [100, 50, 25, 255]);
    let source_view = source.create_view(&wgpu::TextureViewDescriptor::default());
    let size = RenderSize::new(1, 1).expect("non-zero size");
    assert!(matches!(
        renderer.render_effect_with_resources(
            &id,
            &source_view,
            None,
            size,
            &EffectParams::new(),
            &crate::EffectResources::new(),
            EffectRuntime::default(),
        ),
        Err(RenderError::MissingEffectResource)
    ));
    let target = renderer
        .render_effect_with_resources(
            &id,
            &source_view,
            None,
            size,
            &EffectParams::new(),
            &crate::EffectResources::new().with("lut", "identity"),
            EffectRuntime::default(),
        )
        .expect("resource-backed LUT renders");
    let result = read_rgba8(&renderer, &target);
    for (actual, expected) in result.iter().zip([100_u8, 50, 25, 255]) {
        assert!(
            actual.abs_diff(expected) <= 2,
            "{actual} differs from {expected}"
        );
    }
}

#[test]
fn registered_transition_executes_with_two_inputs() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let definition = EffectDef {
        id: "test.crossfade".into(),
        version: 1,
        kind: EffectKind::Transition,
        inputs: EffectInputs::Two,
        params: Vec::new(),
            resources: Vec::new(),
        wgsl_body: "fn transition(uv: vec2f) -> vec4f { return mix(from_color(uv), to_color(uv), progress()); }".into(),
        tier_min: CapabilityTier::TierB,
        license: Some("MIT".into()),
    };
    let id = pollster::block_on(renderer.register_effect(definition))
        .expect("valid runtime transition compiles");
    let from = rgba_texture(&renderer, [255, 0, 0, 255]);
    let to = rgba_texture(&renderer, [0, 0, 255, 255]);
    let from_view = from.create_view(&wgpu::TextureViewDescriptor::default());
    let to_view = to.create_view(&wgpu::TextureViewDescriptor::default());
    let size = RenderSize::new(1, 1).expect("non-zero size");
    let target = renderer
        .render_effect(
            &id,
            &from_view,
            Some(&to_view),
            size,
            &EffectParams::new(),
            EffectRuntime {
                time: alpha2_time::MediaTime::ZERO,
                progress: 0.25,
                ratio: 1.0,
            },
        )
        .expect("registered transition renders");
    assert_rgba8_golden(
        "runtime_transition",
        size,
        &read_rgba8(&renderer, &target),
        1,
    );
}

/// A device of the shared adapter that keeps the adapter's own, smallest,
/// uniform offset alignment instead of WebGL2's 256 bytes, as an embedder
/// passing `Limits::default().using_alignment(adapter.limits())` to
/// [`GpuContext::from_parts`] does.
fn gpu_with_the_adapters_uniform_alignment() -> GpuContext {
    // The shared device's adapter: with only lavapipe's ICD, wgpu's preferred adapter is a
    // GL driver that cannot open a device, and the suite falls back past it.
    let shared = gpu();
    let wanted = shared.capabilities();
    let _turn = alpha2_gpu::serialize_acquisition();
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()))
        .into_iter()
        .find(|adapter| {
            let info = adapter.get_info();
            info.name == wanted.adapter_name && format!("{:?}", info.backend) == wanted.backend
        })
        .expect("the shared adapter");
    let limits = adapter.limits();
    let required_limits = wgpu::Limits::downlevel_webgl2_defaults()
        .using_resolution(limits.clone())
        .using_alignment(limits);
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("alpha2 small uniform alignment test device"),
        required_limits,
        ..wgpu::DeviceDescriptor::default()
    }))
    .expect("a device with the adapter's own alignment");
    GpuContext::from_parts(adapter, device, queue)
}

#[test]
fn every_uniform_a_graph_writes_fits_the_arena_at_the_adapters_own_alignment() {
    // At 256 bytes a slot's padding hides a layer writing more than it reserved; at the 16-64
    // a Vulkan driver reports, it overruns the arena and the capacity assertion panics.
    let _lock = gpu_test_lock();
    let gpu = gpu_with_the_adapters_uniform_alignment();
    let alignment = gpu.device().limits().min_uniform_buffer_offset_alignment;
    eprintln!("uniform offset alignment {alignment}");
    let mut renderer = Renderer::new(gpu);
    let effect = pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("tint compiles");
    let size = RenderSize::new(8, 8).expect("size");
    let colors = [
        [204, 51, 51],
        [51, 204, 51],
        [51, 51, 204],
        [153, 153, 51],
        [51, 153, 153],
        [153, 51, 153],
    ];
    let solids = colors
        .iter()
        .map(|[red, green, blue]| rgba_texture(&renderer, [*red, *green, *blue, 255]))
        .collect::<Vec<_>>();
    let views = solids
        .iter()
        .map(|solid| solid.create_view(&wgpu::TextureViewDescriptor::default()))
        .collect::<Vec<_>>();
    for count in 1..=views.len() {
        let scope = renderer
            .gpu
            .device()
            .push_error_scope(wgpu::ErrorFilter::Validation);
        // Opaque full-frame layers under a graph that also runs an effect pass: the last
        // layer shows.
        let mut graph = RenderGraph::new(size);
        let layers = views[..count]
            .iter()
            .map(|view| GraphLayer {
                source: graph.add_source(SourceView::from_wgpu(view)),
                visual: visual([1.0, 1.0], 1.0),
                blend_mode: BlendMode::Normal,
            })
            .collect::<Vec<_>>();
        let composite = graph.add_composite(layers).expect("composite");
        let graded = graph
            .add_effect(
                effect.clone(),
                composite,
                EffectParams::new(),
                EffectRuntime::default(),
            )
            .expect("effect");
        graph.set_output(graded).expect("output");
        let output = renderer.render_graph(&graph, &[]).expect("graph");
        let pixels = read_rgba8(&renderer, &output);
        assert!(pollster::block_on(scope.pop()).is_none(), "{count} layers");
        assert_rgb_close(
            &pixels,
            &[colors[count - 1]; 64],
            &format!("{count} layers"),
        );
    }
}

/// A one-input effect with `count` float parameters that passes its source.
fn effect_with_float_params(id: &str, count: usize) -> EffectDef {
    EffectDef {
        id: id.into(),
        version: 1,
        kind: EffectKind::Effect,
        inputs: EffectInputs::One,
        params: (0..count)
            .map(|index| ParamSpec {
                name: format!("p{index}"),
                param_type: ParamType::Float,
                default: ParamValue::Float(0.0),
                min: None,
                max: None,
                step: None,
                unit: ParamUnit::Normalized,
                enum_values: Vec::new(),
            })
            .collect(),
        resources: Vec::new(),
        wgsl_body: "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src + vec4f(params.p0); }"
            .into(),
        tier_min: CapabilityTier::TierB,
        license: None,
    }
}

#[test]
fn an_effect_whose_parameters_overflow_the_uniform_binding_limit_is_refused_at_registration() {
    // Each parameter is a 16-byte slot of one uniform binding (16 KiB on WebGL2's limits). Past
    // the cap the pipeline compiles but every bind group is invalid.
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let limit = renderer.limits().max_uniform_buffer_binding_size;
    let fitting = usize::try_from(limit / 16).expect("parameter count fits usize");
    let refused = pollster::block_on(
        renderer.register_effect(effect_with_float_params("test.too-many", fitting + 1)),
    )
    .expect_err("one parameter too many is refused");
    let message = refused
        .messages
        .iter()
        .map(|message| message.message.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        message.contains("uniform binding limit") && message.contains(&fitting.to_string()),
        "{message}"
    );
    assert!(!renderer.is_effect_registered(&crate::EffectId::new("test.too-many".into(), 1)));
    let id = pollster::block_on(
        renderer.register_effect(effect_with_float_params("test.most", fitting)),
    )
    .expect("the most parameters that fit register");
    let size = RenderSize::new(2, 2).expect("size");
    let input = rgba_texture(&renderer, [153, 51, 204, 255]);
    let view = input.create_view(&wgpu::TextureViewDescriptor::default());
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let output = renderer
        .render_effect(
            &id,
            &view,
            None,
            size,
            &EffectParams::new(),
            EffectRuntime::default(),
        )
        .expect("the effect renders");
    let pixels = read_rgba8(&renderer, &output);
    assert!(pollster::block_on(scope.pop()).is_none());
    assert_rgb_close(&pixels, &[[153, 51, 204]; 4], "most parameters");
}

#[test]
fn a_generator_that_declares_a_resource_draws_with_the_resource_it_is_given() {
    // Registration validates a generator's resource slots like an effect's, so its node must
    // carry resources too.
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let id = pollster::block_on(renderer.register_effect(EffectDef {
        id: "test.graded-generator".into(),
        version: 1,
        kind: EffectKind::Generator,
        inputs: EffectInputs::None,
        params: Vec::new(),
        resources: vec![crate::ResourceSpec {
            name: "grade".into(),
            kind: crate::EffectResourceKind::Lut3d,
        }],
        wgsl_body: "fn generate(uv: vec2f) -> vec4f { return vec4f(sample_grade(vec3f(0.2, 0.4, 0.8)), 1.0); }"
            .into(),
        tier_min: CapabilityTier::TierB,
        license: None,
    }))
    .expect("a generator with a resource registers");
    // An inverting LUT in .cube order, red varying fastest, and an identity one.
    let inverted = crate::Lut3dData::from_cube(
        "LUT_3D_SIZE 2\n1 1 1\n0 1 1\n1 0 1\n0 0 1\n1 1 0\n0 1 0\n1 0 0\n0 0 0\n",
    )
    .expect("inverted LUT parses");
    let identity = crate::Lut3dData::from_cube(
        "LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n",
    )
    .expect("identity LUT parses");
    renderer
        .register_lut3d("inverted", &inverted)
        .expect("inverted LUT uploads");
    renderer
        .register_lut3d("identity", &identity)
        .expect("identity LUT uploads");
    let size = RenderSize::new(2, 2).expect("size");
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let drawn = |lut: &str| {
        let output = renderer
            .render_generator_with_resources(
                &id,
                size,
                &EffectParams::new(),
                &crate::EffectResources::new().with("grade", lut),
                EffectRuntime::default(),
            )
            .expect("the generator renders with its resource");
        read_rgba8(&renderer, &output)
    };
    assert_rgb_close(&drawn("inverted"), &[[204, 153, 51]; 4], "inverted");
    assert_rgb_close(&drawn("identity"), &[[51, 102, 204]; 4], "identity");
    // In a graph, the resource is part of the node's identity.
    let mut graph = RenderGraph::new(size);
    let first = graph.add_generator_with_resources(
        id.clone(),
        EffectParams::new(),
        crate::EffectResources::new().with("grade", "inverted"),
        EffectRuntime::default(),
    );
    let second = graph.add_generator_with_resources(
        id.clone(),
        EffectParams::new(),
        crate::EffectResources::new().with("grade", "identity"),
        EffectRuntime::default(),
    );
    assert_ne!(
        graph.cache_keys[first.index],
        graph.cache_keys[second.index]
    );
    graph.set_output(first).expect("output");
    let output = renderer.render_graph(&graph, &[]).expect("graph");
    assert_rgb_close(
        &read_rgba8(&renderer, &output),
        &[[204, 153, 51]; 4],
        "graph",
    );
    assert!(pollster::block_on(scope.pop()).is_none());
    assert_eq!(
        renderer
            .render_generator(&id, size, &EffectParams::new(), EffectRuntime::default())
            .map(|_| ()),
        Err(RenderError::MissingEffectResource)
    );
}
