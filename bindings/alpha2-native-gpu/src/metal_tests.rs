//! The `Metal` interop on real `IOSurface`-backed pixel buffers.
//!
//! No `VideoToolbox` session is needed: each test makes the `CoreVideo` buffer
//! a decoder or the encoder pool would hand over, with known samples, and
//! checks what the renderer's device sees through the import, or what a
//! render into the encoder planes leaves in the buffer. Each test needs the
//! renderer's `Metal` device; without one it skips, recorded as the `metal`
//! capability.

use std::ffi::c_void;
use std::sync::Arc;

use alpha2_ffmpeg_bridge::VideoToolboxFrameLease;
use rsmpeg::avutil::AVFrame;
use rsmpeg::ffi;

use super::*;

type CFTypeRef = *const c_void;
type CVPixelBufferRef = *mut c_void;

const NV12_VIDEO: u32 = u32::from_be_bytes(*b"420v");
const NV12_FULL: u32 = u32::from_be_bytes(*b"420f");
const BGRA: u32 = u32::from_be_bytes(*b"BGRA");
const K_CF_NUMBER_FLOAT64_TYPE: isize = 6;
const K_CV_ATTACHMENT_MODE_SHOULD_PROPAGATE: u32 = 1;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: u8;
    static kCFTypeDictionaryValueCallBacks: u8;
    fn CFDictionaryCreate(
        allocator: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        key_callbacks: *const u8,
        value_callbacks: *const u8,
    ) -> CFTypeRef;
    fn CFNumberCreate(allocator: CFTypeRef, kind: isize, value: *const c_void) -> CFTypeRef;
    fn CFRelease(object: CFTypeRef);
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferIOSurfacePropertiesKey: CFTypeRef;
    static kCVImageBufferCleanApertureKey: CFTypeRef;
    static kCVImageBufferCleanApertureWidthKey: CFTypeRef;
    static kCVImageBufferCleanApertureHeightKey: CFTypeRef;
    static kCVImageBufferCleanApertureHorizontalOffsetKey: CFTypeRef;
    static kCVImageBufferCleanApertureVerticalOffsetKey: CFTypeRef;
    fn CVPixelBufferCreate(
        allocator: CFTypeRef,
        width: usize,
        height: usize,
        format: u32,
        attributes: CFTypeRef,
        output: *mut CVPixelBufferRef,
    ) -> i32;
    fn CVPixelBufferRelease(buffer: CVPixelBufferRef);
    fn CVPixelBufferLockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddressOfPlane(buffer: CVPixelBufferRef, plane: usize) -> *mut u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
    fn CVBufferSetAttachment(buffer: CVPixelBufferRef, key: CFTypeRef, value: CFTypeRef, mode: u32);
}

/// The renderer's `Metal` device as the host shares it, or `None` (a recorded skip).
fn shared_context() -> Option<SharedMetalContext> {
    shared_context_without(wgpu::Features::empty())
}

/// [`shared_context`] on a device opened without the optional features in `withheld`.
fn shared_context_without(withheld: wgpu::Features) -> Option<SharedMetalContext> {
    alpha2_gpu::validation_log::install_unless_off();
    let made = pollster::block_on(alpha2_gpu::GpuContext::request_product_with(
        &alpha2_gpu::ProductRequest {
            withheld,
            ..alpha2_gpu::ProductRequest::default()
        },
    ))
    .map_err(|error| format!("{error:?}"))
    .and_then(|gpu| SharedMetalContext::from_gpu(gpu).map_err(|error| error.to_string()));
    alpha2_test_support::available!(
        alpha2_test_support::Capability::Metal,
        made.map_err(|error| format!("no Metal device ({error})"))
    )
}

/// An owned `CVPixelBuffer`, released when it drops unless a frame took it.
struct PixelBuffer(CVPixelBufferRef);

impl Drop for PixelBuffer {
    fn drop(&mut self) {
        // SAFETY: the +1 reference from CVPixelBufferCreate, released once.
        unsafe { CVPixelBufferRelease(self.0) };
    }
}

