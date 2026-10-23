use serde_json::Value;

use crate::{AudioCodec, Container, ContainerError, VideoCodec, resolve_export_format};

fn corpus() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/cases/export-container-corpus.json"
    ))
    .expect("container corpus is JSON")
}

fn strings(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .expect("array")
        .iter()
        .map(|item| item.as_str().expect("string"))
        .collect()
}

fn optional<T: std::str::FromStr>(value: &Value) -> Option<T>
where
    T::Err: std::fmt::Debug,
{
    value.as_str().map(|text| text.parse().expect("known name"))
}

#[test]
fn the_policy_matches_the_shared_corpus() {
    let corpus = corpus();
    let containers = corpus["containers"].as_array().expect("containers");
    assert_eq!(containers.len(), Container::ALL.len());
    for (case, container) in containers.iter().zip(Container::ALL) {
        assert_eq!(case["id"], container.as_str());
        assert_eq!(case["id"].as_str().unwrap().parse(), Ok(container));
        assert_eq!(strings(&case["extensions"]), container.extensions());
        assert_eq!(case["mime"], container.mime_type());
        let video: Vec<&str> = container
            .video_codecs()
            .iter()
            .map(|c| c.as_str())
            .collect();
        assert_eq!(strings(&case["video"]), video, "{container}");
        let audio: Vec<&str> = container
            .audio_codecs()
            .iter()
            .map(|c| c.as_str())
            .collect();
        assert_eq!(strings(&case["audio"]), audio, "{container}");
        assert_eq!(
            case["defaultVideoCodec"],
            container.default_video_codec().as_str()
        );
        assert_eq!(
            case["defaultAudioCodec"],
            container.default_audio_codec().as_str()
        );
    }
}

#[test]
fn every_resolve_case_matches_the_shared_corpus() {
    for case in corpus()["resolve"].as_array().expect("resolve") {
        let resolved = resolve_export_format(
            optional(&case["container"]),
            case["extension"].as_str(),
            optional(&case["video"]),
            optional(&case["audio"]),
        );
        if let Some(ok) = case.get("ok") {
            let (container, video, audio) =
                resolved.unwrap_or_else(|error| panic!("{case}: {error}"));
            assert_eq!(
                strings(ok),
                [container.as_str(), video.as_str(), audio.as_str()],
                "{case}"
            );
        } else {
            let error = resolved.expect_err(&case.to_string());
            let kind = match error {
                ContainerError::Unknown(_) => "unknown",
                ContainerError::UnknownExtension(_) => "unknown-extension",
                ContainerError::ExtensionMismatch { .. } => "extension-mismatch",
                ContainerError::VideoCodecNotOffered { .. } => "video-codec-not-offered",
                ContainerError::AudioCodecNotOffered { .. } => "audio-codec-not-offered",
            };
            assert_eq!(case["error"], kind, "{case}: {error}");
        }
    }
}

#[test]
fn offered_codecs_are_engine_codecs_with_the_default_first() {
    for container in Container::ALL {
        assert_ne!(container.video_codecs(), []);
        assert_ne!(container.audio_codecs(), []);
        for codec in container.video_codecs() {
            assert!(VideoCodec::ALL.contains(codec));
            assert!(container.offers_video(*codec));
        }
        for codec in container.audio_codecs() {
            assert!(matches!(codec, AudioCodec::Aac | AudioCodec::Opus));
            assert!(container.offers_audio(*codec));
        }
        assert_eq!(container.default_video_codec(), container.video_codecs()[0]);
        assert_eq!(container.default_audio_codec(), container.audio_codecs()[0]);
        let spelled: Container =
            serde_json::from_value(serde_json::json!(container.as_str())).expect("serde");
        assert_eq!(spelled, container);
    }
}

#[test]
fn extensions_name_containers_case_insensitively() {
    assert_eq!(Container::from_extension("MOV"), Some(Container::Mov));
    assert_eq!(Container::from_extension("Webm"), Some(Container::Webm));
    assert_eq!(Container::from_extension("M4V"), Some(Container::Mp4));
    assert_eq!(Container::from_extension(""), None);
    assert_eq!(Container::from_extension("mp4.partial"), None);
    assert_eq!(Container::from_extension("matroska"), None);
    assert_eq!(
        "matroska".parse::<Container>(),
        Err(ContainerError::Unknown("matroska".to_owned()))
    );
}

#[test]
fn errors_name_what_the_container_offers() {
    let error = resolve_export_format(
        Some(Container::Webm),
        Some("webm"),
        Some(VideoCodec::H264),
        None,
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "webm does not offer video codec h264; it offers vp9, av1"
    );
    let error = resolve_export_format(Some(Container::Mov), Some("mp4"), None, None).unwrap_err();
    assert_eq!(
        error.to_string(),
        "a mov output must end in .mov, not \".mp4\""
    );
    let error = resolve_export_format(None, Some("avi"), None, None).unwrap_err();
    assert_eq!(
        error.to_string(),
        "the output extension \".avi\" names no container; use one of .mp4, .m4v, .mov, .mkv, .webm"
    );
}
