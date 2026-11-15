//! CPU graph sources, and the CPU port of yuv-to-linear.wesl and rgba-present.wesl that turns a
//! decoded frame into a source picture.
//!
//! Planes are not copied: a source holds the decoder's buffers and converts on first draw.

use std::fmt;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use alpha2_media::{ColorMetadata, YuvFormat};

use super::CpuRenderer;
use super::curves::{self, extended_linear_to_srgb, linear_to_srgb};
use super::half;
use super::image::{CpuImage, PixelBox};
use super::renderer::add_elapsed;
use crate::hdr::HdrToneMap;
use crate::yuv::{
    HdrHandling, PlaneStorage, PresentedEncoding, hdr_handling, presentation_uniform_bytes,
    uniform_bytes, validate_presentation,
};
use crate::{
    ColorDiagnostic, ColorPolicy, RenderError, RenderSize, SourceAlpha, SourcePlane,
    SourcePresentation, YuvFrame,
};

/// Decoded YCbCr planes owned by the decoder and kept unchanged while any source holds them.
pub trait YuvPlanes: Send + Sync + fmt::Debug {
    /// The frame the planes hold.
    fn frame(&self) -> YuvFrame<'_>;
}

/// RGBA pixels a source converts from: 8-bit, or half-float bits for linear sources.
pub trait RgbaPixels: Send + Sync + fmt::Debug {
    /// The pixel bytes, rows `stride` bytes apart.
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

/// A CPU graph source ([`crate::SourceView::cpu`]) holding premultiplied sRGB RGBA, like every
/// GPU source texture. Decoded frames convert on first draw and the result is kept.
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

    /// Bytes held: the picture once presented.
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
        let started = web_time::Instant::now();
        let image = Arc::new(match &self.inner.content {
            Content::Image(_) => unreachable!("handled above"),
            Content::Yuv { planes, conversion } => {
                present_yuv(renderer, &planes.frame(), conversion, Path::Fastest)
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
                Path::Fastest,
            ),
        });
        add_elapsed(&renderer.stats.present_micros, started);
        renderer
            .stats
            .presented_sources
            .fetch_add(1, Ordering::Relaxed);
        Arc::clone(self.inner.presented.get_or_init(|| image))
    }

    /// The picture drawn by the table path where it applies (`tables`), or by the per-pixel
    /// shader port, which the table path must equal.
    #[cfg(test)]
    pub(crate) fn presented_by(&self, renderer: &CpuRenderer, tables: bool) -> CpuImage {
        let path = if tables { Path::Tables } else { Path::PerPixel };
        match &self.inner.content {
            Content::Image(image) => (**image).clone(),
            Content::Yuv { planes, conversion } => {
                present_yuv(renderer, &planes.frame(), conversion, path)
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
                path,
            ),
        }
    }
}

/// Presentation uniforms parsed from the same bytes the GPU path binds, so both paths match.
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
    /// SDR BT.709 without an alpha plane: decoding and re-encoding with sRGB is the identity, so
    /// the encoded value is kept.
    encoded_identity: bool,
    /// The HDR policy skipped the source, so it shows nothing.
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
    /// Parses `YuvUniforms` (yuv-to-linear.wesl) as `uniform_bytes` writes them.
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

    /// Output pixels inside the drawn rectangle; everything else is transparent.
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

    /// `source_pixel_at`: the stored pixel shown at output `destination`, clamped to the visible
    /// rect.
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

    /// The source axis that output axis `axis` (0 = x) drives.
    const fn source_axis(&self, axis: usize) -> usize {
        if self.orientation & 1 == 0 {
            axis
        } else {
            1 - axis
        }
    }

    /// The coordinate along [`Self::source_axis`] that [`Self::source_pixel_at`] gives output
    /// position `destination` on `axis`, with the same arithmetic.
    fn source_coordinate(&self, axis: usize, destination: f32) -> f32 {
        let local = (destination - self.draw[axis]) / self.draw[axis + 2];
        let reflected = match (self.orientation & 3, axis) {
            (1, 0) | (2, _) | (3, 1) => 1.0 - local,
            _ => local,
        };
        let source_axis = self.source_axis(axis);
        let value = if source_axis == 0 && self.orientation & 4 != 0 {
            1.0 - reflected
        } else {
            reflected
        };
        let origin = self.visible[source_axis];
        let extent = self.visible[source_axis + 2];
        (origin + value * extent).clamp(origin + 0.5, origin + extent - 0.5)
    }

    /// Whether output position `destination` on `axis` is inside the drawn rectangle.
    fn inside_axis(&self, axis: usize, destination: f32) -> bool {
        destination >= self.draw[axis] && destination < self.draw[axis] + self.draw[axis + 2]
    }

    /// The offsets of `filtered`'s samples from a pixel centre along `axis`, in grid order.
    fn grid_offsets(grid: [f32; 2], axis: usize) -> Vec<f32> {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        (0..grid[axis] as u32)
            .map(|index| {
                #[allow(clippy::cast_precision_loss)]
                let offset = (index as f32 + 0.5) / grid[axis] - 0.5;
                offset
            })
            .collect()
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

/// Converts the presented picture row by row with `pixel` into a stored picture.
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
    let mut pixels = renderer.frame_pool.take(bounds.pixels());
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
    CpuImage::pooled(size, bounds, pixels, &renderer.frame_pool)
}

/// One plane sampled like a clamp-to-edge texture: 8-bit codes as `code / 255`, 16-bit
/// containers as `container / 65535`.
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

    /// Bilinear sample at normalized `position`.
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

