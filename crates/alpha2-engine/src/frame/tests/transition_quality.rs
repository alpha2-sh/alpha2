use super::*;
use alpha2_render::SourceView;

struct Frames(Vec<wgpu::TextureView>);

impl FrameProvider for Frames {
    fn texture_for(&self, _: &SourceId, time: MediaTime) -> Option<SourceView<'_>> {
        self.0
            .get(usize::try_from(time.ticks()).ok()?)
            .map(SourceView::from_wgpu)
    }
}

fn clip(id: &str, time: i64, visual: EvaluatedVisual, mode: ClipBlendMode) -> ActiveClip<'static> {
    ActiveClip {
        clip_id: leak(ClipId::new(id).unwrap()),
        source_id: leak(SourceId::new("source").unwrap()),
        track_id: leak(TrackId::new("track").unwrap()),
        source_time: MediaTime(time),
        visual: Some(visual),
        composition: None,
        blend_mode: mode,
        cache: alpha2_model::CacheIntent::Auto,
        masks: &[],
        matte: None,
        is_matte: false,
        generator: None,
        source_composition: None,
    }
}

fn pixels(renderer: &Renderer, size: RenderSize, frame: &FrameDesc, frames: &Frames) -> Vec<u8> {
    let texture = renderer
        .render_frame(size, frame, frames)
        .unwrap()
        .texture
        .unwrap();
    read_rgba8(renderer, &texture)
}

#[test]
fn transition_endpoints_preserve_each_clips_visual_and_blend_mode() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_builtin_transitions()).unwrap();
    let textures = [[80, 120, 160, 200], [255, 80, 20, 255], [20, 100, 255, 255]]
        .map(|pixel| rgba_texture(&renderer, pixel));
    let frames = Frames(
        textures
            .iter()
            .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()))
            .collect(),
    );
    let size = RenderSize::new(16, 8).unwrap();
    let background = clip(
        "background",
        0,
        visual(Vec2::new(1.0, 1.0), 1.0),
        ClipBlendMode::Normal,
    );
    let mut from_visual = visual(Vec2::new(0.8, 0.7), 0.6);
    from_visual.position = Vec2::new(0.4, 0.6);
    from_visual.rotation_degrees = 20.0;
    let mut to_visual = visual(Vec2::new(0.4, 0.5), 0.3);
    to_visual.position = Vec2::new(0.7, 0.3);
    to_visual.rotation_degrees = -30.0;
    for (from_mode, to_mode) in [
        (ClipBlendMode::Normal, ClipBlendMode::Normal),
        (ClipBlendMode::Multiply, ClipBlendMode::Screen),
        (ClipBlendMode::Overlay, ClipBlendMode::Normal),
    ] {
        let from = clip("from", 1, from_visual, from_mode);
        let to = clip("to", 2, to_visual, to_mode);
        for (progress, endpoint) in [(0.0, &from), (1.0, &to)] {
            let reference = FrameDesc {
                background: alpha2_model::Rgb::BLACK,
                blend_space: alpha2_model::BlendSpace::LinearLight,
                time: MediaTime::ZERO,
                visual: vec![background, *endpoint],
                audio: vec![],
                transitions: vec![],
                captions: Vec::new(),
            };
            let actual = FrameDesc {
                background: alpha2_model::Rgb::BLACK,
                blend_space: alpha2_model::BlendSpace::LinearLight,
                visual: vec![background, from, to],
                transitions: vec![ActiveTransition {
                    from_clip: from.clip_id,
                    to_clip: to.clip_id,
                    effect: ActiveEffect::new(
                        leak(ClipEffect {
                            id: "alpha2.transition.fade".into(),
                            version: 1,
                            resources: BTreeMap::new(),
                            params: BTreeMap::new(),
                        }),
                        MediaTime::ZERO,
                    ),
                    progress,
                }],
                ..reference.clone()
            };
            assert_rgba8_near(
                &pixels(&renderer, size, &actual, &frames),
                &pixels(&renderer, size, &reference, &frames),
                2,
            );
        }
    }
}

