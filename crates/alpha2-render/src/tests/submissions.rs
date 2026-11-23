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
    alpha2_gpu::set_watchdog_ms(1);
    let banded = std::panic::catch_unwind(|| draw(heavy_effect(), size));
    alpha2_gpu::set_watchdog_ms(0);
    let (banded, split) = banded.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    assert!(planned.predicted_ms.is_some(), "a heavy frame is planned");
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
const PILOT_NOISE: f64 = 2.0;

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
    let calibrated_ms = calibrated.predicted_ms.expect("a heavy frame is planned");
    assert!(
        last.submissions < mispredicted.submissions
            && last
                .predicted_ms
                .is_some_and(|ms| ms <= PILOT_NOISE * calibrated_ms),
        "a frame calibrated to {calibrated_ms:.1} ms in {} submission(s) is still predicted \
         past it after {FRAMES_TO_LEARN_FROM} frames measured far below their prediction: \
         {last:?}",
        calibrated.submissions
    );
}

#[test]
fn a_frame_of_more_submissions_than_the_clock_keeps_still_lowers_the_rate() {
    let _lock = gpu_test_lock();
    let gpu = own_device();
    let renderer = Renderer::new(gpu.clone());
    let (effect, _) = registered(&renderer, heavy_effect());
    let size = RenderSize::new(40, 40).expect("size");
    let view = flat_source(&renderer, size);
    let pieces = (size.height / 2).min(crate::submission_planner::MAX_BANDS)
        * (size.width / 2).min(crate::submission_planner::MAX_TILE_COLUMNS);
    let quads = usize::try_from(pieces).expect("fits");
    let passes = crate::submission_clock::RECENT_SUBMISSIONS_KEPT / quads + 1;
    let mut graph = RenderGraph::new(size);
    let mut node = graph.add_source(SourceView::from_wgpu(&view));
    for _ in 0..passes {
        node = graph
            .add_effect(
                effect.clone(),
                node,
                EffectParams::new(),
                EffectRuntime::default(),
            )
            .expect("dependency exists");
    }
    graph.set_output(node).expect("output exists");
    let polled = || {
        let result = renderer.execute_graph(&graph).expect("the graph renders");
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
    };
    let _ = polled();
    gpu.rate().seed(1.0);
    let banded = with_watchdog(1.0, polled);
    assert_eq!(
        usize::try_from(banded.submissions).expect("fits"),
        passes * quads,
        "{banded:?}"
    );
    assert!(
        passes * quads > crate::submission_clock::RECENT_SUBMISSIONS_KEPT,
        "{banded:?}"
    );
    assert!(
        gpu.rate().ms_per_unit_pixel() < Some(2.0),
        "{:?}",
        gpu.rate().ms_per_unit_pixel()
    );
}

#[test]
fn a_loss_after_a_split_frame_whose_submissions_each_ran_briefly_is_not_reported_hung() {
    let _lock = gpu_test_lock();
    let gpu = own_device();
    let (size, frame_ms) = {
        let renderer = Renderer::new(gpu.clone());
        let (effect, _) = registered(&renderer, heavy_effect());
        let (size, _, frame_ms) =
            with_watchdog(UNSPLIT_WATCHDOG_MS, || busy_size(&renderer, &effect));
        (size, frame_ms)
    };
    let renderer = Renderer::new(gpu);
    let (effect, _) = registered(&renderer, heavy_effect());
    let view = flat_source(&renderer, size);
    with_watchdog(frame_ms, || {
        let (frame, _) = exported_frame(&renderer, &effect, &view, size);
        assert!(
            frame.submissions >= 4,
            "a frame that keeps the GPU busy for the whole {frame_ms:.1} ms watchdog is drawn in \
             {} submission(s): {frame:?}",
            frame.submissions
        );
        let longest = renderer.longest_submission().expect("timed submissions");
        let classified = renderer.classify_loss(RenderError::GpuWorkDropped);
        assert!(
            matches!(classified, RenderError::GpuWorkDropped),
            "a frame of {} submissions that ran {frame_ms:.1} ms unsplit is reported as {} \
             submission(s) of {:.1} ms: {classified:?}",
            frame.submissions,
            longest.submissions,
            longest.gpu_ms
        );
    });
}

