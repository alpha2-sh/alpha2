//! Layers: blend spaces, track mattes, masks, stacked clips, and intermediates they must not share.

use super::*;

/// Answers each source with its own texture, so a mixed-up binding is visible
/// as one clip's colour appearing in another clip's transformed slot.
struct DistinctSources<'a> {
    entries: Vec<(SourceId, &'a wgpu::TextureView)>,
}

impl FrameProvider for DistinctSources<'_> {
    fn texture_for(&self, source: &SourceId, _: MediaTime) -> Option<SourceView<'_>> {
        self.entries
            .iter()
            .find(|(id, _)| id == source)
            .map(|(_, view)| *view)
            .map(SourceView::from_wgpu)
    }
}

#[test]
fn the_project_blend_space_decides_how_a_frame_blends() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let backdrop = rgba_texture(&renderer, [128, 64, 32, 255]);
    let source = rgba_texture(&renderer, [64, 128, 192, 255]);
    let views = [&backdrop, &source]
        .map(|texture| texture.create_view(&wgpu::TextureViewDescriptor::default()));
    let ids = ["backdrop", "source"].map(|id| SourceId::new(id).expect("valid ID"));
    let provider = DistinctSources {
        entries: ids.iter().cloned().zip(views.iter()).collect(),
    };
    let clip = |index: usize, blend_mode| ActiveClip {
        clip_id: leak(ClipId::new(format!("clip-{index}")).expect("valid ID")),
        source_id: leak(ids[index].clone()),
        track_id: leak(TrackId::new(format!("track-{index}")).expect("valid ID")),
        source_time: MediaTime::ZERO,
        visual: Some(visual(Vec2::new(1.0, 1.0), 1.0)),
        composition: None,
        blend_mode,
        cache: alpha2_model::CacheIntent::Auto,
        masks: &[],
        matte: None,
        is_matte: false,
        generator: None,
        plan: None,
        source_composition: None,
    };
    let render = |blend_space| {
        let frame = FrameDesc {
            time: MediaTime::ZERO,
            background: alpha2_model::Rgb::BLACK,
            blend_space,
            visual: vec![
                clip(0, ClipBlendMode::Normal),
                clip(1, ClipBlendMode::Overlay),
            ],
            audio: Vec::new(),
            transitions: Vec::new(),
            captions: Vec::new(),
        };
        let output = renderer
            .render_frame(RenderSize::new(1, 1).expect("size"), &frame, &provider)
            .expect("frame renders")
            .texture
            .expect("complete frame");
        read_rgba8(&renderer, &output)
    };
    // The renderer's own default is linear light; the project decides.
    let linear = render(alpha2_model::BlendSpace::LinearLight);
    let encoded = render(alpha2_model::BlendSpace::SrgbEncoded);
    // W3C overlay of these two in sRGB-encoded values is (64, 64, 48); the `blend_modes` golden
    // holds the same pixel.
    assert_rgba8_near(&encoded, &[65, 64, 48, 255], 1);
    assert_ne!(linear, encoded);
}

