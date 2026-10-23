//! The render graph: node kinds, what a graph or invocation refuses, and its cache key.

use super::*;

fn assert_graph_pool_after_readback(renderer: &Renderer, base_available: usize) {
    let uses_pack = renderer.intermediate_format() == wgpu::TextureFormat::Rgba16Float;
    let pool = renderer.texture_pool_stats();
    assert_eq!(pool.created, 2 + u64::from(uses_pack));
    assert_eq!(pool.reused, 1);
    assert_eq!(pool.available, base_available + usize::from(uses_pack));
}

#[test]
fn render_graph_unifies_transition_effect_and_composition_nodes() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let tint = pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("tint effect compiles");
    let transition = pollster::block_on(renderer.register_effect(EffectDef {
        id: "test.graph-crossfade".into(),
        version: 1,
        kind: EffectKind::Transition,
        inputs: Vec::new(),
        params: Vec::new(),
            resources: Vec::new(),
        wgsl_body: "fn transition(uv: vec2f) -> vec4f { return mix(from_color(uv), to_color(uv), progress()); }".into(),
        tier_min: CapabilityTier::TierB,
        license: Some("MIT".into()),
    }))
    .expect("crossfade compiles");

    let from = rgba_texture(&renderer, [240, 32, 16, 255]);
    let to = rgba_texture(&renderer, [8, 64, 224, 255]);
    let from_view = from.create_view(&wgpu::TextureViewDescriptor::default());
    let to_view = to.create_view(&wgpu::TextureViewDescriptor::default());
    let size = RenderSize::new(2, 2).expect("non-zero size");
    let mut graph = RenderGraph::new(size);
    let from_node = graph.add_source_with_key(
        SourceView::from_wgpu(&from_view),
        RenderCacheKey::source(10),
    );
    let to_node =
        graph.add_source_with_key(SourceView::from_wgpu(&to_view), RenderCacheKey::source(20));
    let transition_node = graph
        .add_transition(
            transition.clone(),
            from_node,
            to_node,
            EffectParams::new(),
            EffectRuntime {
                progress: 0.25,
                ..EffectRuntime::default()
            },
        )
        .expect("dependencies exist");
    let effect_node = graph
        .add_effect(
            tint.clone(),
            transition_node,
            EffectParams::new().with("tint", ParamValue::Color([0.5, 1.0, 1.0, 1.0])),
            EffectRuntime::default(),
        )
        .expect("dependency exists");
    let output = graph
        .add_composite(vec![GraphLayer {
            source: effect_node,
            visual: visual([1.0, 1.0], 1.0),
            blend_mode: BlendMode::Normal,
        }])
        .expect("dependency exists");
    graph.set_output(output).expect("output exists");
    let _unused = graph
        .add_effect(
            tint.clone(),
            from_node,
            EffectParams::new(),
            EffectRuntime::default(),
        )
        .expect("dependency exists");

    assert_eq!(graph.node_count(), 6);
    assert_eq!(
        graph.node_kind(transition_node),
        Some(RenderNodeKind::Transition)
    );
    assert_eq!(graph.node_kind(effect_node), Some(RenderNodeKind::Effect));
    assert_eq!(graph.node_kind(output), Some(RenderNodeKind::Composite));

    let plan = graph.compile().expect("graph compiles");
    assert_eq!(plan.execution_order().len(), 5);
    assert_eq!(plan.last_use(from_node), Some(2));
    assert_eq!(plan.last_use(to_node), Some(2));
    assert_eq!(plan.last_use(transition_node), Some(3));
    assert_eq!(plan.last_use(effect_node), Some(4));
    assert_eq!(plan.last_use(output), Some(5));

    assert_eq!(renderer.pipeline_cache_stats().builtins, 0);
    let cache_key = renderer.graph_cache_key(&graph).expect("graph has a key");
    assert_eq!(
        cache_key,
        renderer.graph_cache_key(&graph).expect("stable key")
    );
    let warmup = renderer
        .warmup_graphs(&[&graph, &graph])
        .expect("project graphs warm up");
    assert_eq!(warmup.runtime_effects, 2);
    assert_eq!(warmup.builtins, 1);
    assert_eq!(renderer.pipeline_cache_stats().runtime_effects, 2);
    assert_eq!(renderer.pipeline_cache_stats().builtins, 1);

    let graph_before = graph_counts(&renderer);
    let result = renderer.execute_graph(&graph).expect("graph executes");
    assert_graph_delta(&renderer, graph_before, 3, 3);
    assert_rgba8_golden("render_graph", size, &read_rgba8(&renderer, &result), 1);
    assert_graph_pool_after_readback(&renderer, 1);
    drop(result);
    assert_graph_pool_after_readback(&renderer, 2);
}

