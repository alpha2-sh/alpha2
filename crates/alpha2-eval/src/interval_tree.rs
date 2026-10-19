use crate::IndexedClip;
use alpha2_time::{MediaTime, TimeRange};

#[derive(Clone, Debug)]
pub(crate) struct IntervalNode {
    entry: IndexedClip,
    max_end: MediaTime,
    left: Option<usize>,
    right: Option<usize>,
}

/// The balanced interval tree over `entries`, sorted by start, and its root:
/// each entry moves into its node.
pub(crate) fn build_tree(entries: Vec<IndexedClip>) -> (Vec<IntervalNode>, Option<usize>) {
    let mut entries = entries.into_iter().map(Some).collect::<Vec<_>>();
    let mut nodes = Vec::with_capacity(entries.len());
    let root = build_subtree(&mut entries, &mut nodes);
    (nodes, root)
}

fn build_subtree(
    entries: &mut [Option<IndexedClip>],
    nodes: &mut Vec<IntervalNode>,
) -> Option<usize> {
    if entries.is_empty() {
        return None;
    }
    let middle = entries.len() / 2;
    let index = nodes.len();
    let entry = entries[middle]
        .take()
        .expect("each entry is taken once, by its own node");
    nodes.push(IntervalNode {
        max_end: entry.timeline_range.end(),
        entry,
        left: None,
        right: None,
    });
    let (left, right) = entries.split_at_mut(middle);
    let left = build_subtree(left, nodes);
    let right = build_subtree(&mut right[1..], nodes);
    let max_end = [
        Some(nodes[index].max_end),
        left.map(|child| nodes[child].max_end),
        right.map(|child| nodes[child].max_end),
    ]
    .into_iter()
    .flatten()
    .max()
    .expect("the node itself always has an end");
    nodes[index].left = left;
    nodes[index].right = right;
    nodes[index].max_end = max_end;
    Some(index)
}

/// Pushes every clip active at `time` (its half-open interval contains it) in
/// start order.
pub(crate) fn collect_active<'a>(
    nodes: &'a [IntervalNode],
    index: usize,
    time: MediaTime,
    output: &mut Vec<&'a IndexedClip>,
) {
    let node = &nodes[index];
    if let Some(left) = node.left.filter(|&child| nodes[child].max_end > time) {
        collect_active(nodes, left, time, output);
    }
    if node.entry.timeline_range.contains(time) {
        output.push(&node.entry);
    }
    if node.entry.timeline_range.start() <= time
        && let Some(right) = node.right.filter(|&child| nodes[child].max_end > time)
    {
        collect_active(nodes, right, time, output);
    }
}

/// Visits every clip whose timeline interval overlaps a half-open range, with
/// the position of its node.
pub(crate) fn collect_overlapping<'a>(
    nodes: &'a [IntervalNode],
    index: usize,
    range: TimeRange,
    visit: &mut impl FnMut(usize, &'a IndexedClip),
) {
    let node = &nodes[index];
    if let Some(left) = node
        .left
        .filter(|&child| nodes[child].max_end > range.start())
    {
        collect_overlapping(nodes, left, range, visit);
    }
    if node.entry.timeline_range.start() < range.end()
        && node.entry.timeline_range.end() > range.start()
    {
        visit(index, &node.entry);
    }
    if node.entry.timeline_range.start() < range.end()
        && let Some(right) = node
            .right
            .filter(|&child| nodes[child].max_end > range.start())
    {
        collect_overlapping(nodes, right, range, visit);
    }
}

/// Counts the clips overlapping a half-open range that `keep` accepts.
pub(crate) fn count_overlapping(
    nodes: &[IntervalNode],
    index: usize,
    range: TimeRange,
    keep: &impl Fn(&IndexedClip) -> bool,
) -> usize {
    let mut count = 0;
    collect_overlapping(nodes, index, range, &mut |_, entry| {
        count += usize::from(keep(entry));
    });
    count
}

impl IntervalNode {
    /// The clip this node indexes.
    pub(crate) const fn entry(&self) -> &IndexedClip {
        &self.entry
    }
}
