//! What a decoded frame carries: orientation, pixel aspect, color, siting, crop, and planes.

use super::*;

#[test]
fn a_frame_larger_than_alpha2_renders_is_refused_before_it_is_converted() {
    // A grey 1-bit PNG is converted to RGBA (two frame-sized buffers); 8193 x 8193 is 16,785 pixels
    // past MAX_FRAME_PIXELS, from a file of a few kilobytes.
    let path = output_path("large-png").with_extension("png");
    {
        let file = fs::File::create(&path).expect("create");
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), 8_193, 8_193);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::One);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header().expect("header");
        writer
            .write_image_data(&vec![0; 8_193_usize.div_ceil(8) * 8_193])
            .expect("rows");
    }
    let mut decoder = NativeDecoder::open(&path).expect("open");
    let first = decoder.decode_next(&CancelToken::default());
    // Windows cannot remove a file the decoder still has open.
    drop(decoder);
    fs::remove_file(&path).expect("remove");
    assert!(
        matches!(first, Err(DecodeError::Unsupported(_))),
        "{first:?}"
    );
}

#[test]
fn display_matrix_angles_become_clockwise_rotations() {
    for (counter_clockwise, expected) in [
        (0.0, Some(Rotation::Deg0)),
        (90.0, Some(Rotation::Deg270)),
        (-90.0, Some(Rotation::Deg90)),
        (180.0, Some(Rotation::Deg180)),
        (-180.0, Some(Rotation::Deg180)),
        (89.9, Some(Rotation::Deg270)),
        (45.0, None),
    ] {
        assert_eq!(
            rotation_from_display(counter_clockwise),
            expected,
            "{counter_clockwise} deg"
        );
    }
}

/// A JPEG's Exif `Orientation` reaches the probe, the stream, and every
/// decoded frame: the eight corpus stills are one picture stored eight ways,
/// each tagged to show it upright.
#[test]
fn a_still_is_shown_as_its_exif_orientation_says() {
    for value in 1..=8_u16 {
        let name = format!("exif-orientation-{value}.jpg");
        let expected = Orientation::from_exif(value).expect("Exif orientation");
        let probed = probe(corpus(&name)).unwrap_or_else(|error| panic!("{name}: {error:?}"));
        let stream = probed.selected_video_stream().expect("video");
        assert_eq!(stream.orientation, expected, "{name}");
        let stored = if expected.rotation.swaps_axes() {
            (48, 64)
        } else {
            (64, 48)
        };
        assert_eq!(stream.dimensions, Some(stored), "{name}");
        let mut decoder = NativeDecoder::open(corpus(&name)).expect("open still");
        let frame = loop {
            match decoder
                .decode_next(&CancelToken::default())
                .expect("decode still")
            {
                DecodeEvent::Video(frame) => break frame,
                DecodeEvent::EndOfStream => panic!("{name}: no frame"),
                DecodeEvent::Retry | DecodeEvent::Audio(_) => {}
            }
        };
        assert_eq!(frame.orientation, expected, "{name}");
    }
}

/// A display matrix that mirrors is shown mirrored, not only turned.
#[test]
fn a_mirroring_display_matrix_is_a_mirror_and_a_rotation() {
    for (name, rotation) in [
        ("h264-bt709-hflip.mp4", Rotation::Deg0),
        ("h264-bt709-vflip.mp4", Rotation::Deg180),
        ("h264-bt709-transverse.mp4", Rotation::Deg90),
        // Matroska's projection pose: a roll, and a yaw of 180 degrees for the mirror.
        ("h264-bt709-transverse.mkv", Rotation::Deg90),
    ] {
        let probed = probe(corpus(name)).unwrap_or_else(|error| panic!("{name}: {error:?}"));
        assert_eq!(
            probed.selected_video_stream().expect("video").orientation,
            Orientation {
                rotation,
                mirror: true
            },
            "{name}"
        );
    }
}

