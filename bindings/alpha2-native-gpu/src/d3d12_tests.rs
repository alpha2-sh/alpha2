//! The D3D12 interop on real D3D12VA surfaces: decode copies, and the
//! encode handoff of rendered planes into encoder frames.
//!
//! No decode or encode session is needed: each decode test takes a surface
//! from a D3D12VA frames context on the renderer's own device, uploads known
//! samples into it, and checks what the renderer sees through the copy, and
//! when the copy gives the surface back; each encode test renders known
//! samples into the renderer's planes, copies them into a frame of such a
//! pool, and downloads it. Each test needs a D3D12 adapter; without one it
//! skips, recorded as the `d3d12-video` capability.

use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use alpha2_ffmpeg_bridge::{
    D3D12EncodeFrameAccess, D3D12FrameLease, GpuFault, d3d12_frame_fence_value,
};
use rsmpeg::avutil::AVFrame;
use rsmpeg::ffi;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_MESSAGE, D3D12_MESSAGE_SEVERITY_CORRUPTION, D3D12_MESSAGE_SEVERITY_ERROR, ID3D12InfoQueue,
};

use super::*;

/// `AVD3D12VASyncContext` (`libavutil/hwcontext_d3d12va.h`).
#[repr(C)]
struct SyncContext {
    fence: *mut c_void,
    event: *mut c_void,
    fence_value: u64,
}

/// `AVD3D12VAFrame`, what a D3D12VA frame's `data[0]` points to, as `FFmpeg` 8.1 lays it out.
///
/// 8.1 put `subresource_index` before `sync_ctx` without a new major version, so the version
/// guard of the shims cannot catch a layout this copy no longer matches; [`descriptor`] checks it
/// against the bridge's shim, which is compiled against the headers, before a test writes
/// through it.
#[repr(C)]
struct SurfaceDescriptor {
    texture: *mut c_void,
    /// The array slice in a texture-array pool; 0 in the per-frame pools these tests use.
    subresource_index: i32,
    sync: SyncContext,
    /// `AVD3D12VAFrameFlags`.
    flags: i32,
}

/// The renderer's D3D12 device as the host shares it, on the first adapter
/// D3D12VA opens on, or `None` (a recorded skip).
fn shared_context() -> Option<SharedD3D12Context> {
    // Before the device, so the collector watches its debug-layer queue.
    alpha2_gpu::validation_log::install_unless_off();
    let made = pollster::block_on(alpha2_gpu::GpuContext::request_product_accepted(|gpu| {
        SharedD3D12Context::from_gpu(gpu.clone())
    }))
    .map_err(|error| format!("{error:?}"))
    .and_then(|found| found.accepted.ok_or_else(|| found.refusals.to_string()));
    alpha2_test_support::available!(
        alpha2_test_support::Capability::D3d12Video,
        made.map_err(|error| format!("no D3D12 device ({error})"))
    )
}

/// The sample code a test writes at `(x, y)` of `plane`, channel `channel`.
fn code(plane: usize, channel: usize, x: usize, y: usize) -> u16 {
    seeded_code(0, plane, channel, x, y)
}

/// [`code`], shifted by `seed`, so that surfaces of different seeds hold
/// different samples everywhere.
fn seeded_code(seed: usize, plane: usize, channel: usize, x: usize, y: usize) -> u16 {
    let value = 16 + seed * 11 + plane * 61 + channel * 37 + x * 3 + y * 5;
    u16::try_from(value % 220).expect("code") + 16
}

/// A D3D12VA surface of `device`, `software_format` (NV12 or P010) and
/// `width` x `height`, holding `code` in every sample: one byte each at 8
/// bits, left-aligned in a little-endian 16-bit container at 10.
fn surface(device: &AVHWDeviceContext, software_format: i32, width: i32, height: i32) -> AVFrame {
    seeded_surface(device, software_format, width, height, 0)
}

/// [`surface`], holding [`seeded_code`] of `seed`.
fn seeded_surface(
    device: &AVHWDeviceContext,
    software_format: i32,
    width: i32,
    height: i32,
    seed: usize,
) -> AVFrame {
    surface_with(
        device,
        software_format,
        width,
        height,
        |plane, channel, x, y| seeded_code(seed, plane, channel, x, y),
    )
}

/// A D3D12VA surface like [`surface`], holding `code(plane, channel, x, y)` in each sample.
fn surface_with(
    device: &AVHWDeviceContext,
    software_format: i32,
    width: i32,
    height: i32,
    code: impl Fn(usize, usize, usize, usize) -> u16,
) -> AVFrame {
    let mut frames = device.hwframe_ctx_alloc();
    {
        let data = frames.data();
        data.format = ffi::AV_PIX_FMT_D3D12;
        data.sw_format = software_format;
        data.width = width;
        data.height = height;
        data.initial_pool_size = 0;
    }
    frames.init().expect("frames context");
    let mut frame = AVFrame::new();
    frames.get_buffer(&mut frame).expect("surface");
    let ten_bit = software_format == ffi::AV_PIX_FMT_P010LE;
    let mut software = AVFrame::new();
    // SAFETY: a fresh frame, described before its buffers are allocated.
    unsafe {
        let raw = software.as_mut_ptr();
        (*raw).format = software_format;
        (*raw).width = width;
        (*raw).height = height;
    }
    software.alloc_buffer().expect("software frame");
    let sample = if ten_bit { 2 } else { 1 };
    for plane in 0..2 {
        let rows = usize::try_from(if plane == 0 { height } else { height / 2 }).expect("rows");
        let stride = usize::try_from(software.linesize[plane]).expect("stride");
        let channels = if plane == 0 { 1 } else { 2 };
        // SAFETY: the plane's buffer holds `rows` rows of `stride` bytes.
        let bytes = unsafe { std::slice::from_raw_parts_mut(software.data[plane], stride * rows) };
        for (y, row) in bytes.chunks_mut(stride).enumerate() {
            let columns = usize::try_from(width).expect("width") / if plane == 0 { 1 } else { 2 };
            for (index, slot) in row[..columns * channels * sample]
                .chunks_mut(sample)
                .enumerate()
            {
                let value = code(plane, index % channels, index / channels, y);
                if ten_bit {
                    slot.copy_from_slice(&((value * 4) << 6).to_le_bytes());
                } else {
                    slot[0] = u8::try_from(value).expect("8-bit code");
                }
            }
        }
    }
    frame
        .hwframe_transfer_data(&software)
        .expect("the samples upload to the surface");
    frame
}

/// The frame's D3D12VA descriptor, which holds its decode fence.
// `data` is declared as bytes; FFmpeg allocates the descriptor it points to with its own alignment.
#[allow(clippy::cast_ptr_alignment)]
fn descriptor(frame: &mut AVFrame) -> &mut SurfaceDescriptor {
    let linked = alpha2_ffmpeg_bridge::d3d12_frame_info(frame).expect("a D3D12VA frame");
    // SAFETY: a D3D12VA frame's `data[0]` is its `AVD3D12VAFrame`, which lives as long as the
    // frame.
    let descriptor = unsafe { &mut *frame.data[0].cast::<SurfaceDescriptor>() };
    assert_eq!(
        (
            descriptor.texture.addr() as u64,
            descriptor.sync.fence.addr() as u64,
            descriptor.sync.fence_value,
        ),
        (linked.resource, linked.fence, linked.fence_value),
        "SurfaceDescriptor no longer matches the linked FFmpeg's AVD3D12VAFrame"
    );
    descriptor
}

/// The surface's decode fence.
fn decode_fence(frame: &mut AVFrame) -> ID3D12Fence {
    let raw = descriptor(frame).sync.fence;
    // SAFETY: the frame holds the fence; clone adds an independent reference.
    unsafe { ID3D12Fence::from_raw_borrowed(&raw) }
        .expect("decode fence")
        .clone()
}

