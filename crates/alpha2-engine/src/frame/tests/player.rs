//! A player that neither renders nor presents a frame equal to the one it shows.

use std::cell::Cell;

use alpha2_render::RenderCacheKey;

use super::*;
use crate::{Player, QualityProfile};

/// A 16x8 gradient fading in over the first second and held after it: a generated picture, so
/// every frame's key is complete.
fn fade_project(width: u32) -> crate::EngineProject {
    let project = alpha2_model::Project::from_json(
        &r#"{
          "schema_version": 1, "id": "p", "name": "P",
          "settings": { "width": WIDTH, "height": 8, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48000 },
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
        }"#
        .replace("WIDTH", &width.to_string()),
    )
    .expect("a valid project");
    crate::EngineProject::new(project).expect("indexed")
}

/// One video source whose upload generation is `generation` while it has arrived; keyed by it
/// unless `keyed` is false.
struct Uploaded<'a> {
    source: SourceId,
    texture: &'a wgpu::TextureView,
    arrived: Cell<bool>,
    generation: Cell<u64>,
    keyed: bool,
}

impl FrameProvider for Uploaded<'_> {
    fn texture_for(&self, source: &SourceId, _: MediaTime) -> Option<SourceView<'_>> {
        (self.arrived.get() && source == &self.source)
            .then_some(SourceView::from_wgpu(self.texture))
    }

    fn source_key(&self, source: &SourceId, _: MediaTime) -> Option<RenderCacheKey> {
        (self.keyed && source == &self.source)
            .then(|| RenderCacheKey::source(self.generation.get()))
    }
}

/// Whether the player's frame at `ticks` was unchanged, and the graph submissions it made.
fn step(
    player: &mut Player,
    project: &crate::EngineProject,
    renderer: &Renderer,
    frames: &dyn FrameProvider,
    ticks: i64,
) -> (bool, u64) {
    let before = renderer.diagnostics().graph_submissions;
    let frame = player
        .render_at(project, renderer, frames, MediaTime::from_ticks(ticks))
        .expect("a frame");
    assert!(frame.texture.is_some(), "at {ticks}");
    assert_eq!(frame.presented_time, Some(MediaTime::from_ticks(ticks)));
    let unchanged = frame.unchanged;
    (unchanged, renderer.diagnostics().graph_submissions - before)
}

#[test]
fn a_frame_equal_to_the_one_shown_is_neither_rendered_nor_submitted() {
    let _lock = gpu_test_lock();
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_effect(alpha2_render::generator_definition()))
        .expect("the generator compiles");
    let tint = pollster::block_on(renderer.register_effect(runtime_tint_effect(
        "fn effect(_uv: vec2f, src: vec4f) -> vec4f { return src * params.tint; }",
    )))
    .expect("effect compiles");
    let (project, wide) = (fade_project(16), fade_project(32));
    let mut player = Player::new(QualityProfile::export()).expect("quality");
    let at = |player: &mut Player, renderer: &Renderer, project, ticks| {
        step(player, project, renderer, &MissingFrames, ticks)
    };
    // Past the fade nothing moves: a second later the frame is the one shown.
    assert_eq!(at(&mut player, &renderer, &project, 124_000), (false, 1));
    let shown = {
        let frame = player
            .render_at(
                &project,
                &renderer,
                &MissingFrames,
                MediaTime::from_ticks(236_000),
            )
            .expect("a frame");
        assert!(frame.unchanged);
        read_rgba8(&renderer, frame.texture.expect("held"))
    };
    let live = renderer
        .render_frame(
            RenderSize::new(16, 8).expect("size"),
            &project
                .evaluate(MediaTime::from_ticks(236_000))
                .expect("frame"),
            &MissingFrames,
        )
        .expect("renders")
        .texture
        .expect("complete");
    assert_eq!(shown, read_rgba8(&renderer, &live), "it is the live render");
    // Over the fade every frame is new; the same frame again is not.
    assert_eq!(at(&mut player, &renderer, &project, 4_000), (false, 1));
    assert_eq!(at(&mut player, &renderer, &project, 8_000), (false, 1));
    assert_eq!(at(&mut player, &renderer, &project, 8_000), (true, 0));
    // Removing an effect may change how a kept key draws: the frame is drawn again.
    assert!(renderer.unregister_effect(&tint));
    assert_eq!(at(&mut player, &renderer, &project, 8_000), (false, 1));
    assert_eq!(at(&mut player, &renderer, &project, 8_000), (true, 0));
    // So is an explicit clear of kept results, even of an empty cache.
    renderer.clear_result_cache();
    assert_eq!(at(&mut player, &renderer, &project, 8_000), (false, 1));
    // Another render size is another picture.
    assert_eq!(at(&mut player, &renderer, &wide, 8_000), (false, 1));
    assert_eq!(at(&mut player, &renderer, &wide, 8_000), (true, 0));
    // A reset forgets what is shown.
    player.reset();
    assert_eq!(at(&mut player, &renderer, &wide, 8_000), (false, 1));
}

#[test]
fn an_upload_a_transient_source_or_a_hold_renders_the_frame_again() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let project = one_clip_snapshot();
    let size = RenderSize::new(16, 16).expect("size");
    let texture = rgba_image_texture(&renderer, size, &[200; 16 * 16 * 4]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let provider = Uploaded {
        source: SourceId::new("video").expect("valid ID"),
        texture: &view,
        arrived: Cell::new(true),
        generation: Cell::new(1),
        keyed: true,
    };
    let mut player = Player::new(QualityProfile::export()).expect("quality");
    let at = |player: &mut Player, provider: &Uploaded, ticks| {
        step(player, &project, &renderer, provider, ticks)
    };
    assert_eq!(at(&mut player, &provider, 0), (false, 1));
    // The next frame reads the same upload of a still: its picture is the one shown.
    assert_eq!(at(&mut player, &provider, 4_000), (true, 0));
    // A new decode is a new upload generation, so a new key.
    provider.generation.set(2);
    assert_eq!(at(&mut player, &provider, 8_000), (false, 1));
    assert_eq!(at(&mut player, &provider, 8_000), (true, 0));
    // A hold is not that frame: once the source arrives again it is presented, though its key is
    // the one shown before the hold.
    provider.arrived.set(false);
    let held = player
        .render_at(
            &project,
            &renderer,
            &provider,
            MediaTime::from_ticks(12_000),
        )
        .expect("a hold");
    assert!(held.repeated && !held.unchanged);
    provider.arrived.set(true);
    assert_eq!(at(&mut player, &provider, 12_000), (false, 1));
    // A source without a key is never the one shown.
    let transient = Uploaded {
        keyed: false,
        ..provider
    };
    assert_eq!(at(&mut player, &transient, 12_000), (false, 1));
    assert_eq!(at(&mut player, &transient, 12_000), (false, 1));
}
