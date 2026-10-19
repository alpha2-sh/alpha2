use super::*;

/// Which hardware devices exist is the loaded `FFmpeg`'s choice: one it
/// lacks (`VideoToolbox` is macOS-only) is an error a caller can fall
/// back from, where `AVHWDeviceContext::alloc` alone would panic.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "hwdevice"))]
#[test]
fn a_device_type_the_loaded_ffmpeg_lacks_is_an_error() {
    assert!(!hwdevice_available(ffi::AV_HWDEVICE_TYPE_VIDEOTOOLBOX));
    let error = alloc_hwdevice(ffi::AV_HWDEVICE_TYPE_VIDEOTOOLBOX, "VideoToolbox")
        .err()
        .expect("no VideoToolbox outside macOS");
    assert!(matches!(
        error,
        BridgeError::MissingHwDevice("VideoToolbox")
    ));
    assert_eq!(
        error.to_string(),
        "the loaded FFmpeg has no VideoToolbox hardware device"
    );
    // A type the pinned SDK has still allocates.
    #[cfg(target_os = "linux")]
    assert!(alloc_hwdevice(ffi::AV_HWDEVICE_TYPE_VULKAN, "Vulkan").is_ok());
}

/// Why the bridge checks first: rsmpeg's `alloc` unwraps the null context
/// `FFmpeg` returns for a type it lacks.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "hwdevice"))]
#[test]
#[should_panic(expected = "unwrap")]
fn rsmpeg_alloc_panics_on_a_missing_device_type() {
    let _ = AVHWDeviceContext::alloc(ffi::AV_HWDEVICE_TYPE_VIDEOTOOLBOX);
}

/// Codec parameters carrying one display matrix.
fn with_display_matrix(counter_clockwise: f64, hflip: bool) -> AVCodecParameters {
    let mut parameters = AVCodecParameters::new();
    let raw = parameters.as_mut_ptr();
    // SAFETY: `raw` is a live parameters object; the new entry has room for a 3x3 `i32` matrix,
    // which `FFmpeg` then writes in place.
    unsafe {
        let entry = ffi::av_packet_side_data_new(
            &raw mut (*raw).coded_side_data,
            &raw mut (*raw).nb_coded_side_data,
            ffi::AV_PKT_DATA_DISPLAYMATRIX,
            9 * size_of::<i32>(),
            0,
        );
        assert!(!entry.is_null());
        #[allow(clippy::cast_ptr_alignment)] // `av_malloc` aligns side data.
        let matrix = (*entry).data.cast::<i32>();
        // `av_display_rotation_set` takes the clockwise angle.
        ffi::av_display_rotation_set(matrix, -counter_clockwise);
        if hflip {
            ffi::av_display_matrix_flip(matrix, 1, 0);
        }
    }
    parameters
}

#[test]
fn display_rotation_reads_the_angle_and_the_mirror() {
    for (angle, hflip) in [(90.0, false), (-90.0, false), (180.0, false), (90.0, true)] {
        let rotation =
            display_rotation(&with_display_matrix(angle, hflip)).expect("display matrix");
        assert!(
            (rotation.counter_clockwise_degrees.rem_euclid(360.0) - f64::rem_euclid(angle, 360.0))
                .abs()
                < 0.01,
            "{angle} deg {hflip}: {rotation:?}"
        );
        assert_eq!(rotation.flipped, hflip);
    }
    assert_eq!(display_rotation(&AVCodecParameters::new()), None);
}

