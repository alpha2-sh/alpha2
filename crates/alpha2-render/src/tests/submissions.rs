//! Frames cut into submissions under the watchdog's budget draw what one submission draws.

use super::*;

/// The heavy watchdog fixture's body: 1024 taps, each weighted by a chain of transcendentals.
fn heavy_effect() -> EffectDef {
    let fixture = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/cases/gpu-watchdog/heavy-effects.json"
    ))
    .expect("the heavy fixture");
    let effects: serde_json::Value = serde_json::from_str(&fixture).expect("JSON");
    EffectDef {
        id: "test.heavy".into(),
        params: Vec::new(),
        ..runtime_tint_effect(effects[0]["wgslBody"].as_str().expect("a WGSL body"))
    }
}

/// Draws `effect` over a gradient at `size` on a renderer of its own (no result cache shared),
/// and returns the picture and how the frame was submitted.
fn draw(effect: EffectDef, size: RenderSize) -> (Vec<u8>, crate::FrameSubmissions) {
    let mut renderer = Renderer::new(gpu());
    let id = pollster::block_on(renderer.register_effect(effect)).expect("the effect compiles");
    let pixels = (0..size.width * size.height)
        .flat_map(|index| {
            let x = index % size.width;
            let y = index / size.width;
            [
                u8::try_from(x * 255 / size.width).unwrap_or(255),
                u8::try_from(y * 255 / size.height).unwrap_or(255),
                u8::try_from((x ^ y) & 0xff).unwrap_or(0),
                255,
            ]
        })
        .collect::<Vec<_>>();
    let source = rgba_image_texture(&renderer, size, &pixels);
    let view = source.create_view(&wgpu::TextureViewDescriptor::default());
    let mut graph = RenderGraph::new(size);
    let input = graph.add_source(SourceView::from_wgpu(&view));
    let output = graph
        .add_effect(id, input, EffectParams::new(), EffectRuntime::default())
        .expect("dependency exists");
    graph.set_output(output).expect("output exists");
    let result = renderer.execute_graph(&graph).expect("the graph renders");
    let submissions = renderer
        .take_frame_submissions()
        .expect("the frame's submissions");
    (read_rgba8(&renderer, &result), submissions)
}

#[test]
fn banded_submission_renders_the_same_bytes_as_one_submission() {
    let _lock = gpu_test_lock();
    let size = RenderSize::new(512, 288).expect("size");
    let (whole, planned) = draw(heavy_effect(), size);
    // A one-millisecond watchdog leaves no budget: the pass is drawn in the most bands.
    alpha2_gpu::set_watchdog_ms(1);
    let banded = std::panic::catch_unwind(|| draw(heavy_effect(), size));
    alpha2_gpu::set_watchdog_ms(0);
    let (banded, split) = banded.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    assert!(planned.predicted_ms.is_some(), "a heavy frame is planned");
    assert_eq!(
        split.submissions,
        crate::submission_planner::MAX_BANDS,
        "{split:?}"
    );
    assert!(split.submissions > planned.submissions, "{planned:?}");
    assert_eq!(split.cost, planned.cost);
    assert!(
        whole == banded,
        "the bands drew other bytes than the one pass"
    );
}

#[test]
fn a_light_frame_stays_one_submission() {
    let _lock = gpu_test_lock();
    // Even with no budget at all, a frame under the planning threshold is not planned.
    alpha2_gpu::set_watchdog_ms(1);
    let drawn = std::panic::catch_unwind(|| {
        draw(
            runtime_tint_effect(
                "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
            ),
            RenderSize::new(1920, 1080).expect("size"),
        )
    });
    alpha2_gpu::set_watchdog_ms(0);
    let (_, submissions) = drawn.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    assert_eq!(submissions.submissions, 1);
    assert_eq!(submissions.predicted_ms, None);
    assert!(submissions.cost.unit_pixels() > 0.0);
}
