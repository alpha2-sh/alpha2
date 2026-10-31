//! Outputs with alpha or 10 bits, encoded from packed frames and decoded
//! back through `FFmpeg`.

use std::ffi::CString;

use alpha2_media::{BitDepth, Chroma, Container, PixelLayout, PlaneLayout};

use super::*;

fn layout(chroma: Chroma, bit_depth: BitDepth, alpha: bool) -> PixelLayout {
    PixelLayout {
        chroma,
        bit_depth,
        alpha,
        planes: PlaneLayout::Planar,
    }
}

fn config(
    codec: NativeVideoCodec,
    container: Container,
    pixels: PixelLayout,
    width: u32,
    height: u32,
) -> VideoEncodeConfig {
    VideoEncodeConfig {
        width,
        height,
        frame_rate: FrameRate::new(30, 1).expect("rate"),
        color: ColorMetadata::EXPORT,
        rate_control: VideoRateControl {
            bitrate: 20_000_000,
            mode: BitrateMode::Variable,
            quantizer: None,
        },
        codec,
        display: crate::VideoDisplay::default(),
        container,
        pixels,
    }
}

/// Writes code `value(plane, x, y)` into every sample of `frame`, a frame of
/// `pixels` (each sample a byte at 8 bits, a word at 10, shifted for `P010`).
fn fill(
    frame: &mut alpha2_ffmpeg_bridge::Yuv420pFrameBuffer,
    pixels: PixelLayout,
    width: u32,
    height: u32,
    value: impl Fn(usize, usize, usize) -> u16,
) {
    let shapes = pixels.planes(width, height);
    let mut planes = frame.begin_write_planes().expect("planes");
    for (plane, ((bytes, stride), (plane_width, rows, size))) in
        planes.iter_mut().zip(shapes).enumerate()
    {
        for y in 0..rows as usize {
            for x in 0..plane_width as usize {
                let code = value(plane, x, y);
                let at = y * *stride + x * size as usize;
                if size == 1 {
                    bytes[at] = u8::try_from(code).expect("an 8-bit code");
                } else {
                    let code = if pixels.planes == PlaneLayout::P010 {
                        code << 6
                    } else {
                        code
                    };
                    bytes[at..at + 2].copy_from_slice(&code.to_le_bytes());
                }
            }
        }
    }
}

/// Encodes `frames` frames of `value` into `path`; returns the encoder's
/// selection.
fn encode(
    path: &Path,
    config: VideoEncodeConfig,
    frames: u32,
    value: impl Fn(usize, usize, usize) -> u16 + Copy,
) -> crate::NativeVideoEncoderSelection {
    let _ = fs::remove_file(path);
    let mut sink = NativeVideoEncodeSink::create_with_encoder_threads(path, config, None, &|_| 2)
        .expect("a sink");
    let selection = sink.selection().clone();
    let mut frame =
        crate::packed_frame(config.width, config.height, selection.pixels).expect("frame");
    for index in 0..frames {
        fill(
            &mut frame,
            selection.pixels,
            config.width,
            config.height,
            value,
        );
        sink.write_prepared_video(&mut frame, MediaTime::from_ticks(i64::from(index) * 4_000))
            .expect("encoded frame");
    }
    sink.finish(&CancelToken::default()).expect("trailer");
    selection
}

/// The first frame of `path`'s video, decoded by `decoder` (or the codec's
/// default decoder), and its stream's profile.
fn first_frame(path: &Path, decoder: Option<&std::ffi::CStr>) -> (AVFrame, i32) {
    let name = CString::new(path.to_str().expect("UTF-8")).expect("no NUL");
    let mut input = rsmpeg::avformat::AVFormatContextInput::open(&name).expect("input");
    let (index, parameters) = input
        .streams()
        .iter()
        .find(|stream| stream.codecpar().codec_type == ffi::AVMEDIA_TYPE_VIDEO)
        .map(|stream| (stream.index, stream.codecpar().clone()))
        .expect("a video stream");
    let codec = decoder.map_or_else(
        || AVCodec::find_decoder(parameters.codec_id),
        AVCodec::find_decoder_by_name,
    );
    let codec = codec.expect("a decoder");
    let mut context = AVCodecContext::new(&codec);
    context.apply_codecpar(&parameters).expect("parameters");
    context.open(None).expect("decoder opens");
    while let Some(packet) = input.read_packet().expect("packet") {
        if packet.stream_index != index {
            continue;
        }
        context.send_packet(Some(&packet)).expect("send");
        if let Ok(frame) = context.receive_frame() {
            return (frame, parameters.profile);
        }
    }
    context.send_packet(None).expect("flush");
    (
        context.receive_frame().expect("a frame"),
        parameters.profile,
    )
}

