use std::sync::Arc;
use std::sync::atomic::Ordering;

use alpha2_container::{Mp4Flavor, Mp4Writer, TrackKind};
use alpha2_host_core::DecodePolicy;
use alpha2_host_core::audio_stream::{AudioStream, WithPcm};
use alpha2_host_core::encode::{
    AudioEncodeConfig, AudioEncoders, EncodePoll, EncodeSink, VideoEncodeBackend,
    VideoEncodeConfig, VideoEncoders,
};
use alpha2_host_core::export::{PlaneBuffer, VideoSink};
use alpha2_host_core::video_stream::{StreamOptions, Submit, VideoStream};
use alpha2_media::{
    AudioBlock, AudioBlockSink, AudioCodec, BitDepth, BitrateMode, CancelToken, ColorMetadata,
    PixelLayout, SpeakerLayout, VideoCodec, VideoRateControl,
};
use alpha2_time::{FrameRate, MediaTime};

use crate::tests::tone_offset;
use crate::video_encode::{MAX_IN_FLIGHT, VideoToolboxEncoder, codec_type, hooks, listed};
use crate::{AppleAudio, AppleAudioEncoders, AppleVideo, AppleVideoEncoders};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const FRAMES: u64 = 30;
const KEYFRAME_INTERVAL: u32 = 10;
const RATE: u32 = 48_000;
const TONE: f64 = 440.0;

/// The luma level of frame `index`, apart from its neighbors' after lossy coding.
fn level(index: u64) -> u16 {
    40 + u16::try_from(index).expect("small") * 6
}

/// A flat frame at `level(index)` with neutral chroma, in `layout`'s planes.
fn frame(index: u64, layout: PixelLayout) -> Vec<PlaneBuffer> {
    let (width, height) = (WIDTH as usize, HEIGHT as usize);
    let plane = |columns: usize, rows: usize, sample: &[u8]| PlaneBuffer {
        bytes: sample.repeat(columns * rows),
        stride: columns * sample.len(),
        width: u32::try_from(columns).expect("small"),
        height: u32::try_from(rows).expect("small"),
    };
    if layout.bit_depth == BitDepth::Eight {
        let luma = u8::try_from(level(index)).expect("8 bits");
        return vec![
            plane(width, height, &[luma]),
            plane(width / 2, height / 2, &[128]),
            plane(width / 2, height / 2, &[128]),
        ];
    }
    // P010: 10-bit codes in the high bits.
    let luma = (level(index) << 2 << 6).to_le_bytes();
    let neutral = (512_u16 << 6).to_le_bytes();
    vec![
        plane(width, height, &luma),
        PlaneBuffer {
            stride: width * 2,
            ..plane(
                width / 2,
                height / 2,
                &[neutral[0], neutral[1], neutral[0], neutral[1]],
            )
        },
    ]
}

/// One second of a stereo tone in blocks of 1024 samples.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn tone_blocks() -> Vec<AudioBlock> {
    let speakers = SpeakerLayout::default_for(2).expect("stereo");
    (0..RATE as usize)
        .step_by(1_024)
        .map(|start| {
            let samples = (start..(start + 1_024).min(RATE as usize))
                .map(|n| {
                    let phase = std::f64::consts::TAU * TONE * n as f64 / f64::from(RATE);
                    (phase.sin() * 0.25) as f32
                })
                .collect::<Vec<_>>();
            AudioBlock {
                start_sample: i64::try_from(start).expect("small"),
                sample_rate: RATE,
                speakers,
                channels: vec![samples.clone(), samples],
            }
        })
        .collect()
}

