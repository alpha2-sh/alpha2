//! The backend conformance suite on `MediaCodec`, run on a device or emulator with the corpus
//! copied to `ALPHA2_TESTS_DIR`.

use std::sync::Arc;

use alpha2_backend_conformance::{ContainerBackend, conformance_tests};
use alpha2_container::TrackInfo;
use alpha2_host_core::audio_stream::WithPcm;
use alpha2_host_core::video_stream::{
    BackendError, BackendKind, VideoBackends, VideoDecodeBackend,
};

use crate::codec::Codec;
use crate::{AndroidAudio, MediaCodecVideo, VideoOutput};

fn refuses(name: &str) -> Option<&'static str> {
    if name.starts_with("audio-dts") {
        return Some("no mobile platform decodes DTS");
    }
    match name.rsplit('.').next().unwrap_or_default() {
        "ts" | "avi" => return Some("the container layer reads no MPEG-TS or AVI"),
        _ => {}
    }
    // H.264 High 4:4:4 Predictive, which sessions refuse where no decoder takes it.
    if matches!(name, "hdr-pq.mp4" | "hdr-pq-topleft.mp4") {
        return (!some_session_opens(name)).then_some("no decoder here takes its profile");
    }
    // What a device decodes depends on it: the API 29 emulator has no AV1 or AC-3 decoder and
    // only the software MPEG-4 decoder, which reads Simple Profile.
    let (mime, hardware) = match name {
        "av1.mp4" | "av1-colocated.mp4" => ("video/av01", false),
        "audio-ac3-5.1.mp4" => ("audio/ac3", false),
        "audio-eac3-5.0.mp4" => ("audio/eac3", false),
        "cfr-bt709-limited.mp4" | "long-gop.mp4" => ("video/mp4v-es", true),
        _ => return None,
    };
    let decodes = Codec::decoder_for(mime).is_ok_and(|codec| !hardware || !codec.is_software());
    (!decodes).then_some("this device has no decoder for it")
}

/// Whether a hardware or software session of the suite opens on the video of fixture `name`.
fn some_session_opens(name: &str) -> bool {
    let Ok(reader) = alpha2_container::open(&alpha2_backend_conformance::corpus(name)) else {
        return false;
    };
    reader
        .tracks()
        .iter()
        .find(|track| track.video.is_some())
        .is_some_and(|track| {
            [BackendKind::Hardware, BackendKind::Software]
                .into_iter()
                .any(|kind| SuiteVideo.open(track, kind).is_ok())
        })
}

/// The suite's video sessions. The emulator's Codec2 host-backed decoders (`c2.goldfish`) keep
/// the left columns of a picture whose stream crops columns off the left, and nothing in their
/// output format tells it apart from a correct crop, so those sessions decode in software here.
#[derive(Clone, Copy, Debug, Default)]
struct SuiteVideo;

impl VideoBackends for SuiteVideo {
    fn open(
        &self,
        track: &TrackInfo,
        kind: BackendKind,
    ) -> Result<Box<dyn VideoDecodeBackend>, BackendError> {
        let session = MediaCodecVideo::open(track, kind, VideoOutput::ByteBuffers)?;
        if session.component().starts_with("c2.goldfish.") {
            return Err(BackendError::Unsupported(format!(
                "{} crops leading columns off the wrong edge",
                session.component()
            )));
        }
        Ok(Box::new(session))
    }
}

fn mediacodec() -> ContainerBackend {
    // The platform software audio decoders compute in 16-bit fixed point: resumed after a seek,
    // their last bits differ from a straight decode by a few steps (each 1/32768).
    ContainerBackend::new(
        "mediacodec",
        Arc::new(SuiteVideo),
        Arc::new(WithPcm(AndroidAudio)),
        refuses,
    )
    .with_resumed_audio_tolerance(4.0 / 32_768.0)
}

conformance_tests!(mediacodec());

mod encode {
    //! Exports a synthetic clip through the encoders into MP4 or `WebM` and reads it back.

    use std::sync::Arc;

    use alpha2_container::{Mp4Flavor, Mp4Writer, TrackKind};
    use alpha2_host_core::audio_stream::{AudioStream, WithPcm};
    use alpha2_host_core::encode::{
        AudioEncodeConfig, AudioEncoders, EncodeSink, PlaneBuffer, VideoEncodeConfig, VideoEncoders,
    };
    use alpha2_host_core::video_stream::{StreamOptions, VideoStream};
    use alpha2_media::{
        AudioBlock, AudioBlockSink, AudioCodec, BitrateMode, CancelToken, ColorMetadata,
        DecodeEvent, Decoder, PixelLayout, SpeakerLayout, VideoCodec, VideoRateControl,
    };
    use alpha2_test_support::{Capability, skip};
    use alpha2_time::FrameRate;