/// The matrix `FFmpeg`'s JPEG decoder attaches to a frame for each Exif
/// `Orientation` (`libavcodec/mjpegdec.c`: a clockwise angle, then a
/// horizontal flip for the mirrored values) reads back as that angle,
/// counter-clockwise, and the flip.
#[test]
fn frame_display_rotation_reads_the_matrix_a_decoder_attached() {
    for (clockwise, flip) in [
        (0.0, true),
        (180.0, false),
        (180.0, true),
        (90.0, true),
        (90.0, false),
        (-90.0, true),
        (-90.0, false),
    ] {
        let mut frame = AVFrame::new();
        // SAFETY: `frame` is live; the new side data has room for a 3x3 `i32` matrix, which
        // `FFmpeg` writes in place.
        unsafe {
            let entry = ffi::av_frame_new_side_data(
                frame.as_mut_ptr(),
                ffi::AV_FRAME_DATA_DISPLAYMATRIX,
                9 * size_of::<i32>(),
            );
            assert!(!entry.is_null());
            #[allow(clippy::cast_ptr_alignment)] // `av_malloc` aligns side data.
            let matrix = (*entry).data.cast::<i32>();
            ffi::av_display_rotation_set(matrix, clockwise);
            if flip {
                ffi::av_display_matrix_flip(matrix, 1, 0);
            }
        }
        let rotation = frame_display_rotation(&frame).expect("frame display matrix");
        assert!(
            (rotation.counter_clockwise_degrees + clockwise).rem_euclid(360.0) < 0.01
                || (rotation.counter_clockwise_degrees + clockwise).rem_euclid(360.0) > 359.99,
            "{clockwise} deg {flip}: {rotation:?}"
        );
        assert_eq!(rotation.flipped, flip);
    }
    assert_eq!(frame_display_rotation(&AVFrame::new()), None);
}

#[test]
fn video_plane_copy_normalizes_positive_and_negative_stride() {
    let bytes = [1_u8, 2, 99, 4, 5, 99, 7, 8, 99];
    assert_eq!(
        copy_video_plane(bytes.as_ptr(), 3, 2, 3, "plane").expect("positive stride"),
        vec![1, 2, 4, 5, 7, 8]
    );
    // A negative-stride frame points at its logical first row and walks backwards through the
    // allocation for subsequent rows.
    assert_eq!(
        copy_video_plane(
            // SAFETY: index six is the first byte of the final stored row.
            unsafe { bytes.as_ptr().add(6) },
            -3,
            2,
            3,
            "plane",
        )
        .expect("negative stride"),
        vec![7, 8, 4, 5, 1, 2]
    );
}

#[test]
fn yuv420_frame_round_trips_odd_tightly_packed_planes() {
    let y = vec![16_u8, 32, 48, 64, 80, 96, 112, 128, 144];
    let u = vec![90_u8, 100, 110, 120];
    let v = vec![140_u8, 150, 160, 170];
    let frame = yuv420p_frame(&y, 3, &u, 2, &v, 2, 3, 3).expect("YUV frame");
    let copied = yuv_planes(&frame)
        .expect("valid planes")
        .expect("supported format");
    assert_eq!(
        copied.layout,
        yuv_layout(ffi::AV_PIX_FMT_YUV420P).expect("I420")
    );
    assert_eq!((copied.width, copied.height), (3, 3));
    let dimensions = copied
        .planes
        .iter()
        .map(|plane| (plane.width, plane.height))
        .collect::<Vec<_>>();
    assert_eq!(dimensions, [(3, 3), (2, 2), (2, 2)]);
    for (plane, expected) in copied.planes.iter().zip([&y, &u, &v]) {
        assert_eq!(plane.bytes.len(), (plane.stride * plane.height) as usize);
        let rows = plane
            .bytes
            .chunks(plane.stride as usize)
            .flat_map(|row| &row[..plane.width as usize])
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(&rows, expected);
    }
}

#[test]
fn written_frames_share_their_buffer_and_outlive_the_frame() {
    let y = vec![16_u8; 64 * 4];
    let chroma = vec![128_u8; 32 * 2];
    let frame = yuv420p_frame(&y, 64, &chroma, 32, &chroma, 32, 64, 4).expect("YUV frame");
    let data = frame.data.map(<*mut u8>::cast_const);
    let planes = frame_yuv_planes(&frame, true)
        .expect("valid planes")
        .expect("supported format")
        .planes;
    for (plane, pointer) in planes.iter().zip(data) {
        assert_eq!(plane.bytes.as_ptr(), pointer, "a view, not a copy");
    }
    drop(frame);
    assert!(planes[0].bytes.iter().all(|sample| *sample == 16));
    assert!(
        planes[2]
            .bytes
            .chunks(planes[2].stride as usize)
            .all(|row| row[..32].iter().all(|sample| *sample == 128))
    );
}