/// The transfer functions the presenter evaluates per sample.
trait Transfer {
    fn srgb_eotf(value: f32) -> f32;
    fn pq_eotf(code: f32) -> f32;
    fn pq_inverse_eotf(nits: f32) -> f32;
    fn hlg_inverse_oetf(code: f32) -> f32;
    fn pow(base: f32, exponent: f32) -> f32;
}

/// The presenter's curves: `libm`-built tables, identical on every target.
struct Tabled;

impl Transfer for Tabled {
    #[inline]
    fn srgb_eotf(value: f32) -> f32 {
        curves::srgb_to_linear(value.clamp(0.0, 1.0))
    }

    #[inline]
    fn pq_eotf(code: f32) -> f32 {
        curves::pq_eotf(code)
    }

    #[inline]
    fn pq_inverse_eotf(nits: f32) -> f32 {
        curves::pq_inverse_eotf(nits)
    }

    #[inline]
    fn hlg_inverse_oetf(code: f32) -> f32 {
        curves::hlg_inverse_oetf(code)
    }

    #[inline]
    fn pow(base: f32, exponent: f32) -> f32 {
        curves::table_pow(curves::pow_tables(), base, exponent)
    }
}

/// The references the tables are built from.
#[cfg(test)]
struct Exact;

#[cfg(test)]
impl Transfer for Exact {
    fn srgb_eotf(value: f32) -> f32 {
        curves::exact_srgb_to_linear(value.clamp(0.0, 1.0))
    }

    fn pq_eotf(code: f32) -> f32 {
        crate::hdr::pq_eotf(code)
    }

    fn pq_inverse_eotf(nits: f32) -> f32 {
        crate::hdr::pq_inverse_eotf(nits)
    }

    fn hlg_inverse_oetf(code: f32) -> f32 {
        crate::hdr::hlg_inverse_oetf(code)
    }

    #[allow(clippy::cast_possible_truncation)]
    fn pow(base: f32, exponent: f32) -> f32 {
        libm::pow(f64::from(base), f64::from(exponent)) as f32
    }
}

impl YuvConversion {
    fn bt2390_eetf<T: Transfer>(&self, nits: f32) -> f32 {
        let source = self.tone[2];
        let value = T::pq_inverse_eotf(nits.min(self.tone[0])) / source;
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
        T::pq_eotf(mapped * source)
    }

    fn to_bt709(&self, rgb: [f32; 3]) -> [f32; 3] {
        self.gamut
            .map(|row| row[0] * rgb[0] + row[1] * rgb[1] + row[2] * rgb[2])
    }

    fn hdr_to_working<T: Transfer>(&self, encoded: [f32; 3]) -> [f32; 3] {
        let nits = if self.transfer == 2 {
            let scene = encoded.map(T::hlg_inverse_oetf);
            let luma = (0.2627 * scene[0] + 0.678 * scene[1] + 0.0593 * scene[2]).max(0.0);
            let gain = self.tone[0] * T::pow(luma.max(1.0e-10), self.hlg_gamma - 1.0);
            scene.map(|value| value * gain)
        } else {
            encoded.map(T::pq_eotf)
        };
        let converted = self.to_bt709(nits);
        let largest = converted[0].max(converted[1].max(converted[2]));
        let scale = if largest > 0.0 {
            self.bt2390_eetf::<T>(largest) / largest
        } else {
            0.0
        };
        converted.map(|value| (value * scale / self.tone[1]).clamp(0.0, 1.0))
    }

    /// YCbCr at a luma and chroma position, decoded by range and matrix to sRGB-encoded R'G'B'.
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

    /// Sampled codes decoded by range and matrix to sRGB-encoded R'G'B'.
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

    /// Encoded R'G'B' converted to the linear working space.
    fn linear_from_encoded(&self, encoded: [f32; 3]) -> [f32; 3] {
        self.linear_from_encoded_with::<Tabled>(encoded)
    }

    fn linear_from_encoded_with<T: Transfer>(&self, encoded: [f32; 3]) -> [f32; 3] {
        if self.transfer != 0 {
            return self.hdr_to_working::<T>(encoded.map(|value| value.clamp(0.0, 1.0)));
        }
        self.to_bt709(encoded.map(T::srgb_eotf))
            .map(|value| value.clamp(0.0, 1.0))
    }

    /// `coordinates_at`: luma and chroma positions of `source_pixel`.
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

    /// `premultiplied_linear`: straight alpha premultiplied before filtering.
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
        // Four clear texels mix to zero whatever their color, and zero alpha stores as zero.
        if alphas.iter().all(|alpha| *alpha <= 0.0) {
            return [0.0; 4];
        }
        let texels: [[f32; 4]; 4] = std::array::from_fn(|index| {
            let ((luma, chroma), alpha) = (corners[index], alphas[index]);
            let rgb = self.linear_rgb(planes, luma, chroma);
            [rgb[0] * alpha, rgb[1] * alpha, rgb[2] * alpha, alpha]
        });
        let mix = |a: [f32; 4], b: [f32; 4], t: f32| {
            [0, 1, 2, 3].map(|channel| a[channel] * (1.0 - t) + b[channel] * t)
        };
        mix(
            mix(texels[0], texels[1], weight[0]),
            mix(texels[2], texels[3], weight[0]),
            weight[1],
        )
    }

    /// `stored_sample`: one position as the intermediate stores presented sources (sRGB-encoded,
    /// then premultiplied).
    fn stored_sample(&self, planes: &Planes<'_>, source_pixel: [f32; 2]) -> [f32; 4] {
        if !self.alpha_plane {
            let (luma, chroma) = self.coordinates(source_pixel);
            return self.stored_opaque(self.encoded(planes, luma, chroma));
        }
        Self::stored_linear(self.premultiplied_linear(planes, source_pixel))
    }

    /// An opaque sample's encoded R'G'B' as stored.
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

    /// Premultiplied linear light as stored.
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

