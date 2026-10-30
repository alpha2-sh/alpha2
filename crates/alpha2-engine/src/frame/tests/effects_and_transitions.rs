//! Clip effect chains, keyframed parameters, and transition nodes.

use super::*;

#[test]
fn evaluated_frame_executes_the_clip_effect_chain() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("effect compiles");
    let source = SourceId::new("source").expect("valid ID");
    let frame = FrameDesc {
        background: Some(alpha2_model::Rgb::BLACK),
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
                    params: constant_params([("tint", EffectValue::Color([0.5, 1.0, 1.0, 1.0]))]),
                }]),
                MediaTime::ZERO,
            ),
            blend_mode: ClipBlendMode::Normal,
            cache: alpha2_model::CacheIntent::Auto,
            masks: &[],
            matte: None,
            is_matte: false,
            generator: None,
            plan: None,
            source_composition: None,
        }],
        audio: Vec::new(),
        transitions: Vec::new(),
        captions: Vec::new(),
    };
    let input = rgba_texture(&renderer, [200, 100, 50, 255]);
    let view = input.create_view(&wgpu::TextureViewDescriptor::default());
    let provider = SingleFrame {
        source,
        texture: &view,
    };
    let output = renderer
        .render_frame(
            RenderSize::new(1, 1).expect("non-zero size"),
            &frame,
            &provider,
        )
        .expect("frame renders")
        .texture
        .expect("complete frame");
    assert_eq!(read_rgba8(&renderer, &output), [100, 100, 50, 255]);
}

#[test]
#[allow(clippy::too_many_lines)]
fn keyframed_effect_parameters_render_at_the_frame_time_and_clamp_only_overshoot() {
    use alpha2_model::{CubicBezier, Interpolation, Keyframe};

    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    // `gain` scales the source; its catalog range is 0..=1.
    pollster::block_on(renderer.register_effect(EffectDef {
        params: vec![ParamSpec {
            name: "gain".into(),
            param_type: ParamType::Float,
            default: ParamValue::Float(1.0),
            min: Some(0.0),
            max: Some(1.0),
            step: None,
            unit: ParamUnit::Normalized,
            enum_values: Vec::new(),
        }],
        ..runtime_tint_effect(
            "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return vec4f(src.rgb * params.gain, src.a); }",
        )
    }))
    .expect("effect compiles");
    let source = SourceId::new("source").expect("valid ID");
    let input = rgba_texture(&renderer, [255, 255, 255, 255]);
    let view = input.create_view(&wgpu::TextureViewDescriptor::default());
    let provider = SingleFrame {
        source: source.clone(),
        texture: &view,
    };
    let gain = |curve: Interpolation, start: f32| Animated::Keyframes {
        keyframes: vec![
            Keyframe {
                time: MediaTime(0),
                value: EffectValue::Float(start),
                interpolation: curve,
            },
            Keyframe {
                time: MediaTime(1_000),
                value: EffectValue::Float(1.0),
                interpolation: Interpolation::Hold,
            },
        ],
    };
    let render = |param: Animated<EffectValue>, time: i64| {
        let effects = Vec::leak(vec![ClipEffect {
            id: "test.tint".into(),
            version: 1,
            resources: BTreeMap::new(),
            params: BTreeMap::from([("gain".into(), param)]),
        }]);
        let frame = FrameDesc {
            background: Some(alpha2_model::Rgb::BLACK),
            blend_space: alpha2_model::BlendSpace::LinearLight,
            time: MediaTime(time),
            visual: vec![ActiveClip {
                clip_id: leak(ClipId::new("clip").expect("valid ID")),
                source_id: leak(source.clone()),
                track_id: leak(TrackId::new("track").expect("valid ID")),
                source_time: MediaTime::ZERO,
                visual: Some(visual(Vec2::new(1.0, 1.0), 1.0)),
                composition: chain(effects, MediaTime(time)),
                blend_mode: ClipBlendMode::Normal,
                cache: alpha2_model::CacheIntent::Auto,
                masks: &[],
                matte: None,
                is_matte: false,
                generator: None,
                plan: None,
                source_composition: None,
            }],
            audio: Vec::new(),
            transitions: Vec::new(),
            captions: Vec::new(),
        };
        renderer
            .render_frame(
                RenderSize::new(1, 1).expect("non-zero size"),
                &frame,
                &provider,
            )
            .map(|render| read_rgba8(&renderer, &render.texture.expect("complete frame")))
    };

    let ramp = || gain(Interpolation::Linear, 0.0);
    assert_eq!(render(ramp(), 0).expect("start"), [0, 0, 0, 255]);
    let middle = render(ramp(), 500).expect("middle");
    // The effect scales its sRGB-encoded input: half of 255.
    assert_rgba8_near(&middle, &[128, 128, 128, 255], 1);
    assert_eq!(render(ramp(), 1_000).expect("end"), [255, 255, 255, 255]);

    // An overshooting curve carries gain past 1 between the keyframes; the interpolated value is
    // clamped to the range instead of failing the frame.
    let back = Interpolation::CubicBezier(CubicBezier::new(0.34, 1.56, 0.64, 1.0));
    assert!(back.amount(0.7).expect("curve") > 1.0);
    assert_eq!(
        render(gain(back, 0.5), 700).expect("an overshoot renders"),
        [255, 255, 255, 255]
    );
    // An authored value outside the range is still an error, keyframed or not.
    assert!(matches!(
        render(gain(Interpolation::Linear, 2.0), 0),
        Err(RenderError::InvalidEffectParams)
    ));
    assert!(matches!(
        render(
            Animated::Constant {
                value: EffectValue::Float(1.5)
            },
            0
        ),
        Err(RenderError::InvalidEffectParams)
    ));
}

