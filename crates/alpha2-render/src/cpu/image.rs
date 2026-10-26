//! Pictures the CPU renderer keeps: what a graph node draws, and the
//! pictures sources lend it.

use super::half;
use crate::{RenderError, RenderSize};

/// A rectangle of pixels: `x..x + width` by `y..y + height`.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub(crate) struct PixelBox {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl PixelBox {
    pub(crate) const EMPTY: Self = Self {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    };

    pub(crate) const fn whole(size: RenderSize) -> Self {
        Self {
            x: 0,
            y: 0,
            width: size.width,
            height: size.height,
        }
    }

    pub(crate) const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub(crate) const fn right(self) -> u32 {
        self.x + self.width
    }

    pub(crate) const fn bottom(self) -> u32 {
        self.y + self.height
    }

    pub(crate) fn pixels(self) -> usize {
        self.width as usize * self.height as usize
    }

    pub(crate) const fn contains(self, x: u32, y: u32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    /// The pixels both hold; empty when they do not meet.
    pub(crate) fn intersect(self, other: Self) -> Self {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= x || bottom <= y {
            return Self::EMPTY;
        }
        Self {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }
    }

    /// The smallest box holding both; an empty box adds nothing.
    pub(crate) fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Self {
            x,
            y,
            width: self.right().max(other.right()) - x,
            height: self.bottom().max(other.bottom()) - y,
        }
    }

    /// Grown by `margin` pixels on every side, kept inside `size`.
    pub(crate) fn expand(self, margin: u32, size: RenderSize) -> Self {
        if self.is_empty() {
            return self;
        }
        let x = self.x.saturating_sub(margin);
        let y = self.y.saturating_sub(margin);
        let right = self.right().saturating_add(margin).min(size.width);
        let bottom = self.bottom().saturating_add(margin).min(size.height);
        Self {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }
    }
}

/// How a picture stores its premultiplied, sRGB-encoded RGBA.
#[derive(Clone, Debug)]
pub(crate) enum Pixels {
    /// Half-float bits, as the `Rgba16Float` intermediate holds them.
    Half(Vec<[u16; 4]>),
    /// 8-bit codes, as an `Rgba8Unorm` texture holds them.
    Unorm8(Vec<[u8; 4]>),
}

/// A picture the CPU renderer drew or was lent: premultiplied, sRGB-encoded
/// RGBA, as every GPU intermediate texture holds it.
///
/// Only its content rectangle is stored; every pixel outside it is
/// transparent. A label on a 1080p canvas keeps the label's pixels, not the
/// canvas's.
#[derive(Clone, Debug)]
pub struct CpuImage {
    size: RenderSize,
    bounds: PixelBox,
    pixels: Pixels,
}

impl CpuImage {
    pub(crate) fn new(size: RenderSize, bounds: PixelBox, pixels: Pixels) -> Self {
        let bounds = bounds.intersect(PixelBox::whole(size));
        debug_assert_eq!(
            match &pixels {
                Pixels::Half(pixels) => pixels.len(),
                Pixels::Unorm8(pixels) => pixels.len(),
            },
            bounds.pixels()
        );
        Self {
            size,
            bounds,
            pixels,
        }
    }

    /// A transparent picture of `size`, which stores nothing.
    pub(crate) fn transparent(size: RenderSize) -> Self {
        Self {
            size,
            bounds: PixelBox::EMPTY,
            pixels: Pixels::Half(Vec::new()),
        }
    }

    /// Premultiplied, sRGB-encoded RGBA8 pixels, rows packed, as
    /// [`crate::Renderer::upload_rgba8`] takes them: a picture sampled as
    /// that texture is.
    ///
    /// # Errors
    /// Returns [`RenderError::InvalidImageData`] when `rgba8` does not hold
    /// four bytes per pixel of `size`.
    pub fn from_premultiplied_rgba8(size: RenderSize, rgba8: &[u8]) -> Result<Self, RenderError> {
        let expected = size.width as usize * size.height as usize * 4;
        if rgba8.len() != expected {
            return Err(RenderError::InvalidImageData);
        }
        let pixels = rgba8.as_chunks::<4>().0.to_vec();
        Ok(Self::new(
            size,
            PixelBox::whole(size),
            Pixels::Unorm8(pixels),
        ))
    }

    /// The picture's dimensions.
    #[must_use]
    pub const fn size(&self) -> RenderSize {
        self.size
    }

    pub(crate) const fn bounds(&self) -> PixelBox {
        self.bounds
    }

    pub(crate) const fn pixels(&self) -> &Pixels {
        &self.pixels
    }

