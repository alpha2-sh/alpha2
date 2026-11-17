//! Checks made before an operation edits, so it skips a cut or refuses with a reason instead of
//! building a document that fails validation.

use super::{SkipReason, split};
use alpha2_model::{
    Clip, ClipId, ClipReference, ClipTransition, Project, TimeMap, TimelineOwner, Track,
};
use alpha2_time::TimeRange;
use std::collections::{BTreeSet, HashMap, HashSet};

/// Whether `time` lies strictly inside a transition of `track` that names `clip`.
pub(super) fn inside_transition(track: &Track, clip: &ClipId, time: i64) -> bool {
    track.transitions.iter().any(|transition| {
        (&transition.from_clip == clip || &transition.to_clip == clip)
            && transition.timeline_range.start().ticks() < time
            && time < transition.timeline_range.end().ticks()
    })
}

/// Whether `time` lies strictly inside the fade in or fade out of `clip`.
pub(super) fn inside_fade(clip: &Clip, time: i64) -> bool {
    let (start, end) = (
        clip.timeline_range.start().ticks(),
        clip.timeline_range.end().ticks(),
    );
    let (fade_in, fade_out) = (clip.audio.fade_in.ticks(), clip.audio.fade_out.ticks());
    (start < time && time < start.saturating_add(fade_in))
        || (end.saturating_sub(fade_out) < time && time < end)
}

/// The first boundary between the pieces a loop clip cut to `windows` is made of that lies
/// strictly inside one of its transitions on `track` that `kept` keeps, as `(window, boundary)`.
pub(super) fn loop_boundary_in_transition(
    clip: &Clip,
    track: &Track,
    windows: &[(i64, i64)],
    kept: impl Fn(&ClipTransition) -> bool,
) -> Option<(usize, i64)> {
    if !matches!(clip.time_map, TimeMap::Loop { .. }) {
        return None;
    }
    let transitions = track
        .transitions
        .iter()
        .filter(|transition| {
            (transition.from_clip == clip.id || transition.to_clip == clip.id) && kept(transition)
        })
        .collect::<Vec<_>>();
    if transitions.is_empty() {
        return None;
    }
    for (index, window) in windows.iter().enumerate() {
        for piece in split::pieces(clip, *window).iter().skip(1) {
            let boundary = piece.timeline_range.start().ticks();
            if transitions.iter().any(|transition| {
                transition.timeline_range.start().ticks() < boundary
                    && boundary < transition.timeline_range.end().ticks()
            }) {
                return Some((index, boundary));
            }
        }
    }
    None
}

/// Where a reference to a clip is made from, by the old timeline time it covers.
enum Span {
    /// A clip, cut as its own cuts say.
    Clip(ClipId, TimeRange),
    /// A transition's range.
    Range(TimeRange),
    /// A bus, which covers every time.
    Everything,
}

/// A matte or a `via` binding naming `target`, other than a binding through its own clip.
struct Reference {
    target: ClipId,
    matte: bool,
    span: Span,
}

/// Every matte and `via` reference of `project`. Walks a copy the caller is about to edit.
fn references(project: &mut Project) -> Vec<Reference> {
    let mut found = Vec::new();
    project.for_each_clip_reference_mut(&mut |owner, kind, clip| {
        let matte = match kind {
            ClipReference::Matte => true,
            ClipReference::Via => false,
            ClipReference::TransitionFrom | ClipReference::TransitionTo => return,
        };
        let span = match owner {
            TimelineOwner::Clip { id, .. } if !matte && id == clip => return,
            TimelineOwner::Clip { id, range, .. } => Span::Clip(id.clone(), range),
            TimelineOwner::Transition { range, .. } => Span::Range(range),
            TimelineOwner::Track { .. } | TimelineOwner::Master => Span::Everything,
        };
        found.push(Reference {
            target: clip.clone(),
            matte,
            span,
        });
    });
    found
}

/// The windows of `range` between `cuts`.
fn windows(range: TimeRange, cuts: &BTreeSet<i64>) -> Vec<(i64, i64)> {
    let mut windows = Vec::with_capacity(cuts.len() + 1);
    let mut cursor = range.start().ticks();
    for cut in cuts {
        windows.push((cursor, *cut));
        cursor = *cut;
    }
    windows.push((cursor, range.end().ticks()));
    windows
}

