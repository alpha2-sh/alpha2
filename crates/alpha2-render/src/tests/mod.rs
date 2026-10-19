// Tests record fixtures into encoders of their own, outside the renderer's pools.
#![allow(clippy::disallowed_methods)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use alpha2_gpu::{CapabilityTier, GpuContext};
use alpha2_media::{
    ChromaLocation, ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange, DynamicRange,
    Orientation, Rect, Rotation, TransferFunction,
};

use crate::readback::read_rgba8;
use crate::{
    BlendMode, BlendSpace, Color, ColorPolicy, CompositeLayer, EffectDef, EffectInputs, EffectKind,
    EffectParams, EffectRuntime, ExternalTextureLease, GpuVideoPlanes, GraphError, GraphLayer,
    LayerTransform, ParamSpec, ParamType, ParamUnit, ParamValue, PlaneTarget, RenderCacheKey,
    RenderError, RenderGraph, RenderNodeKind, RenderSize, RenderedTexture, Renderer,
    SourcePresentation, SourceView, Yuv420Frame, Yuv420pTargets,
};
use golden::{assert_rgba8_golden, assert_rgba8_golden_tiles};

mod blend_reference;
mod golden;

mod antialiasing;
mod builtin_effect;
mod canaries;
mod catalog_and_timing;
mod chroma_siting;
mod colour_spaces;
mod composite_batch;
mod compositing;
mod device;
mod effect_quality;
mod fixed_passes;
mod generator;
mod gpu_error;
mod layer_bounds;
mod masks;
mod presentation;
mod readbacks;
mod release;
mod render_graph;
mod repacks;
mod result_cache;
mod runtime_effects;
mod single_pass;
mod source_presentation;
mod stalls;
mod texture_pool;
mod threads;
mod transition_quality;
mod view_cache;
mod yuv_conversion;
mod yuv_pack;

/// The pack these tests were written against: chroma centred in each 2x2
/// block, as VP9 and AV1 exports pack it.
fn centred(color: ColorMetadata) -> alpha2_media::YuvEncoding {
    alpha2_media::YuvEncoding::new(color, alpha2_media::ChromaLocation::Center)
}

static GPU_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn gpu_test_lock() -> std::sync::MutexGuard<'static, ()> {
    GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static SHARED_GPU: OnceLock<GpuContext> = OnceLock::new();

/// Returns a handle to the one device this suite shares.
///
/// Per-test renderer state (texture pool, pipeline cache, effect registry,
/// diagnostics, quality controls) lives on [`Renderer`], not on the context,
/// so sharing the device isolates as much as a device per test would. A
/// device per test makes this process tear down as many devices as it has
/// tests, a pattern no alpha2 host uses and the leading suspect for an
/// unreproduced SIGSEGV on a vendor Vulkan driver.
/// `device_creation_survives_repeated_teardown` still covers that path on
/// demand.
///
/// It is the adapter a native host renders on, `request_high_performance`'s,
/// not the first one listed: on a machine with an integrated and a discrete
/// GPU those differ, and a failure only on the host's adapter would pass here.
/// `WGPU_POWER_PREF` and `WGPU_BACKEND` still choose another. A failing test
/// prints which adapter it ran on.
///
/// `ALPHA2_TEST_INTERMEDIATE=rgba8unorm` runs the whole suite on the 8-bit
/// intermediate an adapter without renderable half floats falls back to; no
/// test adapter is one, so this is the only way the suite reaches it. Any
/// other value is refused rather than ignored.
fn gpu() -> GpuContext {
    let shared = SHARED_GPU
        .get_or_init(|| {
            pollster::block_on(GpuContext::request_high_performance())
                .expect("render tests require a working GPU adapter")
        })
        .clone();
    let capabilities = shared.capabilities();
    eprintln!(
        "render tests on {} ({}, {})",
        capabilities.adapter_name, capabilities.backend, capabilities.driver_info
    );
    match std::env::var("ALPHA2_TEST_INTERMEDIATE").as_deref() {
        Err(std::env::VarError::NotPresent) => shared,
        Ok("rgba8unorm") => shared.with_rgba8_intermediate(),
        other => panic!("ALPHA2_TEST_INTERMEDIATE must be unset or rgba8unorm, not {other:?}"),
    }
}

/// The shared device as each intermediate format: as the adapter chose, then
/// as the 8-bit fallback. The tests that run on both hold the composite,
/// background, blend, and readback paths that differ by format.
fn gpus_by_intermediate() -> [GpuContext; 2] {
    let gpu = gpu();
    let rgba8 = gpu.with_rgba8_intermediate();
    [gpu, rgba8]
}