fn dictionary(entries: &[(CFTypeRef, CFTypeRef)]) -> CFTypeRef {
    let (keys, values): (Vec<_>, Vec<_>) = entries.iter().copied().unzip();
    // SAFETY: the arrays hold `count` live CF objects, and the callbacks are CoreFoundation's own.
    unsafe {
        CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            isize::try_from(entries.len()).expect("count"),
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    }
}

fn number(value: f64) -> CFTypeRef {
    // SAFETY: `value` is a live f64 for the call.
    unsafe {
        CFNumberCreate(
            std::ptr::null(),
            K_CF_NUMBER_FLOAT64_TYPE,
            (&raw const value).cast(),
        )
    }
}

/// A new pixel buffer, `IOSurface`-backed when `io_surface` is set, the way a
/// decoder's and the encoder pool's are.
fn pixel_buffer(width: usize, height: usize, format: u32, io_surface: bool) -> PixelBuffer {
    let empty = dictionary(&[]);
    // SAFETY: CoreVideo's exported key.
    let key = unsafe { kCVPixelBufferIOSurfacePropertiesKey };
    let attributes = if io_surface {
        dictionary(&[(key, empty)])
    } else {
        dictionary(&[])
    };
    let mut buffer = std::ptr::null_mut();
    // SAFETY: `attributes` is a live dictionary and `buffer` is writable.
    let status = unsafe {
        CVPixelBufferCreate(
            std::ptr::null(),
            width,
            height,
            format,
            attributes,
            &raw mut buffer,
        )
    };
    // SAFETY: both are +1 references this function made.
    unsafe {
        CFRelease(attributes);
        CFRelease(empty);
    }
    assert_eq!(status, 0, "CVPixelBufferCreate {width}x{height}");
    assert!(!buffer.is_null());
    PixelBuffer(buffer)
}

/// Sets the buffer's clean aperture: `size`, centred, then moved by `offset`.
fn set_clean_aperture(buffer: &PixelBuffer, size: (f64, f64), offset: (f64, f64)) {
    let values = [size.0, size.1, offset.0, offset.1].map(number);
    // SAFETY: CoreVideo's exported keys.
    let keys = unsafe {
        [
            kCVImageBufferCleanApertureWidthKey,
            kCVImageBufferCleanApertureHeightKey,
            kCVImageBufferCleanApertureHorizontalOffsetKey,
            kCVImageBufferCleanApertureVerticalOffsetKey,
        ]
    };
    let aperture = dictionary(&[
        (keys[0], values[0]),
        (keys[1], values[1]),
        (keys[2], values[2]),
        (keys[3], values[3]),
    ]);
    // SAFETY: a live buffer and dictionary; the attachment retains the dictionary, and every +1
    // reference made here is released once.
    unsafe {
        CVBufferSetAttachment(
            buffer.0,
            kCVImageBufferCleanApertureKey,
            aperture,
            K_CV_ATTACHMENT_MODE_SHOULD_PROPAGATE,
        );
        CFRelease(aperture);
        for value in values {
            CFRelease(value);
        }
    }
}

/// Runs `access` on each plane's bytes, rows `bytes per row` apart.
fn with_planes<T>(
    buffer: &PixelBuffer,
    mut access: impl FnMut(usize, &mut [u8], usize) -> T,
) -> Vec<T> {
    // SAFETY: a live buffer; it stays locked until the matching unlock below.
    assert_eq!(unsafe { CVPixelBufferLockBaseAddress(buffer.0, 0) }, 0);
    let mut results = Vec::new();
    for plane in 0..2 {
        // SAFETY: the buffer is locked, so the plane's base address and row length describe its
        // memory, `rows` rows of it.
        let (base, stride) = unsafe {
            (
                CVPixelBufferGetBaseAddressOfPlane(buffer.0, plane),
                CVPixelBufferGetBytesPerRowOfPlane(buffer.0, plane),
            )
        };
        let mut info = MetalFrameInfo::default();
        // SAFETY: a live buffer and a writable output.
        let _ = unsafe { alpha2_native_gpu_metal_frame_info(buffer.0, &raw mut info) };
        let rows = info.plane_height[plane] as usize;
        // SAFETY: as above.
        let bytes = unsafe { std::slice::from_raw_parts_mut(base, stride * rows) };
        results.push(access(plane, bytes, stride));
    }
    // SAFETY: locked above.
    assert_eq!(unsafe { CVPixelBufferUnlockBaseAddress(buffer.0, 0) }, 0);
    results
}

