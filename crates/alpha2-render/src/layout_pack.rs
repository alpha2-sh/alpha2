//! Packs a frame into output [`PixelLayout`]s other than 8-bit 4:2:0: 10-bit codes, 4:2:2 and
//! 4:4:4 chroma, and alpha planes.
//!
//! Both the GPU readback and the CPU renderer pack here, so they write identical codes. 8-bit
//! opaque 4:2:0 uses rgb-to-yuv420.wesl and `cpu::pack` instead.
//!
//! Opaque layouts keep premultiplied color, like the 8-bit pack. Layouts with alpha write
//! straight color `rgb / a` (black at `a = 0`) and weight subsampled chroma taps by alpha so
//! transparent pixels' undefined color never reaches an edge. Codes round to nearest even in
//! IEEE arithmetic, so every platform matches.

use std::cell::Cell;

use alpha2_media::{BitDepth, ChromaLocation, ColorMatrix, PixelLayout, PlaneLayout, YuvEncoding};

use crate::cpu::Workers;
use crate::{PlaneTarget, ReadbackFailure, RenderError};

/// Picture rows a pack reads: `row(y, out)` writes row `y` as premultiplied RGBA.
pub(crate) struct Picture<'a, R> {
    pub width: usize,
    pub height: usize,
    pub row: &'a R,
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
        // RGB matrices have no YUV pack; use BT.709 weights like the 8-bit pack.
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

    /// `value` rounded to the nearest code, ties to even, clamped to range.
    fn code(self, value: f32) -> u16 {
        // Adding and subtracting 2^23 rounds to an integer, ties to even, as `cpu::half::unorm8`
        // does.
        const ROUND: f32 = 8_388_608.0;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let code = ((value.clamp(0.0, self.maximum) + ROUND) - ROUND) as u16;
        code
    }
}

/// Weight of the luma sample `offset` from a chroma sample's even column or row, matching the
/// 8-bit pack's `tap_weight`.
fn tap_weight(offset: i64, cosited: bool) -> f32 {
    if cosited {
        if offset == 0 { 0.5 } else { 0.25 }
    } else if offset >= 0 {
        0.5
    } else {
        0.0
    }
}

/// A pixel's packed color: premultiplied, or straight when the layout keeps alpha.
fn color<const ALPHA: bool>(texel: [f32; 4]) -> [f32; 3] {
    let rgb = [texel[0], texel[1], texel[2]];
    if !ALPHA {
        return rgb.map(|value| value.clamp(0.0, 1.0));
    }
    let a = texel[3].clamp(0.0, 1.0);
    if a <= 0.0 {
        return [0.0; 3];
    }
    rgb.map(|value| (value / a).clamp(0.0, 1.0))
}

trait Store {
    const BYTES: usize;

    fn write(sample: &mut [u8], code: u16);
}

struct Byte;

impl Store for Byte {
    const BYTES: usize = 1;

    fn write(sample: &mut [u8], code: u16) {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "8-bit codes are at most 255"
        )]
        {
            sample[0] = code as u8;
        }
    }
}

struct Word;

impl Store for Word {
    const BYTES: usize = 2;

    fn write(sample: &mut [u8], code: u16) {
        sample.copy_from_slice(&code.to_le_bytes());
    }
}

/// `P010` keeps a 10-bit code in the high bits of a little-endian word.
struct HighWord;

impl Store for HighWord {
    const BYTES: usize = 2;

    fn write(sample: &mut [u8], code: u16) {
        sample.copy_from_slice(&(code << 6).to_le_bytes());
    }
}

/// Plane rows written by one band of chroma rows.
struct Band<'a> {
    first: usize,
    planes: Vec<Vec<&'a mut [u8]>>,
}

/// How a pack splits its chroma rows into bands and runs them.
pub(crate) trait Bands: Sync {
    /// Chroma rows per band, of `chroma_rows`, each covering `row_pixels` picture pixels.
    fn rows_per_band(&self, chroma_rows: usize, row_pixels: usize) -> usize;

    /// Calls `job(index)` once for every index below `count`, in any order, possibly in parallel.
    fn run(&self, count: usize, job: &(dyn Fn(usize) + Sync));
}

struct KeptThreads(Workers);

impl Bands for KeptThreads {
    fn rows_per_band(&self, chroma_rows: usize, row_pixels: usize) -> usize {
        if self.0.threads() > 1 {
            self.0.band_rows(row_pixels)
        } else {
            chroma_rows
        }
    }