/// Every byte of a plane, row padding included, as one sum: a read of
/// each, which memcheck reports when one was never written.
fn plane_sum(plane: &[u8]) -> u64 {
    plane.iter().map(|byte| u64::from(*byte)).sum()
}

/// An encoder that still holds the frame (a reference, as `send_frame`
/// keeps one) keeps the pixels it was given, and the next write goes to
/// other memory, laid out as the buffer was allocated, all of it written.
#[test]
fn a_frame_buffer_an_encoder_still_holds_is_copied_away_before_it_is_rewritten() {
    let mut buffer = Yuv420pFrameBuffer::new(6, 4).expect("buffer");
    let fill = |buffer: &mut Yuv420pFrameBuffer, value: u8| {
        let planes = buffer.begin_write().expect("writable planes");
        let strides = (planes.y_stride, planes.chroma_stride);
        for plane in [planes.y, planes.u, planes.v] {
            assert_eq!(plane_sum(plane), plane_sum(plane), "every byte written");
            plane.fill(value);
        }
        strides
    };
    let first = fill(&mut buffer, 1);
    let held = buffer.frame().clone();
    let held_planes = held.data;
    let second = fill(&mut buffer, 9);
    assert_eq!(first, (6, 3));
    assert_eq!(
        second, first,
        "the copy is laid out as the buffer was allocated"
    );
    assert_ne!(buffer.frame().data[0], held_planes[0], "written elsewhere");
    let planes = yuv_planes(&held).expect("planes").expect("I420").planes;
    assert!(
        planes
            .iter()
            .all(|plane| plane.bytes.iter().all(|byte| *byte == 1))
    );
    let planes = yuv_planes(buffer.frame())
        .expect("planes")
        .expect("I420")
        .planes;
    assert!(
        planes
            .iter()
            .all(|plane| plane.bytes.iter().all(|byte| *byte == 9))
    );
}

/// A copy that wrote only part of each padded row leaves the rest zero, not
/// what the allocator left there.
#[test]
fn row_padding_past_what_a_copy_wrote_is_zero() {
    for (format, chroma_rows) in [(ffi::AV_PIX_FMT_NV12, 2), (ffi::AV_PIX_FMT_YUV420P, 2)] {
        let mut frame = AVFrame::new();
        frame.set_format(format);
        frame.set_width(6);
        frame.set_height(4);
        frame.get_buffer(0).expect("padded frame");
        for buffer in frame.buf {
            // SAFETY: a just-allocated, unshared buffer of `size` bytes.
            if let Some(buffer) = unsafe { buffer.as_ref() } {
                // SAFETY: as above.
                unsafe { ptr::write_bytes(buffer.data, 0xAA, buffer.size) };
            }
        }
        zero_row_padding(&mut frame, 6, 4).expect("padding");
        let planes = usize::try_from(
            // SAFETY: reads the static descriptor table.
            unsafe { ffi::av_pix_fmt_count_planes(format) },
        )
        .expect("planes");
        for plane in 0..planes {
            let stride = usize::try_from(frame.linesize[plane]).expect("stride");
            let (written, rows) = if plane == 0 {
                (6, 4)
            } else {
                (if planes == 2 { 6 } else { 3 }, chroma_rows)
            };
            assert!(stride > written, "a padded row");
            // SAFETY: `rows` rows of `stride` bytes of this frame's own plane.
            let bytes = unsafe { slice::from_raw_parts(frame.data[plane], stride * rows) };
            for row in bytes.chunks(stride) {
                assert!(row[..written].iter().all(|byte| *byte == 0xAA), "kept");
                assert!(row[written..].iter().all(|byte| *byte == 0), "zeroed");
            }
        }
    }
}

