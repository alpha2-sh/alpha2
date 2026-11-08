//! Cutting a clip into parts that each continue its source and time map.

use super::TimelineValue;
use alpha2_model::{Clip, ClipId, DataSet, DataValues, LoopMode, Project, ShapeGeometry, TimeMap};
use alpha2_time::{MediaTime, TimeRange};
use std::collections::HashSet;

/// Fresh clip ids, unique in the project: `<id>.<n>`.
pub(super) struct IdPool {
    taken: HashSet<String>,
}

impl IdPool {
    pub(super) fn new(project: &Project) -> Self {
        Self {
            taken: project
                .timeline
                .tracks
                .iter()
                .flat_map(|track| track.clips.iter().map(|clip| clip.id.as_str().to_owned()))
                .collect(),
        }
    }

    fn fresh(&mut self, base: &ClipId) -> ClipId {
        let mut index = 1_u64;
        loop {
            let id = format!("{base}.{index}");
            if self.taken.insert(id.clone()) {
                return ClipId::new(id).unwrap_or_else(|_| base.clone());
            }
            index += 1;
        }
    }
}

fn range(start: i64, end: i64) -> Option<TimeRange> {
    TimeRange::new(MediaTime(start), MediaTime(end))
        .ok()
        .filter(|range| !range.is_empty())
}

/// `floor(value * num / den)` in wide integers.
fn scale(value: i64, num: i64, den: i64) -> i64 {
    i64::try_from((i128::from(value) * i128::from(num)).div_euclid(i128::from(den.max(1))))
        .unwrap_or(i64::MAX)
}

/// One part of `clip` over `window` with `time_map` and `source`, before ids and keyframes.
fn piece(clip: &Clip, window: (i64, i64), source: (i64, i64), time_map: TimeMap) -> Option<Clip> {
    let mut part = clip.clone();
    part.timeline_range = range(window.0, window.1)?;
    part.source_range = range(source.0, source.1)?;
    part.time_map = time_map;
    Some(part)
}

/// The pieces showing `window` of `clip`: one clip per window, or for a loop, a head for a
/// partial pass, whole passes as a loop and, for a ping-pong starting backward, a reversed pass.
fn pieces(clip: &Clip, window: (i64, i64)) -> Vec<Clip> {
    let (t0, t1) = (
        clip.timeline_range.start().ticks(),
        clip.timeline_range.end().ticks(),
    );
    let (s0, s1) = (
        clip.source_range.start().ticks(),
        clip.source_range.end().ticks(),
    );
    let (length, span) = (t1 - t0, s1 - s0);
    let (a, b) = window;
    match clip.time_map {
        TimeMap::Forward => {
            let from = s0 + scale(a - t0, span, length);
            let to = s0 + scale(b - t0, span, length);
            piece(clip, window, (from, to), TimeMap::Forward)
                .into_iter()
                .collect()
        }
        TimeMap::Reverse => {
            // A reversed clip shows the source end first; each part keeps its mirror.
            let from = s1 - scale(b - t0, span, length);
            let to = s1 - scale(a - t0, span, length);
            piece(clip, window, (from, to), TimeMap::Reverse)
                .into_iter()
                .collect()
        }
        TimeMap::Freeze { .. } => piece(clip, window, (s0, s1), clip.time_map)
            .into_iter()
            .collect(),
        TimeMap::Loop { mode } => {
            let mut out = Vec::new();
            let mut cursor = a;
            while cursor < b {
                let pass = (cursor - t0).div_euclid(span);
                let pass_start = t0 + pass * span;
                let pass_end = (pass_start + span).min(b);
                let backward = mode == LoopMode::PingPong && pass % 2 == 1;
                if cursor == pass_start && !backward && b - cursor > span {
                    // Whole passes from a forward pass: one loop over the rest.
                    out.extend(piece(clip, (cursor, b), (s0, s1), clip.time_map));
                    break;
                }
                let phase = (cursor - pass_start, pass_end - pass_start);
                let (map, source) = if backward {
                    (TimeMap::Reverse, (s1 - phase.1, s1 - phase.0))
                } else {
                    (TimeMap::Forward, (s0 + phase.0, s0 + phase.1))
                };
                out.extend(piece(clip, (cursor, pass_end), source, map));
                cursor = pass_end;
            }
            out
        }
    }
}

/// `clip` cut to the windows in `keep`, at their old timeline positions. A window that is the
/// whole clip keeps the clip; other parts get fresh ids recorded in `created`, and their
/// bindings through the clip read through the part.
pub(super) fn parts(
    clip: &Clip,
    keep: &[(i64, i64)],
    ids: &mut IdPool,
    created: &mut Vec<ClipId>,
) -> Vec<Clip> {
    let whole = (
        clip.timeline_range.start().ticks(),
        clip.timeline_range.end().ticks(),
    );
    if keep == [whole] {
        return vec![clip.clone()];
    }
    let mut out = Vec::new();
    let mut first = true;
    for window in keep {
        for mut part in pieces(clip, *window) {
            let window = (
                part.timeline_range.start().ticks(),
                part.timeline_range.end().ticks(),
            );
            if first {
                first = false;
            } else {
                part.id = ids.fresh(&clip.id);
                created.push(part.id.clone());
            }
            if let Some(composition) = &mut part.composition {
                let advanced = scale(
                    window.0 - whole.0,
                    i64::from(composition.scale.num),
                    i64::from(composition.scale.den),
                );
                composition.offset = MediaTime(composition.offset.ticks() + advanced);
            }
            fades(&mut part, whole, window);
            let new_id = part.id.clone();
            super::clip_timeline_values(&mut part, |value| {
                value.shift_and_prune(0, window);
                value.rename_via(&clip.id, &new_id);
            });
            out.push(part);
        }
    }
    out
}

