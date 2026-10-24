//! Timeline frames and the result cache: a clip's finished picture and the
//! frame are kept only when the host keys their sources, and a clip's cache
//! intent is honoured.

use std::collections::BTreeMap;

use alpha2_eval::{ActiveClip, FrameDesc};
use alpha2_media::SourceId;
use alpha2_model::{CacheIntent, ClipBlendMode, ClipEffect, ClipId, EffectValue, TrackId, Vec2};
use alpha2_render::{RenderCacheKey, RenderSize, Renderer, SourceView};
use alpha2_time::MediaTime;

use super::super::{FrameProvider, FrameRenderer};
use super::{
    chain, constant_params, gpu, gpu_test_lock, leak, read_rgba8, rgba_texture,
    runtime_tint_effect, visual,
};

/// One source, keyed by its id and time when `keyed`.
struct Source<'a> {
    id: SourceId,
    view: &'a wgpu::TextureView,
    keyed: bool,
}

impl FrameProvider for Source<'_> {
    fn texture_for(&self, source: &SourceId, _: MediaTime) -> Option<SourceView<'_>> {
        (source == &self.id).then_some(SourceView::from_wgpu(self.view))
    }

    fn source_key(&self, source: &SourceId, time: MediaTime) -> Option<RenderCacheKey> {
        (self.keyed && source == &self.id)
            .then(|| RenderCacheKey::source(u64::try_from(time.ticks()).expect("non-negative") + 1))
    }
}

fn frame(source: &SourceId, cache: CacheIntent, opacity: f32) -> FrameDesc<'static> {
    FrameDesc {
        background: alpha2_model::Rgb::BLACK,
        blend_space: alpha2_model::BlendSpace::LinearLight,
        time: MediaTime::ZERO,
        visual: vec![ActiveClip {
            clip_id: leak(ClipId::new("clip").expect("valid ID")),
            source_id: leak(source.clone()),
            track_id: leak(TrackId::new("track").expect("valid ID")),
            source_time: MediaTime::ZERO,
            visual: Some(visual(Vec2::new(1.0, 1.0), opacity)),
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
            cache,
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
    }
}

#[test]
fn frames_are_kept_only_from_keyed_sources_and_as_clips_ask() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("effect compiles");
    renderer.set_result_cache_budget(1 << 20);
    let texture = rgba_texture(&renderer, [200, 100, 50, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let id = SourceId::new("source").expect("valid ID");
    let size = RenderSize::new(4, 4).expect("size");
    let render = |provider: &Source<'_>, cache, opacity| {
        let nodes = renderer.diagnostics().graph_nodes;
        let output = renderer
            .render_frame(size, &frame(&id, cache, opacity), provider)
            .expect("renders")
            .texture
            .expect("complete");
        (
            read_rgba8(&renderer, &output),
            renderer.diagnostics().graph_nodes - nodes,
        )
    };

    let unkeyed = Source {
        id: id.clone(),
        view: &view,
        keyed: false,
    };
    let (live, _) = render(&unkeyed, CacheIntent::Auto, 1.0);
    let stats = renderer.result_cache_stats();
    assert_eq!(
        (stats.inserts, stats.ineligible),
        (0, 2),
        "clip picture and frame"
    );

    let keyed = Source {
        id: id.clone(),
        view: &view,
        keyed: true,
    };
    let (first, drawn) = render(&keyed, CacheIntent::Auto, 1.0);
    assert_eq!(first, live, "keys change nothing that renders");
    assert_eq!(drawn, 2);
    assert_eq!(renderer.result_cache_stats().inserts, 2);
    let (again, drawn) = render(&keyed, CacheIntent::Auto, 1.0);
    assert_eq!((again, drawn), (live.clone(), 0), "the frame is found");
    // A new opacity changes the frame, not the clip's picture under it.
    let (_, drawn) = render(&keyed, CacheIntent::Auto, 0.5);
    assert_eq!(drawn, 1);

    // A bypassed clip is drawn live, and so is the frame it is in.
    let hits = renderer.result_cache_stats().hits;
    let (bypassed, drawn) = render(&keyed, CacheIntent::Bypass, 1.0);
    assert_eq!((bypassed, drawn), (live, 2));
    assert_eq!(renderer.result_cache_stats().hits, hits);
}

/// One still: the same key at every time.
struct Still<'a> {
    id: SourceId,
    view: &'a wgpu::TextureView,
}

impl FrameProvider for Still<'_> {
    fn texture_for(&self, source: &SourceId, _: MediaTime) -> Option<SourceView<'_>> {
        (source == &self.id).then_some(SourceView::from_wgpu(self.view))
    }

    fn source_key(&self, source: &SourceId, _: MediaTime) -> Option<RenderCacheKey> {
        (source == &self.id).then_some(RenderCacheKey::source(1))
    }
}