/// A frame whose rows are padded, the padding holding whatever its
/// allocator left there (here 0xA5), with only its samples written.
fn frame_with_unwritten_padding(format: ffi::AVPixelFormat, width: i32, height: i32) -> AVFrame {
    let mut frame = AVFrame::new();
    frame.set_format(format);
    frame.set_width(width);
    frame.set_height(height);
    frame.get_buffer(0).expect("padded frame");
    for buffer in frame.buf {
        // SAFETY: a just-allocated, unshared buffer of `size` bytes.
        if let Some(buffer) = unsafe { buffer.as_ref() } {
            // SAFETY: as above.
            unsafe { ptr::write_bytes(buffer.data, 0xA5, buffer.size) };
        }
    }
    let layout = yuv_layout(format).expect("a YCbCr layout");
    let sample = usize::try_from(layout.bytes_per_sample()).expect("sample");
    let (width, height) = (
        usize::try_from(width).expect("width"),
        usize::try_from(height).expect("height"),
    );
    let chroma = (
        width.div_ceil(1 << layout.chroma_shift_x),
        height.div_ceil(1 << layout.chroma_shift_y),
    );
    let planes = if layout.interleaved_chroma {
        vec![(width, height), (chroma.0 * 2, chroma.1)]
    } else {
        vec![(width, height), chroma, chroma]
    };
    for (index, (samples, rows)) in planes.into_iter().enumerate() {
        let stride = usize::try_from(frame.linesize[index]).expect("stride");
        assert!(stride > samples * sample, "a padded row");
        for row in 0..rows {
            // SAFETY: row `row` of plane `index` holds `stride` bytes, the first
            // `samples * sample` of them its samples.
            unsafe {
                ptr::write_bytes(frame.data[index].add(row * stride), 0x40, samples * sample);
            }
        }
    }
    frame
}

/// `FFmpeg`'s own decoders draw pictures from zeroed pools; libdav1d, an
/// external library, and a replaced allocator do not.
#[test]
fn only_ffmpegs_own_decoders_with_the_default_allocator_return_initialized_frames() {
    unsafe extern "C" fn replaced(
        _: *mut ffi::AVCodecContext,
        _: *mut ffi::AVFrame,
        _: i32,
    ) -> i32 {
        -1
    }
    let decoder = |name: &CStr| {
        let codec = rsmpeg::avcodec::AVCodec::find_decoder_by_name(name).expect("decoder");
        AVCodecContext::new(&codec)
    };
    assert!(decoder_frames_are_initialized(&decoder(c"h264")));
    assert!(decoder_frames_are_initialized(&decoder(c"hevc")));
    assert!(!decoder_frames_are_initialized(&decoder(c"libdav1d")));
    let mut custom = decoder(c"h264");
    // SAFETY: the context is unopened and only this test uses it.
    unsafe { (*custom.as_mut_ptr()).get_buffer2 = Some(replaced) };
    assert!(!decoder_frames_are_initialized(&custom));
}

/// `yuv_planes` takes any frame: one a caller allocated and filled holds
/// in each row's padding whatever its allocator left there, memory nothing
/// wrote. The planes it hands out hold every sample and no byte past them
/// that is not zero.
#[test]
fn yuv_planes_hand_out_no_byte_past_a_rows_samples() {
    for format in [
        ffi::AV_PIX_FMT_YUV420P,
        ffi::AV_PIX_FMT_NV12,
        ffi::AV_PIX_FMT_YUV420P10LE,
    ] {
        let frame = frame_with_unwritten_padding(format, 6, 4);
        let planes = yuv_planes(&frame).expect("planes").expect("YCbCr");
        let sample = planes.layout.bytes_per_sample();
        for (index, plane) in planes.planes.iter().enumerate() {
            let pair = if planes.layout.interleaved_chroma && index == 1 {
                2
            } else {
                1
            };
            let row = usize::try_from(plane.width * sample * pair).expect("row");
            let stride = usize::try_from(plane.stride).expect("stride");
            assert_eq!(plane.bytes.len(), stride * plane.height as usize);
            for bytes in plane.bytes.chunks(stride) {
                assert!(
                    bytes[..row].iter().all(|byte| *byte == 0x40),
                    "{format} samples"
                );
                assert!(
                    bytes[row..].iter().all(|byte| *byte == 0),
                    "{format} plane {index}: {:x?} past the samples",
                    &bytes[row..]
                );
            }
        }
    }
}

