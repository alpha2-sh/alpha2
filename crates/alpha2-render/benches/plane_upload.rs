//! Benchmark for handing decoded YCbCr planes to the renderer. Needs a GPU adapter.
//!
//! `plane_upload` covers 1080p and 2160p 8-bit 4:2:0, 10-bit 4:2:0, P010 and 10-bit 4:4:4 via
//! `prepare_yuv_presented`: `prepare_ms` is CPU staging and recording time, `frame_ms` wall time
//! until converted. Each runs with 16-bit normalized textures (`storage=unorm16`) and without
//! (`storage=uint16`, as in browser `WebGPU`). Not gated.

use std::time::Instant;

use alpha2_gpu::GpuContext;
use alpha2_media::{ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange, YuvFormat};
use alpha2_render::{ColorPolicy, RenderSize, Renderer, SourcePlane, SourcePresentation, YuvFrame};

const FRAMES: u32 = 30;

fn main() {
    // One device at a time: lavapipe loses a second device opened beside the first.
    for without_16_bit_norm in [false, true] {
        let gpu = if without_16_bit_norm {
            device_without_16_bit_norm().expect("GPU adapter")
        } else {
            pollster::block_on(GpuContext::request_product()).expect("GPU adapter")
        };
        // Print the adapter first: a time means nothing without its GPU.
        let adapter = gpu.capabilities();
        println!(
            "adapter: {} on {} ({} {})",
            adapter.adapter_name, adapter.backend, adapter.driver, adapter.driver_info
        );
        let storage = if gpu
            .device()
            .features()
            .contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM)
        {
            "unorm16"
        } else {
            "uint16"
        };
        let renderer = Renderer::new(gpu.clone());
        for (width, height) in [(1_920, 1_080), (3_840, 2_160)] {
            for (name, format) in [
                ("yuv420p", YuvFormat::I420),
                ("yuv420p10le", YuvFormat::planar(1, 1, 10)),
                ("p010", YuvFormat::P010),
                ("yuv444p10le", YuvFormat::planar(0, 0, 10)),
            ] {
                plane_upload(&renderer, &gpu, storage, name, format, (width, height));
            }
        }
    }
}

/// A high-performance device without optional features, like browser `WebGPU`.
fn device_without_16_bit_norm() -> Result<GpuContext, alpha2_gpu::GpuError> {
    pollster::block_on(GpuContext::request_product_with(
        &alpha2_gpu::ProductRequest {
            withheld: wgpu::Features::all(),
            ..alpha2_gpu::ProductRequest::default()
        },
    ))
}

/// Planes of `format` holding a moving ramp at the format's depth and alignment.
#[allow(clippy::cast_possible_truncation)]
fn planes(format: YuvFormat, (width, height): (u32, u32), frame: u32) -> [(Vec<u8>, u32); 3] {
    let bytes = format.bytes_per_sample();
    let (chroma_width, chroma_height) = format.chroma_size(width, height);
    let pairs = if format.interleaved_chroma { 2 } else { 1 };
    let shift = if format.msb_aligned {
        16 - u32::from(format.bit_depth)
    } else {
        0
    };
    let maximum = (1_u32 << format.bit_depth) - 1;
    let plane = |samples_wide: u32, rows: u32, offset: u32| {
        let stride = samples_wide * bytes;
        let mut data = Vec::with_capacity((stride * rows) as usize);
        for row in 0..rows {
            for column in 0..samples_wide {
                let value = ((column + row + frame * 8 + offset) % (maximum + 1)) << shift;
                if bytes == 1 {
                    data.push(value as u8);
                } else {
                    data.extend_from_slice(&(value as u16).to_le_bytes());
                }
            }
        }
        (data, stride)
    };
    [
        plane(width, height, 0),
        plane(chroma_width * pairs, chroma_height, 64),
        plane(chroma_width, chroma_height, 128),
    ]
}

fn plane_upload(
    renderer: &Renderer,
    gpu: &GpuContext,
    storage: &str,
    name: &str,
    format: YuvFormat,
    (width, height): (u32, u32),
) {
    let size = RenderSize::new(width, height).expect("size");
    // Two sets of planes, alternated, so no frame repeats the one before.
    let sources = [
        planes(format, (width, height), 0),
        planes(format, (width, height), 1),
    ];
    let color = ColorMetadata {
        primaries: ColorPrimaries::Bt709,
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Limited,
        ..ColorMetadata::default()
    };
    let wait = || {
        gpu.device()
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .expect("GPU idle");
    };
    let present = |index: u32| -> f64 {
        let source = &sources[index as usize % 2];
        let plane = |index: usize| SourcePlane {
            bytes: &source[index].0,
            stride: source[index].1,
        };
        let frame = YuvFrame {
            format,
            planes: [plane(0), plane(1), plane(2)],
            alpha_plane: None,
            size,
            alpha: 1.0,
            color: Some(color),
        };
        let started = Instant::now();
        let (prepared, _) = renderer
            .prepare_yuv_presented(
                frame,
                SourcePresentation::identity(size),
                ColorPolicy::RejectHdr,
            )
            .expect("the renderer samples this format");
        let (texture, pending) = prepared.into_parts();
        if let Some(pending) = pending {
            assert!(renderer.submit_pending_source(&pending), "submitted");
        }
        let prepare = started.elapsed().as_secs_f64();
        drop(texture);
        prepare
    };
    present(0);
    wait();
    let mut prepare = 0.0;
    let started = Instant::now();
    for index in 1..=FRAMES {
        prepare += present(index);
        wait();
    }
    let wall = started.elapsed().as_secs_f64();
    let per_frame = |seconds: f64| seconds / f64::from(FRAMES) * 1e3;
    println!(
        "plane_upload storage={storage} source={name} size={width}x{height} frames={FRAMES} \
         prepare_ms={:.2} frame_ms={:.2}",
        per_frame(prepare),
        per_frame(wall),
    );
}
