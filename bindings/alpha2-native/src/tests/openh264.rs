//! `OpenH264` through the native sinks. These run where Cisco's binary is
//! installed or named (`ALPHA2_OPENH264_LIBRARY`), as CI's `OpenH264` job does,
//! and are recorded as the `openh264` capability where it is not.

use super::*;

/// Whether this process loaded the library; a recorded skip when not.
fn openh264_available() -> bool {
    match crate::openh264_status() {
        crate::OpenH264Status::Available(_) => true,
        status => {
            alpha2_test_support::skip!(
                alpha2_test_support::Capability::Openh264,
                "OpenH264 is {}: the OpenH264 cases did not run ({})",
                status.as_str(),
                status.reason().unwrap_or_default()
            );
            false
        }
    }
}

fn h264_config(
    width: u32,
    height: u32,
    container: alpha2_media::Container,
    rate: VideoRateControl,
) -> VideoEncodeConfig {
    VideoEncodeConfig {
        width,
        height,
        frame_rate: FrameRate::new(30, 1).expect("rate"),
        color: ColorMetadata::EXPORT,
        rate_control: rate,
        codec: NativeVideoCodec::H264,
        display: crate::VideoDisplay::default(),
        container,
        pixels: alpha2_media::PixelLayout::YUV420P,
    }
}

fn quantizer(qp: u32) -> VideoRateControl {
    VideoRateControl {
        bitrate: 4_000_000,
        mode: BitrateMode::Variable,
        quantizer: Some(qp),
    }
}

/// Frame `index` of a moving pattern: a luma ramp that shifts each frame,
/// over flat chroma that changes slowly.
fn pattern(frame: &mut Yuv420pFrameBuffer, index: u32) {
    let (width, height) = frame.size();
    let planes = frame.begin_write().expect("writable frame");
    for row in 0..height {
        for column in 0..width {
            let value = (column + row / 2 + index as usize * 3) % 220 + 16;
            planes.y[row * planes.y_stride + column] = u8::try_from(value).unwrap_or(16);
        }
    }
    for row in 0..height / 2 {
        for column in 0..width / 2 {
            planes.u[row * planes.chroma_stride + column] =
                u8::try_from(100 + (index % 50)).unwrap_or(128);
            planes.v[row * planes.chroma_stride + column] =
                u8::try_from(150 - (column * 40 / (width / 2).max(1))).unwrap_or(128);
        }
    }
}