/// `encode_srgb` of yuv-to-linear.wesl; the input is already clamped.
fn encode_srgb(value: f32) -> f32 {
    linear_to_srgb(value)
}

struct Planes<'a> {
    y: Plane<'a>,
    u: Plane<'a>,
    v: Option<Plane<'a>>,
    alpha: Option<Plane<'a>>,
}

impl<'a> Planes<'a> {
    /// The planes of `frame`, stored as `format` says.
    fn of(frame: &YuvFrame<'a>, format: YuvFormat) -> Self {
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
        let wide = format.bytes_per_sample() == 2;
        let (chroma_width, chroma_height) = format.chroma_size(frame.size.width, frame.size.height);
        Self {
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
        }
    }
}

/// How `present_yuv` converts: the fastest path that applies, the table path alone, or the
/// per-pixel shader port the table path must match bit for bit.
#[derive(Clone, Copy, PartialEq)]
enum Path {
    Fastest,
    #[cfg(test)]
    Tables,
    #[cfg(test)]
    PerPixel,
}

impl Path {
    /// Whether the table paths may run.
    const fn tables(self) -> bool {
        match self {
            #[cfg(test)]
            Self::PerPixel => false,
            _ => true,
        }
    }
}

fn present_yuv(
    renderer: &CpuRenderer,
    frame: &YuvFrame<'_>,
    conversion: &YuvConversion,
    path: Path,
) -> CpuImage {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let output = RenderSize {
        width: conversion.geometry.output[0] as u32,
        height: conversion.geometry.output[1] as u32,
    };
    if conversion.skipped {
        return CpuImage::transparent(output);
    }
    let planes = Planes::of(frame, conversion.format);
    if path == Path::Fastest
        && let Some(image) = present_separable(renderer, output, &planes, conversion)
    {
        return image;
    }
    if path.tables()
        && let Some(image) = present_tables(renderer, output, &planes, conversion)
    {
        return image;
    }
    let geometry = conversion.geometry;
    let alpha = conversion.alpha;
    present_rows(renderer, output, &geometry, |destination| {
        geometry
            .filtered(destination, |source_pixel| {
                conversion.stored_sample(&planes, source_pixel)
            })
            .map(|value| value * alpha)
    })
}

/// Filters each output column between the two vertically filtered texels its tap names.
fn lerp_into(values: &[f32], taps: &[(usize, usize, f32)], out: &mut [f32]) {
    for (out, &(first, second, weight)) in out.iter_mut().zip(taps) {
        let a = values[first];
        *out = a + (values[second] - a) * weight;
    }
}

/// A sample's texels either side and the weight between, along one source axis.
type Tap = (u32, u32, f32);

/// Where one output column or row reads along the source axis it drives: texels either side and
/// the weight between, for luma and chroma.
#[derive(Clone, Copy, Default)]
struct AxisTap {
    inside: bool,
    luma: Tap,
    chroma: Tap,
}

/// The texels a bilinear sample at normalized `position` reads along an axis `size` texels long,
/// clamped to the edge as `Plane::sample` clamps them.
fn plane_tap(position: f32, size: u32) -> Tap {
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
}

impl YuvConversion {
    /// Taps of output `destination` along output `axis` (0 = x), using exactly the arithmetic of
    /// `source_pixel_at`, `coordinates_at` and a bilinear sample, which are separable: each
    /// output axis drives one source axis.
    fn axis_tap(&self, axis: usize, destination: f32, planes: &Planes<'_>) -> AxisTap {
        let geometry = &self.geometry;
        let source = geometry.source_axis(axis);
        let source_pixel = geometry.source_coordinate(axis, destination);
        let chroma_pixel = ((source_pixel + self.chroma_offset[source])
            * self.chroma_scale[source])
            .clamp(self.chroma_window[source], self.chroma_window[source + 2]);
        let (luma_size, chroma_size) = if source == 0 {
            (planes.y.width, planes.u.width)
        } else {
            (planes.y.height, planes.u.height)
        };
        AxisTap {
            inside: geometry.inside_axis(axis, destination),
            luma: plane_tap(source_pixel / geometry.source[source], luma_size),
            chroma: plane_tap(chroma_pixel / self.chroma_size[source], chroma_size),
        }
    }
}

