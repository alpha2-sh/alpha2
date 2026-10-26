//! What a frame provider lends the CPU renderer as a graph source, and how
//! a decoded frame becomes the picture a source shows: yuv-to-linear.wesl
//! and rgba-present.wesl, run on the CPU over the presented picture's rows.
//!
//! The planes are not copied: a source holds the decoder's buffers and
//! converts from them the first time a graph draws it.

use std::fmt;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use alpha2_media::{ColorMetadata, YuvFormat};

use super::CpuRenderer;
use super::curves::{exact_srgb_to_linear, extended_linear_to_srgb, linear_to_srgb};
use super::half;
use super::image::{CpuImage, PixelBox, Pixels};
use crate::hdr::HdrToneMap;
use crate::yuv::{
    HdrHandling, PlaneStorage, PresentedEncoding, hdr_handling, presentation_uniform_bytes,
    uniform_bytes, validate_presentation,
};
use crate::{
    ColorDiagnostic, ColorPolicy, RenderError, RenderSize, SourceAlpha, SourcePlane,
    SourcePresentation, YuvFrame,
};

/// Decoded YCbCr planes a source converts from, owned by whoever decoded
/// them (a decoder's frame buffer), and kept unchanged while any source
/// holds them.
pub trait YuvPlanes: Send + Sync + fmt::Debug {
    /// The frame the planes hold.
    fn frame(&self) -> YuvFrame<'_>;
}

/// RGBA pixels a source converts from: 8-bit, or half-float bits for
/// linear-light sources.
pub trait RgbaPixels: Send + Sync + fmt::Debug {
    /// The pixels' bytes, rows `stride` bytes apart.
    fn plane(&self) -> SourcePlane<'_>;
}

impl RgbaPixels for Vec<u8> {
    fn plane(&self) -> SourcePlane<'_> {
        SourcePlane {
            bytes: self,
            stride: 0,
        }
    }
}

/// A picture the CPU renderer draws from as a graph source
/// ([`crate::SourceView::cpu`]): premultiplied, sRGB-encoded RGBA, as every
/// GPU source texture holds.
///
/// A source made from a decoded frame converts it the first time a graph
/// draws it, and keeps the picture for every later draw.
#[derive(Clone)]
pub struct CpuSource {
    inner: Arc<SourceInner>,
}

struct SourceInner {
    content: Content,
    presented: OnceLock<Arc<CpuImage>>,
}

enum Content {
    Image(Arc<CpuImage>),
    Yuv {
        planes: Arc<dyn YuvPlanes>,
        conversion: Box<YuvConversion>,
    },
    Rgba {
        pixels: Arc<dyn RgbaPixels>,
        stride: u32,
        size: RenderSize,
        geometry: Geometry,
        encoding: PresentedEncoding,
    },
}

impl fmt::Debug for CpuSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match &self.inner.content {
            Content::Image(_) => "image",
            Content::Yuv { .. } => "yuv",
            Content::Rgba { .. } => "rgba",
        };
        formatter
            .debug_struct("CpuSource")
            .field("kind", &kind)
            .field("presented", &self.inner.presented.get().is_some())
            .finish()
    }
}

impl CpuSource {
    /// A source that shows `picture` as it is.
    #[must_use]
    pub fn image(picture: Arc<CpuImage>) -> Self {
        Self::new(Content::Image(picture))
    }

    fn new(content: Content) -> Self {
        Self {
            inner: Arc::new(SourceInner {
                content,
                presented: OnceLock::new(),
            }),
        }
    }

    /// Bytes the source holds: its picture once presented.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        match &self.inner.content {
            Content::Image(image) => image.retained_bytes(),
            _ => self
                .inner
                .presented
                .get()
                .map_or(0, |image| image.retained_bytes()),
        }
    }

    /// The picture the source shows, converting it on first use.
    pub(crate) fn presented(&self, renderer: &CpuRenderer) -> Arc<CpuImage> {
        if let Content::Image(image) = &self.inner.content {
            return Arc::clone(image);
        }
        if let Some(image) = self.inner.presented.get() {
            return Arc::clone(image);
        }
        let image = Arc::new(match &self.inner.content {
            Content::Image(_) => unreachable!("handled above"),
            Content::Yuv { planes, conversion } => {
                present_yuv(renderer, &planes.frame(), conversion)
            }
            Content::Rgba {
                pixels,
                stride,
                size,
                geometry,
                encoding,
            } => present_rgba(
                renderer,
                &RgbaSource {
                    bytes: pixels.plane().bytes,
                    stride: *stride as usize,
                    size: *size,
                    encoding: *encoding,
                },
                geometry,
            ),
        });
        renderer
            .stats
            .presented_sources
            .fetch_add(1, Ordering::Relaxed);
        Arc::clone(self.inner.presented.get_or_init(|| image))
    }
}

/// The presentation uniforms the shaders take, read from the very bytes the
/// GPU path binds, so both paths convert with the same values.
#[derive(Clone, Copy, Debug)]
struct Geometry {
    output: [f32; 2],
    source: [f32; 2],
    visible: [f32; 4],
    draw: [f32; 4],
    orientation: u32,
    max_filter_samples: f32,
}

#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
struct YuvConversion {
    rows: [[f32; 3]; 3],
    range_scale: [f32; 3],
    range_offset: [f32; 3],
    transfer: u32,
    alpha: f32,
    alpha_plane: bool,
    alpha_scale: f32,
    geometry: Geometry,
    chroma_size: [f32; 2],
    chroma_offset: [f32; 2],
    interleaved: bool,
    chroma_scale: [f32; 2],
    gamut: [[f32; 3]; 3],
    tone: [f32; 4],
    hlg_gamma: f32,
    chroma_window: [f32; 4],
    format: YuvFormat,
    /// SDR with BT.709 primaries and no alpha plane: decoding with the sRGB
    /// EOTF and encoding again is the identity, so the conversion keeps the
    /// encoded value.
    encoded_identity: bool,
    /// The HDR policy skipped the source: it shows nothing.
    skipped: bool,
}