/// Sample `(x, y)` of `plane` of a decoded `frame` whose samples are
/// `bytes` wide.
#[allow(unsafe_code)]
fn sample(frame: &AVFrame, plane: usize, x: usize, y: usize, bytes: usize) -> u16 {
    let stride = usize::try_from(frame.linesize[plane]).expect("positive stride");
    // SAFETY: `(x, y)` lies inside the plane of the decoded frame, which holds `height` rows of
    // `linesize` bytes.
    unsafe {
        let row = frame.data[plane].add(y * stride + x * bytes);
        if bytes == 1 {
            u16::from(*row)
        } else {
            u16::from_le_bytes([*row, *row.add(1)])
        }
    }
}

/// An opaque square in the middle of a transparent frame, mid-grey.
fn half_transparent(
    alpha_max: u16,
    depth_scale: u16,
) -> impl Fn(usize, usize, usize) -> u16 + Copy {
    move |plane, x, _| match plane {
        0 => 126 * depth_scale,
        1 | 2 => 128 * depth_scale,
        _ => {
            if x >= 32 {
                alpha_max
            } else {
                0
            }
        }
    }
}

#[test]
fn vp9_alpha_webm_round_trips_its_alpha() {
    crate::require_pinned_ffmpeg_for_tests();
    let pixels = layout(Chroma::Yuv420, BitDepth::Eight, true);
    let path = output_path("vp9-alpha").with_extension("webm");
    let selection = encode(
        &path,
        config(NativeVideoCodec::Vp9, Container::Webm, pixels, 64, 32),
        3,
        half_transparent(255, 1),
    );
    assert_eq!(selection.encoder_name, "libvpx-vp9");
    assert_eq!(selection.pixels, pixels);
    // FFmpeg's own VP9 decoder drops the alpha stream; libvpx's reads it from the Matroska
    // block additions.
    let (frame, _) = first_frame(&path, Some(c"libvpx-vp9"));
    assert_eq!(frame.format, ffi::AV_PIX_FMT_YUVA420P);
    for (x, expected) in [(4, 0), (60, 255)] {
        let alpha = sample(&frame, 3, x, 16, 1);
        assert!(alpha.abs_diff(expected) <= 1, "alpha at {x}: {alpha}");
    }
}

#[test]
fn prores_4444_keeps_alpha_and_422_hq_has_none() {
    crate::require_pinned_ffmpeg_for_tests();
    let alpha = layout(Chroma::Yuv444, BitDepth::Ten, true);
    let path = output_path("prores-4444").with_extension("mov");
    let selection = encode(
        &path,
        config(NativeVideoCodec::ProRes, Container::Mov, alpha, 64, 32),
        2,
        half_transparent(1023, 4),
    );
    assert_eq!(selection.encoder_name, "prores_ks");
    let (frame, profile) = first_frame(&path, None);
    assert_eq!(u32::try_from(profile), Ok(ffi::AV_PROFILE_PRORES_4444));
    // The decoder writes 4444 at 12 bits: four times the 10-bit codes.
    assert_eq!(frame.format, ffi::AV_PIX_FMT_YUVA444P12LE);
    assert_eq!(sample(&frame, 3, 4, 16, 2), 0);
    assert!(sample(&frame, 3, 60, 16, 2).abs_diff(4095) <= 4);
    assert!(sample(&frame, 0, 60, 16, 2).abs_diff(126 * 16) <= 16);

    let hq = layout(Chroma::Yuv422, BitDepth::Ten, false);
    let path = output_path("prores-hq").with_extension("mov");
    encode(
        &path,
        config(NativeVideoCodec::ProRes, Container::Mov, hq, 64, 32),
        2,
        half_transparent(0, 4),
    );
    let (frame, profile) = first_frame(&path, None);
    assert_eq!(u32::try_from(profile), Ok(ffi::AV_PROFILE_PRORES_HQ));
    assert_eq!(frame.format, ffi::AV_PIX_FMT_YUV422P10LE);
}

