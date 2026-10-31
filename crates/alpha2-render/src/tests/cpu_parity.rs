//! The CPU renderer against the GPU renderer: the same graph drawn by both,
//! the pictures compared by tolerance class. A failure writes both pictures
//! to `target/cpu-parity/` to look at.

#![allow(clippy::cast_possible_truncation)]

use std::sync::Arc;

use super::{adapter_gpu, gpu_test_lock};
use crate::{
    CpuImage, CpuRenderer, CpuSource, GraphError, RenderControls, RenderGraph, RenderNodeId,
    RenderSize, Renderer, SourceView, UploadTexture,
};

/// How far the CPU picture may be from the GPU's, in 8-bit codes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum Tolerance {
    /// No sample differs.
    Exact,
    /// At most one code; at most 0.01 % of the samples two.
    Close,
    /// At most four codes; at most four samples up to eight: antialiased
    /// edges, cascaded blurs, rotations.
    Edges,
    /// At most four codes but for 0.2 % of the samples: math with a
    /// discontinuity a rounding error can cross (a gray test, a division
    /// by what is left of white), where two GPUs disagree as much.
    Unstable,
}

impl Tolerance {
    fn check(self, gpu: &[u8], cpu: &[u8]) -> Result<(), String> {
        assert_eq!(gpu.len(), cpu.len());
        let mut histogram = [0_usize; 256];
        let mut worst = (0_u8, 0_usize);
        for (index, (a, b)) in gpu.iter().zip(cpu).enumerate() {
            let difference = a.abs_diff(*b);
            histogram[usize::from(difference)] += 1;
            if difference > worst.0 {
                worst = (difference, index);
            }
        }
        let samples = gpu.len();
        let over = |limit: usize| histogram[limit + 1..].iter().sum::<usize>();
        let allowed = match self {
            Self::Exact => over(0) == 0,
            Self::Close => over(2) == 0 && over(1) <= samples.div_ceil(10_000),
            Self::Edges => over(8) == 0 && over(4) <= 4,
            Self::Unstable => over(4) <= samples.div_ceil(500),
        };
        if allowed {
            return Ok(());
        }
        let (difference, index) = worst;
        Err(format!(
            "{self:?}: worst difference {difference} at pixel {} channel {} (gpu {} cpu {}); \
             samples differing by 1: {}, 2: {}, 3-4: {}, more: {}",
            index / 4,
            index % 4,
            gpu[index],
            cpu[index],
            histogram[1],
            histogram[2],
            histogram[3] + histogram[4],
            over(4),
        ))
    }
}

/// One picture both renderers can draw from.
pub(crate) struct TestPicture {
    gpu: UploadTexture,
    cpu: CpuSource,
}

/// Which renderer a graph is built for.
#[derive(Copy, Clone)]
pub(crate) enum Side {
    Gpu,
    Cpu,
}

impl TestPicture {
    pub(crate) fn view(&self, side: Side) -> SourceView<'_> {
        match side {
            Side::Gpu => SourceView::from(&self.gpu),
            Side::Cpu => SourceView::cpu(&self.cpu),
        }
    }
}

/// A GPU renderer and a CPU renderer, both with every built-in.
pub(crate) struct Parity {
    pub(crate) gpu: Renderer,
    pub(crate) cpu: CpuRenderer,
    _lock: super::GpuTurn,
}

impl Parity {
    /// The GPU renderer draws on the adapter's own intermediate even under
    /// `ALPHA2_TEST_INTERMEDIATE=rgba8unorm`: the CPU renderer stores
    /// pictures as the half-float intermediate does and has no 8-bit one to
    /// compare with. The CPU renderer takes the filter sample budget the GPU
    /// applies, which a Tier-B device (GL) caps at 16.
    pub(crate) fn new() -> Self {
        let lock = gpu_test_lock();
        let mut gpu = Renderer::new(adapter_gpu());
        gpu.register_builtins().expect("built-ins register");
        let cpu = CpuRenderer::new(3);
        let quality = gpu.quality_controls();
        cpu.set_quality_controls(quality.max_filter_samples, quality.motion_blur_subframes);
        Self {
            gpu,
            cpu,
            _lock: lock,
        }
    }

    /// `tolerance` against this GPU. A software rasterizer (lavapipe in CI)
    /// computes `pow` with less precision than the hardware the classes
    /// were set on, and lands up to a few codes from it near black in linear
    /// light: there the classes widen one step. Metal does the same: wgpu
    /// compiles its shaders with Metal's default fast math, whose `pow` and
    /// division are approximate.
    pub(crate) fn class(&self, tolerance: Tolerance) -> Tolerance {
        let capabilities = self.gpu.gpu_capabilities();
        if tolerance == Tolerance::Close
            && (capabilities.software_rasterizer || capabilities.backend == "Metal")
        {
            Tolerance::Edges
        } else {
            tolerance
        }
    }

