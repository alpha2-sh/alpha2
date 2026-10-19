//! Media tools on decoded frames: picking a representative thumbnail,
//! making proxy-sized frames, and resizing a picture to the size it is shown
//! at. Pure functions of the frame, with no `FFmpeg`.

use alpha2_media::{DecodedVideoFrame, PixelFormat, Plane, Rect};

/// Samples across and down a frame when it is scored.
const SCORE_GRID: (u32, u32) = (128, 72);

/// Mean luma, as a fraction of full scale, below which a frame is black and
/// above which it is white: neither makes a thumbnail.
pub const THUMBNAIL_LUMA_RANGE: (f64, f64) = (0.08, 0.92);

/// How a frame would do as a thumbnail.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct FrameScore {
    /// Mean luma, `0..=1` of the code range.
    pub mean_luma: f64,
    /// Variance of the 4-neighbour Laplacian of luma over a fixed grid: high
    /// for detail, low for blur and flat frames.
    pub sharpness: f64,
}

/// Scores a frame on a fixed grid of luma samples, so the score does not
/// depend on the frame's size.
///
/// # Errors
/// Returns a message when a plane is shorter than its declared layout.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub fn score_frame(frame: &DecodedVideoFrame) -> Result<FrameScore, String> {
    let (columns, rows) = SCORE_GRID;
    let crop = frame.visible_rect;
    if crop.width == 0 || crop.height == 0 {
        return Err("frame has no visible pixels".into());
    }
    let mut luma = Vec::with_capacity((columns * rows) as usize);
    for row in 0..rows {
        for column in 0..columns {
            let x =
                crop.x + (u64::from(column) * u64::from(crop.width) / u64::from(columns)) as u32;
            let y = crop.y + (u64::from(row) * u64::from(crop.height) / u64::from(rows)) as u32;
            luma.push(luma_at(frame, x, y)?);
        }
    }
    let mean = luma.iter().sum::<f64>() / luma.len() as f64;
    let at = |column: u32, row: u32| luma[(row * columns + column) as usize];
    let mut laplacians = Vec::with_capacity(luma.len());
    for row in 1..rows - 1 {
        for column in 1..columns - 1 {
            laplacians.push(
                4.0 * at(column, row)
                    - at(column - 1, row)
                    - at(column + 1, row)
                    - at(column, row - 1)
                    - at(column, row + 1),
            );
        }
    }
    let average = laplacians.iter().sum::<f64>() / laplacians.len() as f64;
    let sharpness = laplacians
        .iter()
        .map(|value| (value - average).powi(2))
        .sum::<f64>()
        / laplacians.len() as f64;
    Ok(FrameScore {
        mean_luma: mean,
        sharpness,
    })
}

/// The best thumbnail among scored frames: the sharpest that is neither
/// black nor white, or the sharpest of all when every frame is. `None` for
/// no frames.
#[must_use]
pub fn choose_thumbnail(scores: &[FrameScore]) -> Option<usize> {
    let (dark, bright) = THUMBNAIL_LUMA_RANGE;
    let sharpest = |acceptable: &dyn Fn(&FrameScore) -> bool| {
        scores
            .iter()
            .enumerate()
            .filter(|(_, score)| acceptable(score))
            .max_by(|(_, a), (_, b)| a.sharpness.total_cmp(&b.sharpness))
            .map(|(index, _)| index)
    };
    sharpest(&|score| (dark..=bright).contains(&score.mean_luma)).or_else(|| sharpest(&|_| true))
}

