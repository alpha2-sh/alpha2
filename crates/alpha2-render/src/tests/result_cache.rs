//! The graph result cache: a kept result is the live render's texture, found
//! again only under a complete key, and dropped when what the key leaves out
//! changes.

use super::{gpu, gpu_test_lock, rgba_texture, runtime_tint_effect, visual};
use crate::readback::read_rgba8;
use crate::{
    BlendMode, CacheEligibility, CacheIntent, EffectId, EffectParams, EffectRuntime, GraphLayer,
    ParamValue, RenderCacheKey, RenderGraph, RenderSize, Renderer, ResultCacheStats, SourceView,
};

const TINT: &str = "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }";

fn renderer_with_tint() -> (Renderer, EffectId) {
    let mut renderer = Renderer::new(gpu());
    let tint = pollster::block_on(renderer.register_effect(runtime_tint_effect(TINT)))
        .expect("tint compiles");
    (renderer, tint)
}

struct Built<'a> {
    graph: RenderGraph<'a>,
    effect: crate::RenderNodeId,
    output: crate::RenderNodeId,
}

/// A source, a tint over it, and a composite of the tint: the tint and the
/// output are candidates.
fn graph<'a>(
    source: &'a wgpu::TextureView,
    keyed: bool,
    tint: &EffectId,
    red: f32,
    opacity: f32,
) -> Built<'a> {
    let size = RenderSize::new(4, 4).expect("size");
    let mut graph = RenderGraph::new(size);
    let source = if keyed {
        graph.add_source_with_key(SourceView::from_wgpu(source), RenderCacheKey::source(7))
    } else {
        graph.add_source(SourceView::from_wgpu(source))
    };
    let effect = graph
        .add_effect(
            tint.clone(),
            source,
            EffectParams::new().with("tint", ParamValue::Color([red, 1.0, 1.0, 1.0])),
            EffectRuntime::default(),
        )
        .expect("effect");
    let output = graph
        .add_composite(vec![GraphLayer {
            source: effect,
            visual: visual([1.0, 1.0], opacity),
            blend_mode: BlendMode::Normal,
        }])
        .expect("composite");
    graph.set_output(output).expect("output");
    graph
        .set_cache_intent(effect, CacheIntent::Auto)
        .expect("known node");
    graph
        .set_cache_intent(output, CacheIntent::Auto)
        .expect("known node");
    Built {
        graph,
        effect,
        output,
    }
}

fn run(renderer: &Renderer, graph: &RenderGraph<'_>) -> (Vec<u8>, ResultCacheStats, u64) {
    let nodes = renderer.diagnostics().graph_nodes;
    let scope = renderer
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let output = renderer.execute_graph(graph).expect("renders");
    let pixels = read_rgba8(renderer, &output);
    assert!(
        pollster::block_on(scope.pop()).is_none(),
        "no validation error"
    );
    let encoded = renderer.diagnostics().graph_nodes - nodes;
    (pixels, renderer.result_cache_stats(), encoded)
}

