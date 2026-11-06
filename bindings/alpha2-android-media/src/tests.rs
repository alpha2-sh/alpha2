//! The backend conformance suite on `MediaCodec`, run on a device or emulator with the corpus
//! copied to `ALPHA2_TESTS_DIR`.

use std::sync::Arc;

use alpha2_backend_conformance::{ContainerBackend, conformance_tests};
use alpha2_host_core::audio_stream::WithPcm;

use crate::codec::Codec;
use crate::{AndroidAudio, AndroidVideo};

fn refuses(name: &str) -> Option<&'static str> {
    if name.starts_with("audio-dts") {
        return Some("no mobile platform decodes DTS");
    }
    if matches!(name, "hdr-pq.mp4" | "hdr-pq-topleft.mp4") {
        return Some("Android decodes H.264 up to High profile; these are 10-bit 4:4:4");
    }
    match name.rsplit('.').next().unwrap_or_default() {
        "ts" | "avi" => return Some("the container layer reads no MPEG-TS or AVI"),
        _ => {}
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

fn mediacodec() -> ContainerBackend {
    ContainerBackend::new(
        "mediacodec",
        Arc::new(AndroidVideo),
        Arc::new(WithPcm(AndroidAudio)),
        refuses,
    )
}

conformance_tests!(mediacodec());

mod encode {
    //! Exports a synthetic clip through the encoders into MP4 and reads it back.

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
    use alpha2_time::FrameRate;

    use crate::{AndroidAudio, AndroidAudioEncoders, AndroidVideo, AndroidVideoEncoders};

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

    fn export(name: &str, video: VideoCodec, audio: AudioCodec) -> Option<std::path::PathBuf> {
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
        let encoder = match AndroidVideoEncoders.open(&config) {
            Ok(encoder) => encoder,
            Err(error) => {
                eprintln!("{name}: no {video:?} encoder here: {error}");
                return None;
            }
        };
        let audio_config = AudioEncodeConfig {
            codec: audio,
            project_rate: 48_000,
            speakers: SpeakerLayout::STEREO,
            bitrate: 128_000,
        };
        let audio_encoder = match AndroidAudioEncoders.open(&audio_config) {
            Ok(encoder) => encoder,
            Err(error) => {
                eprintln!("{name}: no {audio:?} encoder here: {error}");
                return None;
            }
        };
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
        let mut sink = EncodeSink::new(
            Box::new(Mp4Writer::create(file, Mp4Flavor::Mp4)),
            encoder,
            config,
            Some((audio_encoder, audio_config)),
        );
        for index in 0..FRAMES {
            sink.write_frame(index, &planes(index)).expect("frame");
        }
        tone(&mut sink);
        sink.finish().expect("finish");
        Some(path)
    }

    fn check(path: &std::path::Path, audio: AudioCodec) {
        let reader = alpha2_container::open(path).expect("reads back");
        let kinds = reader
            .tracks()
            .iter()
            .map(|track| track.kind)
            .collect::<Vec<_>>();
        assert_eq!(kinds, [TrackKind::Video, TrackKind::Audio]);
        let priming = reader.tracks()[1].audio.as_ref().expect("audio").priming;
        eprintln!("{}: priming {priming}", path.display());
        drop(reader);
        let cancel = CancelToken::default();
        let mut video =
            VideoStream::open_file(path, Arc::new(AndroidVideo), StreamOptions::default())
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
        // Every written sample, and at most a frame of the encoder's padding.
        assert!(
            (48_000..48_000 + 1024).contains(&samples.len()),
            "{} samples",
            samples.len()
        );
        // The tone is where it was written: compare a stretch after the codec settles.
        let offset = (1000..1100)
            .map(|at| {
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let expected =
                    0.25 * (std::f64::consts::TAU * 440.0 * at as f64 / 48_000.0).sin() as f32;
                (samples[at] - expected).abs()
            })
            .fold(0.0_f32, f32::max);
        eprintln!("{audio:?}: largest tone error {offset}");
        assert!(offset < 0.05, "the tone is shifted: {offset}");
    }

    #[test]
    fn h264_and_aac_export_reads_back_frame_for_frame() {
        let path = export("a2-h264-aac.mp4", VideoCodec::H264, AudioCodec::Aac).expect("H.264");
        check(&path, AudioCodec::Aac);
        let again = export("a2-h264-aac-2.mp4", VideoCodec::H264, AudioCodec::Aac).expect("H.264");
        let same = std::fs::read(&path).ok() == std::fs::read(&again).ok();
        eprintln!("same-device repeat byte-identical: {same}");
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(again);
    }

    #[test]
    fn hevc_and_opus_export_reads_back_where_the_device_has_them() {
        if let Some(path) = export("a2-hevc-opus.mp4", VideoCodec::Hevc, AudioCodec::Opus) {
            check(&path, AudioCodec::Opus);
            let _ = std::fs::remove_file(path);
        }
    }
}