#[test]
fn each_submission_of_a_split_frame_names_the_effect_encoded_into_it() {
    let _lock = gpu_test_lock();
    let gpu = own_device();
    let renderer = Renderer::new(gpu.clone());
    let (first, _) = registered(&renderer, heavy_effect());
    let (second, _) = registered(
        &renderer,
        EffectDef {
            id: "test.heavy.second".into(),
            ..heavy_effect()
        },
    );
    let size = RenderSize::new(256, 144).expect("size");
    let view = flat_source(&renderer, size);
    let mut graph = RenderGraph::new(size);
    let input = graph.add_source(SourceView::from_wgpu(&view));
    let middle = graph
        .add_effect(
            first.clone(),
            input,
            EffectParams::new(),
            EffectRuntime::default(),
        )
        .expect("dependency exists");
    let output = graph
        .add_effect(
            second.clone(),
            middle,
            EffectParams::new(),
            EffectRuntime::default(),
        )
        .expect("dependency exists");
    graph.set_output(output).expect("output exists");
    let frame = || {
        renderer
            .render_graph_yuv420p(&graph, &[], centred(ColorMetadata::default()))
            .expect("the frame renders")
            .wait_ready()
            .expect("the readback maps");
        renderer
            .take_frame_submissions()
            .expect("the frame's submissions")
    };
    let bands = crate::submission_planner::MAX_BANDS;
    seeded_at(&gpu, frame().cost.unit_pixels(), f64::from(bands * 2 - 8));
    let frame = frame();
    let name = |id: &crate::EffectId| vec![format!("{}@{}", id.as_str(), id.version())];
    let bands = usize::try_from(bands / 2).expect("fits");
    let kept = renderer.submission_clock.kept_effects();
    let named: Vec<Vec<String>> = kept
        [kept.len() - usize::try_from(frame.submissions).expect("fits")..]
        .iter()
        .filter(|effects| !effects.is_empty())
        .cloned()
        .collect();
    let second_from = named
        .iter()
        .position(|effects| *effects == name(&second))
        .unwrap_or(named.len());
    assert!(
        second_from >= bands
            && named.len() - second_from >= bands
            && named[..second_from]
                .iter()
                .all(|effects| *effects == name(&first))
            && named[second_from..]
                .iter()
                .all(|effects| *effects == name(&second)),
        "{frame:?}: {named:?}"
    );
}

fn frame_planned_at(
    gpu: &GpuContext,
    renderer: &Renderer,
    graph: &RenderGraph<'_>,
    budgets: f64,
) -> (crate::FrameSubmissions, f64, Vec<u8>) {
    let draw = || {
        let result = renderer.execute_graph(graph).expect("the graph renders");
        let submissions = renderer
            .take_frame_submissions()
            .expect("the frame's submissions");
        (submissions, read_rgba8(renderer, &result))
    };
    seeded_at(gpu, draw().0.cost.unit_pixels(), budgets);
    let _ = renderer.take_gpu_submission_stats();
    let (submissions, pixels) = draw();
    let longest = renderer
        .take_gpu_submission_stats()
        .max_predicted_ms
        .expect("a frame over the planning threshold is planned");
    (submissions, longest, pixels)
}

fn seeded_at(gpu: &GpuContext, unit_pixels: f64, budgets: f64) {
    let budget = f64::from(alpha2_gpu::submission_budget_ms());
    gpu.rate()
        .seed(budgets * budget / unit_pixels / alpha2_gpu::MARGIN);
}

const OVER_ROUNDING: f64 = 1.0 + 1e-9;