#[test]
fn a_track_matte_keeps_its_clip_where_the_matte_says_and_is_never_drawn() {
    use alpha2_model::{Matte, MatteMode};

    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(alpha2_render::track_matte_definition()))
        .expect("the matte compiles");
    let size = RenderSize::new(2, 1).expect("size");
    // The clip is red; the matte clip is opaque white on the left and, on the right,
    // transparent for the alpha modes and opaque black for the luma modes.
    let red = rgba_image_texture(&renderer, size, &[255, 0, 0, 255, 255, 0, 0, 255]);
    let alpha_matte = rgba_image_texture(&renderer, size, &[255, 255, 255, 255, 0, 0, 0, 0]);
    let luma_matte = rgba_image_texture(&renderer, size, &[255, 255, 255, 255, 0, 0, 0, 255]);
    let views = [&red, &alpha_matte, &luma_matte]
        .map(|texture| texture.create_view(&wgpu::TextureViewDescriptor::default()));
    let ids = ["red", "alpha-matte", "luma-matte"].map(|id| SourceId::new(id).expect("id"));
    let provider = DistinctSources {
        entries: ids.iter().cloned().zip(views.iter()).collect(),
    };
    let matte_id = leak(ClipId::new("matte").expect("id"));
    let render = |mode: MatteMode, with_matte: bool| {
        let luma = matches!(mode, MatteMode::Luma | MatteMode::LumaInverted);
        let clip = |id: &'static ClipId, source: usize, matte, is_matte| ActiveClip {
            clip_id: id,
            source_id: leak(ids[source].clone()),
            track_id: leak(TrackId::new(format!("track-{source}")).expect("id")),
            source_time: MediaTime::ZERO,
            visual: Some(visual(Vec2::new(1.0, 1.0), 1.0)),
            composition: None,
            blend_mode: ClipBlendMode::Normal,
            cache: alpha2_model::CacheIntent::Auto,
            masks: &[],
            matte,
            is_matte,
            generator: None,
            plan: None,
            source_composition: None,
        };
        let target = clip(
            leak(ClipId::new("target").expect("id")),
            0,
            Some(leak(Matte {
                source: matte_id.clone(),
                mode,
            })),
            false,
        );
        let mut visual_clips = vec![target];
        // The matte clip sits above its target, where it would show if drawn.
        if with_matte {
            visual_clips.push(clip(matte_id, if luma { 2 } else { 1 }, None, true));
        }
        let frame = FrameDesc {
            time: MediaTime::ZERO,
            background: alpha2_model::Rgb::BLACK,
            blend_space: alpha2_model::BlendSpace::LinearLight,
            visual: visual_clips,
            audio: Vec::new(),
            transitions: Vec::new(),
            captions: Vec::new(),
        };
        let output = renderer
            .render_frame(size, &frame, &provider)
            .expect("frame renders")
            .texture
            .expect("complete frame");
        read_rgba8(&renderer, &output)
    };
    let (red, black) = ([255, 0, 0, 255], [0, 0, 0, 255]);
    for (mode, left, right) in [
        (MatteMode::Alpha, red, black),
        (MatteMode::AlphaInverted, black, red),
        (MatteMode::Luma, red, black),
        (MatteMode::LumaInverted, black, red),
    ] {
        assert_eq!(render(mode, true), [left, right].concat(), "{mode:?}");
    }
    assert_eq!(render(MatteMode::Alpha, false), [black, black].concat());
    assert_eq!(render(MatteMode::LumaInverted, false), [red, red].concat());
}