fn word(bytes: &[u8], offset: usize) -> [u8; 4] {
    bytes[offset..offset + 4].try_into().expect("four bytes")
}

fn float(bytes: &[u8], offset: usize) -> f32 {
    f32::from_ne_bytes(word(bytes, offset))
}

fn unsigned(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(word(bytes, offset))
}

fn floats<const N: usize>(bytes: &[u8], offset: usize) -> [f32; N] {
    std::array::from_fn(|index| float(bytes, offset + index * 4))
}

impl YuvConversion {
    /// The conversion from `YuvUniforms` (yuv-to-linear.wesl) as
    /// `uniform_bytes` writes it.
    fn from_uniform(bytes: &[u8], format: YuvFormat) -> Self {
        let rows = [floats::<3>(bytes, 0), floats(bytes, 16), floats(bytes, 32)];
        let gamut = [
            floats::<3>(bytes, 176),
            floats(bytes, 192),
            floats(bytes, 208),
        ];
        let transfer = unsigned(bytes, 80);
        let alpha_plane = unsigned(bytes, 88) != 0;
        let identity = gamut.iter().enumerate().all(|(row, values)| {
            values.iter().enumerate().all(|(column, value)| {
                let expected = if row == column { 1.0 } else { 0.0 };
                (value - expected).abs() <= 1.0e-6
            })
        });
        let size = floats::<4>(bytes, 96);
        Self {
            rows,
            range_scale: floats(bytes, 48),
            range_offset: floats(bytes, 64),
            transfer,
            alpha: float(bytes, 84),
            alpha_plane,
            alpha_scale: float(bytes, 92),
            geometry: Geometry {
                output: [size[0], size[1]],
                source: [size[2], size[3]],
                visible: floats(bytes, 112),
                draw: floats(bytes, 128),
                orientation: unsigned(bytes, 160),
                max_filter_samples: float(bytes, 272),
            },
            chroma_size: floats(bytes, 144),
            chroma_offset: floats(bytes, 152),
            interleaved: unsigned(bytes, 164) == 1,
            chroma_scale: floats(bytes, 168),
            gamut,
            tone: floats(bytes, 224),
            hlg_gamma: float(bytes, 240),
            chroma_window: floats(bytes, 256),
            format,
            encoded_identity: transfer == 0 && identity && !alpha_plane,
            skipped: false,
        }
    }
}

impl Geometry {
    /// From `PresentationParams` (rgba-present.wesl).
    fn from_uniform(bytes: &[u8]) -> Self {
        let size = floats::<4>(bytes, 0);
        Self {
            output: [size[0], size[1]],
            source: [size[2], size[3]],
            visible: floats(bytes, 16),
            draw: floats(bytes, 32),
            orientation: unsigned(bytes, 48),
            max_filter_samples: float(bytes, 56),
        }
    }

    /// The output pixels inside the drawn rectangle: everything else is
    /// transparent.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn bounds(&self, size: RenderSize) -> PixelBox {
        // Pixel centres p + 0.5 inside [draw.x, draw.x + draw.w).
        let first = |origin: f32| (origin - 0.5).ceil().max(0.0) as u32;
        let end = |origin: f32, extent: f32| ((origin + extent - 0.5).ceil()).max(0.0) as u32;
        let x = first(self.draw[0]);
        let y = first(self.draw[1]);
        let right = end(self.draw[0], self.draw[2]).min(size.width);
        let bottom = end(self.draw[1], self.draw[3]).min(size.height);
        if right <= x || bottom <= y {
            return PixelBox::EMPTY;
        }
        PixelBox {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }
    }

    fn inside(&self, destination: [f32; 2]) -> bool {
        destination[0] >= self.draw[0]
            && destination[1] >= self.draw[1]
            && destination[0] < self.draw[0] + self.draw[2]
            && destination[1] < self.draw[1] + self.draw[3]
    }

    /// `source_pixel_at`: the stored pixel position output position
    /// `destination` shows, kept inside the visible rect.
    fn source_pixel_at(&self, destination: [f32; 2]) -> [f32; 2] {
        let local = [
            (destination[0] - self.draw[0]) / self.draw[2],
            (destination[1] - self.draw[1]) / self.draw[3],
        ];
        let mut source = match self.orientation & 3 {
            1 => [local[1], 1.0 - local[0]],
            2 => [1.0 - local[0], 1.0 - local[1]],
            3 => [1.0 - local[1], local[0]],
            _ => local,
        };
        if self.orientation & 4 != 0 {
            source[0] = 1.0 - source[0];
        }
        let [origin_x, origin_y, width, height] = self.visible;
        [
            (origin_x + source[0] * width).clamp(origin_x + 0.5, origin_x + width - 0.5),
            (origin_y + source[1] * height).clamp(origin_y + 0.5, origin_y + height - 0.5),
        ]
    }

    /// `filter_grid`: samples per output pixel along each axis.
    fn filter_grid(&self) -> [f32; 2] {
        let mut extent = [self.visible[2], self.visible[3]];
        if self.orientation & 1 != 0 {
            extent.swap(0, 1);
        }
        let wanted = |extent: f32, drawn: f32| {
            let footprint = extent / drawn;
            if footprint > 2.0 {
                (footprint - 0.0001).ceil().clamp(1.0, 8.0)
            } else {
                1.0
            }
        };
        let wanted = [
            wanted(extent[0], self.draw[2]),
            wanted(extent[1], self.draw[3]),
        ];
        let budget = self.max_filter_samples.clamp(1.0, 64.0);
        let columns = wanted[0].min(budget);
        let rows = wanted[1].min((budget / columns).floor().max(1.0));
        [columns, rows]
    }

    /// `filtered_stored` around `stored_sample`.
    fn filtered(&self, destination: [f32; 2], sample: impl Fn([f32; 2]) -> [f32; 4]) -> [f32; 4] {
        let grid = self.filter_grid();
        #[allow(clippy::float_cmp)]
        if grid == [1.0, 1.0] {
            return sample(self.source_pixel_at(destination));
        }
        let mut sum = [0.0_f32; 4];
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        for row in 0..grid[1] as u32 {
            for column in 0..grid[0] as u32 {
                #[allow(clippy::cast_precision_loss)]
                let offset = [
                    (column as f32 + 0.5) / grid[0] - 0.5,
                    (row as f32 + 0.5) / grid[1] - 0.5,
                ];
                let value = sample(
                    self.source_pixel_at([destination[0] + offset[0], destination[1] + offset[1]]),
                );
                for channel in 0..4 {
                    sum[channel] += value[channel];
                }
            }
        }
        let count = grid[0] * grid[1];
        sum.map(|value| value / count)
    }
}