#[test]
fn render_graph_rejects_an_effect_in_a_transition_node() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let effect = pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("effect compiles");
    let texture = rgba_texture(&renderer, [255; 4]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let size = RenderSize::new(1, 1).expect("non-zero size");
    let mut graph = RenderGraph::new(size);
    let from = graph.add_source(SourceView::from_wgpu(&view));
    let to = graph.add_source(SourceView::from_wgpu(&view));
    let output = graph
        .add_transition(
            effect,
            from,
            to,
            EffectParams::new(),
            EffectRuntime::default(),
        )
        .expect("dependencies exist");
    graph.set_output(output).expect("output exists");
    assert!(matches!(
        renderer.execute_graph(&graph),
        Err(GraphError::EffectKindMismatch)
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn graphs_and_invocations_refuse_what_they_cannot_draw() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let effect = pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("effect compiles");
    let transition = pollster::block_on(renderer.register_effect(EffectDef {
        id: "test.mix".into(),
        kind: EffectKind::Transition,
        inputs: Vec::new(),
        params: Vec::new(),
        wgsl_body: "fn transition(uv: vec2f) -> vec4f { return mix(from_color(uv), to_color(uv), progress()); }".into(),
        ..runtime_tint_effect("")
    }))
    .expect("transition compiles");
    let generator = pollster::block_on(renderer.register_effect(crate::generator_definition()))
        .expect("generator compiles");
    let texture = rgba_texture(&renderer, [255; 4]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let size = RenderSize::new(1, 1).expect("non-zero size");
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);

    let mut graph = RenderGraph::new(size);
    let source = graph.add_source(SourceView::from_wgpu(&view));
    graph.set_output(source).expect("output exists");
    assert_eq!(
        renderer.execute_graph(&graph).err(),
        Some(GraphError::SourceOutput)
    );
    assert_eq!(
        GraphError::SourceOutput.into_render_error(),
        RenderError::InvalidGraph
    );

    // A registered effect of another kind in an effect node, and in a generator node.
    let mut graph = RenderGraph::new(size);
    let source = graph.add_source(SourceView::from_wgpu(&view));
    let output = graph
        .add_effect(
            transition.clone(),
            source,
            EffectParams::new(),
            EffectRuntime::default(),
        )
        .expect("dependency exists");
    graph.set_output(output).expect("output exists");
    assert_eq!(
        renderer.execute_graph(&graph).err(),
        Some(GraphError::EffectKindMismatch),
        "a transition in an effect node"
    );
    for wrong in [&effect, &transition] {
        let mut graph = RenderGraph::new(size);
        let output =
            graph.add_generator(wrong.clone(), EffectParams::new(), EffectRuntime::default());
        graph.set_output(output).expect("output exists");
        assert_eq!(
            renderer.execute_graph(&graph).err(),
            Some(GraphError::EffectKindMismatch),
            "{} in a generator node",
            wrong.as_str()
        );
    }

    // The one-call invocations refuse the other kinds, and a transition without its second
    // input.
    for wrong in [&effect, &transition] {
        assert_eq!(
            renderer
                .render_generator(wrong, size, &EffectParams::new(), EffectRuntime::default())
                .err(),
            Some(RenderError::InvalidGraph),
            "render_generator({})",
            wrong.as_str()
        );
    }
    assert_eq!(
        renderer
            .render_effect(
                &generator,
                &view,
                None,
                size,
                &EffectParams::new(),
                EffectRuntime::default()
            )
            .err(),
        Some(RenderError::InvalidGraph),
        "render_effect(generator)"
    );
    assert_eq!(
        renderer
            .render_effect(
                &transition,
                &view,
                None,
                size,
                &EffectParams::new(),
                EffectRuntime::default()
            )
            .err(),
        Some(RenderError::MissingEffectInput)
    );
    let unknown = crate::EffectId::new("test.unregistered".into(), 1);
    assert_eq!(
        renderer
            .render_effect(
                &unknown,
                &view,
                None,
                size,
                &EffectParams::new(),
                EffectRuntime::default()
            )
            .err(),
        Some(RenderError::UnknownEffect)
    );

    // Runtime values: progress within 0..=1, a positive ratio, everything finite.
    let render = |runtime: EffectRuntime| {
        renderer
            .render_effect(
                &transition,
                &view,
                Some(&view),
                size,
                &EffectParams::new(),
                runtime,
            )
            .err()
    };
    for (label, runtime) in [
        (
            "progress -0.01",
            EffectRuntime {
                progress: -0.01,
                ..EffectRuntime::default()
            },
        ),
        (
            "progress 1.01",
            EffectRuntime {
                progress: 1.01,
                ..EffectRuntime::default()
            },
        ),
        (
            "progress NaN",
            EffectRuntime {
                progress: f32::NAN,
                ..EffectRuntime::default()
            },
        ),
        (
            "ratio 0",
            EffectRuntime {
                ratio: 0.0,
                ..EffectRuntime::default()
            },
        ),
        (
            "ratio -1",
            EffectRuntime {
                ratio: -1.0,
                ..EffectRuntime::default()
            },
        ),
        (
            "ratio infinite",
            EffectRuntime {
                ratio: f32::INFINITY,
                ..EffectRuntime::default()
            },
        ),
    ] {
        assert_eq!(
            render(runtime),
            Some(RenderError::InvalidEffectParameter),
            "{label}"
        );
    }
    for (label, runtime) in [
        (
            "progress 0",
            EffectRuntime {
                progress: 0.0,
                ..EffectRuntime::default()
            },
        ),
        (
            "progress 1",
            EffectRuntime {
                progress: 1.0,
                ..EffectRuntime::default()
            },
        ),
        (
            "a tiny ratio",
            EffectRuntime {
                ratio: f32::MIN_POSITIVE,
                ..EffectRuntime::default()
            },
        ),
        (
            "a negative time",
            EffectRuntime {
                time: alpha2_time::MediaTime::from_ticks(-3 * alpha2_time::TICKS_PER_SECOND),
                ..EffectRuntime::default()
            },
        ),
    ] {
        assert_eq!(render(runtime), None, "{label} is valid");
    }
    assert!(pollster::block_on(scope.pop()).is_none());
}

#[test]
fn a_graph_cache_key_changes_with_the_quality_and_storage_that_change_its_pixels() {
    // Quality controls and the intermediate format change pixels, so a key without them would
    // reuse a preview frame for an export.
    let _lock = gpu_test_lock();
    let [gpu, rgba8] = gpus_by_intermediate();
    let renderer = Renderer::new(gpu);
    let size = RenderSize::new(4, 4).expect("size");
    let texture = rgba_texture(&renderer, [153, 51, 204, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut graph = RenderGraph::new(size);
    let layer = GraphLayer {
        source: graph.add_source(SourceView::from_wgpu(&view)),
        visual: visual([0.5, 0.5], 1.0),
        blend_mode: BlendMode::Normal,
    };
    let output = graph.add_composite(vec![layer]).expect("composite");
    graph.set_output(output).expect("output");
    let key = |renderer: &Renderer| renderer.graph_cache_key(&graph).expect("graph has a key");

    renderer.set_quality_controls(1, 1);
    let preview = key(&renderer);
    assert_eq!(preview, key(&renderer), "stable");
    renderer.set_quality_controls(64, 1);
    let export = key(&renderer);
    assert_ne!(preview, export, "max filter samples");
    // Passes see at most 64 samples, so asking for more draws the same.
    renderer.set_quality_controls(100, 1);
    assert_eq!(
        key(&renderer),
        export,
        "max filter samples past what passes take"
    );
    renderer.set_quality_controls(64, 4);
    assert_ne!(key(&renderer), export, "motion blur subframes");
    renderer.set_quality_controls(64, 1);
    assert_eq!(key(&renderer), export, "back to the same configuration");

    let eight_bit = Renderer::new(rgba8);
    eight_bit.set_quality_controls(64, 1);
    if eight_bit.intermediate_format() == renderer.intermediate_format() {
        // Under ALPHA2_TEST_INTERMEDIATE=rgba8unorm both are already 8-bit.
        assert_eq!(key(&eight_bit), export, "the same storage");
    } else {
        assert_ne!(key(&eight_bit), export, "intermediate format");
    }
}