fn assert_within_the_budget(longest: f64, planned: crate::FrameSubmissions, what: &str) {
    let budget = f64::from(alpha2_gpu::submission_budget_ms());
    assert!(
        longest <= budget * OVER_ROUNDING,
        "{what} went in {} submission(s), the longest predicted at {longest:.0} ms against a \
         budget of {budget:.0} ms: {planned:?}",
        planned.submissions
    );
}

fn yuv_source(
    renderer: &Renderer,
    size: RenderSize,
    output: RenderSize,
) -> crate::PreparedSourceTexture {
    let (width, height) = (size.width as usize, size.height as usize);
    let y = (0..width * height)
        .map(|index| u8::try_from(16 + (index % width) * 200 / width).unwrap_or(16))
        .collect::<Vec<_>>();
    let u = vec![96_u8; width / 2 * height / 2];
    let v = vec![160_u8; width / 2 * height / 2];
    renderer
        .prepare_yuv420p_presented(
            Yuv420Frame {
                y: &y,
                u: &u,
                v: &v,
                y_stride: size.width,
                chroma_stride: size.width / 2,
                size,
                alpha: 1.0,
                color: Some(ColorMetadata::default()),
            },
            SourcePresentation {
                visible_rect: alpha2_media::Rect {
                    x: 0,
                    y: 0,
                    width: size.width,
                    height: size.height,
                },
                ..SourcePresentation::identity(output)
            },
            ColorPolicy::RejectHdr,
        )
        .expect("prepared YUV source")
        .0
}

fn converted_frame(
    renderer: &Renderer,
    source: RenderSize,
    output: RenderSize,
    convert_in_frame: bool,
) -> (crate::FrameSubmissions, Vec<u8>) {
    let (texture, pending) = yuv_source(renderer, source, output).into_parts();
    let pending = pending.expect("the conversion is pending");
    if !convert_in_frame {
        assert!(renderer.submit_pending_source(&pending));
    }
    let mut graph = RenderGraph::new(output);
    let input = graph.add_source(SourceView::from_wgpu(texture.view()));
    let composite = graph
        .add_composite(vec![GraphLayer {
            source: input,
            visual: visual([1.0, 1.0], 1.0),
            blend_mode: BlendMode::Normal,
        }])
        .expect("source exists");
    graph.set_output(composite).expect("output exists");
    let result = renderer
        .render_graph(&graph, &[&pending])
        .expect("the frame renders");
    let submissions = renderer
        .take_frame_submissions()
        .expect("the frame's submissions");
    (submissions, read_rgba8(renderer, &result))
}

#[test]
fn source_conversion_is_charged_to_the_frame_that_runs_it() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(64, 32).expect("size");
    let (converted_before, _) = converted_frame(&renderer, size, size, false);
    let (converted_in_frame, _) = converted_frame(&renderer, size, size, true);
    assert!(
        converted_in_frame.cost.unit_pixels() > converted_before.cost.unit_pixels(),
        "a frame that also converts its source is charged {converted_in_frame:?}, the same frame \
         without the conversion {converted_before:?}"
    );
}

#[test]
fn a_pack_is_charged_to_the_frame_it_packs() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(64, 32).expect("size");
    let view = flat_source(&renderer, size);
    let (effect, _) = registered(
        &renderer,
        runtime_tint_effect(
            "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
        ),
    );
    let graph = effect_graph(&view, &effect, size);
    drop(renderer.execute_graph(&graph).expect("the graph renders"));
    let unpacked = renderer
        .take_frame_submissions()
        .expect("the frame's submissions");
    renderer
        .render_graph_yuv420p(&graph, &[], centred(ColorMetadata::default()))
        .expect("the frame renders")
        .wait_ready()
        .expect("the readback maps");
    let packed = renderer
        .take_frame_submissions()
        .expect("the frame's submissions");
    assert!(
        packed.cost.unit_pixels() > unpacked.cost.unit_pixels(),
        "a packed frame is charged {packed:?}, the same frame unpacked {unpacked:?}"
    );
}