/// Encodes the clip to `path`; returns the video encoder's name and whether it is hardware.
fn encode(
    path: &std::path::Path,
    codec: VideoCodec,
    layout: PixelLayout,
    rate: VideoRateControl,
    audio: AudioCodec,
) -> Result<(String, bool), String> {
    let frame_rate = FrameRate::new(30, 1).expect("rate");
    let config = VideoEncodeConfig {
        codec,
        width: WIDTH,
        height: HEIGHT,
        frame_rate,
        layout,
        rate,
        color: ColorMetadata::EXPORT,
        keyframe_interval: KEYFRAME_INTERVAL,
        software: false,
    };
    let video = AppleVideoEncoders
        .open(&config)
        .map_err(|error| error.to_string())?;
    let audio_config = AudioEncodeConfig {
        codec: audio,
        project_rate: RATE,
        speakers: SpeakerLayout::default_for(2).expect("stereo"),
        bitrate: 128_000,
    };
    let audio = AppleAudioEncoders
        .open(&audio_config)
        .map_err(|error| error.to_string())?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    let writer = Box::new(Mp4Writer::create(file, Mp4Flavor::Mp4));
    let mut sink = EncodeSink::new(
        writer,
        video,
        config,
        Some((audio, audio_config)),
        alpha2_media::CancelToken::default(),
    );
    let (name, hardware, _) = sink.video_encoder();
    let encoder = (name.to_owned(), hardware);
    let cancel = CancelToken::default();
    for index in 0..FRAMES {
        let time = MediaTime::from_ticks(0);
        sink.write_video(index, time, layout, &frame(index, layout))
            .map_err(|error| error.to_string())?;
    }
    for block in tone_blocks() {
        sink.write_audio(&block, &cancel)
            .map_err(|error| error.to_string())?;
    }
    sink.finish().map_err(|error| error.to_string())?;
    Ok(encoder)
}

/// Reads the video of `path` back: packets in order with keyframes at the interval, every frame
/// at its level. Returns the failures and the keyframe indices.
fn check_video(path: &std::path::Path, layout: PixelLayout) -> (Vec<String>, Vec<u64>) {
    let mut failures = Vec::new();
    let mut reader = alpha2_container::open(path).expect("readable output");
    let video = alpha2_container::default_track(reader.tracks(), TrackKind::Video).expect("video");
    let id = reader.tracks()[video].id;
    let mut keyframes = Vec::new();
    let mut last = None;
    let mut packets = 0;
    while let Some(packet) = reader.next_packet(id).expect("packet") {
        if packet.keyframe {
            keyframes.push(packets);
        }
        if last.is_some_and(|last| packet.pts <= last) || packet.dts != packet.pts {
            failures.push(format!(
                "packet {packets} at {} is out of order",
                packet.pts
            ));
        }
        last = Some(packet.pts);
        packets += 1;
    }
    let forced = (0..FRAMES).step_by(KEYFRAME_INTERVAL as usize);
    if packets != FRAMES || forced.clone().any(|index| !keyframes.contains(&index)) {
        failures.push(format!("{packets} packets with keyframes at {keyframes:?}"));
    }

    let mut stream = VideoStream::open_file(path, Arc::new(AppleVideo), StreamOptions::default())
        .expect("video opens");
    let cancel = CancelToken::default();
    let mut index = 0;
    while let Some(frame) = stream.next(&cancel).expect("frame") {
        let decoded = frame.to_cpu().expect("CPU frame");
        let plane = &decoded.planes[0];
        let (row, column) = (plane.height as usize / 2, plane.width as usize / 2);
        let at = row * plane.stride as usize;
        // P010 keeps the 10-bit code in the high bits, so the high byte is the 8-bit level.
        let sample = if layout.bit_depth == BitDepth::Eight {
            u16::from(plane.bytes[at + column])
        } else {
            u16::from(plane.bytes[at + column * 2 + 1])
        };
        let expected = level(index);
        if sample.abs_diff(expected) > 3 {
            failures.push(format!("frame {index}: luma {sample}, not {expected}"));
        }
        index += 1;
    }
    if index != FRAMES {
        failures.push(format!("{index} frames decoded, not {FRAMES}"));
    }
    (failures, keyframes)
}