/// Converts the presented picture's rows with `pixel`, a function of the
/// output position, into a stored picture.
fn present_rows(
    renderer: &CpuRenderer,
    size: RenderSize,
    geometry: &Geometry,
    pixel: impl Fn([f32; 2]) -> [f32; 4] + Sync + Send,
) -> CpuImage {
    let bounds = geometry.bounds(size);
    if bounds.is_empty() {
        return CpuImage::transparent(size);
    }
    let width = bounds.width as usize;
    let mut pixels = vec![[0_u16; 4]; bounds.pixels()];
    renderer
        .workers
        .bands(&mut pixels, width, |first_row, band| {
            for (index, line) in band.chunks_mut(width).enumerate() {
                #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
                let y = (bounds.y as usize + first_row + index) as f32 + 0.5;
                for (offset, out) in line.iter_mut().enumerate() {
                    #[allow(clippy::cast_precision_loss)]
                    let x = (bounds.x as usize + offset) as f32 + 0.5;
                    let destination = [x, y];
                    *out = if geometry.inside(destination) {
                        half::store4(pixel(destination))
                    } else {
                        [0; 4]
                    };
                }
            }
        });
    CpuImage::new(size, bounds, Pixels::Half(pixels))
}

/// One plane's samples, read as a sampler with clamp-to-edge addressing
/// reads a texture of them: 8-bit codes as `code / 255`, 16-bit containers
/// as `container / 65535`.
struct Plane<'a> {
    bytes: &'a [u8],
    stride: usize,
    width: u32,
    height: u32,
    wide: bool,
    /// Samples per texel: 2 for interleaved chroma.
    components: usize,
}

impl Plane<'_> {
    #[inline]
    fn texel(&self, x: i64, y: i64, component: usize) -> f32 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (x, y) = (
            x.clamp(0, i64::from(self.width) - 1) as usize,
            y.clamp(0, i64::from(self.height) - 1) as usize,
        );
        let sample = x * self.components + component;
        if self.wide {
            let offset = y * self.stride + sample * 2;
            f32::from(u16::from_le_bytes([
                self.bytes[offset],
                self.bytes[offset + 1],
            ])) / 65535.0
        } else {
            half::unorm8_value(self.bytes[y * self.stride + sample])
        }
    }

    /// A bilinear sample at normalized position `position`.
    #[inline]
    fn sample(&self, position: [f32; 2], component: usize) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        let texel = [
            position[0] * self.width as f32 - 0.5,
            position[1] * self.height as f32 - 0.5,
        ];
        let origin = [texel[0].floor(), texel[1].floor()];
        let weight = [texel[0] - origin[0], texel[1] - origin[1]];
        #[allow(clippy::cast_possible_truncation)]
        let (x, y) = (origin[0] as i64, origin[1] as i64);
        let top = self.texel(x, y, component)
            + (self.texel(x + 1, y, component) - self.texel(x, y, component)) * weight[0];
        let bottom = self.texel(x, y + 1, component)
            + (self.texel(x + 1, y + 1, component) - self.texel(x, y + 1, component)) * weight[0];
        top + (bottom - top) * weight[1]
    }
}

/// The sRGB EOTF of the presentation shader: clamped first.
fn srgb_eotf(value: f32) -> f32 {
    exact_srgb_to_linear(value.clamp(0.0, 1.0))
}

// SMPTE ST 2084 and ARIB STD-B67, as yuv-to-linear.wesl computes them in
// `f32`, with `libm` so every target computes the same.
const PQ_M1: f32 = 0.159_301_76;
const PQ_M2: f32 = 78.843_75;
const PQ_C1: f32 = 0.835_937_5;
const PQ_C2: f32 = 18.851_563;
const PQ_C3: f32 = 18.6875;

fn pq_eotf(code: f32) -> f32 {
    let e = libm::powf(code.clamp(0.0, 1.0).max(1.0e-10), 1.0 / PQ_M2);
    let y = libm::powf((e - PQ_C1).max(0.0) / (PQ_C2 - PQ_C3 * e), 1.0 / PQ_M1);
    y * 10_000.0
}

fn pq_inverse_eotf(nits: f32) -> f32 {
    let y = libm::powf((nits / 10_000.0).clamp(0.0, 1.0).max(1.0e-10), PQ_M1);
    libm::powf((PQ_C1 + PQ_C2 * y) / (1.0 + PQ_C3 * y), PQ_M2)
}

fn hlg_inverse_oetf(code: f32) -> f32 {
    let e = code.clamp(0.0, 1.0);
    if e <= 0.5 {
        return e * e / 3.0;
    }
    (libm::expf((e - 0.559_910_7) / 0.178_832_77) + 0.284_668_92) / 12.0
}

impl YuvConversion {
    fn bt2390_eetf(&self, nits: f32) -> f32 {
        let source = self.tone[2];
        let value = pq_inverse_eotf(nits.min(self.tone[0])) / source;
        let max_lum = self.tone[3] / source;
        let knee = 1.5 * max_lum - 0.5;
        let mut mapped = value;
        if value >= knee && knee < 1.0 {
            let t = (value - knee) / (1.0 - knee);
            let t2 = t * t;
            let t3 = t2 * t;
            mapped = (2.0 * t3 - 3.0 * t2 + 1.0) * knee
                + (t3 - 2.0 * t2 + t) * (1.0 - knee)
                + (-2.0 * t3 + 3.0 * t2) * max_lum;
        }
        pq_eotf(mapped * source)
    }