/// The luma PSNR of `decoded` against frame `index` of [`pattern`].
#[allow(clippy::cast_precision_loss)]
fn luma_psnr(decoded: &DecodedVideoFrame, index: u32, width: usize, height: usize) -> f64 {
    let plane = &decoded.planes[0];
    let stride = plane.stride as usize;
    let mut error = 0.0_f64;
    for row in 0..height {
        for column in 0..width {
            let expected = (column + row / 2 + index as usize * 3) % 220 + 16;
            let difference = f64::from(plane.bytes[row * stride + column]) - expected as f64;
            error += difference * difference;
        }
    }
    let mse = error / (width * height) as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

/// Encodes `frames` frames of [`pattern`] with `encoder` into `path`.
fn encode_pattern(
    path: &Path,
    config: VideoEncodeConfig,
    encoder: Option<&str>,
    frames: u32,
) -> crate::NativeVideoEncoderSelection {
    let _ = fs::remove_file(path);
    let mut sink = NativeVideoEncodeSink::create_with_encoder_threads(path, config, encoder, &|| 2)
        .expect("an OpenH264 sink");
    let width = i32::try_from(config.width).unwrap();
    let height = i32::try_from(config.height).unwrap();
    let mut frame = Yuv420pFrameBuffer::new(width, height).expect("frame");
    for index in 0..frames {
        pattern(&mut frame, index);
        sink.write_prepared_video(&mut frame, MediaTime::from_ticks(i64::from(index) * 4_000))
            .expect("encoded frame");
    }
    let selection = sink.selection().clone();
    sink.finish(&CancelToken::default()).expect("trailer");
    selection
}

#[test]
fn openh264_writes_h264_every_container_decodes_frame_for_frame() {
    if !openh264_available() {
        return;
    }
    let (width, height, frames) = (640_usize, 360_usize, 70_u32);
    for container in [
        alpha2_media::Container::Mp4,
        alpha2_media::Container::Mov,
        alpha2_media::Container::Mkv,
    ] {
        let path =
            output_path(&format!("openh264-{container}")).with_extension(container.extensions()[0]);
        let selection = encode_pattern(
            &path,
            h264_config(640, 360, container, quantizer(23)),
            Some(crate::OPENH264_ENCODER_NAME),
            frames,
        );
        assert_eq!(selection.encoder_name, crate::OPENH264_ENCODER_NAME);
        assert_eq!(selection.class, NativeEncoderClass::Software);
        assert!(!selection.hardware_accelerated);
        assert_eq!(selection.rate_control, crate::AppliedRateControl::Quantizer);
        assert_eq!(selection.quantizer_refused, None);
        assert_eq!(selection.software_threads, Some(1), "one slice at 360 rows");

        let probed = probe(&path).expect("probe");
        let video = probed
            .streams
            .iter()
            .find(|stream| stream.kind == StreamKind::Video)
            .expect("a video stream");
        assert_eq!(video.codec, "h264", "{container}");
        assert_eq!(video.dimensions, Some((640, 360)));
        assert_eq!(video.color, Some(ColorMetadata::EXPORT), "{container}");
        let mut decoder = NativeDecoder::open(&path).expect("decoder");
        let cancel = CancelToken::default();
        let mut count = 0_u32;
        loop {
            match decoder.decode_next(&cancel) {
                Ok(DecodeEvent::Video(frame)) => {
                    // Matroska keeps times in milliseconds.
                    let expected = i64::from(count) * 4_000;
                    assert!(
                        (frame.pts.ticks() - expected).abs() <= 120,
                        "{container} frame {count} at {:?}",
                        frame.pts
                    );
                    let psnr = luma_psnr(&frame, count, width, height);
                    assert!(psnr > 38.0, "{container} frame {count}: {psnr:.2} dB");
                    count += 1;
                }
                Ok(DecodeEvent::EndOfStream) => break,
                Ok(_) => {}
                Err(error) => panic!("{container}: {error}"),
            }
        }
        assert_eq!(count, frames, "{container}");
        drop(decoder);
        fs::remove_file(path).expect("remove fixture");
    }
}

#[test]
fn openh264_output_is_the_same_bytes_every_run() {
    if !openh264_available() {
        return;
    }
    // 1080 rows: four slices on four threads, where load balancing would make runs differ.
    let config = h264_config(1920, 1080, alpha2_media::Container::Mp4, quantizer(27));
    let first = output_path("openh264-determinism-a");
    let second = output_path("openh264-determinism-b");
    let selection = encode_pattern(&first, config, Some(crate::OPENH264_ENCODER_NAME), 20);
    assert_eq!(selection.software_threads, Some(4));
    encode_pattern(&second, config, Some(crate::OPENH264_ENCODER_NAME), 20);
    let (a, b) = (fs::read(&first).unwrap(), fs::read(&second).unwrap());
    assert!(a == b, "two runs of one export differ");
    fs::remove_file(first).unwrap();
    fs::remove_file(second).unwrap();
}

#[test]
fn openh264_holds_a_bitrate_without_dropping_frames() {
    if !openh264_available() {
        return;
    }
    let path = output_path("openh264-bitrate");
    let selection = encode_pattern(
        &path,
        h264_config(
            640,
            360,
            alpha2_media::Container::Mp4,
            VideoRateControl::bitrate(500_000, BitrateMode::Constant).unwrap(),
        ),
        Some(crate::OPENH264_ENCODER_NAME),
        60,
    );
    assert_eq!(selection.rate_control, crate::AppliedRateControl::Bitrate);
    let mut decoder = NativeDecoder::open(&path).expect("decoder");
    let cancel = CancelToken::default();
    let mut count = 0;
    while let Ok(event) = decoder.decode_next(&cancel) {
        match event {
            DecodeEvent::Video(_) => count += 1,
            DecodeEvent::EndOfStream => break,
            _ => {}
        }
    }
    assert_eq!(count, 60, "every frame is in the file");
    let bytes = fs::metadata(&path).unwrap().len();
    // Two seconds at 500 kb/s, with room for the container and the rate control's slack.
    assert!(bytes < 200_000, "{bytes} bytes for two seconds at 500 kb/s");
    drop(decoder);
    fs::remove_file(path).unwrap();
}

#[test]
fn openh264_is_listed_after_ffmpeg_s_encoders_with_its_library() {
    if !openh264_available() {
        return;
    }
    let listed = crate::video_encoder_capabilities(NativeVideoCodec::H264);
    let last = listed.last().expect("an H.264 encoder");
    assert_eq!(last.encoder_name, crate::OPENH264_ENCODER_NAME);
    assert_eq!(last.class, NativeEncoderClass::Software);
    let library = last.library.as_ref().expect("its library");
    assert_eq!(library.notice, crate::OPENH264_NOTICE);
    assert!(!library.version.is_empty() && !library.path.is_empty());
    assert!(
        listed[..listed.len() - 1]
            .iter()
            .all(|capability| capability.library.is_none())
    );
}

#[test]
fn openh264_encodes_h264_only() {
    let error = NativeVideoEncodeSink::create_with_encoder(
        output_path("openh264-hevc"),
        VideoEncodeConfig {
            codec: NativeVideoCodec::Hevc,
            ..h264_config(640, 360, alpha2_media::Container::Mp4, quantizer(27))
        },
        crate::OPENH264_ENCODER_NAME,
    )
    .err()
    .expect("refused");
    assert_eq!(
        error.to_string(),
        EncodeError::Adapter("video encoder openh264 does not implement hevc".into()).to_string()
    );
}
