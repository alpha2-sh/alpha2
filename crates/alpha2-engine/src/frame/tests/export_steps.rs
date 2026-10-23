//! Export steps that wait for a frame, and an unregistered effect at every entry point.

use super::*;

#[test]
fn a_waiting_export_step_keeps_its_frame_and_its_retry_renders_it() {
    // Waiting must not advance the cursor, which the browser's retry relies on, and every step
    // must refuse a snapshot other than the one its exporter was made for.
    use crate::{ExportStep, ExportTextureStep, Exporter, QualityProfile};
    use alpha2_media::VideoCodec;
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let snapshot = one_clip_snapshot();
    let size = RenderSize::new(16, 16).expect("size");
    let texture = rgba_image_texture(&renderer, size, &[200; 16 * 16 * 4]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let provider = ArrivingFrame {
        source: SourceId::new("video").expect("valid ID"),
        texture: &view,
        arrived: std::cell::Cell::new(false),
    };
    let range = alpha2_time::TimeRange::new(MediaTime::ZERO, MediaTime::from_ticks(12_000))
        .expect("three frames");
    let exporter = || {
        Exporter::new(
            &snapshot,
            range,
            alpha2_time::FrameRate::new(30, 1).expect("rate"),
            QualityProfile::export(),
            VideoCodec::H264,
        )
        .expect("exporter")
    };

    let mut readback = exporter();
    provider.arrived.set(false);
    let before = (readback.pending_time(), readback.progress());
    let step = readback
        .render_next(&snapshot, &renderer, &provider)
        .expect("a step");
    assert!(
        matches!(step, ExportStep::Waiting { time, .. } if Some(time) == before.0),
        "{step:?}"
    );
    assert_eq!((readback.pending_time(), readback.progress()), before);
    provider.arrived.set(true);
    let step = readback
        .render_next(&snapshot, &renderer, &provider)
        .expect("a step");
    assert!(
        matches!(step, ExportStep::Rendered { index: 0, time, .. } if Some(time) == before.0),
        "{step:?}"
    );

    let mut textures = exporter();
    provider.arrived.set(false);
    let before = (textures.pending_time(), textures.progress());
    let step = textures
        .render_next_texture(&snapshot, &renderer, &provider)
        .expect("a step");
    assert!(matches!(step, ExportTextureStep::Waiting { .. }));
    assert_eq!((textures.pending_time(), textures.progress()), before);
    provider.arrived.set(true);
    let step = textures
        .render_next_texture(&snapshot, &renderer, &provider)
        .expect("a step");
    assert!(
        matches!(step, ExportTextureStep::Rendered { index: 0, time, .. } if Some(time) == before.0)
    );

    // Another snapshot, even of the same project, is refused at every step; a copy of the
    // export's own is not.
    let other = one_clip_snapshot();
    let mut checked = exporter();
    assert_eq!(
        checked.prefetch_needs(&other, 2).err(),
        Some(crate::EngineError::ExportProjectChanged)
    );
    assert_eq!(
        checked.render_next(&other, &renderer, &provider).err(),
        Some(crate::EngineError::ExportProjectChanged)
    );
    assert_eq!(
        checked
            .render_next_texture(&other, &renderer, &provider)
            .err(),
        Some(crate::EngineError::ExportProjectChanged)
    );
    assert_eq!(checked.progress(), exporter().progress());
    let copy = snapshot.clone();
    assert!(checked.prefetch_needs(&copy, 2).is_ok());
    assert!(matches!(
        checked.render_next(&copy, &renderer, &provider),
        Ok(ExportStep::Rendered { index: 0, .. })
    ));
}

#[test]
fn an_unregistered_effect_is_named_by_every_entry_point() {
    // Every entry point names the effect, since a browser host has no check of its own to say
    // which effect is not registered.
    use crate::{EngineError, Exporter, Player, QualityProfile};
    use alpha2_media::VideoCodec;
    let _lock = gpu_test_lock();
    let renderer = Renderer::new(gpu());
    let mut project = one_clip_snapshot().project().clone();
    project.compositions = vec![alpha2_model::Composition {
        id: alpha2_model::CompositionId::new("look").expect("valid ID"),
        nodes: vec![
            alpha2_model::CompositionNode::new(
                alpha2_model::NodeId::new("clip").expect("valid ID"),
                alpha2_model::NodeOp::Clip {},
            ),
            alpha2_model::CompositionNode::new(
                alpha2_model::NodeId::new("glow").expect("valid ID"),
                alpha2_model::NodeOp::Effect {
                    input: Some(alpha2_model::NodeId::new("clip").expect("valid ID")),
                    inputs: BTreeMap::new(),
                    effect: ClipEffect {
                        id: "vendor.glow".to_owned(),
                        version: 2,
                        params: BTreeMap::new(),
                        resources: BTreeMap::new(),
                    },
                },
            ),
        ],
        output: Some(alpha2_model::NodeId::new("glow").expect("valid ID")),
        inputs: Vec::new(),
        controls: Vec::new(),
    }];
    project.timeline.tracks[0].clips[0].composition = Some(alpha2_model::ClipComposition::new(
        alpha2_model::CompositionId::new("look").expect("valid ID"),
    ));
    let snapshot = crate::EngineProject::new(project).expect("valid project");
    let expected = EngineError::UnregisteredEffect {
        effect: "vendor.glow".to_owned(),
        version: 2,
    };
    assert_eq!(snapshot.effects(), [("vendor.glow", 2)]);
    assert_eq!(snapshot.check_effects(&renderer), Err(expected.clone()));
    assert_eq!(
        expected.to_string(),
        "the project draws effect vendor.glow@2, which is not registered"
    );
    assert_eq!(expected.code(), crate::ErrorCode::EffectError);
    let mut player = Player::default();
    assert_eq!(
        player
            .render_at(&snapshot, &renderer, &MissingFrames, MediaTime::ZERO)
            .err(),
        Some(expected.clone())
    );
    let mut exporter = Exporter::for_project(&snapshot, QualityProfile::export(), VideoCodec::H264)
        .expect("exporter");
    assert_eq!(
        exporter
            .render_next(&snapshot, &renderer, &MissingFrames)
            .err(),
        Some(expected.clone())
    );
    assert_eq!(
        exporter
            .render_next_texture(&snapshot, &renderer, &MissingFrames)
            .err(),
        Some(expected)
    );
}
