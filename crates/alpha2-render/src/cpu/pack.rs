//! The frame's picture packed into an encoder's planar YUV420 frame, as
//! rgb-to-yuv420.wesl packs it on the GPU: written straight into the
//! caller's planes, with no staging copy.

use alpha2_media::YuvEncoding;

use super::half;
use super::image::CpuImage;
use super::workers::Workers;
use crate::{ReadbackFailure, RenderError, Yuv420pTargets};

/// The pack's coefficients, as the shader's `YuvParams`.
#[derive(Copy, Clone)]
struct Coding {
    kr: f32,
    kg: f32,
    kb: f32,
    luma_black: f32,
    luma_span: f32,
    chroma_center: f32,
    chroma_span: f32,
    cosited: [bool; 2],
}

impl Coding {
    fn new(encoding: YuvEncoding) -> Self {
        let values = crate::readback::yuv_uniform(encoding);
        Self {
            kr: values[0],
            kg: values[1],
            kb: values[2],
            luma_black: values[4],
            luma_span: values[5],
            chroma_center: values[6],
            chroma_span: values[7],
            cosited: [values[8] != 0.0, values[9] != 0.0],
        }
    }

    #[inline]
    fn luma(self, rgb: [f32; 3]) -> f32 {
        rgb[0] * self.kr + rgb[1] * self.kg + rgb[2] * self.kb
    }

    #[inline]
    fn yuv(self, rgb: [f32; 3]) -> [f32; 3] {
        let luma = self.luma(rgb);
        [
            self.luma_black + self.luma_span * luma,
            self.chroma_center + self.chroma_span * (rgb[2] - luma) / (2.0 * (1.0 - self.kb)),
            self.chroma_center + self.chroma_span * (rgb[0] - luma) / (2.0 * (1.0 - self.kr)),
        ]
    }
}

/// How much the luma sample `offset` from a chroma sample's even column (or
/// row) weighs in it: co-sited, 1:2:1 around it; centred, the pair it sits
/// between.
fn tap_weight(offset: i64, cosited: bool) -> f32 {
    if cosited {
        if offset == 0 { 0.5 } else { 0.25 }
    } else if offset >= 0 {
        0.5
    } else {
        0.0
    }
}

/// The luma samples a chroma sample sums, in the order it sums them: the
/// line of `lines` (rows 2j - 1 to 2j + 1), the column offset from its even
/// column plus one, and the weight.
fn chroma_taps(coding: Coding) -> Vec<(usize, usize, f32)> {
    let mut taps = Vec::with_capacity(9);
    for dy in -1..=1_i64 {
        let row_weight = tap_weight(dy, coding.cosited[1]);
        if row_weight == 0.0 {
            continue;
        }
        for dx in -1..=1_i64 {
            let column_weight = tap_weight(dx, coding.cosited[0]);
            if column_weight == 0.0 {
                continue;
            }
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            taps.push((
                (dy + 1) as usize,
                (dx + 1) as usize,
                row_weight * column_weight,
            ));
        }
    }
    taps
}

/// The chroma samples of one row from the picture rows around it, as the
/// shader filters them; a column left of the picture repeats its edge.
fn chroma_row(
    lines: &[Vec<[f32; 3]>; 3],
    coding: Coding,
    taps: &[(usize, usize, f32)],
    u: &mut [u8],
    v: &mut [u8],
) {
    for (column, (u, v)) in u.iter_mut().zip(v.iter_mut()).enumerate() {
        let x = column * 2;
        let mut rgb = [0.0_f32; 3];
        if !coding.cosited[0] && !coding.cosited[1] {
            // The 2x2 box under the sample, summed and then scaled.
            for sample in [lines[1][x], lines[1][x + 1], lines[2][x], lines[2][x + 1]] {
                for channel in 0..3 {
                    rgb[channel] += sample[channel];
                }
            }
            rgb = rgb.map(|value| value * 0.25);
        } else {
            for &(line, offset, weight) in taps {
                // The picture's width is even, so only the left edge repeats.
                let sample = lines[line][(x + offset).saturating_sub(1)];
                for channel in 0..3 {
                    rgb[channel] += sample[channel] * weight;
                }
            }
        }
        let yuv = coding.yuv(rgb);
        *u = half::unorm8(yuv[1]);
        *v = half::unorm8(yuv[2]);
    }
}

/// Packs `picture` into `targets` with `encoding`.
///
/// # Errors
/// Returns [`RenderError::TextureReadback`] for an odd-sized picture or a
/// target too small for its plane.
pub(crate) fn pack_yuv420p(
    workers: &Workers,
    picture: &CpuImage,
    encoding: YuvEncoding,
    targets: &mut Yuv420pTargets<'_>,
) -> Result<(), RenderError> {
    let size = picture.size();
    if !size.width.is_multiple_of(2) || !size.height.is_multiple_of(2) {
        return Err(RenderError::TextureReadback(
            ReadbackFailure::UnsupportedTarget,
        ));
    }
    let (width, height) = (size.width as usize, size.height as usize);
    let (chroma_width, chroma_height) = (width / 2, height / 2);
    targets.y.fits(width, height)?;
    targets.u.fits(chroma_width, chroma_height)?;
    targets.v.fits(chroma_width, chroma_height)?;
    let coding = Coding::new(encoding);
    let taps = chroma_taps(coding);
    let (y_stride, u_stride, v_stride) = (targets.y.stride, targets.u.stride, targets.v.stride);
    // Each chroma row with the two luma rows it covers, written by one band.
    let mut lumas = targets.y.bytes.chunks_mut(y_stride);
    let mut rows = Vec::with_capacity(chroma_height);
    for (u, v) in targets
        .u
        .bytes
        .chunks_mut(u_stride)
        .zip(targets.v.bytes.chunks_mut(v_stride))
        .take(chroma_height)
    {
        let top = lumas.next().expect("the luma plane fits");
        let bottom = lumas.next().expect("the luma plane fits");
        rows.push((top, bottom, u, v));
    }
    workers.bands(&mut rows, 1, |first, band| {
        // The picture's rows around this chroma row, clamped at the edges as
        // the shader's loads are.
        let mut lines = [
            vec![[0.0_f32; 3]; width],
            vec![[0.0_f32; 3]; width],
            vec![[0.0_f32; 3]; width],
        ];
        let mut decoded = vec![[0.0_f32; 4]; width];
        for (index, (top, bottom, u, v)) in band.iter_mut().enumerate() {
            let row = first + index;
            // Rows 2j - 1 (for co-sited rows only), 2j and 2j + 1 of the picture, edge repeated.
            let first_line = usize::from(!coding.cosited[1]);
            for (offset, line) in lines.iter_mut().enumerate().skip(first_line) {
                let y = (row * 2 + offset).saturating_sub(1).min(height - 1);
                #[allow(clippy::cast_possible_truncation)]
                picture.decode_row(y as u32, 0, &mut decoded);
                for (rgb, texel) in line.iter_mut().zip(&decoded) {
                    *rgb = [
                        texel[0].clamp(0.0, 1.0),
                        texel[1].clamp(0.0, 1.0),
                        texel[2].clamp(0.0, 1.0),
                    ];
                }
            }
            for (luma_row, line) in [(&mut **top, &lines[1]), (&mut **bottom, &lines[2])] {
                for (out, rgb) in luma_row[..width].iter_mut().zip(line) {
                    *out = half::unorm8(coding.luma_black + coding.luma_span * coding.luma(*rgb));
                }
            }
            chroma_row(
                &lines,
                coding,
                &taps,
                &mut u[..chroma_width],
                &mut v[..chroma_width],
            );
        }
    });
    Ok(())
}