fn assert_graph_delta(
    renderer: &Renderer,
    before: (u64, u64, u64),
    expected_nodes: u64,
    expected_passes: u64,
) {
    let after = renderer.diagnostics();
    assert_eq!(after.graph_nodes - before.0, expected_nodes);
    assert_eq!(after.graph_passes - before.1, expected_passes);
    assert_eq!(after.graph_submissions - before.2, 1);
}

fn graph_counts(renderer: &Renderer) -> (u64, u64, u64) {
    let diagnostics = renderer.diagnostics();
    (
        diagnostics.graph_nodes,
        diagnostics.graph_passes,
        diagnostics.graph_submissions,
    )
}

fn rgba_readback_pipeline_count(renderer: &Renderer) -> usize {
    usize::from(renderer.intermediate_format() == wgpu::TextureFormat::Rgba16Float)
}

fn full_range_bt709() -> ColorMetadata {
    ColorMetadata {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferFunction::Bt709,
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Full,
        dynamic_range: DynamicRange::Sdr,
    }
}

fn full_range_frame(
    format: alpha2_media::YuvFormat,
    planes: [crate::SourcePlane<'_>; 3],
    size: RenderSize,
) -> crate::YuvFrame<'_> {
    crate::YuvFrame {
        format,
        planes,
        alpha_plane: None,
        size,
        alpha: 1.0,
        color: Some(full_range_bt709()),
    }
}

fn plane(bytes: &[u8], stride: usize) -> crate::SourcePlane<'_> {
    crate::SourcePlane {
        bytes,
        stride: u32::try_from(stride).expect("small stride"),
    }
}

fn assert_rgb_close(actual: &[u8], expected: &[[u8; 3]], label: &str) {
    for (index, (pixel, expected)) in actual.chunks(4).zip(expected).enumerate() {
        for channel in 0..3 {
            assert!(
                pixel[channel].abs_diff(expected[channel]) <= 2,
                "{label}: pixel {index} channel {channel}: {} != {}",
                pixel[channel],
                expected[channel]
            );
        }
        assert_eq!(pixel[3], 255, "{label}: pixel {index} alpha");
    }
}

#[derive(Debug, Default)]
struct ExternalLeaseCalls {
    prepared: AtomicUsize,
    flushed: AtomicUsize,
    submitted: AtomicUsize,
}

#[derive(Debug)]
struct TestExternalLease {
    submitted: Arc<AtomicBool>,
    calls: Arc<ExternalLeaseCalls>,
    flush_failure: Option<RenderError>,
}

impl TestExternalLease {
    fn new(submitted: &Arc<AtomicBool>) -> Self {
        Self {
            submitted: Arc::clone(submitted),
            calls: Arc::new(ExternalLeaseCalls::default()),
            flush_failure: None,
        }
    }
}

impl ExternalTextureLease for TestExternalLease {
    fn retained_bytes(&self) -> u64 {
        6
    }

    fn prepare_submission(&self) -> bool {
        self.calls.prepared.fetch_add(1, Ordering::AcqRel);
        true
    }

    fn flush_submission(&self) -> Result<(), RenderError> {
        self.calls.flushed.fetch_add(1, Ordering::AcqRel);
        self.flush_failure.map_or(Ok(()), Err)
    }

    fn submitted(&self) {
        self.calls.submitted.fetch_add(1, Ordering::AcqRel);
        self.submitted.store(true, Ordering::Release);
    }
}

/// Builds separate R8 luma and RG8 chroma textures for one `width` x `height`
/// NV12 image, mirroring the plane views a hardware import produces.
fn nv12_test_planes(
    renderer: &Renderer,
    width: u32,
    height: u32,
    luma: &[u8],
    chroma: &[u8],
) -> (wgpu::Texture, wgpu::Texture) {
    let make_texture = |label, format, width: u32, height: u32, bytes: &[u8], row: u32| {
        let texture = renderer
            .gpu
            .device()
            .create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
        renderer.gpu.queue().write_texture(
            texture.as_image_copy(),
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(height),
            },
            texture.size(),
        );
        texture
    };
    let y = make_texture(
        "test NV12 Y",
        wgpu::TextureFormat::R8Unorm,
        width,
        height,
        luma,
        width,
    );
    let uv = make_texture(
        "test NV12 UV",
        wgpu::TextureFormat::Rg8Unorm,
        width / 2,
        height / 2,
        chroma,
        width,
    );
    (y, uv)
}