/// The Windows and macOS shims' records convert to their public frame
/// records on every target, so a change to a field's name or type fails
/// here, not only on the platform that builds the shim.
#[test]
fn the_platform_frame_records_convert_field_for_field_on_every_target() {
    let d3d12 = interop::Alpha2D3D12FrameInfo {
        resource: 1,
        fence: 2,
        fence_value: 3,
        device: 4,
        dxgi_format: 5,
        width: 6,
        height: 7,
        hardware_format: 8,
        software_format: 9,
        surface_width: 10,
        surface_height: 11,
    };
    assert_eq!(
        D3D12FrameInfo::from_descriptor(&d3d12),
        D3D12FrameInfo {
            resource: 1,
            fence: 2,
            fence_value: 3,
            device: 4,
            dxgi_format: 5,
            width: 6,
            height: 7,
            software_format: 9,
            surface_width: 10,
            surface_height: 11,
        }
    );
    let videotoolbox = interop::Alpha2VideoToolboxFrameInfo {
        pixel_buffer: 1,
        width: 2,
        height: 3,
        hardware_format: 4,
        software_format: 5,
    };
    assert_eq!(
        VideoToolboxFrameInfo::from_descriptor(&videotoolbox),
        VideoToolboxFrameInfo {
            pixel_buffer: 1,
            width: 2,
            height: 3,
            software_format: 5,
        }
    );
}

/// The `VideoToolbox` shim reads a frame's pixel buffer, size and the
/// layout its frames context allocated, refuses anything else, and picks
/// `VideoToolbox` output only when the decoder offers it. It reads only
/// `FFmpeg`'s structures, so this runs on Linux too; the pixel buffer is a
/// stand-in address, copied and never read through.
#[cfg(all(any(target_os = "linux", target_os = "macos"), feature = "hwdevice"))]
#[test]
fn the_videotoolbox_shim_reads_its_frames_and_refuses_anything_else() {
    let mut frame = AVFrame::new();
    frame.set_format(ffi::AV_PIX_FMT_VIDEOTOOLBOX);
    frame.set_width(64);
    frame.set_height(48);
    assert!(
        read_videotoolbox_frame_info(&frame).is_err(),
        "no pixel buffer"
    );
    // SAFETY: `frame` owns no buffer in `data[3]`; it is cleared again before the frame drops.
    unsafe { (*frame.as_mut_ptr()).data[3] = std::ptr::without_provenance_mut(0x1000) };
    assert_eq!(
        read_videotoolbox_frame_info(&frame).expect("a VideoToolbox frame"),
        VideoToolboxFrameInfo {
            pixel_buffer: 0x1000,
            width: 64,
            height: 48,
            software_format: ffi::AV_PIX_FMT_NONE,
        },
        "without a frames context the layout is unknown"
    );
    #[cfg(target_os = "linux")]
    let device = AVHWDeviceContext::alloc(ffi::AV_HWDEVICE_TYPE_VULKAN);
    #[cfg(target_os = "macos")]
    let device = rsmpeg::avutil::AVHWDeviceContext::alloc(ffi::AV_HWDEVICE_TYPE_VIDEOTOOLBOX);
    let mut frames = device.hwframe_ctx_alloc();
    frames.data().sw_format = ffi::AV_PIX_FMT_P010LE;
    // SAFETY: the frame takes a reference of its own to the live frames context and releases it
    // when it drops.
    unsafe { (*frame.as_mut_ptr()).hw_frames_ctx = ffi::av_buffer_ref(frames.as_ptr()) };
    assert_eq!(
        read_videotoolbox_frame_info(&frame)
            .expect("a VideoToolbox frame")
            .software_format,
        ffi::AV_PIX_FMT_P010LE
    );
    frame.set_format(ffi::AV_PIX_FMT_NV12);
    assert!(
        read_videotoolbox_frame_info(&frame).is_err(),
        "a software frame"
    );
    // SAFETY: as above.
    unsafe { (*frame.as_mut_ptr()).data[3] = std::ptr::null_mut() };

    let offered = [
        ffi::AV_PIX_FMT_YUV420P,
        ffi::AV_PIX_FMT_VIDEOTOOLBOX,
        ffi::AV_PIX_FMT_NONE,
    ];
    let software_only = [ffi::AV_PIX_FMT_YUV420P, ffi::AV_PIX_FMT_NONE];
    // SAFETY: each list ends in AV_PIX_FMT_NONE and the callback reads no codec context.
    unsafe {
        assert_eq!(
            alpha2_videotoolbox_get_format(std::ptr::null_mut(), offered.as_ptr()),
            ffi::AV_PIX_FMT_VIDEOTOOLBOX
        );
        assert_eq!(
            alpha2_videotoolbox_get_format(std::ptr::null_mut(), software_only.as_ptr()),
            ffi::AV_PIX_FMT_NONE,
            "which makes the decoder open fail, and the caller fall back"
        );
        assert_eq!(
            alpha2_videotoolbox_get_format(std::ptr::null_mut(), std::ptr::null()),
            ffi::AV_PIX_FMT_NONE
        );
    }
}