    fn to_bt709(&self, rgb: [f32; 3]) -> [f32; 3] {
        self.gamut
            .map(|row| row[0] * rgb[0] + row[1] * rgb[1] + row[2] * rgb[2])
    }

    fn hdr_to_working(&self, encoded: [f32; 3]) -> [f32; 3] {
        let nits = if self.transfer == 2 {
            let scene = encoded.map(hlg_inverse_oetf);
            let luma = (0.2627 * scene[0] + 0.678 * scene[1] + 0.0593 * scene[2]).max(0.0);
            let gain = self.tone[0] * libm::powf(luma.max(1.0e-10), self.hlg_gamma - 1.0);
            scene.map(|value| value * gain)
        } else {
            encoded.map(pq_eotf)
        };
        let converted = self.to_bt709(nits);
        let largest = converted[0].max(converted[1].max(converted[2]));
        let scale = if largest > 0.0 {
            self.bt2390_eetf(largest) / largest
        } else {
            0.0
        };
        converted.map(|value| (value * scale / self.tone[1]).clamp(0.0, 1.0))
    }

    /// The YCbCr at a luma and a chroma position, decoded by the matrix and
    /// range: sRGB-encoded R'G'B'.
    fn encoded(&self, planes: &Planes<'_>, luma: [f32; 2], chroma: [f32; 2]) -> [f32; 3] {
        let u = planes.u.sample(chroma, 0);
        let v = if self.interleaved {
            planes.u.sample(chroma, 1)
        } else {
            planes
                .v
                .as_ref()
                .map_or(0.0, |plane| plane.sample(chroma, 0))
        };
        self.encoded_from([planes.y.sample(luma, 0), u, v])
    }

    /// Sampled codes, decoded by the range and the matrix: sRGB-encoded
    /// R'G'B'.
    #[inline]
    fn encoded_from(&self, codes: [f32; 3]) -> [f32; 3] {
        let yuv = [
            codes[0] * self.range_scale[0] + self.range_offset[0],
            codes[1] * self.range_scale[1] + self.range_offset[1],
            codes[2] * self.range_scale[2] + self.range_offset[2],
        ];
        self.rows
            .map(|row| row[0] * yuv[0] + row[1] * yuv[1] + row[2] * yuv[2])
    }

    /// `linear_rgb`.
    fn linear_rgb(&self, planes: &Planes<'_>, luma: [f32; 2], chroma: [f32; 2]) -> [f32; 3] {
        self.linear_from_encoded(self.encoded(planes, luma, chroma))
    }

    /// Encoded R'G'B' brought into the linear working space.
    fn linear_from_encoded(&self, encoded: [f32; 3]) -> [f32; 3] {
        if self.transfer != 0 {
            return self.hdr_to_working(encoded.map(|value| value.clamp(0.0, 1.0)));
        }
        self.to_bt709(encoded.map(srgb_eotf))
            .map(|value| value.clamp(0.0, 1.0))
    }

    /// `coordinates_at`: the luma and chroma positions of `source_pixel`.
    fn coordinates(&self, source_pixel: [f32; 2]) -> ([f32; 2], [f32; 2]) {
        let chroma_pixel = [
            ((source_pixel[0] + self.chroma_offset[0]) * self.chroma_scale[0])
                .clamp(self.chroma_window[0], self.chroma_window[2]),
            ((source_pixel[1] + self.chroma_offset[1]) * self.chroma_scale[1])
                .clamp(self.chroma_window[1], self.chroma_window[3]),
        ];
        (
            [
                source_pixel[0] / self.geometry.source[0],
                source_pixel[1] / self.geometry.source[1],
            ],
            [
                chroma_pixel[0] / self.chroma_size[0],
                chroma_pixel[1] / self.chroma_size[1],
            ],
        )
    }

    fn alpha_at(&self, planes: &Planes<'_>, luma: [f32; 2]) -> f32 {
        planes.alpha.as_ref().map_or(1.0, |plane| {
            (plane.sample(luma, 0) * self.alpha_scale).clamp(0.0, 1.0)
        })
    }

    /// `premultiplied_linear`: straight alpha premultiplied before it is
    /// filtered.
    fn premultiplied_linear(&self, planes: &Planes<'_>, source_pixel: [f32; 2]) -> [f32; 4] {
        let (luma, chroma) = self.coordinates(source_pixel);
        let texel = [
            luma[0] * self.geometry.source[0] - 0.5,
            luma[1] * self.geometry.source[1] - 0.5,
        ];
        let origin = [texel[0].floor(), texel[1].floor()];
        let weight = [texel[0] - origin[0], texel[1] - origin[1]];
        let first = [self.geometry.visible[0], self.geometry.visible[1]];
        let last = [
            first[0] + self.geometry.visible[2] - 1.0,
            first[1] + self.geometry.visible[3] - 1.0,
        ];
        let corner = |dx: f32, dy: f32| {
            self.coordinates([
                (origin[0] + dx).clamp(first[0], last[0]) + 0.5,
                (origin[1] + dy).clamp(first[1], last[1]) + 0.5,
            ])
        };
        let corners = [
            corner(0.0, 0.0),
            corner(1.0, 0.0),
            corner(0.0, 1.0),
            corner(1.0, 1.0),
        ];
        let alphas = corners.map(|(luma, _)| self.alpha_at(planes, luma));
        if alphas.iter().all(|alpha| *alpha >= 1.0) {
            let rgb = self.linear_rgb(planes, luma, chroma);
            return [rgb[0], rgb[1], rgb[2], 1.0];
        }
        let texels: Vec<[f32; 4]> = corners
            .iter()
            .zip(alphas)
            .map(|((luma, chroma), alpha)| {
                let rgb = self.linear_rgb(planes, *luma, *chroma);
                [rgb[0] * alpha, rgb[1] * alpha, rgb[2] * alpha, alpha]
            })
            .collect();
        let mix = |a: [f32; 4], b: [f32; 4], t: f32| {
            [0, 1, 2, 3].map(|channel| a[channel] * (1.0 - t) + b[channel] * t)
        };
        mix(
            mix(texels[0], texels[1], weight[0]),
            mix(texels[2], texels[3], weight[0]),
            weight[1],
        )
    }

