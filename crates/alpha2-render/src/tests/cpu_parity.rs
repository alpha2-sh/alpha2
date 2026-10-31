//! The CPU renderer against the GPU renderer: the same graph drawn by both,
//! the pictures compared by tolerance class. A failure writes both pictures
//! to `target/cpu-parity/` to look at.

#![allow(clippy::cast_possible_truncation)]

use std::sync::Arc;

use super::device_precision::device_precision;
use super::{adapter_gpu, gpu_test_lock};
use crate::{
    CpuImage, CpuRenderer, CpuSource, GraphError, RenderControls, RenderGraph, RenderNodeId,
    RenderSize, Renderer, SourceView, UploadTexture,
};

/// [`Parity::class`] widens on a device whose sRGB `pow` is off by more than
/// this many codes: a hundred times the least precise one measured (the
/// table in `device_precision.rs`).
const POW_WIDEN: f64 = 0.01;

/// [`Parity::class`] widens on a device that filters 8-bit pictures with
/// fewer bits of weight than this: the M4 (5) and the software rasterizers
/// (1), whose every sampled picture is off, filtered or not.
const COARSE_FILTER_BITS: u32 = 6;

/// [`Parity::filtered`] widens on a device that filters 8-bit pictures with
/// fewer bits of weight than this: NVIDIA (8) and WARP (7), not RADV or the
/// Iris Xe (9).
const WEIGHT_WIDEN: u32 = 9;

/// No sample of an [`Tolerance::Unstable`] comparison may differ by more
/// than this many codes: the smallest power of two over the largest
/// difference measured on hardware (a halftone cell edge, 34 codes on RADV,
/// the Iris Xe and the M4).
const UNSTABLE_CAP: usize = 64;

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
    /// At most four codes but for 0.2 % of the samples, and those by at most
    /// [`UNSTABLE_CAP`]: math with a discontinuity a rounding error can cross
    /// (a gray test, a division by what is left of white), where two GPUs
    /// disagree as much.
    Unstable,
    /// As [`Tolerance::Unstable`], by any amount: a step function (posterize
    /// levels, a hard hue edge), where a sample on a step lands on either
    /// side and the step can be most of the range.
    Steps,
}

/// How far apart two pictures are, sample by sample.
struct Differences {
    /// Samples by their difference in codes.
    histogram: [usize; 256],
    /// The largest difference and the first sample that has it.
    worst: (u8, usize),
    samples: usize,
}

impl Differences {
    fn of(gpu: &[u8], cpu: &[u8]) -> Self {
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
        Self {
            histogram,
            worst,
            samples: gpu.len(),
        }
    }

    /// Samples more than `limit` codes apart.
    fn over(&self, limit: usize) -> usize {
        self.histogram[limit + 1..].iter().sum()
    }
}

impl Tolerance {
    fn allows(self, differences: &Differences) -> bool {
        let over = |limit| differences.over(limit);
        match self {
            Self::Exact => over(0) == 0,
            Self::Close => over(2) == 0 && over(1) <= differences.samples.div_ceil(10_000),
            Self::Edges => over(8) == 0 && over(4) <= 4,
            Self::Unstable => {
                over(4) <= differences.samples.div_ceil(500) && over(UNSTABLE_CAP) == 0
            }
            Self::Steps => over(4) <= differences.samples.div_ceil(500),
        }
    }