/// Reads `path` back: the video as [`check_video`] does, and the tone from sample 0 within
/// `tolerance` samples of its phase.
fn check(path: &std::path::Path, layout: PixelLayout, tolerance: f64) -> Vec<String> {
    let (mut failures, _) = check_video(path, layout);
    let cancel = CancelToken::default();
    let mut audio = AudioStream::open_file(
        path,
        &WithPcm(AppleAudio),
        RATE,
        None,
        DecodePolicy::default(),
    )
    .expect("audio opens");
    let mut first = None;
    let mut samples = Vec::new();
    while let Some(block) = audio.next_block(&cancel).expect("block") {
        first.get_or_insert(block.start_sample);
        samples.extend_from_slice(&block.channels[0]);
    }
    if first != Some(0) {
        failures.push(format!("the audio starts at {first:?}, not 0"));
    }
    if samples.len() < RATE as usize - 1_024 {
        failures.push(format!("{} samples of audio", samples.len()));
    }
    let offset = tone_offset(&samples, TONE, RATE);
    if offset.abs() > tolerance {
        failures.push(format!("the tone is {offset:.2} samples late"));
    }
    failures
}

fn round_trip(codec: VideoCodec, layout: PixelLayout, rate: VideoRateControl, audio: AudioCodec) {
    static CASE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let case = CASE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "alpha2-apple-encode-{}-{case}.mp4",
        std::process::id()
    ));
    let (encoder, hardware) = encode(&path, codec, layout, rate, audio)
        .unwrap_or_else(|error| panic!("{codec:?} {audio:?}: {error}"));
    let listed = codec_type(codec).map(|(codec_type, _)| listed(codec_type));
    assert_eq!(
        hardware,
        listed.is_some_and(|listed| listed.iter().any(|encoder| encoder.hardware)),
        "{encoder} is chosen for {codec:?}, hardware before software"
    );
    // Opus's own phase response puts the tone 3.3 samples early, in `FFmpeg`'s decode of the
    // same file too; a wrong priming would be off by hundreds.
    let tolerance = if audio == AudioCodec::Opus { 4.0 } else { 2.0 };
    let failures = check(&path, layout, tolerance);
    let _ = std::fs::remove_file(&path);
    assert!(
        failures.is_empty(),
        "{encoder} with {audio:?}: {failures:#?}"
    );
}

const BITRATE: VideoRateControl = VideoRateControl {
    bitrate: 2_000_000,
    mode: BitrateMode::Variable,
    quantizer: None,
};

#[test]
fn h264_and_aac_round_trip_through_mp4() {
    round_trip(
        VideoCodec::H264,
        PixelLayout::YUV420P,
        BITRATE,
        AudioCodec::Aac,
    );
}

#[test]
fn h264_at_a_quantizer_and_a_constant_rate_round_trips() {
    let quantizer = VideoRateControl {
        quantizer: Some(23),
        ..BITRATE
    };
    round_trip(
        VideoCodec::H264,
        PixelLayout::YUV420P,
        quantizer,
        AudioCodec::Aac,
    );
    let constant = VideoRateControl {
        mode: BitrateMode::Constant,
        ..BITRATE
    };
    round_trip(
        VideoCodec::H264,
        PixelLayout::YUV420P,
        constant,
        AudioCodec::Aac,
    );
}

#[test]
fn hevc_and_opus_round_trip_through_mp4() {
    round_trip(
        VideoCodec::Hevc,
        PixelLayout::YUV420P,
        BITRATE,
        AudioCodec::Opus,
    );
}

#[test]
fn hevc_main10_round_trips_through_mp4() {
    let p010 = PixelLayout {
        bit_depth: BitDepth::Ten,
        ..PixelLayout::YUV420P
    }
    .as_p010();
    round_trip(VideoCodec::Hevc, p010, BITRATE, AudioCodec::Aac);
}

