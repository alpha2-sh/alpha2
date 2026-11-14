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
        data: alpha2_eval::SampleContext::NONE,
        correction: None,
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
        plan: None,
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
    renderer.register_builtin_transitions().unwrap();
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
                background: Some(alpha2_model::Rgb::BLACK),
                blend_space: alpha2_model::BlendSpace::LinearLight,
                time: MediaTime::ZERO,
                visual: vec![background, *endpoint],
                audio: vec![],
                transitions: vec![],
                captions: Vec::new(),
            };
            let actual = FrameDesc {
                background: Some(alpha2_model::Rgb::BLACK),
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
                        alpha2_eval::SampleContext::NONE,
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
    renderer.register_builtin_transitions().unwrap();
    let texture = rgba_texture(&renderer, [255, 255, 255, 255]);
    let frames = Frames(vec![
        texture.create_view(&wgpu::TextureViewDescriptor::default()),
    ]);
    let frame = FrameDesc {
        background: Some(alpha2_model::Rgb::BLACK),
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
                alpha2_eval::SampleContext::NONE,
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
    renderer.register_builtin_transitions().unwrap();
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
            alpha2_eval::SampleContext::NONE,
        ),
        progress,
    };
    let frame = |visual: Vec<ActiveClip<'static>>, transitions| FrameDesc {
        background: Some(alpha2_model::Rgb::BLACK),
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

fn fade_between(
    from: &ActiveClip<'static>,
    to: &ActiveClip<'static>,
    progress: f32,
) -> ActiveTransition<'static> {
    ActiveTransition {
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
            alpha2_eval::SampleContext::NONE,
        ),
        progress,
    }
}

const LIST_ORDERS: [[usize; 3]; 6] = [
    [0, 1, 2],
    [0, 2, 1],
    [1, 0, 2],
    [1, 2, 0],
    [2, 0, 1],
    [2, 1, 0],
];

type Tournament = fn(
    &ActiveClip<'static>,
    &ActiveClip<'static>,
    &ActiveClip<'static>,
    [f32; 3],
) -> [ActiveTransition<'static>; 3];

#[test]
fn three_overlapping_clips_draw_one_picture_in_any_list_order() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    renderer.register_builtin_transitions().unwrap();
    // B green, C blue, A red, drawn in that frame order.
    let textures = [[0, 255, 0, 255], [0, 0, 255, 255], [255, 0, 0, 255]]
        .map(|pixel| rgba_texture(&renderer, pixel));
    let frames = Frames(
        textures
            .iter()
            .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()))
            .collect(),
    );
    let size = RenderSize::new(4, 4).unwrap();
    let full = || visual(Vec2::new(1.0, 1.0), 1.0);
    let b = clip("b", 0, full(), ClipBlendMode::Normal);
    let c = clip("c", 1, full(), ClipBlendMode::Normal);
    let a = clip("a", 2, full(), ClipBlendMode::Normal);
    let draw = |transitions: Vec<ActiveTransition<'static>>| {
        let frame = FrameDesc {
            background: Some(alpha2_model::Rgb::BLACK),
            blend_space: alpha2_model::BlendSpace::SrgbEncoded,
            time: MediaTime::ZERO,
            visual: vec![b, c, a],
            audio: vec![],
            transitions,
            captions: Vec::new(),
        };
        pixels(&renderer, size, &frame, &frames)[..4].to_vec()
    };
    // At progress 0.5 the clip led by both others draws half its first leader and a quarter of
    // each other clip.
    let tournaments: [(&str, [u8; 4], Tournament); 2] = [
        (
            "a leads b and c, b leads c",
            [128, 64, 64, 255],
            |b, c, a, p| {
                [
                    fade_between(b, c, p[0]),
                    fade_between(a, b, p[1]),
                    fade_between(a, c, p[2]),
                ]
            },
        ),
        (
            "b leads c and a, c leads a",
            [64, 128, 64, 255],
            |b, c, a, p| {
                [
                    fade_between(b, c, p[0]),
                    fade_between(b, a, p[1]),
                    fade_between(c, a, p[2]),
                ]
            },
        ),
    ];
    for (label, expected, tournament) in tournaments {
        let listed = |order: [usize; 3], progress: [f32; 3]| {
            let all = tournament(&b, &c, &a, progress);
            order.map(|index| all[index]).to_vec()
        };
        let picture = draw(listed(LIST_ORDERS[0], [0.5; 3]));
        assert_rgba8_near(&picture, &expected, 1);
        for order in LIST_ORDERS {
            assert_eq!(
                draw(listed(order, [0.5; 3])),
                picture,
                "{label}, order {order:?}"
            );
            for moved in 0..3 {
                let mut progress = [0.5; 3];
                progress[moved] = 0.1;
                assert_ne!(
                    draw(listed(order, progress)),
                    picture,
                    "{label}, order {order:?}: transition {moved} is not drawn"
                );
            }
        }
    }
}

