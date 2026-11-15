//! Registration checks of what a font makes the text host do, read from the raw tables in time
//! linear in their size. The font crates bound depth, not the work a shared subgraph multiplies.

use std::collections::HashMap;

use rustybuzz::ttf_parser::{self, Tag};

/// Most simple glyphs one glyph may expand to through its components.
const MAX_COMPOSITE_LEAVES: u64 = 1_024;

/// Most paints one COLR v1 base glyph may visit, counting shared paints once per use.
const MAX_PAINT_VISITS: u64 = 16_384;

/// Most faces one collection may hold.
pub(crate) const MAX_COLLECTION_FACES: u32 = 256;

/// Refuses a face whose composite glyphs or color paint graph pass a budget or refer to themselves.
pub(crate) fn check_face(face: &ttf_parser::Face<'_>) -> Result<(), String> {
    let raw = face.raw_face();
    if let (Some(glyf), Some(loca)) = (
        raw.table(Tag::from_bytes(b"glyf")),
        raw.table(Tag::from_bytes(b"loca")),
    ) {
        let long = matches!(
            face.tables().head.index_to_location_format,
            ttf_parser::head::IndexToLocationFormat::Long
        );
        composite_leaves(glyf, loca, long, face.number_of_glyphs())?;
    }
    if let Some(colr) = raw.table(Tag::from_bytes(b"COLR")) {
        paint_visits(colr)?;
    }
    Ok(())
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn u24_at(data: &[u8], at: usize) -> Option<u32> {
    let bytes = data.get(at..at + 3)?;
    Some(u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

/// Walks a graph from every root without recursion, summing each node's weight and its
/// children's totals, memoized. `children` lists a node's children; a node reached again while
/// it is being counted is a ring.
fn totals<N: Copy + Eq + std::hash::Hash>(
    roots: impl IntoIterator<Item = N>,
    weight: impl Fn(N) -> u64,
    children: impl Fn(N) -> Vec<N>,
    limit: u64,
    what: &str,
) -> Result<(), String> {
    // `None` while counting, the total once counted.
    let mut counted = HashMap::<N, Option<u64>>::new();
    for root in roots {
        if counted.contains_key(&root) {
            continue;
        }
        let mut stack = vec![(root, children(root), 0_usize, weight(root))];
        counted.insert(root, None);
        while let Some((node, below, next, total)) = stack.last_mut() {
            if let Some(&child) = below.get(*next) {
                *next += 1;
                match counted.get(&child) {
                    Some(Some(child_total)) => *total = total.saturating_add(*child_total),
                    Some(None) => return Err(format!("{what} refers to itself")),
                    None => {
                        counted.insert(child, None);
                        let entry = (child, children(child), 0, weight(child));
                        stack.push(entry);
                    }
                }
                continue;
            }
            let (node, total) = (*node, *total);
            if total > limit {
                return Err(format!("{what} expands to more than {limit}"));
            }
            counted.insert(node, Some(total));
            stack.pop();
            if let Some((_, _, _, parent)) = stack.last_mut() {
                *parent = parent.saturating_add(total);
            }
        }
    }
    Ok(())
}

/// Checks the simple glyphs each glyph expands to through its components.
fn composite_leaves(glyf: &[u8], loca: &[u8], long: bool, glyphs: u16) -> Result<(), String> {
    let range = |glyph: u16| -> Option<&[u8]> {
        let index = usize::from(glyph);
        let (start, end) = if long {
            (u32_at(loca, index * 4)?, u32_at(loca, index * 4 + 4)?)
        } else {
            (
                u32::from(u16_at(loca, index * 2)?) * 2,
                u32::from(u16_at(loca, index * 2 + 2)?) * 2,
            )
        };
        glyf.get(usize::try_from(start).ok()?..usize::try_from(end).ok()?)
    };
    let components = |glyph: u16| -> Vec<u16> {
        let Some(data) = range(glyph) else {
            return Vec::new();
        };
        if u16_at(data, 0).is_none_or(|contours| contours & 0x8000 == 0) {
            return Vec::new();
        }
        let mut found = Vec::new();
        let mut at = 10;
        while let (Some(flags), Some(component)) = (u16_at(data, at), u16_at(data, at + 2)) {
            found.push(component);
            at += 4 + if flags & 0x0001 != 0 { 4 } else { 2 };
            at += if flags & 0x0008 != 0 {
                2
            } else if flags & 0x0040 != 0 {
                4
            } else if flags & 0x0080 != 0 {
                8
            } else {
                0
            };
            if flags & 0x0020 == 0 {
                break;
            }
        }
        found
    };
    let simple = |glyph: u16| -> u64 {
        u64::from(
            range(glyph)
                .is_some_and(|data| u16_at(data, 0).is_some_and(|contours| contours & 0x8000 == 0)),
        )
    };
    totals(
        0..glyphs,
        simple,
        components,
        MAX_COMPOSITE_LEAVES,
        "a composite glyph",
    )
}

/// Checks the paints each COLR v1 base glyph visits.
fn paint_visits(colr: &[u8]) -> Result<(), String> {
    if u16_at(colr, 0) != Some(1) {
        return Ok(());
    }
    let offset = |at: usize| u32_at(colr, at).filter(|offset| *offset != 0);
    let Some(base_list) = offset(14).and_then(|at| usize::try_from(at).ok()) else {
        return Ok(());
    };
    let layer_list = offset(18).and_then(|at| usize::try_from(at).ok());
    let mut bases = HashMap::new();
    for record in 0..u32_at(colr, base_list).unwrap_or(0) {
        let at = base_list + 4 + 6 * usize::try_from(record).unwrap_or(usize::MAX);
        let (Some(glyph), Some(paint)) = (u16_at(colr, at), u32_at(colr, at + 2)) else {
            break;
        };
        bases.insert(
            glyph,
            base_list + usize::try_from(paint).unwrap_or(usize::MAX),
        );
    }
    let child = |paint: usize, at: usize| -> Option<usize> {
        let relative = u24_at(colr, paint + at).filter(|offset| *offset != 0)?;
        paint.checked_add(usize::try_from(relative).ok()?)
    };
    let children = |paint: usize| -> Vec<usize> {
        match colr.get(paint) {
            Some(1) => {
                let (Some(count), Some(first), Some(list)) =
                    (colr.get(paint + 1), u32_at(colr, paint + 2), layer_list)
                else {
                    return Vec::new();
                };
                (0..u32::from(*count))
                    .filter_map(|layer| {
                        let index = usize::try_from(first.checked_add(layer)?).ok()?;
                        let relative = u32_at(colr, list + 4 + 4 * index)?;
                        list.checked_add(usize::try_from(relative).ok()?)
                    })
                    .collect()
            }
            Some(11) => u16_at(colr, paint + 1)
                .and_then(|glyph| bases.get(&glyph).copied())
                .into_iter()
                .collect(),
            Some(10 | 12..=31) => child(paint, 1).into_iter().collect(),
            Some(32) => [child(paint, 1), child(paint, 5)]
                .into_iter()
                .flatten()
                .collect(),
            _ => Vec::new(),
        }
    };
    totals(
        bases.values().copied().collect::<Vec<_>>(),
        |_| 1,
        children,
        MAX_PAINT_VISITS,
        "a color glyph's painting",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_count_shared_children_per_use_and_refuse_rings() {
        // Node n has two children n + 1 up to 10; leaves weigh one.
        let doubling = |node: u32| {
            if node < 10 {
                vec![node + 1, node + 1]
            } else {
                Vec::new()
            }
        };
        let leaf = |node: u32| u64::from(node == 10);
        assert!(totals([0_u32], leaf, doubling, 1_024, "x").is_ok());
        assert!(totals([0_u32], leaf, doubling, 1_023, "x").is_err());
        let ring = |node: u32| vec![(node + 1) % 3];
        let error = totals([0_u32], |_| 1, ring, u64::MAX, "x").expect_err("a ring");
        assert!(error.contains("itself"), "{error}");
    }
}