/// The sample code a test writes at `(x, y)` of `plane`, channel `channel`.
fn code(plane: usize, channel: usize, x: usize, y: usize) -> u16 {
    let value = 16 + plane * 61 + channel * 37 + x * 3 + y * 5;
    u16::try_from(value % 220).expect("code") + 16
}

/// Writes `code` into every sample of the buffer: one byte each at 8 bits,
/// left-aligned in a little-endian 16-bit container at 10.
fn fill(buffer: &PixelBuffer, ten_bit: bool) {
    with_planes(buffer, |plane, bytes, stride| {
        let channels = if plane == 0 { 1 } else { 2 };
        let sample = if ten_bit { 2 } else { 1 };
        for (y, row) in bytes.chunks_mut(stride).enumerate() {
            for (index, slot) in row.chunks_mut(sample).enumerate() {
                let (x, channel) = (index / channels, index % channels);
                let value = code(plane, channel, x, y);
                if ten_bit {
                    // The 8-bit code as a 10-bit one, left-aligned.
                    slot.copy_from_slice(&((value * 4) << 6).to_le_bytes()[..slot.len()]);
                } else {
                    slot[0] = u8::try_from(value).expect("8-bit code");
                }
            }
        }
    });
}

/// An `AV_PIX_FMT_VIDEOTOOLBOX` frame holding `buffer`, as a decoder or the
/// encoder pool hands one over.
fn frame(buffer: PixelBuffer, width: i32, height: i32) -> AVFrame {
    unsafe extern "C" fn release(_: *mut c_void, data: *mut u8) {
        // SAFETY: `data` is the pixel buffer whose reference the frame took.
        unsafe { CVPixelBufferRelease(data.cast()) };
    }
    let mut frame = AVFrame::new();
    let raw = frame.as_mut_ptr();
    let pointer = buffer.0;
    std::mem::forget(buffer);
    // SAFETY: a fresh frame; its first buffer now owns the pixel buffer's reference and releases it
    // with the frame.
    unsafe {
        (*raw).format = ffi::AV_PIX_FMT_VIDEOTOOLBOX;
        (*raw).width = width;
        (*raw).height = height;
        (*raw).data[3] = pointer.cast();
        (*raw).buf[0] =
            ffi::av_buffer_create(pointer.cast(), 1, Some(release), std::ptr::null_mut(), 0);
        assert!(!(*raw).buf[0].is_null());
    }
    frame
}

const SAMPLE_SHADER: &str = "
@group(0) @binding(0) var plane: texture_2d<f32>;
@vertex
fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let corner = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return vec4<f32>(corner * 2.0 - 1.0, 0.0, 1.0);
}
@fragment
fn fragment(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return textureLoad(plane, vec2<u32>(position.xy), 0);
}
";

