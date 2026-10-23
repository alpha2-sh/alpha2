//! Effect and pass timing, which never changes what is rendered.

use super::*;

#[test]
fn effect_timing_instrumentation_never_changes_the_rendered_result() {
    // Timing is on and every frame draws a different picture, so a submission that timing broke
    // cannot read back a stale target that happens to match.
    let _lock = gpu_test_lock();
    let register = |renderer: &mut Renderer| {
        pollster::block_on(renderer.register_effect(runtime_tint_effect(
            "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
        )))
        .expect("effect compiles");
    };
    let mut timed = Renderer::new(gpu());
    register(&mut timed);
    timed.set_pass_timing_enabled(true);
    let mut untimed = Renderer::new(gpu());
    register(&mut untimed);
    let size = RenderSize::new(64, 64).expect("non-zero size");
    let source = SourceId::new("source").expect("valid ID");
    let input = rgba_texture(&timed, [40, 160, 240, 255]);
    let view = input.create_view(&wgpu::TextureViewDescriptor::default());
    let provider = SingleFrame {
        source: source.clone(),
        texture: &view,
    };
    let frame = |iteration: u8| {
        let level = f32::from(iteration + 1) / 33.0;
        FrameDesc {
            background: alpha2_model::Rgb::BLACK,
            blend_space: alpha2_model::BlendSpace::LinearLight,
            time: MediaTime::ZERO,
            visual: vec![ActiveClip {
                clip_id: leak(ClipId::new("clip").expect("valid ID")),
                source_id: leak(source.clone()),
                track_id: leak(TrackId::new("track").expect("valid ID")),
                source_time: MediaTime::ZERO,
                visual: Some(visual(Vec2::new(1.0, 1.0), 1.0)),
                composition: chain(
                    Vec::leak(vec![ClipEffect {
                        id: "test.tint".into(),
                        version: 1,
                        resources: BTreeMap::new(),
                        params: constant_params([(
                            "tint",
                            EffectValue::Color([level, level, level, 1.0]),
                        )]),
                    }]),
                    MediaTime::ZERO,
                ),
                blend_mode: ClipBlendMode::Normal,
                cache: alpha2_model::CacheIntent::Auto,
                masks: &[],
                matte: None,
                is_matte: false,
                generator: None,
                shapes: None,
                source_composition: None,
            }],
            audio: Vec::new(),
            transitions: Vec::new(),
            captions: Vec::new(),
        }
    };
    let pixels = |renderer: &Renderer, frame: &FrameDesc| {
        let output = renderer
            .render_frame(size, frame, &provider)
            .expect("valid frame");
        read_rgba8(
            renderer,
            &output.texture.expect("complete frame has output"),
        )
    };

    // Each frame is read back before the next, so the probe is taken frame after frame; whether it
    // was taken must not be observable in the output.
    let mut previous: Option<Vec<u8>> = None;
    for iteration in 0..32 {
        let frame = frame(iteration);
        let expected = pixels(&untimed, &frame);
        let got = pixels(&timed, &frame);
        assert_eq!(
            got, expected,
            "iteration {iteration}: instrumentation changed the result"
        );
        assert_ne!(
            Some(&got),
            previous.as_ref(),
            "iteration {iteration} drew a new picture"
        );
        previous = Some(got);
    }
    assert!(
        !timed.diagnostics().passes.is_empty(),
        "the passes were timed"
    );
}

#[test]
fn pass_timing_is_off_by_default_and_never_alters_the_result() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("effect compiles");
    let size = RenderSize::new(32, 32).expect("non-zero size");
    let source = SourceId::new("source").expect("valid ID");
    let input = rgba_texture(&renderer, [10, 90, 200, 255]);
    let view = input.create_view(&wgpu::TextureViewDescriptor::default());
    let provider = SingleFrame {
        source: source.clone(),
        texture: &view,
    };
    let frame = FrameDesc {
        background: alpha2_model::Rgb::BLACK,
        blend_space: alpha2_model::BlendSpace::LinearLight,
        time: MediaTime::ZERO,
        visual: vec![ActiveClip {
            clip_id: leak(ClipId::new("clip").expect("valid ID")),
            source_id: leak(source.clone()),
            track_id: leak(TrackId::new("track").expect("valid ID")),
            source_time: MediaTime::ZERO,
            visual: Some(visual(Vec2::new(1.0, 1.0), 1.0)),
            composition: chain(
                Vec::leak(vec![ClipEffect {
                    id: "test.tint".into(),
                    version: 1,
                    resources: BTreeMap::new(),
                    params: constant_params([("tint", EffectValue::Color([1.0, 1.0, 1.0, 1.0]))]),
                }]),
                MediaTime::ZERO,
            ),
            blend_mode: ClipBlendMode::Normal,
            cache: alpha2_model::CacheIntent::Auto,
            masks: &[],
            matte: None,
            is_matte: false,
            generator: None,
            shapes: None,
            source_composition: None,
        }],
        audio: Vec::new(),
        transitions: Vec::new(),
        captions: Vec::new(),
    };

    // The default path records nothing, so a host that never asks for timing pays for none of
    // it.
    assert!(!renderer.pass_timing_enabled());
    let render = || {
        let output = renderer
            .render_frame(size, &frame, &provider)
            .expect("valid frame");
        read_rgba8(&renderer, &output.texture.expect("complete frame"))
    };
    let baseline = render();
    assert!(
        renderer.diagnostics().passes.is_empty(),
        "no pass history is collected by default"
    );

    renderer.set_pass_timing_enabled(true);
    assert!(renderer.pass_timing_enabled());
    let timed = render();
    assert_eq!(timed, baseline, "timing must not change the result");
    let passes = renderer.diagnostics().passes;
    assert!(!passes.is_empty(), "enabled timing records passes");
    assert!(
        passes
            .iter()
            .all(|pass| pass.label == "effect" || pass.label == "composite"),
        "labels stay stable: {passes:?}"
    );

    renderer.set_pass_timing_enabled(false);
    assert_eq!(
        renderer.diagnostics().passes,
        [] as [alpha2_render::PassTiming; 0]
    );
    assert_eq!(render(), baseline);
    assert_eq!(
        renderer.diagnostics().passes,
        [] as [alpha2_render::PassTiming; 0]
    );
}