/// Fades stay where they were: a part keeps a fade only where the fade lies inside it.
fn fades(part: &mut Clip, whole: (i64, i64), window: (i64, i64)) {
    if window.0 != whole.0 {
        part.audio.fade_in = MediaTime::ZERO;
    }
    if window.1 != whole.1 {
        part.audio.fade_out = MediaTime::ZERO;
    }
}

/// `part` moved left by `delta` ticks with its keyframes.
pub(super) fn moved(mut part: Clip, delta: i64) -> Clip {
    if delta == 0 {
        return part;
    }
    let (start, end) = (
        part.timeline_range.start().ticks(),
        part.timeline_range.end().ticks(),
    );
    part.timeline_range = TimeRange::new(MediaTime(start - delta), MediaTime(end - delta))
        .unwrap_or(part.timeline_range);
    super::clip_timeline_values(&mut part, |value| {
        value.shift_and_prune(-delta, (start, end));
    });
    part
}

/// Visits each animated value of a mask outline.
pub(super) fn visit_geometry(
    geometry: &mut ShapeGeometry,
    visit: &mut impl FnMut(&mut dyn TimelineValue),
) {
    match geometry {
        ShapeGeometry::Rectangle(rectangle) => {
            visit(&mut rectangle.center);
            visit(&mut rectangle.size);
            visit(&mut rectangle.corner_radius);
            visit(&mut rectangle.rotation_degrees);
        }
        ShapeGeometry::Ellipse(ellipse) => {
            visit(&mut ellipse.center);
            visit(&mut ellipse.size);
            visit(&mut ellipse.rotation_degrees);
        }
        ShapeGeometry::Polystar(polystar) => {
            visit(&mut polystar.center);
            visit(&mut polystar.points);
            visit(&mut polystar.outer_radius);
            visit(&mut polystar.inner_radius);
            visit(&mut polystar.outer_roundness);
            visit(&mut polystar.inner_roundness);
            visit(&mut polystar.rotation_degrees);
        }
        ShapeGeometry::Path(path) => visit(&mut path.path),
    }
}

/// A timeline-anchored set with the removed ranges cut out and later times moved left. A sample
/// set keeps, at each cut, the value read just after it, so the kept side reads as before.
pub(super) fn cut_data(set: &mut DataSet, removal: &[(i64, i64)]) {
    let new_time = |tick: i64| super::new_time(tick, removal);
    match &mut set.values {
        DataValues::Points(samples)
        | DataValues::Scalars(samples)
        | DataValues::Similarity(samples) => {
            let width = samples.values.len() / samples.times.len().max(1);
            let channels = samples.channels.len().max(1);
            let arity = width / channels;
            let mut entries: Vec<(i64, Vec<Option<f32>>)> = Vec::new();
            for (index, time) in samples.times.iter().enumerate() {
                if let Some(tick) = new_time(time.ticks()) {
                    entries.push((
                        tick,
                        samples.values[index * width..(index + 1) * width].to_vec(),
                    ));
                }
            }
            for (_, to) in removal {
                let Some(at) = new_time(*to) else { continue };
                if entries.iter().any(|(tick, _)| *tick == at) {
                    continue;
                }
                let held = (0..channels)
                    .flat_map(|channel| {
                        crate::sample_channel(samples, channel, arity, MediaTime(*to)).map_or_else(
                            || vec![None; arity],
                            |values| values.into_iter().map(Some).collect(),
                        )
                    })
                    .collect::<Vec<_>>();
                entries.push((at, held));
            }
            entries.sort_by_key(|(tick, _)| *tick);
            samples.times = entries.iter().map(|(tick, _)| MediaTime(*tick)).collect();
            samples.values = entries.into_iter().flat_map(|(_, values)| values).collect();
        }
        DataValues::Words(words) => {
            words.retain_mut(|word| cut_item(&mut word.start, &mut word.end, removal, false));
            words.sort_by_key(|word| (word.start, word.end));
        }
        DataValues::Intervals(intervals) => {
            intervals.retain_mut(|item| cut_item(&mut item.start, &mut item.end, removal, true));
            intervals.sort_by_key(|item| (item.start, item.end));
        }
    }
}

/// An item's edges moved past removed ranges; `false` drops it when nothing of it stays.
fn cut_item(
    start: &mut MediaTime,
    end: &mut MediaTime,
    removal: &[(i64, i64)],
    instants: bool,
) -> bool {
    let windows =
        super::kept_windows_ticks(start.ticks(), end.ticks().max(start.ticks() + 1), removal);
    let (Some(first), Some(last)) = (windows.first(), windows.last()) else {
        return false;
    };
    let from = first.0 - super::shift_of(first.0, removal);
    let to = if end.ticks() == start.ticks() {
        from
    } else {
        last.1 - 1 - super::shift_of(last.1 - 1, removal) + 1
    };
    *start = MediaTime(from);
    *end = MediaTime(to);
    instants || from < to
}