    /// `stored_sample`: one position as the intermediate stores every
    /// presented source, sRGB-encoded, then premultiplied.
    fn stored_sample(&self, planes: &Planes<'_>, source_pixel: [f32; 2]) -> [f32; 4] {
        if !self.alpha_plane {
            let (luma, chroma) = self.coordinates(source_pixel);
            return self.stored_opaque(self.encoded(planes, luma, chroma));
        }
        Self::stored_linear(self.premultiplied_linear(planes, source_pixel))
    }

    /// An opaque sample's encoded R'G'B' as the intermediate stores it.
    #[inline]
    fn stored_opaque(&self, encoded: [f32; 3]) -> [f32; 4] {
        if self.encoded_identity {
            return [
                encoded[0].clamp(0.0, 1.0),
                encoded[1].clamp(0.0, 1.0),
                encoded[2].clamp(0.0, 1.0),
                1.0,
            ];
        }
        let rgb = self.linear_from_encoded(encoded);
        Self::stored_linear([rgb[0], rgb[1], rgb[2], 1.0])
    }

    /// Premultiplied linear light as the intermediate stores it.
    fn stored_linear(linear: [f32; 4]) -> [f32; 4] {
        if linear[3] <= 0.0 {
            return [0.0; 4];
        }
        let alpha = linear[3];
        [
            encode_srgb(linear[0] / alpha) * alpha,
            encode_srgb(linear[1] / alpha) * alpha,
            encode_srgb(linear[2] / alpha) * alpha,
            alpha,
        ]
    }
}

/// `encode_srgb` of yuv-to-linear.wesl: its input is already clamped.
fn encode_srgb(value: f32) -> f32 {
    linear_to_srgb(value)
}

struct Planes<'a> {
    y: Plane<'a>,
    u: Plane<'a>,
    v: Option<Plane<'a>>,
    alpha: Option<Plane<'a>>,
}

fn present_yuv(
    renderer: &CpuRenderer,
    frame: &YuvFrame<'_>,
    conversion: &YuvConversion,
) -> CpuImage {
    fn plane<'a>(
        source: &SourcePlane<'a>,
        (width, height): (u32, u32),
        wide: bool,
        components: usize,
    ) -> Plane<'a> {
        Plane {
            bytes: source.bytes,
            stride: source.stride as usize,
            width,
            height,
            wide,
            components,
        }
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let output = RenderSize {
        width: conversion.geometry.output[0] as u32,
        height: conversion.geometry.output[1] as u32,
    };
    if conversion.skipped {
        return CpuImage::transparent(output);
    }
    let format = conversion.format;
    let wide = format.bytes_per_sample() == 2;
    let (chroma_width, chroma_height) = format.chroma_size(frame.size.width, frame.size.height);
    let planes = Planes {
        y: plane(
            &frame.planes[0],
            (frame.size.width, frame.size.height),
            wide,
            1,
        ),
        u: plane(
            &frame.planes[1],
            (chroma_width, chroma_height),
            wide,
            if format.interleaved_chroma { 2 } else { 1 },
        ),
        v: (!format.interleaved_chroma)
            .then(|| plane(&frame.planes[2], (chroma_width, chroma_height), wide, 1)),
        alpha: frame
            .alpha_plane
            .as_ref()
            .map(|alpha| plane(alpha, (frame.size.width, frame.size.height), wide, 1)),
    };
    let geometry = conversion.geometry;
    let alpha = conversion.alpha;
    if let Some(image) = present_separable(renderer, output, &planes, conversion) {
        return image;
    }
    present_rows(renderer, output, &geometry, |destination| {
        geometry
            .filtered(destination, |source_pixel| {
                conversion.stored_sample(&planes, source_pixel)
            })
            .map(|value| value * alpha)
    })
}

/// Each output column's sample of `values`, a plane row filtered
/// vertically: the two texels its tap names, filtered between.
fn lerp_into(values: &[f32], taps: &[(usize, usize, f32)], out: &mut [f32]) {
    for (out, &(first, second, weight)) in out.iter_mut().zip(taps) {
        let a = values[first];
        *out = a + (values[second] - a) * weight;
    }
}

/// Where one output column (or row) of an upright presentation reads its
/// planes: the luma and chroma texels either side and the weight between.
#[derive(Clone, Copy, Default)]
struct AxisTap {
    inside: bool,
    luma: (u32, u32, f32),
    chroma: (u32, u32, f32),
}

impl YuvConversion {
    /// The taps of output position `destination` along `axis` (0 for x),
    /// computed with exactly the arithmetic of `source_pixel_at`,
    /// `coordinates_at` and a bilinear sample, which work per axis when the
    /// presentation turns by a half turn or not at all.
    fn axis_tap(&self, axis: usize, destination: f32, luma_size: u32, chroma_size: u32) -> AxisTap {
        let geometry = &self.geometry;
        let inside = destination >= geometry.draw[axis]
            && destination < geometry.draw[axis] + geometry.draw[axis + 2];
        let local = (destination - geometry.draw[axis]) / geometry.draw[axis + 2];
        let mut source = if geometry.orientation & 3 == 2 {
            1.0 - local
        } else {
            local
        };
        if axis == 0 && geometry.orientation & 4 != 0 {
            source = 1.0 - source;
        }
        let origin = geometry.visible[axis];
        let extent = geometry.visible[axis + 2];
        let source_pixel = (origin + source * extent).clamp(origin + 0.5, origin + extent - 0.5);
        let chroma_pixel = ((source_pixel + self.chroma_offset[axis]) * self.chroma_scale[axis])
            .clamp(self.chroma_window[axis], self.chroma_window[axis + 2]);
        let taps = |position: f32, size: u32| {
            #[allow(clippy::cast_precision_loss)]
            let texel = position * size as f32 - 0.5;
            let floor = texel.floor();
            let last = i64::from(size) - 1;
            #[allow(clippy::cast_possible_truncation)]
            let first = floor as i64;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            (
                first.clamp(0, last) as u32,
                (first + 1).clamp(0, last) as u32,
                texel - floor,
            )
        };
        AxisTap {
            inside,
            luma: taps(source_pixel / geometry.source[axis], luma_size),
            chroma: taps(chroma_pixel / self.chroma_size[axis], chroma_size),
        }
    }
}