#[test]
fn evaluated_frame_executes_active_transition_nodes() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    let transition = pollster::block_on(renderer.register_effect(EffectDef {
        id: "test.frame-fade".into(),
        version: 1,
        kind: EffectKind::Transition,
        inputs: Vec::new(),
        params: Vec::new(),
            resources: Vec::new(),
        wgsl_body: "fn transition(uv: vec2f) -> vec4f { return mix(from_color(uv), to_color(uv), progress()); }".into(),
        tier_min: CapabilityTier::TierB,
        license: Some("MIT".into()),
    }))
    .expect("transition compiles");
    let source = SourceId::new("source").expect("valid ID");
    let from_clip = ClipId::new("from").expect("valid ID");
    let to_clip = ClipId::new("to").expect("valid ID");
    let active = |clip_id: ClipId, source_time: MediaTime| ActiveClip {
        clip_id: leak(clip_id),
        source_id: leak(source.clone()),
        track_id: leak(TrackId::new("track").expect("valid ID")),
        source_time,
        visual: Some(visual(Vec2::new(1.0, 1.0), 1.0)),
        composition: None,
        blend_mode: ClipBlendMode::Normal,
        cache: alpha2_model::CacheIntent::Auto,
        masks: &[],
        matte: None,
        is_matte: false,
        generator: None,
        plan: None,
        source_composition: None,
    };
    let frame = FrameDesc {
        background: Some(alpha2_model::Rgb::BLACK),
        blend_space: alpha2_model::BlendSpace::LinearLight,
        time: MediaTime::ZERO,
        visual: vec![
            active(from_clip.clone(), MediaTime::ZERO),
            active(to_clip.clone(), MediaTime(1)),
        ],
        audio: Vec::new(),
        captions: Vec::new(),
        transitions: vec![ActiveTransition {
            from_clip: leak(from_clip),
            to_clip: leak(to_clip),
            effect: ActiveEffect::new(
                leak(ClipEffect {
                    id: transition.as_str().into(),
                    version: transition.version(),
                    resources: BTreeMap::new(),
                    params: BTreeMap::new(),
                }),
                MediaTime::ZERO,
            ),
            progress: 0.5,
        }],
    };
    let from = rgba_texture(&renderer, [255, 0, 0, 255]);
    let to = rgba_texture(&renderer, [0, 0, 255, 255]);
    let from_view = from.create_view(&wgpu::TextureViewDescriptor::default());
    let to_view = to.create_view(&wgpu::TextureViewDescriptor::default());
    let provider = TimedFrames {
        source,
        first: &from_view,
        second: &to_view,
    };
    let output = renderer
        .render_frame(
            RenderSize::new(1, 1).expect("non-zero size"),
            &frame,
            &provider,
        )
        .expect("frame renders")
        .texture
        .expect("complete frame");
    let reference: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../tests/cases/render_reference.json"
    ))
    .expect("render reference JSON");
    let expected: Vec<u8> =
        serde_json::from_value(reference["transition_midpoint"].clone()).expect("RGBA reference");
    assert_rgba8_near(&read_rgba8(&renderer, &output), &expected, 1);
}
