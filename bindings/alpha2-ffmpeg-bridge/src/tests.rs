use super::*;

/// A device type the loaded `FFmpeg` lacks is an error, where `AVHWDeviceContext::alloc` would
/// panic.
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
    #[cfg(target_os = "linux")]
    assert!(alloc_hwdevice(ffi::AV_HWDEVICE_TYPE_VULKAN, "Vulkan").is_ok());
}

/// rsmpeg's `alloc` unwraps the null context returned for a missing type.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "hwdevice"))]
#[test]
#[should_panic(expected = "unwrap")]
fn rsmpeg_alloc_panics_on_a_missing_device_type() {
    let _ = AVHWDeviceContext::alloc(ffi::AV_HWDEVICE_TYPE_VIDEOTOOLBOX);
}

fn with_display_matrix(counter_clockwise: f64, hflip: bool) -> AVCodecParameters {
    let mut parameters = AVCodecParameters::new();
    let raw = parameters.as_mut_ptr();
    // SAFETY: `raw` is live; the new entry has room for the 3x3 `i32` matrix `FFmpeg` writes.
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

/// Each Exif `Orientation` matrix from `libavcodec/mjpegdec.c` (clockwise angle, then a flip for
/// mirrored values) reads back as the counter-clockwise angle and the flip.
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
        // SAFETY: `frame` is live; the side data has room for the 3x3 `i32` matrix `FFmpeg` writes.
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
    // A negative-stride frame points at its first logical row and walks backwards.
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

/// Sums every byte including padding so memcheck reports any unwritten byte.
fn plane_sum(plane: &[u8]) -> u64 {
    plane.iter().map(|byte| u64::from(*byte)).sum()
}

/// A frame an encoder still references keeps its pixels; the next write goes to fresh, fully
/// written memory with the same layout.
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

/// A partial-row copy leaves the rest of the padded row zero, not allocator garbage.
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

/// A frame whose row padding holds 0xA5, with only its samples written.
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
            // `samples * sample` of them samples.
            unsafe {
                ptr::write_bytes(frame.data[index].add(row * stride), 0x40, samples * sample);
            }
        }
    }
    frame
}

/// `FFmpeg`'s own decoders use zeroed pools; libdav1d and replaced allocators do not.
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

/// `yuv_planes` copies every sample of a caller-filled frame and zeroes everything past them.
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

/// The Windows and macOS shim records convert on every target, so a field change fails on Linux
/// too.
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

/// The `VideoToolbox` shim reads only `FFmpeg` structures, so it is tested on Linux with a
/// stand-in pixel buffer address that is never dereferenced.
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
    // SAFETY: the frame takes its own reference to the live frames context.
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

/// The D3D12 shim refuses non-D3D12 frames and devices, and picks D3D12 only when offered.
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
    // SAFETY: as above; a frame without data or a frames context is refused before either is read.
    let refused = unsafe { alpha2_d3d12_frame_info(frame.as_ptr(), &raw mut descriptor) };
    assert_eq!(refused, -libc_einval(), "no texture");
    assert!(matches!(
        d3d12_frame_info(&frame),
        Err(BridgeError::FfmpegCode(_))
    ));
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
fn interleaved_audio_keeps_channel_order() {
    let channels = [
        vec![0.25_f32, 0.5, 0.75, 1.0],
        vec![-0.25_f32, -0.5, -0.75, -1.5],
    ];
    let frame = audio_frame(&channels, ffi::AV_SAMPLE_FMT_FLT, 48_000, 0).expect("FLT frame");
    assert_eq!(frame.format, ffi::AV_SAMPLE_FMT_FLT);
    assert_eq!(frame.nb_samples, 4);
    // SAFETY: an FLT frame has one plane of nb_samples x channels f32 samples.
    let packed = unsafe { slice::from_raw_parts(frame.data[0], 8 * size_of::<f32>()) };
    let packed = packed
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f32::from_ne_bytes(*bytes))
        .collect::<Vec<_>>();
    assert_eq!(packed, [0.25, -0.25, 0.5, -0.5, 0.75, -0.75, 1.0, -1.5]);
    let frame = audio_frame(&channels, ffi::AV_SAMPLE_FMT_S16, 48_000, 0).expect("S16 frame");
    // SAFETY: an S16 frame has one plane of nb_samples x channels i16 samples.
    let packed = unsafe { slice::from_raw_parts(frame.data[0], 8 * size_of::<i16>()) };
    let packed = packed
        .as_chunks::<2>()
        .0
        .iter()
        .map(|bytes| i16::from_ne_bytes(*bytes))
        .collect::<Vec<_>>();
    // Rounded to the nearest step, with the out-of-range sample clamped.
    assert_eq!(
        packed,
        [
            8_192, -8_192, 16_384, -16_384, 24_575, -24_575, 32_767, -32_767
        ]
    );
    assert!(audio_frame(&channels, ffi::AV_SAMPLE_FMT_DBL, 48_000, 0).is_err());
}