/// [`frame`] at `seconds`, its effect sampled there.
fn frame_at(source: &SourceId, seconds: i64) -> FrameDesc<'static> {
    let time = MediaTime::from_ticks(seconds * alpha2_time::TICKS_PER_SECOND);
    let mut frame = frame(source, CacheIntent::Auto, 1.0);
    frame.time = time;
    frame.visual[0].composition = frame.visual[0]
        .composition
        .map(|composition| alpha2_eval::ActiveComposition::new(composition.plan(), time));
    frame
}

#[test]
fn a_still_under_an_effect_that_cannot_read_the_time_is_found_in_later_frames() {
    let _lock = gpu_test_lock();
    let id = SourceId::new("source").expect("valid ID");
    let size = RenderSize::new(4, 4).expect("size");
    // Whether each body's picture is found again a second later: a body that names `time`,
    // `time_ticks` or the uniform they read, even one that reaches it directly, is drawn again in
    // every frame.
    for (body, found) in [
        (
            "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
            true,
        ),
        (
            "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint * min(time(), 1.0); }",
            false,
        ),
        (
            "fn effect(_uv: vec2f, src: vec4f) -> vec4f { \
             return src * params.tint * min(runtime.resolution_time.z, 1.0); }",
            false,
        ),
        (
            "fn effect(_uv: vec2f, src: vec4f) -> vec4f { \
             return src * params.tint * f32(min(time_ticks().x, 1u)); }",
            false,
        ),
    ] {
        let mut renderer = Renderer::new(gpu());
        pollster::block_on(renderer.register_effect(runtime_tint_effect(body)))
            .expect("effect compiles");
        renderer.set_result_cache_budget(1 << 20);
        let texture = rgba_texture(&renderer, [200, 100, 50, 255]);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let still = Still {
            id: id.clone(),
            view: &view,
        };
        let render = |seconds| {
            let nodes = renderer.diagnostics().graph_nodes;
            let output = renderer
                .render_frame(size, &frame_at(&id, seconds), &still)
                .expect("renders")
                .texture
                .expect("complete");
            (
                read_rgba8(&renderer, &output),
                renderer.diagnostics().graph_nodes - nodes,
            )
        };
        let (first, drawn) = render(1);
        assert_eq!(drawn, 2, "{body}");
        let (later, drawn) = render(2);
        if found {
            assert_eq!((later, drawn), (first, 0), "{body}");
            assert_eq!(renderer.result_cache_stats().hits, 1, "{body}");
        } else {
            assert_eq!(drawn, 2, "{body}");
            assert_eq!(renderer.result_cache_stats().hits, 0, "{body}");
        }
    }
}

#[test]
fn film_grain_is_new_in_every_frame_of_a_long_timeline() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_builtins()).expect("built-ins compile");
    renderer.set_result_cache_budget(1 << 20);
    let texture = rgba_texture(&renderer, [128, 128, 128, 255]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let id = SourceId::new("source").expect("valid ID");
    let still = Still {
        id: id.clone(),
        view: &view,
    };
    let size = RenderSize::new(4, 4).expect("size");
    let effects: &'static [ClipEffect] = Vec::leak(vec![ClipEffect {
        id: "alpha2.effect.film-grain".into(),
        version: 1,
        resources: BTreeMap::new(),
        params: constant_params([
            ("amount", EffectValue::Float(0.5)),
            ("size", EffectValue::Float(0.5)),
        ]),
    }]);
    // 2^18 s (about 73 hours) on: f32 seconds there are 1/32 s apart, so 60 fps frames
    // would share a time two by two.
    let start = (1_i64 << 18) * alpha2_time::TICKS_PER_SECOND;
    let frames = (0..5)
        .map(|index| {
            let time = MediaTime::from_ticks(start + index * alpha2_time::TICKS_PER_SECOND / 60);
            let mut frame = frame(&id, CacheIntent::Auto, 1.0);
            frame.time = time;
            frame.visual[0].composition = chain(effects, time);
            let output = renderer
                .render_frame(size, &frame, &still)
                .expect("renders")
                .texture
                .expect("complete");
            read_rgba8(&renderer, &output)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        renderer.result_cache_stats().hits,
        0,
        "no frame is another's"
    );
    for (index, pair) in frames.windows(2).enumerate() {
        assert_ne!(
            pair[0],
            pair[1],
            "frames {index} and {} draw new grain",
            index + 1
        );
    }
}

