//! Bakes data bindings into keyframes and caption words into cues.

use super::{BakeTarget, EditError, EditReport};
use crate::SampleContext;
use alpha2_model::{
    Animated, AnimatedPoint, DataAnchor, DataBinding, DataInterpolation, DataSpace, DataValues,
    DataVia, Domain, Interpolation, Keyframe, PointKeyframe, Project, Vec2,
};
use alpha2_time::{MediaTime, TimeRange};

/// A value found at a bake target's path, with the domain its keyframe values must lie in.
enum Slot<'a> {
    Scalar(&'a mut Animated<f32>, Domain),
    /// A two-component value, each component in the domain.
    Vec2(&'a mut Animated<Vec2>, Domain),
    Point(&'a mut AnimatedPoint),
    /// A built-in effect's parameter, with its declared type and range; `None` for a host effect.
    Param(
        &'a mut Animated<alpha2_model::EffectValue>,
        Option<(alpha2_model::EffectParamType, Domain)>,
    ),
}

pub(super) fn bake(
    project: &Project,
    targets: &[BakeTarget],
) -> Result<(Project, EditReport), EditError> {
    let mut edited = project.clone();
    for target in targets {
        match target {
            BakeTarget::Track { track, property } if property == "captions" => {
                let position = edited
                    .timeline
                    .tracks
                    .iter()
                    .position(|candidate| &candidate.id == track)
                    .ok_or_else(|| EditError::UnknownTarget(format!("track {track}")))?;
                let captions = edited.timeline.tracks[position]
                    .captions
                    .as_ref()
                    .filter(|captions| captions.from_words.is_some())
                    .ok_or_else(|| EditError::NothingToBake(format!("track {track} captions")))?;
                let cues = crate::caption_cues(project, track, captions)
                    .map_err(|_| words_past_their_limits(track))?;
                let captions = edited.timeline.tracks[position]
                    .captions
                    .as_mut()
                    .expect("captions");
                captions.cues = cues;
                captions.from_words = None;
            }
            BakeTarget::Track { track, property } => {
                let position = edited
                    .timeline
                    .tracks
                    .iter()
                    .position(|candidate| &candidate.id == track)
                    .ok_or_else(|| EditError::UnknownTarget(format!("track {track}")))?;
                let at = format!("track {track} {property}");
                let extent = project.extent().unwrap_or_else(|| {
                    TimeRange::new(MediaTime::ZERO, MediaTime::ZERO).expect("range")
                });
                let audio = &mut edited.timeline.tracks[position].audio;
                let slot = match property.as_str() {
                    "audio.gain" => Slot::Scalar(&mut audio.gain, Domain::GAIN),
                    "audio.pan" => Slot::Scalar(&mut audio.pan, Domain::PAN),
                    _ => return Err(EditError::UnknownTarget(at)),
                };
                bake_slot(project, slot, &at, extent, None)?;
            }
            BakeTarget::Clip { clip, property } => {
                let at = format!("clip {clip} {property}");
                let found = edited
                    .timeline
                    .tracks
                    .iter_mut()
                    .flat_map(|track| track.clips.iter_mut())
                    .find(|candidate| &candidate.id == clip)
                    .ok_or_else(|| EditError::UnknownTarget(format!("clip {clip}")))?;
                if property == "visual.correction" {
                    return Err(EditError::CorrectionNotBakeable(at));
                }
                let window = found.timeline_range;
                let slot = clip_slot(found, property)
                    .ok_or_else(|| EditError::UnknownTarget(at.clone()))?;
                bake_slot(project, slot, &at, window, None)?;
            }
            BakeTarget::Node {
                composition,
                node,
                property,
            } => {
                let at = format!("composition {composition} node {node} {property}");
                let found = edited
                    .compositions
                    .iter_mut()
                    .find(|candidate| &candidate.id == composition)
                    .ok_or_else(|| {
                        EditError::UnknownTarget(format!("composition {composition}"))
                    })?;
                let original = project
                    .compositions
                    .iter()
                    .find(|candidate| &candidate.id == composition)
                    .expect("the same composition");
                let entry = found
                    .nodes
                    .iter_mut()
                    .find(|candidate| &candidate.id == node)
                    .ok_or_else(|| EditError::UnknownTarget(format!("node {node}")))?;
                let slot = node_slot(&mut entry.op, property)
                    .ok_or_else(|| EditError::UnknownTarget(at.clone()))?;
                bake_slot(
                    project,
                    slot,
                    &at,
                    TimeRange::new(MediaTime::ZERO, MediaTime::ZERO).expect("range"),
                    Some(original),
                )?;
            }
        }
    }
    Ok((edited, EditReport::default()))
}

fn clip_slot<'a>(clip: &'a mut alpha2_model::Clip, property: &str) -> Option<Slot<'a>> {
    Some(match property {
        "visual.position" => Slot::Point(&mut clip.visual.position),
        "visual.scale" => Slot::Vec2(&mut clip.visual.scale, Domain::ANY),
        "visual.rotation_degrees" => Slot::Scalar(&mut clip.visual.rotation_degrees, Domain::ANY),
        "visual.opacity" => Slot::Scalar(&mut clip.visual.opacity, Domain::UNIT),
        "audio.gain" => Slot::Scalar(&mut clip.audio.gain, Domain::GAIN),
        "audio.pan" => Slot::Scalar(&mut clip.audio.pan, Domain::PAN),
        _ => {
            let rest = property.strip_prefix("masks[")?;
            let (index, field) = rest.split_once("].")?;
            let mask = clip.masks.get_mut(index.parse::<usize>().ok()?)?;
            if field == "feather" {
                return Some(Slot::Scalar(&mut mask.feather, Domain::NON_NEGATIVE));
            }
            geometry_slot(&mut mask.shape, field.strip_prefix("shape.")?)?
        }
    })
}

