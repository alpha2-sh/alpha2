//! Frames whose sources are missing: availability, atomic renders, and a player that holds or waits.

use super::*;

#[test]
fn availability_pass_deduplicates_missing_sources() {
    let _lock = gpu_test_lock();
    let gpu = gpu();
    let source = SourceId::new("source").expect("valid ID");
    let item = ActiveClip {
        clip_id: leak(ClipId::new("one").expect("valid ID")),
        source_id: leak(source.clone()),
        track_id: leak(TrackId::new("track").expect("valid ID")),
        source_time: MediaTime(0),
        visual: None,
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
        time: MediaTime(0),
        visual: vec![item, item],
        audio: Vec::new(),
        transitions: Vec::new(),
        captions: Vec::new(),
    };
    let report = Renderer::new(gpu).inspect_frame(&frame, &MissingFrames);
    assert_eq!(report.missing_sources, vec![source]);
}

#[test]
fn evaluated_frame_render_is_atomic_on_missing_inputs() {
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
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
            composition: None,
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
    let size = RenderSize::new(1, 1).expect("non-zero size");
    let missing = renderer
        .render_frame(size, &frame, &MissingFrames)
        .expect("availability is not a render error");
    assert!(!missing.is_complete());
    assert_eq!(missing.report.missing_sources, vec![source.clone()]);
    assert_eq!(renderer.texture_pool_stats().created, 0);

    let input = rgba_texture(&renderer, [255, 0, 0, 255]);
    let view = input.create_view(&wgpu::TextureViewDescriptor::default());
    let available = SingleFrame {
        source,
        texture: &view,
    };
    let frame_output = renderer
        .render_frame(size, &frame, &available)
        .expect("valid frame");
    assert!(frame_output.report.is_complete());
    let texture = frame_output.texture.expect("complete frame has output");
    assert_eq!(read_rgba8(&renderer, &texture), [255, 0, 0, 255]);
}

#[test]
fn a_player_holds_its_last_frame_or_waits_when_a_source_is_missing() {
    use crate::{MissingFramePolicy, Player};
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let snapshot = one_clip_snapshot();
    let size = RenderSize::new(16, 16).expect("size");
    let texture = rgba_image_texture(&renderer, size, &[200; 16 * 16 * 4]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let provider = ArrivingFrame {
        source: SourceId::new("video").expect("valid ID"),
        texture: &view,
        arrived: std::cell::Cell::new(true),
    };
    let (shown, later) = (MediaTime::ZERO, MediaTime::from_ticks(4_000));
    let counts = || {
        let diagnostics = renderer.diagnostics();
        (
            diagnostics.presented_frames,
            diagnostics.repeated_frames,
            diagnostics.dropped_frames,
        )
    };
    // What a render answered: the time shown, its pixels, whether repeated, and what was
    // missing.
    let step = |player: &mut Player, time| {
        let frame = player
            .render_at(&snapshot, &renderer, &provider, time)
            .expect("a frame or a hold");
        (
            frame.presented_time,
            frame.texture.map(|texture| read_rgba8(&renderer, texture)),
            frame.repeated,
            frame.missing_sources.len(),
        )
    };

    let mut hold = Player::default();
    assert_eq!(
        hold.missing_frame_policy(),
        MissingFramePolicy::HoldPrevious
    );
    let before = counts();
    let (time, first, repeated, missing) = step(&mut hold, shown);
    assert_eq!((time, repeated, missing), (Some(shown), false, 0));
    let first = first.expect("a picture");
    provider.arrived.set(false);
    let repeated_frame = step(&mut hold, later);
    assert_eq!(
        repeated_frame,
        (Some(shown), Some(first.clone()), true, 1),
        "the last frame, as repeated"
    );
    hold.reset();
    let after_reset = step(&mut hold, later);
    assert_eq!(
        after_reset,
        (None, None, false, 1),
        "nothing is held after a reset"
    );
    let after = counts();
    assert_eq!(
        (after.0 - before.0, after.1 - before.1, after.2 - before.2),
        (2, 1, 1),
        "presented, repeated, dropped"
    );

    let mut wait = Player::default().with_missing_frame_policy(MissingFramePolicy::Wait);
    provider.arrived.set(true);
    assert_eq!(step(&mut wait, shown).0, Some(shown));
    provider.arrived.set(false);
    assert_eq!(
        step(&mut wait, later),
        (None, None, false, 1),
        "waiting shows nothing"
    );
    provider.arrived.set(true);
    let (time, picture, repeated, _) = step(&mut wait, later);
    assert_eq!((time, picture, repeated), (Some(later), Some(first), false));
}
