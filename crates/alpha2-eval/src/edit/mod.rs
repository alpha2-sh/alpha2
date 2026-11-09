//! The edit operations `remove_ranges`, `split_at` and `bake`, which apply data to a project once.
//!
//! Operations are named for primitives, never for features, and are pure: a project and a
//! request in, a project and a report out, with no I/O. Every kept moment renders as before:
//! clips are split so each part continues its source and time map, clip-attached keyframes move
//! with their clip, timeline-anchored data and caption cues are cut and shifted, and transitions
//! whose cut point is removed are removed. The result validates and is canonical.

mod bake;
mod json;
mod request;
mod split;
#[cfg(test)]
mod surface_corpus_tests;
#[cfg(test)]
mod tests;

pub use json::{EditFailure, EditedJson, apply_json};
pub use request::{
    BakeTarget, Created, DataRef, EditError, EditReport, EditRequest, Moved, RangeList, Ranges,
    RawRange, Ripple, Target, TimeList, Times,
};

use alpha2_model::{Animated, AnimatedPoint, Clip, ClipId, DataAnchor, DataValues, Project, Track};
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
    edited.validate().map_err(EditError::InvalidResult)?;
    Ok((edited, report))
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
    // Padding keeps material at each edge; cuts then land on the project's frame grid.
    let removal = union(
        removal
            .into_iter()
            .map(|(start, end)| (start + padding.ticks(), end - padding.ticks()))
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
    let mut renamed: HashMap<ClipId, Vec<(TimeRange, ClipId)>> = HashMap::new();
    let crossed = crossed_transitions(project, &removal, &shifted_tracks, &cut_clips);
    for (position, track) in edited.timeline.tracks.iter_mut().enumerate() {
        let shifts = shifted_tracks.contains(&position);
        let mut clips = Vec::with_capacity(track.clips.len());
        for clip in &track.clips {
            if !shifts && !cut_clips.contains(&clip.id) {
                clips.push(clip.clone());
                continue;
            }
            let mut own = removal.clone();
            own.extend(crossed.get(&clip.id).into_iter().flatten().copied());
            let keep = kept_windows(clip.timeline_range, &union(own));
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
        if shifts {
            if let Some(captions) = &mut track.captions {
                captions.cues = shift_cues(&captions.cues, &removal);
            }
            shift_track_keys(track, &removal);
        }
    }
    retarget_transitions(&mut edited, project, &removal, &shifted_tracks, &renamed);
    retarget_mattes(&mut edited, &renamed);
    if ripple == Ripple::Timeline {
        shift_master_keys(&mut edited, &removal);
        for set in &mut edited.data {
            if set.anchor == DataAnchor::Timeline {
                split::cut_data(set, &removal);
            }
        }
    }
    report.moved = moved_stretches(project, &removal, &shifted_tracks);
    Ok((edited, report))
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

/// Keyframes of a curve not tied to a clip, on a rippled timeline: those inside removed ranges
/// go, the rest move, and the value at each cut is keyed on both sides.
fn shift_keys(animation: &mut Animated<f32>, removal: &[(i64, i64)]) {
    let Animated::Keyframes { keyframes } = animation else {
        return;
    };
    let context = crate::SampleContext::NONE;
    let old = Animated::Keyframes {
        keyframes: keyframes.clone(),
    };
    let value = |tick: i64| crate::sample_scalar(&old, MediaTime(tick), &context);
    let mut keys: Vec<alpha2_model::Keyframe<f32>> = Vec::new();
    for keyframe in keyframes.iter() {
        if let Some(tick) = new_time(keyframe.time.ticks(), removal) {
            keys.push(alpha2_model::Keyframe {
                time: MediaTime(tick),
                ..keyframe.clone()
            });
        }
    }
    for (from, to) in removal {
        let left = from - 1 - shift_of(from - 1, removal);
        let right = to - shift_of(*to, removal);
        if left >= 0 {
            keys.push(alpha2_model::Keyframe {
                time: MediaTime(left),
                value: value(from - 1),
                interpolation: alpha2_model::Interpolation::Hold,
            });
        }
        keys.push(alpha2_model::Keyframe {
            time: MediaTime(right),
            value: value(*to),
            interpolation: alpha2_model::Interpolation::Linear,
        });
    }
    keys.sort_by_key(|key| key.time);
    keys.dedup_by_key(|key| key.time);
    *keyframes = keys;
}

fn shift_track_keys(track: &mut Track, removal: &[(i64, i64)]) {
    shift_keys(&mut track.audio.gain, removal);
    shift_keys(&mut track.audio.pan, removal);
}

fn shift_master_keys(project: &mut Project, removal: &[(i64, i64)]) {
    shift_keys(&mut project.audio.gain, removal);
}

/// Transitions whose cut point is removed go; the rest name the parts that now hold them and
/// move with their track.
fn retarget_transitions(
    edited: &mut Project,
    original: &Project,
    removal: &[(i64, i64)],
    shifted: &HashSet<usize>,
    renamed: &HashMap<ClipId, Vec<(TimeRange, ClipId)>>,
) {
    for (position, track) in edited.timeline.tracks.iter_mut().enumerate() {
        let before = &original.timeline.tracks[position].transitions;
        let mut kept = Vec::with_capacity(before.len());
        for transition in before {
            let range = transition.timeline_range;
            let crosses = removal
                .iter()
                .any(|(from, to)| *from < range.end().ticks() && range.start().ticks() < *to);
            if crosses
                && (shifted.contains(&position) || renamed.contains_key(&transition.from_clip))
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
            transition.timeline_range = TimeRange::new(
                MediaTime(range.start().ticks() - delta),
                MediaTime(range.end().ticks() - delta),
            )
            .unwrap_or(range);
            kept.push(transition);
        }
        track.transitions = kept;
    }
}

/// Mattes naming a split clip name its first part.
fn retarget_mattes(edited: &mut Project, renamed: &HashMap<ClipId, Vec<(TimeRange, ClipId)>>) {
    let first = renamed
        .iter()
        .filter_map(|(old, parts)| Some((old.clone(), parts.first()?.1.clone())))
        .collect::<HashMap<_, _>>();
    let present = edited
        .timeline
        .tracks
        .iter()
        .flat_map(|track| track.clips.iter().map(|clip| clip.id.clone()))
        .collect::<HashSet<_>>();
    for clip in edited
        .timeline
        .tracks
        .iter_mut()
        .flat_map(|track| track.clips.iter_mut())
    {
        if let Some(matte) = &mut clip.matte
            && let Some(new) = first.get(&matte.source)
        {
            matte.source = new.clone();
        }
        if clip
            .matte
            .as_ref()
            .is_some_and(|matte| !present.contains(&matte.source))
        {
            clip.matte = None;
        }
    }
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

fn split_at(
    project: &Project,
    times: &Times,
    target: &Target,
) -> Result<(Project, EditReport), EditError> {
    let targets = target_clips(project, target)?;
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
    for (_, clip) in &targets {
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
    let target_ids = targets
        .iter()
        .map(|(_, clip)| &clip.id)
        .collect::<HashSet<_>>();
    let mut edited = project.clone();
    let mut report = EditReport::default();
    let mut ids = split::IdPool::new(project);
    let mut renamed = HashMap::new();
    for track in &mut edited.timeline.tracks {
        let mut clips = Vec::with_capacity(track.clips.len());
        for clip in &track.clips {
            let (start, end) = (
                clip.timeline_range.start().ticks(),
                clip.timeline_range.end().ticks(),
            );
            let inside = cuts.range(start + 1..end).copied().collect::<Vec<_>>();
            if !target_ids.contains(&clip.id) || inside.is_empty() {
                clips.push(clip.clone());
                continue;
            }
            let mut windows = Vec::with_capacity(inside.len() + 1);
            let mut cursor = start;
            for cut in inside {
                windows.push((cursor, cut));
                cursor = cut;
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
    retarget_transitions(&mut edited, project, &[], &HashSet::new(), &renamed);
    retarget_mattes(&mut edited, &renamed);
    Ok((edited, report))
}

/// Each animated value of a clip whose keyframes are in timeline time, for moving them.
pub(crate) fn clip_timeline_values(clip: &mut Clip, mut visit: impl FnMut(&mut dyn TimelineValue)) {
    visit(&mut clip.visual.position);
    visit(&mut clip.visual.scale);
    visit(&mut clip.visual.rotation_degrees);
    visit(&mut clip.visual.opacity);
    for mask in &mut clip.masks {
        visit(&mut mask.feather);
        split::visit_geometry(&mut mask.shape, &mut visit);
    }
    visit(&mut clip.audio.gain);
    visit(&mut clip.audio.pan);
}

/// A value keyed in timeline time whose keyframes can move and be pruned to a window.
pub(crate) trait TimelineValue {
    /// Moves every keyframe by `delta` ticks and keeps those that matter in `window`.
    fn shift_and_prune(&mut self, delta: i64, window: (i64, i64));
    /// Points `via: { clip: from }` at `to`.
    fn rename_via(&mut self, from: &ClipId, to: &ClipId);
}

/// The keyframes that decide values inside `window`: those inside it and one on each side.
fn prune_range(times: &[i64], window: (i64, i64)) -> std::ops::Range<usize> {
    let first = times
        .partition_point(|time| *time <= window.0)
        .saturating_sub(1);
    let last = (times.partition_point(|time| *time < window.1) + 1).min(times.len());
    first..last.max(first)
}

fn rename_binding(binding: &mut alpha2_model::DataBinding, from: &ClipId, to: &ClipId) {
    if binding.via == Some(alpha2_model::DataVia::Clip(from.clone())) {
        binding.via = Some(alpha2_model::DataVia::Clip(to.clone()));
    }
}

impl<T: Clone> TimelineValue for Animated<T> {
    fn shift_and_prune(&mut self, delta: i64, window: (i64, i64)) {
        if let Self::Keyframes { keyframes } = self {
            let times = keyframes
                .iter()
                .map(|key| key.time.ticks())
                .collect::<Vec<_>>();
            let keep = prune_range(&times, window);
            *keyframes = keyframes[keep]
                .iter()
                .map(|key| alpha2_model::Keyframe {
                    time: MediaTime(key.time.ticks() + delta),
                    ..key.clone()
                })
                .collect();
        }
    }

    fn rename_via(&mut self, from: &ClipId, to: &ClipId) {
        if let Self::Data(binding) = self {
            rename_binding(binding, from, to);
        }
    }
}

impl TimelineValue for AnimatedPoint {
    fn shift_and_prune(&mut self, delta: i64, window: (i64, i64)) {
        if let Self::Keyframes { keyframes } = self {
            let times = keyframes
                .iter()
                .map(|key| key.time.ticks())
                .collect::<Vec<_>>();
            let keep = prune_range(&times, window);
            *keyframes = keyframes[keep]
                .iter()
                .map(|key| alpha2_model::PointKeyframe {
                    time: MediaTime(key.time.ticks() + delta),
                    ..key.clone()
                })
                .collect();
        }
    }

    fn rename_via(&mut self, from: &ClipId, to: &ClipId) {
        if let Self::Data(binding) = self {
            rename_binding(binding, from, to);
        }
    }
}