/// Luma at a pixel, `0..=1` of the code range: the first plane of a YCbCr
/// frame, or BT.709 weights on the RGB codes of an RGBA one.
fn luma_at(frame: &DecodedVideoFrame, x: u32, y: u32) -> Result<f64, String> {
    let plane = frame.planes.first().ok_or("frame has no planes")?;
    match frame.pixel_format {
        PixelFormat::Yuv(format) => {
            let wide = format.bit_depth > 8;
            let value = f64::from(sample(plane, x, y, if wide { 2 } else { 1 }, 1, 0)?);
            // An MSB-aligned sample (P010) fills the whole `u16`.
            let full_scale = if format.msb_aligned {
                65_535.0
            } else {
                f64::from((1_u32 << format.bit_depth) - 1)
            };
            Ok(value / full_scale)
        }
        PixelFormat::Rgba8 => {
            let channel = |index| sample(plane, x, y, 1, 4, index).map(f64::from);
            Ok((0.2126 * channel(0)? + 0.7152 * channel(1)? + 0.0722 * channel(2)?) / 255.0)
        }
        other => Err(format!("cannot read luma from {other:?}")),
    }
}

/// Component `component` of the sample at `(x, y)` of a plane with
/// `components` components per position, each `bytes` wide (a
/// little-endian `u16` when two).
fn sample(
    plane: &Plane,
    x: u32,
    y: u32,
    bytes: usize,
    components: usize,
    component: usize,
) -> Result<u16, String> {
    let offset = y as usize * plane.stride as usize + (x as usize * components + component) * bytes;
    let slice = plane
        .bytes
        .get(offset..offset + bytes)
        .ok_or("plane is shorter than its layout")?;
    Ok(if bytes == 2 {
        u16::from_le_bytes([slice[0], slice[1]])
    } else {
        u16::from(slice[0])
    })
}

/// The frame shrunk by `factor` (2 for half size, 4 for quarter), each output
/// sample the mean of the `factor x factor` block it covers, the visible
/// size rounded down to even numbers so 4:2:0 encoders take it.
///
/// # Errors
/// Returns a message for a factor below 2, a frame too small to shrink, or a
/// plane shorter than its layout.
pub fn downscale_frame(
    frame: &DecodedVideoFrame,
    factor: u32,
) -> Result<DecodedVideoFrame, String> {
    if factor < 2 {
        return Err("a proxy factor is 2 or more".into());
    }
    let crop = frame.visible_rect;
    let (width, height) = ((crop.width / factor) & !1, (crop.height / factor) & !1);
    if width == 0 || height == 0 {
        return Err(format!(
            "a {}x{} frame is too small to shrink by {factor}",
            crop.width, crop.height
        ));
    }
    let planes = match frame.pixel_format {
        PixelFormat::Rgba8 => vec![shrink_plane(
            plane(frame, 0)?,
            (crop.x, crop.y),
            (width, height),
            factor,
            1,
            4,
        )?],
        PixelFormat::Yuv(format) => {
            let bytes = if format.bit_depth > 8 { 2 } else { 1 };
            let (shift_x, shift_y) = (
                u32::from(format.chroma_shift_x),
                u32::from(format.chroma_shift_y),
            );
            let chroma_size = (width.div_ceil(1 << shift_x), height.div_ceil(1 << shift_y));
            let chroma_origin = (crop.x >> shift_x, crop.y >> shift_y);
            let mut planes = vec![shrink_plane(
                plane(frame, 0)?,
                (crop.x, crop.y),
                (width, height),
                factor,
                bytes,
                1,
            )?];
            if format.interleaved_chroma {
                planes.push(shrink_plane(
                    plane(frame, 1)?,
                    chroma_origin,
                    chroma_size,
                    factor,
                    bytes,
                    2,
                )?);
            } else {
                for index in 1..frame.planes.len() {
                    planes.push(shrink_plane(
                        plane(frame, index)?,
                        chroma_origin,
                        chroma_size,
                        factor,
                        bytes,
                        1,
                    )?);
                }
            }
            planes
        }
        other => return Err(format!("cannot shrink {other:?} frames")),
    };
    Ok(DecodedVideoFrame {
        visible_rect: Rect {
            x: 0,
            y: 0,
            width,
            height,
        },
        planes,
        ..frame.clone()
    })
}