    use super::SuiteVideo;
    use crate::{AndroidAudio, AndroidAudioEncoders, AndroidVideoEncoders};

    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 240;
    const FRAMES: u64 = 30;

    /// Frame `index`: flat luma `16 + 6 * index`, neutral chroma.
    fn planes(index: u64) -> Vec<PlaneBuffer> {
        let luma = u8::try_from(16 + 6 * index).expect("small");
        [
            (WIDTH, HEIGHT, luma),
            (WIDTH / 2, HEIGHT / 2, 128),
            (WIDTH / 2, HEIGHT / 2, 128),
        ]
        .into_iter()
        .map(|(width, height, value)| PlaneBuffer {
            bytes: vec![value; (width * height) as usize],
            stride: width as usize,
            width,
            height,
        })
        .collect()
    }

    /// One second of a 440 Hz tone at 0.25 in both channels, in blocks of 1000.
    fn tone(sink: &mut EncodeSink) {
        let cancel = CancelToken::default();
        for block in 0..48 {
            let start = block * 1000;
            let samples = (start..start + 1000)
                .map(|n| {
                    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                    let value = (std::f64::consts::TAU * 440.0 * n as f64 / 48_000.0).sin() as f32;
                    0.25 * value
                })
                .collect::<Vec<_>>();
            sink.write_audio(
                &AudioBlock {
                    start_sample: start,
                    sample_rate: 48_000,
                    speakers: SpeakerLayout::STEREO,
                    channels: vec![samples.clone(), samples],
                },
                &cancel,
            )
            .expect("audio");
        }
    }

    /// Whether this device encodes `video`, recording the skip of `capability` where it does not.
    fn encodes(video: VideoCodec, capability: Capability) -> bool {
        let available = AndroidVideoEncoders.available_codecs();
        if available.contains(&video) {
            return true;
        }
        skip!(
            capability,
            "no {video:?} encoder here (this device encodes {available:?})"
        );
        false
    }

    /// Exports the clip into `name`, `WebM` when `webm`, else MP4.
    fn export(name: &str, video: VideoCodec, audio: AudioCodec, webm: bool) -> std::path::PathBuf {
        let rate = FrameRate::new(30, 1).expect("rate");
        let config = VideoEncodeConfig {
            codec: video,
            width: WIDTH,
            height: HEIGHT,
            frame_rate: rate,
            layout: PixelLayout::YUV420P,
            rate: VideoRateControl {
                bitrate: 2_000_000,
                mode: BitrateMode::Variable,
                quantizer: None,
            },
            color: ColorMetadata::EXPORT,
            keyframe_interval: 10,
            software: false,
        };
        let encoder = AndroidVideoEncoders.open(&config).unwrap_or_else(|error| {
            panic!("{name}: the {video:?} encoder listed refuses: {error}")
        });
        let audio_config = AudioEncodeConfig {
            codec: audio,
            project_rate: 48_000,
            speakers: SpeakerLayout::STEREO,
            bitrate: 128_000,
        };
        let audio_encoder = AndroidAudioEncoders
            .open(&audio_config)
            .unwrap_or_else(|error| panic!("{name}: no {audio:?} encoder: {error}"));
        eprintln!(
            "{name}: {} (hardware {}), {}",
            encoder.name(),
            encoder.hardware(),
            audio_encoder.name()
        );
        let path = std::env::temp_dir().join(name);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("output");
        let writer: Box<dyn alpha2_container::ContainerWriter> = if webm {
            Box::new(alpha2_container::MatroskaWriter::create(
                file,
                alpha2_container::MatroskaFlavor::WebM,
            ))
        } else {
            Box::new(Mp4Writer::create(file, Mp4Flavor::Mp4))
        };
        let mut sink =
            EncodeSink::new(writer, encoder, config, Some((audio_encoder, audio_config)));
        for index in 0..FRAMES {
            sink.write_frame(index, &planes(index)).expect("frame");
        }
        tone(&mut sink);
        sink.finish().expect("finish");
        path
    }