#[test]
fn clip_masks_are_applied_after_its_effects_in_source_space() {
    use alpha2_model::{
        Animated, AnimatedPoint, FillRule, MaskMode, RectangleGeometry, ShapeGeometry, ShapeMask,
    };

    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    for definition in alpha2_render::mask_combine_definitions() {
        pollster::block_on(renderer.register_effect(definition)).expect("the masks compile");
    }
    let size = RenderSize::new(4, 1).expect("size");
    let white = rgba_image_texture(&renderer, size, &[255; 16]);
    let view = white.create_view(&wgpu::TextureViewDescriptor::default());
    let source = SourceId::new("white").expect("id");
    let provider = SingleFrame {
        source: source.clone(),
        texture: &view,
    };
    // The left half of the source, moved right by the clip's transform: the mask moves with the
    // clip.
    let masks = Vec::leak(vec![ShapeMask {
        shape: ShapeGeometry::Rectangle(RectangleGeometry {
            center: AnimatedPoint::Constant {
                value: Vec2::new(0.25, 0.5),
            },
            size: Animated::Constant {
                value: Vec2::new(0.5, 2.0),
            },
            corner_radius: Animated::Constant { value: 0.0 },
            rotation_degrees: Animated::Constant { value: 0.0 },
        }),
        feather: Animated::Constant { value: 0.0 },
        invert: false,
        mode: MaskMode::Add,
        fill_rule: FillRule::Nonzero,
    }]);
    let frame = FrameDesc {
        time: MediaTime::ZERO,
        background: alpha2_model::Rgb::BLACK,
        blend_space: alpha2_model::BlendSpace::LinearLight,
        visual: vec![ActiveClip {
            clip_id: leak(ClipId::new("clip").expect("id")),
            source_id: leak(source.clone()),
            track_id: leak(TrackId::new("track").expect("id")),
            source_time: MediaTime::ZERO,
            visual: Some(EvaluatedVisual {
                position: Vec2::new(1.0, 0.5),
                ..visual(Vec2::new(1.0, 1.0), 1.0)
            }),
            composition: None,
            blend_mode: ClipBlendMode::Normal,
            cache: alpha2_model::CacheIntent::Auto,
            masks,
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
    let output = renderer
        .render_frame(size, &frame, &provider)
        .expect("frame renders")
        .texture
        .expect("complete frame");
    let pixels = read_rgba8(&renderer, &output);
    let red_of = |x: usize| pixels[x * 4];
    assert_eq!(
        (red_of(0), red_of(1), red_of(2), red_of(3)),
        (0, 0, 255, 255)
    );
}

#[test]
fn stacked_clips_keep_their_own_source_across_repeated_frames() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("effect compiles");
    let size = RenderSize::new(64, 64).expect("non-zero size");

    let colors = [[255_u8, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]];
    let textures = colors.map(|color| rgba_texture(&renderer, color));
    let views: Vec<wgpu::TextureView> = textures
        .iter()
        .map(|texture| texture.create_view(&wgpu::TextureViewDescriptor::default()))
        .collect();
    let sources: Vec<SourceId> = (0..3)
        .map(|index| SourceId::new(format!("source-{index}")).expect("valid ID"))
        .collect();
    let provider = DistinctSources {
        entries: sources
            .iter()
            .cloned()
            .zip(views.iter())
            .collect::<Vec<_>>(),
    };

    // Each clip carries an effect chain, which is what puts a pooled intermediate texture between
    // the source and the compositor.
    let tint = || ClipEffect {
        id: "test.tint".into(),
        version: 1,
        resources: BTreeMap::new(),
        params: constant_params([("tint", EffectValue::Color([1.0, 1.0, 1.0, 1.0]))]),
    };
    let visual_at = |scale: f32, rotation: f32| EvaluatedVisual {
        position: Vec2::new(0.5, 0.5),
        anchor: Vec2::new(0.5, 0.5),
        scale: Vec2::new(scale, scale),
        rotation_degrees: rotation,
        opacity: 1.0,
    };
    // Layer 0 fills the frame; layers 1 and 2 are rotated and shrunk.
    let frame = FrameDesc {
        background: alpha2_model::Rgb::BLACK,
        blend_space: alpha2_model::BlendSpace::LinearLight,
        time: MediaTime::ZERO,
        visual: [(1.0, 0.0), (0.5, 30.0), (0.25, -20.0)]
            .into_iter()
            .enumerate()
            .map(|(index, (scale, rotation))| ActiveClip {
                clip_id: leak(ClipId::new(format!("clip-{index}")).expect("valid ID")),
                source_id: leak(sources[index].clone()),
                track_id: leak(TrackId::new(format!("track-{index}")).expect("valid ID")),
                source_time: MediaTime::ZERO,
                visual: Some(visual_at(scale, rotation)),
                composition: chain(Vec::leak(vec![tint()]), MediaTime::ZERO),
                blend_mode: ClipBlendMode::Normal,
                cache: alpha2_model::CacheIntent::Auto,
                masks: &[],
                matte: None,
                is_matte: false,
                generator: None,
                plan: None,
                source_composition: None,
            })
            .collect(),
        audio: Vec::new(),
        transitions: Vec::new(),
        captions: Vec::new(),
    };

    // Repeat so that pooled intermediates are recycled between frames; a texture handed out while
    // still referenced shows up as a colour swap.
    for iteration in 0..8 {
        let output = renderer
            .render_frame(size, &frame, &provider)
            .expect("valid frame");
        let texture = output.texture.expect("complete frame has output");
        let pixels = read_rgba8(&renderer, &texture);

        // The topmost clip owns the centre; the frame edge belongs to clip 0.
        let at = |x: u32, y: u32| {
            let offset = ((y * size.width + x) * 4) as usize;
            [
                pixels[offset],
                pixels[offset + 1],
                pixels[offset + 2],
                pixels[offset + 3],
            ]
        };
        let centre = at(size.width / 2, size.height / 2);
        assert!(
            centre[2] > centre[0] && centre[2] > centre[1],
            "iteration {iteration}: centre belongs to the blue clip, got {centre:?}"
        );
        let corner = at(1, 1);
        assert!(
            corner[0] > corner[1] && corner[0] > corner[2],
            "iteration {iteration}: the corner belongs to the red clip, got {corner:?}"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_transformed_top_layer_never_samples_another_track_across_many_frames() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("effect compiles");
    // Export renders at full project size, where every intermediate is large enough that the pool
    // recycles aggressively between frames.
    let size = RenderSize::new(256, 144).expect("non-zero size");

    let colors = [
        [255_u8, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 0, 255],
        [255, 0, 255, 255],
    ];
    let textures: Vec<wgpu::Texture> = colors
        .iter()
        .map(|color| rgba_texture(&renderer, *color))
        .collect();
    let views: Vec<wgpu::TextureView> = textures
        .iter()
        .map(|texture| texture.create_view(&wgpu::TextureViewDescriptor::default()))
        .collect();
    let sources: Vec<SourceId> = (0..5)
        .map(|index| SourceId::new(format!("source-{index}")).expect("valid ID"))
        .collect();
    let provider = DistinctSources {
        entries: sources.iter().cloned().zip(views.iter()).collect(),
    };

    let clip = |index: usize, visual: EvaluatedVisual, effects: Vec<ClipEffect>| ActiveClip {
        clip_id: leak(ClipId::new(format!("clip-{index}")).expect("valid ID")),
        source_id: leak(sources[index].clone()),
        track_id: leak(TrackId::new(format!("track-{index}")).expect("valid ID")),
        source_time: MediaTime::ZERO,
        visual: Some(visual),
        composition: chain(Vec::leak(effects), MediaTime::ZERO),
        blend_mode: ClipBlendMode::Normal,
        cache: alpha2_model::CacheIntent::Auto,
        masks: &[],
        matte: None,
        is_matte: false,
        generator: None,
        plan: None,
        source_composition: None,
    };
    let faint = |opacity: f32| EvaluatedVisual {
        position: Vec2::new(0.5, 0.5),
        anchor: Vec2::new(0.5, 0.5),
        scale: Vec2::new(1.0, 1.0),
        rotation_degrees: 0.0,
        opacity,
    };
    let top = EvaluatedVisual {
        position: Vec2::new(0.75, 0.5),
        anchor: Vec2::new(0.5, 0.5),
        scale: Vec2::new(0.3, 0.3),
        rotation_degrees: 30.0,
        opacity: 1.0,
    };
    let tint = ClipEffect {
        id: "test.tint".into(),
        version: 1,
        resources: BTreeMap::new(),
        params: constant_params([("tint", EffectValue::Color([1.0, 1.0, 1.0, 1.0]))]),
    };
    let frame = FrameDesc {
        background: alpha2_model::Rgb::BLACK,
        blend_space: alpha2_model::BlendSpace::LinearLight,
        time: MediaTime::ZERO,
        visual: vec![
            clip(0, faint(0.2), Vec::new()),
            clip(1, faint(0.2), Vec::new()),
            clip(2, faint(0.2), Vec::new()),
            clip(3, faint(1.0), vec![tint]),
            clip(4, top, Vec::new()),
        ],
        audio: Vec::new(),
        transitions: Vec::new(),
        captions: Vec::new(),
    };

    // A recycled or mis-bound texture shows up as another clip's colour.
    let expected = colors[4];
    for iteration in 0..24 {
        let output = renderer
            .render_frame(size, &frame, &provider)
            .expect("valid frame");
        let texture = output.texture.expect("complete frame has output");
        let pixels = read_rgba8(&renderer, &texture);
        let x = size.width * 3 / 4;
        let y = size.height / 2;
        let offset = ((y * size.width + x) * 4) as usize;
        let sampled = [
            pixels[offset],
            pixels[offset + 1],
            pixels[offset + 2],
            pixels[offset + 3],
        ];
        for channel in 0..3 {
            let difference = i32::from(sampled[channel]) - i32::from(expected[channel]);
            assert!(
                difference.abs() <= 8,
                "iteration {iteration}: top layer sampled {sampled:?}, expected {expected:?}"
            );
        }
    }
}

#[test]
fn two_effected_clips_do_not_share_an_intermediate_texture() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("effect compiles");
    let size = RenderSize::new(256, 144).expect("non-zero size");

    let colors = [
        [255_u8, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 0, 255],
        [255, 0, 255, 255],
    ];
    let textures = colors.map(|color| rgba_texture(&renderer, color));
    let views = textures
        .each_ref()
        .map(|texture| texture.create_view(&wgpu::TextureViewDescriptor::default()));
    let sources =
        [0, 1, 2, 3, 4].map(|index| SourceId::new(format!("source-{index}")).expect("valid ID"));
    let provider = DistinctSources {
        entries: sources.iter().cloned().zip(views.iter()).collect(),
    };

    let tint = || ClipEffect {
        id: "test.tint".into(),
        version: 1,
        resources: BTreeMap::new(),
        params: constant_params([("tint", EffectValue::Color([1.0, 1.0, 1.0, 1.0]))]),
    };
    let clip = |index: usize, visual: EvaluatedVisual, effects: Vec<ClipEffect>| ActiveClip {
        clip_id: leak(ClipId::new(format!("clip-{index}")).expect("valid ID")),
        source_id: leak(sources[index].clone()),
        track_id: leak(TrackId::new(format!("track-{index}")).expect("valid ID")),
        source_time: MediaTime::ZERO,
        visual: Some(visual),
        composition: chain(Vec::leak(effects), MediaTime::ZERO),
        blend_mode: ClipBlendMode::Normal,
        cache: alpha2_model::CacheIntent::Auto,
        masks: &[],
        matte: None,
        is_matte: false,
        generator: None,
        plan: None,
        source_composition: None,
    };
    let faint = |opacity: f32| EvaluatedVisual {
        position: Vec2::new(0.5, 0.5),
        anchor: Vec2::new(0.5, 0.5),
        scale: Vec2::new(1.0, 1.0),
        rotation_degrees: 0.0,
        opacity,
    };
    let top = EvaluatedVisual {
        position: Vec2::new(0.75, 0.5),
        anchor: Vec2::new(0.5, 0.5),
        scale: Vec2::new(0.3, 0.3),
        rotation_degrees: 30.0,
        opacity: 1.0,
    };
    // Both the top clip and the one directly beneath it carry an effect, so two pooled
    // intermediates feed adjacent composite layers.
    let frame = FrameDesc {
        background: alpha2_model::Rgb::BLACK,
        blend_space: alpha2_model::BlendSpace::LinearLight,
        time: MediaTime::ZERO,
        visual: vec![
            clip(0, faint(0.2), Vec::new()),
            clip(1, faint(0.2), Vec::new()),
            clip(2, faint(0.2), Vec::new()),
            clip(3, faint(1.0), vec![tint()]),
            clip(4, top, vec![tint()]),
        ],
        audio: Vec::new(),
        transitions: Vec::new(),
        captions: Vec::new(),
    };

    let expected = colors[4];
    let below = colors[3];
    for iteration in 0..24 {
        let output = renderer
            .render_frame(size, &frame, &provider)
            .expect("valid frame");
        let texture = output.texture.expect("complete frame has output");
        let pixels = read_rgba8(&renderer, &texture);
        let offset = (((size.height / 2) * size.width + size.width * 3 / 4) * 4) as usize;
        let sampled: [u8; 4] = pixels[offset..offset + 4].try_into().expect("a pixel");
        let matches = |want: [u8; 4]| {
            (0..3)
                .all(|channel| (i32::from(sampled[channel]) - i32::from(want[channel])).abs() <= 8)
        };
        assert!(
            !matches(below),
            "iteration {iteration}: the top layer shows the clip below it ({sampled:?})"
        );
        assert!(
            matches(expected),
            "iteration {iteration}: top layer sampled {sampled:?}, expected {expected:?}"
        );
    }
}

/// Five stacked clips: three faint backgrounds, one effected clip, and an
/// effected clip on top that is rotated, shrunk, and pushed to the right.
fn stacked_effect_frame(sources: &[SourceId]) -> FrameDesc<'static> {
    let tint = || ClipEffect {
        id: "test.tint".into(),
        version: 1,
        resources: BTreeMap::new(),
        params: constant_params([("tint", EffectValue::Color([1.0, 1.0, 1.0, 1.0]))]),
    };
    let flat = |opacity: f32| EvaluatedVisual {
        position: Vec2::new(0.5, 0.5),
        anchor: Vec2::new(0.5, 0.5),
        scale: Vec2::new(1.0, 1.0),
        rotation_degrees: 0.0,
        opacity,
    };
    let clip = |index: usize, visual: EvaluatedVisual, effects: Vec<ClipEffect>| ActiveClip {
        clip_id: leak(ClipId::new(format!("clip-{index}")).expect("valid ID")),
        source_id: leak(sources[index].clone()),
        track_id: leak(TrackId::new(format!("track-{index}")).expect("valid ID")),
        source_time: MediaTime::ZERO,
        visual: Some(visual),
        composition: chain(Vec::leak(effects), MediaTime::ZERO),
        blend_mode: ClipBlendMode::Normal,
        cache: alpha2_model::CacheIntent::Auto,
        masks: &[],
        matte: None,
        is_matte: false,
        generator: None,
        plan: None,
        source_composition: None,
    };
    FrameDesc {
        background: alpha2_model::Rgb::BLACK,
        blend_space: alpha2_model::BlendSpace::LinearLight,
        time: MediaTime::ZERO,
        visual: vec![
            clip(0, flat(0.2), Vec::new()),
            clip(1, flat(0.2), Vec::new()),
            clip(2, flat(0.2), Vec::new()),
            clip(3, flat(1.0), vec![tint()]),
            clip(
                4,
                EvaluatedVisual {
                    position: Vec2::new(0.75, 0.5),
                    anchor: Vec2::new(0.5, 0.5),
                    scale: Vec2::new(0.3, 0.3),
                    rotation_degrees: 30.0,
                    opacity: 1.0,
                },
                vec![tint()],
            ),
        ],
        audio: Vec::new(),
        transitions: Vec::new(),
        captions: Vec::new(),
    }
}

#[test]
fn stacked_effect_layers_survive_per_frame_source_uploads_without_a_sync() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("effect compiles");
    let size = RenderSize::new(256, 144).expect("non-zero size");
    let colors = [
        [255_u8, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 0, 255],
        [255, 0, 255, 255],
    ];
    let sources: Vec<SourceId> = (0..5)
        .map(|index| SourceId::new(format!("source-{index}")).expect("valid ID"))
        .collect();
    let frame = stacked_effect_frame(&sources);

    // Queue several frames before reading any back, as a browser host does, so a cross-frame pool
    // hazard is not hidden by a per-frame sync.
    let mut pending = Vec::new();
    let mut retained_sources = Vec::new();
    for _ in 0..12 {
        let textures: Vec<wgpu::Texture> = colors
            .iter()
            .map(|color| rgba_texture(&renderer, *color))
            .collect();
        let views: Vec<wgpu::TextureView> = textures
            .iter()
            .map(|texture| texture.create_view(&wgpu::TextureViewDescriptor::default()))
            .collect();
        {
            let provider = DistinctSources {
                entries: sources.iter().cloned().zip(views.iter()).collect(),
            };
            let output = renderer
                .render_frame(size, &frame, &provider)
                .expect("valid frame");
            pending.push(output.texture.expect("complete frame has output"));
        }
        retained_sources.push((textures, views));
    }

    let expected = colors[4];
    let below = colors[3];
    for (iteration, rendered) in pending.iter().enumerate() {
        let pixels = read_rgba8(&renderer, rendered);
        let offset = (((size.height / 2) * size.width + size.width * 3 / 4) * 4) as usize;
        let sampled = [
            pixels[offset],
            pixels[offset + 1],
            pixels[offset + 2],
            pixels[offset + 3],
        ];
        let matches = |want: [u8; 4]| {
            (0..3)
                .all(|channel| (i32::from(sampled[channel]) - i32::from(want[channel])).abs() <= 8)
        };
        assert!(
            !matches(below),
            "frame {iteration}: the top layer shows the clip below it ({sampled:?})"
        );
        assert!(
            matches(expected),
            "frame {iteration}: top layer sampled {sampled:?}, expected {expected:?}"
        );
    }
}

/// Answers each source with its own texture, keyed by the source alone: a
/// still, the same at every time.
struct StillSources<'a> {
    entries: Vec<(SourceId, &'a wgpu::TextureView)>,
}

impl FrameProvider for StillSources<'_> {
    fn texture_for(&self, source: &SourceId, _: MediaTime) -> Option<SourceView<'_>> {
        self.entries
            .iter()
            .find(|(id, _)| id == source)
            .map(|(_, view)| *view)
            .map(SourceView::from_wgpu)
    }

    fn source_key(&self, source: &SourceId, _: MediaTime) -> Option<alpha2_render::RenderCacheKey> {
        self.entries
            .iter()
            .position(|(id, _)| id == source)
            .map(|index| {
                alpha2_render::RenderCacheKey::source(u64::try_from(index).expect("small") + 1)
            })
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_matte_is_drawn_once_where_it_is_named_and_mask_and_matte_keep_their_cache_keys() {
    use alpha2_model::{Animated, Matte, MatteMode, ShapeMask};

    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(alpha2_render::track_matte_definition()))
        .expect("the matte compiles");
    for definition in alpha2_render::mask_combine_definitions() {
        pollster::block_on(renderer.register_effect(definition)).expect("the masks compile");
    }
    let size = RenderSize::new(2, 1).expect("size");
    let red = rgba_image_texture(&renderer, size, &[255, 0, 0, 255, 255, 0, 0, 255]);
    let blue = rgba_image_texture(&renderer, size, &[0, 0, 255, 255, 0, 0, 255, 255]);
    let matte = rgba_image_texture(&renderer, size, &[255, 255, 255, 255, 0, 0, 0, 0]);
    let views = [&red, &blue, &matte, &matte]
        .map(|texture| texture.create_view(&wgpu::TextureViewDescriptor::default()));
    let ids = ["red", "blue", "matte", "matte-copy"].map(|id| SourceId::new(id).expect("id"));
    let all = StillSources {
        entries: ids.iter().cloned().zip(views.iter()).collect(),
    };
    let clip = |id: &str, source: usize, matte: Option<&str>, is_matte: bool| ActiveClip {
        clip_id: leak(ClipId::new(id).expect("id")),
        source_id: leak(ids[source].clone()),
        track_id: leak(TrackId::new(format!("track-{id}")).expect("id")),
        source_time: MediaTime::ZERO,
        visual: Some(visual(Vec2::new(1.0, 1.0), 1.0)),
        composition: None,
        blend_mode: ClipBlendMode::Normal,
        cache: alpha2_model::CacheIntent::Auto,
        masks: &[],
        matte: matte.map(|source| {
            leak(Matte {
                source: ClipId::new(source).expect("id"),
                mode: MatteMode::Alpha,
            })
        }),
        is_matte,
        generator: None,
        plan: None,
        source_composition: None,
    };
    let frame = |time: i64, visual: Vec<ActiveClip<'static>>| FrameDesc {
        time: MediaTime(time),
        background: alpha2_model::Rgb::BLACK,
        blend_space: alpha2_model::BlendSpace::LinearLight,
        visual,
        audio: Vec::new(),
        transitions: Vec::new(),
        captions: Vec::new(),
    };
    let render = |renderer: &Renderer, frame: &FrameDesc, provider: &dyn FrameProvider| {
        let nodes = renderer.diagnostics().graph_nodes;
        let output = renderer
            .render_frame(size, frame, provider)
            .expect("frame renders")
            .texture
            .map(|texture| read_rgba8(renderer, &texture));
        (output, renderer.diagnostics().graph_nodes - nodes)
    };

    // Two clips sharing one matte draw it once; with a copy each, twice.
    let shared = frame(
        0,
        vec![
            clip("a", 0, Some("m"), false),
            clip("b", 1, Some("m"), false),
            clip("m", 2, None, true),
        ],
    );
    let copies = frame(
        0,
        vec![
            clip("a", 0, Some("m"), false),
            clip("b", 1, Some("m2"), false),
            clip("m", 2, None, true),
            clip("m2", 3, None, true),
        ],
    );
    let (shared_picture, shared_nodes) = render(&renderer, &shared, &all);
    let (copies_picture, copies_nodes) = render(&renderer, &copies, &all);
    assert_eq!(shared_picture, copies_picture);
    assert!(
        shared_nodes < copies_nodes,
        "a shared matte draws {shared_nodes} nodes, two copies {copies_nodes}"
    );

    let without_matte_source = StillSources {
        entries: vec![(ids[0].clone(), &views[0])],
    };
    let (picture, _) = render(
        &renderer,
        &frame(0, vec![clip("a", 0, None, false), clip("m", 2, None, true)]),
        &without_matte_source,
    );
    assert_eq!(picture, Some([255, 0, 0, 255, 255, 0, 0, 255].to_vec()));

    // A constant mask and a matte draw the same at any time, so a later frame of keyed stills is
    // found whole in the result cache.
    renderer.set_result_cache_budget(1 << 20);
    let masks: &'static [ShapeMask] = Vec::leak(vec![ShapeMask {
        shape: alpha2_model::ShapeGeometry::Ellipse(alpha2_model::EllipseGeometry {
            center: alpha2_model::AnimatedPoint::Constant {
                value: Vec2::new(0.5, 0.5),
            },
            size: Animated::Constant {
                value: Vec2::new(0.8, 0.8),
            },
            rotation_degrees: Animated::Constant { value: 0.0 },
        }),
        feather: Animated::Constant { value: 0.0 },
        invert: false,
        mode: alpha2_model::MaskMode::default(),
        fill_rule: alpha2_model::FillRule::Nonzero,
    }]);
    let masked = |time| {
        let mut target = clip("a", 0, Some("m"), false);
        target.masks = masks;
        frame(time, vec![target, clip("m", 2, None, true)])
    };
    let (first, drawn) = render(&renderer, &masked(0), &all);
    assert!(drawn > 0);
    let (later, drawn) = render(&renderer, &masked(4_000), &all);
    assert_eq!(
        (later, drawn),
        (first, 0),
        "the frame at another time is found"
    );
}