#[test]
fn a_kept_result_is_found_under_the_same_key_and_is_the_live_render() {
    let _lock = gpu_test_lock();
    let (renderer, tint) = renderer_with_tint();
    renderer.set_result_cache_budget(1 << 20);
    let texture = rgba_texture(&renderer, [200, 100, 50, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    let first = graph(&view, true, &tint, 0.5, 1.0);
    let (live, stats, encoded) = run(&renderer, &first.graph);
    assert_eq!(encoded, 2, "tint and composite drawn");
    assert_eq!((stats.misses, stats.inserts, stats.entries), (2, 2, 2));

    // A new graph with the same content: the output is found, nothing is drawn.
    let again = graph(&view, true, &tint, 0.5, 1.0);
    let (cached, stats, encoded) = run(&renderer, &again.graph);
    assert_eq!(cached, live);
    assert_eq!(encoded, 0);
    assert_eq!(stats.hits, 1);
    assert_eq!(stats.skipped_nodes, 1, "the tint under the hit");

    // Only the composite changed: the tint is found and the composite drawn over it.
    let faded = graph(&view, true, &tint, 0.5, 0.5);
    let (_, stats, encoded) = run(&renderer, &faded.graph);
    assert_eq!(encoded, 1);
    assert_eq!(stats.hits, 2);

    let redder = graph(&view, true, &tint, 1.0, 1.0);
    let (pixels, stats, encoded) = run(&renderer, &redder.graph);
    assert_eq!(encoded, 2);
    assert_ne!(pixels, live);
    assert_eq!(stats.hits, 2);
}

#[test]
fn a_transient_source_or_a_bypass_keeps_results_out() {
    let _lock = gpu_test_lock();
    let (renderer, tint) = renderer_with_tint();
    renderer.set_result_cache_budget(1 << 20);
    let texture = rgba_texture(&renderer, [200, 100, 50, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    let transient = graph(&view, false, &tint, 0.5, 1.0);
    assert_eq!(
        transient.graph.cache_eligibility(transient.output),
        Ok(CacheEligibility::TransientSource)
    );
    let (_, stats, _) = run(&renderer, &transient.graph);
    assert_eq!((stats.ineligible, stats.inserts), (2, 0));

    // A bypass on the tint reaches the composite drawn from it.
    let mut bypassed = graph(&view, true, &tint, 0.5, 1.0);
    bypassed
        .graph
        .set_cache_intent(bypassed.effect, CacheIntent::Bypass)
        .expect("known node");
    assert_eq!(
        bypassed.graph.cache_eligibility(bypassed.output),
        Ok(CacheEligibility::Bypassed)
    );
    let (_, stats, encoded) = run(&renderer, &bypassed.graph);
    assert_eq!(encoded, 2);
    assert_eq!(stats.inserts, 0);

    // And a bypass on the output reaches down to the tint.
    let mut from_above = graph(&view, true, &tint, 0.5, 1.0);
    from_above
        .graph
        .set_cache_intent(from_above.output, CacheIntent::Bypass)
        .expect("known node");
    assert_eq!(
        from_above.graph.cache_eligibility(from_above.effect),
        Ok(CacheEligibility::Bypassed)
    );
}

#[test]
fn the_budget_evicts_least_recently_used_results_and_pinned_ones_last() {
    let _lock = gpu_test_lock();
    let (renderer, tint) = renderer_with_tint();
    let texture = rgba_texture(&renderer, [200, 100, 50, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bytes = {
        let texel = u64::from(
            renderer
                .intermediate_format()
                .block_copy_size(None)
                .expect("texel"),
        );
        16 * texel
    };
    // Room for two 4x4 results.
    renderer.set_result_cache_budget(2 * bytes);
    let mut pinned = graph(&view, true, &tint, 0.25, 1.0);
    pinned
        .graph
        .set_cache_intent(pinned.output, CacheIntent::Pin)
        .expect("known node");
    let (_, stats, _) = run(&renderer, &pinned.graph);
    assert_eq!((stats.entries, stats.pinned), (2, 1));
    for red in [0.5, 0.75] {
        let (_, _, _) = run(&renderer, &graph(&view, true, &tint, red, 1.0).graph);
    }
    let stats = renderer.result_cache_stats();
    assert_eq!(stats.entries, 2);
    assert!(stats.bytes <= 2 * bytes);
    assert_eq!(stats.pinned, 1, "the pinned output outlived newer results");
    let (_, stats, encoded) = run(&renderer, &graph(&view, true, &tint, 0.25, 1.0).graph);
    assert_eq!(encoded, 0, "the pinned output is still found");
    assert!(stats.evictions >= 3);

    renderer.set_result_cache_budget(0);
    assert_eq!(
        renderer.result_cache_stats().entries,
        0,
        "off drops everything"
    );
    let (_, stats, encoded) = run(&renderer, &graph(&view, true, &tint, 0.25, 1.0).graph);
    assert_eq!(encoded, 2);
    assert_eq!(stats.entries, 0);
}

#[test]
fn what_keys_leave_out_clears_the_cache() {
    let _lock = gpu_test_lock();
    let (mut renderer, tint) = renderer_with_tint();
    renderer.set_result_cache_budget(1 << 20);
    let texture = rgba_texture(&renderer, [200, 100, 50, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let fill = |renderer: &Renderer| {
        run(renderer, &graph(&view, true, &tint, 0.5, 1.0).graph);
        assert_eq!(renderer.result_cache_stats().entries, 2);
    };

    fill(&renderer);
    let quality = renderer.quality_controls();
    renderer.set_quality_controls(quality.max_filter_samples, quality.motion_blur_subframes);
    assert_eq!(
        renderer.result_cache_stats().entries,
        2,
        "unchanged quality keeps it"
    );
    renderer.set_quality_controls(quality.max_filter_samples / 2, 1);
    let stats = renderer.result_cache_stats();
    assert_eq!((stats.entries, stats.invalidations[2]), (0, 1));

    fill(&renderer);
    let lut = crate::Lut3dData {
        edge_length: 2,
        domain_min: [0.0; 3],
        domain_max: [1.0; 3],
        values: (0..8)
            .map(|index| {
                let axis = |bit: u32| f32::from(u8::from(index & (1 << bit) != 0));
                [axis(0), axis(1), axis(2)]
            })
            .collect(),
    };
    renderer.register_lut3d("look", &lut).expect("lut");
    assert_eq!(renderer.result_cache_stats().invalidations[1], 1);

    fill(&renderer);
    assert!(renderer.unregister_effect(&tint));
    assert_eq!(renderer.result_cache_stats().invalidations[0], 1);

    renderer.clear_result_cache();
    assert_eq!(
        renderer.result_cache_stats().invalidations[3],
        0,
        "nothing to clear"
    );
}

#[test]
fn a_failed_encode_keeps_nothing() {
    let _lock = gpu_test_lock();
    let (renderer, tint) = renderer_with_tint();
    renderer.set_result_cache_budget(1 << 20);
    let texture = rgba_texture(&renderer, [200, 100, 50, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let size = RenderSize::new(4, 4).expect("size");
    let mut graph = RenderGraph::new(size);
    let source = graph.add_source_with_key(SourceView::from_wgpu(&view), RenderCacheKey::source(7));
    let effect = graph
        .add_effect(tint, source, EffectParams::new(), EffectRuntime::default())
        .expect("effect");
    // Drawn after the candidate, and never registered.
    let missing = graph
        .add_effect(
            EffectId::new("test.missing".to_owned(), 1),
            effect,
            EffectParams::new(),
            EffectRuntime::default(),
        )
        .expect("effect");
    graph.set_output(missing).expect("output");
    graph
        .set_cache_intent(effect, CacheIntent::Auto)
        .expect("known node");
    assert!(renderer.execute_graph(&graph).is_err());
    assert_eq!(renderer.result_cache_stats().entries, 0);
}

#[test]
fn a_hit_takes_the_pin_the_node_asks_for_now() {
    // A hit applies the node's current cache intent, not the one the entry was stored with.
    let _lock = gpu_test_lock();
    let (renderer, tint) = renderer_with_tint();
    let texture = rgba_texture(&renderer, [200, 100, 50, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let texel = u64::from(
        renderer
            .intermediate_format()
            .block_copy_size(None)
            .expect("texel"),
    );
    renderer.set_result_cache_budget(2 * 16 * texel);
    let with_output = |intent| {
        let built = graph(&view, true, &tint, 0.25, 1.0);
        let mut graph = built.graph;
        graph
            .set_cache_intent(built.output, intent)
            .expect("known node");
        graph
    };
    let (_, stats, _) = run(&renderer, &with_output(CacheIntent::Auto));
    assert_eq!((stats.entries, stats.pinned), (2, 0));
    // Pinned once cached: the hit pins it.
    let (_, stats, encoded) = run(&renderer, &with_output(CacheIntent::Pin));
    assert_eq!(encoded, 0, "the output is found");
    assert_eq!(stats.pinned, 1, "the hit pins what the node pins now");
    for red in [0.5, 0.75] {
        run(&renderer, &graph(&view, true, &tint, red, 1.0).graph);
    }
    let (_, stats, encoded) = run(&renderer, &with_output(CacheIntent::Pin));
    assert_eq!(encoded, 0, "the pinned output outlived newer results");
    assert_eq!(stats.pinned, 1);
    // Back to Auto: the hit unpins it, and newer results push it out.
    let (_, stats, encoded) = run(&renderer, &with_output(CacheIntent::Auto));
    assert_eq!(encoded, 0);
    assert_eq!(
        stats.pinned, 0,
        "the hit unpins what the node no longer pins"
    );
    for red in [0.5, 0.75] {
        run(&renderer, &graph(&view, true, &tint, red, 1.0).graph);
    }
    let (_, _, encoded) = run(&renderer, &with_output(CacheIntent::Auto));
    assert!(encoded > 0, "an unpinned output is evicted like any other");
}

#[test]
fn a_filter_sample_budget_past_what_a_pass_takes_is_applied_and_reported_as_what_it_takes() {
    // Every pass and shader clamps its filter samples, so a budget past the limit is stored and
    // reported as the limit, and moving between two such budgets keeps every kept result.
    let _lock = gpu_test_lock();
    let (renderer, tint) = renderer_with_tint();
    let limit = renderer.filter_sample_limit();
    assert!(limit <= crate::RenderQualityControls::MAX_FILTER_SAMPLES);
    let texture = rgba_texture(&renderer, [200, 100, 50, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let built = graph(&view, true, &tint, 0.5, 1.0);
    let planned = |renderer: &Renderer| {
        built
            .graph
            .compile_with_quality(renderer.quality_controls())
            .expect("compiles")
            .filter_sample_limit(built.effect)
    };
    let reduced = |requested| {
        vec![crate::CapabilityDiagnostic::FilterSamplesReduced {
            requested,
            applied: limit,
        }]
    };
    let quality_invalidations = |renderer: &Renderer| {
        renderer.result_cache_stats().invalidations[crate::CacheInvalidation::Quality as usize]
    };

    renderer.set_quality_controls(256, 1);
    assert_eq!(renderer.quality_controls().max_filter_samples, limit);
    assert_eq!(renderer.diagnostics().max_filter_samples, limit);
    assert_eq!(planned(&renderer), Some(limit));
    assert_eq!(renderer.diagnostics().degradations, reduced(256));

    // Another budget past the limit renders the same pixels: kept results stay.
    renderer.set_result_cache_budget(1 << 20);
    run(&renderer, &built.graph);
    assert_eq!(renderer.result_cache_stats().entries, 2);
    let before = quality_invalidations(&renderer);
    renderer.set_quality_controls(4096, 1);
    assert_eq!(renderer.result_cache_stats().entries, 2);
    assert_eq!(quality_invalidations(&renderer), before);
    assert_eq!(renderer.diagnostics().degradations, reduced(4096));

    // One the passes take is applied as asked, nothing is reported, and it changes the
    // pixels kept results were made with.
    renderer.set_quality_controls(limit / 2, 1);
    assert_eq!(renderer.quality_controls().max_filter_samples, limit / 2);
    assert_eq!(planned(&renderer), Some(limit / 2));
    assert_eq!(renderer.diagnostics().degradations, Vec::new());
    assert_eq!(renderer.result_cache_stats().entries, 0);
    assert_eq!(quality_invalidations(&renderer), before + 1);
}

#[test]
fn a_named_input_decides_what_its_effect_may_cache() {
    // A named input is as much a dependency as the source: a transient one leaves the
    // effect's key incomplete, and a bypass on it reaches the effect.
    let _lock = gpu_test_lock();
    let (renderer, tint) = renderer_with_tint();
    let texture = rgba_texture(&renderer, [200, 100, 50, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let size = RenderSize::new(2, 2).expect("size");
    let build = |keyed_map: bool| {
        let mut graph = RenderGraph::new(size);
        let source =
            graph.add_source_with_key(SourceView::from_wgpu(&view), RenderCacheKey::source(1));
        let map = if keyed_map {
            graph.add_source_with_key(SourceView::from_wgpu(&view), RenderCacheKey::source(2))
        } else {
            graph.add_source(SourceView::from_wgpu(&view))
        };
        let effect = graph
            .add_effect_with_inputs(
                tint.clone(),
                source,
                crate::EffectInputs::new().with("map", map),
                EffectParams::new(),
                crate::EffectResources::new(),
                EffectRuntime::default(),
            )
            .expect("earlier nodes");
        (graph, map, effect)
    };
    let (transient, _, effect) = build(false);
    assert_eq!(
        transient.cache_eligibility(effect),
        Ok(CacheEligibility::TransientSource)
    );
    let (mut keyed, map, effect) = build(true);
    assert_eq!(
        keyed.cache_eligibility(effect),
        Ok(CacheEligibility::Eligible)
    );
    keyed
        .set_cache_intent(map, CacheIntent::Bypass)
        .expect("known node");
    assert_eq!(
        keyed.cache_eligibility(effect),
        Ok(CacheEligibility::Bypassed)
    );
}