#[test]
fn ten_bit_outputs_keep_more_luma_codes_than_eight_bits_have() {
    crate::require_pinned_ffmpeg_for_tests();
    let pixels = layout(Chroma::Yuv420, BitDepth::Ten, false);
    // A luma ramp over every 10-bit limited-range code: 8 bits have 220 of them at most. 128 rows,
    // the fewest `av1_nvenc` takes.
    let ramp = |plane: usize, x: usize, _: usize| {
        if plane == 0 {
            u16::try_from(64 + x * 876 / 1023).expect("a code")
        } else {
            512
        }
    };
    // Each codec this runtime lists an encoder of 10-bit pictures for: HEVC only where NVENC is,
    // AV1 not where the valgrind run hides `libsvtav1`. A listed encoder must open.
    let capabilities = crate::codec_capabilities();
    let mut ran = Vec::new();
    for (codec, container, label) in [
        (NativeVideoCodec::Vp9, Container::Webm, "vp9-ten"),
        (NativeVideoCodec::Av1, Container::Mkv, "av1-ten"),
        (NativeVideoCodec::Hevc, Container::Mp4, "hevc-ten"),
    ] {
        if !crate::encodes_pixels(&capabilities, codec, pixels) {
            eprintln!("skipped {label}: this runtime lists no encoder of 10-bit pictures");
            continue;
        }
        let path = output_path(label).with_extension(container.extensions()[0]);
        let _ = fs::remove_file(&path);
        let mut sink = NativeVideoEncodeSink::create_with_encoder_threads(
            &path,
            config(codec, container, pixels, 1024, 128),
            None,
            &|_| 2,
        )
        .unwrap_or_else(|error| panic!("{label}: {error}"));
        let selection = sink.selection().clone();
        let mut frame = crate::packed_frame(1024, 128, selection.pixels).expect("frame");
        for index in 0..3 {
            fill(&mut frame, selection.pixels, 1024, 128, ramp);
            sink.write_prepared_video(&mut frame, MediaTime::from_ticks(index * 4_000))
                .expect("encoded frame");
        }
        sink.finish(&CancelToken::default()).expect("trailer");
        let (frame, _) = first_frame(&path, None);
        assert_eq!(frame.format, ffi::AV_PIX_FMT_YUV420P10LE, "{label}");
        let codes: std::collections::BTreeSet<u16> =
            (0..1024).map(|x| sample(&frame, 0, x, 32, 2)).collect();
        assert!(
            codes.len() > 600,
            "{label} keeps {} luma codes",
            codes.len()
        );
        ran.push((
            label,
            selection.encoder_name,
            selection.pixels.ffmpeg_name(),
        ));
    }
    // `libvpx-vp9` is in every pinned runtime.
    assert!(ran.iter().any(|(label, ..)| *label == "vp9-ten"), "{ran:?}");
}

#[test]
fn an_encoder_without_the_pixel_format_is_passed_over() {
    crate::require_pinned_ffmpeg_for_tests();
    // `mpeg4` takes 8-bit 4:2:0 only, so a 10-bit request (one the policy would refuse first) is
    // refused for its pixel format, before anything opens.
    let pixels = layout(Chroma::Yuv420, BitDepth::Ten, false);
    let path = output_path("mpeg4-ten").with_extension("mp4");
    let error = NativeVideoEncodeSink::create_with_encoder_threads(
        &path,
        config(
            NativeVideoCodec::Mpeg4Part2,
            Container::Mp4,
            pixels,
            256,
            128,
        ),
        None,
        &|_| 2,
    )
    .err()
    .expect("refused");
    assert!(error.to_string().contains("pixel-format"), "{error}");
}