/// A surface wrapped as the decoder hands it over, and a probe that says
/// whether anything still holds it.
fn leased(frame: AVFrame) -> (Arc<D3D12FrameLease>, Weak<D3D12FrameLease>) {
    let lease = Arc::new(D3D12FrameLease::new(frame));
    let probe = Arc::downgrade(&lease);
    (lease, probe)
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

/// Every texel of `view` as the renderer's shaders read it, red and green,
/// drawn into a float target of the plane's size in a submission that waits
/// on the copy the way the host's sampling does.
#[allow(clippy::too_many_lines)]
fn sample(
    context: &SharedD3D12Context,
    imported: &ImportedD3D12Frame,
    view: &wgpu::TextureView,
    size: (u32, u32),
) -> Vec<[f32; 2]> {
    let device = context.gpu.device();
    let format = wgpu::TextureFormat::Rgba32Float;
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("sample plane"),
        source: wgpu::ShaderSource::Wgsl(SAMPLE_SHADER.into()),
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
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
    });
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
    imported.lease.prepare_submission();
    let submitted = context.gpu.submit([encoder.finish()]);
    imported.lease.submitted();
    context
        .gpu
        .map_async(&read.slice(..), wgpu::MapMode::Read, |result| {
            result.expect("map");
        });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(submitted),
            timeout: Some(Duration::from_secs(10)),
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

