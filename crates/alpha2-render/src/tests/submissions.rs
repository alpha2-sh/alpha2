//! Frames cut into submissions under the watchdog's budget draw what one submission draws.

use super::*;

/// The heavy watchdog fixture's body: 1024 taps, each weighted by a chain of transcendentals.
fn heavy_effect() -> EffectDef {
    let fixture = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/cases/gpu-watchdog/heavy-effects.json"
    ));
    let effects: serde_json::Value = serde_json::from_str(fixture).expect("JSON");
    EffectDef {
        id: "test.heavy".into(),
        params: Vec::new(),
        ..runtime_tint_effect(effects[0]["wgslBody"].as_str().expect("a WGSL body"))
    }
}

/// Draws `effect` over a gradient on its own renderer, returning the picture and its submissions.
fn draw(effect: EffectDef, size: RenderSize) -> (Vec<u8>, crate::FrameSubmissions) {
    let renderer = Renderer::new(gpu());
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
    let (whole, planned) = with_watchdog(UNSPLIT_WATCHDOG_MS, || draw(heavy_effect(), size));
    assert_eq!(planned.submissions, 1, "{planned:?}");
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

const SIDES: [u32; 5] = [256, 512, 1024, 2048, 4096];
const SHORTEST_SETTABLE_WATCHDOG_MS: f64 = 16.0;
const FRAMES_TO_LEARN_FROM: usize = 4;
const UNSPLIT_WATCHDOG_MS: f64 = 1_000_000.0;

fn own_device() -> GpuContext {
    pollster::block_on(GpuContext::request_product()).expect("a GPU adapter")
}

fn registered(renderer: &Renderer, definition: EffectDef) -> (crate::EffectId, u64) {
    let id = pollster::block_on(renderer.register_effect(definition)).expect("the effect compiles");
    let pass_id = renderer.effects().get(&id).expect("registered").pass_id;
    (id, pass_id)
}

fn flat_source(renderer: &Renderer, size: RenderSize) -> wgpu::TextureView {
    let pixels = vec![128; usize::try_from(size.width * size.height * 4).expect("fits")];
    rgba_image_texture(renderer, size, &pixels).create_view(&wgpu::TextureViewDescriptor::default())
}

fn effect_graph<'a>(
    view: &'a wgpu::TextureView,
    effect: &crate::EffectId,
    size: RenderSize,
) -> RenderGraph<'a> {
    let mut graph = RenderGraph::new(size);
    let input = graph.add_source(SourceView::from_wgpu(view));
    let output = graph
        .add_effect(
            effect.clone(),
            input,
            EffectParams::new(),
            EffectRuntime::default(),
        )
        .expect("dependency exists");
    graph.set_output(output).expect("output exists");
    graph
}

fn exported_frame(
    renderer: &Renderer,
    effect: &crate::EffectId,
    view: &wgpu::TextureView,
    size: RenderSize,
) -> (crate::FrameSubmissions, f64) {
    let started = std::time::Instant::now();
    let readback = renderer
        .render_graph_yuv420p(
            &effect_graph(view, effect, size),
            &[],
            centred(ColorMetadata::default()),
        )
        .expect("the frame renders");
    readback.wait_ready().expect("the readback maps");
    let milliseconds = started.elapsed().as_secs_f64() * 1_000.0;
    let submissions = renderer
        .take_frame_submissions()
        .expect("the frame's submissions");
    (submissions, milliseconds)
}

fn polled_frame(
    renderer: &Renderer,
    effect: &crate::EffectId,
    view: &wgpu::TextureView,
    size: RenderSize,
) -> crate::FrameSubmissions {
    let result = renderer
        .execute_graph(&effect_graph(view, effect, size))
        .expect("the graph renders");
    renderer
        .gpu
        .device()
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(renderer.gpu_wait_timeout()),
        })
        .expect("the device polls");
    drop(result);
    renderer
        .take_frame_submissions()
        .expect("the frame's submissions")
}

fn busy_size(
    renderer: &Renderer,
    effect: &crate::EffectId,
) -> (RenderSize, wgpu::TextureView, f64) {
    SIDES
        .into_iter()
        .find_map(|side| {
            let size = RenderSize::new(side, side).expect("size");
            let view = flat_source(renderer, size);
            let _ = exported_frame(renderer, effect, &view, size);
            let (_, milliseconds) = exported_frame(renderer, effect, &view, size);
            (milliseconds >= SHORTEST_SETTABLE_WATCHDOG_MS).then_some((size, view, milliseconds))
        })
        .expect("some size keeps the GPU busy for the shortest watchdog this test can set")
}

fn with_watchdog<T>(milliseconds: f64, body: impl FnOnce() -> T) -> T {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a watchdog in milliseconds far below 2^32"
    )]
    alpha2_gpu::set_watchdog_ms(milliseconds as u32);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
    alpha2_gpu::set_watchdog_ms(0);
    result.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

