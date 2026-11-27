//! The highlight box: a run's line fragments, folded from run to run by their progress.

use std::ops::Range;

use super::{Layout, TextHighlightRequest, animated::ClusterInfo};

/// The highlight's boxes, `[left, right, top, bottom]` in layout pixels before padding: the
/// folded box, then the last step's other fragments. `extents` gives each cluster's advance where
/// it is drawn, `None` for one without glyphs.
pub(crate) fn boxes(
    layout: &Layout,
    clusters: &[ClusterInfo],
    extents: &[Option<(f32, f32)>],
    highlight: &TextHighlightRequest,
) -> Vec<[f32; 4]> {
    let mut steps = highlight.steps.iter().filter_map(|(bytes, progress)| {
        let fragments = fragments(layout, clusters, extents, bytes);
        (!fragments.is_empty()).then_some((fragments, *progress))
    });
    let Some((first, _)) = steps.next() else {
        return Vec::new();
    };
    let mut folded = first[0];
    let mut last = first;
    for (fragments, progress) in steps {
        let to = fragments[0];
        folded = std::array::from_fn(|side| folded[side] + (to[side] - folded[side]) * progress);
        last = fragments;
    }
    std::iter::once(folded)
        .chain(last.into_iter().skip(1))
        .collect()
}

/// The line fragments of the clusters whose first byte lies in `bytes`: per line, their advances
/// with white space at either end left out, by the em box of their largest size.
fn fragments(
    layout: &Layout,
    clusters: &[ClusterInfo],
    extents: &[Option<(f32, f32)>],
    bytes: &Range<usize>,
) -> Vec<[f32; 4]> {
    let mut fragments = Vec::new();
    let mut members = clusters
        .iter()
        .enumerate()
        .filter(|(_, cluster)| !cluster.ellipsis && bytes.contains(&cluster.bytes.start))
        .peekable();
    while let Some(&(_, first)) = members.peek() {
        let line = first.line;
        let on_line = std::iter::from_fn(|| members.next_if(|(_, cluster)| cluster.line == line))
            .collect::<Vec<_>>();
        let kept = on_line
            .iter()
            .position(|(_, cluster)| !cluster.is_space)
            .zip(on_line.iter().rposition(|(_, cluster)| !cluster.is_space))
            .map_or(&[][..], |(start, end)| &on_line[start..=end]);
        let mut fragment: Option<[f32; 4]> = None;
        for (index, cluster) in kept {
            let Some((left, right)) = extents[*index] else {
                continue;
            };
            let baseline = layout.baseline(line as usize);
            let (top, bottom) = (
                baseline - 0.8 * cluster.font_size,
                baseline + 0.2 * cluster.font_size,
            );
            fragment = Some(fragment.map_or([left, right, top, bottom], |held| {
                [
                    held[0].min(left),
                    held[1].max(right),
                    held[2].min(top),
                    held[3].max(bottom),
                ]
            }));
        }
        fragments.extend(fragment);
    }
    fragments
}