#[test]
fn planar_audio_supports_the_declared_channel_matrix() {
    for channel_count in [1_usize, 2, 3, 8] {
        let channels = (0..channel_count)
            .map(|channel| {
                vec![f32::from(u8::try_from(channel).expect("small channel")) + 0.25; 1_024]
            })
            .collect::<Vec<_>>();
        let frame =
            audio_frame(&channels, ffi::AV_SAMPLE_FMT_FLTP, 48_000, 7).expect("valid planar frame");
        assert_eq!(frame.nb_samples, 1_024);
        assert_eq!(
            frame.ch_layout().nb_channels,
            i32::try_from(channel_count).expect("small channel count")
        );
        assert!(usize::try_from(frame.linesize[0]).unwrap_or(0) >= 4_096);
        for (index, expected) in channels.iter().enumerate() {
            assert!(!frame.data[index].is_null());
            let byte_len = expected.len() * size_of::<f32>();
            // SAFETY: `audio_frame` made one FLTP plane of `expected.len()` initialized samples.
            let actual = unsafe { slice::from_raw_parts(frame.data[index], byte_len) };
            // SAFETY: a Vec<f32> is initialized storage readable as bytes.
            let expected =
                unsafe { slice::from_raw_parts(expected.as_ptr().cast::<u8>(), byte_len) };
            assert_eq!(actual, expected);
        }
    }
}

