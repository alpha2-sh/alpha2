//! Source fingerprints and provider sources.

use super::*;

#[test]
fn fingerprints_round_trip_and_are_checked() {
    let mut project = project();
    let hash = "0123456789abcdef".repeat(4);
    let mut fingerprint = crate::MediaFingerprint {
        content_sha256: Some(hash),
        byte_length: Some(1_024),
        modified_ms: Some(1_790_000_000_000),
        streams: Some(crate::StreamTopology {
            video: Some(crate::VideoStreamShape::new(
                1_920,
                1_080,
                Some(alpha2_time::FrameRate::new(30_000, 1_001).expect("rate")),
            )),
            audio: Some(crate::AudioStreamShape::new(2, 48_000)),
        }),
        duration_ticks: Some(120_000),
        color: Some(alpha2_media::ColorMetadata::default()),
    };
    project.sources[0].fingerprint = Some(fingerprint.clone());
    project.validate().expect("valid");
    let json = serde_json::to_string(&project).expect("serialize");
    assert_eq!(Project::from_json(&json).expect("load"), project);
    // An absent fingerprint is not written.
    project.sources[0].fingerprint = None;
    assert!(
        !serde_json::to_string(&project)
            .expect("serialize")
            .contains("fingerprint")
    );

    for broken in [
        crate::MediaFingerprint {
            content_sha256: Some("0123456789ABCDEF".repeat(4)),
            ..fingerprint.clone()
        },
        crate::MediaFingerprint {
            content_sha256: Some("abc".into()),
            ..fingerprint.clone()
        },
        crate::MediaFingerprint {
            duration_ticks: Some(-1),
            ..fingerprint.clone()
        },
        crate::MediaFingerprint {
            streams: Some(crate::StreamTopology {
                video: Some(crate::VideoStreamShape::new(0, 1_080, None)),
                audio: None,
            }),
            ..fingerprint.clone()
        },
        crate::MediaFingerprint {
            streams: Some(crate::StreamTopology {
                video: None,
                audio: Some(crate::AudioStreamShape::new(2, 0)),
            }),
            ..fingerprint.clone()
        },
    ] {
        project.sources[0].fingerprint = Some(broken.clone());
        assert!(
            matches!(
                project.validate(),
                Err(crate::ModelError::InvalidFingerprint { .. })
            ),
            "{broken:?}"
        );
    }
    // An empty fingerprint records nothing and is valid.
    fingerprint = crate::MediaFingerprint::default();
    project.sources[0].fingerprint = Some(fingerprint.clone());
    project.validate().expect("empty evidence");
    // The engine draws a generated source: there is no media to fingerprint.
    let mut generated = crate::MediaSource::generated(
        id(SourceId::new, "wash"),
        crate::Generator::Fill {
            paint: crate::Paint::Solid {
                color: crate::Rgba::new(1.0, 0.0, 0.0, 1.0),
            },
        },
    );
    generated.fingerprint = Some(fingerprint);
    project.sources.push(generated);
    assert!(matches!(
        project.validate(),
        Err(crate::ModelError::InvalidFingerprint { .. })
    ));
}

#[test]
fn provider_sources_fit_their_kind_and_settings() {
    use crate::{MissingFrames, ProviderSource};
    let mut project = project();
    let rate = alpha2_time::FrameRate::new(24, 1).expect("rate");
    let sequence = |frames: Vec<Option<&str>>, missing| ProviderSource::ImageSequence {
        frames: frames.into_iter().map(|f| f.map(str::to_owned)).collect(),
        frame_rate: rate,
        start_number: 1,
        missing,
        primaries: None,
    };
    // The first source is a video: animated images and sequences fit it.
    for provider in [
        ProviderSource::AnimatedImage,
        sequence(vec![Some("f1"), Some("f2")], MissingFrames::Reject),
        sequence(vec![Some("f1"), None, Some("f3")], MissingFrames::Hold),
        sequence(vec![Some("f1"), None], MissingFrames::Blank),
        ProviderSource::ImageSequence {
            frames: vec![Some("f1".to_owned())],
            frame_rate: rate,
            start_number: 1,
            missing: MissingFrames::Reject,
            primaries: Some(crate::LinearPrimaries::AcesAp1),
        },
    ] {
        project.sources[0].provider = Some(provider.clone());
        project
            .validate()
            .unwrap_or_else(|error| panic!("{provider:?}: {error}"));
        let json = serde_json::to_string(&project).expect("serialize");
        assert_eq!(Project::from_json(&json).expect("load"), project);
    }
    for provider in [
        // An SVG and an OpenEXR image are stills: not videos.
        ProviderSource::Svg,
        ProviderSource::Exr { primaries: None },
        sequence(Vec::new(), MissingFrames::Reject),
        sequence(vec![None, Some("f2")], MissingFrames::Hold),
        sequence(vec![Some("f1"), None], MissingFrames::Reject),
        sequence(vec![Some("f1"), Some("f1")], MissingFrames::Reject),
        sequence(vec![Some("")], MissingFrames::Reject),
    ] {
        project.sources[0].provider = Some(provider.clone());
        assert!(
            matches!(
                project.validate(),
                Err(crate::ModelError::InvalidProvider { .. })
            ),
            "{provider:?}"
        );
    }
    // Provider media has no audio of its own.
    project.sources[0].provider = Some(ProviderSource::AnimatedImage);
    project.sources[0].has_audio = true;
    assert!(matches!(
        project.validate(),
        Err(crate::ModelError::InvalidProvider { .. })
    ));
}