impl Plane<'_> {
    /// Normalized `component` of texel `x` in row `y`.
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

    /// `component` of texels from `first` in the rows `row` names, filtered between them, into
    /// `out`.
    fn vertical(&self, row: Tap, first: u32, component: usize, out: &mut [f32]) {
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

/// Row-wise fast path for an upright or half-turned source with no alpha plane or
/// minification: taps are computed once per column and row, planes filtered vertically once
/// per output row, then horizontally per pixel. Same taps as the per-pixel path, summed in the
/// other order; `None` for other presentations.
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
        .map(|x| conversion.axis_tap(0, x as f32 + 0.5, planes))
        .collect::<Vec<_>>();
    let all_inside = columns.iter().all(|column| column.inside);
    // Column taps relative to the first texel of each plane's row.
    let (luma_base, chroma_base) = (
        columns.iter().map(|tap| tap.luma.0).min().unwrap_or(0),
        columns.iter().map(|tap| tap.chroma.0).min().unwrap_or(0),
    );
    let relative = |(first, second, weight): Tap, base: u32| {
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
    let reach = |pick: fn(&AxisTap) -> Tap| {
        let first = columns.iter().map(|tap| pick(tap).0).min().unwrap_or(0);
        let last = columns.iter().map(|tap| pick(tap).1).max().unwrap_or(0);
        (first, last)
    };
    let (luma_first, luma_last) = reach(|tap| tap.luma);
    let (chroma_first, chroma_last) = reach(|tap| tap.chroma);
    let chroma_len = (chroma_last - chroma_first + 1) as usize;
    let scratch = || {
        [
            vec![0.0_f32; (luma_last - luma_first + 1) as usize],
            vec![0.0_f32; chroma_len],
            vec![0.0_f32; chroma_len],
            vec![0.0_f32; width],
            vec![0.0_f32; width],
            vec![0.0_f32; width],
        ]
    };
    let mut pixels = renderer.frame_pool.take(bounds.pixels());
    renderer
        .workers
        .bands_with(&mut pixels, width, scratch, |scratch, first_row, band| {
            // Filter one output row's planes vertically, then each pixel horizontally.
            band.fill([0; 4]);
            for buffer in scratch.iter_mut() {
                buffer.fill(0.0);
            }
            let [luma, u, v, sample_y, sample_u, sample_v] =
                scratch.each_mut().map(Vec::as_mut_slice);
            for (index, line) in band.chunks_mut(width).enumerate() {
                #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
                let y = (bounds.y as usize + first_row + index) as f32 + 0.5;
                let row = conversion.axis_tap(1, y, planes);
                if !row.inside {
                    continue;
                }
                planes.y.vertical(row.luma, luma_first, 0, luma);
                planes.u.vertical(row.chroma, chroma_first, 0, u);
                if conversion.interleaved {
                    planes.u.vertical(row.chroma, chroma_first, 1, v);
                } else if let Some(plane) = planes.v.as_ref() {
                    plane.vertical(row.chroma, chroma_first, 0, v);
                }
                let between = |values: &[f32], (first, second, weight): Tap, base: u32| {
                    let a = values[(first - base) as usize];
                    a + (values[(second - base) as usize] - a) * weight
                };
                if conversion.encoded_identity && all_inside {
                    // Inlined conversion, one pass at a time: samples, then range, matrix and
                    // clamp.
                    let [scale_y, scale_u, scale_v] = conversion.range_scale;
                    let [offset_y, offset_u, offset_v] = conversion.range_offset;
                    let [red, green, blue] = conversion.rows;
                    let alpha_bits = half::store(alpha);
                    lerp_into(luma, &luma_taps, sample_y);
                    lerp_into(u, &chroma_taps, sample_u);
                    lerp_into(v, &chroma_taps, sample_v);
                    for (((out, y), cb), cr) in line
                        .iter_mut()
                        .zip(sample_y.iter())
                        .zip(sample_u.iter())
                        .zip(sample_v.iter())
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
                        between(luma, column.luma, luma_first),
                        between(u, column.chroma, chroma_first),
                        between(v, column.chroma, chroma_first),
                    ];
                    let stored = conversion.stored_opaque(conversion.encoded_from(codes));
                    *out = half::store4(stored.map(|value| value * alpha));
                }
            }
        });
    Some(CpuImage::pooled(
        output,
        bounds,
        pixels,
        &renderer.frame_pool,
    ))
}

/// Taps of every filter sample of every output position along one output axis.
struct AxisTable {
    /// Per output position, whether its centre is inside the drawn rectangle.
    inside: Vec<bool>,
    /// Per output position, one tap per sample in grid order.
    taps: Vec<AxisTap>,
    samples: usize,
}

impl AxisTable {
    /// `tap` at each of `offsets` from the centre of each position `first..first + count`.
    fn new(
        geometry: &Geometry,
        axis: usize,
        (first, count): (u32, u32),
        offsets: &[f32],
        tap: impl Fn(usize, f32) -> AxisTap,
    ) -> Self {
        let mut inside = Vec::with_capacity(count as usize);
        let mut taps = Vec::with_capacity(count as usize * offsets.len());
        for position in first..first + count {
            #[allow(clippy::cast_precision_loss)]
            let destination = position as f32 + 0.5;
            inside.push(geometry.inside_axis(axis, destination));
            taps.extend(offsets.iter().map(|offset| tap(axis, destination + offset)));
        }
        Self {
            inside,
            taps,
            samples: offsets.len(),
        }
    }

    fn at(&self, position: usize) -> &[AxisTap] {
        &self.taps[position * self.samples..(position + 1) * self.samples]
    }

    /// The first and last texel any tap reads, for luma and chroma.
    fn reach(&self) -> [(u32, u32); 2] {
        let span = |pick: fn(&AxisTap) -> Tap| {
            let first = self.taps.iter().map(|tap| pick(tap).0).min().unwrap_or(0);
            let last = self.taps.iter().map(|tap| pick(tap).1).max().unwrap_or(0);
            (first, last)
        };
        [span(|tap| tap.luma), span(|tap| tap.chroma)]
    }
}

/// The line of texels a fixed tap selects across the other source axis, decoded once.
struct Lines {
    /// Texels `first..` of the two lines the tap names (rows when upright, columns when turned).
    near: Vec<f32>,
    far: Vec<f32>,
    first: u32,
}

impl Lines {
    fn new(first: u32, last: u32) -> Self {
        let length = (last - first + 1) as usize;
        Self {
            near: vec![0.0; length],
            far: vec![0.0; length],
            first,
        }
    }