#[test]
fn a_frame_whose_conversions_pass_the_planning_threshold_is_planned() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let source = RenderSize::new(2048, 2048).expect("size");
    let output = RenderSize::new(256, 256).expect("size");
    let (planned, _) = converted_frame(&renderer, source, output, true);
    let (unplanned, _) = converted_frame(&renderer, source, output, false);
    assert!(
        planned.predicted_ms.is_some() && unplanned.predicted_ms.is_none(),
        "with its conversion {planned:?}, without {unplanned:?}"
    );
}

#[test]
fn a_frame_cut_after_its_conversions_draws_what_one_submission_draws() {
    let _lock = gpu_test_lock();
    let gpu = own_device();
    let renderer = Renderer::new(gpu.clone());
    let source = RenderSize::new(2048, 2048).expect("size");
    let output = RenderSize::new(256, 256).expect("size");
    let (whole, _) = with_watchdog(UNSPLIT_WATCHDOG_MS, || {
        converted_frame(&renderer, source, output, true)
    });
    assert_eq!(whole.submissions, 1, "{whole:?}");
    let (_, whole_picture) = converted_frame(&renderer, source, output, true);
    seeded_at(&gpu, whole.cost.unit_pixels(), 8.0);
    let _ = renderer.take_gpu_submission_stats();
    let (cut, cut_picture) = converted_frame(&renderer, source, output, true);
    let longest = renderer
        .take_gpu_submission_stats()
        .max_predicted_ms
        .expect("the frame is planned");
    assert!(cut.submissions >= 8, "{cut:?}");
    assert_within_the_budget(longest, cut, "a frame predicted at eight budgets");
    assert!(
        whole_picture == cut_picture,
        "the cut frame drew other bytes"
    );
}

fn layered_graph<'a>(
    views: &'a [wgpu::TextureView],
    layers: &[LayerTransform],
    size: RenderSize,
) -> RenderGraph<'a> {
    let mut graph = RenderGraph::new(size);
    let sources = views
        .iter()
        .map(|view| graph.add_source(SourceView::from_wgpu(view)))
        .collect::<Vec<_>>();
    let layers = layers
        .iter()
        .enumerate()
        .map(|(index, visual)| GraphLayer {
            source: sources[index % sources.len()],
            visual: *visual,
            blend_mode: BlendMode::Normal,
        })
        .collect();
    let output = graph.add_composite(layers).expect("sources exist");
    graph.set_output(output).expect("output exists");
    graph
}

fn layer_sources(renderer: &Renderer, size: RenderSize) -> [wgpu::TextureView; 2] {
    [
        flat_source(renderer, size),
        rgba_texture(renderer, [200, 40, 90, 255])
            .create_view(&wgpu::TextureViewDescriptor::default()),
    ]
}

#[test]
fn a_composite_predicted_past_the_budget_is_cut_into_submissions_under_it() {
    let _lock = gpu_test_lock();
    let gpu = own_device();
    let renderer = Renderer::new(gpu.clone());
    let size = RenderSize::new(512, 288).expect("size");
    let views = layer_sources(&renderer, size);
    let graph = layered_graph(&views, &[visual([1.0, 1.0], 0.5); 16], size);
    let (_, _, whole) = frame_planned_at(&gpu, &renderer, &graph, 0.5);
    let (planned, longest, cut) = frame_planned_at(&gpu, &renderer, &graph, 8.0);
    assert_within_the_budget(
        longest,
        planned,
        "sixteen whole-frame layers at eight budgets",
    );
    assert!(whole == cut, "the cut composite drew other bytes");
}