/// Reads an RGBA16F texture back as it is stored, without the RGBA8 pack.
fn read_rgba16f(renderer: &Renderer, target: &RenderedTexture) -> Vec<f32> {
    assert_eq!(target.format, wgpu::TextureFormat::Rgba16Float);
    let device = renderer.gpu.device();
    let row = target.size.width * 8;
    let padded =
        row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("alpha2 test RGBA16F readback"),
        size: u64::from(padded * target.size.height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(target.size.height),
            },
        },
        wgpu::Extent3d {
            width: target.size.width,
            height: target.size.height,
            depth_or_array_layers: 1,
        },
    );
    renderer.gpu.submit([encoder.finish()]);
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, |result| result.expect("map"));
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("poll");
    let mapped = buffer.slice(..).get_mapped_range().expect("mapped");
    let mut values = Vec::new();
    for line in mapped.chunks(padded as usize) {
        for half in line[..row as usize].as_chunks::<2>().0 {
            values.push(half_to_f32(u16::from_le_bytes(*half)));
        }
    }
    values
}

fn half_to_f32(bits: u16) -> f32 {
    let sign = if bits & 0x8000 == 0 { 1.0 } else { -1.0 };
    let exponent = i32::from((bits >> 10) & 0x1f);
    let fraction = f32::from(bits & 0x3ff);
    sign * match exponent {
        0 => fraction * 2.0_f32.powi(-24),
        31 => f32::INFINITY,
        _ => (1.0 + fraction / 1024.0) * 2.0_f32.powi(exponent - 15),
    }
}

fn visual(scale: [f32; 2], opacity: f32) -> LayerTransform {
    LayerTransform {
        position: [0.5, 0.5],
        anchor: [0.5, 0.5],
        scale,
        rotation_degrees: 0.0,
        opacity,
    }
}

fn runtime_tint_effect(wgsl_body: &str) -> EffectDef {
    EffectDef {
        id: "test.tint".into(),
        version: 1,
        kind: EffectKind::Effect,
        inputs: EffectInputs::One,
        params: vec![ParamSpec {
            name: "tint".into(),
            param_type: ParamType::Color,
            default: ParamValue::Color([1.0; 4]),
            min: None,
            max: None,
            step: None,
            unit: ParamUnit::Color,
            enum_values: Vec::new(),
        }],
        resources: Vec::new(),
        wgsl_body: wgsl_body.into(),
        tier_min: CapabilityTier::TierB,
        license: Some("MIT".into()),
    }
}

fn transition_test_image(size: RenderSize, inverse: bool) -> Vec<u8> {
    (0..size.height)
        .flat_map(|y| {
            (0..size.width).flat_map(move |x| {
                let red = u8::try_from(x * 255 / (size.width - 1)).expect("channel fits u8");
                let green = u8::try_from(y * 255 / (size.height - 1)).expect("channel fits u8");
                if inverse {
                    [255 - red, green, red, 255]
                } else {
                    [red, green, 255 - green, 255]
                }
            })
        })
        .collect()
}

fn rgba_texture(renderer: &Renderer, pixel: [u8; 4]) -> wgpu::Texture {
    rgba_image_texture(
        renderer,
        RenderSize::new(1, 1).expect("non-zero size"),
        &pixel,
    )
}

/// Parameters of the built-in chroma key comparing RGB distance
/// (`distance_mode` `rgb`), the key a straight sRGB-encoded color.
fn chroma_key_params(
    color: [f32; 3],
    threshold: f32,
    softness: f32,
    spill_suppression: f32,
) -> EffectParams<'static> {
    EffectParams::new()
        .with(
            "key_color",
            ParamValue::Color([color[0], color[1], color[2], 1.0]),
        )
        .with("threshold", ParamValue::Float(threshold))
        .with("softness", ParamValue::Float(softness))
        .with("spill_suppression", ParamValue::Float(spill_suppression))
        .with("distance_mode", ParamValue::Enum(0))
}

fn assert_near(actual: u32, expected: u32, tolerance: u32) {
    assert!(
        actual.abs_diff(expected) <= tolerance,
        "expected {actual} to be within {tolerance} pixels of {expected}"
    );
}

fn assert_rgba8_near(actual: &[u8], expected: &[u8], tolerance: u8) {
    assert_eq!(actual.len(), expected.len(), "RGBA byte lengths differ");
    if let Some((index, (actual, expected))) = actual
        .iter()
        .zip(expected)
        .enumerate()
        .find(|(_, (actual, expected))| actual.abs_diff(**expected) > tolerance)
    {
        panic!(
            "RGBA mismatch at byte {index}: actual {actual}, expected {expected}, tolerance {tolerance}"
        );
    }
}

