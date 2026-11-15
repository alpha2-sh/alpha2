//! Checks of exported pictures and files that the desktop and device suites share.

/// Every hardware encoder stays above this on the transcode test's frames; a chroma plane swapped,
/// shifted by a row or offset by a column falls far below it.
pub const PLANE_PSNR_FLOOR_DB: f64 = 38.0;

/// The PSNR of `exported` against `original`, 8-bit samples, in dB; infinite when equal.
#[must_use]
pub fn plane_psnr(exported: &[u8], original: &[u8]) -> f64 {
    let squared = exported
        .iter()
        .zip(original)
        .map(|(a, b)| {
            let difference = f64::from(*a) - f64::from(*b);
            difference * difference
        })
        .sum::<f64>();
    if squared == 0.0 {
        return f64::INFINITY;
    }
    #[allow(clippy::cast_precision_loss)]
    let samples = exported.len() as f64;
    10.0 * (255.0 * 255.0 * samples / squared).log10()
}

fn plane_sad(exported: &[u8], original: &[u8], width: usize, row_offset: usize) -> u64 {
    exported
        .chunks(width)
        .zip(original.chunks(width).skip(row_offset))
        .flat_map(|(a, b)| a.iter().zip(b))
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum()
}

/// Compares frames of packed I420 planes (`[frame][plane][sample]`, `width` luma columns) with
/// their sources. Every plane must reach `floor_db`, and each chroma plane must be closer to its
/// own source plane than to the other one or to itself one row down, to catch swaps and shifts.
///
/// # Errors
/// What differs, by frame and plane.
pub fn plane_check(
    exported: &[Vec<Vec<u8>>],
    original: &[Vec<Vec<u8>>],
    width: u32,
    floor_db: f64,
) -> Result<(), String> {
    if exported.len() != original.len() {
        return Err(format!(
            "{} frames exported for {} source frames",
            exported.len(),
            original.len()
        ));
    }
    let chroma_width = width.div_ceil(2) as usize;
    for (index, (frame, source)) in exported.iter().zip(original).enumerate() {
        if frame.len() != 3 || source.len() != 3 {
            return Err(format!("frame {index}: not three planes"));
        }
        for plane in 0..3 {
            if frame[plane].len() != source[plane].len() {
                return Err(format!("frame {index} plane {plane}: another size"));
            }
            let psnr = plane_psnr(&frame[plane], &source[plane]);
            if psnr < floor_db {
                return Err(format!(
                    "frame {index} plane {plane}: {psnr:.2} dB, below the {floor_db} dB floor"
                ));
            }
        }
        for (plane, other) in [(1, 2), (2, 1)] {
            let own = plane_sad(&frame[plane], &source[plane], chroma_width, 0);
            let swapped = plane_sad(&frame[plane], &source[other], chroma_width, 0);
            let shifted = plane_sad(&frame[plane], &source[plane], chroma_width, 1);
            // A swap or shift that leaves the source unchanged cannot be detected.
            let swap_visible = plane_sad(&source[plane], &source[other], chroma_width, 0) > 0;
            let shift_visible = plane_sad(&source[plane], &source[plane], chroma_width, 1) > 0;
            if (swap_visible && own >= swapped) || (shift_visible && own >= shifted) {
                return Err(format!(
                    "frame {index} plane {plane}: {own} from its source plane, {swapped} from the \
                     other chroma plane, {shifted} from its own one row down"
                ));
            }
        }
    }
    Ok(())
}

/// A Matroska file's blocks in file order, as (track number, milliseconds), and its cluster times.
///
/// # Panics
/// For bytes that are not a Matroska file with clusters.
#[must_use]
pub fn matroska_blocks(bytes: &[u8]) -> (Vec<(u64, i64)>, Vec<i64>) {
    fn vint(bytes: &[u8], at: &mut usize, keep_marker: bool) -> u64 {
        let first = bytes[*at];
        let length = first.leading_zeros() as usize + 1;
        let mut value = if keep_marker {
            u64::from(first)
        } else {
            u64::from(first) & ((1 << (8 - length)) - 1)
        };
        for index in 1..length {
            value = (value << 8) | u64::from(bytes[*at + index]);
        }
        *at += length;
        value
    }
    fn walk(bytes: &[u8], blocks: &mut Vec<(u64, i64)>, clusters: &mut Vec<i64>) {
        let mut at = 0;
        while at < bytes.len() {
            let id = vint(bytes, &mut at, true);
            let size = usize::try_from(vint(bytes, &mut at, false)).expect("size");
            let body = &bytes[at..at + size];
            match id {
                // Segment, Cluster and BlockGroup hold the elements read here.
                0x1853_8067 | 0x1F43_B675 | 0xA0 => walk(body, blocks, clusters),
                0xE7 => {
                    let time = body
                        .iter()
                        .fold(0_i64, |time, byte| time << 8 | i64::from(*byte));
                    clusters.push(time);
                }
                // SimpleBlock and Block: track number, then a 16-bit time relative to the cluster.
                0xA3 | 0xA1 => {
                    let mut offset = 0;
                    let track = vint(body, &mut offset, false);
                    let relative = i16::from_be_bytes([body[offset], body[offset + 1]]);
                    let cluster = *clusters.last().expect("a block inside a cluster");
                    blocks.push((track, cluster + i64::from(relative)));
                }
                _ => {}
            }
            at += size;
        }
    }
    let (mut blocks, mut clusters) = (Vec::new(), Vec::new());
    walk(bytes, &mut blocks, &mut clusters);
    (blocks, clusters)
}