#[test]
fn every_available_codec_opens_an_encoder() {
    let video = AppleVideoEncoders.available_codecs();
    assert!(video.contains(&VideoCodec::H264), "{video:?}");
    for codec in video {
        let config = VideoEncodeConfig {
            codec,
            width: WIDTH,
            height: HEIGHT,
            frame_rate: FrameRate::new(30, 1).expect("rate"),
            layout: PixelLayout::YUV420P,
            rate: BITRATE,
            color: ColorMetadata::EXPORT,
            keyframe_interval: KEYFRAME_INTERVAL,
            software: false,
        };
        if let Err(error) = AppleVideoEncoders.open(&config) {
            panic!("{codec:?}: {error}");
        }
    }
    let audio = AppleAudioEncoders.available_codecs();
    assert!(audio.contains(&AudioCodec::Aac), "{audio:?}");
    for codec in audio {
        let config = AudioEncodeConfig {
            codec,
            project_rate: RATE,
            speakers: SpeakerLayout::default_for(2).expect("stereo"),
            bitrate: 128_000,
        };
        if let Err(error) = AppleAudioEncoders.open(&config) {
            panic!("{codec:?}: {error}");
        }
    }
}

/// Planes of noise, so every frame costs the encoder real work.
fn noise(width: u32, height: u32, seed: u32) -> Vec<PlaneBuffer> {
    let plane = |columns: u32, rows: u32, salt: u32| {
        let mut state = seed.wrapping_mul(2_654_435_761).wrapping_add(salt) | 1;
        let bytes = (0..columns as usize * rows as usize)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                state.to_le_bytes()[3]
            })
            .collect();
        PlaneBuffer {
            bytes,
            stride: columns as usize,
            width: columns,
            height: rows,
        }
    };
    vec![
        plane(width, height, 1),
        plane(width / 2, height / 2, 2),
        plane(width / 2, height / 2, 3),
    ]
}

fn h264_config(width: u32, height: u32, software: bool) -> VideoEncodeConfig {
    VideoEncodeConfig {
        codec: VideoCodec::H264,
        width,
        height,
        frame_rate: FrameRate::new(30, 1).expect("rate"),
        layout: PixelLayout::YUV420P,
        rate: BITRATE,
        color: ColorMetadata::EXPORT,
        keyframe_interval: KEYFRAME_INTERVAL,
        software,
    }
}

#[test]
fn an_encoder_dropped_with_frames_in_flight_completes_them_first() {
    for software in [false, true] {
        let config = h264_config(640, 360, software);
        let pictures = (0..4).map(|seed| noise(640, 360, seed)).collect::<Vec<_>>();
        for round in 0..100 {
            let mut encoder = VideoToolboxEncoder::open(&config).expect("an encoder opens");
            let callbacks = encoder.callbacks();
            let step = i64::from(encoder.timescale()) / 30;
            let mut accepted = 0;
            for index in 0..16_i64 {
                let picture = &pictures[usize::try_from(index % 4).expect("small")];
                match encoder.submit(index * step, picture, index == 0) {
                    Ok(Submit::Accepted) => accepted += 1,
                    Ok(Submit::Busy) => break,
                    Err(error) => panic!("round {round}, frame {index}: {error}"),
                }
            }
            drop(encoder);
            assert_eq!(
                callbacks.load(Ordering::SeqCst),
                accepted,
                "software {software}, round {round}: every submitted frame completed before the drop returned"
            );
        }
    }
}