fn geometry_slot<'a>(shape: &'a mut alpha2_model::ShapeGeometry, field: &str) -> Option<Slot<'a>> {
    use alpha2_model::ShapeGeometry;
    Some(match (shape, field) {
        (ShapeGeometry::Rectangle(geometry), "center") => Slot::Point(&mut geometry.center),
        (ShapeGeometry::Rectangle(geometry), "size") => {
            Slot::Vec2(&mut geometry.size, Domain::NON_NEGATIVE)
        }
        (ShapeGeometry::Rectangle(geometry), "corner_radius") => {
            Slot::Scalar(&mut geometry.corner_radius, Domain::NON_NEGATIVE)
        }
        (ShapeGeometry::Rectangle(geometry), "rotation_degrees") => {
            Slot::Scalar(&mut geometry.rotation_degrees, Domain::ANY)
        }
        (ShapeGeometry::Ellipse(geometry), "center") => Slot::Point(&mut geometry.center),
        (ShapeGeometry::Ellipse(geometry), "size") => {
            Slot::Vec2(&mut geometry.size, Domain::NON_NEGATIVE)
        }
        (ShapeGeometry::Ellipse(geometry), "rotation_degrees") => {
            Slot::Scalar(&mut geometry.rotation_degrees, Domain::ANY)
        }
        (ShapeGeometry::Polystar(geometry), "center") => Slot::Point(&mut geometry.center),
        (ShapeGeometry::Polystar(geometry), "points") => {
            Slot::Scalar(&mut geometry.points, Domain::POLYSTAR_POINTS)
        }
        (ShapeGeometry::Polystar(geometry), "outer_radius") => {
            Slot::Scalar(&mut geometry.outer_radius, Domain::NON_NEGATIVE)
        }
        (ShapeGeometry::Polystar(geometry), "inner_radius") => {
            Slot::Scalar(&mut geometry.inner_radius, Domain::NON_NEGATIVE)
        }
        (ShapeGeometry::Polystar(geometry), "outer_roundness") => {
            Slot::Scalar(&mut geometry.outer_roundness, Domain::UNIT)
        }
        (ShapeGeometry::Polystar(geometry), "inner_roundness") => {
            Slot::Scalar(&mut geometry.inner_roundness, Domain::UNIT)
        }
        (ShapeGeometry::Polystar(geometry), "rotation_degrees") => {
            Slot::Scalar(&mut geometry.rotation_degrees, Domain::ANY)
        }
        _ => return None,
    })
}

fn node_slot<'a>(op: &'a mut alpha2_model::NodeOp, property: &str) -> Option<Slot<'a>> {
    use alpha2_model::NodeOp;
    Some(match op {
        NodeOp::Effect { effect, .. } | NodeOp::Transition { effect, .. }
            if property.starts_with("effect.params.") =>
        {
            let name = property.strip_prefix("effect.params.")?;
            let declared = alpha2_model::builtin_effect(&effect.id)
                .and_then(|spec| spec.param(name))
                .map(|param| (param.param_type, param.domain()));
            Slot::Param(effect.params.get_mut(name)?, declared)
        }
        NodeOp::Transition { progress, .. } if property == "progress" => {
            Slot::Scalar(progress, Domain::UNIT)
        }
        NodeOp::Transform { transform, .. } => match property {
            "transform.position" => Slot::Point(&mut transform.position),
            "transform.scale" => Slot::Vec2(&mut transform.scale, Domain::ANY),
            "transform.rotation_degrees" => {
                Slot::Scalar(&mut transform.rotation_degrees, Domain::ANY)
            }
            "transform.opacity" => Slot::Scalar(&mut transform.opacity, Domain::UNIT),
            _ => return None,
        },
        NodeOp::KeyMix { opacity, .. } if property == "opacity" => {
            Slot::Scalar(opacity, Domain::UNIT)
        }
        NodeOp::Merge { layers } => {
            let index = property
                .strip_prefix("layers[")?
                .strip_suffix("].opacity")?;
            Slot::Scalar(
                &mut layers.get_mut(index.parse::<usize>().ok()?)?.opacity,
                Domain::UNIT,
            )
        }
        _ => return None,
    })
}