/// Fails unless a child test run by name passed and ran exactly that test: `--exact` with a path
/// that matches nothing would run nothing and still succeed.
fn assert_child_ran(output: &std::process::Output) {
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "the child test failed or did not run ({}):\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Runs `test` inside a validation error scope on `context`'s device; any
/// validation error fails it. What the debug layer reports meanwhile is
/// reported, or fails it, as `ALPHA2_VALIDATION_LOG` says.
fn validated<T>(context: &SharedD3D12Context, test: impl FnOnce() -> T) -> T {
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

#[test]
fn a_decode_surface_copies_into_the_planes_it_holds_at_either_depth() {
    let Some(context) = shared_context() else {
        return;
    };
    let (width, height) = (64_u32, 32_u32);
    let mut formats = vec![(ffi::AV_PIX_FMT_NV12, NativePlaneLayout::Nv12)];
    if context.supports_ten_bit() {
        formats.push((ffi::AV_PIX_FMT_P010LE, NativePlaneLayout::P010));
    } else {
        eprintln!("note: this device has no P010 plane views; 10-bit copies are refused");
    }
    for (format, layout) in formats {
        let frame = surface(&context.ffmpeg_device(), format, 64, 32);
        let (lease, probe) = leased(frame);
        validated(&context, || {
            let imported = context
                .copy_surface(lease)
                .unwrap_or_else(|error| panic!("{layout:?}: {error}"));
            assert_eq!(imported.layout, layout);
            assert_eq!((imported.width, imported.height), (width, height));
            // A 10-bit code sits left-aligned in its 16-bit container: code << 8 here, read as a
            // fraction of 65535.
            let full = if layout == NativePlaneLayout::P010 {
                65535.0 / 256.0
            } else {
                255.0
            };
            for (plane, view, size, channels) in [
                (0, &imported.y, (width, height), 1),
                (1, &imported.uv, (width / 2, height / 2), 2),
            ] {
                let texels = sample(&context, &imported, view, size);
                for (index, texel) in texels.iter().enumerate() {
                    let (x, y) = (index % size.0 as usize, index / size.0 as usize);
                    for (channel, sampled) in texel.iter().take(channels).enumerate() {
                        let expected = f32::from(code(plane, channel, x, y));
                        let seen = sampled * full;
                        assert!(
                            (seen - expected).abs() < 0.05,
                            "{layout:?} plane {plane} ({x}, {y}) channel {channel}: sampled {seen}, wrote {expected}"
                        );
                    }
                }
            }
            assert!(
                probe.upgrade().is_some(),
                "the copy holds its surface while it is sampled"
            );
        });
        assert!(
            probe.upgrade().is_none(),
            "{layout:?}: a finished copy gives its surface back to the decoder"
        );
    }
}

#[test]
fn a_stream_of_copies_reuses_its_targets_and_command_lists_and_shows_each_surface() {
    let Some(context) = shared_context() else {
        return;
    };
    let device = context.ffmpeg_device();
    let copy = |width: i32, height: i32, seed: usize| {
        let frame = seeded_surface(&device, ffi::AV_PIX_FMT_NV12, width, height, seed);
        context
            .copy_surface(Arc::new(D3D12FrameLease::new(frame)))
            .expect("copy")
    };
    // [targets made, command slots made, idle targets, idle slots]
    let counts = || context.copy_pool_counts();
    validated(&context, || {
        for seed in 0..4 {
            let imported = copy(64, 32, seed);
            // A reused target holds this surface's samples, not the last one's.
            let texels = sample(&context, &imported, &imported.y, (64, 32));
            for (index, texel) in texels.iter().enumerate() {
                let (x, y) = (index % 64, index / 64);
                let expected = f32::from(seeded_code(seed, 0, 0, x, y));
                assert!(
                    (texel[0] * 255.0 - expected).abs() < 0.05,
                    "copy {seed} ({x}, {y}): sampled {}, wrote {expected}",
                    texel[0] * 255.0
                );
            }
        }
    });
    assert_eq!(
        counts(),
        [1, 1, 1, 1],
        "one copy at a time: one target, one slot"
    );
    let held: Vec<_> = (0..3).map(|seed| copy(64, 32, seed)).collect();
    assert_eq!(counts(), [3, 3, 0, 0]);
    drop(held);
    assert_eq!(counts(), [3, 3, 3, 3]);
    // A surface of another size gets a target of its own; the pool keeps four idle at most.
    drop(copy(32, 16, 0));
    assert_eq!(counts(), [4, 3, 4, 3]);
    // A full pool makes room by freeing its least recently given back target, so a stream of the
    // new size reuses one target from its second frame on.
    for _ in 0..3 {
        drop(copy(128, 64, 0));
        assert_eq!(counts(), [5, 3, 4, 3]);
    }
    drop(copy(32, 16, 0));
    assert_eq!(counts(), [5, 3, 4, 3], "the other size stayed idle");
    // Trimming frees every idle target: 64x32, 32x16 and 128x64 NV12 at 1.5 bytes a pixel.
    assert_eq!(
        context.release_idle_copies(),
        (64 * 32 * 2 + 32 * 16 + 128 * 64) * 3 / 2
    );
    assert_eq!(counts(), [5, 3, 0, 3]);
    drop(copy(64, 32, 0));
    assert_eq!(
        counts(),
        [6, 3, 1, 3],
        "a copy after the trim makes its target again"
    );
    // A copy may outlive the context it was made by; the pool goes with the last of them.
    let imported = copy(64, 32, 0);
    drop(context);
    drop(imported);
}

#[test]
fn zero_blocks_of_a_decoded_frame_read_as_zero_in_fresh_and_reused_targets() {
    let Some(context) = shared_context() else {
        return;
    };
    let device = context.ffmpeg_device();
    let (width, height) = (64_usize, 32_usize);
    let mut formats = vec![(ffi::AV_PIX_FMT_NV12, NativePlaneLayout::Nv12)];
    if context.supports_ten_bit() {
        formats.push((ffi::AV_PIX_FMT_P010LE, NativePlaneLayout::P010));
    }
    // Luma is zero in alternating 8x8 blocks (64-byte rows at 8 bits) and seeded elsewhere;
    // chroma is 128, full-range black.
    let zero_block = |x: usize, y: usize| (x / 8 + y / 8).is_multiple_of(2);
    let blocks = |plane: usize, channel: usize, x: usize, y: usize| match plane {
        0 if zero_block(x, y) => 0,
        0 => seeded_code(7, plane, channel, x, y),
        _ => 128,
    };
    for (format, layout) in formats {
        let full = if layout == NativePlaneLayout::P010 {
            65535.0 / 256.0
        } else {
            255.0
        };
        let check = |frame: AVFrame, what: &str| {
            let imported = context
                .copy_surface(Arc::new(D3D12FrameLease::new(frame)))
                .unwrap_or_else(|error| panic!("{layout:?} {what}: {error}"));
            for (plane, view, size, channels) in [
                (0, &imported.y, (width, height), 1),
                (1, &imported.uv, (width / 2, height / 2), 2),
            ] {
                let size32 = (
                    u32::try_from(size.0).expect("width"),
                    u32::try_from(size.1).expect("height"),
                );
                let texels = sample(&context, &imported, view, size32);
                for (index, texel) in texels.iter().enumerate() {
                    let (x, y) = (index % size.0, index / size.0);
                    for (channel, sampled) in texel.iter().take(channels).enumerate() {
                        let expected = f32::from(blocks(plane, channel, x, y));
                        let seen = sampled * full;
                        let close = if expected == 0.0 {
                            seen == 0.0
                        } else {
                            (seen - expected).abs() < 0.05
                        };
                        assert!(
                            close,
                            "{layout:?} {what}: plane {plane} ({x}, {y}) channel {channel}: \
                             sampled {seen}, wrote {expected}"
                        );
                    }
                }
            }
        };
        validated(&context, || {
            // A bright surface first, then its target freed, so the next target can land on
            // memory that holds its samples.
            let bright = surface_with(&device, format, 64, 32, |_, _, _, _| 235);
            drop(
                context
                    .copy_surface(Arc::new(D3D12FrameLease::new(bright)))
                    .expect("bright copy"),
            );
            let _ = context.release_idle_copies();
            check(
                surface_with(&device, format, 64, 32, blocks),
                "fresh target",
            );
            check(
                surface_with(&device, format, 64, 32, blocks),
                "reused target",
            );
        });
    }
}

#[test]
// FFmpeg's contexts sit behind byte pointers, allocated with their own alignment.
#[allow(clippy::cast_ptr_alignment)]
fn a_surface_of_another_device_or_a_mismatched_format_is_refused() {
    let Some(context) = shared_context() else {
        return;
    };
    let device = context.ffmpeg_device();
    // The checks read only the frames context's device and software format, so another device or
    // format is stood in for by relabelling them, restored before the surface is freed.
    let frame = surface(&device, ffi::AV_PIX_FMT_NV12, 64, 32);
    // SAFETY: a D3D12VA frame's frames context, alive while the frame is.
    let frames = unsafe { &mut *(*frame.hw_frames_ctx).data.cast::<ffi::AVHWFramesContext>() };
    // SAFETY: its device context is alive while the frames context is; a D3D12VA device's `hwctx`
    // begins with its `ID3D12Device`.
    let device_slot = unsafe {
        &mut *(*(*frames.device_ref).data.cast::<ffi::AVHWDeviceContext>())
            .hwctx
            .cast::<*mut c_void>()
    };
    let own_device = *device_slot;
    let own_format = frames.sw_format;
    let mut refusals = Vec::new();
    for (name, other_device, other_format) in [
        (
            "another device",
            Some(own_device.cast::<u8>().wrapping_add(16).cast::<c_void>()),
            None,
        ),
        ("NV12 labelled P010", None, Some(ffi::AV_PIX_FMT_P010LE)),
        ("NV12 labelled I420", None, Some(ffi::AV_PIX_FMT_YUV420P)),
    ] {
        if let Some(other) = other_device {
            *device_slot = other;
        }
        if let Some(other) = other_format {
            frames.sw_format = other;
        }
        // A clone shares the surface, so the relabelled frame is the one under test.
        let (lease, probe) = leased(frame.clone());
        let result = context.copy_surface(lease);
        *device_slot = own_device;
        frames.sw_format = own_format;
        refusals.push((
            name,
            result.map(|_| ()).map_err(|error| match error {
                BridgeError::InvalidInput(message) => message.to_owned(),
                other => other.to_string(),
            }),
        ));
        assert!(
            probe.upgrade().is_none(),
            "{name}: the refusal keeps nothing"
        );
    }
    assert_eq!(
        refusals,
        [
            (
                "another device",
                Err("D3D12 frame device mismatch".to_owned())
            ),
            (
                "NV12 labelled P010",
                Err("D3D12 frame is not NV12 or P010".to_owned())
            ),
            (
                "NV12 labelled I420",
                Err("D3D12 frame is not NV12 or P010".to_owned())
            ),
        ]
    );
    let error = context
        .copy_surface(Arc::new(D3D12FrameLease::new(AVFrame::new())))
        .expect_err("a software frame");
    assert_ne!(error.to_string(), "");
    drop(frame);
}

#[test]
fn a_ten_bit_surface_is_refused_on_a_device_without_p010_plane_views() {
    // A device opened without P010 (and the 16-bit normalized textures its planes are viewed as)
    // has no P010 plane views on any adapter, so the refusal runs everywhere D3D12VA does. wgpu
    // grants P010 on its own, so withholding the 16-bit formats alone is not enough.
    let made = pollster::block_on(async {
        let mut refusals = Vec::new();
        for (_, adapter) in
            alpha2_gpu::GpuContext::adapters_by_preference_on(wgpu::Backends::DX12).await
        {
            if adapter.get_info().device_type == wgpu::DeviceType::Cpu {
                continue;
            }
            let name = adapter.get_info().name;
            match alpha2_gpu::GpuContext::open_adapter_without(
                adapter,
                wgpu::Features::TEXTURE_FORMAT_16BIT_NORM | wgpu::Features::TEXTURE_FORMAT_P010,
            )
            .await
            {
                Ok(gpu) => match SharedD3D12Context::from_gpu(gpu) {
                    Ok(context) => return Ok(context),
                    Err(error) => refusals.push(format!("{name}: {error}")),
                },
                Err(error) => refusals.push(format!("{name}: {error:?}")),
            }
        }
        Err(format!(
            "no D3D12 device without P010 ({})",
            refusals.join("; ")
        ))
    });
    let Some(context) =
        alpha2_test_support::available!(alpha2_test_support::Capability::D3d12Video, made)
    else {
        return;
    };
    assert!(
        !context.supports_ten_bit(),
        "a device opened without P010 has P010 views"
    );
    let frame = surface(&context.ffmpeg_device(), ffi::AV_PIX_FMT_P010LE, 64, 32);
    let error = context
        .copy_surface(Arc::new(D3D12FrameLease::new(frame)))
        .expect_err("refused");
    assert!(error.to_string().contains("P010 plane views"), "{error}");
}

#[test]
fn a_copy_that_may_still_be_on_the_gpu_keeps_its_surface_from_the_decoder() {
    let Some(context) = shared_context() else {
        return;
    };
    // A copy that failed after it was submitted with no fence (the queue signal) may read the
    // surface for as long as it runs; one that failed before it was submitted reads nothing.
    for (step, kept) in [
        (COPY_SUBMITTED_UNFENCED, true),
        (crate::interop::ALPHA2_GPU_STEP_CREATE_RESOURCE, false),
    ] {
        let (lease, probe) = leased(surface(
            &context.ffmpeg_device(),
            ffi::AV_PIX_FMT_NV12,
            64,
            32,
        ));
        inject::fail_next_copy(step);
        let error = context.copy_surface(lease).expect_err("the copy fails");
        assert_eq!(error.gpu_fault(), Some(GpuFault::DeviceLost), "{error}");
        assert_eq!(
            probe.upgrade().is_some(),
            kept,
            "step {step}: {error}; the surface goes back only when no copy can be reading it"
        );
    }
}

#[test]
fn a_dropped_copy_takes_its_staged_wait_off_the_queue() {
    let Some(context) = shared_context() else {
        return;
    };
    let queue_has_wait = |fence: &ID3D12Fence| {
        // SAFETY: the context's queue is D3D12.
        let queue = unsafe { context.gpu.queue().as_hal::<wgpu_hal::api::Dx12>() }.expect("D3D12");
        let found = queue.remove_wait_fence(fence);
        if found {
            // Put it back; this only looked.
            queue.add_wait_fence(fence.clone(), 1);
        }
        found
    };
    let frame = surface(&context.ffmpeg_device(), ffi::AV_PIX_FMT_NV12, 64, 32);
    let imported = context
        .copy_surface(Arc::new(D3D12FrameLease::new(frame)))
        .expect("copy");
    let fence = imported.lease.fence.clone();
    imported.lease.prepare_submission();
    assert!(
        queue_has_wait(&fence),
        "staging puts the copy's wait on the queue"
    );
    // A sampling submission that never happened: its wait must not hold up the next one.
    drop(imported);
    assert!(
        !queue_has_wait(&fence),
        "a copy dropped before its sampling submission leaves no wait behind"
    );
    let device = context.gpu.device();
    let encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    let submitted = context.gpu.submit([encoder.finish()]);
    device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(submitted),
            timeout: Some(Duration::from_secs(10)),
        })
        .expect("the next submission finishes");
}