#[test]
fn a_heavy_pass_is_banded_by_what_its_pilot_measured_however_low_the_device_rate() {
    let _lock = gpu_test_lock();
    let gpu = own_device();
    let renderer = Renderer::new(gpu.clone());
    let (measured, measured_pass) = registered(&renderer, heavy_effect());
    gpu.rate().seed(f64::MIN_POSITIVE);
    let any_size = |side: u32| u64::from(side) * u64::from(side);
    for side in SIDES {
        gpu.rate().record_pass_rate(
            alpha2_gpu::PassKey::new(measured_pass, any_size(side)),
            f64::MIN_POSITIVE,
        );
    }
    let (size, view, frame_ms) = busy_size(&renderer, &measured);
    let (piloted, _) = registered(
        &renderer,
        EffectDef {
            id: "test.heavy.piloted".into(),
            ..heavy_effect()
        },
    );
    let (first, _) = with_watchdog(frame_ms, || {
        exported_frame(&renderer, &piloted, &view, size)
    });
    assert!(
        first.submissions >= 4,
        "a pass that keeps the GPU busy for the whole {frame_ms:.1} ms watchdog is drawn in {} \
         submission(s) on its first frame: {first:?}",
        first.submissions
    );
}

#[test]
fn a_device_rate_seeded_too_low_is_raised_by_exported_frames_that_ran_over_it() {
    let _lock = gpu_test_lock();
    let gpu = own_device();
    if gpu.capabilities().backend == "Gl" {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::ExactGpuCompletion,
            "the GL backend reports a completion before a wait on it blocks"
        );
        return;
    }
    let renderer = Renderer::new(gpu.clone());
    let (effect, pass) = registered(&renderer, heavy_effect());
    gpu.rate().seed(f64::MIN_POSITIVE);
    for side in SIDES {
        gpu.rate().record_pass_rate(
            alpha2_gpu::PassKey::new(pass, u64::from(side) * u64::from(side)),
            f64::MIN_POSITIVE,
        );
    }
    let (size, view, frame_ms) = busy_size(&renderer, &effect);
    let last = with_watchdog(frame_ms, || {
        (0..FRAMES_TO_LEARN_FROM)
            .map(|_| exported_frame(&renderer, &effect, &view, size).0)
            .last()
            .expect("frames")
    });
    assert!(
        last.submissions >= 4,
        "a frame that keeps the GPU busy for the whole {frame_ms:.1} ms watchdog is still drawn \
         in {} submission(s) after {FRAMES_TO_LEARN_FROM} frames: {last:?}",
        last.submissions
    );
}

#[test]
fn a_device_rate_seeded_too_high_is_recalibrated_by_the_frames_it_mispredicts() {
    let _lock = gpu_test_lock();
    let gpu = own_device();
    let renderer = Renderer::new(gpu.clone());
    let (effect, _) = registered(&renderer, heavy_effect());
    let size = RenderSize::new(256, 144).expect("size");
    let view = flat_source(&renderer, size);
    let calibrated = polled_frame(&renderer, &effect, &view, size);
    assert!(
        calibrated.submissions < crate::submission_planner::MAX_BANDS,
        "{calibrated:?}"
    );
    let far_past_its_plan = 8.0
        * calibrated
            .predicted_ms
            .expect("a heavy frame is planned")
            .max(f64::from(alpha2_gpu::watchdog_ms()));
    gpu.rate()
        .seed(far_past_its_plan / calibrated.cost.unit_pixels());
    let mispredicted = polled_frame(&renderer, &effect, &view, size);
    assert!(
        mispredicted.submissions > calibrated.submissions,
        "{mispredicted:?} against {calibrated:?}"
    );
    let last = (0..FRAMES_TO_LEARN_FROM)
        .map(|_| polled_frame(&renderer, &effect, &view, size))
        .last()
        .expect("frames");
    assert!(
        last.submissions <= calibrated.submissions,
        "a frame calibrated to {} submission(s) is still cut in {} after {FRAMES_TO_LEARN_FROM} \
         frames measured far below their prediction: {last:?}",
        calibrated.submissions,
        last.submissions
    );
}

#[test]
fn a_frame_of_more_submissions_than_the_clock_keeps_still_lowers_the_rate() {
    let _lock = gpu_test_lock();
    let gpu = own_device();
    let renderer = Renderer::new(gpu.clone());
    let (effect, _) = registered(&renderer, heavy_effect());
    let size = RenderSize::new(512, 288).expect("size");
    let view = flat_source(&renderer, size);
    let _ = polled_frame(&renderer, &effect, &view, size);
    gpu.rate().seed(1.0);
    let banded = with_watchdog(1.0, || polled_frame(&renderer, &effect, &view, size));
    assert_eq!(
        banded.submissions,
        crate::submission_planner::MAX_BANDS,
        "{banded:?}"
    );
    assert!(
        gpu.rate().ms_per_unit_pixel() < Some(2.0),
        "{:?}",
        gpu.rate().ms_per_unit_pixel()
    );
}