/// The largest distance in milliseconds between the latest block of track 1 and of track 2 seen
/// at any point of a file-order walk of Matroska `blocks`.
#[must_use]
pub fn widest_track_distance(blocks: &[(u64, i64)]) -> i64 {
    let mut last = [None, None];
    let mut widest = 0;
    for &(track, time) in blocks {
        if let Some(slot) = track
            .checked_sub(1)
            .and_then(|index| last.get_mut(usize::try_from(index).ok()?))
        {
            *slot = Some(time);
        }
        if let [Some(first), Some(second)] = last {
            widest = widest.max((first - second).abs());
        }
    }
    widest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plane_check_refuses_a_swapped_or_shifted_chroma_plane() {
        let (width, height) = (256_usize, 144_usize);
        let (chroma_width, chroma_height) = (width / 2, height / 2);
        // U varies along rows and V along columns, so a swap, row shift or column offset each show.
        let plane = |w: usize, h: usize, sample: &dyn Fn(usize, usize) -> u8| -> Vec<u8> {
            (0..h)
                .flat_map(|y| (0..w).map(move |x| sample(x, y)))
                .collect()
        };
        let frame = |noise: u8| -> Vec<Vec<u8>> {
            vec![
                plane(width, height, &|x, y| {
                    u8::try_from((x * 3 + y * 5) % 200 + 16).expect("luma")
                        ^ (noise & u8::try_from(x % 2).expect("bit"))
                }),
                plane(chroma_width, chroma_height, &|_, y| {
                    u8::try_from(16 + y * 3 % 200).expect("u")
                }),
                plane(chroma_width, chroma_height, &|x, _| {
                    u8::try_from(16 + x * 2 % 200).expect("v")
                }),
            ]
        };
        let w32 = u32::try_from(width).expect("w");
        let check = |exported: &[Vec<Vec<u8>>], source: &[Vec<Vec<u8>>]| {
            plane_check(exported, source, w32, PLANE_PSNR_FLOOR_DB)
        };
        let source = vec![frame(0); 3];
        assert_eq!(check(&source, &source), Ok(()));
        assert_eq!(
            check(&vec![frame(1); 3], &source),
            Ok(()),
            "one code of noise"
        );
        let swapped: Vec<Vec<Vec<u8>>> = source
            .iter()
            .map(|planes| vec![planes[0].clone(), planes[2].clone(), planes[1].clone()])
            .collect();
        assert!(check(&swapped, &source).is_err(), "U and V swapped");
        let shift = |plane: &[u8], rows: usize, columns: usize| -> Vec<u8> {
            (0..chroma_height)
                .flat_map(|y| {
                    (0..chroma_width).map(move |x| {
                        plane[(y + rows).min(chroma_height - 1) * chroma_width
                            + (x + columns).min(chroma_width - 1)]
                    })
                })
                .collect()
        };
        for (rows, columns, what) in [(1, 0, "one row"), (0, 1, "one column")] {
            let moved: Vec<Vec<Vec<u8>>> = source
                .iter()
                .map(|planes| {
                    vec![
                        planes[0].clone(),
                        shift(&planes[1], rows, columns),
                        shift(&planes[2], rows, columns),
                    ]
                })
                .collect();
            assert!(check(&moved, &source).is_err(), "chroma moved {what}");
        }
    }

    #[test]
    fn the_widest_track_distance_follows_the_file_order() {
        let blocks = [(1, 0), (2, 0), (2, 1_500), (1, 40), (1, 1_600), (2, 1_520)];
        assert_eq!(widest_track_distance(&blocks), 1_500);
        assert_eq!(widest_track_distance(&[(1, 0), (1, 9_000)]), 0);
    }
}