#[test]
fn a_copy_that_does_not_finish_in_time_keeps_its_surface_from_the_decoder() {
    // A child isolates its short wait timeout from concurrently running tests.
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "d3d12::tests::copy_timeout_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .output()
        .expect("child");
    assert_child_ran(&output);
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "run by a_copy_that_does_not_finish_in_time_keeps_its_surface_from_the_decoder"]
fn copy_timeout_child() {
    let Some(context) = shared_context() else {
        return;
    };
    set_wait_timeout(Duration::from_millis(1));
    for timeout in [false, true] {
        let mut frame = surface(&context.ffmpeg_device(), ffi::AV_PIX_FMT_NV12, 64, 32);
        // A surface the decoder has not finished: the copy waits on the GPU for a decode fence
        // value nothing has signalled, so it cannot end within the 1 ms wait.
        let decode = decode_fence(&mut frame);
        let mut blocked = None;
        if timeout {
            // SAFETY: a live fence.
            let value = unsafe { decode.GetCompletedValue() } + 1000;
            descriptor(&mut frame).sync.fence_value = value;
            blocked = Some(value);
        }
        let (lease, probe) = leased(frame);
        let imported = context.copy_surface(lease).expect("the copy submits");
        let copy_fence = imported.lease.fence.clone();
        let copy_value = imported.lease.fence_value;
        if blocked.is_none() {
            // Over before the 1 ms wait starts, whatever else the GPU is running.
            // SAFETY: a live fence.
            unsafe { copy_fence.SetEventOnCompletion(copy_value, HANDLE::default()) }
                .expect("the copy finishes");
        }
        let started = Instant::now();
        drop(imported);
        assert!(started.elapsed() < Duration::from_secs(2));
        let Some(value) = blocked else {
            assert!(
                probe.upgrade().is_none(),
                "a finished copy gives its surface back"
            );
            continue;
        };
        assert!(
            probe.upgrade().is_some(),
            "a copy not known to be over keeps its surface from the decoder"
        );
        // Let the copy run, so the device ends with nothing outstanding.
        // SAFETY: both fences are live.
        unsafe {
            decode.Signal(value).expect("signal");
            copy_fence
                .SetEventOnCompletion(copy_value, HANDLE::default())
                .expect("the copy finishes");
        }
    }
}

#[test]
fn no_wait_timeout_reaches_the_d3d12_waits_as_infinite() {
    // `ALPHA2_GPU_TIMEOUT_MS=4294967295` is `INFINITE` in milliseconds: the conversion stops one
    // short, and the C helper itself refuses the value from any caller. A child, so the timeout it
    // stores holds up no other test's wait.
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "d3d12::tests::wait_timeout_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .output()
        .expect("child");
    assert_child_ran(&output);
    assert!(
        output.status.success(),
        "{:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "run by no_wait_timeout_reaches_the_d3d12_waits_as_infinite"]
fn wait_timeout_child() {
    const INFINITE: u32 = u32::MAX;
    // SAFETY: reads the one integer the setters store.
    let stored = || unsafe { super::alpha2_native_gpu_d3d12_wait_timeout() };
    assert_eq!(stored(), 30_000, "the default");
    crate::set_gpu_wait_timeout(Duration::from_millis(u64::from(INFINITE)));
    assert_eq!(stored(), INFINITE - 1);
    // SAFETY: stores one integer.
    unsafe { super::alpha2_native_gpu_d3d12_set_wait_timeout(INFINITE) };
    assert_eq!(stored(), INFINITE - 1, "the C helper clamps on its own");
    set_wait_timeout(Duration::from_millis(1));
    assert_eq!(stored(), 1);
}

#[test]
fn a_d3d12va_context_gives_back_the_device_reference_it_took() {
    // A child, because D3D12 makes one device per adapter in a process: every other test running
    // here shares the count this reads.
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "d3d12::tests::device_reference_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .output()
        .expect("child");
    assert_child_ran(&output);
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "{:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "run by a_d3d12va_context_gives_back_the_device_reference_it_took"]
fn device_reference_child() {
    let Some(context) = shared_context() else {
        return;
    };
    let gpu = context.gpu();
    drop(context);
    // SAFETY: the context is D3D12; the clone is an independent reference.
    let device = unsafe { gpu.device().as_hal::<wgpu_hal::api::Dx12>() }
        .expect("D3D12")
        .raw_device()
        .clone();
    let references = || {
        let raw = device.as_raw();
        // SAFETY: a live COM object, whose first field points to its `IUnknown` vtable; the
        // reference taken is given back at once.
        unsafe {
            let vtable = &**raw.cast::<*const windows::core::IUnknown_Vtbl>();
            (vtable.AddRef)(raw);
            (vtable.Release)(raw)
        }
    };
    let before = references();
    let context = SharedD3D12Context::from_gpu(gpu.clone()).expect("D3D12VA context");
    // Three: the shim's own, `FFmpeg`'s video device (an interface of the same object), and the
    // copy pool's.
    assert_eq!(references(), before + 3, "the context holds the device");
    // A frames context and a surface of it, as a decoder makes them, hold the device context.
    let frame = surface(&context.ffmpeg_device(), ffi::AV_PIX_FMT_NV12, 64, 32);
    drop(context);
    drop(frame);
    assert_eq!(
        references(),
        before,
        "freeing the context gives back the reference it took"
    );
}

#[test]
fn a_decoder_surface_outlives_the_renderer_device_it_was_made_on() {
    // A child, because the first context a process makes keeps the D3D12 runtime loaded for the
    // rest of it: in this process an earlier test may already have.
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "d3d12::tests::renderer_released_first_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .output()
        .expect("child");
    assert_child_ran(&output);
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "{:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "run by a_decoder_surface_outlives_the_renderer_device_it_was_made_on"]
fn renderer_released_first_child() {
    let Some(context) = shared_context() else {
        return;
    };
    // A decoder's frames, and so its device, may be released after the renderer that made the
    // device: an export's decode worker ends after its renderer. wgpu unloads the D3D12 runtime
    // with its last device, so unless the device context pins it, the frame's resources would be
    // released through unloaded code.
    let frame = surface(&context.ffmpeg_device(), ffi::AV_PIX_FMT_NV12, 64, 32);
    drop(context);
    drop(frame);
}

/// The shared device's surface copies, as the cross-backend suite drives
/// them.
struct Conformance;

impl crate::conformance::Backend for Conformance {
    type Context = SharedD3D12Context;
    type Surface = D3D12FrameLease;
    const BACKEND: alpha2_ffmpeg_bridge::NativeGpuBackend =
        alpha2_ffmpeg_bridge::NativeGpuBackend::D3D12;
    // What `CreateCommittedResource`, the copy's call that makes a target, can return;
    // E_INVALIDARG names no fault.
    const STATUSES: &'static [(i32, Option<GpuFault>)] = &[
        (0x887A_0005_u32.cast_signed(), Some(GpuFault::DeviceLost)),
        (0x8007_000E_u32.cast_signed(), Some(GpuFault::DeviceMemory)),
        (0x8007_0057_u32.cast_signed(), None),
    ];

    fn context() -> Option<SharedD3D12Context> {
        shared_context()
    }

    fn surface(context: &SharedD3D12Context) -> Arc<D3D12FrameLease> {
        let frame = surface(&context.ffmpeg_device(), ffi::AV_PIX_FMT_NV12, 64, 32);
        Arc::new(D3D12FrameLease::new(frame))
    }

    fn fail_next_import(status: i32) {
        inject::fail_next_copy_with(crate::interop::ALPHA2_GPU_STEP_CREATE_RESOURCE, status);
    }

    fn import(
        context: &SharedD3D12Context,
        surface: Arc<D3D12FrameLease>,
    ) -> Result<(), BridgeError> {
        let imported = context.copy_surface(surface)?;
        imported.lease.prepare_submission();
        let submitted = context.gpu.submit([]);
        imported.lease.submitted();
        context
            .gpu
            .device()
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submitted),
                timeout: Some(Duration::from_secs(5)),
            })
            .expect("the sampling submission finishes");
        Ok(())
    }

    fn leaked() -> u64 {
        // The copy helpers leak only a copy whose end a wait could not show, which no import here
        // leaves: a dropped copy that is over gives its surface back, as the suite checks.
        0
    }
}