/// A draw that writes each texel of its bound plane to the same texel of a
/// `format` target.
fn sample_pipeline(device: &wgpu::Device, format: wgpu::TextureFormat) -> wgpu::RenderPipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("sample plane"),
        source: wgpu::ShaderSource::Wgsl(SAMPLE_SHADER.into()),
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("sample plane"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vertex"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fragment"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(format.into())],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// Every texel of `view` as the renderer's shaders read it, red and green.
///
/// The renderer's device has no compute limits, so a draw copies each texel
/// into a float target of the plane's size.
fn sample(
    context: &SharedMetalContext,
    view: &wgpu::TextureView,
    size: (u32, u32),
) -> Vec<[f32; 2]> {
    let device = context.gpu.device();
    let format = wgpu::TextureFormat::Rgba32Float;
    let pipeline = sample_pipeline(device, format);
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sampled plane"),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let row = size.0 * 16;
    let padded = row.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let read = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("sampled plane read"),
        size: u64::from(padded * size.1),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("sample plane"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::TextureView(view),
        }],
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("sample plane"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.draw(0..3, 0..1);
    }
    encoder.copy_texture_to_buffer(
        target.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &read,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: None,
            },
        },
        target.size(),
    );
    let submitted = context.gpu.submit([encoder.finish()]);
    context
        .gpu
        .map_async(&read.slice(..), wgpu::MapMode::Read, |result| {
            result.expect("map");
        });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(submitted),
            timeout: Some(std::time::Duration::from_secs(10)),
        })
        .expect("the sampling pass finishes");
    let mapped = read.slice(..).get_mapped_range().expect("mapped");
    mapped
        .chunks(padded as usize)
        .flat_map(|line| line[..row as usize].as_chunks::<16>().0)
        .map(|texel| {
            let channel =
                |at: usize| f32::from_le_bytes(texel[at..at + 4].try_into().expect("f32"));
            [channel(0), channel(4)]
        })
        .collect()
}

/// Runs `test` inside a validation error scope on `context`'s device; any
/// validation error fails it. What `wgpu-hal` logs meanwhile is reported, or
/// fails it, as `ALPHA2_VALIDATION_LOG` says.
fn validated<T>(context: &SharedMetalContext, test: impl FnOnce() -> T) -> T {
    let _turn = alpha2_gpu::validation_log::TestTurn::new(());
    let scope = context
        .gpu
        .device()
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let result = test();
    if let Some(error) = pollster::block_on(scope.pop()) {
        panic!("validation error: {error}");
    }
    result
}

/// Imports a filled buffer of `format` and returns the import.
fn import(
    context: &SharedMetalContext,
    buffer: PixelBuffer,
    (width, height): (i32, i32),
) -> Result<ImportedMetalFrame, BridgeError> {
    context.import_surface(Arc::new(VideoToolboxFrameLease::new(frame(
        buffer, width, height,
    ))))
}

#[test]
fn a_decoded_surface_imports_as_the_planes_it_holds_at_either_depth() {
    let Some(context) = shared_context() else {
        return;
    };
    let (width, height) = (64_u32, 32_u32);
    let mut formats = vec![(NV12_VIDEO, false), (NV12_FULL, false)];
    if context.supports_ten_bit() {
        formats.extend([
            (PIXEL_FORMAT_420_10_VIDEO_RANGE, true),
            (PIXEL_FORMAT_420_10_FULL_RANGE, true),
        ]);
    } else {
        eprintln!("note: this device has no 16-bit norm planes; 10-bit imports are refused");
    }
    for (format, ten_bit) in formats {
        let name = String::from_utf8_lossy(&format.to_be_bytes()).into_owned();
        let buffer = pixel_buffer(width as usize, height as usize, format, true);
        fill(&buffer, ten_bit);
        validated(&context, || {
            let imported = import(&context, buffer, (64, 32))
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            let layout = if ten_bit {
                NativePlaneLayout::P010
            } else {
                NativePlaneLayout::Nv12
            };
            assert_eq!(imported.layout, layout, "{name}");
            assert_eq!(imported.visible_rect, [0, 0, width, height], "{name}");
            assert_eq!(imported.coded_size, [width, height], "{name}");
            // A 10-bit code sits left-aligned in its 16-bit container: code << 8 here, read as a
            // fraction of 65535.
            let full = if ten_bit { 65535.0 / 256.0 } else { 255.0 };
            for (plane, view, size, channels) in [
                (0, &imported.y, (width, height), 1),
                (1, &imported.uv, (width / 2, height / 2), 2),
            ] {
                let texels = sample(&context, view, size);
                for (index, texel) in texels.iter().enumerate() {
                    let (x, y) = (index % size.0 as usize, index / size.0 as usize);
                    for (channel, sampled) in texel.iter().take(channels).enumerate() {
                        let expected = f32::from(code(plane, channel, x, y));
                        let seen = sampled * full;
                        assert!(
                            (seen - expected).abs() < 0.05,
                            "{name} plane {plane} ({x}, {y}) channel {channel}: sampled {seen}, wrote {expected}"
                        );
                    }
                }
            }
        });
    }
}