impl Plane<'_> {
    /// Sample `component` of texel `x` in row `y`, normalized.
    #[inline]
    fn at(&self, x: u32, y: u32, component: usize) -> f32 {
        let sample = x as usize * self.components + component;
        let row = y as usize * self.stride;
        if self.wide {
            let offset = row + sample * 2;
            f32::from(u16::from_le_bytes([
                self.bytes[offset],
                self.bytes[offset + 1],
            ])) / 65535.0
        } else {
            half::unorm8_value(self.bytes[row + sample])
        }
    }

    /// `component` of the texels `first..` of the rows `row` names,
    /// filtered between them, into `out`.
    fn vertical(&self, row: (u32, u32, f32), first: u32, component: usize, out: &mut [f32]) {
        if !self.wide {
            let count = out.len();
            let line = |y: u32| {
                let start = y as usize * self.stride + first as usize * self.components + component;
                self.bytes[start..]
                    .iter()
                    .step_by(self.components)
                    .take(count)
                    .map(|&code| half::unorm8_value(code))
            };
            if row.2 == 0.0 {
                for (value, top) in out.iter_mut().zip(line(row.0)) {
                    *value = top;
                }
            } else {
                for ((value, top), bottom) in out.iter_mut().zip(line(row.0)).zip(line(row.1)) {
                    *value = top + (bottom - top) * row.2;
                }
            }
            return;
        }
        for (offset, value) in out.iter_mut().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let x = first + offset as u32;
            let top = self.at(x, row.0, component);
            *value = if row.2 == 0.0 {
                top
            } else {
                top + (self.at(x, row.1, component) - top) * row.2
            };
        }
    }
}

/// The presentation of an upright (or half-turned) source with no alpha
/// plane and no minification grid, by output rows: each column's and each
/// row's taps are worked out once, each output row's planes are filtered
/// vertically once, and every pixel then filters horizontally between them.
/// The same taps as the per-pixel path, summed in the other order; `None`
/// for any other presentation.
#[allow(clippy::too_many_lines)]
fn present_separable(
    renderer: &CpuRenderer,
    output: RenderSize,
    planes: &Planes<'_>,
    conversion: &YuvConversion,
) -> Option<CpuImage> {
    let geometry = &conversion.geometry;
    if conversion.alpha_plane || geometry.orientation & 1 != 0 || {
        #[allow(clippy::float_cmp)]
        let unfiltered = geometry.filter_grid() == [1.0, 1.0];
        !unfiltered
    } {
        return None;
    }
    let bounds = geometry.bounds(output);
    if bounds.is_empty() {
        return Some(CpuImage::transparent(output));
    }
    #[allow(clippy::cast_precision_loss)]
    let columns = (bounds.x..bounds.right())
        .map(|x| conversion.axis_tap(0, x as f32 + 0.5, planes.y.width, planes.u.width))
        .collect::<Vec<_>>();
    let all_inside = columns.iter().all(|column| column.inside);
    // The columns' taps relative to the first texel each plane's row holds.
    let (luma_base, chroma_base) = (
        columns.iter().map(|tap| tap.luma.0).min().unwrap_or(0),
        columns.iter().map(|tap| tap.chroma.0).min().unwrap_or(0),
    );
    let relative = |(first, second, weight): (u32, u32, f32), base: u32| {
        ((first - base) as usize, (second - base) as usize, weight)
    };
    let luma_taps = columns
        .iter()
        .map(|tap| relative(tap.luma, luma_base))
        .collect::<Vec<_>>();
    let chroma_taps = columns
        .iter()
        .map(|tap| relative(tap.chroma, chroma_base))
        .collect::<Vec<_>>();
    let alpha = conversion.alpha;
    let width = bounds.width as usize;
    // The texels the columns read, on each plane.
    let reach = |pick: fn(&AxisTap) -> (u32, u32, f32)| {
        let first = columns.iter().map(|tap| pick(tap).0).min().unwrap_or(0);
        let last = columns.iter().map(|tap| pick(tap).1).max().unwrap_or(0);
        (first, last)
    };
    let (luma_first, luma_last) = reach(|tap| tap.luma);
    let (chroma_first, chroma_last) = reach(|tap| tap.chroma);
    let mut pixels = vec![[0_u16; 4]; bounds.pixels()];
    renderer
        .workers
        .bands(&mut pixels, width, |first_row, band| {
            // One output row's planes filtered vertically, then each pixel
            // filtered horizontally between them.
            let mut luma = vec![0.0_f32; (luma_last - luma_first + 1) as usize];
            let chroma_len = (chroma_last - chroma_first + 1) as usize;
            let mut u = vec![0.0_f32; chroma_len];
            let mut v = vec![0.0_f32; chroma_len];
            let (mut sample_y, mut sample_u, mut sample_v) =
                (vec![0.0; width], vec![0.0; width], vec![0.0; width]);
            for (index, line) in band.chunks_mut(width).enumerate() {
                #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
                let y = (bounds.y as usize + first_row + index) as f32 + 0.5;
                let row = conversion.axis_tap(1, y, planes.y.height, planes.u.height);
                if !row.inside {
                    continue;
                }
                planes.y.vertical(row.luma, luma_first, 0, &mut luma);
                planes.u.vertical(row.chroma, chroma_first, 0, &mut u);
                if conversion.interleaved {
                    planes.u.vertical(row.chroma, chroma_first, 1, &mut v);
                } else if let Some(plane) = planes.v.as_ref() {
                    plane.vertical(row.chroma, chroma_first, 0, &mut v);
                }
                let between =
                    |values: &[f32], (first, second, weight): (u32, u32, f32), base: u32| {
                        let a = values[(first - base) as usize];
                        a + (values[(second - base) as usize] - a) * weight
                    };
                if conversion.encoded_identity && all_inside {
                    // The conversion inlined, a pass at a time over the row: the
                    // three samples, then range, matrix and clamp, as stored.
                    let [scale_y, scale_u, scale_v] = conversion.range_scale;
                    let [offset_y, offset_u, offset_v] = conversion.range_offset;
                    let [red, green, blue] = conversion.rows;
                    let alpha_bits = half::store(alpha);
                    lerp_into(&luma, &luma_taps, &mut sample_y);
                    lerp_into(&u, &chroma_taps, &mut sample_u);
                    lerp_into(&v, &chroma_taps, &mut sample_v);
                    for (((out, y), cb), cr) in
                        line.iter_mut().zip(&sample_y).zip(&sample_u).zip(&sample_v)
                    {
                        let y = y * scale_y + offset_y;
                        let cb = cb * scale_u + offset_u;
                        let cr = cr * scale_v + offset_v;
                        let channel = |row: [f32; 3]| {
                            half::store(
                                (row[0] * y + row[1] * cb + row[2] * cr).clamp(0.0, 1.0) * alpha,
                            )
                        };
                        *out = [channel(red), channel(green), channel(blue), alpha_bits];
                    }
                    continue;
                }
                for (out, column) in line.iter_mut().zip(&columns) {
                    if !column.inside {
                        continue;
                    }
                    let codes = [
                        between(&luma, column.luma, luma_first),
                        between(&u, column.chroma, chroma_first),
                        between(&v, column.chroma, chroma_first),
                    ];
                    let stored = conversion.stored_opaque(conversion.encoded_from(codes));
                    *out = half::store4(stored.map(|value| value * alpha));
                }
            }
        });
    Some(CpuImage::new(output, bounds, Pixels::Half(pixels)))
}