    fn check(path: &std::path::Path) {
        let reader = alpha2_container::open(path).expect("reads back");
        let kinds = reader
            .tracks()
            .iter()
            .map(|track| track.kind)
            .collect::<Vec<_>>();
        assert_eq!(kinds, [TrackKind::Video, TrackKind::Audio]);
        drop(reader);
        let cancel = CancelToken::default();
        let mut video =
            VideoStream::open_file(path, Arc::new(SuiteVideo), StreamOptions::default())
                .expect("video");
        let mut lumas = Vec::new();
        while let Ok(DecodeEvent::Video(frame)) = video.decode_next(&cancel) {
            let plane = &frame.planes[0];
            let middle = (plane.height / 2 * plane.stride + plane.width / 2) as usize;
            lumas.push((frame.pts.ticks(), plane.bytes[middle]));
        }
        assert_eq!(lumas.len() as u64, FRAMES, "{lumas:?}");
        for (index, (_, luma)) in lumas.iter().enumerate() {
            let expected = 16 + 6 * index;
            // A flat frame at 2 Mbit/s comes back within the encoder's rounding of its value.
            assert!(
                usize::from(*luma).abs_diff(expected) <= 3,
                "frame {index}: {luma}"
            );
        }
        let mut sound = AudioStream::open_file(
            path,
            &WithPcm(AndroidAudio),
            48_000,
            None,
            alpha2_host_core::DecodePolicy::default(),
        )
        .expect("audio");
        let mut samples = Vec::new();
        let mut first = None;
        while let Ok(DecodeEvent::Audio(block)) = sound.decode_next(&cancel) {
            first.get_or_insert(block.start_sample);
            samples.extend_from_slice(&block.channels[0]);
        }
        assert_eq!(first, Some(0), "audio starts at zero");
        // Every written sample, and at most the rest of the last frame where a reader keeps the
        // frame whole.
        assert!(
            (48_000..48_000 + 1024).contains(&samples.len()),
            "{} samples",
            samples.len()
        );
        // The tone is where it was written: the coding error at 128 kbit/s stays under 0.05,
        // while a shift of 4 samples alone reaches 0.058 (0.0144 per sample at 440 Hz).
        let offset = (1000..1100)
            .map(|at| {
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let expected =
                    0.25 * (std::f64::consts::TAU * 440.0 * at as f64 / 48_000.0).sin() as f32;
                (samples[at] - expected).abs()
            })
            .fold(0.0_f32, f32::max);
        assert!(offset < 0.05, "the tone is shifted: {offset}");
    }

    #[test]
    fn h264_and_aac_export_reads_back_frame_for_frame() {
        let path = export("a2-h264-aac.mp4", VideoCodec::H264, AudioCodec::Aac, false);
        check(&path);
        let again = export(
            "a2-h264-aac-2.mp4",
            VideoCodec::H264,
            AudioCodec::Aac,
            false,
        );
        let same = std::fs::read(&path).ok() == std::fs::read(&again).ok();
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(again);
        assert!(same, "a repeated export on the same device differs");
    }

    #[test]
    fn vp9_and_av1_with_opus_export_to_webm_where_the_device_has_them() {
        for (video, capability, name) in [
            (
                VideoCodec::Vp9,
                Capability::MediacodecEncodeVp9,
                "a2-vp9-opus.webm",
            ),
            (
                VideoCodec::Av1,
                Capability::MediacodecEncodeAv1,
                "a2-av1-opus.webm",
            ),
        ] {
            if encodes(video, capability) {
                let path = export(name, video, AudioCodec::Opus, true);
                check(&path);
                let _ = std::fs::remove_file(path);
            }
        }
    }

    #[test]
    fn hevc_and_opus_export_reads_back_where_the_device_has_them() {
        if encodes(VideoCodec::Hevc, Capability::MediacodecEncodeHevc) {
            let path = export(
                "a2-hevc-opus.mp4",
                VideoCodec::Hevc,
                AudioCodec::Opus,
                false,
            );
            check(&path);
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn every_listed_audio_encoder_opens() {
        let available = AndroidAudioEncoders.available_codecs();
        assert!(available.contains(&AudioCodec::Aac), "{available:?}");
        for codec in available {
            let config = AudioEncodeConfig {
                codec,
                project_rate: 48_000,
                speakers: SpeakerLayout::STEREO,
                bitrate: 128_000,
            };
            AndroidAudioEncoders
                .open(&config)
                .unwrap_or_else(|error| panic!("{codec:?}: {error}"));
        }
    }
}
