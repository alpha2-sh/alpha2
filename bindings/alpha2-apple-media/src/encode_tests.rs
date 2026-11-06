use std::sync::Arc;

use alpha2_container::{Mp4Flavor, Mp4Writer, TrackKind};
use alpha2_host_core::DecodePolicy;
use alpha2_host_core::audio_stream::{AudioStream, WithPcm};
use alpha2_host_core::encode::{
    AudioEncodeConfig, AudioEncoders, EncodeSink, VideoEncodeConfig, VideoEncoders,
};
use alpha2_host_core::export::{PlaneBuffer, VideoSink};
use alpha2_host_core::video_stream::{StreamOptions, VideoStream};
use alpha2_media::{
    AudioBlock, AudioBlockSink, AudioCodec, BitDepth, BitrateMode, CancelToken, ColorMetadata,
    PixelLayout, SpeakerLayout, VideoCodec, VideoRateControl,
};
use alpha2_time::{FrameRate, MediaTime};

use crate::tests::tone_offset;
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

/// Encodes the clip to `path` with the first encoder that opens, hardware before software;
/// returns the video encoder's name.
fn encode(
    path: &std::path::Path,
    codec: VideoCodec,
    layout: PixelLayout,
    rate: VideoRateControl,
    audio: AudioCodec,
) -> Result<String, String> {
    let frame_rate = FrameRate::new(30, 1).expect("rate");
    let mut config = VideoEncodeConfig {
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
    let video = AppleVideoEncoders.open(&config).or_else(|hardware| {
        config.software = true;
        AppleVideoEncoders
            .open(&config)
            .map_err(|software| format!("hardware: {hardware}; software: {software}"))
    })?;
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
    let mut sink = EncodeSink::new(writer, video, config, Some((audio, audio_config)));
    let name = sink.video_encoder().0.to_owned();
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
    Ok(name)
}

/// Reads `path` back: packets in order with keyframes at the interval, every frame at its level,
/// and the tone from sample 0 within `tolerance` samples of its phase.
fn check(path: &std::path::Path, layout: PixelLayout, tolerance: f64) -> Vec<String> {
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
    let encoder = encode(&path, codec, layout, rate, audio)
        .unwrap_or_else(|error| panic!("{codec:?} {audio:?}: {error}"));
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
