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
    fingerprint = crate::MediaFingerprint::default();
    project.sources[0].fingerprint = Some(fingerprint.clone());
    project.validate().expect("empty evidence");
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
    project.sources[0].provider = Some(ProviderSource::AnimatedImage);
    project.sources[0].has_audio = true;
    assert!(matches!(
        project.validate(),
        Err(crate::ModelError::InvalidProvider { .. })
    ));
}

#[test]
fn latex_and_pdf_sources_read_with_their_defaults() {
    use crate::{ProviderSource, Rgba};
    let white = Rgba::new(1.0, 1.0, 1.0, 1.0);
    let parsed = |json: &str| serde_json::from_str::<ProviderSource>(json).expect("provider");
    assert_eq!(
        parsed(r#"{"format":"latex","latex":"x^2","font_size":64}"#),
        ProviderSource::Latex {
            latex: "x^2".into(),
            font_size: 64.0,
            color: white,
            display: true,
        }
    );
    assert_eq!(
        parsed(
            r##"{"format":"latex","latex":"x","font_size":8,"color":"#ff000080","display":false}"##
        ),
        ProviderSource::Latex {
            latex: "x".into(),
            font_size: 8.0,
            color: Rgba::from_hex("#ff000080").expect("hex"),
            display: false,
        }
    );
    assert!(serde_json::from_str::<ProviderSource>(r#"{"format":"latex","latex":"x"}"#).is_err());
    assert_eq!(
        parsed(r#"{"format":"pdf"}"#),
        ProviderSource::Pdf {
            page: 1,
            background: white,
        }
    );
    assert!(serde_json::from_str::<ProviderSource>(r#"{"format":"pdf","pages":2}"#).is_err());
}

#[test]
fn latex_and_pdf_sources_are_checked_against_their_limits_and_locators() {
    use crate::{MAX_LATEX_BYTES, MAX_LATEX_FONT_SIZE, MAX_PDF_PAGES, ProviderSource, Rgba};
    let white = Rgba::new(1.0, 1.0, 1.0, 1.0);
    let latex = |latex: String, font_size: f32| ProviderSource::Latex {
        latex,
        font_size,
        color: white,
        display: true,
    };
    let pdf = |page: u32| ProviderSource::Pdf {
        page,
        background: Rgba::new(0.0, 0.0, 0.0, 0.0),
    };
    let mut project = project();
    project.sources[0].kind = crate::MediaKind::Image;
    project.sources[0].locator = "asset://document".into();
    for provider in [pdf(1), pdf(MAX_PDF_PAGES)] {
        project.sources[0].provider = Some(provider.clone());
        project
            .validate()
            .unwrap_or_else(|error| panic!("{provider:?}: {error}"));
        let json = serde_json::to_string(&project).expect("serialize");
        assert_eq!(Project::from_json(&json).expect("load"), project);
    }
    for provider in [pdf(0), pdf(MAX_PDF_PAGES + 1)] {
        project.sources[0].provider = Some(provider.clone());
        assert!(
            matches!(
                project.validate(),
                Err(crate::ModelError::InvalidProvider { .. })
            ),
            "{provider:?}"
        );
    }
    project.sources[0].provider = Some(latex("x".into(), 64.0));
    assert!(
        matches!(
            project.validate(),
            Err(crate::ModelError::InvalidProvider { .. })
        ),
        "a LaTeX source has no locator"
    );
    project.sources[0].locator = String::new();
    for provider in [
        latex("x".into(), 64.0),
        latex("x".repeat(MAX_LATEX_BYTES), MAX_LATEX_FONT_SIZE),
    ] {
        project.sources[0].provider = Some(provider.clone());
        project
            .validate()
            .unwrap_or_else(|error| panic!("{provider:?}: {error}"));
        let json = serde_json::to_string(&project).expect("serialize");
        assert_eq!(Project::from_json(&json).expect("load"), project);
    }
    for provider in [
        latex(String::new(), 64.0),
        latex("x".repeat(MAX_LATEX_BYTES + 1), 64.0),
        latex("x".into(), 0.0),
        latex("x".into(), MAX_LATEX_FONT_SIZE * 2.0),
        latex("x".into(), f32::NAN),
        pdf(1),
    ] {
        project.sources[0].provider = Some(provider.clone());
        assert!(
            matches!(
                project.validate(),
                Err(crate::ModelError::InvalidProvider { .. }
                    | crate::ModelError::EmptyLocator { .. })
            ),
            "{provider:?}"
        );
    }
    project.sources[0].provider = Some(latex("x".into(), 64.0));
    project.sources[0].fingerprint = Some(crate::MediaFingerprint {
        content_sha256: None,
        byte_length: Some(1),
        modified_ms: None,
        streams: None,
        duration_ticks: None,
        color: None,
    });
    assert!(matches!(
        project.validate(),
        Err(crate::ModelError::InvalidProvider { .. })
    ));
}