#[test]
fn a_ten_bit_surface_is_refused_on_a_device_without_sixteen_bit_planes() {
    // Every Apple GPU has 16-bit normalized textures, so the refusal is reached on a device opened
    // without them.
    let Some(context) = shared_context_without(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM) else {
        return;
    };
    assert!(
        !context.supports_ten_bit(),
        "a device without 16-bit textures imports 10-bit planes"
    );
    let buffer = pixel_buffer(64, 32, PIXEL_FORMAT_420_10_VIDEO_RANGE, true);
    let error = import(&context, buffer, (64, 32)).expect_err("refused");
    assert!(error.to_string().contains("16-bit"), "{error}");
}

#[test]
fn only_an_iosurface_backed_four_two_zero_buffer_imports() {
    let Some(context) = shared_context() else {
        return;
    };
    for (name, buffer, described) in [
        (
            "BGRA",
            pixel_buffer(64, 32, BGRA, true),
            "BGRA (64x32, 0 planes) is not an IOSurface-backed 4:2:0 buffer: pixel format is not \
             420v/420f/x420/xf20",
        ),
        (
            "420v without an IOSurface",
            pixel_buffer(64, 32, NV12_VIDEO, false),
            "420v (64x32, 2 planes) is not an IOSurface-backed 4:2:0 buffer: no IOSurface",
        ),
    ] {
        let error = import(&context, buffer, (64, 32)).expect_err(name);
        assert!(error.to_string().contains(described), "{name}: {error}");
    }
    let error = context
        .import_surface(Arc::new(VideoToolboxFrameLease::new(AVFrame::new())))
        .expect_err("a software frame");
    assert_ne!(error.to_string(), "");
}

#[test]
fn the_visible_rectangle_is_the_clean_aperture_inside_the_buffer() {
    let Some(context) = shared_context() else {
        return;
    };
    let (width, height) = (64_u32, 32_u32);
    for (name, size, offset, expected) in [
        ("none", None, (0.0, 0.0), [0, 0, 64, 32]),
        ("centred", Some((60.0, 28.0)), (0.0, 0.0), [2, 2, 60, 28]),
        (
            "moved right",
            Some((60.0, 28.0)),
            (2.0, 0.0),
            [4, 2, 60, 28],
        ),
        // Larger than the buffer: the whole buffer is shown.
        ("too wide", Some((80.0, 28.0)), (0.0, 0.0), [0, 0, 64, 32]),
        (
            "off the right edge",
            Some((60.0, 28.0)),
            (4.0, 0.0),
            [0, 0, 64, 32],
        ),
        // Fractional edges at 0.5 and 64.0: each edge rounds on its own, so the rectangle stays
        // inside the buffer.
        (
            "fractional",
            Some((63.5, 32.0)),
            (0.25, 0.0),
            [1, 0, 63, 32],
        ),
        // Fractional edges at 0.25 and 63.75.
        (
            "fractional inside",
            Some((63.5, 32.0)),
            (0.0, 0.0),
            [0, 0, 64, 32],
        ),
    ] {
        let buffer = pixel_buffer(width as usize, height as usize, NV12_VIDEO, true);
        if let Some(size) = size {
            set_clean_aperture(&buffer, size, offset);
        }
        let imported =
            import(&context, buffer, (64, 32)).unwrap_or_else(|error| panic!("{name}: {error}"));
        let [x, y, visible_width, visible_height] = imported.visible_rect;
        assert!(
            x + visible_width <= width && y + visible_height <= height,
            "{name}: {:?} is outside the {width}x{height} buffer",
            imported.visible_rect
        );
        assert_eq!(imported.visible_rect, expected, "{name}");
    }
}