    /// `tolerance` for a pass that samples a source texture between texels
    /// with the hardware filter. NVIDIA filters with fewer bits of weight
    /// (and of 16-bit normalized texels) than RADV, lavapipe or the CPU, and
    /// a noisy test picture turns that into a code or two after the YCbCr
    /// matrix; there the class widens one step.
    pub(crate) fn filtered(&self, tolerance: Tolerance) -> Tolerance {
        let tolerance = self.class(tolerance);
        if tolerance == Tolerance::Close
            && self.gpu.gpu_capabilities().adapter_name.contains("NVIDIA")
        {
            Tolerance::Edges
        } else {
            tolerance
        }
    }

    /// A picture of premultiplied sRGB-encoded RGBA8 pixels, as the GPU's
    /// `upload_rgba8` and the CPU's text sources take them.
    pub(crate) fn picture(&self, size: RenderSize, rgba8: &[u8]) -> TestPicture {
        TestPicture {
            gpu: self.gpu.upload_rgba8(size, rgba8).expect("upload"),
            cpu: CpuSource::image(Arc::new(
                CpuImage::from_premultiplied_rgba8(size, rgba8).expect("picture"),
            )),
        }
    }

    /// Draws the graph `build` builds on both renderers and compares the
    /// pictures. `build` adds the nodes for one side and returns the output.
    pub(crate) fn check<'a>(
        &self,
        name: &str,
        size: RenderSize,
        tolerance: Tolerance,
        build: impl Fn(&mut RenderGraph<'a>, Side) -> Result<RenderNodeId, GraphError>,
    ) {
        let draw = |side: Side| {
            let mut graph = RenderGraph::new(size);
            let output = build(&mut graph, side).expect("graph builds");
            graph.set_output(output).expect("output");
            graph
        };
        let gpu_graph = draw(Side::Gpu);
        let texture = self.gpu.execute_graph(&gpu_graph).expect("GPU draws");
        let gpu_pixels = self.gpu.read_rgba8(&texture).expect("GPU readback");
        let cpu_graph = draw(Side::Cpu);
        let image = self.cpu.execute_graph(&cpu_graph).expect("CPU draws");
        let cpu_pixels = image.read_rgba8();
        assert_close(name, size, self.class(tolerance), &gpu_pixels, &cpu_pixels);
    }
}

/// Fails with the comparison when `cpu` is not within `tolerance` of `gpu`,
/// after writing both pictures out.
pub(crate) fn assert_close(
    name: &str,
    size: RenderSize,
    tolerance: Tolerance,
    gpu: &[u8],
    cpu: &[u8],
) {
    if let Err(message) = tolerance.check(gpu, cpu) {
        let directory =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/cpu-parity");
        let _ = std::fs::create_dir_all(&directory);
        for (side, pixels) in [("gpu", gpu), ("cpu", cpu)] {
            let path = directory.join(format!("{name}-{side}.png"));
            if let Ok(file) = std::fs::File::create(&path) {
                let mut encoder =
                    png::Encoder::new(std::io::BufWriter::new(file), size.width, size.height);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                if let Ok(mut writer) = encoder.write_header() {
                    let _ = writer.write_image_data(pixels);
                }
            }
        }
        panic!(
            "{name}: CPU differs from GPU: {message} (pictures in {})",
            directory.display()
        );
    }
}

/// A test card: smooth ramps, hard edges, a translucent corner, and noise,
/// premultiplied.
pub(crate) fn test_card(size: RenderSize, opaque: bool) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(size.width as usize * size.height as usize * 4);
    let mut state = 0x1234_5678_u32;
    for y in 0..size.height {
        for x in 0..size.width {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (state >> 24) as u8;
            #[allow(clippy::cast_possible_truncation)]
            let ramp = (x * 255 / size.width.max(1)) as u8;
            #[allow(clippy::cast_possible_truncation)]
            let fall = (y * 255 / size.height.max(1)) as u8;
            let checker = if (x / 8 + y / 8) % 2 == 0 { 230 } else { 40 };
            let mut pixel = [
                ramp,
                fall,
                if x < size.width / 2 { checker } else { noise },
                255,
            ];
            if !opaque && x > size.width * 3 / 4 && y > size.height * 3 / 4 {
                let alpha = 96_u16;
                pixel = [
                    (u16::from(pixel[0]) * alpha / 255) as u8,
                    (u16::from(pixel[1]) * alpha / 255) as u8,
                    (u16::from(pixel[2]) * alpha / 255) as u8,
                    alpha as u8,
                ];
            }
            pixels.extend_from_slice(&pixel);
        }
    }
    pixels
}
