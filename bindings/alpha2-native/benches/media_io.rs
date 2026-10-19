//! Standalone benchmark for native media I/O around the engine.
//!
//! - `wav_sink`: wall time to write 60 s of 48 kHz stereo through
//!   `NativeWavEncodeSink` in 4096-sample blocks.
//! - `decode_convert`: software-decoding 1080p FFV1 in yuv420p, yuv444p,
//!   yuv420p10le and yuv444p10le and converting each frame for upload. The
//!   same codec in all four isolates the layout: every one reaches the
//!   renderer as the decoder's own planes, so `convert_ms_per_frame` is only
//!   their description, near zero, and 10-bit samples are normalized on the
//!   GPU. `alpha2-render`'s `plane_upload` bench times handing such planes
//!   (and P010's, a hardware decoder's) to the renderer.
//!
//! Needs the pinned `FFmpeg` SDK runtime (`scripts/fetch-ffmpeg-sdk.sh`).
//! Times are recorded, never gated.

use std::{
    ffi::CString,
    path::{Path, PathBuf},
    time::Instant,
};

use alpha2_media::{AudioBlock, AudioBlockSink, CancelToken};
use alpha2_native::{NativeExportDecoder, NativeWavEncodeSink};
use rsmpeg::{
    avcodec::{AVCodec, AVCodecContext},
    avformat::AVFormatContextOutput,
    avutil::{AVFrame, ra},
    error::RsmpegError,
    ffi,
    swscale::SwsContext,
};

const WIDTH: i32 = 1_920;
const HEIGHT: i32 = 1_080;
const FRAMES: i64 = 30;

fn main() {
    let directory = std::env::temp_dir().join(format!("alpha2-media-io-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("scratch directory");
    wav_sink(&directory.join("sink.wav"));
    for (name, format) in [
        ("yuv420p", ffi::AV_PIX_FMT_YUV420P),
        ("yuv444p", ffi::AV_PIX_FMT_YUV444P),
        ("yuv420p10le", ffi::AV_PIX_FMT_YUV420P10LE),
        ("yuv444p10le", ffi::AV_PIX_FMT_YUV444P10LE),
    ] {
        let path = directory.join(format!("{name}.mkv"));
        write_ffv1(&path, format);
        decode_convert(name, &path);
    }
    std::fs::remove_dir_all(&directory).ok();
}

fn wav_sink(path: &Path) {
    const RATE: u32 = 48_000;
    const BLOCK: usize = 4_096;
    let cancel = CancelToken::default();
    let mut sink = NativeWavEncodeSink::create(path, RATE, 2).expect("WAV sink");
    let total = RATE as usize * 60;
    let started = Instant::now();
    let mut written = 0;
    while written < total {
        let frames = BLOCK.min(total - written);
        let block = AudioBlock {
            start_sample: i64::try_from(written).expect("small"),
            sample_rate: RATE,
            speakers: alpha2_media::SpeakerLayout::STEREO,
            channels: vec![vec![0.25; frames]; 2],
        };
        sink.write_audio(&block, &cancel).expect("WAV block");
        written += frames;
    }
    sink.finish_audio(&cancel).expect("WAV finish");
    println!(
        "wav_sink seconds=60 channels=2 wall_ms={:.1}",
        started.elapsed().as_secs_f64() * 1e3
    );
}

fn decode_convert(name: &str, path: &PathBuf) {
    let cancel = CancelToken::default();
    let mut decoder = NativeExportDecoder::open_with_threads(path, 1).expect("decoder");
    let started = Instant::now();
    let mut frames = 0_u32;
    while decoder
        .decode_video_next(&cancel)
        .expect("decode")
        .is_some()
    {
        frames += 1;
    }
    let wall = started.elapsed().as_secs_f64();
    let stages = decoder.take_stage_timings();
    let per_frame = |seconds: f64| seconds / f64::from(frames) * 1e3;
    println!(
        "decode_convert source={name} size={WIDTH}x{HEIGHT} frames={frames} \
         decode_ms_per_frame={:.2} convert_ms_per_frame={:.2}",
        per_frame(wall),
        per_frame(stages.convert.as_secs_f64()),
    );
}

/// Writes `FRAMES` frames of a moving gradient as FFV1 in `format`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn write_ffv1(path: &Path, format: ffi::AVPixelFormat) {
    let codec = AVCodec::find_encoder_by_name(c"ffv1").expect("FFV1 encoder");
    let mut encoder = AVCodecContext::new(&codec);
    encoder.set_width(WIDTH);
    encoder.set_height(HEIGHT);
    encoder.set_pix_fmt(format);
    encoder.set_time_base(ra(1, 30));
    encoder.open(None).expect("open FFV1");
    let path = CString::new(path.to_str().expect("UTF-8 path")).expect("no NUL");
    let mut output = AVFormatContextOutput::create(&path).expect("output");
    {
        let mut stream = output.new_stream();
        stream.set_time_base(ra(1, 30));
        stream.set_codecpar(encoder.extract_codecpar());
    }
    output.write_header(&mut None).expect("header");
    let mut scaler = SwsContext::get_context(
        WIDTH,
        HEIGHT,
        ffi::AV_PIX_FMT_RGBA,
        WIDTH,
        HEIGHT,
        format,
        ffi::SWS_BILINEAR,
        None,
        None,
        None,
    )
    .expect("scaler");
    let (width, height) = (WIDTH as usize, HEIGHT as usize);
    let mut rgba = vec![255_u8; width * height * 4];
    for index in 0..FRAMES {
        for (pixel, value) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (pixel % width, pixel / width);
            let shift = index as usize * 8;
            value[0] = ((x + shift) % 256) as u8;
            value[1] = ((y * 2 + shift) % 256) as u8;
            value[2] = ((x ^ y) % 256) as u8;
        }
        let source = alpha2_ffmpeg_bridge::rgba_frame(&rgba, WIDTH, HEIGHT).expect("RGBA frame");
        let mut frame = AVFrame::new();
        frame.set_format(format);
        frame.set_width(WIDTH);
        frame.set_height(HEIGHT);
        frame.alloc_buffer().expect("frame buffer");
        scaler
            .scale_frame(&source, 0, HEIGHT, &mut frame)
            .expect("convert");
        frame.set_pts(index);
        encoder.send_frame(Some(&frame)).expect("send frame");
        drain(&mut encoder, &mut output);
    }
    encoder.send_frame(None).expect("flush");
    drain(&mut encoder, &mut output);
    output.write_trailer().expect("trailer");
}

fn drain(encoder: &mut AVCodecContext, output: &mut AVFormatContextOutput) {
    loop {
        match encoder.receive_packet() {
            Ok(mut packet) => {
                packet.set_stream_index(0);
                packet.rescale_ts(ra(1, 30), output.streams()[0].time_base);
                output.interleaved_write_frame(&mut packet).expect("write");
            }
            Err(RsmpegError::EncoderDrainError | RsmpegError::EncoderFlushedError) => break,
            Err(error) => panic!("FFV1 encode: {error}"),
        }
    }
}