#[test]
fn the_d3d12_copy_keeps_the_interop_contract() {
    crate::conformance::check::<Conformance>();
}

/// What a test renders into plane `plane` (0 luma, 1 chroma) of an NV12
/// pair: [`seeded_code`] of `seed`, as a unorm fraction, one channel for
/// luma and two for chroma.
fn plane_shader(plane: u32, seed: u32) -> String {
    format!(
        "
@vertex
fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {{
    let corner = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return vec4<f32>(corner * 2.0 - 1.0, 0.0, 1.0);
}}
fn code(channel: u32, x: u32, y: u32) -> f32 {{
    let value = 16u + {seed}u * 11u + {plane}u * 61u + channel * 37u + x * 3u + y * 5u;
    return f32(value % 220u + 16u) / 255.0;
}}
@fragment
fn fragment(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {{
    let x = u32(position.x);
    let y = u32(position.y);
    return vec4<f32>(code(0u, x, y), code(1u, x, y), 0.0, 1.0);
}}
"
    )
}

/// An R8 luma and an RG8 chroma plane of an NV12 picture of `size`, made as
/// the renderer's pack makes them.
fn plane_textures(
    context: &SharedD3D12Context,
    size: (u32, u32),
) -> (wgpu::Texture, wgpu::Texture) {
    let make = |format, width, height| {
        context
            .gpu
            .device()
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("encode plane"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
    };
    (
        make(wgpu::TextureFormat::R8Unorm, size.0, size.1),
        make(wgpu::TextureFormat::Rg8Unorm, size.0 / 2, size.1 / 2),
    )
}

/// Renders [`seeded_code`] of `seed` into both planes, as the pack does, and
/// submits it.
fn render_planes(
    context: &SharedD3D12Context,
    luma: &wgpu::Texture,
    chroma: &wgpu::Texture,
    seed: u32,
) -> wgpu::SubmissionIndex {
    let device = context.gpu.device();
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    for (plane, texture) in [(0, luma), (1, chroma)] {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("encode plane"),
            source: wgpu::ShaderSource::Wgsl(plane_shader(plane, seed).into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("encode plane"),
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
                targets: &[Some(texture.format().into())],
            }),
            multiview_mask: None,
            cache: None,
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("encode plane"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
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
        pass.draw(0..3, 0..1);
    }
    context.gpu.submit([encoder.finish()])
}

/// The bytes of `texture` as wgpu reads them back, rows packed.
fn read_texture(context: &SharedD3D12Context, texture: &wgpu::Texture, texel: u32) -> Vec<u8> {
    let device = context.gpu.device();
    let row = texture.width() * texel;
    let padded = row.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let read = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("encode plane read"),
        size: u64::from(padded * texture.height()),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &read,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: None,
            },
        },
        texture.size(),
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
            timeout: Some(Duration::from_secs(10)),
        })
        .expect("the read finishes");
    let mapped = read.slice(..).get_mapped_range().expect("mapped");
    mapped
        .chunks(padded as usize)
        .flat_map(|line| line[..row as usize].iter().copied())
        .collect()
}

/// A frame of a D3D12VA NV12 pool of `size` on the context's device, as an
/// encoder's input pool hands one out: its texture in COMMON, its fence at
/// whatever the pool left.
fn encode_frame(context: &SharedD3D12Context, size: (u32, u32)) -> AVFrame {
    let mut frames = context.ffmpeg_device().hwframe_ctx_alloc();
    {
        let data = frames.data();
        data.format = ffi::AV_PIX_FMT_D3D12;
        data.sw_format = ffi::AV_PIX_FMT_NV12;
        data.width = i32::try_from(size.0).expect("width");
        data.height = i32::try_from(size.1).expect("height");
        data.initial_pool_size = 0;
    }
    frames.init().expect("frames context");
    let mut frame = AVFrame::new();
    frames.get_buffer(&mut frame).expect("pool frame");
    frame
}

/// The frame's samples on the CPU. `FFmpeg`'s download waits on the GPU for
/// the frame's fence value first, as its encoder does.
fn download(frame: &AVFrame) -> AVFrame {
    let mut software = AVFrame::new();
    software
        .hwframe_transfer_data(frame)
        .expect("the frame downloads");
    software
}

/// Asserts that every sample of an NV12 `frame` of `size` holds
/// [`seeded_code`] of `seed`.
fn assert_planes(frame: &AVFrame, size: (u32, u32), seed: usize, what: &str) {
    let (width, height) = (size.0 as usize, size.1 as usize);
    for plane in 0..2 {
        let (columns, rows, channels) = if plane == 0 {
            (width, height, 1)
        } else {
            (width / 2, height / 2, 2)
        };
        let stride = usize::try_from(frame.linesize[plane]).expect("stride");
        // SAFETY: the plane's buffer holds `rows` rows of `stride` bytes.
        let bytes = unsafe { std::slice::from_raw_parts(frame.data[plane], stride * rows) };
        for y in 0..rows {
            for x in 0..columns {
                for channel in 0..channels {
                    let sample = bytes[y * stride + x * channels + channel];
                    let wrote = seeded_code(seed, plane, channel, x, y);
                    assert_eq!(
                        u16::from(sample),
                        wrote,
                        "{what}: plane {plane} ({x}, {y}) channel {channel}"
                    );
                }
            }
        }
    }
}

/// Asserts [`seeded_code`] of `seed` in R8 or RG8 plane bytes read back.
fn assert_rendered(bytes: &[u8], width: usize, plane: usize, seed: usize, what: &str) {
    let channels = plane + 1;
    for (index, &seen) in bytes.iter().enumerate() {
        let (texel, channel) = (index / channels, index % channels);
        let (x, y) = (texel % width, texel / width);
        assert_eq!(
            u16::from(seen),
            seeded_code(seed, plane, channel, x, y),
            "{what}: plane {plane} ({x}, {y}) channel {channel}"
        );
    }
}

/// The frame's fence, which the copy waits on and signals.
fn frame_fence(frame: &AVFrame) -> ID3D12Fence {
    let info = alpha2_ffmpeg_bridge::d3d12_frame_info(frame).expect("a D3D12VA frame");
    let raw = raw_pointer(info.fence, "fence").expect("fence pointer");
    // SAFETY: the frame holds the fence; clone adds an independent reference.
    unsafe { ID3D12Fence::from_raw_borrowed(&raw) }
        .expect("frame fence")
        .clone()
}