/// Straight-alpha RGBA `pixels`, `width` x `height`, resized to
/// `new_width` x `new_height`: each axis resampled by a tent filter as wide
/// as a source pixel or an output pixel, whichever is wider, so an axis that
/// shrinks averages what each output pixel covers (as a proxy averages its
/// blocks) and one that grows interpolates. Colour is weighted by alpha, so a
/// transparent pixel's colour does not bleed into its neighbours. An axis
/// that keeps its size is left exactly as it is.
///
/// # Errors
/// Returns a message for an empty size or a buffer that is not
/// `width` x `height` pixels.
pub fn resize_rgba(
    pixels: &[u8],
    (width, height): (u32, u32),
    (new_width, new_height): (u32, u32),
) -> Result<Vec<u8>, String> {
    let (columns, rows) = (width as usize, height as usize);
    if width == 0 || height == 0 || new_width == 0 || new_height == 0 {
        return Err("cannot resize an empty picture".to_owned());
    }
    if columns
        .checked_mul(rows)
        .and_then(|count| count.checked_mul(4))
        != Some(pixels.len())
    {
        return Err("RGBA buffer does not match its size".to_owned());
    }
    // Alpha-weighted colour and alpha, in 0..=1.
    let mut picture = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[r, g, b, a]| {
            let alpha = f32::from(a) / 255.0;
            [
                f32::from(r) / 255.0 * alpha,
                f32::from(g) / 255.0 * alpha,
                f32::from(b) / 255.0 * alpha,
                alpha,
            ]
        })
        .collect::<Vec<_>>();
    let (mut current_columns, mut current_rows) = (columns, rows);
    if new_width != width {
        let taps = tent_taps(columns, new_width as usize);
        let mut resized = vec![0.0_f32; new_width as usize * rows * 4];
        for y in 0..rows {
            for (x, row_taps) in taps.iter().enumerate() {
                let out = &mut resized[(y * new_width as usize + x) * 4..][..4];
                for &(source, weight) in row_taps {
                    for (channel, value) in out.iter_mut().enumerate() {
                        *value += weight * picture[(y * columns + source) * 4 + channel];
                    }
                }
            }
        }
        picture = resized;
        current_columns = new_width as usize;
    }
    if new_height != height {
        let taps = tent_taps(rows, new_height as usize);
        let mut resized = vec![0.0_f32; current_columns * new_height as usize * 4];
        for (y, column_taps) in taps.iter().enumerate() {
            for x in 0..current_columns {
                let out = &mut resized[(y * current_columns + x) * 4..][..4];
                for &(source, weight) in column_taps {
                    for (channel, value) in out.iter_mut().enumerate() {
                        *value += weight * picture[(source * current_columns + x) * 4 + channel];
                    }
                }
            }
        }
        picture = resized;
        current_rows = new_height as usize;
    }
    debug_assert_eq!(picture.len(), current_columns * current_rows * 4);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let code = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    Ok(picture
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[r, g, b, a]| {
            let colour = |value: f32| if a > 0.0 { code(value / a) } else { 0 };
            [colour(r), colour(g), colour(b), code(a)]
        })
        .collect())
}

/// For each of `outputs` samples along an axis of `inputs`, the input
/// samples it weighs and their weights, which sum to one: a tent centred
/// where the output sample's centre falls, reaching the larger of the two
/// sample spacings to each side, its samples clamped to the edge.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn tent_taps(inputs: usize, outputs: usize) -> Vec<Vec<(usize, f32)>> {
    let scale = inputs as f64 / outputs as f64;
    let radius = scale.max(1.0);
    (0..outputs)
        .map(|output| {
            let centre = (output as f64 + 0.5) * scale - 0.5;
            let first = (centre - radius).floor() as i64;
            let last = (centre + radius).ceil() as i64;
            let mut taps = Vec::new();
            for input in first..=last {
                let weight = 1.0 - (input as f64 - centre).abs() / radius;
                if weight <= 0.0 {
                    continue;
                }
                let clamped = input.clamp(0, inputs as i64 - 1) as usize;
                match taps.iter_mut().find(|(index, _)| *index == clamped) {
                    Some((_, total)) => *total += weight,
                    None => taps.push((clamped, weight)),
                }
            }
            let sum: f64 = taps.iter().map(|(_, weight)| weight).sum();
            taps.into_iter()
                .map(|(index, weight)| (index, (weight / sum) as f32))
                .collect()
        })
        .collect()
}