/// The binding of a slot, if it follows data.
fn binding_of<'a>(slot: &'a Slot<'_>) -> Option<&'a DataBinding> {
    match slot {
        Slot::Scalar(value, _) => value.data(),
        Slot::Vec2(value, _) => value.data(),
        Slot::Point(value) => value.data(),
        Slot::Param(value, _) => value.data(),
    }
}

/// The times a baked binding gets keyframes at: where its value can change inside `window`
/// (timeline places) or at each sample (composition places), plus every frame start for canvas
/// space, which also follows the other clip's motion. A `linear` binding also keys the first
/// change after the window, so its last line keeps its slope, and where the clip it reads
/// through stops showing source time along one line, past which no change is read.
fn key_times(
    project: &Project,
    binding: &DataBinding,
    context: &SampleContext<'_>,
    window: TimeRange,
    composition: Option<&alpha2_model::Composition>,
    linear: bool,
) -> Vec<i64> {
    let Some(set) = project.data.iter().find(|set| set.id == binding.data) else {
        return Vec::new();
    };
    let Some(samples) = set.values.samples() else {
        return Vec::new();
    };
    if let Some(composition) = composition {
        let start = composition
            .nodes
            .iter()
            .find_map(|node| match (&node.op, &binding.via) {
                (
                    alpha2_model::NodeOp::Media { start, .. },
                    Some(alpha2_model::DataVia::Node(via)),
                ) if &node.id == via => Some(start.ticks()),
                _ => None,
            });
        return start.map_or_else(Vec::new, |start| {
            samples
                .times
                .iter()
                .map(|time| time.ticks() - start)
                .filter(|time| *time >= 0)
                .collect()
        });
    }
    let (start, end) = (window.start().ticks(), window.end().ticks());
    let mut times = vec![start];
    let mut cursor = start;
    while let Some(next) = crate::next_data_change(binding, MediaTime(cursor), context) {
        if next.ticks() >= end {
            if linear {
                times.push(next.ticks());
            }
            break;
        }
        times.push(next.ticks());
        cursor = next.ticks();
    }
    if linear {
        times.extend(line_breaks(project, set, binding, window));
    }
    if binding.space == Some(DataSpace::Canvas) {
        let rate = project.settings.frame_rate;
        let first = rate.frame_at_or_before(window.start()).unwrap_or(0);
        let mut frame = first;
        while let Some(time) = rate.frame_start(frame) {
            if time.ticks() >= end {
                break;
            }
            if time.ticks() > start {
                times.push(time.ticks());
            }
            frame += 1;
        }
    }
    times.sort_unstable();
    times.dedup();
    times
}

fn line_breaks(
    project: &Project,
    set: &alpha2_model::DataSet,
    binding: &DataBinding,
    window: TimeRange,
) -> Vec<i64> {
    let (DataAnchor::Source { .. }, Some(DataVia::Clip(via))) = (&set.anchor, &binding.via) else {
        return Vec::new();
    };
    let Some(clip) = project
        .timeline
        .tracks
        .iter()
        .flat_map(|track| &track.clips)
        .find(|clip| &clip.id == via)
    else {
        return Vec::new();
    };
    let (start, end) = (window.start().ticks(), window.end().ticks());
    let mut breaks = crate::picture_line_breaks(
        clip.timeline_range,
        clip.source_range,
        clip.time_map,
        project.settings.frame_rate,
    )
    .unwrap_or_default();
    breaks.push(end - 1);
    breaks.retain(|tick| (start..end).contains(tick));
    breaks
}

