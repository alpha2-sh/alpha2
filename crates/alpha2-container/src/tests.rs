use super::*;

fn track(kind: TrackKind, enabled: bool, default: bool) -> TrackInfo {
    TrackInfo {
        id: TrackId(1),
        kind,
        timescale: 1_000,
        duration: None,
        language: None,
        title: None,
        default,
        enabled,
        video: None,
        audio: None,
    }
}

#[test]
fn the_default_track_is_the_first_enabled_default_else_the_first() {
    let tracks = [
        track(TrackKind::Video, true, false),
        track(TrackKind::Audio, true, false),
        track(TrackKind::Audio, false, true),
        track(TrackKind::Audio, true, true),
    ];
    assert_eq!(default_track(&tracks, TrackKind::Audio), Some(3));
    assert_eq!(default_track(&tracks, TrackKind::Video), Some(0));
    assert_eq!(default_track(&tracks[..2], TrackKind::Audio), Some(1));
    assert_eq!(default_track(&tracks, TrackKind::Other), None);
}

#[test]
fn files_are_recognized_by_their_first_bytes() {
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus");
    for (name, expected) in [
        ("h264-bt709-copy.mp4", Some(ContainerFormat::Mp4)),
        ("audio-lcr.mov", Some(ContainerFormat::Mp4)),
        ("cover-art.m4a", Some(ContainerFormat::Mp4)),
        ("h264-bt709-copy.mkv", Some(ContainerFormat::Matroska)),
        ("vp9.webm", Some(ContainerFormat::Matroska)),
        ("audio-quad.wav", Some(ContainerFormat::Wav)),
        ("audio-adts.aac", Some(ContainerFormat::Adts)),
        ("audio-flac-4.0.flac", Some(ContainerFormat::Flac)),
        ("audio-vorbis-5.1.ogg", Some(ContainerFormat::Ogg)),
        ("h264-bt709-copy.ts", None),
        ("odd-dimensions.avi", None),
        ("parity-image.png", None),
    ] {
        let head = std::fs::read(corpus.join(name)).expect(name);
        let found = sniff(&head[..head.len().min(4096)]);
        match expected {
            Some(format) => assert_eq!(found.ok(), Some(format), "{name}"),
            None => assert!(
                matches!(found, Err(DemuxError::Unsupported(_))),
                "{name}: {found:?}"
            ),
        }
    }
}