#[test]
fn a_cycle_of_transitions_is_refused() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    renderer.register_builtin_transitions().unwrap();
    let textures = [[0, 255, 0, 255], [0, 0, 255, 255], [255, 0, 0, 255]]
        .map(|pixel| rgba_texture(&renderer, pixel));
    let frames = Frames(
        textures
            .iter()
            .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()))
            .collect(),
    );
    let full = || visual(Vec2::new(1.0, 1.0), 1.0);
    let b = clip("b", 0, full(), ClipBlendMode::Normal);
    let c = clip("c", 1, full(), ClipBlendMode::Normal);
    let a = clip("a", 2, full(), ClipBlendMode::Normal);
    let frame = FrameDesc {
        background: Some(alpha2_model::Rgb::BLACK),
        blend_space: alpha2_model::BlendSpace::SrgbEncoded,
        time: MediaTime::ZERO,
        visual: vec![b, c, a],
        audio: vec![],
        transitions: vec![
            fade_between(&a, &b, 0.5),
            fade_between(&b, &c, 0.5),
            fade_between(&c, &a, 0.5),
        ],
        captions: Vec::new(),
    };
    assert!(matches!(
        renderer.render_frame(RenderSize::new(4, 4).unwrap(), &frame, &frames),
        Err(RenderError::InvalidGraph)
    ));
}

/// A track whose clips start at 0, 1 and 2 seconds and overlap until 10 seconds, with a fade
/// between each two, listed in `order`; transition `eased` has a late easing.
fn three_clip_project(
    pairs: [(&str, &str, i64); 3],
    order: [usize; 3],
    eased: Option<usize>,
) -> crate::EngineProject {
    let fill = |id: &str, red: u8, green: u8, blue: u8| {
        format!(
            r#"{{ "id": "{id}", "kind": "generated", "generator": {{ "type": "fill", "paint": {{
                "type": "solid", "color": {{ "red": {red}, "green": {green}, "blue": {blue}, "alpha": 1 }} }} }} }}"#
        )
    };
    let clip = |id: &str, source: &str, start: i64| {
        format!(
            r#"{{ "id": "{id}", "source_id": "{source}", "timeline_range": {{ "start": {start}, "end": 240000 }},
                 "source_range": {{ "start": 0, "end": {} }} }}"#,
            240_000 - start
        )
    };
    let linear = r#"{ "type": "linear" }"#;
    let late = r#"{ "type": "cubic_bezier", "x1": 0.9, "y1": 0.0, "x2": 1.0, "y2": 0.1 }"#;
    let transitions = order
        .iter()
        .map(|&index| {
            let (from, to, start) = pairs[index];
            let easing = if eased == Some(index) { late } else { linear };
            format!(
                r#"{{ "from_clip": "{from}", "to_clip": "{to}", "timeline_range": {{ "start": {start}, "end": 240000 }},
                     "easing": {easing}, "effect": {{ "id": "alpha2.transition.fade", "version": 1 }} }}"#
            )
        })
        .collect::<Vec<_>>();
    let document = format!(
        r#"{{ "schema_version": 1, "id": "p", "name": "P",
             "settings": {{ "width": 16, "height": 16, "frame_rate": {{ "num": 30, "den": 1 }},
                            "sample_rate": 48000, "blend_space": "srgb_encoded" }},
             "sources": [ {}, {}, {} ],
             "timeline": {{ "tracks": [ {{ "id": "v", "name": "V", "kind": "video",
               "clips": [ {}, {}, {} ],
               "transitions": [ {} ] }} ] }} }}"#,
        fill("green", 0, 1, 0),
        fill("blue", 0, 0, 1),
        fill("red", 1, 0, 0),
        clip("b", "green", 0),
        clip("c", "blue", 24_000),
        clip("a", "red", 48_000),
        transitions.join(", ")
    );
    let project = alpha2_model::Project::from_json(&document).expect("valid project");
    crate::EngineProject::new(project).expect("indexed")
}

#[test]
fn three_overlapping_clips_draw_one_picture_in_any_document_order() {
    let renderer = alpha2_render::CpuRenderer::new(1);
    let size = RenderSize::new(16, 16).unwrap();
    let draw = |project: &crate::EngineProject| {
        let frame = project.evaluate(MediaTime(144_000)).expect("frame");
        renderer
            .render_frame(size, &frame, &MissingFrames)
            .expect("frame renders")
            .texture
            .expect("complete")
            .read_rgba8()[..4]
            .to_vec()
    };
    for pairs in [
        [("b", "c", 24_000), ("a", "b", 48_000), ("a", "c", 48_000)],
        [("b", "c", 24_000), ("b", "a", 48_000), ("c", "a", 48_000)],
    ] {
        let picture = draw(&three_clip_project(pairs, LIST_ORDERS[0], None));
        for order in LIST_ORDERS {
            assert_eq!(
                draw(&three_clip_project(pairs, order, None)),
                picture,
                "{pairs:?}, document order {order:?}"
            );
            for eased in 0..3 {
                assert_ne!(
                    draw(&three_clip_project(pairs, order, Some(eased))),
                    picture,
                    "{pairs:?}, document order {order:?}: transition {eased} is not drawn"
                );
            }
        }
    }
}