/// Whether `fence` reaches `value` within `timeout`.
fn reaches(fence: &ID3D12Fence, value: u64, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    // SAFETY: a live fence.
    while unsafe { fence.GetCompletedValue() } < value {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

/// How many references the frame's pool buffer has: 1 when only the frame
/// holds it, more while anything else keeps it from the pool.
fn pool_references(frame: &AVFrame) -> i32 {
    // SAFETY: a pool frame's first buffer is live while the frame is.
    unsafe { ffi::av_buffer_get_ref_count(frame.buf[0]) }
}

/// Renders seed `seed` into the pair, copies it into `frame` through
/// `copier`, and returns the value the copy signals.
fn copy_rendered(
    context: &SharedD3D12Context,
    copier: &mut D3D12EncodeCopier,
    planes: &(wgpu::Texture, wgpu::Texture),
    frame: &mut AVFrame,
    size: (u32, u32),
    seed: u32,
) -> Result<u64, BridgeError> {
    copier.await_slot()?;
    render_planes(context, &planes.0, &planes.1, seed);
    let mut access = D3D12EncodeFrameAccess::begin(frame)?;
    copier.submit(&mut access, &planes.0, &planes.1, size)?;
    Ok(access.info().fence_value)
}

#[test]
fn rendered_planes_copy_into_an_encoder_frame_exactly() {
    let Some(context) = shared_context() else {
        return;
    };
    // An output of whole macroblocks, and one of an odd number of them, whose chroma rows and
    // columns are odd too.
    for size in [(256, 192), (158, 62)] {
        let mut copier = D3D12EncodeCopier::new(&context).expect("copier");
        let planes = plane_textures(&context, size);
        let mut frame = encode_frame(&context, size);
        let before = d3d12_frame_fence_value(&frame).expect("fence value");
        let signalled = validated(&context, || {
            copy_rendered(&context, &mut copier, &planes, &mut frame, size, 0)
                .expect("the copy submits")
        });
        assert_eq!(signalled, before + 1);
        assert_eq!(
            d3d12_frame_fence_value(&frame).expect("fence value"),
            before + 1,
            "the encoder waits for exactly the value the copy signals"
        );
        assert_planes(&download(&frame), size, 0, &format!("{size:?}"));
        copier.finish().expect("no copy is left waiting");
        assert_eq!(pool_references(&frame), 1, "the copier gave the frame back");
    }
}

#[test]
fn a_stream_of_encode_copies_cycles_its_slots_and_keeps_each_frame() {
    let Some(context) = shared_context() else {
        return;
    };
    let size = (64, 32);
    let mut copier = D3D12EncodeCopier::new(&context).expect("copier");
    assert_eq!(D3D12EncodeCopier::slots(), 4);
    let pairs: Vec<_> = (0..D3D12EncodeCopier::slots())
        .map(|_| plane_textures(&context, size))
        .collect();
    let mut frames: Vec<_> = (0..10).map(|_| encode_frame(&context, size)).collect();
    validated(&context, || {
        for (index, frame) in frames.iter_mut().enumerate() {
            let slot = copier.await_slot().expect("slot");
            assert_eq!(slot, index % 4, "the slots go round in order");
            let pair = &pairs[slot];
            render_planes(
                &context,
                &pair.0,
                &pair.1,
                u32::try_from(index).expect("seed"),
            );
            let mut access = D3D12EncodeFrameAccess::begin(frame).expect("frame access");
            copier
                .submit(&mut access, &pair.0, &pair.1, size)
                .expect("the copy submits");
        }
    });
    // The copier holds each of its last four frames until its slot comes round again.
    let held: Vec<_> = frames.iter().map(pool_references).collect();
    assert_eq!(
        held,
        [1, 1, 1, 1, 1, 1, 2, 2, 2, 2],
        "frames held by the copier"
    );
    for (seed, frame) in frames.iter().enumerate() {
        assert_planes(&download(frame), size, seed, &format!("frame {seed}"));
    }
    copier.finish().expect("no copy is left waiting");
    assert!(frames.iter().all(|frame| pool_references(frame) == 1));
}

#[test]
fn an_encode_frame_of_another_size_is_refused_before_any_copy() {
    let Some(context) = shared_context() else {
        return;
    };
    let mut copier = D3D12EncodeCopier::new(&context).expect("copier");
    let planes = plane_textures(&context, (64, 32));
    // A frame smaller than the planes, and an odd size, are refused by the helper's own checks.
    let mut small = encode_frame(&context, (32, 16));
    let before = d3d12_frame_fence_value(&small).expect("fence value");
    {
        let mut access = D3D12EncodeFrameAccess::begin(&mut small).expect("frame access");
        let error = copier
            .submit(&mut access, &planes.0, &planes.1, (64, 32))
            .expect_err("a frame smaller than the copy");
        assert!(matches!(error, BridgeError::InvalidInput(_)), "{error}");
        let error = copier
            .submit(&mut access, &planes.0, &planes.1, (31, 16))
            .expect_err("an odd size");
        assert!(matches!(error, BridgeError::InvalidInput(_)), "{error}");
    }
    assert_eq!(
        d3d12_frame_fence_value(&small).expect("fence value"),
        before
    );
    assert_eq!(pool_references(&small), 1, "a refusal keeps nothing");
    let mut frame = encode_frame(&context, (64, 32));
    copy_rendered(&context, &mut copier, &planes, &mut frame, (64, 32), 3).expect("copy");
    assert_planes(&download(&frame), (64, 32), 3, "after the refusals");
    copier.finish().expect("no copy is left waiting");
}

#[test]
fn a_copy_waits_for_the_frame_fence() {
    let Some(context) = shared_context() else {
        return;
    };
    let size = (64, 32);
    let mut copier = D3D12EncodeCopier::new(&context).expect("copier");
    let planes = plane_textures(&context, size);
    let mut frame = encode_frame(&context, size);
    let fence = frame_fence(&frame);
    // The frame as an encode that has not finished reading it leaves it: its fence waits for a
    // value nothing has signalled yet.
    // SAFETY: a live fence.
    let gate = unsafe { fence.GetCompletedValue() } + 1000;
    D3D12EncodeFrameAccess::begin(&mut frame)
        .expect("frame access")
        .commit(gate)
        .expect("gate");
    let signalled = copy_rendered(&context, &mut copier, &planes, &mut frame, size, 2)
        .expect("the copy submits");
    assert_eq!(signalled, gate + 1);
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !reaches(&fence, gate, Duration::ZERO),
        "the copy ran before the encoder released the frame"
    );
    // The encoder lets go of the frame: its queue signals the value the copy waits for.
    // SAFETY: a live fence.
    unsafe { fence.Signal(gate) }.expect("release the frame");
    assert!(
        reaches(&fence, gate + 1, Duration::from_secs(10)),
        "the copy runs once the frame is released"
    );
    assert_planes(&download(&frame), size, 2, "after the wait");
    copier.finish().expect("no copy is left waiting");
}

#[test]
fn an_encode_copy_that_may_still_be_on_the_gpu_keeps_its_frame_from_the_pool() {
    let Some(context) = shared_context() else {
        return;
    };
    let size = (64, 32);
    // A copy that failed after it was submitted with no fence (the queue signal) may write the
    // frame for as long as it runs; one that failed before it was submitted wrote nothing.
    for (step, kept) in [
        (crate::interop::ALPHA2_GPU_STEP_QUEUE_WAIT, false),
        (crate::interop::ALPHA2_GPU_STEP_CLOSE_COMMAND_LIST, false),
        (COPY_SUBMITTED_UNFENCED, true),
    ] {
        let mut copier = D3D12EncodeCopier::new(&context).expect("copier");
        let planes = plane_textures(&context, size);
        let mut frame = encode_frame(&context, size);
        let before = d3d12_frame_fence_value(&frame).expect("fence value");
        inject::fail_next_encode_copy(step);
        let error = copy_rendered(&context, &mut copier, &planes, &mut frame, size, 0)
            .expect_err("the copy fails");
        assert_eq!(error.gpu_fault(), Some(GpuFault::DeviceLost), "{error}");
        assert_eq!(
            d3d12_frame_fence_value(&frame).expect("fence value"),
            before,
            "step {step}: a failed copy leaves the frame's fence value as it was"
        );
        assert_eq!(
            pool_references(&frame) > 1,
            kept,
            "step {step}: {error}; the frame goes back only when no copy can be writing it"
        );
        let mut next = encode_frame(&context, size);
        copy_rendered(&context, &mut copier, &planes, &mut next, size, 1).expect("the next copy");
        assert_planes(&download(&next), size, 1, &format!("after step {step}"));
        copier.finish().expect("no copy is left waiting");
    }
}

#[test]
fn an_encode_copier_that_does_not_finish_in_time_keeps_what_its_copies_use() {
    // A child isolates its short wait timeout from concurrently running tests.
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "d3d12::tests::encode_copier_timeout_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .output()
        .expect("child");
    assert_child_ran(&output);
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "run by an_encode_copier_that_does_not_finish_in_time_keeps_what_its_copies_use"]
fn encode_copier_timeout_child() {
    let Some(context) = shared_context() else {
        return;
    };
    set_wait_timeout(Duration::from_millis(1));
    let size = (64, 32);
    let mut copier = D3D12EncodeCopier::new(&context).expect("copier");
    let probe = Arc::new(());
    copier
        .retained
        .as_mut()
        .expect("retained work")
        .lifetime_probe = Some(Arc::clone(&probe));
    let planes = plane_textures(&context, size);
    let mut frame = encode_frame(&context, size);
    let fence = frame_fence(&frame);
    // A frame whose encode never lets go: the copy waits on the GPU for a fence value nothing
    // signals, so it cannot end within the 1 ms wait.
    // SAFETY: a live fence.
    let gate = unsafe { fence.GetCompletedValue() } + 1000;
    D3D12EncodeFrameAccess::begin(&mut frame)
        .expect("frame access")
        .commit(gate)
        .expect("gate");
    copy_rendered(&context, &mut copier, &planes, &mut frame, size, 0).expect("the copy submits");
    let started = Instant::now();
    let error = copier.finish().expect_err("the copy is still waiting");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(error.gpu_fault(), Some(GpuFault::Timeout), "{error}");
    assert_eq!(
        Arc::strong_count(&probe),
        2,
        "a copier not known to be done keeps its retained work, and its device"
    );
    assert_eq!(
        pool_references(&frame),
        2,
        "the frame it copies into stays out of the pool"
    );
    // Let the copy run, so the device ends with nothing outstanding.
    // SAFETY: a live fence.
    unsafe { fence.Signal(gate) }.expect("signal");
    assert!(reaches(&fence, gate + 1, Duration::from_secs(10)));
}

/// The device's D3D12 debug-layer message queue, when the layer is on.
fn debug_layer(context: &SharedD3D12Context) -> Option<ID3D12InfoQueue> {
    // SAFETY: the context is D3D12; the cast takes an independent reference.
    let hal = unsafe { context.gpu.device().as_hal::<wgpu_hal::api::Dx12>() }.expect("D3D12");
    hal.raw_device().cast::<ID3D12InfoQueue>().ok()
}

/// The error and corruption messages the debug layer has stored.
fn debug_layer_errors(queue: &ID3D12InfoQueue) -> Vec<String> {
    let mut errors = Vec::new();
    // SAFETY: the queue is live; each message is read into a buffer of the length it reports, and
    // its description lives in that buffer.
    unsafe {
        for index in 0..queue.GetNumStoredMessages() {
            let mut length = 0;
            if queue.GetMessage(index, None, &raw mut length).is_err() || length == 0 {
                continue;
            }
            let mut storage = vec![0_u64; length.div_ceil(8)];
            let message = storage.as_mut_ptr().cast::<D3D12_MESSAGE>();
            if queue
                .GetMessage(index, Some(message), &raw mut length)
                .is_err()
            {
                continue;
            }
            let message = &*message;
            if message.Severity == D3D12_MESSAGE_SEVERITY_ERROR
                || message.Severity == D3D12_MESSAGE_SEVERITY_CORRUPTION
            {
                let text =
                    std::slice::from_raw_parts(message.pDescription, message.DescriptionByteLength);
                errors.push(
                    String::from_utf8_lossy(text)
                        .trim_end_matches('\0')
                        .to_owned(),
                );
            }
        }
    }
    errors
}

#[test]
fn the_copy_leaves_the_planes_where_wgpu_expects_them() {
    // A child, because the debug layer and GPU-based validation are switched on for a process
    // before its first device: this one may already have made it.
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "d3d12::tests::encode_plane_state_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("WGPU_VALIDATION", "1")
        .env("WGPU_GPU_BASED_VALIDATION", "1")
        .output()
        .expect("child");
    assert_child_ran(&output);
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "run by the_copy_leaves_the_planes_where_wgpu_expects_them"]
fn encode_plane_state_child() {
    let Some(context) = shared_context() else {
        return;
    };
    let layer = debug_layer(&context);
    match &layer {
        Some(queue) => {
            // The check below reads what this reads; show that an error reaches it. The
            // validation collector reads it too, as an error this test made on purpose.
            let ((), collected) = alpha2_gpu::validation_log::expect_errors(|| {
                // SAFETY: a live queue and a NUL-terminated message.
                unsafe {
                    queue
                        .AddApplicationMessage(
                            D3D12_MESSAGE_SEVERITY_ERROR,
                            windows::core::s!("alpha2 debug-layer probe"),
                        )
                        .expect("probe message");
                }
            });
            assert_eq!(debug_layer_errors(queue), ["alpha2 debug-layer probe"]);
            if alpha2_gpu::validation_log::mode() != alpha2_gpu::validation_log::ValidationMode::Off
            {
                assert!(
                    collected
                        .iter()
                        .any(|record| record.message == "alpha2 debug-layer probe"),
                    "the validation collector missed the probe: {collected:?}"
                );
            }
            // SAFETY: a live queue.
            unsafe { queue.ClearStoredMessages() };
            eprintln!("note: the D3D12 debug layer is on, with GPU-based validation asked for");
        }
        None => eprintln!("note: no D3D12 debug layer on this machine; only wgpu validates"),
    }
    let size = (64, 32);
    let mut copier = D3D12EncodeCopier::new(&context).expect("copier");
    let planes = plane_textures(&context, size);
    let mut first = encode_frame(&context, size);
    let mut second = encode_frame(&context, size);
    validated(&context, || {
        copy_rendered(&context, &mut copier, &planes, &mut first, size, 0).expect("first copy");
        // The next render into the same pair, which wgpu records from the state it tracked.
        copy_rendered(&context, &mut copier, &planes, &mut second, size, 1).expect("second copy");
        // And wgpu's own copy out of them, after the raw copy put them back.
        assert_rendered(&read_texture(&context, &planes.0, 1), 64, 0, 1, "luma");
        assert_rendered(&read_texture(&context, &planes.1, 2), 32, 1, 1, "chroma");
    });
    copier.finish().expect("no copy is left waiting");
    context
        .gpu
        .device()
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(Duration::from_secs(10)),
        })
        .expect("the queue drains");
    // Read before the downloads below: FFmpeg 8.1.3's own download records its copy on a copy
    // queue from COPY_SOURCE, which the debug layer reports against FFmpeg, not this copy.
    if let Some(queue) = &layer {
        let errors = debug_layer_errors(queue);
        assert!(
            errors.is_empty(),
            "the D3D12 debug layer reported: {errors:#?}"
        );
    }
    assert_planes(&download(&first), size, 0, "first");
    assert_planes(&download(&second), size, 1, "second");
}