    /// Decodes rows `tap.0` and `tap.1` of `component`.
    fn rows(&mut self, plane: &Plane<'_>, tap: Tap, component: usize) {
        for (offset, (near, far)) in self.near.iter_mut().zip(self.far.iter_mut()).enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let x = self.first + offset as u32;
            *near = plane.at(x, tap.0, component);
            *far = plane.at(x, tap.1, component);
        }
    }

    /// Decodes columns `tap.0` and `tap.1` of `component` and lerps between them, as
    /// `Plane::sample` lerps horizontally before it lerps vertically.
    fn columns(&mut self, plane: &Plane<'_>, tap: Tap, component: usize) {
        for (offset, near) in self.near.iter_mut().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let y = self.first + offset as u32;
            let left = plane.at(tap.0, y, component);
            *near = left + (plane.at(tap.1, y, component) - left) * tap.2;
        }
    }

    /// `Plane::sample` from rows decoded by [`Self::rows`], at column tap `across` and `weight`.
    #[inline]
    fn between_rows(&self, across: Tap, weight: f32) -> f32 {
        let (left, right) = (
            (across.0 - self.first) as usize,
            (across.1 - self.first) as usize,
        );
        let top = self.near[left] + (self.near[right] - self.near[left]) * across.2;
        let bottom = self.far[left] + (self.far[right] - self.far[left]) * across.2;
        top + (bottom - top) * weight
    }

    /// `Plane::sample` from columns lerped by [`Self::columns`], at row tap `across`.
    #[inline]
    fn between_columns(&self, across: Tap) -> f32 {
        let (top, bottom) = (
            (across.0 - self.first) as usize,
            (across.1 - self.first) as usize,
        );
        let top_value = self.near[top];
        top_value + (self.near[bottom] - top_value) * across.2
    }
}

/// Table path for the presentations `present_separable` declines, except alpha planes:
/// quarter turns and minification. Each output axis drives one source axis, so one table per
/// output axis holds every sample's taps; for each output row and filter row the two source
/// lines it reads are decoded once. Every sample is evaluated in the per-pixel path's order
/// (horizontal lerp, vertical lerp, conversion, grid sum), so the bits are the same.
#[allow(clippy::too_many_lines)]
fn present_tables(
    renderer: &CpuRenderer,
    output: RenderSize,
    planes: &Planes<'_>,
    conversion: &YuvConversion,
) -> Option<CpuImage> {
    if conversion.alpha_plane {
        return None;
    }
    let geometry = &conversion.geometry;
    let bounds = geometry.bounds(output);
    if bounds.is_empty() {
        return Some(CpuImage::transparent(output));
    }
    let grid = geometry.filter_grid();
    #[allow(clippy::float_cmp)]
    let single = grid == [1.0, 1.0];
    let count = grid[0] * grid[1];
    let tap = |axis, destination| conversion.axis_tap(axis, destination, planes);
    let columns = AxisTable::new(
        geometry,
        0,
        (bounds.x, bounds.width),
        &Geometry::grid_offsets(grid, 0),
        tap,
    );
    let rows = AxisTable::new(
        geometry,
        1,
        (bounds.y, bounds.height),
        &Geometry::grid_offsets(grid, 1),
        tap,
    );
    // Upright and half-turned rows fix a source row pair; quarter-turned rows fix a column pair.
    let turned = geometry.source_axis(1) == 0;
    let [(luma_first, luma_last), (chroma_first, chroma_last)] = columns.reach();
    let alpha = conversion.alpha;
    let width = bounds.width as usize;
    let scratch = || {
        (
            [
                Lines::new(luma_first, luma_last),
                Lines::new(chroma_first, chroma_last),
                Lines::new(chroma_first, chroma_last),
            ],
            vec![[0.0_f32; 4]; width],
        )
    };
    let mut pixels = renderer.frame_pool.take(bounds.pixels());
    renderer.workers.bands_with(
        &mut pixels,
        width,
        scratch,
        |([luma, u, v], sums), first_row, band| {
            band.fill([0; 4]);
            for (index, line) in band.chunks_mut(width).enumerate() {
                let row = first_row + index;
                if !rows.inside[row] {
                    continue;
                }
                sums.fill([0.0; 4]);
                for row_tap in rows.at(row) {
                    if turned {
                        luma.columns(&planes.y, row_tap.luma, 0);
                        u.columns(&planes.u, row_tap.chroma, 0);
                    } else {
                        luma.rows(&planes.y, row_tap.luma, 0);
                        u.rows(&planes.u, row_tap.chroma, 0);
                    }
                    let v_plane = if conversion.interleaved {
                        Some((&planes.u, 1))
                    } else {
                        planes.v.as_ref().map(|plane| (plane, 0))
                    };
                    if let Some((plane, component)) = v_plane {
                        if turned {
                            v.columns(plane, row_tap.chroma, component);
                        } else {
                            v.rows(plane, row_tap.chroma, component);
                        }
                    }
                    for (offset, (out, sum)) in line.iter_mut().zip(sums.iter_mut()).enumerate() {
                        if !columns.inside[offset] {
                            continue;
                        }
                        for column_tap in columns.at(offset) {
                            let sample = |lines: &Lines, across: Tap, weight: f32| {
                                if turned {
                                    lines.between_columns(across)
                                } else {
                                    lines.between_rows(across, weight)
                                }
                            };
                            let codes = [
                                sample(luma, column_tap.luma, row_tap.luma.2),
                                sample(u, column_tap.chroma, row_tap.chroma.2),
                                if v_plane.is_some() {
                                    sample(v, column_tap.chroma, row_tap.chroma.2)
                                } else {
                                    0.0
                                },
                            ];
                            let value = conversion.stored_opaque(conversion.encoded_from(codes));
                            if single {
                                *out = half::store4(value.map(|channel| channel * alpha));
                            } else {
                                for (total, channel) in sum.iter_mut().zip(value) {
                                    *total += channel;
                                }
                            }
                        }
                    }
                }
                if !single {
                    for (offset, (out, sum)) in line.iter_mut().zip(sums.iter()).enumerate() {
                        if columns.inside[offset] {
                            *out = half::store4(sum.map(|total| total / count * alpha));
                        }
                    }
                }
            }
        },
    );
    Some(CpuImage::pooled(
        output,
        bounds,
        pixels,
        &renderer.frame_pool,
    ))
}

