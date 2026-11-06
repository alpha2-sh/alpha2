//! The backend conformance suite on `VideoToolbox` and `AudioToolbox`, run on macOS, in the iOS
//! simulator, or on a device with the corpus copied to `ALPHA2_TESTS_DIR`.

use std::sync::Arc;

use alpha2_backend_conformance::{ContainerBackend, conformance_tests, corpus};
use alpha2_container::TrackKind;
use alpha2_host_core::audio_stream::{AudioBackends, WithPcm};
use alpha2_host_core::video_stream::{BackendKind, VideoBackends};

use crate::{AppleAudio, AppleVideo};

/// Whether some session opens for the default `kind` track of the corpus file `name`.
fn opens(name: &str, kind: TrackKind) -> bool {
    let Ok(reader) = alpha2_container::open(&corpus(name)) else {
        return false;
    };
    let Some(index) = alpha2_container::default_track(reader.tracks(), kind) else {
        return false;
    };
    let track = &reader.tracks()[index];
    match kind {
        TrackKind::Video => [BackendKind::Hardware, BackendKind::Software]
            .into_iter()
            .any(|kind| AppleVideo.open(track, kind).is_ok()),
        _ => AppleAudio.open(track).is_ok(),
    }
}

fn refuses(name: &str) -> Option<&'static str> {
    match name.rsplit('.').next().unwrap_or_default() {
        "mkv" | "webm" => return Some("Matroska is not read yet"),
        "ts" | "avi" => return Some("the container layer reads no MPEG-TS or AVI"),
        _ => {}
    }
    if matches!(name, "hdr-pq.mp4" | "hdr-pq-topleft.mp4") {
        return Some("VideoToolbox decodes 8-bit 4:2:0 H.264; these are 10-bit 4:4:4");
    }
    // What a system decodes depends on it: AV1 needs an AV1 decoder (hardware on recent devices),
    // the MPEG-4 Part 2 decoder may not read Advanced Simple Profile, AC-3 and E-AC-3 decoders
    // are not on every system, and some systems decode AAC and Opus to two channels at most.
    let kind = match name {
        "av1.mp4" | "av1-colocated.mp4" | "cfr-bt709-limited.mp4" | "long-gop.mp4" => {
            TrackKind::Video
        }
        "audio-ac3-5.1.mp4" | "audio-eac3-5.0.mp4" | "audio-aac-3.0.mp4" | "audio-adts-3.0.aac"
        | "audio-aac-quad.mp4" | "audio-aac-5.1.mp4" | "audio-aac-7.1.mp4"
        | "audio-opus-5.1.ogg" => TrackKind::Audio,
        _ => return None,
    };
    (!opens(name, kind)).then_some("this system has no decoder for it")
}

fn apple() -> ContainerBackend {
    ContainerBackend::new(
        "apple",
        Arc::new(AppleVideo),
        Arc::new(WithPcm(AppleAudio)),
        refuses,
    )
}

conformance_tests!(apple());

/// Samples by which a decoded tone of `frequency` Hz lags a sine starting at sample 0, measured
/// over 0.1 s to 0.3 s.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn tone_offset(samples: &[f32], frequency: f64, rate: u32) -> f64 {
    let step = std::f64::consts::TAU * frequency / f64::from(rate);
    let window = rate as usize / 10..rate as usize * 3 / 10;
    let (mut sine, mut cosine) = (0.0, 0.0);
    for (index, sample) in samples
        .iter()
        .enumerate()
        .take(window.end)
        .skip(window.start)
    {
        let angle = step * index as f64;
        sine += f64::from(*sample) * angle.sin();
        cosine += f64::from(*sample) * angle.cos();
    }
    -cosine.atan2(sine) / step
}

#[test]
fn decoded_tones_keep_their_timing() {
    use alpha2_host_core::audio_stream::AudioStream;
    use alpha2_media::{CancelToken, DecodeEvent, Decoder, Speaker};
    // Codecs whose decoders drop leading samples or hold some back, and some that do neither.
    let cases = [
        ("audio-ac3-5.1.mp4", Speaker::FrontLeft, 300.0, 48_000),
        ("audio-eac3-5.0.mp4", Speaker::FrontLeft, 300.0, 48_000),
        ("audio-opus-5.1.ogg", Speaker::FrontLeft, 300.0, 48_000),
        ("audio-mp3-lame.mp3", Speaker::FrontCenter, 440.0, 44_100),
        ("audio-aac-5.1.mp4", Speaker::FrontLeft, 300.0, 48_000),
        ("audio-flac-4.0.flac", Speaker::FrontLeft, 300.0, 48_000),
        ("audio-vorbis-5.1.ogg", Speaker::FrontLeft, 300.0, 48_000),
    ];
    let mut failures = Vec::new();
    for (name, speaker, frequency, rate) in cases {
        if refuses(name).is_some() {
            continue;
        }
        let mut stream = AudioStream::open_file(
            &corpus(name),
            &WithPcm(AppleAudio),
            rate,
            None,
            alpha2_host_core::DecodePolicy::default(),
        )
        .unwrap_or_else(|error| panic!("{name}: {error}"));
        let channel = stream
            .speakers()
            .speakers()
            .iter()
            .position(|named| *named == speaker)
            .expect("speaker");
        // The tone starts with the stream's first sample, after any skipped priming.
        let mut first = None;
        let mut samples = Vec::new();
        while let DecodeEvent::Audio(block) = stream
            .decode_next(&CancelToken::default())
            .unwrap_or_else(|error| panic!("{name}: {error}"))
        {
            let start = *first.get_or_insert(block.start_sample);
            let at = i64::try_from(samples.len()).expect("small");
            assert_eq!(block.start_sample - start, at, "{name}");
            samples.extend_from_slice(&block.channels[channel]);
        }
        let offset = tone_offset(&samples, frequency, rate);
        if offset.abs() > 1.0 {
            failures.push(format!("{name}: the tone is {offset:.2} samples late"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