/// A codec's warning is kept under its name, after a mark and not before.
#[test]
fn log_capture_keeps_warning_lines_with_their_class() {
    let codec = rsmpeg::avcodec::AVCodec::find_decoder(ffi::AV_CODEC_ID_H264).expect("h264");
    let before = log_mark();
    let mut context = AVCodecContext::new(&codec);
    let options = rsmpeg::avutil::AVDictionary::new(c"threads", c"not-a-number", 0);
    assert!(
        context.open(Some(options)).is_err(),
        "a bad option value fails the open"
    );
    let lines = log_since(before, &["h264"]);
    assert!(
        lines.iter().any(|line| line.text.contains("not-a-number")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .all(|line| line.class == "h264" && line.level <= ffi::AV_LOG_WARNING.cast_signed())
    );
    assert_eq!(log_since(log_mark(), &["h264"]), []);
}

#[test]
fn log_detail_keeps_the_last_four_lines_ascii() {
    let mark = log_mark();
    for index in 0..6 {
        crate::log::record_for_tests(16, "test_detail", &format!("line {index}\n"));
    }
    crate::log::record_for_tests(16, "test_detail", &format!("caf\u{e9} {}", "x".repeat(300)));
    let lines = log_since(mark, &["test_detail"]);
    assert_eq!(lines.len(), 7);
    let detail = log_detail(&lines);
    assert!(
        detail.starts_with("; FFmpeg said: [test_detail] line 3 | "),
        "{detail}"
    );
    assert!(!detail.contains("line 2"), "{detail}");
    assert!(detail.is_ascii(), "{detail}");
    assert!(detail.contains("caf\\u{e9} x"), "{detail}");
    let last = detail.rsplit(" | ").next().expect("the last line");
    assert_eq!(last.len(), "[test_detail] ".len() + 200, "{last}");
    assert_eq!(log_detail(&[]), "");
}

/// The pinned runtime decodes VP9 alpha, which only libvpx reads (`FFmpeg`'s `vp9` drops it).
#[test]
fn the_runtime_has_the_libvpx_vp9_decoder() {
    let decoder = rsmpeg::avcodec::AVCodec::find_decoder_by_name(c"libvpx-vp9")
        .expect("this FFmpeg runtime has no libvpx-vp9 decoder");
    assert_eq!(decoder.id, ffi::AV_CODEC_ID_VP9);
}

/// A frame of `format` whose every sample of plane `i` is `samples[i]`, at 8 or 16 bits per sample.
fn uniform_frame(
    format: ffi::AVPixelFormat,
    width: i32,
    height: i32,
    samples: [u16; 4],
) -> AVFrame {
    let mut frame = AVFrame::new();
    frame.set_format(format);
    frame.set_width(width);
    frame.set_height(height);
    frame.get_buffer(0).expect("frame");
    let wide = sample_profile(format).expect("descriptor").bit_depth > 8;
    for (plane, sample) in samples.iter().enumerate() {
        let data = frame.data[plane];
        if data.is_null() {
            continue;
        }
        let rows = usize::try_from(plane_rows(format, height, plane)).expect("rows");
        let bytes = usize::try_from(frame.linesize[plane]).expect("linesize") * rows;
        // SAFETY: the plane holds `rows` rows at its linesize inside the frame's buffer.
        let plane = unsafe { slice::from_raw_parts_mut(data, bytes) };
        if wide {
            for pair in plane.as_chunks_mut::<2>().0 {
                *pair = sample.to_le_bytes();
            }
        } else {
            plane.fill(u8::try_from(*sample).expect("8-bit sample"));
        }
    }
    frame
}

fn plane_rows(format: ffi::AVPixelFormat, height: i32, plane: usize) -> i32 {
    // SAFETY: `av_pix_fmt_desc_get` returns null or a static descriptor.
    let descriptor = unsafe { ffi::av_pix_fmt_desc_get(format).as_ref() }.expect("descriptor");
    let rgb = descriptor.flags & u64::from(ffi::AV_PIX_FMT_FLAG_RGB) != 0;
    if (plane == 1 || plane == 2) && !rgb {
        -((-height) >> descriptor.log2_chroma_h)
    } else {
        height
    }
}

const TAIL_PATTERN: u8 = 0xA5;

/// Fills every byte of the scaler's destination after the last row of its last plane.
fn mark_destination_tail(scaler: &mut PackedScaler) -> (*const u8, usize) {
    let frame = &scaler.destination;
    // SAFETY: a frame from `av_frame_get_buffer` has one live buffer in `buf[0]`.
    let buffer = unsafe { frame.buf[0].as_ref() }.expect("one buffer");
    let mut end = 0;
    for plane in 0..4 {
        if frame.data[plane].is_null() {
            continue;
        }
        let rows = usize::try_from(plane_rows(frame.format, frame.height, plane)).expect("rows");
        let stride = usize::try_from(frame.linesize[plane]).expect("linesize");
        let offset = frame.data[plane] as usize - buffer.data as usize;
        end = end.max(offset + rows * stride);
    }
    assert!(end < buffer.size, "no slack after the last plane");
    // SAFETY: `end..size` lies inside the frame's only buffer, which the scaler owns.
    unsafe { ptr::write_bytes(buffer.data.add(end), TAIL_PATTERN, buffer.size - end) };
    // SAFETY: as above.
    (unsafe { buffer.data.add(end) }, buffer.size - end)
}

fn tail_is_untouched((start, len): (*const u8, usize)) -> bool {
    // SAFETY: the range `mark_destination_tail` returned, in a buffer the scaler still owns.
    unsafe { slice::from_raw_parts(start, len) }
        .iter()
        .all(|byte| *byte == TAIL_PATTERN)
}

fn rgba_scaler(width: i32, height: i32) -> PackedScaler {
    let mut scaler = PackedScaler::new(
        width,
        height,
        ffi::AV_PIX_FMT_YUVA420P,
        ffi::AV_PIX_FMT_RGBA,
    )
    .expect("scaler");
    let bt709 = i32::try_from(ffi::SWS_CS_ITU709).expect("coefficient");
    scaler
        .configure_colorspace(bt709, false, bt709, true)
        .expect("colorspace");
    scaler
}

#[test]
fn alpha_420_to_rgba_writes_every_column_and_nothing_past_the_destination() {
    let widths = (1..=70).chain([250, 500, 854, 1080]);
    for width in widths {
        for height in [2, 15, 16] {
            let mut scaler = rgba_scaler(width, height);
            let tail = mark_destination_tail(&mut scaler);
            let frame = uniform_frame(
                ffi::AV_PIX_FMT_YUVA420P,
                width,
                height,
                [126, 128, 128, 255],
            );
            let bytes = scaler.convert(&frame).expect("convert");
            let (w, h) = (
                usize::try_from(width).expect("w"),
                usize::try_from(height).expect("h"),
            );
            assert_eq!(bytes.len(), 4 * w * h, "{width}x{height}");
            let first = &bytes[..3];
            for (index, pixel) in bytes.as_chunks::<4>().0.iter().enumerate() {
                // The general scaler, which odd heights take, rounds opaque alpha to 254.
                assert!(pixel[3] >= 254, "{width}x{height} pixel {index}: {pixel:?}");
                for channel in 0..3 {
                    assert!(
                        pixel[channel].abs_diff(first[channel]) <= 2,
                        "{width}x{height} pixel {index}: {pixel:?} against {first:?}"
                    );
                }
            }
            assert!(
                tail_is_untouched(tail),
                "{width}x{height} wrote past its planes"
            );
        }
    }
}

#[test]
fn alpha_420_to_planar_444_keeps_alpha_opaque_and_stays_inside_the_destination() {
    for (source, target, opaque) in [
        (ffi::AV_PIX_FMT_YUVA420P, ffi::AV_PIX_FMT_YUVA444P, 255_u16),
        (
            ffi::AV_PIX_FMT_YUVA420P10LE,
            ffi::AV_PIX_FMT_YUVA444P10LE,
            1023,
        ),
    ] {
        let wide = opaque > 255;
        let sample_bytes = if wide { 2 } else { 1 };
        for width in 1..=8 {
            for height in [3, 4, 16] {
                let mut scaler = PackedScaler::new(width, height, source, target).expect("scaler");
                let tail = mark_destination_tail(&mut scaler);
                let mid = if wide { 512 } else { 128 };
                let frame = uniform_frame(source, width, height, [mid, mid, mid, opaque]);
                let bytes = scaler.convert(&frame).expect("convert");
                let plane = usize::try_from(width * height).expect("pixels") * sample_bytes;
                assert_eq!(bytes.len(), 4 * plane, "{width}x{height}");
                let alpha = &bytes[3 * plane..];
                let samples: Vec<u16> = if wide {
                    alpha
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|pair| u16::from_le_bytes(*pair))
                        .collect()
                } else {
                    alpha.iter().map(|byte| u16::from(*byte)).collect()
                };
                assert!(
                    samples.iter().all(|sample| *sample == opaque),
                    "{source:?} {width}x{height}: {samples:?}"
                );
                assert!(
                    tail_is_untouched(tail),
                    "{width}x{height} wrote past its planes"
                );
            }
        }
    }
}

#[test]
fn a_scaler_refuses_a_frame_of_another_size_or_format() {
    let mut scaler = rgba_scaler(64, 64);
    let smaller = uniform_frame(ffi::AV_PIX_FMT_YUVA420P, 16, 16, [126, 128, 128, 255]);
    let other_format = uniform_frame(ffi::AV_PIX_FMT_YUV420P, 64, 64, [126, 128, 128, 0]);
    for frame in [&smaller, &other_format] {
        assert!(matches!(
            scaler.convert(frame),
            Err(BridgeError::InvalidInput(_))
        ));
        assert!(matches!(
            scaler.convert_to_frame(frame),
            Err(BridgeError::InvalidInput(_))
        ));
    }
    let matching = uniform_frame(ffi::AV_PIX_FMT_YUVA420P, 64, 64, [126, 128, 128, 255]);
    let converted = scaler.convert_to_frame(&matching).expect("convert");
    assert_eq!((converted.width, converted.height), (64, 64));
    assert_eq!(converted.format, ffi::AV_PIX_FMT_RGBA);
    assert_eq!(converted.linesize[0] % 64, 0);
}