#[test]
fn a_composite_batch_past_the_budget_is_drawn_in_bands_of_the_same_picture() {
    let _lock = gpu_test_lock();
    let gpu = own_device();
    let renderer = Renderer::new(gpu.clone());
    let size = RenderSize::new(1920, 1080).expect("size");
    let views = layer_sources(&renderer, size);
    let small = |x: f32, y: f32, turn: f32| LayerTransform {
        position: [x, y],
        rotation_degrees: turn,
        ..visual([0.3, 0.4], 0.7)
    };
    let graph = layered_graph(
        &views,
        &[
            small(0.3, 0.3, 10.0),
            visual([1.0, 1.0], 0.5),
            small(0.55, 0.6, -20.0),
            small(0.6, 0.45, 35.0),
        ],
        size,
    );
    let (_, _, whole) = frame_planned_at(&gpu, &renderer, &graph, 0.5);
    let (planned, longest, cut) = frame_planned_at(&gpu, &renderer, &graph, 12.0);
    assert!(planned.submissions >= 12, "{planned:?}");
    assert_within_the_budget(longest, planned, "three batches at twelve budgets");
    assert!(whole == cut, "the banded batches drew other bytes");
}

#[test]
fn a_composite_is_charged_for_the_pixels_and_samples_its_layers_take() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(256, 144).expect("size");
    let views = layer_sources(&renderer, size);
    let minified = [rgba_image_texture(
        &renderer,
        RenderSize::new(2048, 1152).expect("size"),
        &vec![128; 2048 * 1152 * 4],
    )
    .create_view(&wgpu::TextureViewDescriptor::default())];
    let cost = |views: &[wgpu::TextureView], layer: LayerTransform| {
        drop(
            renderer
                .execute_graph(&layered_graph(views, &[layer], size))
                .expect("the composite renders"),
        );
        renderer
            .take_frame_submissions()
            .expect("the frame's submissions")
            .cost
            .unit_pixels()
    };
    let whole = cost(&views, visual([1.0, 1.0], 0.5));
    let quarter = cost(&views, visual([0.5, 0.5], 0.5));
    let sampled = cost(&minified, visual([1.0, 1.0], 0.5));
    assert!(
        quarter < whole && whole < sampled,
        "a quarter-frame layer is charged {quarter}, a whole-frame one {whole}, a whole-frame one \
         minified eight times {sampled}"
    );
}

fn vector_graph(
    items: &[alpha2_geometry::EvaluatedItem],
    size: RenderSize,
) -> RenderGraph<'static> {
    let scene = alpha2_geometry::build_scene(
        items,
        alpha2_geometry::CanvasSize::new(size.width, size.height),
    );
    let mut graph = RenderGraph::new(size);
    let node = graph.add_vector(Arc::new(scene));
    graph.set_output(node).expect("output exists");
    graph
}

fn whole_canvas_fills(fills: u8) -> Vec<alpha2_geometry::EvaluatedItem> {
    (0..fills)
        .flat_map(|index| {
            [
                super::vector::rectangle(0.5, 0.5, 1.0, 1.0),
                super::vector::fill(super::vector::solid(
                    f32::from(index) / f32::from(fills),
                    0.5,
                    0.25,
                    0.5,
                )),
            ]
        })
        .collect()
}

#[test]
fn a_vector_scene_predicted_past_the_budget_is_cut_into_submissions_under_it() {
    let _lock = gpu_test_lock();
    let gpu = own_device();
    let renderer = Renderer::new(gpu.clone());
    let size = RenderSize::new(3840, 2160).expect("size");
    let graph = vector_graph(&whole_canvas_fills(4), size);
    let (_, _, whole) = frame_planned_at(&gpu, &renderer, &graph, 0.5);
    let (planned, longest, cut) = frame_planned_at(&gpu, &renderer, &graph, 8.0);
    assert_within_the_budget(longest, planned, "a vector scene at eight budgets");
    assert!(whole == cut, "the cut scene drew other bytes");
}