    fn run(&self, count: usize, job: &(dyn Fn(usize) + Sync)) {
        self.0.for_each(count, job);
    }
}

fn readback_threads() -> &'static KeptThreads {
    static THREADS: std::sync::OnceLock<KeptThreads> = std::sync::OnceLock::new();
    THREADS.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        KeptThreads(Workers::named(cores, "alpha2-pack"))
    })
}

/// Packs `picture` into one target per plane of `layout`, in storage order
/// ([`PixelLayout::planes`]), in bands on threads kept from one pack to the next, one per core.
///
/// # Errors
/// As [`pack_with`].
pub(crate) fn pack<R: Fn(usize, &mut [[f32; 4]]) + Sync>(
    picture: &Picture<'_, R>,
    encoding: YuvEncoding,
    layout: PixelLayout,
    targets: &mut [PlaneTarget<'_>],
) -> Result<(), RenderError> {
    pack_with(picture, encoding, layout, targets, readback_threads())
}

/// Packs `picture` into one target per plane of `layout`, in storage order
/// ([`PixelLayout::planes`]), splitting and running bands as `bands` says.
///
/// # Errors
/// [`RenderError::TextureReadback`] for a size the chroma cannot subsample (odd for 4:2:0, odd
/// width for 4:2:2), a wrong target count, or a target too small.
pub(crate) fn pack_with<R: Fn(usize, &mut [[f32; 4]]) + Sync>(
    picture: &Picture<'_, R>,
    encoding: YuvEncoding,
    layout: PixelLayout,
    targets: &mut [PlaneTarget<'_>],
    bands: &dyn Bands,
) -> Result<(), RenderError> {
    crate::color::check_packable(encoding)?;
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
    let per_band = bands
        .rows_per_band(chroma_rows, width * step_y)
        .clamp(1, chroma_rows);
    let count = chroma_rows.div_ceil(per_band);
    let mut split: Vec<Band<'_>> = (0..count)
        .map(|band| Band {
            first: band * per_band,
            planes: Vec::new(),
        })
        .collect();
    for (target, &(_, rows, _)) in targets.iter_mut().zip(&planes) {
        let mut rows_left = target.bytes.chunks_mut(target.stride).take(rows as usize);
        // Luma and alpha planes have `step_y` rows per chroma row.
        let ratio = rows as usize / chroma_rows;
        for band in &mut split {
            let count = per_band.min(chroma_rows.saturating_sub(band.first)) * ratio;
            band.planes.push(rows_left.by_ref().take(count).collect());
        }
    }
    let coding = Coding::new(encoding, layout.bit_depth);
    let taps = [
        Taps::new(step_x, siting(encoding.chroma).0),
        Taps::new(step_y, siting(encoding.chroma).1),
    ];
    let split = split
        .into_iter()
        .map(|band| std::sync::Mutex::new(Some(band)))
        .collect::<Vec<_>>();
    bands.run(count, &|index| {
        let band = split[index]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(band) = band {
            pack_layout_band(picture, coding, taps, layout, band);
        }
    });
    Ok(())
}

fn pack_layout_band<R: Fn(usize, &mut [[f32; 4]]) + Sync>(
    picture: &Picture<'_, R>,
    coding: Coding,
    taps: [Taps; 2],
    layout: PixelLayout,
    band: Band<'_>,
) {
    let (step_x, step_y) = layout.chroma.subsampling();
    let steps = (step_x as usize, step_y as usize);
    let pack = match (layout.bit_depth, layout.planes, layout.alpha) {
        (BitDepth::Eight, PlaneLayout::Planar, false) => pack_band::<R, Byte, false, false>,
        (BitDepth::Eight, PlaneLayout::Planar, true) => pack_band::<R, Byte, true, false>,
        (BitDepth::Eight, PlaneLayout::P010, false) => pack_band::<R, Byte, false, true>,
        (BitDepth::Eight, PlaneLayout::P010, true) => pack_band::<R, Byte, true, true>,
        (BitDepth::Ten, PlaneLayout::Planar, false) => pack_band::<R, Word, false, false>,
        (BitDepth::Ten, PlaneLayout::Planar, true) => pack_band::<R, Word, true, false>,
        (BitDepth::Ten, PlaneLayout::P010, false) => pack_band::<R, HighWord, false, true>,
        (BitDepth::Ten, PlaneLayout::P010, true) => pack_band::<R, HighWord, true, true>,
    };
    pack(picture, coding, taps, steps, band);
}

/// Whether chroma is co-sited with the even luma column and row.
const fn siting(location: ChromaLocation) -> (bool, bool) {
    match location {
        ChromaLocation::Left => (true, false),
        ChromaLocation::TopLeft => (true, true),
        ChromaLocation::Unspecified | ChromaLocation::Center => (false, false),
    }
}

/// The luma taps one chroma sample sums along an axis, in summation order: offset from the
/// sample's even column or row, and weight.
#[derive(Clone, Copy)]
struct Taps {
    taps: [(i64, f32); 3],
    len: usize,
}

impl Taps {
    fn new(step: usize, cosited: bool) -> Self {
        let mut taps = [(0, 0.0); 3];
        if step == 1 {
            taps[0] = (0, 1.0);
            return Self { taps, len: 1 };
        }
        let mut len = 0;
        for offset in -1..=1_i64 {
            let weight = tap_weight(offset, cosited);
            if weight != 0.0 {
                taps[len] = (offset, weight);
                len += 1;
            }
        }
        Self { taps, len }
    }

    fn all(&self) -> &[(i64, f32)] {
        &self.taps[..self.len]
    }
}

std::thread_local! {
    static LINES: Cell<Vec<Vec<[f32; 4]>>> = const { Cell::new(Vec::new()) };
}

/// Decoded picture rows kept across chroma rows, each tagged with its row.
struct Lines {
    held: Vec<Option<usize>>,
    lines: Vec<Vec<[f32; 4]>>,
}

impl Lines {
    fn new(count: usize, width: usize) -> Self {
        let mut lines = LINES.take();
        lines.resize_with(count, Vec::new);
        for line in &mut lines {
            line.resize(width, [0.0; 4]);
        }
        Self {
            held: vec![None; count],
            lines,
        }
    }

    /// Slots holding picture rows `rows` (`None` where a row is not needed), decoding only the
    /// rows no slot holds yet.
    fn fetch<R: Fn(usize, &mut [[f32; 4]]) + Sync>(
        &mut self,
        picture: &Picture<'_, R>,
        rows: &[Option<usize>],
    ) -> [usize; 3] {
        let mut slots = [usize::MAX; 3];
        let mut used = [false; 3];
        for (slot, row) in slots.iter_mut().zip(rows) {
            let Some(row) = row else { continue };
            if let Some(held) = self.held.iter().position(|held| *held == Some(*row)) {
                *slot = held;
                used[held] = true;
            }
        }
        for index in 0..rows.len().min(3) {
            let Some(row) = rows[index] else { continue };
            if slots[index] != usize::MAX {
                continue;
            }
            // An edge row repeated within this chroma row shares one line.
            if let Some(earlier) = (0..index).find(|&earlier| rows[earlier] == Some(row)) {
                slots[index] = slots[earlier];
                continue;
            }
            let free = (0..self.lines.len())
                .find(|slot| !used[*slot])
                .expect("a free line for every needed row");
            (picture.row)(row, &mut self.lines[free]);
            self.held[free] = Some(row);
            used[free] = true;
            slots[index] = free;
        }
        slots
    }
}

impl Drop for Lines {
    fn drop(&mut self) {
        LINES.set(std::mem::take(&mut self.lines));
    }
}

fn pack_band<R, S, const ALPHA: bool, const INTERLEAVED: bool>(
    picture: &Picture<'_, R>,
    coding: Coding,
    [columns, rows]: [Taps; 2],
    (step_x, step_y): (usize, usize),
    band: Band<'_>,
) where
    R: Fn(usize, &mut [[f32; 4]]) + Sync,
    S: Store,
{
    let width = picture.width;
    let height = picture.height;
    let chroma_width = width / step_x;
    let Band { first, planes } = band;
    let mut planes = planes.into_iter();
    let mut luma = planes.next().unwrap_or_default();
    let mut chroma_planes: Vec<Vec<&mut [u8]>> = if INTERLEAVED {
        vec![planes.next().unwrap_or_default()]
    } else {
        vec![
            planes.next().unwrap_or_default(),
            planes.next().unwrap_or_default(),
        ]
    };
    let mut alpha_rows = if ALPHA {
        planes.next().unwrap_or_default()
    } else {
        Vec::new()
    };
    let chroma_rows = chroma_planes[0].len();
    // Line `o` is picture row `top + o - 1`, edge-clamped: the row above (read only by co-sited
    // 4:2:0 chroma) and the `step_y` rows covered. The row below is never read.
    let above = rows.all().iter().any(|&(offset, _)| offset < 0);
    let mut window = Lines::new(step_y + 1, width);
    for local in 0..chroma_rows {
        let top = (first + local) * step_y;
        let mut needed = [None; 3];
        for (offset, row) in needed.iter_mut().enumerate().take(step_y + 1) {
            if offset > 0 || above {
                *row = Some((top + offset).saturating_sub(1).min(height - 1));
            }
        }
        let slots = window.fetch(picture, &needed);
        let line = |offset: usize| window.lines[slots[offset]].as_slice();
        for inner in 0..step_y {
            let line = line(inner + 1);
            write_row::<S>(luma[local * step_y + inner], line, |texel| {
                coding.y(color::<ALPHA>(texel))
            });
            if ALPHA {
                write_row::<S>(alpha_rows[local * step_y + inner], line, |texel| {
                    coding.alpha(texel[3])
                });
            }
        }
        let lines = [0, 1, 2].map(|offset| {
            if slots[offset] == usize::MAX {
                &[][..]
            } else {
                line(offset)
            }
        });
        if step_x == 1 && step_y == 1 {
            write_chroma::<S, INTERLEAVED>(&mut chroma_planes, local, chroma_width, |column| {
                coding.uv(color::<ALPHA>(lines[1][column]))
            });
        } else {
            write_chroma::<S, INTERLEAVED>(&mut chroma_planes, local, chroma_width, |column| {
                subsampled_chroma::<ALPHA>(&lines, column, step_x, (&columns, &rows), coding, width)
            });
        }
    }
}

fn write_row<S: Store>(out: &mut [u8], line: &[[f32; 4]], code: impl Fn([f32; 4]) -> u16) {
    for (sample, texel) in out.chunks_exact_mut(S::BYTES).zip(line) {
        S::write(sample, code(*texel));
    }
}

fn write_chroma<S: Store, const INTERLEAVED: bool>(
    planes: &mut [Vec<&mut [u8]>],
    local: usize,
    chroma_width: usize,
    sample: impl Fn(usize) -> [u16; 2],
) {
    if INTERLEAVED {
        let pairs = planes[0][local].chunks_exact_mut(2 * S::BYTES);
        for (column, pair) in pairs.take(chroma_width).enumerate() {
            let [cb, cr] = sample(column);
            let (first, second) = pair.split_at_mut(S::BYTES);
            S::write(first, cb);
            S::write(second, cr);
        }
        return;
    }
    let (u, v) = planes.split_at_mut(1);
    let samples = u[0][local]
        .chunks_exact_mut(S::BYTES)
        .zip(v[0][local].chunks_exact_mut(S::BYTES));
    for (column, (u, v)) in samples.take(chroma_width).enumerate() {
        let [cb, cr] = sample(column);
        S::write(u, cb);
        S::write(v, cr);
    }
}

/// One subsampled chroma sample's Cb and Cr: its taps sited as the 8-bit pack sites them,
/// alpha-weighted when the layout keeps alpha. `lines[o]` is the row `o - 1` from the sample's
/// first row.
fn subsampled_chroma<const ALPHA: bool>(
    lines: &[&[[f32; 4]]; 3],
    column: usize,
    step_x: usize,
    (columns, rows): (&Taps, &Taps),
    coding: Coding,
    width: usize,
) -> [u16; 2] {
    let x = column * step_x;
    let mut sum = [0.0_f32; 3];
    let mut weight_sum = 0.0_f32;
    for &(dy, row_weight) in rows.all() {
        let line = lines[usize::try_from(1 + dy).unwrap_or(0)];
        for &(dx, column_weight) in columns.all() {
            let at = usize::try_from(i64::try_from(x).unwrap_or(0) + dx)
                .unwrap_or(0)
                .min(width - 1);
            let texel = line[at];
            let weight = row_weight * column_weight;
            if ALPHA {
                let a = texel[3].clamp(0.0, 1.0) * weight;
                for (channel, value) in sum.iter_mut().enumerate() {
                    *value += texel[channel].clamp(0.0, 1.0) * weight;
                }
                weight_sum += a;
            } else {
                let rgb = color::<false>(texel);
                for (channel, value) in sum.iter_mut().enumerate() {
                    *value += rgb[channel] * weight;
                }
            }
        }
    }
    if !ALPHA {
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