/// The D3D12 shim refuses what is not a D3D12 frame or device, and picks
/// D3D12 output only when the decoder offers it. Windows only: the shim
/// needs the Windows headers.
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
#[test]
fn the_d3d12_shim_refuses_what_is_not_its_own() {
    let mut descriptor = D3D12FrameDescriptor::default();
    let mut frame = AVFrame::new();
    frame.set_format(ffi::AV_PIX_FMT_NV12);
    // SAFETY: a live frame and descriptor; the shim checks the format before it reads anything
    // else.
    let refused = unsafe { alpha2_d3d12_frame_info(frame.as_ptr(), &raw mut descriptor) };
    assert_eq!(refused, -libc_einval(), "a software frame");
    frame.set_format(ffi::AV_PIX_FMT_D3D12);
    // SAFETY: as above; a D3D12 frame without data or a frames context is refused before either
    // is read.
    let refused = unsafe { alpha2_d3d12_frame_info(frame.as_ptr(), &raw mut descriptor) };
    assert_eq!(refused, -libc_einval(), "no texture");
    assert!(matches!(
        d3d12_frame_info(&frame),
        Err(BridgeError::FfmpegCode(_))
    ));
    // The encode handoff's fence-value shims check the same before they write or read.
    // SAFETY: a live frame with no descriptor, which the shim refuses before touching it.
    let refused = unsafe { alpha2_d3d12_frame_set_fence_value(frame.as_mut_ptr(), 7) };
    assert_eq!(refused, -libc_einval(), "no descriptor to write");
    assert!(matches!(
        d3d12_frame_fence_value(&frame),
        Err(BridgeError::FfmpegCode(_))
    ));
    assert!(D3D12EncodeFrameAccess::begin(&mut frame).is_err());
    // SAFETY: a null device is refused before the context is touched.
    assert!(unsafe { create_d3d12_device_context(0) }.is_err());

    let offered = [
        ffi::AV_PIX_FMT_YUV420P,
        ffi::AV_PIX_FMT_D3D12,
        ffi::AV_PIX_FMT_NONE,
    ];
    let software_only = [ffi::AV_PIX_FMT_YUV420P, ffi::AV_PIX_FMT_NONE];
    // SAFETY: each list ends in AV_PIX_FMT_NONE and the callback reads no codec context.
    unsafe {
        assert_eq!(
            alpha2_d3d12_get_format(std::ptr::null_mut(), offered.as_ptr()),
            ffi::AV_PIX_FMT_D3D12
        );
        assert_eq!(
            alpha2_d3d12_get_format(std::ptr::null_mut(), software_only.as_ptr()),
            ffi::AV_PIX_FMT_NONE
        );
    }
}

/// `EINVAL`, which `AVERROR(EINVAL)` negates.
#[cfg(all(target_os = "windows", feature = "hwdevice"))]
const fn libc_einval() -> i32 {
    22
}