const fn overlaps(a: (i64, i64), b: (i64, i64)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// Cuts of `planned` that leave a matte or a `via` binding with no single part to name, or that
/// leave a matte source part unnamed, into `skip`.
fn reference_skips(
    planned: &[Planned<'_>],
    references: &[Reference],
    position: &HashMap<&ClipId, usize>,
    skip: &mut Vec<(usize, i64, SkipReason)>,
) {
    for (index, entry) in planned.iter().enumerate() {
        if entry.cuts.is_empty() {
            continue;
        }
        let parts = windows(entry.clip.timeline_range, &entry.cuts);
        let mut named = HashSet::new();
        let mut matted = false;
        for reference in references
            .iter()
            .filter(|reference| reference.target == entry.clip.id)
        {
            let reason = if reference.matte {
                matted = true;
                SkipReason::MatteSource
            } else {
                SkipReason::ReadThrough
            };
            let spans = match &reference.span {
                Span::Clip(id, range) => position.get(id).map_or_else(
                    || vec![(range.start().ticks(), range.end().ticks())],
                    |owner| windows(*range, &planned[*owner].cuts),
                ),
                Span::Range(range) => vec![(range.start().ticks(), range.end().ticks())],
                Span::Everything => vec![(i64::MIN, i64::MAX)],
            };
            for span in spans {
                let over = parts
                    .iter()
                    .enumerate()
                    .filter(|(_, part)| overlaps(**part, span))
                    .map(|(part, _)| part)
                    .collect::<Vec<_>>();
                match over.as_slice() {
                    [one] => {
                        named.insert(*one);
                    }
                    [] if reference.matte => {
                        named.insert(0);
                    }
                    [] => skip.extend(entry.cuts.iter().map(|cut| (index, *cut, reason))),
                    _ => skip.extend(
                        entry
                            .cuts
                            .iter()
                            .filter(|cut| span.0 < **cut && **cut < span.1)
                            .map(|cut| (index, *cut, reason)),
                    ),
                }
            }
        }
        // A matte source part no matted clip names would be drawn.
        if matted {
            for (part, window) in parts.iter().enumerate() {
                if !named.contains(&part) {
                    let cut = if part == 0 { window.1 } else { window.0 };
                    skip.push((index, cut, SkipReason::MatteSource));
                }
            }
        }
    }
}

/// One target clip of a `split_at` and the cuts it keeps.
pub(super) struct Planned<'a> {
    /// Its track's position.
    pub track: usize,
    /// The clip.
    pub clip: &'a Clip,
    /// Cuts strictly inside it, in timeline time.
    pub cuts: BTreeSet<i64>,
}

/// Leaves out every cut of `planned` that would change what a clip shows: inside one of its
/// transitions or fades, making a loop piece boundary inside a transition, or cutting a matte
/// source or a clip read through where its owner is not cut alike. Returns the cuts left out.
pub(super) fn skip_cuts(
    project: &Project,
    edited: &mut Project,
    planned: &mut [Planned<'_>],
) -> Vec<(usize, i64, SkipReason)> {
    let mut skipped = Vec::new();
    for (index, entry) in planned.iter_mut().enumerate() {
        let track = &project.timeline.tracks[entry.track];
        entry.cuts.retain(|cut| {
            let reason = if inside_transition(track, &entry.clip.id, *cut) {
                SkipReason::InTransition
            } else if inside_fade(entry.clip, *cut) {
                SkipReason::InFade
            } else {
                return true;
            };
            skipped.push((index, *cut, reason));
            false
        });
    }
    let references = references(edited);
    let position = planned
        .iter()
        .enumerate()
        .map(|(index, entry)| (&entry.clip.id, index))
        .collect::<HashMap<_, _>>();
    loop {
        let mut skip: Vec<(usize, i64, SkipReason)> = Vec::new();
        for (index, entry) in planned.iter().enumerate() {
            let track = &project.timeline.tracks[entry.track];
            let windows = windows(entry.clip.timeline_range, &entry.cuts);
            if let Some((window, _)) =
                loop_boundary_in_transition(entry.clip, track, &windows, |_| true)
            {
                skip.push((index, windows[window].0, SkipReason::InTransition));
            }
        }
        reference_skips(planned, &references, &position, &mut skip);
        let mut changed = false;
        for (index, cut, reason) in skip {
            if planned[index].cuts.remove(&cut) {
                skipped.push((index, cut, reason));
                changed = true;
            }
        }
        if !changed {
            return skipped;
        }
    }
}