#[test]
fn a_pixel_aspect_ratio_is_the_one_ffmpeg_guesses() {
    let cancel = CancelToken::default();
    let mut wrong = Vec::new();
    for (name, probed, early, late) in pixel_aspect_fixtures() {
        let probe = probe(corpus(name)).unwrap_or_else(|error| panic!("{name}: {error:?}"));
        let stream = probe.selected_video_stream().expect("video");
        if stream.sample_aspect_ratio != probed {
            wrong.push(format!(
                "{name}: probed {:?}, expected {probed:?}",
                stream.sample_aspect_ratio
            ));
        }
        let mut decoder = NativeExportDecoder::open_with_threads(corpus(name), 1).expect(name);
        let mut index = 0;
        let mut wrong_run = None;
        while let Some(frame) = decoder.decode_video_next(&cancel).expect(name) {
            let expected = if index < 30 { early } else { late };
            let found = frame.metadata().sample_aspect_ratio;
            if found != expected && (index == 0 || index == 30 || wrong_run != Some(index - 1)) {
                wrong.push(format!(
                    "{name} from frame {index}: {found:?}, expected {expected:?}"
                ));
            }
            wrong_run = (found != expected).then_some(index);
            index += 1;
        }
        assert_eq!(index, 60, "{name}");
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// A frame whose layout the renderer does not sample (alpha, here) still
/// reaches it with what the renderer converts: its own components at their
/// own depth, its PQ BT.2020 color, and its alpha, as 4:4:4 planes, never as
/// RGBA that would be shown as SDR.
#[test]
fn a_frame_the_renderer_cannot_sample_keeps_its_components_color_and_alpha() {
    let mut decoder = NativeDecoder::open(corpus("prores-pq-alpha.mov")).expect("alpha");
    let frame = first_frame(&mut decoder, "prores-pq-alpha.mov");
    assert_eq!(
        frame.pixel_format,
        PixelFormat::Yuv(YuvFormat::planar(0, 0, 12).with_alpha()),
        "luma, chroma, and alpha as 12-bit 4:4:4 planes, the decoder's own depth"
    );
    assert_eq!(frame.planes.len(), 4);
    assert_eq!(frame.color, Some(PQ_BT2020));
    // The same picture without alpha is a layout the renderer samples: its codes are the alpha
    // frame's.
    let mut opaque = NativeDecoder::open(corpus("prores-pq-opaque.mov")).expect("opaque");
    let reference = first_frame(&mut opaque, "prores-pq-opaque.mov");
    assert_eq!(
        reference.pixel_format,
        PixelFormat::Yuv(YuvFormat::planar(0, 0, 12))
    );
    for (index, (plane, twin)) in frame.planes.iter().zip(&reference.planes).enumerate() {
        for row in [0, 20, 40] {
            for (x, (code, twin)) in row16(plane, row)
                .into_iter()
                .zip(row16(twin, row))
                .enumerate()
            {
                assert!(
                    code.abs_diff(twin) <= 1,
                    "plane {index} ({x}, {row}): {code} against {twin}"
                );
            }
        }
    }
    let alpha = &frame.planes[3];
    assert!(
        row16(alpha, 0).iter().all(|&code| code == 4_095),
        "opaque above the blocks"
    );
    let half = row16(alpha, 40);
    assert!(
        half[..32].iter().all(|&code| code.abs_diff(2_048) < 8)
            && half[32..].iter().all(|&code| code == 4_095),
        "half alpha over the first two blocks: {half:?}"
    );
}

/// Red, green, and blue a frame stores come out in that order, unchanged,
/// under the identity matrix, so a declared wide gamut is still converted.
#[test]
fn an_rgb_frame_the_renderer_converts_keeps_red_green_and_blue() {
    let name = "parity-rgb.mov";
    let mut plain = NativeDecoder::open(corpus(name)).expect(name);
    let rgba = first_frame(&mut plain, name);
    assert_eq!(
        rgba.pixel_format,
        PixelFormat::Rgba8,
        "BT.709 SDR stays RGBA"
    );
    let mut declared = NativeDecoder::open(corpus(name)).expect(name);
    let wide = ColorMetadata {
        primaries: alpha2_media::ColorPrimaries::Bt2020,
        ..ColorMetadata::EXPORT
    };
    declared.declare_color(Some(wide));
    let frame = first_frame(&mut declared, name);
    assert_eq!(
        frame.pixel_format,
        PixelFormat::Yuv(YuvFormat::planar(0, 0, 8))
    );
    assert_eq!(
        frame.color,
        Some(ColorMetadata {
            matrix: ColorMatrix::Rgb,
            range: ColorRange::Full,
            ..wide
        })
    );
    let pixels = &rgba.planes[0];
    for (channel, plane) in frame.planes.iter().enumerate() {
        let samples = pixels
            .bytes
            .chunks(4)
            .map(|pixel| pixel[channel])
            .collect::<Vec<_>>();
        assert_eq!(plane.bytes, samples, "channel {channel}");
    }
}

/// An RGB frame in another gamut than BT.709 reaches an encode sink as
/// planes of red, green, and blue under the identity matrix. Fed to the
/// 4:2:0 converter as luma and chroma, a red picture would come out green
/// with no error; the sink converts them as it converts RGBA: into the
/// output's matrix and range, the same codes the RGBA frame of the same
/// picture gives, its gamut left as it is.
#[test]
#[allow(clippy::cast_precision_loss)]
fn an_encode_sink_converts_red_green_and_blue_planes_into_its_matrix() {
    let name = "parity-rgb.mov";
    let encoded = |declared: Option<ColorMetadata>, label: &str| {
        let mut decoder = NativeDecoder::open(corpus(name)).expect(name);
        decoder.declare_color(declared);
        let frame = first_frame(&mut decoder, name);
        let path = output_path(label);
        let _ = fs::remove_file(&path);
        let mut sink = NativeVideoEncodeSink::create(
            &path,
            VideoEncodeConfig {
                width: 96,
                height: 64,
                frame_rate: FrameRate::new(12, 1).expect("rate"),
                color: ColorMetadata {
                    primaries: frame.color.unwrap_or_default().primaries,
                    ..ColorMetadata::EXPORT
                },
                rate_control: VideoRateControl::bitrate(4_000_000, BitrateMode::Variable).unwrap(),
                codec: NativeVideoCodec::Mpeg4Part2,
                display: crate::VideoDisplay::default(),
                container: alpha2_media::Container::Mp4,
                pixels: alpha2_media::PixelLayout::YUV420P,
            },
        )
        .expect("encoder");
        let cancel = CancelToken::default();
        let format = frame.pixel_format;
        sink.write(EncodeEvent::Video(frame), &cancel)
            .expect("frame");
        sink.finish(&cancel).expect("trailer");
        let mut output = NativeDecoder::open(&path).expect("output");
        let written = first_frame(&mut output, label);
        drop(output);
        fs::remove_file(path).expect("remove output");
        (format, written)
    };
    let (rgba, reference) = encoded(None, "rgb-bt709");
    assert_eq!(rgba, PixelFormat::Rgba8, "BT.709 SDR arrives as RGBA");
    let (planar, wide) = encoded(
        Some(ColorMetadata {
            primaries: alpha2_media::ColorPrimaries::Bt2020,
            ..ColorMetadata::EXPORT
        }),
        "rgb-bt2020",
    );
    assert_eq!(planar, PixelFormat::Yuv(YuvFormat::planar(0, 0, 8)));
    assert_eq!(
        wide.color
            .map(|color| (color.primaries, color.matrix, color.range)),
        Some((
            alpha2_media::ColorPrimaries::Bt2020,
            ColorMatrix::Bt709,
            ColorRange::Limited
        ))
    );
    for (index, (plane, twin)) in wide.planes.iter().zip(&reference.planes).enumerate() {
        let (sum, count) = plane
            .bytes
            .iter()
            .zip(twin.bytes.iter())
            .fold((0_u64, 0_u64), |(sum, count), (code, twin)| {
                (sum + u64::from(code.abs_diff(*twin)), count + 1)
            });
        // Two swscale paths and two encodes; Y'CbCr read as RGB differs by tens.
        assert!(
            sum <= count * 2,
            "plane {index}: mean code difference {:.1}",
            sum as f64 / count as f64
        );
    }
}

/// A frame nothing declares a color for reaches the renderer without one,
/// in software as in a hardware session, so the renderer's fallback to
/// [`ColorMetadata::UNDECLARED`] is the one that applies and is reported
/// (`MissingMetadataFallback`). A tagged or declared frame carries its color.
#[test]
fn a_frame_without_a_declared_color_carries_none() {
    let name = "missing-color-metadata.mp4";
    let mut untagged = NativeDecoder::open(corpus(name)).expect(name);
    assert_eq!(first_frame(&mut untagged, name).color, None, "{name}");
    let mut declared = NativeDecoder::open(corpus(name)).expect(name);
    declared.declare_color(Some(ColorMetadata::EXPORT));
    assert_eq!(
        first_frame(&mut declared, name).color,
        Some(ColorMetadata::EXPORT),
        "declared"
    );
    let name = "cfr-bt709-limited.mp4";
    let mut tagged = NativeDecoder::open(corpus(name)).expect(name);
    assert_eq!(
        first_frame(&mut tagged, name).color,
        Some(ColorMetadata::EXPORT),
        "{name}"
    );
}

/// A color the project declares wins over what the frames and the stream
/// are tagged with, in software and in a hardware session's metadata, and a
/// hardware session refuses a declared HDR source as it does a tagged one.
#[test]
fn a_declared_color_wins_over_every_tag() {
    let name = "hdr-pq-untagged.mp4";
    let mut untagged = NativeDecoder::open(corpus(name)).expect(name);
    let tagged = first_frame(&mut untagged, name).color.expect("a range tag");
    assert_eq!(
        tagged.transfer,
        alpha2_media::TransferFunction::Bt709,
        "the stream says SDR"
    );
    let mut declared = NativeDecoder::open(corpus(name)).expect(name);
    declared.declare_color(Some(PQ_BT2020));
    let frame = first_frame(&mut declared, name);
    assert_eq!(frame.color, Some(PQ_BT2020));
    assert!(
        matches!(frame.pixel_format, PixelFormat::Yuv(_)),
        "planes as the renderer samples them"
    );

    let cancel = CancelToken::default();
    let mut export = NativeExportDecoder::open(corpus(name))
        .expect(name)
        .with_declared_color(Some(PQ_BT2020))
        .expect("a software session takes any declaration");
    let frame = export
        .decode_video_next(&cancel)
        .expect(name)
        .expect("frame");
    assert_eq!(frame.metadata().color, Some(PQ_BT2020));
    // A declaration of none leaves the tags in charge.
    let mut undeclared = NativeExportDecoder::open(corpus("hdr-pq.mp4"))
        .expect("hdr-pq.mp4")
        .with_declared_color(None)
        .expect("software");
    let frame = undeclared
        .decode_video_next(&cancel)
        .expect("hdr-pq.mp4")
        .expect("frame");
    assert_eq!(frame.metadata().color, Some(PQ_BT2020));
}

/// Where each stream's chroma samples sit, as the decoder reports it on its
/// frames: the value the browser reads from the same stream's parameter sets.
#[test]
fn each_frame_carries_the_chroma_siting_its_stream_signals() {
    let cancel = CancelToken::default();
    for (name, siting) in [
        ("hdr-pq.mp4", ChromaLocation::Left),
        // H.264 without VUI infers chroma sample location type 0 (E.2.1), which FFmpeg leaves
        // unspecified.
        ("h264-bt709-no-vui.mp4", ChromaLocation::Left),
        ("hdr-pq-topleft.mp4", ChromaLocation::TopLeft),
        ("hevc-topleft.mp4", ChromaLocation::TopLeft),
        ("av1-colocated.mp4", ChromaLocation::TopLeft),
        ("av1.mp4", ChromaLocation::Unspecified),
    ] {
        let mut decoder = NativeExportDecoder::open(corpus(name)).expect(name);
        let frame = decoder
            .decode_video_next(&cancel)
            .expect(name)
            .expect("frame");
        assert_eq!(frame.metadata().chroma_location, siting, "{name}");
    }
}

/// A software decode shows the picture the SPS crops to, however few
/// columns it crops on the left: `FFmpeg` otherwise rounds a left crop down
/// to keep plane pointers aligned, and below 64 columns drops it whole.
#[test]
fn a_software_decode_shows_exactly_the_columns_its_stream_crops_to() {
    let mut cropped = NativeDecoder::open(corpus("h264-bt709-crop-left.mp4")).expect("cropped");
    let mut whole = NativeDecoder::open(corpus("h264-bt709-copy.mp4")).expect("whole");
    let probed = probe(corpus("h264-bt709-crop-left.mp4")).expect("probe");
    assert_eq!(probed.streams[0].dimensions, Some((120, 96)));
    for index in 0..3 {
        let shown = first_frame(&mut cropped, "h264-bt709-crop-left.mp4");
        let coded = first_frame(&mut whole, "h264-bt709-copy.mp4");
        let rect = shown.visible_rect;
        assert_eq!(
            (rect.width, rect.height),
            (120, 96),
            "frame {index}: {rect:?}"
        );
        assert_eq!(shown.chroma_location, coded.chroma_location);
        for (plane, step) in [(0, 1), (1, 2), (2, 2)] {
            let (shown_plane, coded_plane) = (&shown.planes[plane], &coded.planes[plane]);
            for row in 0..coded_plane.height {
                let at = |source: &Plane, x: u32| {
                    source.bytes[usize::try_from(row * source.stride + x).expect("offset")]
                };
                for column in 0..120 / step {
                    assert_eq!(
                        at(shown_plane, rect.x / step + column),
                        at(coded_plane, 8 / step + column),
                        "frame {index}, plane {plane}, row {row}, column {column}"
                    );
                }
            }
        }
    }
}

/// Only a location `FFmpeg` leaves unspecified is inferred: H.264 types 3-5
/// (top, bottom-left, bottom), which the renderer samples centred, stay so.
#[test]
fn only_an_h264_location_ffmpeg_leaves_unspecified_is_inferred_left() {
    use crate::convert::decoded_chroma_location;
    let h264 = ffi::AV_CODEC_ID_H264;
    assert_eq!(
        decoded_chroma_location(h264, ffi::AVCHROMA_LOC_UNSPECIFIED),
        ChromaLocation::Left
    );
    assert_eq!(
        decoded_chroma_location(h264, ffi::AVCHROMA_LOC_CENTER),
        ChromaLocation::Center
    );
    for signalled in [
        ffi::AVCHROMA_LOC_TOP,
        ffi::AVCHROMA_LOC_BOTTOMLEFT,
        ffi::AVCHROMA_LOC_BOTTOM,
    ] {
        assert_eq!(
            decoded_chroma_location(h264, signalled),
            ChromaLocation::Unspecified
        );
    }
    for other in [
        ffi::AV_CODEC_ID_HEVC,
        ffi::AV_CODEC_ID_VP9,
        ffi::AV_CODEC_ID_AV1,
    ] {
        assert_eq!(
            decoded_chroma_location(other, ffi::AVCHROMA_LOC_UNSPECIFIED),
            ChromaLocation::Unspecified
        );
    }
}

/// A converting scaler is made once for a stream's frames: `FFmpeg` rewrites
/// the flags and source format it caches by while it initialises, so asking
/// `sws_getCachedContext` with the original ones would remake it every frame
/// for RGB, alpha, palette, and odd-width input.
#[test]
fn a_stream_converts_every_frame_with_the_scaler_its_first_made() {
    for name in ["parity-rgb.mov", "prores-pq-alpha.mov"] {
        let mut decoder = NativeDecoder::open(corpus(name)).expect(name);
        for _ in 0..3 {
            first_frame(&mut decoder, name);
        }
        assert_eq!(decoder.video_scaler.made(), 1, "{name}");
    }
}

/// A mirroring display matrix is its rotation followed by a horizontal flip,
/// which is the mirror before the opposite rotation: `FFmpeg` writes Exif
/// `Orientation` 5 as 90 degrees clockwise then a flip, read back as a
/// counter-clockwise angle of -90 degrees.
#[test]
fn a_mirroring_display_matrix_mirrors_before_the_opposite_turn() {
    for (counter_clockwise, flipped, expected) in [
        (0.0, false, Orientation::rotated(Rotation::Deg0)),
        (90.0, false, Orientation::rotated(Rotation::Deg270)),
        (0.0, true, Orientation::from_exif(2).expect("2")),
        (180.0, true, Orientation::from_exif(4).expect("4")),
        (-90.0, true, Orientation::from_exif(5).expect("5")),
        (90.0, true, Orientation::from_exif(7).expect("7")),
    ] {
        assert_eq!(
            orientation_from_display(DisplayRotation {
                counter_clockwise_degrees: counter_clockwise,
                flipped,
            }),
            Some(expected),
            "{counter_clockwise} deg flipped {flipped}"
        );
    }
    assert_eq!(
        orientation_from_display(DisplayRotation {
            counter_clockwise_degrees: 45.0,
            flipped: true,
        }),
        None
    );
}

/// A CPU frame's metadata says its own layout and depth: 8-bit straight
/// RGBA for a decoder's frame, and linear half-float for an extension
/// provider's, not 8-bit RGBA.
#[test]
fn a_cpu_frames_metadata_says_its_own_layout_and_depth() {
    let rgba = rgba_frame(MediaTime::ZERO, [10, 20, 30, 128]);
    let metadata = crate::NativeVideoFrame::Cpu(rgba.clone()).metadata();
    assert_eq!(
        (metadata.plane_layout, metadata.bit_depth),
        (crate::NativePlaneLayout::CpuRgba8, 8)
    );
    let linear = DecodedVideoFrame {
        pixel_format: PixelFormat::LinearRgbaF16,
        ..rgba
    };
    let metadata = crate::NativeVideoFrame::Cpu(linear).metadata();
    assert_eq!(
        (metadata.plane_layout, metadata.bit_depth),
        (crate::NativePlaneLayout::CpuLinearRgbaF16, 16)
    );
}

/// Bytes of plane `index` of `frame` past each row's samples, up to its
/// stride: what a decoder may never have written.
fn row_padding(frame: &DecodedVideoFrame, index: usize) -> Vec<u8> {
    let PixelFormat::Yuv(format) = frame.pixel_format else {
        panic!("a YCbCr frame, not {:?}", frame.pixel_format);
    };
    let plane = &frame.planes[index];
    let sample = if format.bit_depth > 8 { 2 } else { 1 };
    let pair = if format.interleaved_chroma && index == 1 {
        2
    } else {
        1
    };
    let row = usize::try_from(plane.width * sample * pair).expect("row");
    let stride = usize::try_from(plane.stride).expect("stride");
    assert_eq!(plane.bytes.len(), stride * plane.height as usize);
    plane
        .bytes
        .chunks(stride)
        .flat_map(|bytes| bytes[row..].iter().copied())
        .collect()
}

/// The child half of `decoded_planes_hold_no_byte_the_decoder_never_wrote`,
/// run with `MALLOC_PERTURB_` so memory nothing wrote holds a pattern rather
/// than whatever zeroed pages the allocator happened to reuse.
#[test]
#[ignore = "run by decoded_planes_hold_no_byte_the_decoder_never_wrote in a child process with MALLOC_PERTURB_ set"]
fn decode_planes_under_malloc_perturb() {
    let name = std::env::var("ALPHA2_PADDING_INPUT").expect("run by its parent");
    let shared = std::env::var("ALPHA2_PADDING_SHARED").expect("run by its parent") == "1";
    let mut decoder = NativeDecoder::open(corpus(&name)).expect("a software session opens");
    let (mut frames, mut padding) = (0, 0);
    loop {
        match decoder.decode_next(&CancelToken::default()) {
            Ok(DecodeEvent::Video(frame)) => {
                frames += 1;
                for index in 0..frame.planes.len() {
                    let bytes = row_padding(&frame, index);
                    padding += bytes.len();
                    let unwritten = bytes.iter().filter(|byte| **byte != 0).count();
                    assert_eq!(
                        unwritten,
                        0,
                        "{name} frame {frames} plane {index}: {unwritten} of {} padding bytes are not zero",
                        bytes.len()
                    );
                }
            }
            Ok(DecodeEvent::EndOfStream) => break,
            Ok(_) => {}
            Err(error) => panic!("{name}: {error}"),
        }
    }
    assert!(frames > 0, "{name} decoded no frame");
    // A decoder whose buffers are written throughout hands its planes on as views, padded rows
    // and all; any other decoder's planes are copies of their samples alone.
    assert_eq!(padding > 0, shared, "{name}: {padding} padding bytes");
    println!("{name}: {frames} frames, {padding} padding bytes, all zero");
}

/// libdav1d decodes into pictures from a pool `FFmpeg` does not zero, and
/// writes only the picture, so the rest of each row holds whatever the
/// allocator left there. Every byte of a decoded plane past a row's samples
/// is zero, whichever decoder made it.
#[cfg(target_os = "linux")]
#[test]
fn decoded_planes_hold_no_byte_the_decoder_never_wrote() {
    // libdav1d's pictures are copied; FFmpeg's own H.264 decoder's are shared.
    for (name, shared) in [
        ("av1.mp4", "0"),
        ("av1-10bit-padded.mp4", "0"),
        ("h264-padded.mp4", "1"),
    ] {
        let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tests::frame_metadata::decode_planes_under_malloc_perturb",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("ALPHA2_PADDING_INPUT", name)
            .env("ALPHA2_PADDING_SHARED", shared)
            // glibc fills each allocation with the complement of this byte (0xa5).
            .env("MALLOC_PERTURB_", "90")
            .output()
            .expect("child");
        let stdout = String::from_utf8_lossy(&child.stdout);
        let stderr = String::from_utf8_lossy(&child.stderr);
        assert!(
            child.status.success() && stdout.contains("1 passed"),
            "{name}: {}\n{stdout}\n{stderr}",
            child.status
        );
    }
}

/// A hardware frame's visible rectangle must lie inside the surface it was
/// decoded into. `FFmpeg` 8.1.3 alone makes a D3D12VA surface the visible size,
/// so after an SPS in the stream brings a left crop with a new picture size
/// (the 256x144 stream followed by `h264-bt709-crop-left.mp4`'s 128x96 one, 8
/// columns cropped from the left), the frame's picture reaches 8 columns past
/// a 120-wide surface, and a session must hand the source to software
/// (fallback reason `unsupportedCrop`), not import a picture whose last
/// columns were never decoded. Pure arithmetic: runs on every platform.
#[test]
fn a_hardware_picture_that_reaches_past_its_surface_has_no_visible_rect() {
    use crate::video_frame::visible_rect_within_surface as within;
    let rect = |x, y, width, height| alpha2_media::Rect {
        x,
        y,
        width,
        height,
    };
    // `FFmpeg` has already taken the right and bottom crop off a hardware frame's size.
    assert_eq!(
        within(128, 96, [8, 0, 0, 0], 120, 96),
        None,
        "the crop-left picture in the 120x96 surface FFmpeg reinitializes to"
    );
    assert_eq!(within(120, 104, [0, 8, 0, 0], 120, 96), None, "a top crop");
    assert_eq!(
        within(128, 96, [8, 0, 0, 0], 128, 96),
        Some(rect(8, 0, 120, 96)),
        "a surface the coded size, as Vulkan's"
    );
    assert_eq!(
        within(1920, 1080, [0; 4], 1920, 1080),
        Some(rect(0, 0, 1920, 1080)),
        "1080p, its 1088 coded rows already cut to 1080"
    );
    assert_eq!(
        within(256, 144, [0, 0, 6, 4], 256, 144),
        Some(rect(0, 0, 250, 140)),
        "a crop still on the frame on the right and bottom"
    );
    assert_eq!(
        within(8, 8, [16, 0, 0, 0], 64, 64),
        None,
        "a crop past the frame"
    );
    assert_eq!(
        within(8, 8, [usize::MAX, 0, 0, 0], 64, 64),
        None,
        "a crop past any size"
    );
}

/// The same on a real D3D12VA session: `h264-bt709-res-change.ts` is the
/// 256x144 transcode's 15 frames, then the crop-left stream's 128x96 SPS in
/// band, 8 columns cropped from the left. `FFmpeg` 8.1.3 alone makes a D3D12VA
/// decoder's surfaces the visible size, 120x96 there, for a picture that
/// reaches 128 columns; the session's `get_format` gives it surfaces of the
/// coded size instead (`alpha2_d3d12_get_format`), as `FFmpeg`'s master does, so
/// every picture fits its surface and the whole stream decodes in hardware.
///
/// Decoding it into the visible-size surfaces also left an Intel Iris Xe
/// (8086:9a49) on driver 32.0.101.7092 waiting forever on a lock inside the
/// driver in the next D3D12 video call on the device from another thread,
/// which hung every later hardware test of this binary.
#[cfg(all(target_os = "windows", feature = "hwdecode"))]
#[test]
fn a_d3d12va_stream_whose_sps_brings_a_left_crop_in_band_decodes_it_in_hardware() {
    let Some(context) = hardware_decode_context() else {
        return;
    };
    let name = "h264-bt709-res-change.ts";
    let cancel = CancelToken::default();
    let mut decoder = open_hardware_decoder(corpus(name), &context)
        .unwrap_or_else(|error| panic!("{name}: its first SPS opens in hardware: {error:?}"));
    let mut shown = Vec::new();
    while let Some(frame) = decoder
        .decode_video_next(&cancel)
        .unwrap_or_else(|error| panic!("{name} frame {}: {error:?}", shown.len()))
    {
        assert!(frame.is_hardware(), "{name} frame {}", shown.len());
        shown.push(frame.metadata().visible_rect);
    }
    let rect = |x, width, height| alpha2_media::Rect {
        x,
        y: 0,
        width,
        height,
    };
    assert!(shown.len() > 15, "{name}: {} frames", shown.len());
    assert!(
        shown[..15].iter().all(|shown| *shown == rect(0, 256, 144)),
        "{shown:?}"
    );
    assert!(
        shown[15..].iter().all(|shown| *shown == rect(8, 120, 96)),
        "{shown:?}"
    );
    assert_eq!(decoder.hardware_failure(), None, "{name}");
}