#[allow(clippy::too_many_lines)]
fn bake_slot(
    project: &Project,
    slot: Slot<'_>,
    at: &str,
    window: TimeRange,
    composition: Option<&alpha2_model::Composition>,
) -> Result<(), EditError> {
    let binding = binding_of(&slot)
        .cloned()
        .ok_or_else(|| EditError::NothingToBake(at.to_owned()))?;
    let index = crate::DataIndex::new(project);
    let context = match composition {
        Some(composition) => SampleContext::composition(project, &index, composition),
        None => SampleContext::timeline(project, &index),
    };
    let linear = project
        .data
        .iter()
        .find(|set| set.id == binding.data)
        .and_then(|set| set.values.samples())
        .is_some_and(|samples| samples.interpolation == DataInterpolation::Linear)
        && binding.space != Some(DataSpace::Canvas);
    let interpolation = if linear {
        Interpolation::Linear
    } else {
        Interpolation::Hold
    };
    let mut times = key_times(project, &binding, &context, window, composition, linear);
    if times.is_empty() {
        times.push(window.start().ticks());
    }
    let points = matches!(
        project
            .data
            .iter()
            .find(|set| set.id == binding.data)
            .map(|set| &set.values),
        Some(DataValues::Points(_))
    );
    let scalar = |at: i64| crate::bound_scalar(&binding, MediaTime(at), &context).unwrap_or(0.0);
    let pair = |at: i64| crate::bound_vec2(&binding, MediaTime(at), &context).unwrap_or_default();
    match slot {
        Slot::Scalar(value, domain) => {
            if linear {
                times = with_crossings(&times, domain, |at| vec![scalar(at)]);
            }
            *value = Animated::Keyframes {
                keyframes: times
                    .iter()
                    .map(|time| Keyframe {
                        time: MediaTime(*time),
                        value: domain.clamp(scalar(*time)),
                        interpolation,
                    })
                    .collect(),
            };
        }
        Slot::Vec2(value, domain) => {
            if linear {
                times = with_crossings(&times, domain, |at| {
                    let value = pair(at);
                    vec![value.x, value.y]
                });
            }
            *value = Animated::Keyframes {
                keyframes: times
                    .iter()
                    .map(|time| {
                        let value = pair(*time);
                        Keyframe {
                            time: MediaTime(*time),
                            value: Vec2::new(domain.clamp(value.x), domain.clamp(value.y)),
                            interpolation,
                        }
                    })
                    .collect(),
            };
        }
        Slot::Point(value) => {
            *value = AnimatedPoint::Keyframes {
                keyframes: times
                    .iter()
                    .map(|time| PointKeyframe {
                        time: MediaTime(*time),
                        value: pair(*time),
                        interpolation,
                        out_tangent: Vec2::default(),
                        in_tangent: Vec2::default(),
                    })
                    .collect(),
            };
        }
        Slot::Param(value, declared) => {
            let (declared, domain) =
                declared.ok_or_else(|| EditError::UnboundedParameter(at.to_owned()))?;
            let vector = points || declared == alpha2_model::EffectParamType::Vec2;
            let sample = |at: i64| {
                if vector {
                    let value = pair(at);
                    vec![value.x, value.y]
                } else {
                    vec![scalar(at)]
                }
            };
            if linear {
                times = with_crossings(&times, domain, sample);
            }
            *value = Animated::Keyframes {
                keyframes: times
                    .iter()
                    .map(|time| {
                        let values = sample(*time);
                        let value = if vector {
                            alpha2_model::EffectValue::Vec2([
                                domain.clamp(values[0]),
                                domain.clamp(values[1]),
                            ])
                        } else {
                            alpha2_model::EffectValue::Float(domain.clamp(values[0]))
                        };
                        Keyframe {
                            time: MediaTime(*time),
                            value,
                            interpolation,
                        }
                    })
                    .collect(),
            };
        }
    }
    Ok(())
}

/// `times` plus, between two neighbours where a component of the line `sample` draws crosses an
/// end of `domain`, the ticks either side of the crossing. Linear keys clamped at those ticks
/// then show the live value, which is clamped after interpolating, at every tick.
fn with_crossings(times: &[i64], domain: Domain, sample: impl Fn(i64) -> Vec<f32>) -> Vec<i64> {
    let mut out = times.to_vec();
    for pair in times.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        for (start, end) in sample(from).into_iter().zip(sample(to)) {
            for bound in [domain.min, domain.max] {
                let (start, end, bound) = (f64::from(start), f64::from(end), f64::from(bound));
                if !bound.is_finite() || (start - bound) * (end - bound) >= 0.0 {
                    continue;
                }
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let crossing = from as f64 + (bound - start) / (end - start) * (to - from) as f64;
                #[allow(clippy::cast_possible_truncation)]
                let below = crossing.floor() as i64;
                out.extend(
                    [below, below + 1]
                        .into_iter()
                        .filter(|tick| from < *tick && *tick < to),
                );
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The refusal of a caption bake whose words derive no cues to count.
fn words_past_their_limits(track: &alpha2_model::TrackId) -> EditError {
    EditError::TooManyCues {
        track: track.clone(),
        found: None,
        maximum: alpha2_model::MAX_CLIPS_PER_TRACK,
    }
}