#[test]
fn a_sample_profile_says_whether_its_components_are_red_green_and_blue() {
    for (format, rgb, alpha, depth) in [
        (ffi::AV_PIX_FMT_YUVA444P12LE, false, true, 12),
        (ffi::AV_PIX_FMT_GRAY10LE, false, false, 10),
        (ffi::AV_PIX_FMT_UYVY422, false, false, 8),
        (ffi::AV_PIX_FMT_RGB24, true, false, 8),
        (ffi::AV_PIX_FMT_GBRAP16LE, true, true, 16),
        // A palette's entries are red, green, blue, and alpha.
        (ffi::AV_PIX_FMT_PAL8, true, true, 8),
    ] {
        let profile = sample_profile(format).expect("descriptor");
        let name = pixel_format_name(format).unwrap_or_default();
        assert_eq!(profile.is_rgb, rgb, "{name}");
        assert_eq!(profile.has_alpha, alpha, "{name}");
        assert_eq!(profile.bit_depth, depth, "{name}");
    }
}

#[test]
fn yuv_layouts_come_from_the_pixel_format_descriptor() {
    let layout = |shift_x, shift_y, bit_depth, interleaved_chroma, msb_aligned| {
        Some(YuvPlaneLayout {
            chroma_shift_x: shift_x,
            chroma_shift_y: shift_y,
            bit_depth,
            interleaved_chroma,
            msb_aligned,
        })
    };
    for (format, expected) in [
        (ffi::AV_PIX_FMT_YUV420P, layout(1, 1, 8, false, false)),
        (ffi::AV_PIX_FMT_YUVJ422P, layout(1, 0, 8, false, false)),
        (ffi::AV_PIX_FMT_YUV444P, layout(0, 0, 8, false, false)),
        (ffi::AV_PIX_FMT_YUV410P, layout(2, 2, 8, false, false)),
        (ffi::AV_PIX_FMT_YUV420P10LE, layout(1, 1, 10, false, false)),
        (ffi::AV_PIX_FMT_YUV444P12LE, layout(0, 0, 12, false, false)),
        (ffi::AV_PIX_FMT_YUV422P16LE, layout(1, 0, 16, false, false)),
        (ffi::AV_PIX_FMT_NV12, layout(1, 1, 8, true, false)),
        (ffi::AV_PIX_FMT_P010LE, layout(1, 1, 10, true, true)),
        (ffi::AV_PIX_FMT_YUV420P10BE, None),
        (ffi::AV_PIX_FMT_NV21, None),
        (ffi::AV_PIX_FMT_YUVA420P, None),
        (ffi::AV_PIX_FMT_GRAY8, None),
        (ffi::AV_PIX_FMT_RGB24, None),
        (ffi::AV_PIX_FMT_GBRP, None),
        (ffi::AV_PIX_FMT_PAL8, None),
        (ffi::AV_PIX_FMT_VULKAN, None),
    ] {
        assert_eq!(
            yuv_layout(format),
            expected,
            "{}",
            pixel_format_name(format).unwrap_or_default()
        );
    }
}

#[test]
fn planar_audio_supports_the_declared_channel_matrix() {
    for channel_count in [1_usize, 2, 3, 8] {
        let channels = (0..channel_count)
            .map(|channel| {
                vec![f32::from(u8::try_from(channel).expect("small channel")) + 0.25; 1_024]
            })
            .collect::<Vec<_>>();
        let frame = audio_frame_f32p(&channels, 48_000, 7).expect("valid planar frame");
        assert_eq!(frame.nb_samples, 1_024);
        assert_eq!(
            frame.ch_layout().nb_channels,
            i32::try_from(channel_count).expect("small channel count")
        );
        assert!(usize::try_from(frame.linesize[0]).unwrap_or(0) >= 4_096);
        for (index, expected) in channels.iter().enumerate() {
            assert!(!frame.data[index].is_null());
            let byte_len = expected.len() * size_of::<f32>();
            // SAFETY: `audio_frame_f32p` created one FLTP plane containing exactly
            // `expected.len()` initialized f32 samples.
            let actual = unsafe { slice::from_raw_parts(frame.data[index], byte_len) };
            // SAFETY: a Vec<f32> is initialized storage readable as bytes.
            let expected =
                unsafe { slice::from_raw_parts(expected.as_ptr().cast::<u8>(), byte_len) };
            assert_eq!(actual, expected);
        }
    }
}