fn plane(frame: &DecodedVideoFrame, index: usize) -> Result<&Plane, String> {
    frame
        .planes
        .get(index)
        .ok_or_else(|| format!("frame has no plane {index}"))
}

/// A `size` plane whose samples average `factor x factor` blocks of `plane`
/// from `origin` on, clamped to the source plane's edge.
///
/// The plane's length is checked once; each output row then sums whole
/// source rows, `factor` of them, so a sample is read by indexing a row.
fn shrink_plane(
    source: &Plane,
    origin: (u32, u32),
    size: (u32, u32),
    factor: u32,
    bytes: usize,
    components: usize,
) -> Result<Plane, String> {
    let (width, height) = size;
    let stride = width as usize * components * bytes;
    let (source_width, source_height) = (source.width as usize, source.height as usize);
    let source_stride = source.stride as usize;
    let row_bytes = source_width * components * bytes;
    if source_width == 0
        || source_height == 0
        || source_stride < row_bytes
        || source.bytes.len() < (source_height - 1) * source_stride + row_bytes
    {
        return Err("plane is shorter than its layout".to_owned());
    }
    // The source column each output column's `dx`-th sample comes from, clamped to the edge.
    let columns = (0..width as usize)
        .flat_map(|x| {
            (0..factor as usize)
                .map(move |dx| (origin.0 as usize + x * factor as usize + dx).min(source_width - 1))
        })
        .collect::<Vec<_>>();
    let count = u64::from(factor * factor);
    let mut sums = vec![0_u64; width as usize * components];
    let mut output = Vec::with_capacity(stride * height as usize);
    for y in 0..height as usize {
        sums.fill(0);
        for dy in 0..factor as usize {
            let sy = (origin.1 as usize + y * factor as usize + dy).min(source_height - 1);
            let row = &source.bytes[sy * source_stride..sy * source_stride + row_bytes];
            if bytes == 2 {
                accumulate(&mut sums, &columns, factor as usize, components, |at| {
                    u64::from(u16::from_le_bytes([row[2 * at], row[2 * at + 1]]))
                });
            } else {
                accumulate(&mut sums, &columns, factor as usize, components, |at| {
                    u64::from(row[at])
                });
            }
        }
        for sum in &sums {
            let mean = (sum + count / 2) / count;
            if bytes == 2 {
                output.extend_from_slice(&u16::try_from(mean).unwrap_or(u16::MAX).to_le_bytes());
            } else {
                output.push(u8::try_from(mean).unwrap_or(u8::MAX));
            }
        }
    }
    Ok(Plane {
        bytes: output.into(),
        stride: u32::try_from(stride).map_err(|_| "plane is too wide")?,
        width,
        height,
    })
}