#[test]
fn every_adapter_answers_for_every_codec_with_extents_or_a_reason_in_time() {
    let mut answered = 0;
    {
        let _turn = alpha2_gpu::serialize_acquisition();
        let instance = alpha2_gpu::new_instance(wgpu::Backends::DX12);
        // Every DX12 adapter, the software one (WARP) included.
        for adapter in pollster::block_on(instance.enumerate_adapters(wgpu::Backends::DX12)) {
            let info = adapter.get_info();
            let Ok((device, queue)) =
                pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            else {
                eprintln!("{}: no device", info.name);
                continue;
            };
            let gpu = GpuContext::from_parts(adapter, device, queue);
            for codec in [
                D3D12EncodeCodec::H264,
                D3D12EncodeCodec::Hevc,
                D3D12EncodeCodec::Av1,
            ] {
                let started = Instant::now();
                let answer = query_encode_support(&gpu, codec);
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "{}: the {codec:?} query took {:?}",
                    info.name,
                    started.elapsed()
                );
                match &answer {
                    Ok(extent) => {
                        assert!(
                            extent.minimum.0 <= extent.maximum.0
                                && extent.minimum.1 <= extent.maximum.1
                                && extent.maximum.0 > 0,
                            "{extent:?}"
                        );
                        assert!(extent.granularity.0 >= 1 && extent.granularity.1 >= 1);
                        assert!(!extent.rate_control_modes.is_empty(), "{extent:?}");
                        assert_eq!(extent.codec, codec);
                    }
                    Err(reason) => assert!(
                        reason.contains(&info.name) && reason.len() > info.name.len(),
                        "{reason}"
                    ),
                }
                eprintln!(
                    "{} ({:04x}:{:04x}, {:?}) {codec:?}: {answer:?}",
                    info.name, info.vendor, info.device, info.device_type
                );
                answered += 1;
            }
        }
    }
    if answered == 0 {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::D3d12Video,
            "no DX12 adapter"
        );
        return;
    }
    // A context asks once per codec, and answers the same each time after.
    let Some(context) = shared_context() else {
        return;
    };
    for codec in [D3D12EncodeCodec::H264, D3D12EncodeCodec::Hevc] {
        assert_eq!(context.encode_support(codec), context.encode_support(codec));
        assert!(context.encode_support[codec as usize].get().is_some());
    }
}

