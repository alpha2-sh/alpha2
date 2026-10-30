//! A frame's picture packed into an output [`PixelLayout`] other than 8-bit
//! 4:2:0: 10-bit codes, 4:2:2 and 4:4:4 chroma, and an alpha plane.
//!
//! The GPU renderer reads its picture back as stored and the CPU renderer
//! hands its own over; both pack here, so the two write the same codes.
//! 8-bit opaque 4:2:0 is not packed here: it keeps the pack it always had
//! (rgb-to-yuv420.wesl and `cpu::pack`).
//!
//! The picture holds premultiplied, sRGB-encoded color. An opaque layout
//! keeps premultiplied color, as the 8-bit pack does. A layout with alpha
//! writes straight color, `rgb / a` (black where `a` is 0), and weighs each
//! subsampled chroma sample's taps by their alpha, so the undefined color of
//! transparent pixels never reaches an edge. Codes are rounded to nearest,
//! ties to even, in IEEE arithmetic, so every platform writes the same ones.

use alpha2_media::{BitDepth, ChromaLocation, ColorMatrix, PixelLayout, PlaneLayout, YuvEncoding};

use crate::{PlaneTarget, ReadbackFailure, RenderError};

/// The picture rows a pack reads: `row(y, out)` writes row `y`'s
/// premultiplied RGBA, `width` texels.
pub(crate) struct Picture<'a> {
    pub width: usize,
    pub height: usize,
    pub row: &'a (dyn Fn(usize, &mut [[f32; 4]]) + Sync),
}

/// How color becomes codes at one bit depth.
#[derive(Copy, Clone)]
struct Coding {
    kr: f32,
    kg: f32,
    kb: f32,
    luma_black: f32,
    luma_span: f32,
    chroma_center: f32,
    chroma_span: f32,
    maximum: f32,
}

impl Coding {
    fn new(encoding: YuvEncoding, bit_depth: BitDepth) -> Self {
        // Components that are already RGB have no pack: they take BT.709's, as the 8-bit pack.
        let (kr, kb) = encoding
            .color
            .matrix
            .luma_weights()
            .or(ColorMatrix::Bt709.luma_weights())
            .expect("BT.709 is a YCbCr matrix");
        let levels = encoding.color.range.levels(bit_depth.bits());
        Self {
            kr,
            kg: 1.0 - kr - kb,
            kb,
            luma_black: levels.luma_black,
            luma_span: levels.luma_span,
            chroma_center: levels.chroma_center,
            chroma_span: levels.chroma_span,
            maximum: levels.maximum,
        }
    }

    fn luma(self, rgb: [f32; 3]) -> f32 {
        rgb[0] * self.kr + rgb[1] * self.kg + rgb[2] * self.kb
    }

    fn y(self, rgb: [f32; 3]) -> u16 {
        self.code(self.luma_black + self.luma_span * self.luma(rgb))
    }

    fn uv(self, rgb: [f32; 3]) -> [u16; 2] {
        let luma = self.luma(rgb);
        [
            self.code(
                self.chroma_center + self.chroma_span * (rgb[2] - luma) / (2.0 * (1.0 - self.kb)),
            ),
            self.code(
                self.chroma_center + self.chroma_span * (rgb[0] - luma) / (2.0 * (1.0 - self.kr)),
            ),
        ]
    }

    fn alpha(self, alpha: f32) -> u16 {
        self.code(alpha.clamp(0.0, 1.0) * self.maximum)
    }

    fn neutral(self) -> [u16; 2] {
        let center = self.code(self.chroma_center);
        [center, center]
    }

    /// `value` rounded to the nearest code, ties to even, within the range.
    fn code(self, value: f32) -> u16 {
        // Adding and taking away 2^23 rounds a value below 2^23 to a whole number in IEEE
        // arithmetic, as `cpu::half::unorm8` does.
        const ROUND: f32 = 8_388_608.0;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let code = ((value.clamp(0.0, self.maximum) + ROUND) - ROUND) as u16;
        code
    }
}

/// How much the luma sample `offset` from a chroma sample's even column (or
/// row) weighs in it, as the 8-bit pack's `tap_weight`.
fn tap_weight(offset: i64, cosited: bool) -> f32 {
    if cosited {
        if offset == 0 { 0.5 } else { 0.25 }
    } else if offset >= 0 {
        0.5
    } else {
        0.0
    }
}

/// The color a pixel packs: premultiplied, or straight where the layout
/// keeps alpha, clamped to the encoded range.
fn color(texel: [f32; 4], alpha: bool) -> [f32; 3] {
    let rgb = [texel[0], texel[1], texel[2]];
    if !alpha {
        return rgb.map(|value| value.clamp(0.0, 1.0));
    }
    let a = texel[3].clamp(0.0, 1.0);
    if a <= 0.0 {
        return [0.0; 3];
    }
    rgb.map(|value| (value / a).clamp(0.0, 1.0))
}