fn rgba_image_texture(renderer: &Renderer, size: RenderSize, pixels: &[u8]) -> wgpu::Texture {
    let texture = renderer
        .gpu
        .device()
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("alpha2 test source texture"),
            size: wgpu::Extent3d {
                width: size.width,
                height: size.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
    renderer.gpu.queue().write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size.width * 4),
            rows_per_image: Some(size.height),
        },
        wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        },
    );
    texture
}

/// Reads a mapped readback the way a browser export does: the staging bytes
/// as they are, then each plane at the offset and stride `plane_layout`
/// reports, as a `VideoFrame` given that `layout` would.
fn i420_by_plane_layout(readback: crate::Yuv420pReadback) -> Vec<u8> {
    let size = readback.size();
    let layout = readback.plane_layout();
    let staging = readback
        .finish_mapped(|mapped| mapped.to_vec())
        .expect("mapped staging bytes");
    let widths = [size.width, size.width / 2, size.width / 2];
    let rows = [size.height, size.height / 2, size.height / 2];
    let mut packed = Vec::new();
    for ((plane, width), rows) in layout.into_iter().zip(widths).zip(rows) {
        for row in 0..u64::from(rows) {
            let start = usize::try_from(plane.offset + row * u64::from(plane.stride)).unwrap();
            packed.extend_from_slice(&staging[start..start + width as usize]);
        }
    }
    packed
}

/// Export drops alpha and keeps premultiplied RGB, which is the frame over
/// black: a translucent pixel is darker, not shown at full strength.
fn expected_chroma(block: [[u8; 4]; 4], metadata: ColorMetadata) -> (u8, u8) {
    let average = block.into_iter().fold([0.0_f32; 3], |mut sum, pixel| {
        let alpha = f32::from(pixel[3]) / 255.0;
        for channel in 0..3 {
            sum[channel] += f32::from(pixel[channel]) / 255.0 * alpha;
        }
        sum
    });
    let rgb = average.map(|value| value * 0.25);
    let (kr, kg, kb) = match metadata.matrix {
        ColorMatrix::Bt601 => (0.299, 0.587, 0.114),
        ColorMatrix::Bt709 | ColorMatrix::Rgb => (0.2126, 0.7152, 0.0722),
        ColorMatrix::Bt2020Ncl => (0.2627, 0.6780, 0.0593),
    };
    let (offset, scale) = match metadata.range {
        ColorRange::Limited => (128.0 / 255.0, 224.0 / 255.0),
        ColorRange::Full => (0.5, 1.0),
    };
    let luma = rgb[0] * kr + rgb[1] * kg + rgb[2] * kb;
    let u = offset + scale * (rgb[2] - luma) / (2.0 * (1.0 - kb));
    let v = offset + scale * (rgb[0] - luma) / (2.0 * (1.0 - kr));
    (quantize_unorm8(u), quantize_unorm8(v))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn quantize_unorm8(value: f32) -> u8 {
    // Clamping proves the rounded value fits in u8; Rust has no checked float
    // conversion.
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// One caller-owned NV12 plane target the test can also read back.
fn nv12_test_plane(
    context: &GpuContext,
    label: &'static str,
    size: RenderSize,
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    context.device().create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// A mappable buffer sized for `rows` padded rows.
fn nv12_test_readback(
    context: &GpuContext,
    label: &'static str,
    row: u32,
    rows: u32,
) -> wgpu::Buffer {
    context.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: u64::from(row) * u64::from(rows),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    })
}

/// Maps `buffer` and drops the row padding the copy alignment required.
fn unpad_rows(
    context: &GpuContext,
    buffer: &wgpu::Buffer,
    row: u32,
    width: u32,
    height: u32,
) -> Vec<u8> {
    buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    context
        .device()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("readback poll");
    let view = buffer
        .slice(..)
        .get_mapped_range()
        .expect("mapped readback range");
    let width = width as usize;
    let mut out = Vec::with_capacity(width * height as usize);
    for index in 0..height as usize {
        let start = index * row as usize;
        out.extend_from_slice(&view[start..start + width]);
    }
    drop(view);
    buffer.unmap();
    out
}

/// A 4 x 2 I420 frame of `planes`, at `alpha`.
fn i420(planes: [crate::SourcePlane<'_>; 3], alpha: f32) -> crate::YuvFrame<'_> {
    crate::YuvFrame {
        format: alpha2_media::YuvFormat::I420,
        planes,
        alpha_plane: None,
        size: RenderSize::new(4, 2).expect("size"),
        alpha,
        color: Some(ColorMetadata::default()),
    }
}
