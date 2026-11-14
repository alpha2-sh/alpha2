//! The edit operations `remove_ranges`, `split_at` and `bake`, which apply data to a project once.
//!
//! Operations are named for primitives, never for features, and are pure: a project and a
//! request in, a project and a report out, with no I/O. Clips are split so each part continues its
//! source and time map, keyframes move with their clip or transition, timeline-anchored data and
//! caption cues are cut and shifted, transitions whose cut point is removed are removed, and mattes
//! and `via` bindings follow the part beside them or the edit is refused. Every value keyed in
//! timeline time is reached through the model's walkers, and bus curves keep their shape across a
//! cut. Before editing, `split_at` leaves out cuts it cannot make exactly and `remove_ranges`
//! refuses them, so a valid request never ends as an invalid result. Kept moments render as before
//! except for reversed parts off the frame grid and the sound at a loop wrap a cut turns into a
//! part boundary. The result validates and is canonical.

mod bake;
#[cfg(test)]
mod bake_tests;
#[cfg(test)]
mod clip_reference_tests;
mod json;
mod plan;
#[cfg(test)]
mod plan_tests;
mod request;
mod split;
#[cfg(test)]
mod surface_corpus_tests;
#[cfg(test)]
mod tests;

pub use json::{EditFailure, EditedJson, apply_json};
pub use request::{
    BakeTarget, Created, DataRef, EditError, EditReport, EditRequest, Moved, RangeList, Ranges,
    RawRange, Ripple, SkipReason, Skipped, Target, TimeList, Times,
};

use alpha2_model::{
    Animated, Clip, ClipId, ClipReference, DataAnchor, DataValues, Project, TimelineOwner, Track,
};
use alpha2_time::{FrameRate, MediaTime, TimeRange};
use std::collections::{BTreeSet, HashMap, HashSet};

/// Applies `request` to `project`.
///
/// # Errors
/// [`EditError`] for a request that names something the project lacks or does not fit the
/// operation, and [`EditError::InvalidResult`] if the result fails validation.
pub fn apply(project: &Project, request: &EditRequest) -> Result<(Project, EditReport), EditError> {
    let (edited, report) = match request {
        EditRequest::RemoveRanges {
            ranges,
            target,
            ripple,
            invert,
            padding,
            min_duration,
        } => remove_ranges(
            project,
            ranges,
            target,
            *ripple,
            *invert,
            *padding,
            *min_duration,
        )?,
        EditRequest::SplitAt { times, target } => split_at(project, times, target)?,
        EditRequest::Bake { targets } => bake::bake(project, targets)?,
    };
    check_counts(&edited)?;
    edited.validate().map_err(EditError::InvalidResult)?;
    Ok((edited, report))
}

/// Refuses a result with more clips or caption cues on a track than a track holds.
fn check_counts(edited: &Project) -> Result<(), EditError> {
    let maximum = alpha2_model::MAX_CLIPS_PER_TRACK;
    for track in &edited.timeline.tracks {
        if track.clips.len() > maximum {
            return Err(EditError::TooManyClips {
                track: track.id.clone(),
                found: track.clips.len(),
                maximum,
            });
        }
        if let Some(captions) = &track.captions
            && captions.cues.len() > maximum
        {
            return Err(EditError::TooManyCues {
                track: track.id.clone(),
                found: captions.cues.len(),
                maximum,
            });
        }
    }
    Ok(())
}

/// The target clips as `(track position, clip)`, in document order.
fn target_clips<'a>(
    project: &'a Project,
    target: &Target,
) -> Result<Vec<(usize, &'a Clip)>, EditError> {
    let clips = match target {
        Target::Track(id) => {
            let (position, track) = project
                .timeline
                .tracks
                .iter()
                .enumerate()
                .find(|(_, track)| &track.id == id)
                .ok_or_else(|| EditError::UnknownTarget(format!("track {id}")))?;
            track
                .clips
                .iter()
                .map(|clip| (position, clip))
                .collect::<Vec<_>>()
        }
        Target::Clips(ids) => {
            let mut found = Vec::with_capacity(ids.len());
            for id in ids {
                let entry = project
                    .timeline
                    .tracks
                    .iter()
                    .enumerate()
                    .find_map(|(position, track)| {
                        track
                            .clips
                            .iter()
                            .find(|clip| &clip.id == id)
                            .map(|clip| (position, clip))
                    })
                    .ok_or_else(|| EditError::UnknownTarget(format!("clip {id}")))?;
                found.push(entry);
            }
            found
        }
    };
    if clips.is_empty() {
        return Err(EditError::EmptyTarget);
    }
    Ok(clips)
}