struct RgbaSource<'a> {
    bytes: &'a [u8],
    stride: usize,
    size: RenderSize,
    encoding: PresentedEncoding,
}

impl RgbaSource<'_> {
    /// Texel at `(x, y)` with edge repeat: 8-bit codes, or half floats for linear sources.
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

    /// `filtered_sample` of rgba-present.wesl: bilinear premultiplied color in the filtering space.
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
        self.filtered_corners(&corners, weight)
    }

    /// The texels a bilinear sample at source coordinate `position` reads along an axis `size`
    /// texels long, clamped as [`Self::texel`] clamps them.
    fn axis_tap(position: f32, size: u32) -> Tap {
        let texel = position - 0.5;
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
    }

    /// `filtered_sample` from its four texels (left to right, then top to bottom) and weights.
    #[inline]
    fn filtered_corners(&self, corners: &[[f32; 4]; 4], weight: [f32; 2]) -> [f32; 4] {
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
            return bilinear(corners);
        }
        // Straight alpha is premultiplied before filtering.
        let premultiplied = corners.map(|texel| {
            [
                texel[0] * texel[3],
                texel[1] * texel[3],
                texel[2] * texel[3],
                texel[3],
            ]
        });
        if premultiplied.iter().all(|texel| texel[3] >= 1.0) {
            let source = bilinear(corners);
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
        self.stored(self.filtered_sample(source_pixel))
    }

    /// A filtered sample as stored.
    #[inline]
    fn stored(&self, filtered: [f32; 4]) -> [f32; 4] {
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

fn present_rgba(
    renderer: &CpuRenderer,
    source: &RgbaSource<'_>,
    geometry: &Geometry,
    path: Path,
) -> CpuImage {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let output = RenderSize {
        width: geometry.output[0] as u32,
        height: geometry.output[1] as u32,
    };
    if path.tables() {
        return present_rgba_tables(renderer, output, source, geometry);
    }
    present_rows(renderer, output, geometry, |destination| {
        geometry.filtered(destination, |source_pixel| {
            source.stored_sample(source_pixel)
        })
    })
}

/// Two lines of RGBA texels a fixed tap selects across the other source axis.
struct RgbaLines {
    near: Vec<[f32; 4]>,
    far: Vec<[f32; 4]>,
    first: u32,
}

/// [`present_tables`] for RGBA sources: the same tables, with each sample's four texels read from
/// two decoded lines and filtered by `filtered_sample`'s own arithmetic.
#[allow(clippy::too_many_lines)]
fn present_rgba_tables(
    renderer: &CpuRenderer,
    output: RenderSize,
    source: &RgbaSource<'_>,
    geometry: &Geometry,
) -> CpuImage {
    let bounds = geometry.bounds(output);
    if bounds.is_empty() {
        return CpuImage::transparent(output);
    }
    let grid = geometry.filter_grid();
    #[allow(clippy::float_cmp)]
    let single = grid == [1.0, 1.0];
    let count = grid[0] * grid[1];
    let tap = |axis, destination| {
        let source_axis = geometry.source_axis(axis);
        let size = if source_axis == 0 {
            source.size.width
        } else {
            source.size.height
        };
        let taps = RgbaSource::axis_tap(geometry.source_coordinate(axis, destination), size);
        AxisTap {
            inside: geometry.inside_axis(axis, destination),
            luma: taps,
            chroma: taps,
        }
    };
    let columns = AxisTable::new(
        geometry,
        0,
        (bounds.x, bounds.width),
        &Geometry::grid_offsets(grid, 0),
        tap,
    );
    let rows = AxisTable::new(
        geometry,
        1,
        (bounds.y, bounds.height),
        &Geometry::grid_offsets(grid, 1),
        tap,
    );
    let turned = geometry.source_axis(1) == 0;
    let [(first, last), _] = columns.reach();
    let width = bounds.width as usize;
    let scratch = || {
        let length = (last - first + 1) as usize;
        (
            RgbaLines {
                near: vec![[0.0; 4]; length],
                far: vec![[0.0; 4]; length],
                first,
            },
            vec![[0.0_f32; 4]; width],
        )
    };
    let mut pixels = renderer.frame_pool.take(bounds.pixels());
    renderer.workers.bands_with(
        &mut pixels,
        width,
        scratch,
        |(lines, sums), first_row, band| {
            band.fill([0; 4]);
            for (index, line) in band.chunks_mut(width).enumerate() {
                let row = first_row + index;
                if !rows.inside[row] {
                    continue;
                }
                sums.fill([0.0; 4]);
                for row_tap in rows.at(row) {
                    let fixed = row_tap.luma;
                    for (offset, (near, far)) in
                        lines.near.iter_mut().zip(lines.far.iter_mut()).enumerate()
                    {
                        #[allow(clippy::cast_possible_truncation)]
                        let along = i64::from(lines.first + offset as u32);
                        let (a, b) = (i64::from(fixed.0), i64::from(fixed.1));
                        (*near, *far) = if turned {
                            (source.texel(a, along), source.texel(b, along))
                        } else {
                            (source.texel(along, a), source.texel(along, b))
                        };
                    }
                    for (offset, (out, sum)) in line.iter_mut().zip(sums.iter_mut()).enumerate() {
                        if !columns.inside[offset] {
                            continue;
                        }
                        for column_tap in columns.at(offset) {
                            let across = column_tap.luma;
                            let (low, high) = (
                                (across.0 - lines.first) as usize,
                                (across.1 - lines.first) as usize,
                            );
                            // Corners left to right, then top to bottom, and the x and y weights.
                            let (corners, weight) = if turned {
                                (
                                    [
                                        lines.near[low],
                                        lines.far[low],
                                        lines.near[high],
                                        lines.far[high],
                                    ],
                                    [fixed.2, across.2],
                                )
                            } else {
                                (
                                    [
                                        lines.near[low],
                                        lines.near[high],
                                        lines.far[low],
                                        lines.far[high],
                                    ],
                                    [across.2, fixed.2],
                                )
                            };
                            let value = source.stored(source.filtered_corners(&corners, weight));
                            if single {
                                *out = half::store4(value);
                            } else {
                                for (total, channel) in sum.iter_mut().zip(value) {
                                    *total += channel;
                                }
                            }
                        }
                    }
                }
                if !single {
                    for (offset, (out, sum)) in line.iter_mut().zip(sums.iter()).enumerate() {
                        if columns.inside[offset] {
                            *out = half::store4(sum.map(|total| total / count));
                        }
                    }
                }
            }
        },
    );
    CpuImage::pooled(output, bounds, pixels, &renderer.frame_pool)
}

impl CpuRenderer {
    /// A source showing decoded YCbCr `planes` placed by `presentation`, converted on first draw;
    /// the CPU counterpart of [`crate::Renderer::prepare_yuv_presented`].
    ///
    /// # Errors
    /// [`RenderError::InvalidImageData`] for an unsupported format or short planes, and
    /// [`RenderError::UnsupportedHdr`] when [`ColorPolicy::RejectHdr`] meets an HDR source.
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

    /// A source showing RGBA8 `pixels` placed by `presentation`; the CPU counterpart of
    /// [`crate::Renderer::prepare_rgba_presented`]. A zero `stride` means packed rows.
    ///
    /// # Errors
    /// [`RenderError::InvalidImageData`] for short pixels or an invalid presentation.
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

    /// A source showing premultiplied linear BT.709 RGBA half-float bits (`1.0` = SDR white); the
    /// CPU counterpart of [`crate::Renderer::prepare_linear_presented`]. A zero `stride` means
    /// packed rows.
    ///
    /// # Errors
    /// [`RenderError::InvalidImageData`] for short pixels or an invalid presentation.
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

#[cfg(test)]
mod tests {
    use alpha2_media::{
        ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange, DynamicRange, TransferFunction,
        YuvFormat,
    };

    use super::{Exact, Planes, Tabled, Transfer, YuvConversion, half};
    use crate::hdr::HdrToneMap;
    use crate::yuv::{PlaneStorage, uniform_bytes};
    use crate::{RenderSize, SourcePlane, SourcePresentation, YuvFrame};

    fn conversion(
        primaries: ColorPrimaries,
        transfer: TransferFunction,
        peak: u32,
    ) -> YuvConversion {
        let hdr = matches!(transfer, TransferFunction::Pq | TransferFunction::Hlg);
        let color = ColorMetadata {
            primaries,
            transfer,
            matrix: ColorMatrix::Bt2020Ncl,
            range: ColorRange::Limited,
            dynamic_range: if hdr {
                DynamicRange::Hdr
            } else {
                DynamicRange::Sdr
            },
        };
        let size = RenderSize::new(16, 16).expect("size");
        let format = YuvFormat::P010;
        let bytes = uniform_bytes(
            color,
            hdr.then_some(HdrToneMap {
                source_peak_nits: peak,
            }),
            1.0,
            size,
            SourcePresentation::identity(size),
            format,
            PlaneStorage::Unorm16.code_scale(format),
            64.0,
        );
        YuvConversion::from_uniform(&bytes, format)
    }

    /// The stored value of encoded R'G'B' `encoded` with the curves of `T`.
    fn stored<T: Transfer>(conversion: &YuvConversion, encoded: [f32; 3]) -> [f32; 4] {
        let rgb = conversion.linear_from_encoded_with::<T>(encoded);
        YuvConversion::stored_linear([rgb[0], rgb[1], rgb[2], 1.0])
    }

    /// One half-float step at `value`'s magnitude.
    fn half_step(value: f32) -> f32 {
        let exponent = (half::store(value.abs()) >> 10) & 0x1f;
        libm::exp2f(f32::from(exponent.max(1)) - 25.0)
    }

    #[test]
    fn the_presentation_tables_stay_far_below_a_half_float_step() {
        let cases = [
            (ColorPrimaries::Bt2020, TransferFunction::Pq, 1_000),
            (ColorPrimaries::Bt2020, TransferFunction::Pq, 4_000),
            (ColorPrimaries::Bt2020, TransferFunction::Hlg, 1_000),
            (ColorPrimaries::Bt2020, TransferFunction::Hlg, 400),
            (ColorPrimaries::Bt2020, TransferFunction::Bt709, 0),
            (ColorPrimaries::Bt601, TransferFunction::Bt709, 0),
        ];
        let mut state = 0x9e37_79b9_u32;
        for (primaries, transfer, peak) in cases {
            let conversion = conversion(primaries, transfer, peak);
            let (mut worst, mut worst_raw) = (0.0_f32, 0.0_f32);
            let (mut changed, mut values) = (0_u32, 0_u32);
            // Every 10-bit grey, then random 10-bit colors.
            let codes = (0..1024_u32)
                .map(|code| [code; 3])
                .chain((0..200_000).map(|_| {
                    [0; 3].map(|_| {
                        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        state >> 22
                    })
                }));
            for code in codes {
                #[allow(clippy::cast_precision_loss)]
                let encoded = code.map(|code| code as f32 / 1023.0);
                let exact = stored::<Exact>(&conversion, encoded);
                let tabled = stored::<Tabled>(&conversion, encoded);
                let brightest = exact[0].max(exact[1]).max(exact[2]);
                for (exact, tabled) in exact.iter().zip(tabled) {
                    let error = (tabled - exact).abs();
                    worst = worst.max(error / half_step(exact.max(brightest)));
                    worst_raw = worst_raw.max(error / half_step(*exact));
                    changed += u32::from(half::store(tabled) != half::store(*exact));
                    values += 1;
                }
            }
            println!(
                "{transfer:?} {primaries:?} peak {peak}: worst {worst:.4} half steps of the \
                 brightest channel, {worst_raw:.4} of the channel, {changed} of {values} \
                 stored values differ"
            );
            assert!(worst < 0.05, "{transfer:?} {primaries:?} {peak}: {worst}");
        }
    }

    /// `premultiplied_linear` without its early return for transparent neighbourhoods.
    fn premultiplied_linear_reference(
        conversion: &YuvConversion,
        planes: &Planes<'_>,
        source_pixel: [f32; 2],
    ) -> [f32; 4] {
        let (luma, chroma) = conversion.coordinates(source_pixel);
        let geometry = &conversion.geometry;
        let texel = [
            luma[0] * geometry.source[0] - 0.5,
            luma[1] * geometry.source[1] - 0.5,
        ];
        let origin = [texel[0].floor(), texel[1].floor()];
        let weight = [texel[0] - origin[0], texel[1] - origin[1]];
        let first = [geometry.visible[0], geometry.visible[1]];
        let last = [
            first[0] + geometry.visible[2] - 1.0,
            first[1] + geometry.visible[3] - 1.0,
        ];
        let corner = |dx: f32, dy: f32| {
            conversion.coordinates([
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
        let alphas = corners.map(|(luma, _)| conversion.alpha_at(planes, luma));
        if alphas.iter().all(|alpha| *alpha >= 1.0) {
            let rgb = conversion.linear_rgb(planes, luma, chroma);
            return [rgb[0], rgb[1], rgb[2], 1.0];
        }
        let texels: Vec<[f32; 4]> = corners
            .iter()
            .zip(alphas)
            .map(|((luma, chroma), alpha)| {
                let rgb = conversion.linear_rgb(planes, *luma, *chroma);
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

    #[test]
    fn alpha_plane_samples_keep_their_bits_without_the_clear_ones() {
        let (width, height) = (48_u32, 32_u32);
        let format = YuvFormat::I420.with_alpha();
        #[allow(clippy::cast_possible_truncation)]
        let ramp = |count: u32, step: u32| (0..count).map(|index| (index * step) as u8).collect();
        let luma: Vec<u8> = ramp(width * height, 7);
        let u: Vec<u8> = ramp(width * height / 4, 3);
        let v: Vec<u8> = ramp(width * height / 4, 5);
        // Clear, then a soft edge, then opaque, across the frame.
        #[allow(clippy::cast_possible_truncation)]
        let alpha = (0..width * height)
            .map(|index| ((index % width).saturating_sub(16) * 16).min(255) as u8)
            .collect::<Vec<u8>>();
        let plane = |bytes, stride| SourcePlane { bytes, stride };
        let size = RenderSize::new(width, height).expect("size");
        let frame = YuvFrame {
            format,
            planes: [
                plane(&luma, width),
                plane(&u, width / 2),
                plane(&v, width / 2),
            ],
            alpha_plane: Some(plane(&alpha, width)),
            size,
            alpha: 1.0,
            color: Some(ColorMetadata::EXPORT),
        };
        let bytes = uniform_bytes(
            ColorMetadata::EXPORT,
            None,
            1.0,
            size,
            SourcePresentation::identity(size),
            format,
            PlaneStorage::Unorm8.code_scale(format),
            64.0,
        );
        let conversion = YuvConversion::from_uniform(&bytes, format);
        let planes = Planes::of(&frame, format);
        let (mut clear, mut soft) = (0, 0);
        #[allow(clippy::cast_precision_loss)]
        for step in 0..(width * 3) * (height * 3) {
            let source_pixel = [
                (step % (width * 3)) as f32 / 3.0 + 0.17,
                (step / (width * 3)) as f32 / 3.0 + 0.11,
            ];
            let new = conversion.premultiplied_linear(&planes, source_pixel);
            let old = premultiplied_linear_reference(&conversion, &planes, source_pixel);
            assert_eq!(
                new.map(f32::to_bits),
                old.map(f32::to_bits),
                "{source_pixel:?}"
            );
            clear += usize::from(new[3] == 0.0);
            soft += usize::from(new[3] > 0.0 && new[3] < 1.0);
        }
        assert!(clear > 0 && soft > 0, "{clear} clear, {soft} soft samples");
    }
}