struct RgbaSource<'a> {
    bytes: &'a [u8],
    stride: usize,
    size: RenderSize,
    encoding: PresentedEncoding,
}

impl RgbaSource<'_> {
    /// The texel at `(x, y)` with the edge repeated, as the texture holds
    /// it: 8-bit codes, or half floats for linear-light sources.
    fn texel(&self, x: i64, y: i64) -> [f32; 4] {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (x, y) = (
            x.clamp(0, i64::from(self.size.width) - 1) as usize,
            y.clamp(0, i64::from(self.size.height) - 1) as usize,
        );
        if self.encoding == PresentedEncoding::Linear {
            let offset = y * self.stride + x * 8;
            std::array::from_fn(|channel| {
                half::load(u16::from_le_bytes([
                    self.bytes[offset + channel * 2],
                    self.bytes[offset + channel * 2 + 1],
                ]))
            })
        } else {
            let offset = y * self.stride + x * 4;
            std::array::from_fn(|channel| half::unorm8_value(self.bytes[offset + channel]))
        }
    }

    /// `filtered_sample` of rgba-present.wesl: a bilinear sample as
    /// premultiplied color in the space the texture is filtered in.
    fn filtered_sample(&self, source_pixel: [f32; 2]) -> [f32; 4] {
        let texel = [source_pixel[0] - 0.5, source_pixel[1] - 0.5];
        let origin = [texel[0].floor(), texel[1].floor()];
        let weight = [texel[0] - origin[0], texel[1] - origin[1]];
        #[allow(clippy::cast_possible_truncation)]
        let (x, y) = (origin[0] as i64, origin[1] as i64);
        let corners = [
            self.texel(x, y),
            self.texel(x + 1, y),
            self.texel(x, y + 1),
            self.texel(x + 1, y + 1),
        ];
        let mix = |a: [f32; 4], b: [f32; 4], t: f32| {
            [0, 1, 2, 3].map(|channel| a[channel] + (b[channel] - a[channel]) * t)
        };
        let bilinear = |texels: &[[f32; 4]; 4]| {
            mix(
                mix(texels[0], texels[1], weight[0]),
                mix(texels[2], texels[3], weight[0]),
                weight[1],
            )
        };
        if self.encoding != PresentedEncoding::Srgb(SourceAlpha::Straight) {
            return bilinear(&corners);
        }
        // Straight alpha is premultiplied before it is filtered.
        let premultiplied = corners.map(|texel| {
            [
                texel[0] * texel[3],
                texel[1] * texel[3],
                texel[2] * texel[3],
                texel[3],
            ]
        });
        if premultiplied.iter().all(|texel| texel[3] >= 1.0) {
            let source = bilinear(&corners);
            return [
                source[0] * source[3],
                source[1] * source[3],
                source[2] * source[3],
                source[3],
            ];
        }
        bilinear(&premultiplied)
    }

    /// `stored_sample`.
    fn stored_sample(&self, source_pixel: [f32; 2]) -> [f32; 4] {
        let filtered = self.filtered_sample(source_pixel);
        if self.encoding != PresentedEncoding::Linear {
            return filtered;
        }
        if filtered[3] <= 0.0 {
            return [0.0; 4];
        }
        let alpha = filtered[3];
        [
            extended_linear_to_srgb(filtered[0] / alpha) * alpha,
            extended_linear_to_srgb(filtered[1] / alpha) * alpha,
            extended_linear_to_srgb(filtered[2] / alpha) * alpha,
            alpha,
        ]
    }
}

fn present_rgba(renderer: &CpuRenderer, source: &RgbaSource<'_>, geometry: &Geometry) -> CpuImage {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let output = RenderSize {
        width: geometry.output[0] as u32,
        height: geometry.output[1] as u32,
    };
    present_rows(renderer, output, geometry, |destination| {
        geometry.filtered(destination, |source_pixel| {
            source.stored_sample(source_pixel)
        })
    })
}