/// Writes `code` as sample `index` of a row of the plane: a byte at 8 bits,
/// a little-endian word at 10, shifted to the word's top for `P010`.
fn put(row: &mut [u8], index: usize, code: u16, layout: PixelLayout) {
    match (layout.bit_depth, layout.planes) {
        (BitDepth::Eight, _) => {
            #[allow(clippy::cast_possible_truncation)]
            {
                row[index] = code as u8;
            }
        }
        (BitDepth::Ten, PlaneLayout::Planar) => {
            row[index * 2..index * 2 + 2].copy_from_slice(&code.to_le_bytes());
        }
        (BitDepth::Ten, PlaneLayout::P010) => {
            row[index * 2..index * 2 + 2].copy_from_slice(&(code << 6).to_le_bytes());
        }
    }
}

/// The rows of each plane one band of chroma rows writes.
struct Band<'a> {
    /// The first chroma row.
    first: usize,
    /// Per plane, its rows in this band.
    planes: Vec<Vec<&'a mut [u8]>>,
}

/// Packs `picture` into `targets`, one per plane of `layout` in its storage
/// order ([`PixelLayout::planes`]).
///
/// # Errors
/// Returns [`RenderError::TextureReadback`] for a picture its chroma cannot
/// subsample (an odd size for 4:2:0, an odd width for 4:2:2), the wrong
/// number of targets, or a target too small for its plane.
pub(crate) fn pack(
    picture: &Picture<'_>,
    encoding: YuvEncoding,
    layout: PixelLayout,
    targets: &mut [PlaneTarget<'_>],
) -> Result<(), RenderError> {
    let refuse = |failure| Err(RenderError::TextureReadback(failure));
    let (width, height) = (picture.width, picture.height);
    let (step_x, step_y) = layout.chroma.subsampling();
    let (step_x, step_y) = (step_x as usize, step_y as usize);
    if !width.is_multiple_of(step_x) || !height.is_multiple_of(step_y) || width == 0 || height == 0
    {
        return refuse(ReadbackFailure::UnsupportedTarget);
    }
    let planes = layout.planes(
        u32::try_from(width).unwrap_or(u32::MAX),
        u32::try_from(height).unwrap_or(u32::MAX),
    );
    if planes.len() != targets.len() {
        return refuse(ReadbackFailure::Layout);
    }
    for (target, &(plane_width, rows, bytes)) in targets.iter().zip(&planes) {
        target.fits((plane_width * bytes) as usize, rows as usize)?;
    }
    let chroma_rows = height / step_y;
    // Each band of chroma rows with the luma and alpha rows they cover.
    let bands = available_bands(chroma_rows);
    let per_band = chroma_rows.div_ceil(bands);
    let mut split: Vec<Band<'_>> = (0..bands)
        .map(|band| Band {
            first: band * per_band,
            planes: Vec::new(),
        })
        .collect();
    for (target, &(_, rows, _)) in targets.iter_mut().zip(&planes) {
        let mut rows_left = target.bytes.chunks_mut(target.stride).take(rows as usize);
        // Rows of this plane per chroma row: the luma and alpha planes have `step_y`.
        let ratio = rows as usize / chroma_rows;
        for band in &mut split {
            let count = per_band.min(chroma_rows.saturating_sub(band.first)) * ratio;
            band.planes.push(rows_left.by_ref().take(count).collect());
        }
    }
    let coding = Coding::new(encoding, layout.bit_depth);
    let run = |band: Band<'_>| pack_band(picture, coding, encoding.chroma, layout, band);
    #[cfg(not(target_arch = "wasm32"))]
    if split.len() > 1 {
        std::thread::scope(|scope| {
            for band in split {
                scope.spawn(move || run(band));
            }
        });
        return Ok(());
    }
    for band in split {
        run(band);
    }
    Ok(())
}

/// How many bands to split `rows` chroma rows into: one per core, up to 8,
/// with at least 16 rows each.
fn available_bands(rows: usize) -> usize {
    #[cfg(not(target_arch = "wasm32"))]
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    #[cfg(target_arch = "wasm32")]
    let cores = 1;
    cores.min(8).min(rows / 16).max(1)
}

#[allow(clippy::too_many_lines)]
fn pack_band(
    picture: &Picture<'_>,
    coding: Coding,
    siting: ChromaLocation,
    layout: PixelLayout,
    band: Band<'_>,
) {
    let width = picture.width;
    let height = picture.height;
    let (step_x, step_y) = layout.chroma.subsampling();
    let (step_x, step_y) = (step_x as usize, step_y as usize);
    let (cosited_x, cosited_y) = match siting {
        ChromaLocation::Left => (true, false),
        ChromaLocation::TopLeft => (true, true),
        ChromaLocation::Unspecified | ChromaLocation::Center => (false, false),
    };
    let chroma_width = width / step_x;
    let Band { first, planes } = band;
    let mut planes = planes.into_iter();
    let mut luma = planes.next().unwrap_or_default();
    let mut chroma_planes: Vec<Vec<&mut [u8]>> = match layout.planes {
        PlaneLayout::Planar => vec![
            planes.next().unwrap_or_default(),
            planes.next().unwrap_or_default(),
        ],
        PlaneLayout::P010 => vec![planes.next().unwrap_or_default()],
    };
    let mut alpha_rows = if layout.alpha {
        planes.next().unwrap_or_default()
    } else {
        Vec::new()
    };
    let chroma_rows = chroma_planes[0].len();
    // Picture rows around one chroma row: one above, the `step_y` it covers, one below.
    let mut lines = vec![vec![[0.0_f32; 4]; width]; step_y + 2];
    for local in 0..chroma_rows {
        let row = first + local;
        let top = row * step_y;
        for (offset, line) in lines.iter_mut().enumerate() {
            let y = (top + offset).saturating_sub(1).min(height - 1);
            (picture.row)(y, line);
        }
        // Luma and alpha for the rows this chroma row covers.
        for inner in 0..step_y {
            let line = &lines[inner + 1];
            let out = &mut luma[local * step_y + inner];
            for (x, texel) in line.iter().enumerate() {
                put(out, x, coding.y(color(*texel, layout.alpha)), layout);
            }
            if layout.alpha {
                let out = &mut alpha_rows[local * step_y + inner];
                for (x, texel) in line.iter().enumerate() {
                    put(out, x, coding.alpha(texel[3]), layout);
                }
            }
        }
        for column in 0..chroma_width {
            let uv = chroma_sample(
                &lines,
                column,
                step_x,
                step_y,
                cosited_x,
                cosited_y,
                coding,
                layout.alpha,
            );
            match layout.planes {
                PlaneLayout::Planar => {
                    let (u, v) = chroma_planes.split_at_mut(1);
                    put(u[0][local], column, uv[0], layout);
                    put(v[0][local], column, uv[1], layout);
                }
                PlaneLayout::P010 => {
                    put(chroma_planes[0][local], column * 2, uv[0], layout);
                    put(chroma_planes[0][local], column * 2 + 1, uv[1], layout);
                }
            }
        }
    }
}

/// One chroma sample's Cb and Cr from the rows around it: the pixel itself
/// at 4:4:4; otherwise its taps, sited as the 8-bit pack sites them, each
/// weighed by its alpha where the layout keeps alpha.
#[allow(clippy::too_many_arguments)]
fn chroma_sample(
    lines: &[Vec<[f32; 4]>],
    column: usize,
    step_x: usize,
    step_y: usize,
    cosited_x: bool,
    cosited_y: bool,
    coding: Coding,
    alpha: bool,
) -> [u16; 2] {
    let width = lines[0].len();
    let x = column * step_x;
    if step_x == 1 && step_y == 1 {
        return coding.uv(color(lines[1][x], alpha));
    }
    let taps = |step: usize, cosited: bool| -> Vec<(i64, f32)> {
        if step == 1 {
            return vec![(0, 1.0)];
        }
        (-1..=1_i64)
            .map(|offset| (offset, tap_weight(offset, cosited)))
            .filter(|&(_, weight)| weight != 0.0)
            .collect()
    };
    let mut sum = [0.0_f32; 3];
    let mut weight_sum = 0.0_f32;
    for (dy, row_weight) in taps(step_y, cosited_y) {
        // The line of the tap: line 1 is the first row the chroma row covers.
        let line = &lines[usize::try_from(1 + dy).unwrap_or(0)];
        for (dx, column_weight) in taps(step_x, cosited_x) {
            let at = usize::try_from(i64::try_from(x).unwrap_or(0) + dx)
                .unwrap_or(0)
                .min(width - 1);
            let texel = line[at];
            let weight = row_weight * column_weight;
            if alpha {
                // Premultiplied color sums the alpha-weighted straight color.
                let a = texel[3].clamp(0.0, 1.0) * weight;
                for (channel, value) in sum.iter_mut().enumerate() {
                    *value += texel[channel].clamp(0.0, 1.0) * weight;
                }
                weight_sum += a;
            } else {
                let rgb = color(texel, false);
                for (channel, value) in sum.iter_mut().enumerate() {
                    *value += rgb[channel] * weight;
                }
            }
        }
    }
    if !alpha {
        return coding.uv(sum);
    }
    if weight_sum <= 0.0 {
        return coding.neutral();
    }
    coding.uv(sum.map(|value| (value / weight_sum).clamp(0.0, 1.0)))
}

#[cfg(test)]
#[path = "layout_pack_tests.rs"]
mod tests;