#[test]
fn an_encoder_without_a_delay_bound_holds_at_most_the_frames_in_flight_cap() {
    let (width, height) = (3840, 2160);
    hooks::REFUSE_DELAY_BOUND.set(true);
    let opened = VideoToolboxEncoder::open(&h264_config(width, height, true));
    hooks::REFUSE_DELAY_BOUND.set(false);
    let mut encoder = opened.expect("a software encoder opens");
    let pictures = (0..4)
        .map(|seed| noise(width, height, seed))
        .collect::<Vec<_>>();
    let step = i64::from(encoder.timescale()) / 30;
    let mut most = 0;
    let mut index = 0_i64;
    while index < 40 {
        let picture = &pictures[usize::try_from(index % 4).expect("small")];
        match encoder.submit(index * step, picture, index == 0) {
            Ok(Submit::Accepted) => index += 1,
            Ok(Submit::Busy) => {
                while let EncodePoll::Packet(_) = encoder.receive(true).expect("a packet") {}
            }
            Err(error) => panic!("frame {index}: {error}"),
        }
        most = most.max(encoder.in_flight());
    }
    assert!(
        most <= MAX_IN_FLIGHT,
        "{most} frames in flight, over {MAX_IN_FLIGHT}"
    );
}

/// Encodes the flat clip without audio to `path`, losing the session at frame `lose_at` and
/// settling before frame `settle_at`.
fn encode_through_a_loss(
    path: &std::path::Path,
    codec: VideoCodec,
    lose_at: Option<(u64, bool)>,
    settle_at: Option<u64>,
) -> Result<(), alpha2_media::EncodeError> {
    let config = VideoEncodeConfig {
        codec,
        ..h264_config(WIDTH, HEIGHT, false)
    };
    let mut encoder = VideoToolboxEncoder::open(&config)?;
    if let Some((index, other_record)) = lose_at {
        encoder.lose_session_at(index, other_record);
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .expect("output");
    let writer = Box::new(Mp4Writer::create(file, Mp4Flavor::Mp4));
    let mut sink = EncodeSink::new(
        writer,
        Box::new(encoder),
        config,
        None,
        CancelToken::default(),
    );
    for index in 0..FRAMES {
        if settle_at == Some(index) {
            sink.settle()?;
        }
        sink.write_frame(index, &frame(index, PixelLayout::YUV420P))?;
    }
    sink.finish()
}

fn scratch_mp4(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("alpha2-apple-{name}-{}.mp4", std::process::id()))
}

#[test]
fn a_lost_session_is_replaced_and_the_stream_goes_on_from_a_keyframe() {
    for codec in [VideoCodec::H264, VideoCodec::Hevc] {
        let path = scratch_mp4(&format!("lost-{codec:?}"));
        encode_through_a_loss(&path, codec, Some((18, false)), None)
            .unwrap_or_else(|error| panic!("{codec:?}: {error}"));
        let (failures, keyframes) = check_video(&path, PixelLayout::YUV420P);
        let _ = std::fs::remove_file(&path);
        assert!(failures.is_empty(), "{codec:?}: {failures:#?}");
        let resumed = keyframes.iter().any(|&index| index > 10 && index <= 18);
        assert!(
            resumed,
            "{codec:?}: keyframes {keyframes:?}, none where the session was replaced"
        );
    }
}

#[test]
fn a_replaced_session_with_another_configuration_rejects_the_encoder() {
    let path = scratch_mp4("lost-other-record");
    let result = encode_through_a_loss(&path, VideoCodec::H264, Some((18, true)), None);
    let _ = std::fs::remove_file(&path);
    match result {
        Err(alpha2_media::EncodeError::EncoderRejected { message, .. }) => {
            assert!(message.contains("configuration record"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn settling_mid_stream_keeps_every_frame() {
    for codec in [VideoCodec::H264, VideoCodec::Hevc] {
        let path = scratch_mp4(&format!("settled-{codec:?}"));
        encode_through_a_loss(&path, codec, None, Some(7))
            .unwrap_or_else(|error| panic!("{codec:?}: {error}"));
        let (failures, _) = check_video(&path, PixelLayout::YUV420P);
        let _ = std::fs::remove_file(&path);
        assert!(failures.is_empty(), "{codec:?}: {failures:#?}");
    }
}