#[test]
fn a_vector_scene_cut_between_and_inside_its_draws_draws_the_same_picture() {
    let _lock = gpu_test_lock();
    let gpu = own_device();
    let renderer = Renderer::new(gpu.clone());
    let size = RenderSize::new(1920, 1080).expect("size");
    let ellipse = |x: f64, turn: f64| {
        alpha2_geometry::EvaluatedItem::Geometry(alpha2_geometry::EvaluatedGeometry::Ellipse {
            center: alpha2_geometry::kurbo::Point::new(x, 0.5),
            size: alpha2_geometry::kurbo::Vec2::new(0.6, 0.35),
            rotation_degrees: turn,
        })
    };
    let faded = |items| {
        super::vector::group(
            items,
            alpha2_geometry::EvaluatedTransform {
                opacity: 0.6,
                ..alpha2_geometry::EvaluatedTransform::default()
            },
        )
    };
    let mut items = whole_canvas_fills(3);
    items.push(faded(vec![
        ellipse(0.4, 20.0),
        super::vector::fill(super::vector::gradient(
            2,
            [0.1, 0.2],
            [0.9, 0.7],
            alpha2_model::Spread::Pad,
        )),
        faded(vec![
            ellipse(0.6, -35.0),
            super::vector::fill(super::vector::solid(0.2, 0.7, 0.9, 0.8)),
        ]),
    ]));
    items.push(ellipse(0.5, 60.0));
    items.push(super::vector::fill(super::vector::gradient(
        3,
        [0.5, 0.5],
        [0.4, 0.0],
        alpha2_model::Spread::Reflect,
    )));
    for space in [BlendSpace::LinearLight, BlendSpace::SrgbEncoded] {
        let mut graph = vector_graph(&items, size);
        graph.set_blend_space(space);
        let (_, _, whole) = frame_planned_at(&gpu, &renderer, &graph, 0.5);
        let (planned, longest, cut) = frame_planned_at(&gpu, &renderer, &graph, 24.0);
        assert!(planned.submissions >= 24, "{space:?}: {planned:?}");
        assert_within_the_budget(longest, planned, "a scene of groups at twenty-four budgets");
        assert!(whole == cut, "{space:?}: the cut scene drew other bytes");
    }
}

#[test]
fn a_vector_scene_is_charged_for_what_it_draws() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let size = RenderSize::new(64, 64).expect("size");
    let cost = |items: &[alpha2_geometry::EvaluatedItem]| {
        let mut graph = vector_graph(items, size);
        graph.set_blend_space(BlendSpace::SrgbEncoded);
        drop(renderer.execute_graph(&graph).expect("the scene renders"));
        renderer
            .take_frame_submissions()
            .expect("the frame's submissions")
            .cost
            .unit_pixels()
    };
    let one = cost(&whole_canvas_fills(1));
    let many = cost(&whole_canvas_fills(64));
    let corner = cost(&[
        super::vector::rectangle(0.125, 0.125, 0.25, 0.25),
        super::vector::fill(super::vector::solid(0.5, 0.5, 0.25, 0.5)),
    ]);
    assert!(
        corner < one && one < many,
        "a fill of a sixteenth of the canvas is charged {corner} unit pixels, one of the whole \
         canvas {one}, sixty-four of it {many}"
    );
}

#[test]
fn a_pass_past_the_most_bands_is_cut_across_too_and_not_submitted_over_the_budget() {
    let _lock = gpu_test_lock();
    let gpu = own_device();
    let renderer = Renderer::new(gpu.clone());
    let (effect, _) = registered(&renderer, heavy_effect());
    let size = RenderSize::new(512, 288).expect("size");
    let view = flat_source(&renderer, size);
    let graph = effect_graph(&view, &effect, size);
    let (_, _, whole) = frame_planned_at(&gpu, &renderer, &graph, 0.5);
    let past_the_bands = 8.0 * f64::from(crate::submission_planner::MAX_BANDS);
    let (planned, longest, cut) = frame_planned_at(&gpu, &renderer, &graph, past_the_bands);
    assert_within_the_budget(
        longest,
        planned,
        &format!(
            "a pass predicted at {past_the_bands} budgets (watchdog {} ms)",
            alpha2_gpu::watchdog_ms()
        ),
    );
    assert!(whole == cut, "the tiles drew other bytes than the one pass");
}