/// Intervals on an anchor, from a data set or a list.
fn read_ranges(
    project: &Project,
    ranges: &Ranges,
) -> Result<(DataAnchor, Vec<(MediaTime, MediaTime)>), EditError> {
    match ranges {
        Ranges::Data(DataRef { data }) => {
            let set = project
                .data
                .iter()
                .find(|set| &set.id == data)
                .ok_or_else(|| EditError::UnknownData(data.clone()))?;
            let DataValues::Intervals(intervals) = &set.values else {
                return Err(EditError::WrongKind {
                    data: data.clone(),
                    kind: set.kind(),
                });
            };
            Ok((
                set.anchor.clone(),
                intervals
                    .iter()
                    .map(|item| (item.start, item.end))
                    .collect(),
            ))
        }
        Ranges::List(RangeList { list, anchor }) => {
            let anchor = anchor.clone().ok_or(EditError::MissingAnchor)?;
            for (index, range) in list.iter().enumerate() {
                if range.start > range.end {
                    return Err(EditError::InvalidRange(index));
                }
            }
            Ok((
                anchor,
                list.iter().map(|range| (range.start, range.end)).collect(),
            ))
        }
    }
}

/// Sorted, merged, non-empty timeline ranges.
fn union(mut ranges: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    ranges.retain(|(start, end)| start < end);
    ranges.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// The nearest project frame start to `tick`, ties to the earlier one.
fn snap(tick: i64, rate: FrameRate) -> i64 {
    let Some(frame) = rate.frame_at_or_before(MediaTime(tick)) else {
        return tick;
    };
    let before = rate.frame_start(frame).map_or(tick, MediaTime::ticks);
    let after = rate.frame_start(frame + 1).map_or(tick, MediaTime::ticks);
    if after - tick < tick - before {
        after
    } else {
        before
    }
}

/// The timeline ranges `intervals` on `anchor` cover through the target clips.
fn timeline_ranges(
    project: &Project,
    anchor: &DataAnchor,
    intervals: &[(MediaTime, MediaTime)],
    targets: &[(usize, &Clip)],
) -> Vec<(i64, i64)> {
    let mut ranges = Vec::new();
    match anchor {
        DataAnchor::Timeline => ranges.extend(
            intervals
                .iter()
                .map(|(start, end)| (start.ticks().max(0), end.ticks())),
        ),
        DataAnchor::Source { source_id } => {
            for (_, clip) in targets
                .iter()
                .filter(|(_, clip)| &clip.source_id == source_id)
            {
                for (start, end) in intervals {
                    let mapped = crate::source_range_to_timeline(
                        clip.timeline_range,
                        clip.source_range,
                        clip.time_map,
                        project.settings.frame_rate,
                        *start,
                        *end,
                    )
                    .unwrap_or_default();
                    ranges.extend(
                        mapped
                            .into_iter()
                            .map(|range| (range.start().ticks(), range.end().ticks())),
                    );
                }
            }
        }
    }
    union(ranges)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn remove_ranges(
    project: &Project,
    ranges: &Ranges,
    target: &Target,
    ripple: Ripple,
    invert: bool,
    padding: MediaTime,
    min_duration: MediaTime,
) -> Result<(Project, EditReport), EditError> {
    let targets = target_clips(project, target)?;
    let (anchor, intervals) = read_ranges(project, ranges)?;
    let rate = project.settings.frame_rate;
    let mut removal = timeline_ranges(project, &anchor, &intervals, &targets);
    if invert {
        let start = targets
            .iter()
            .map(|(_, clip)| clip.timeline_range.start().ticks())
            .min()
            .unwrap_or(0);
        let end = targets
            .iter()
            .map(|(_, clip)| clip.timeline_range.end().ticks())
            .max()
            .unwrap_or(0);
        let mut complement = Vec::new();
        let mut cursor = start;
        for (from, to) in &removal {
            if *from > cursor {
                complement.push((cursor, (*from).min(end)));
            }
            cursor = cursor.max(*to);
        }
        if cursor < end {
            complement.push((cursor, end));
        }
        removal = complement;
    }
    // Padding keeps material at each edge (a negative one widens, not before 0); cuts then land on
    // the project's frame grid.
    let removal = union(
        removal
            .into_iter()
            .map(|(start, end)| ((start + padding.ticks()).max(0), end - padding.ticks()))
            .filter(|(start, end)| end - start >= min_duration.ticks().max(1))
            .map(|(start, end)| (snap(start, rate), snap(end, rate)))
            .collect(),
    );
    let mut edited = project.clone();
    let mut report = EditReport {
        removed: removal
            .iter()
            .filter_map(|(start, end)| TimeRange::new(MediaTime(*start), MediaTime(*end)).ok())
            .collect(),
        ..EditReport::default()
    };
    if removal.is_empty() {
        return Ok((edited, report));
    }
    let shifted_tracks: HashSet<usize> = match ripple {
        Ripple::None => HashSet::new(),
        Ripple::Track => targets.iter().map(|(position, _)| *position).collect(),
        Ripple::Timeline => (0..project.timeline.tracks.len()).collect(),
    };
    let cut_clips: HashSet<&ClipId> = match ripple {
        Ripple::None => targets.iter().map(|(_, clip)| &clip.id).collect(),
        Ripple::Track | Ripple::Timeline => HashSet::new(),
    };
    let mut ids = split::IdPool::new(project);
    let mut renamed = Parts::new();
    let crossed = crossed_transitions(project, &removal, &shifted_tracks, &cut_clips);
    let original_tracks = &project.timeline.tracks;
    for (position, track) in edited.timeline.tracks.iter_mut().enumerate() {
        let shifts = shifted_tracks.contains(&position);
        let mut clips = Vec::with_capacity(track.clips.len());
        for clip in &track.clips {
            // A clip named by a crossed transition loses its overlap even when not targeted.
            if !shifts && !cut_clips.contains(&clip.id) && !crossed.contains_key(&clip.id) {
                clips.push(clip.clone());
                continue;
            }
            let mut own = removal.clone();
            own.extend(crossed.get(&clip.id).into_iter().flatten().copied());
            let keep = kept_windows(clip.timeline_range, &union(own));
            refuse_unsafe_cut(clip, &original_tracks[position], &keep, &removal)?;
            let parts = split::parts(clip, &keep, &mut ids, &mut report.created);
            let mut placed = Vec::with_capacity(parts.len());
            for part in parts {
                let delta = if shifts {
                    shift_of(part.timeline_range.start().ticks(), &removal)
                } else {
                    0
                };
                placed.push((part.timeline_range, split::moved(part, delta)));
            }
            renamed.insert(
                clip.id.clone(),
                placed
                    .iter()
                    .map(|(old, part)| (*old, part.id.clone()))
                    .collect(),
            );
            clips.extend(placed.into_iter().map(|(_, part)| part));
        }
        track.clips = clips;
    }
    let transition_spans =
        retarget_transitions(&mut edited, project, &removal, &shifted_tracks, &renamed);
    // Bus keys of every shifted track, and the master's when every track shifts.
    edited.for_each_bus_value_mut(&mut |owner, _, value| {
        let shifts = match owner {
            TimelineOwner::Track { track, .. } => shifted_tracks.contains(&track),
            _ => ripple == Ripple::Timeline,
        };
        if shifts {
            shift_keys(value, &removal);
        }
    });
    for (position, track) in edited.timeline.tracks.iter_mut().enumerate() {
        if let Some(captions) = &mut track.captions
            && shifted_tracks.contains(&position)
        {
            captions.cues = shift_cues(&captions.cues, &removal);
        }
    }
    if ripple == Ripple::Timeline {
        for set in &mut edited.data {
            if set.anchor == DataAnchor::Timeline {
                split::cut_data(set, &removal);
            }
        }
    }
    retarget_clip_references(&mut edited, project, &renamed, &transition_spans)?;
    report.moved = moved_stretches(project, &removal, &shifted_tracks);
    Ok((edited, report))
}

/// Refuses a removal that cuts `clip` (into the windows `keep`) inside one of its fades, or that
/// leaves a loop piece boundary inside one of its transitions no removed range crosses.
fn refuse_unsafe_cut(
    clip: &Clip,
    track: &Track,
    keep: &[(i64, i64)],
    removal: &[(i64, i64)],
) -> Result<(), EditError> {
    let (start, end) = (
        clip.timeline_range.start().ticks(),
        clip.timeline_range.end().ticks(),
    );
    if keep == [(start, end)] {
        return Ok(());
    }
    let edge = keep
        .iter()
        .flat_map(|(from, to)| [*from, *to])
        .find(|edge| plan::inside_fade(clip, *edge));
    if let Some(edge) = edge {
        return Err(EditError::InFade {
            clip: clip.id.clone(),
            time: MediaTime(edge),
        });
    }
    let kept = |transition: &alpha2_model::ClipTransition| {
        let range = transition.timeline_range;
        !removal
            .iter()
            .any(|(from, to)| *from < range.end().ticks() && range.start().ticks() < *to)
    };
    match plan::loop_boundary_in_transition(clip, track, keep, kept) {
        Some((_, boundary)) => Err(EditError::InTransition {
            clip: clip.id.clone(),
            time: MediaTime(boundary),
        }),
        None => Ok(()),
    }
}

/// For each clip of a transition a removed range crosses, what it also loses so the two no longer
/// overlap. The from-clip loses what follows the first cut inside the transition, and the to-clip
/// what precedes the last.
fn crossed_transitions(
    project: &Project,
    removal: &[(i64, i64)],
    shifted: &HashSet<usize>,
    cut: &HashSet<&ClipId>,
) -> HashMap<ClipId, Vec<(i64, i64)>> {
    let mut extra: HashMap<ClipId, Vec<(i64, i64)>> = HashMap::new();
    for (position, track) in project.timeline.tracks.iter().enumerate() {
        for transition in &track.transitions {
            if !shifted.contains(&position)
                && !cut.contains(&transition.from_clip)
                && !cut.contains(&transition.to_clip)
            {
                continue;
            }
            let (start, end) = (
                transition.timeline_range.start().ticks(),
                transition.timeline_range.end().ticks(),
            );
            let inside = removal
                .iter()
                .filter(|(from, to)| *from < end && start < *to)
                .collect::<Vec<_>>();
            let (Some(first), Some(last)) = (inside.first(), inside.last()) else {
                continue;
            };
            extra
                .entry(transition.from_clip.clone())
                .or_default()
                .push((first.0.max(start), end));
            extra
                .entry(transition.to_clip.clone())
                .or_default()
                .push((start, last.1.min(end)));
        }
    }
    extra
}

/// The windows of `range` outside every removed range.
fn kept_windows(range: TimeRange, removal: &[(i64, i64)]) -> Vec<(i64, i64)> {
    kept_windows_ticks(range.start().ticks(), range.end().ticks(), removal)
}

/// The windows of `start..end` outside every removed range.
fn kept_windows_ticks(start: i64, end: i64, removal: &[(i64, i64)]) -> Vec<(i64, i64)> {
    let mut windows = Vec::new();
    let mut cursor = start;
    for (from, to) in removal {
        if *to <= cursor || *from >= end {
            continue;
        }
        if *from > cursor {
            windows.push((cursor, *from));
        }
        cursor = cursor.max(*to);
    }
    if cursor < end {
        windows.push((cursor, end));
    }
    windows
}

/// How far a time at or after `tick` moves left, which is the removed length before it.
fn shift_of(tick: i64, removal: &[(i64, i64)]) -> i64 {
    removal
        .iter()
        .map(|(from, to)| {
            if *to <= tick {
                to - from
            } else if *from < tick {
                tick - from
            } else {
                0
            }
        })
        .sum()
}

/// A kept time's new position, or `None` for a removed one.
fn new_time(tick: i64, removal: &[(i64, i64)]) -> Option<i64> {
    if removal
        .iter()
        .any(|(from, to)| (*from..*to).contains(&tick))
    {
        return None;
    }
    Some(tick - shift_of(tick, removal))
}

/// Cues trimmed where they cross a removed range, dropped when wholly inside one, and shifted;
/// run times stay with the text they time.
fn shift_cues(
    cues: &[alpha2_model::CaptionCue],
    removal: &[(i64, i64)],
) -> Vec<alpha2_model::CaptionCue> {
    let mut out = Vec::new();
    for cue in cues {
        let windows = kept_windows(cue.timeline_range, removal);
        let (Some(first), Some(last)) = (windows.first(), windows.last()) else {
            continue;
        };
        let start = first.0 - shift_of(first.0, removal);
        let end = last.1 - 1 - shift_of(last.1 - 1, removal) + 1;
        let Ok(range) = TimeRange::new(MediaTime(start), MediaTime(end.max(start))) else {
            continue;
        };
        if range.is_empty() {
            continue;
        }
        let mut cue = cue.clone();
        let old_start = cue.timeline_range.start().ticks();
        cue.timeline_range = range;
        if let alpha2_model::TextContent::Runs(runs) = &mut cue.text {
            for run in runs.iter_mut() {
                run.time = run.time.and_then(|time| {
                    let from = new_time(old_start + time.start.ticks(), removal).or_else(|| {
                        windows
                            .iter()
                            .find(|(lo, _)| *lo > old_start + time.start.ticks())
                            .map(|(lo, _)| lo - shift_of(*lo, removal))
                    })?;
                    let to_old = old_start + time.end.ticks();
                    let to = new_time(to_old - 1, removal)
                        .map(|tick| tick + 1)
                        .or_else(|| {
                            windows
                                .iter()
                                .rev()
                                .find(|(_, hi)| *hi < to_old)
                                .map(|(_, hi)| hi - shift_of(hi - 1, removal))
                        })?;
                    let (from, to) = (from - start, to - start);
                    (from < to).then_some(alpha2_model::RunTime {
                        start: MediaTime(from),
                        end: MediaTime(to),
                    })
                });
            }
        }
        alpha2_model::clip_runs(&mut cue);
        out.push(cue);
    }
    out
}

/// Keyframes of a bus curve on a rippled timeline. Keys inside removed ranges go and the rest
/// move; the value at each cut is keyed on both sides, a one-tick hold step across the cut. Every
/// key takes the part of the old curve up to the next key, so kept moments sample as before.
fn shift_keys(animation: &mut Animated<f32>, removal: &[(i64, i64)]) {
    let Animated::Keyframes { keyframes } = animation else {
        return;
    };
    let old = std::mem::take(keyframes);
    // Each new key at its old time, with the old key it keeps (none for one a cut adds).
    let mut at = old
        .iter()
        .enumerate()
        .filter(|(_, key)| new_time(key.time.ticks(), removal).is_some())
        .map(|(index, key)| (key.time.ticks(), Some(index)))
        .collect::<Vec<_>>();
    for (from, to) in removal {
        if from - 1 - shift_of(from - 1, removal) >= 0 {
            at.push((from - 1, None));
        }
        at.push((*to, None));
    }
    at.sort_by_key(|(tick, kept)| (*tick, kept.is_none()));
    at.dedup_by_key(|(tick, _)| *tick);
    let curve = Animated::Keyframes { keyframes: old };
    let Animated::Keyframes { keyframes: old } = &curve else {
        return;
    };
    let value =
        |tick: i64| crate::sample_scalar(&curve, MediaTime(tick), &crate::SampleContext::NONE);
    let mut keys = Vec::with_capacity(at.len());
    for (position, (tick, kept)) in at.iter().enumerate() {
        let next = at.get(position + 1).map(|(tick, _)| *tick);
        let interpolation = match next {
            Some(next)
                if removal
                    .iter()
                    .any(|(from, to)| *tick < *from && *to <= next) =>
            {
                alpha2_model::Interpolation::Hold
            }
            Some(next) => part_of_curve(old, *tick, next).unwrap_or_else(|| {
                kept.map_or(alpha2_model::Interpolation::Linear, |index| {
                    old[index].interpolation
                })
            }),
            None => kept.map_or(alpha2_model::Interpolation::Linear, |index| {
                old[index].interpolation
            }),
        };
        keys.push(alpha2_model::Keyframe {
            time: MediaTime(tick - shift_of(*tick, removal)),
            value: kept.map_or_else(|| value(*tick), |index| old[index].value),
            interpolation,
        });
    }
    *keyframes = keys;
}

/// The interpolation that shows the old curve from `from` to `to`, which lie in one segment, or
/// `None` outside every segment, where the curve is constant.
fn part_of_curve(
    keys: &[alpha2_model::Keyframe<f32>],
    from: i64,
    to: i64,
) -> Option<alpha2_model::Interpolation> {
    let next = keys.partition_point(|key| key.time.ticks() <= from);
    let (start, end) = (keys.get(next.checked_sub(1)?)?, keys.get(next)?);
    let alpha2_model::Interpolation::CubicBezier(curve) = start.interpolation else {
        return Some(start.interpolation);
    };
    let progress =
        |tick: i64| crate::animation::interpolation_amount(start.time, end.time, MediaTime(tick));
    if from == start.time.ticks() && to == end.time.ticks() {
        return Some(start.interpolation);
    }
    // A part whose ends are equal shows a constant; the nearest exact form is a line.
    Some(curve.between(progress(from), progress(to)).map_or(
        alpha2_model::Interpolation::Linear,
        alpha2_model::Interpolation::CubicBezier,
    ))
}

/// The parts each processed clip became, as `(old range, new id)`. A clip removed whole has none.
type Parts = HashMap<ClipId, Vec<(TimeRange, ClipId)>>;

/// Transitions whose cut point is removed go; the rest name the parts that now hold them and
/// move with their track. Returns each kept transition's old range, per track.
fn retarget_transitions(
    edited: &mut Project,
    original: &Project,
    removal: &[(i64, i64)],
    shifted: &HashSet<usize>,
    renamed: &Parts,
) -> Vec<Vec<TimeRange>> {
    let mut spans = Vec::with_capacity(edited.timeline.tracks.len());
    for (position, track) in edited.timeline.tracks.iter_mut().enumerate() {
        let before = &original.timeline.tracks[position].transitions;
        let mut kept = Vec::with_capacity(before.len());
        let mut old = Vec::with_capacity(before.len());
        for transition in before {
            let range = transition.timeline_range;
            let crosses = removal
                .iter()
                .any(|(from, to)| *from < range.end().ticks() && range.start().ticks() < *to);
            if crosses
                && (shifted.contains(&position)
                    || renamed.contains_key(&transition.from_clip)
                    || renamed.contains_key(&transition.to_clip))
            {
                continue;
            }
            let owner = |clip: &ClipId| {
                renamed.get(clip).map_or_else(
                    || Some(clip.clone()),
                    |parts| {
                        parts
                            .iter()
                            .find(|(old, _)| {
                                old.start() <= range.start() && range.end() <= old.end()
                            })
                            .map(|(_, id)| id.clone())
                    },
                )
            };
            let (Some(from), Some(to)) = (owner(&transition.from_clip), owner(&transition.to_clip))
            else {
                continue;
            };
            let delta = if shifted.contains(&position) {
                shift_of(range.start().ticks(), removal)
            } else {
                0
            };
            let mut transition = transition.clone();
            transition.from_clip = from;
            transition.to_clip = to;
            transition.for_each_timeline_value_mut(&mut |_, value| value.shift(-delta));
            transition.timeline_range = TimeRange::new(
                MediaTime(range.start().ticks() - delta),
                MediaTime(range.end().ticks() - delta),
            )
            .unwrap_or(range);
            kept.push(transition);
            old.push(range);
        }
        track.transitions = kept;
        spans.push(old);
    }
    spans
}

/// The part of `clip` a reference over the old `span` (`None`: the whole timeline) names, or
/// `None`. One part must overlap the span; a matte over no part stays inactive on the first part.
fn part_for(
    clip: &ClipId,
    span: Option<TimeRange>,
    matte: bool,
    renamed: &Parts,
    old_ranges: &HashMap<&ClipId, TimeRange>,
) -> Option<ClipId> {
    let Some(parts) = renamed.get(clip) else {
        return Some(clip.clone());
    };
    if let [(range, id)] = parts.as_slice()
        && old_ranges.get(clip) == Some(range)
    {
        return Some(id.clone());
    }
    let mut over = parts.iter().filter(|(range, _)| {
        span.is_none_or(|span| range.start() < span.end() && span.start() < range.end())
    });
    match (over.next(), over.next()) {
        (Some((_, id)), None) => Some(id.clone()),
        (None, _) if matte => parts.first().map(|(_, id)| id.clone()),
        _ => None,
    }
}

/// A [`TimelineOwner`] that outlives the walk, for naming it in a refusal.
enum Owner {
    Clip(ClipId),
    Transition(usize, usize),
    Track(usize),
    Master,
}

impl Owner {
    fn of(owner: TimelineOwner<'_>) -> Self {
        match owner {
            TimelineOwner::Clip { id, .. } => Self::Clip(id.clone()),
            TimelineOwner::Transition { track, index, .. } => Self::Transition(track, index),
            TimelineOwner::Track { track, .. } => Self::Track(track),
            TimelineOwner::Master => Self::Master,
        }
    }

    fn describe(&self, project: &Project) -> String {
        match self {
            Self::Clip(id) => format!("clip {id}"),
            Self::Transition(track, index) => {
                let transition = &project.timeline.tracks[*track].transitions[*index];
                format!(
                    "transition {} to {}",
                    transition.from_clip, transition.to_clip
                )
            }
            Self::Track(track) => format!("track {}", project.timeline.tracks[*track].id),
            Self::Master => "the master bus".to_owned(),
        }
    }
}

/// Points mattes and `via` bindings at the part of a cut clip beside them, and refuses where no
/// single part fits or where a matte part would be left unnamed, which would draw it.
fn retarget_clip_references(
    edited: &mut Project,
    original: &Project,
    renamed: &Parts,
    transition_spans: &[Vec<TimeRange>],
) -> Result<(), EditError> {
    let original_clips = || {
        original
            .timeline
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
    };
    let old_ranges = original_clips()
        .map(|clip| (&clip.id, clip.timeline_range))
        .collect::<HashMap<_, _>>();
    let part_ranges = renamed
        .values()
        .flatten()
        .map(|(range, id)| (id, *range))
        .collect::<HashMap<_, _>>();
    let mut refused: Option<(ClipId, ClipReference, Owner)> = None;
    edited.for_each_clip_reference_mut(&mut |owner, kind, clip| {
        if refused.is_some() {
            return;
        }
        let (span, own) = match owner {
            TimelineOwner::Clip { id, range, .. } => (
                Some(part_ranges.get(id).copied().unwrap_or(range)),
                Some(id),
            ),
            TimelineOwner::Transition { track, index, .. } => {
                (transition_spans[track].get(index).copied(), None)
            }
            TimelineOwner::Track { .. } | TimelineOwner::Master => (None, None),
        };
        let matte = match kind {
            ClipReference::Matte => true,
            // A binding through its own clip was renamed with the clip's part.
            ClipReference::Via if own != Some(clip) => false,
            _ => return,
        };
        match part_for(clip, span, matte, renamed, &old_ranges) {
            Some(part) => *clip = part,
            None => refused = Some((clip.clone(), kind, Owner::of(owner))),
        }
    });
    match refused {
        Some((clip, ClipReference::Matte, _)) => return Err(EditError::MatteSource(clip)),
        Some((clip, _, owner)) => {
            return Err(EditError::ReadThrough {
                clip,
                by: owner.describe(edited),
            });
        }
        None => {}
    }
    let sources = original_clips()
        .filter_map(|clip| clip.matte.as_ref().map(|matte| &matte.source))
        .collect::<HashSet<_>>();
    let named = edited
        .timeline
        .tracks
        .iter()
        .flat_map(|track| &track.clips)
        .filter_map(|clip| clip.matte.as_ref().map(|matte| &matte.source))
        .collect::<HashSet<_>>();
    for clip in original_clips().filter(|clip| sources.contains(&clip.id)) {
        let unnamed = renamed.get(&clip.id).map_or_else(
            || !named.contains(&clip.id),
            |parts| parts.iter().any(|(_, id)| !named.contains(id)),
        );
        if unnamed {
            return Err(EditError::MatteSource(clip.id.clone()));
        }
    }
    Ok(())
}

/// The kept stretches between removed ranges that moved, over every shifted track's extent.
fn moved_stretches(
    project: &Project,
    removal: &[(i64, i64)],
    shifted: &HashSet<usize>,
) -> Vec<Moved> {
    if shifted.is_empty() {
        return Vec::new();
    }
    let end = project
        .timeline
        .tracks
        .iter()
        .enumerate()
        .filter(|(position, _)| shifted.contains(position))
        .flat_map(|(_, track)| {
            track
                .clips
                .iter()
                .map(|clip| clip.timeline_range.end().ticks())
        })
        .max()
        .unwrap_or(0);
    let mut moved = Vec::new();
    for (index, (_, to)) in removal.iter().enumerate() {
        let next = removal.get(index + 1).map_or(end, |(from, _)| *from);
        if *to >= next {
            continue;
        }
        let delta = shift_of(*to, removal);
        if let (Ok(from), Ok(to)) = (
            TimeRange::new(MediaTime(*to), MediaTime(next)),
            TimeRange::new(MediaTime(to - delta), MediaTime(next - delta)),
        ) {
            moved.push(Moved { from, to });
        }
    }
    moved
}

/// The timeline times `times` maps to through the target clips, on the project's frame grid.
fn split_cuts(
    project: &Project,
    times: &Times,
    targets: &[(usize, &Clip)],
) -> Result<BTreeSet<i64>, EditError> {
    let rate = project.settings.frame_rate;
    let (anchor, instants) = match times {
        Times::Data(DataRef { data }) => {
            let set = project
                .data
                .iter()
                .find(|set| &set.id == data)
                .ok_or_else(|| EditError::UnknownData(data.clone()))?;
            let DataValues::Intervals(intervals) = &set.values else {
                return Err(EditError::WrongKind {
                    data: data.clone(),
                    kind: set.kind(),
                });
            };
            (
                set.anchor.clone(),
                intervals.iter().map(|item| item.start).collect::<Vec<_>>(),
            )
        }
        Times::List(TimeList { list, anchor }) => (
            anchor.clone().ok_or(EditError::MissingAnchor)?,
            list.clone(),
        ),
    };
    let mut cuts: BTreeSet<i64> = BTreeSet::new();
    for (_, clip) in targets {
        match &anchor {
            DataAnchor::Timeline => {
                cuts.extend(instants.iter().map(|time| snap(time.ticks(), rate)));
            }
            DataAnchor::Source { source_id } if &clip.source_id == source_id => {
                for instant in &instants {
                    let mapped = crate::source_instant_to_timeline(
                        clip.timeline_range,
                        clip.source_range,
                        clip.time_map,
                        rate,
                        *instant,
                    )
                    .unwrap_or_default();
                    cuts.extend(mapped.into_iter().map(|time| snap(time.ticks(), rate)));
                }
            }
            DataAnchor::Source { .. } => {}
        }
    }
    Ok(cuts)
}

fn split_at(
    project: &Project,
    times: &Times,
    target: &Target,
) -> Result<(Project, EditReport), EditError> {
    let targets = target_clips(project, target)?;
    let cuts = split_cuts(project, times, &targets)?;
    let mut planned = Vec::with_capacity(targets.len());
    for (track, clip) in &targets {
        let (start, end) = (
            clip.timeline_range.start().ticks(),
            clip.timeline_range.end().ticks(),
        );
        let cuts = cuts.range(start + 1..end).copied().collect::<BTreeSet<_>>();
        if !cuts.is_empty()
            && !planned
                .iter()
                .any(|entry: &plan::Planned<'_>| entry.clip.id == clip.id)
        {
            planned.push(plan::Planned {
                track: *track,
                clip,
                cuts,
            });
        }
    }
    let mut edited = project.clone();
    let mut report = EditReport::default();
    let mut skipped = plan::skip_cuts(project, &mut edited, &mut planned);
    skipped.sort_by_key(|(index, cut, _)| {
        let entry = &planned[*index];
        let order = project.timeline.tracks[entry.track]
            .clips
            .iter()
            .position(|clip| clip.id == entry.clip.id);
        (entry.track, order, *cut)
    });
    report.skipped = skipped
        .into_iter()
        .map(|(index, cut, reason)| Skipped {
            clip: planned[index].clip.id.clone(),
            time: MediaTime(cut),
            reason,
        })
        .collect();
    let cut_by = planned
        .iter()
        .filter(|entry| !entry.cuts.is_empty())
        .map(|entry| (&entry.clip.id, &entry.cuts))
        .collect::<HashMap<_, _>>();
    let mut ids = split::IdPool::new(project);
    let mut renamed = HashMap::new();
    for track in &mut edited.timeline.tracks {
        let mut clips = Vec::with_capacity(track.clips.len());
        for clip in &track.clips {
            let Some(inside) = cut_by.get(&clip.id) else {
                clips.push(clip.clone());
                continue;
            };
            let (start, end) = (
                clip.timeline_range.start().ticks(),
                clip.timeline_range.end().ticks(),
            );
            let mut windows = Vec::with_capacity(inside.len() + 1);
            let mut cursor = start;
            for cut in *inside {
                windows.push((cursor, *cut));
                cursor = *cut;
            }
            windows.push((cursor, end));
            let parts = split::parts(clip, &windows, &mut ids, &mut report.created);
            renamed.insert(
                clip.id.clone(),
                parts
                    .iter()
                    .map(|part| (part.timeline_range, part.id.clone()))
                    .collect::<Vec<_>>(),
            );
            clips.extend(parts);
        }
        track.clips = clips;
    }
    let transition_spans =
        retarget_transitions(&mut edited, project, &[], &HashSet::new(), &renamed);
    retarget_clip_references(&mut edited, project, &renamed, &transition_spans)?;
    Ok((edited, report))
}