/// Clears the encoder planes to `luma` and `chroma`, as the NV12 pack writes
/// them, and waits for the render.
fn clear_planes(
    context: &SharedMetalContext,
    planes: &MetalEncoderPlanes,
    luma: f64,
    chroma: (f64, f64),
) {
    let device = context.gpu.device();
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    for (texture, color) in [
        (
            &planes.luma,
            wgpu::Color {
                r: luma,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
        ),
        (
            &planes.chroma,
            wgpu::Color {
                r: chroma.0,
                g: chroma.1,
                b: 0.0,
                a: 1.0,
            },
        ),
    ] {
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("encoder plane"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(color),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
    }
    let submitted = context.gpu.submit([encoder.finish()]);
    device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(submitted),
            timeout: Some(std::time::Duration::from_secs(10)),
        })
        .expect("the render finishes");
}

#[test]
fn a_render_into_the_encoder_planes_is_what_the_pixel_buffer_holds() {
    let Some(context) = shared_context() else {
        return;
    };
    // The second size has an odd-sized chroma plane.
    for (width, height) in [(64_usize, 32_usize), (66, 34)] {
        let buffer = pixel_buffer(width, height, NV12_VIDEO, true);
        let pointer = buffer.0;
        // The frame owns its reference; this one keeps the buffer readable after the frame and
        // planes are gone.
        // SAFETY: a live buffer; the extra reference is released by `kept`.
        let kept = PixelBuffer(unsafe { CVPixelBufferRetain(pointer) });
        let frame = frame(
            buffer,
            i32::try_from(width).expect("w"),
            i32::try_from(height).expect("h"),
        );
        validated(&context, || {
            let planes = context.encoder_planes(&frame).expect("encoder planes");
            let extent = |value: usize| u32::try_from(value).expect("extent");
            assert_eq!(
                (planes.luma.width(), planes.luma.height()),
                (extent(width), extent(height))
            );
            assert_eq!(
                (planes.chroma.width(), planes.chroma.height()),
                (extent(width.div_ceil(2)), extent(height.div_ceil(2)))
            );
            assert_eq!(planes.luma.format(), wgpu::TextureFormat::R8Unorm);
            assert_eq!(planes.chroma.format(), wgpu::TextureFormat::Rg8Unorm);
            clear_planes(
                &context,
                &planes,
                64.0 / 255.0,
                (128.0 / 255.0, 192.0 / 255.0),
            );
        });
        drop(frame);
        let mismatches = with_planes(&kept, |plane, bytes, stride| {
            let (columns, rows, expected): (usize, usize, &[u8]) = if plane == 0 {
                (width, height, &[64])
            } else {
                (width.div_ceil(2), height.div_ceil(2), &[128, 192])
            };
            bytes
                .chunks(stride)
                .take(rows)
                .flat_map(|row| row[..columns * expected.len()].chunks(expected.len()))
                .filter(|sample| *sample != expected)
                .count()
        });
        assert_eq!(
            mismatches,
            [0, 0],
            "{width}x{height}: samples the render did not write"
        );
    }
}

#[test]
fn only_an_iosurface_backed_eight_bit_video_range_buffer_is_an_encoder_target() {
    let Some(context) = shared_context() else {
        return;
    };
    for (name, buffer, status) in [
        ("420f", pixel_buffer(64, 32, NV12_FULL, true), -2),
        ("BGRA", pixel_buffer(64, 32, BGRA, true), -2),
        (
            "x420",
            pixel_buffer(64, 32, PIXEL_FORMAT_420_10_VIDEO_RANGE, true),
            -2,
        ),
        (
            "420v without an IOSurface",
            pixel_buffer(64, 32, NV12_VIDEO, false),
            -4,
        ),
    ] {
        let frame = frame(buffer, 64, 32);
        let error = context.encoder_planes(&frame).expect_err(name);
        assert!(
            error
                .to_string()
                .contains(&format!("{} ({status})", encoder_plane_reason(status))),
            "{name}: {error}"
        );
    }
    let error = context
        .encoder_planes(&AVFrame::new())
        .expect_err("a software frame");
    assert!(error.to_string().starts_with("encoder frame:"), "{error}");
}