#[test]
fn a_context_on_an_adapter_without_nv12_plane_views_refuses_decode_alone() {
    let Some(context) = shared_context() else {
        return;
    };
    assert_eq!(
        context.decode_planes(),
        context
            .gpu
            .device()
            .features()
            .contains(wgpu::Features::TEXTURE_FORMAT_NV12)
    );
    // The context of an adapter without them, which `from_gpu` keeps: encode copies raw and needs
    // no plane view, while a decode copy is refused where decode asks.
    let mut without = context.clone();
    without.decode_planes = false;
    let frame = surface(&context.ffmpeg_device(), ffi::AV_PIX_FMT_NV12, 64, 32);
    let (lease, probe) = leased(frame);
    let error = without.copy_surface(lease).expect_err("refused");
    assert!(
        matches!(
            error,
            BridgeError::InvalidInput("D3D12 adapter does not expose NV12 plane views")
        ),
        "{error}"
    );
    assert!(probe.upgrade().is_none(), "the refusal keeps nothing");
    let size = (64, 32);
    let mut copier = D3D12EncodeCopier::new(&without).expect("copier");
    let planes = plane_textures(&without, size);
    let mut frame = encode_frame(&without, size);
    copy_rendered(&without, &mut copier, &planes, &mut frame, size, 4).expect("encode copies");
    assert_planes(&download(&frame), size, 4, "without plane views");
    copier.finish().expect("no copy is left waiting");
}

#[test]
fn an_encode_refusal_names_the_adapter_the_codec_and_the_check() {
    use crate::interop::{
        ALPHA2_D3D12_ENCODE_CHECK_CODEC, ALPHA2_D3D12_ENCODE_CHECK_INPUT_FORMAT,
        ALPHA2_D3D12_ENCODE_CHECK_SUPPORT, ALPHA2_D3D12_ENCODE_CHECK_VIDEO_DEVICE,
        ALPHA2_D3D12_RATE_CONTROL_CBR, ALPHA2_D3D12_RATE_CONTROL_VBR,
    };
    let refused = |check, status: u32| D3D12EncodeCaps {
        refused_check: check,
        refused_status: status.cast_signed(),
        ..D3D12EncodeCaps::default()
    };
    let reason = |codec, caps: &D3D12EncodeCaps| encode_refusal("GPU A", codec, caps);
    assert_eq!(
        reason(
            D3D12EncodeCodec::Hevc,
            &refused(ALPHA2_D3D12_ENCODE_CHECK_VIDEO_DEVICE, 0x8000_4002)
        ),
        "GPU A: ID3D12VideoDevice3 unavailable (HRESULT 0x80004002)"
    );
    assert_eq!(
        reason(
            D3D12EncodeCodec::Av1,
            &refused(ALPHA2_D3D12_ENCODE_CHECK_CODEC, 0)
        ),
        "GPU A reports no D3D12 video encoder for av1"
    );
    assert_eq!(
        reason(
            D3D12EncodeCodec::H264,
            &refused(ALPHA2_D3D12_ENCODE_CHECK_INPUT_FORMAT, 0)
        ),
        "GPU A: D3D12 Video Encode refuses h264 main at the NV12 input check (not supported)"
    );
    // A device that lists bit-rate modes and refuses each in the whole configuration, as an
    // Intel Iris Xe on driver 30.0.101.2079 does.
    let mut caps = refused(ALPHA2_D3D12_ENCODE_CHECK_SUPPORT, 0x887A_0020);
    caps.refused_rate_control_modes = ALPHA2_D3D12_RATE_CONTROL_CBR | ALPHA2_D3D12_RATE_CONTROL_VBR;
    caps.rate_control_status = [0, 0x887A_0020_u32.cast_signed(), 0, 0];
    assert_eq!(
        reason(D3D12EncodeCodec::Hevc, &caps),
        "GPU A: D3D12 Video Encode refuses hevc main at the \
         D3D12_FEATURE_VIDEO_ENCODER_SUPPORT check (cbr: HRESULT 0x887A0020, vbr: not supported)"
    );
}

/// Prints what tells the DXGI rows of one GPU apart. Run by hand when a machine lists an adapter
/// twice.
#[test]
#[ignore = "a measurement, run by hand"]
fn print_dxgi_adapter_identities() {
    for (index, adapter) in pollster::block_on(alpha2_gpu::GpuContext::adapters_by_preference_on(
        wgpu::Backends::DX12,
    )) {
        let info = adapter.get_info();
        println!(
            "[{index}] {} {:?}: {:?}",
            info.name,
            info.device_type,
            super::d3d12_adapter_identity(&adapter)
        );
    }
}

/// WARP renders but has no D3D12 Video device. Binding it names the adapter, the interface and
/// the `HRESULT`, where `FFmpeg`'s device context would fail with a bare `EINVAL`.
#[test]
fn an_adapter_without_a_video_device_names_the_interface_and_hresult() {
    let instance = alpha2_gpu::new_instance(wgpu::Backends::DX12);
    let warp = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::DX12))
        .into_iter()
        .find(|adapter| adapter.get_info().device_type == wgpu::DeviceType::Cpu);
    let Some(warp) = alpha2_test_support::available!(
        alpha2_test_support::Capability::D3d12Video,
        warp.ok_or_else(|| "no WARP adapter".to_owned())
    ) else {
        return;
    };
    let gpu = pollster::block_on(alpha2_gpu::GpuContext::open_adapter(warp)).expect("WARP opens");
    let Err(error) = SharedD3D12Context::from_gpu(gpu) else {
        panic!("WARP has no D3D12 Video device");
    };
    let error = error.to_string();
    assert!(
        error.contains("Microsoft Basic Render Driver (LUID 0x")
            && error.contains("ID3D12VideoDevice unavailable (HRESULT 0x80004002 E_NOINTERFACE)"),
        "{error}"
    );
}

#[test]
fn an_hresult_is_given_in_hex_with_its_name() {
    assert_eq!(
        hresult_text(0x8000_4002_u32.cast_signed()),
        "HRESULT 0x80004002 E_NOINTERFACE"
    );
    assert_eq!(
        hresult_text(0x887A_0006_u32.cast_signed()),
        "HRESULT 0x887A0006 DXGI_ERROR_DEVICE_HUNG"
    );
    assert_eq!(
        hresult_text(0x8899_0001_u32.cast_signed()),
        "HRESULT 0x88990001"
    );
}