    fn check(self, gpu: &[u8], cpu: &[u8], differences: &Differences) -> Result<(), String> {
        if self.allows(differences) {
            return Ok(());
        }
        let (difference, index) = differences.worst;
        let histogram = &differences.histogram;
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
            differences.over(4),
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
        let precision = device_precision();
        eprintln!("parity classes: {precision:?}");
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

    /// `tolerance` against this GPU. A device that filters 8-bit pictures
    /// coarsely (at half precision, or rounding the result to 8 bits) is a
    /// code or two off in every picture it samples, filtered or not, and
    /// crosses a discontinuity more often; one whose `pow` is imprecise lands
    /// a few codes off near black in linear light. There the class widens one
    /// step.
    #[allow(clippy::unused_self)] // The precision measured is that of this parity's GPU device.
    pub(crate) fn class(&self, tolerance: Tolerance) -> Tolerance {
        let precision = device_precision();
        if precision.filter_weight_bits >= COARSE_FILTER_BITS
            && precision.pow_error_codes <= POW_WIDEN
        {
            return tolerance;
        }
        match tolerance {
            Tolerance::Close => Tolerance::Edges,
            Tolerance::Unstable => Tolerance::Steps,
            other => other,
        }
    }

    /// `tolerance` for a pass that samples a source texture between texels
    /// with the hardware filter. A device that filters with fewer bits of
    /// weight than RADV turns a noisy test picture into a code or two after
    /// the YCbCr matrix; there the class widens one step.
    pub(crate) fn filtered(&self, tolerance: Tolerance) -> Tolerance {
        let tolerance = self.class(tolerance);
        if tolerance == Tolerance::Close && device_precision().filter_weight_bits < WEIGHT_WIDEN {
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

    /// Draws the graph `build` builds where the two renderers are not
    /// expected to agree on color (an effect's input past the precision its
    /// shader computes in): the GPU's picture, drawn twice from nothing
    /// cached, must be the same bytes. With `keeps_alpha` (the effect passes
    /// its source's alpha through), the CPU's alpha must also be the GPU's.
    pub(crate) fn check_gpu_determinism<'a>(
        &self,
        name: &str,
        size: RenderSize,
        keeps_alpha: bool,
        build: impl Fn(&mut RenderGraph<'a>, Side) -> Result<RenderNodeId, GraphError>,
    ) {
        let draw = |side: Side| {
            let mut graph = RenderGraph::new(size);
            let output = build(&mut graph, side).expect("graph builds");
            graph.set_output(output).expect("output");
            graph
        };
        let gpu_graph = draw(Side::Gpu);
        let gpu_pixels = [0, 1].map(|_| {
            self.gpu.clear_result_cache();
            self.gpu.clear_texture_pool();
            let texture = self.gpu.execute_graph(&gpu_graph).expect("GPU draws");
            self.gpu.read_rgba8(&texture).expect("GPU readback")
        });
        assert!(
            gpu_pixels[0] == gpu_pixels[1],
            "{name}: the GPU draws different pictures from one graph"
        );
        if !keeps_alpha {
            return;
        }
        let cpu_graph = draw(Side::Cpu);
        let cpu_pixels = self
            .cpu
            .execute_graph(&cpu_graph)
            .expect("CPU draws")
            .read_rgba8();
        // Each picture's alpha as an opaque gray picture, so a failure writes it out to look at.
        let alpha = |pixels: &[u8]| {
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|pixel| [pixel[3], pixel[3], pixel[3], 255])
                .collect::<Vec<_>>()
        };
        assert_close(
            &format!("{name}-alpha"),
            size,
            Tolerance::Exact,
            &alpha(&gpu_pixels[0]),
            &alpha(&cpu_pixels),
        );
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
    let differences = Differences::of(gpu, cpu);
    if let Some(path) = std::env::var_os("ALPHA2_PARITY_REPORT") {
        report(std::path::Path::new(&path), name, tolerance, &differences);
    }
    if let Err(message) = tolerance.check(gpu, cpu, &differences) {
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

/// Appends one JSON line describing a comparison to `path`: the class it was
/// held to, its worst difference and where, how many samples differ by how
/// much, and the device with its measured precision.
fn report(path: &std::path::Path, name: &str, tolerance: Tolerance, differences: &Differences) {
    use std::io::Write as _;

    let precision = device_precision();
    let adapter = super::SHARED_GPU
        .get()
        .map(|gpu| gpu.capabilities().summary())
        .unwrap_or_default();
    let line = serde_json::json!({
        "name": name,
        "class": format!("{tolerance:?}"),
        "worst": differences.worst.0,
        "at": differences.worst.1,
        "histogram_1": differences.histogram[1],
        "histogram_2": differences.histogram[2],
        "histogram_3_4": differences.histogram[3] + differences.histogram[4],
        "over_4": differences.over(4),
        "over_8": differences.over(8),
        "over_16": differences.over(16),
        "over_32": differences.over(32),
        "over_64": differences.over(64),
        "samples": differences.samples,
        "adapter": adapter,
        "filter_weight_bits": precision.filter_weight_bits,
        "pow_error_codes": precision.pow_error_codes,
    });
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "{line}"));
    if let Err(error) = written {
        panic!("ALPHA2_PARITY_REPORT {}: {error}", path.display());
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