#[test]
fn a_held_caption_is_found_in_later_frames_until_its_text_or_fonts_change() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(alpha2_render::generator_definition()))
        .expect("the generator compiles");
    renderer.set_result_cache_budget(1 << 24);
    let project = super::caption_project();
    let font = std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture font");
    let mut captions = crate::TextTextures::new();
    captions
        .add_font("Alpha2 Fixture", font.clone(), 0)
        .expect("font");
    let size = RenderSize::new(160, 90).expect("size");
    let render = |captions: &mut crate::TextTextures, ticks: i64| {
        let frame = project
            .evaluate(MediaTime::from_ticks(ticks))
            .expect("frame");
        captions.prepare(&renderer, &frame, size).expect("captions");
        let nodes = renderer.diagnostics().graph_nodes;
        let output = renderer
            .render_frame(size, &frame, &super::CaptionFrames(captions))
            .expect("frame renders");
        (
            read_rgba8(&renderer, &output.texture.expect("complete")),
            renderer.diagnostics().graph_nodes - nodes,
        )
    };
    let (first, drawn) = render(&mut captions, 0);
    assert!(drawn > 0);
    assert_eq!(renderer.result_cache_stats().ineligible, 0);
    // The first cue, a frame and most of a second later.
    for ticks in [4_000, 116_000] {
        let (held, drawn) = render(&mut captions, ticks);
        assert_eq!((held == first, drawn), (true, 0), "at {ticks}");
    }
    let (second, drawn) = render(&mut captions, 120_000);
    assert!(drawn > 0);
    assert_ne!(second, first);
    let (_, again) = render(&mut captions, 124_000);
    assert_eq!(again, 0);
    // Another font set may draw the same caption with other glyphs: the frame over it is drawn
    // again, as many passes as the first time.
    captions
        .add_font("Alpha2 Fixture Again", font, 0)
        .expect("font");
    let (_, redrawn) = render(&mut captions, 124_000);
    assert_eq!(redrawn, drawn, "a new font set is drawn again");
}

/// A generated picture is timeless: its node keeps one cache key, so a later
/// frame finds it in the result cache. Over a fade the frame is drawn again
/// and the picture under it is not; where nothing moves, the whole frame is
/// found.
#[test]
fn a_generated_picture_is_found_in_later_frames() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(alpha2_render::generator_definition()))
        .expect("the generator compiles");
    renderer.set_result_cache_budget(1 << 20);
    let project = alpha2_model::Project::from_json(
        r#"{
          "schema_version": 1, "id": "p", "name": "P",
          "settings": { "width": 16, "height": 8, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48000 },
          "sources": [
            { "id": "backdrop", "kind": "generated", "generator": { "type": "fill", "paint": {
                "type": "linear_gradient", "start": { "x": 0, "y": 0.5 }, "end": { "x": 1, "y": 0.5 },
                "stops": [
                  { "offset": 0, "color": { "red": 0, "green": 0, "blue": 1, "alpha": 1 } },
                  { "offset": 1, "color": { "red": 1, "green": 1, "blue": 0, "alpha": 1 } }
                ] } } }
          ],
          "timeline": { "tracks": [
            { "id": "t", "name": "T", "kind": "video", "clips": [
              { "id": "c", "source_id": "backdrop", "timeline_range": { "start": 0, "end": 240000 },
                "source_range": { "start": 0, "end": 240000 },
                "visual": {
                  "position": { "mode": "constant", "value": { "x": 0.5, "y": 0.5 } },
                  "anchor": { "x": 0.5, "y": 0.5 },
                  "scale": { "mode": "constant", "value": { "x": 1, "y": 1 } },
                  "rotation_degrees": { "mode": "constant", "value": 0 },
                  "opacity": { "mode": "keyframes", "keyframes": [
                    { "time": 0, "value": 0, "interpolation": { "type": "linear" } },
                    { "time": 120000, "value": 1, "interpolation": { "type": "hold" } } ] } } } ],
              "transitions": [] }
          ] }
        }"#,
    )
    .expect("a valid project");
    let project = crate::EngineProject::new(project).expect("indexed");
    let size = RenderSize::new(16, 8).expect("size");
    let render = |ticks: i64| {
        let frame = project
            .evaluate(MediaTime::from_ticks(ticks))
            .expect("evaluated");
        let nodes = renderer.diagnostics().graph_nodes;
        let output = renderer
            .render_frame(size, &frame, &super::MissingFrames)
            .expect("renders");
        assert!(output.report.is_complete(), "at {ticks}");
        (
            read_rgba8(&renderer, &output.texture.expect("complete")),
            renderer.diagnostics().graph_nodes - nodes,
        )
    };
    let (_, drawn) = render(4_000);
    assert_eq!(drawn, 2);
    assert_eq!(renderer.result_cache_stats().ineligible, 0);
    // A frame later the fade has moved: only the frame is drawn again.
    let (_, drawn) = render(8_000);
    assert_eq!(drawn, 1, "the generated picture is found a frame later");
    assert_eq!(renderer.result_cache_stats().hits, 1);
    // Past the fade, nothing moves: a frame and a second later the whole frame is found.
    let (opaque, drawn) = render(124_000);
    assert_eq!(drawn, 1);
    let (later, drawn) = render(236_000);
    assert_eq!((later, drawn), (opaque, 0), "the whole frame is found");
}