impl CpuRenderer {
    /// A source showing decoded YCbCr `planes` as `presentation` places
    /// them, converted the first time a graph draws it: the CPU counterpart
    /// of [`crate::Renderer::prepare_yuv_presented`].
    ///
    /// # Errors
    /// Returns [`RenderError::InvalidImageData`] for an unsupported format
    /// or planes that do not hold the frame, and
    /// [`RenderError::UnsupportedHdr`] when [`ColorPolicy::RejectHdr`] meets
    /// an HDR source.
    pub fn present_yuv(
        &self,
        planes: Arc<dyn YuvPlanes>,
        presentation: SourcePresentation,
        policy: ColorPolicy,
    ) -> Result<(CpuSource, Vec<ColorDiagnostic>), RenderError> {
        let frame = planes.frame();
        let mut diagnostics = Vec::new();
        let color = frame.color.unwrap_or_else(|| {
            diagnostics.push(ColorDiagnostic::MissingMetadataFallback);
            ColorMetadata::UNDECLARED
        });
        let tone: Option<HdrToneMap> = match hdr_handling(color, policy, &mut diagnostics)? {
            HdrHandling::Sdr => None,
            HdrHandling::ToneMap(tone) => Some(tone),
            HdrHandling::Skip => {
                return Ok((
                    CpuSource::image(Arc::new(CpuImage::transparent(presentation.output_size))),
                    diagnostics,
                ));
            }
        };
        let format = frame.format;
        if !frame.alpha.is_finite()
            || !(0.0..=1.0).contains(&frame.alpha)
            || !format.is_supported()
            || format.alpha != frame.alpha_plane.is_some()
        {
            return Err(RenderError::InvalidImageData);
        }
        validate_presentation(frame.size, presentation)?;
        let (chroma_width, chroma_height) = format.chroma_size(frame.size.width, frame.size.height);
        let pairs = if format.interleaved_chroma { 2 } else { 1 };
        let specs = [
            (frame.size.width, frame.size.height, 1_u32),
            (chroma_width, chroma_height, pairs),
            (chroma_width, chroma_height, 1),
        ];
        for (plane, (width, height, samples)) in frame
            .planes
            .iter()
            .zip(specs)
            .take(format.color_plane_count())
            .chain(
                frame
                    .alpha_plane
                    .iter()
                    .map(|plane| (plane, (frame.size.width, frame.size.height, 1))),
            )
        {
            let row_bytes = width
                .checked_mul(samples)
                .and_then(|row| row.checked_mul(format.bytes_per_sample()))
                .ok_or(RenderError::InvalidImageData)?;
            crate::yuv::validate_plane(plane.bytes, plane.stride, row_bytes, height)?;
        }
        let storage = if format.bytes_per_sample() == 1 {
            PlaneStorage::Unorm8
        } else {
            PlaneStorage::Unorm16
        };
        #[allow(clippy::cast_precision_loss)]
        let bytes = uniform_bytes(
            color,
            tone,
            frame.alpha,
            frame.size,
            presentation,
            format,
            storage.code_scale(format),
            self.quality_controls().max_filter_samples as f32,
        );
        let conversion = Box::new(YuvConversion::from_uniform(&bytes, format));
        Ok((
            CpuSource::new(Content::Yuv { planes, conversion }),
            diagnostics,
        ))
    }

    /// A source showing RGBA8 `pixels` of `size`, rows `stride` bytes
    /// apart, as `presentation` places them: the CPU counterpart of
    /// [`crate::Renderer::prepare_rgba_presented`]. A `stride` of zero means
    /// rows packed.
    ///
    /// # Errors
    /// Returns [`RenderError::InvalidImageData`] for pixels that do not hold
    /// `size` or an invalid presentation.
    pub fn present_rgba8(
        &self,
        pixels: Arc<dyn RgbaPixels>,
        size: RenderSize,
        stride: u32,
        presentation: SourcePresentation,
        alpha: SourceAlpha,
    ) -> Result<CpuSource, RenderError> {
        self.present_rgba(
            pixels,
            size,
            stride,
            4,
            presentation,
            PresentedEncoding::Srgb(alpha),
        )
    }

    /// A source showing premultiplied linear-light RGBA as half-float bits,
    /// in BT.709 primaries with `1.0` at SDR white: the CPU counterpart of
    /// [`crate::Renderer::prepare_linear_presented`]. A `stride` of zero
    /// means rows packed.
    ///
    /// # Errors
    /// Returns [`RenderError::InvalidImageData`] for pixels that do not hold
    /// `size` or an invalid presentation.
    pub fn present_linear_rgba16f(
        &self,
        pixels: Arc<dyn RgbaPixels>,
        size: RenderSize,
        stride: u32,
        presentation: SourcePresentation,
    ) -> Result<CpuSource, RenderError> {
        self.present_rgba(
            pixels,
            size,
            stride,
            8,
            presentation,
            PresentedEncoding::Linear,
        )
    }

    fn present_rgba(
        &self,
        pixels: Arc<dyn RgbaPixels>,
        size: RenderSize,
        stride: u32,
        bytes_per_pixel: u32,
        presentation: SourcePresentation,
        encoding: PresentedEncoding,
    ) -> Result<CpuSource, RenderError> {
        validate_presentation(size, presentation)?;
        let row_bytes = size
            .width
            .checked_mul(bytes_per_pixel)
            .ok_or(RenderError::InvalidImageData)?;
        let stride = if stride == 0 { row_bytes } else { stride };
        crate::yuv::validate_plane(pixels.plane().bytes, stride, row_bytes, size.height)?;
        #[allow(clippy::cast_precision_loss)]
        let bytes = presentation_uniform_bytes(
            size,
            presentation,
            encoding,
            self.quality_controls().max_filter_samples as f32,
        );
        Ok(CpuSource::new(Content::Rgba {
            pixels,
            stride,
            size,
            geometry: Geometry::from_uniform(&bytes),
            encoding,
        }))
    }
}