/// Every status the two Metal helpers return has its words, and an unknown
/// one says so rather than borrowing another's; the four-character codes
/// print as text, or in hex when they are not printable.
#[test]
fn helper_codes_have_words() {
    let encoder = (-6..=-1).map(encoder_plane_reason).collect::<Vec<_>>();
    assert_eq!(
        encoder,
        [
            "not an Apple-family GPU",
            "Metal returned no texture",
            "no IOSurface",
            "not two planes",
            "not 8-bit video-range NV12",
            "bad argument",
        ]
    );
    let frame = (-4..=-1).map(frame_info_reason).collect::<Vec<_>>();
    assert_eq!(
        frame,
        [
            "no IOSurface",
            "not two planes",
            "pixel format is not 420v/420f/x420/xf20",
            "null argument",
        ]
    );
    assert_eq!(encoder_plane_reason(-7), "unknown refusal");
    assert_eq!(frame_info_reason(-5), "unknown refusal");
    assert_eq!(four_cc_text(NV12_VIDEO), "420v");
    assert_eq!(four_cc_text(PIXEL_FORMAT_420_10_FULL_RANGE), "xf20");
    assert_eq!(four_cc_text(32), "0x00000020");
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferRetain(buffer: CVPixelBufferRef) -> CVPixelBufferRef;
}

/// The shared device's surface imports, as the cross-backend suite drives
/// them.
struct Conformance;

impl crate::conformance::Backend for Conformance {
    type Context = SharedMetalContext;
    type Surface = VideoToolboxFrameLease;
    const BACKEND: alpha2_ffmpeg_bridge::NativeGpuBackend =
        alpha2_ffmpeg_bridge::NativeGpuBackend::Metal;
    // What `CVMetalTextureCacheCreateTextureFromImage`, the import's only platform call, can
    // return: kCVReturnAllocationFailed, and kCVReturnPixelBufferNotMetalCompatible, which names
    // no fault.
    const STATUSES: &'static [(i32, Option<alpha2_ffmpeg_bridge::GpuFault>)] = &[
        (-6662, Some(alpha2_ffmpeg_bridge::GpuFault::DeviceMemory)),
        (-6683, None),
    ];

    fn context() -> Option<SharedMetalContext> {
        shared_context()
    }

    fn surface(_: &SharedMetalContext) -> Arc<VideoToolboxFrameLease> {
        let buffer = pixel_buffer(64, 32, NV12_VIDEO, true);
        fill(&buffer, false);
        Arc::new(VideoToolboxFrameLease::new(frame(buffer, 64, 32)))
    }

    fn fail_next_import(status: i32) {
        // The chroma plane, so the luma texture the import already made must be released too.
        inject::fail_next_import_with(1, status);
    }

    fn import(
        context: &SharedMetalContext,
        surface: Arc<VideoToolboxFrameLease>,
    ) -> Result<(), BridgeError> {
        let imported = context.import_surface(surface)?;
        // A draw that samples both planes, read back, so the submission has ended.
        validated(context, || {
            sample(context, &imported.y, (64, 32));
            sample(context, &imported.uv, (32, 16));
        });
        imported.lease.submitted();
        Ok(())
    }

    fn leaked() -> u64 {
        // An import submits nothing of its own, so what it could leak is a `CoreVideo` texture;
        // the suite runs on one thread, and the imports it lets go of release theirs there.
        inject::live_textures()
    }
}

#[test]
fn the_metal_import_keeps_the_interop_contract() {
    crate::conformance::check::<Conformance>();
}