#[test]
fn transition_opacity_is_applied_once_at_the_midpoint() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_builtin_transitions()).unwrap();
    let texture = rgba_texture(&renderer, [255, 255, 255, 255]);
    let frames = Frames(vec![
        texture.create_view(&wgpu::TextureViewDescriptor::default()),
    ]);
    let frame = FrameDesc {
        background: alpha2_model::Rgb::BLACK,
        blend_space: alpha2_model::BlendSpace::LinearLight,
        time: MediaTime::ZERO,
        visual: vec![
            clip(
                "from",
                0,
                visual(Vec2::new(1.0, 1.0), 0.25),
                ClipBlendMode::Normal,
            ),
            clip(
                "to",
                0,
                visual(Vec2::new(1.0, 1.0), 0.75),
                ClipBlendMode::Normal,
            ),
        ],
        audio: vec![],
        captions: Vec::new(),
        transitions: vec![ActiveTransition {
            from_clip: leak(ClipId::new("from").unwrap()),
            to_clip: leak(ClipId::new("to").unwrap()),
            effect: ActiveEffect::new(
                leak(ClipEffect {
                    id: "alpha2.transition.fade".into(),
                    version: 1,
                    resources: BTreeMap::new(),
                    params: BTreeMap::new(),
                }),
                MediaTime::ZERO,
            ),
            progress: 0.5,
        }],
    };
    // The fade's midpoint is white at 0.5 coverage. Over the black background in linear light
    // that is sRGB(0.5) = 188; opacity applied twice would be 0.25 coverage, sRGB(0.25) = 137.
    assert_rgba8_near(
        &pixels(&renderer, RenderSize::new(1, 1).unwrap(), &frame, &frames),
        &[188, 188, 188, 255],
        1,
    );
}

#[test]
fn chained_transitions_on_two_tracks_with_a_matted_endpoint_show_their_endpoints() {
    use alpha2_model::{Matte, MatteMode};
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_builtin_transitions()).unwrap();
    pollster::block_on(renderer.register_effect(alpha2_render::track_matte_definition())).unwrap();
    // A red, B green, C blue, the matte half covering, E grey, F orange.
    let textures = [
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 255, 128],
        [90, 90, 90, 255],
        [255, 140, 0, 255],
    ]
    .map(|pixel| rgba_texture(&renderer, pixel));
    let frames = Frames(
        textures
            .iter()
            .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()))
            .collect(),
    );
    let size = RenderSize::new(8, 8).unwrap();
    let full = || visual(Vec2::new(1.0, 1.0), 1.0);
    let red = clip("a", 0, full(), ClipBlendMode::Normal);
    let mut green = clip("b", 1, full(), ClipBlendMode::Normal);
    green.matte = Some(leak(Matte {
        source: ClipId::new("m").unwrap(),
        mode: MatteMode::Alpha,
    }));
    let blue = clip("c", 2, full(), ClipBlendMode::Normal);
    let mut matte = clip("m", 3, full(), ClipBlendMode::Normal);
    matte.is_matte = true;
    let grey = clip("e", 4, full(), ClipBlendMode::Normal);
    let orange = clip("f", 5, full(), ClipBlendMode::Screen);
    let fade = |from: &ActiveClip<'static>, to: &ActiveClip<'static>, progress| ActiveTransition {
        from_clip: from.clip_id,
        to_clip: to.clip_id,
        effect: ActiveEffect::new(
            leak(ClipEffect {
                id: "alpha2.transition.fade".into(),
                version: 1,
                resources: BTreeMap::new(),
                params: BTreeMap::new(),
            }),
            MediaTime::ZERO,
        ),
        progress,
    };
    let frame = |visual: Vec<ActiveClip<'static>>, transitions| FrameDesc {
        background: alpha2_model::Rgb::BLACK,
        blend_space: alpha2_model::BlendSpace::LinearLight,
        time: MediaTime::ZERO,
        visual,
        audio: vec![],
        transitions,
        captions: Vec::new(),
    };
    // Whether each transition has reached its end: A to B, B to C, E to F.
    for (to_green, to_blue, to_orange) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (false, false, true),
        (true, false, true),
        (true, true, true),
    ] {
        let progress = |done: bool| if done { 1.0 } else { 0.0 };
        let lower = if to_blue {
            blue
        } else if to_green {
            green
        } else {
            red
        };
        let upper = if to_orange { orange } else { grey };
        let reference = frame(vec![lower, matte, upper], vec![]);
        let actual = frame(
            vec![red, green, blue, matte, grey, orange],
            vec![
                fade(&red, &green, progress(to_green)),
                fade(&green, &blue, progress(to_blue)),
                fade(&grey, &orange, progress(to_orange)),
            ],
        );
        assert_rgba8_near(
            &pixels(&renderer, size, &actual, &frames),
            &pixels(&renderer, size, &reference, &frames),
            2,
        );
    }
}