    /// Bytes the picture holds.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        let pixels = self.bounds.pixels() as u64;
        match self.pixels {
            Pixels::Half(_) => pixels * 8,
            Pixels::Unorm8(_) => pixels * 4,
        }
    }

    /// The picture as tightly packed RGBA8, as [`crate::Renderer::read_rgba8`]
    /// reads a texture back: each component rounded to the nearest code.
    #[must_use]
    pub fn read_rgba8(&self) -> Vec<u8> {
        let width = self.size.width as usize;
        let mut bytes = vec![0_u8; width * self.size.height as usize * 4];
        let bounds = self.bounds;
        for row in 0..bounds.height as usize {
            let start = ((bounds.y as usize + row) * width + bounds.x as usize) * 4;
            let out = &mut bytes[start..start + bounds.width as usize * 4];
            match &self.pixels {
                Pixels::Half(pixels) => {
                    let line = &pixels[row * bounds.width as usize..][..bounds.width as usize];
                    for (pixel, out) in line.iter().zip(out.as_chunks_mut::<4>().0) {
                        *out = half::load4(*pixel).map(half::unorm8);
                    }
                }
                Pixels::Unorm8(pixels) => {
                    let line = &pixels[row * bounds.width as usize..][..bounds.width as usize];
                    out.copy_from_slice(line.as_flattened());
                }
            }
        }
        bytes
    }

    /// The pixel at `(x, y)`, which must lie inside the picture: transparent
    /// outside the content rectangle.
    #[inline]
    pub(crate) fn texel(&self, x: u32, y: u32) -> [f32; 4] {
        let bounds = self.bounds;
        if !bounds.contains(x, y) {
            return [0.0; 4];
        }
        let index = (y - bounds.y) as usize * bounds.width as usize + (x - bounds.x) as usize;
        match &self.pixels {
            Pixels::Half(pixels) => half::load4(pixels[index]),
            Pixels::Unorm8(pixels) => pixels[index].map(half::unorm8_value),
        }
    }

    /// Pixels `x0..x0 + out.len()` of row `y`, which must lie inside the
    /// picture, into `out`: transparent outside the content rectangle.
    pub(crate) fn decode_row(&self, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        let bounds = self.bounds;
        if y < bounds.y || y >= bounds.bottom() {
            out.fill([0.0; 4]);
            return;
        }
        let row = (y - bounds.y) as usize * bounds.width as usize;
        // The part of the request inside the content, decoded; zeros around it.
        let start = x0.max(bounds.x);
        #[allow(clippy::cast_possible_truncation)]
        let end = (x0 + out.len() as u32).min(bounds.right());
        out.fill([0.0; 4]);
        if start >= end {
            return;
        }
        let target = &mut out[(start - x0) as usize..(end - x0) as usize];
        let source = row + (start - bounds.x) as usize..row + (end - bounds.x) as usize;
        match &self.pixels {
            Pixels::Half(pixels) => {
                let table = half::table();
                for (pixel, bits) in target.iter_mut().zip(&pixels[source]) {
                    *pixel = bits.map(|bits| table[usize::from(bits)]);
                }
            }
            Pixels::Unorm8(pixels) => {
                for (pixel, codes) in target.iter_mut().zip(&pixels[source]) {
                    *pixel = codes.map(half::unorm8_value);
                }
            }
        }
    }

    /// The pixel at `(x, y)` with the edge repeated past it, as a sampler
    /// with clamp-to-edge addressing reads it.
    #[inline]
    pub(crate) fn texel_clamped(&self, x: i64, y: i64) -> [f32; 4] {
        let last_x = i64::from(self.size.width) - 1;
        let last_y = i64::from(self.size.height) - 1;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        self.texel(x.clamp(0, last_x) as u32, y.clamp(0, last_y) as u32)
    }

    /// A bilinear sample at normalized position `(u, v)`, the edge repeated:
    /// what `textureSampleLevel` with a linear, clamp-to-edge sampler reads
    /// at level 0.
    #[inline]
    pub(crate) fn sample(&self, u: f32, v: f32) -> [f32; 4] {
        #[allow(clippy::cast_precision_loss)]
        let x = u * self.size.width as f32 - 0.5;
        #[allow(clippy::cast_precision_loss)]
        let y = v * self.size.height as f32 - 0.5;
        let x0 = x.floor();
        let y0 = y.floor();
        let fx = x - x0;
        let fy = y - y0;
        #[allow(clippy::cast_possible_truncation)]
        let (ix, iy) = (x0 as i64, y0 as i64);
        let t00 = self.texel_clamped(ix, iy);
        let t10 = self.texel_clamped(ix + 1, iy);
        let t01 = self.texel_clamped(ix, iy + 1);
        let t11 = self.texel_clamped(ix + 1, iy + 1);
        let mut out = [0.0; 4];
        for channel in 0..4 {
            let top = t00[channel] + (t10[channel] - t00[channel]) * fx;
            let bottom = t01[channel] + (t11[channel] - t01[channel]) * fx;
            out[channel] = top + (bottom - top) * fy;
        }
        out
    }
}