/// Adds one source row's samples into `sums`, one per output sample: each
/// output column reads the `factor` source columns `columns` lists for it,
/// and `read` gives the sample at a sample index of the row.
fn accumulate(
    sums: &mut [u64],
    columns: &[usize],
    factor: usize,
    components: usize,
    read: impl Fn(usize) -> u64,
) {
    for (x, sources) in columns.chunks_exact(factor).enumerate() {
        for component in 0..components {
            sums[x * components + component] += sources
                .iter()
                .map(|column| read(column * components + component))
                .sum::<u64>();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alpha2_media::{ChromaLocation, Orientation, SampleAspectRatio, YuvFormat};
    use alpha2_time::MediaTime;

    /// An I420 frame whose luma is `luma(x, y)` and chroma mid-grey.
    fn frame(width: u32, height: u32, luma: impl Fn(u32, u32) -> u8) -> DecodedVideoFrame {
        let y_plane = Plane {
            bytes: (0..height)
                .flat_map(|y| (0..width).map(move |x| (x, y)))
                .map(|(x, y)| luma(x, y))
                .collect::<Vec<_>>()
                .into(),
            stride: width,
            width,
            height,
        };
        let chroma = |value| Plane {
            bytes: vec![value; (width.div_ceil(2) * height.div_ceil(2)) as usize].into(),
            stride: width.div_ceil(2),
            width: width.div_ceil(2),
            height: height.div_ceil(2),
        };
        DecodedVideoFrame {
            pts: MediaTime::ZERO,
            dts: None,
            duration: None,
            visible_rect: Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            sample_aspect_ratio: SampleAspectRatio::default(),
            orientation: Orientation::UPRIGHT,
            pixel_format: PixelFormat::Yuv(YuvFormat::I420),
            chroma_location: ChromaLocation::default(),
            color: None,
            planes: vec![y_plane, chroma(128), chroma(128)],
        }
    }

    #[test]
    fn the_thumbnail_is_the_sharpest_frame_neither_black_nor_white() {
        let black = frame(128, 72, |_, _| 5);
        // White with sparse black dots: the sharpest frame, but white.
        let white = frame(
            128,
            72,
            |x, y| {
                if x % 4 == 0 && y % 4 == 0 { 0 } else { 255 }
            },
        );
        let soft = frame(128, 72, |x, _| {
            u8::try_from(40 + x * 150 / 128).expect("small")
        });
        let detailed = frame(
            128,
            72,
            |x, y| {
                if (x / 4 + y / 4) % 2 == 0 { 40 } else { 200 }
            },
        );
        let scores =
            [&black, &white, &soft, &detailed].map(|frame| score_frame(frame).expect("score"));
        assert!(scores[0].mean_luma < THUMBNAIL_LUMA_RANGE.0);
        assert!(scores[1].mean_luma > THUMBNAIL_LUMA_RANGE.1);
        assert!(scores[3].sharpness > scores[2].sharpness);
        assert!(scores[1].sharpness > scores[3].sharpness);
        assert_eq!(choose_thumbnail(&scores), Some(3));
        // All black or white: the sharpest anyway.
        assert_eq!(choose_thumbnail(&scores[..2]), Some(1));
        assert_eq!(choose_thumbnail(&[]), None);
    }

    #[test]
    fn a_proxy_averages_blocks_and_keeps_an_even_size() {
        // A 10x6 frame with a horizontal ramp: half size is 4x2 after rounding 5x3 down to even,
        // each sample the mean of its 2x2 block.
        let source = frame(10, 6, |x, _| u8::try_from(x * 20).expect("small"));
        let half = downscale_frame(&source, 2).expect("proxy");
        assert_eq!((half.visible_rect.width, half.visible_rect.height), (4, 2));
        assert_eq!(&half.planes[0].bytes[..4], &[10, 50, 90, 130]);
        assert_eq!((half.planes[1].width, half.planes[1].height), (2, 1));
        assert!(half.planes[1].bytes.iter().all(|value| *value == 128));
        assert!(downscale_frame(&source, 1).is_err());
        assert!(downscale_frame(&frame(3, 3, |_, _| 0), 4).is_err());
    }

    #[test]
    fn a_resize_keeps_flat_colour_weighs_alpha_and_leaves_a_kept_axis_alone() {
        let rgba = |pixels: &[[u8; 4]]| pixels.iter().flatten().copied().collect::<Vec<_>>();
        let flat = rgba(&[[200, 100, 50, 255]; 60 * 4]);
        for size in [(64, 4), (32, 4), (60, 7), (17, 3)] {
            let resized = resize_rgba(&flat, (60, 4), size).expect("resize");
            assert!(
                resized.chunks(4).all(|pixel| pixel == [200, 100, 50, 255]),
                "{size:?}"
            );
        }
        // A transparent pixel's colour is no one's: halving red and a clear blue gives half-clear
        // red.
        let half = resize_rgba(&rgba(&[[255, 0, 0, 255], [0, 0, 255, 0]]), (2, 1), (1, 1))
            .expect("resize");
        assert_eq!(half, [255, 0, 0, 128]);
        // A 2:1 stretch interpolates between the stored samples and keeps the rows untouched.
        let ramp = rgba(&[[0, 0, 0, 255], [200, 200, 200, 255]]);
        let stretched = resize_rgba(&ramp, (2, 1), (4, 1)).expect("resize");
        assert_eq!(
            stretched
                .chunks(4)
                .map(|pixel| pixel[0])
                .collect::<Vec<_>>(),
            [0, 50, 150, 200]
        );
        assert!(resize_rgba(&ramp, (2, 1), (0, 1)).is_err());
        assert!(resize_rgba(&ramp, (3, 1), (4, 1)).is_err());
    }

    /// A `size` plane whose samples average `factor x factor` blocks of
    /// `plane` from `origin` on, clamped to the source plane's edge, read
    /// sample by sample through a bounds-checked call: the reference the
    /// row-at-a-time implementation must match byte for byte.
    fn reference_shrink_plane(
        source: &Plane,
        origin: (u32, u32),
        size: (u32, u32),
        factor: u32,
        bytes: usize,
        components: usize,
    ) -> Result<Plane, String> {
        let (width, height) = size;
        let stride = width as usize * components * bytes;
        let mut output = Vec::with_capacity(stride * height as usize);
        for y in 0..height {
            for x in 0..width {
                for component in 0..components {
                    let mut sum = 0_u64;
                    let mut count = 0_u64;
                    for dy in 0..factor {
                        for dx in 0..factor {
                            let sx =
                                (origin.0 + x * factor + dx).min(source.width.saturating_sub(1));
                            let sy =
                                (origin.1 + y * factor + dy).min(source.height.saturating_sub(1));
                            sum += u64::from(sample(source, sx, sy, bytes, components, component)?);
                            count += 1;
                        }
                    }
                    let mean = (sum + count / 2) / count;
                    if bytes == 2 {
                        output.extend_from_slice(
                            &u16::try_from(mean).unwrap_or(u16::MAX).to_le_bytes(),
                        );
                    } else {
                        output.push(u8::try_from(mean).unwrap_or(u8::MAX));
                    }
                }
            }
        }
        Ok(Plane {
            bytes: output.into(),
            stride: u32::try_from(stride).map_err(|_| "plane is too wide")?,
            width,
            height,
        })
    }

    /// Shrinking a proxy row by row gives the same bytes as reading every
    /// sample through a bounds-checked call, edges and offsets included.
    #[test]
    fn shrinking_by_rows_gives_the_samples_sample_by_sample_did() {
        let plane = |width: u32, height: u32, bytes: usize, components: usize| {
            let stride = width as usize * components * bytes + 3;
            Plane {
                bytes: (0..stride * height as usize)
                    .map(|index| u8::try_from((index * 7919 + index / 13) % 251).expect("byte"))
                    .collect::<Vec<_>>()
                    .into(),
                stride: u32::try_from(stride).expect("stride"),
                width,
                height,
            }
        };
        for (width, height) in [(17, 9), (64, 48), (1, 1)] {
            for (bytes, components) in [(1, 1), (2, 1), (1, 2), (2, 2)] {
                let source = plane(width, height, bytes, components);
                for factor in [2, 4] {
                    for origin in [(0, 0), (3, 1)] {
                        let size = ((width / factor).max(1), (height / factor).max(1));
                        let expected = reference_shrink_plane(
                            &source, origin, size, factor, bytes, components,
                        )
                        .expect("reference");
                        let found = shrink_plane(&source, origin, size, factor, bytes, components)
                            .expect("rows");
                        assert_eq!(
                            found.bytes, expected.bytes,
                            "{width}x{height} {bytes}B x{components} /{factor} at {origin:?}"
                        );
                    }
                }
            }
        }
    }
}
