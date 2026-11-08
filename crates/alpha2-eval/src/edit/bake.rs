//! Baking: data bindings become keyframes, caption words become cues.

use super::{BakeTarget, EditError, EditReport};
use crate::SampleContext;
use alpha2_model::{
    Animated, AnimatedPoint, DataBinding, DataInterpolation, DataSpace, DataValues, Interpolation,
    Keyframe, PointKeyframe, Project, Vec2,
};
use alpha2_time::{MediaTime, TimeRange};

/// A value found at a bake target's path.
enum Slot<'a> {
    Scalar(&'a mut Animated<f32>),
    Vec2(&'a mut Animated<Vec2>),
    Point(&'a mut AnimatedPoint),
    /// An effect parameter, with its declared type when the effect is a built-in.
    Param(
        &'a mut Animated<alpha2_model::EffectValue>,
        Option<alpha2_model::EffectParamType>,
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
                let cues = crate::caption_cues(project, captions);
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
                    "audio.gain" => Slot::Scalar(&mut audio.gain),
                    "audio.pan" => Slot::Scalar(&mut audio.pan),
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
        "visual.scale" => Slot::Vec2(&mut clip.visual.scale),
        "visual.rotation_degrees" => Slot::Scalar(&mut clip.visual.rotation_degrees),
        "visual.opacity" => Slot::Scalar(&mut clip.visual.opacity),
        "audio.gain" => Slot::Scalar(&mut clip.audio.gain),
        "audio.pan" => Slot::Scalar(&mut clip.audio.pan),
        _ => {
            let rest = property.strip_prefix("masks[")?;
            let (index, field) = rest.split_once("].")?;
            let mask = clip.masks.get_mut(index.parse::<usize>().ok()?)?;
            if field == "feather" {
                return Some(Slot::Scalar(&mut mask.feather));
            }
            geometry_slot(&mut mask.shape, field.strip_prefix("shape.")?)?
        }
    })
}

fn geometry_slot<'a>(shape: &'a mut alpha2_model::ShapeGeometry, field: &str) -> Option<Slot<'a>> {
    use alpha2_model::ShapeGeometry;
    Some(match (shape, field) {
        (ShapeGeometry::Rectangle(geometry), "center") => Slot::Point(&mut geometry.center),
        (ShapeGeometry::Rectangle(geometry), "size") => Slot::Vec2(&mut geometry.size),
        (ShapeGeometry::Rectangle(geometry), "corner_radius") => {
            Slot::Scalar(&mut geometry.corner_radius)
        }
        (ShapeGeometry::Rectangle(geometry), "rotation_degrees") => {
            Slot::Scalar(&mut geometry.rotation_degrees)
        }
        (ShapeGeometry::Ellipse(geometry), "center") => Slot::Point(&mut geometry.center),
        (ShapeGeometry::Ellipse(geometry), "size") => Slot::Vec2(&mut geometry.size),
        (ShapeGeometry::Ellipse(geometry), "rotation_degrees") => {
            Slot::Scalar(&mut geometry.rotation_degrees)
        }
        (ShapeGeometry::Polystar(geometry), "center") => Slot::Point(&mut geometry.center),
        (ShapeGeometry::Polystar(geometry), "points") => Slot::Scalar(&mut geometry.points),
        (ShapeGeometry::Polystar(geometry), "outer_radius") => {
            Slot::Scalar(&mut geometry.outer_radius)
        }
        (ShapeGeometry::Polystar(geometry), "inner_radius") => {
            Slot::Scalar(&mut geometry.inner_radius)
        }
        (ShapeGeometry::Polystar(geometry), "outer_roundness") => {
            Slot::Scalar(&mut geometry.outer_roundness)
        }
        (ShapeGeometry::Polystar(geometry), "inner_roundness") => {
            Slot::Scalar(&mut geometry.inner_roundness)
        }
        (ShapeGeometry::Polystar(geometry), "rotation_degrees") => {
            Slot::Scalar(&mut geometry.rotation_degrees)
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
                .map(|param| param.param_type);
            Slot::Param(effect.params.get_mut(name)?, declared)
        }
        NodeOp::Transition { progress, .. } if property == "progress" => Slot::Scalar(progress),
        NodeOp::Transform { transform, .. } => match property {
            "transform.position" => Slot::Point(&mut transform.position),
            "transform.scale" => Slot::Vec2(&mut transform.scale),
            "transform.rotation_degrees" => Slot::Scalar(&mut transform.rotation_degrees),
            "transform.opacity" => Slot::Scalar(&mut transform.opacity),
            _ => return None,
        },
        NodeOp::Merge { layers } => {
            let index = property
                .strip_prefix("layers[")?
                .strip_suffix("].opacity")?;
            Slot::Scalar(&mut layers.get_mut(index.parse::<usize>().ok()?)?.opacity)
        }
        _ => return None,
    })
}

/// The binding of a slot, if it follows data.
fn binding_of<'a>(slot: &'a Slot<'_>) -> Option<&'a DataBinding> {
    match slot {
        Slot::Scalar(value) => value.data(),
        Slot::Vec2(value) => value.data(),
        Slot::Point(value) => value.data(),
        Slot::Param(value, _) => value.data(),
    }
}

/// The times a baked binding gets keyframes at: where its value can change inside `window`
/// (timeline places) or at each sample (composition places), plus every frame start for canvas
/// space, which also follows the other clip's motion.
fn key_times(
    project: &Project,
    binding: &DataBinding,
    context: &SampleContext<'_>,
    window: TimeRange,
    composition: Option<&alpha2_model::Composition>,
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
            break;
        }
        times.push(next.ticks());
        cursor = next.ticks();
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
    let mut times = key_times(project, &binding, &context, window, composition);
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
    match slot {
        Slot::Scalar(value) => {
            *value = Animated::Keyframes {
                keyframes: times
                    .iter()
                    .map(|time| Keyframe {
                        time: MediaTime(*time),
                        value: crate::bound_scalar(&binding, MediaTime(*time), &context)
                            .unwrap_or(0.0),
                        interpolation,
                    })
                    .collect(),
            };
        }
        Slot::Vec2(value) => {
            *value = Animated::Keyframes {
                keyframes: times
                    .iter()
                    .map(|time| Keyframe {
                        time: MediaTime(*time),
                        value: crate::bound_vec2(&binding, MediaTime(*time), &context)
                            .unwrap_or_default(),
                        interpolation,
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
                        value: crate::bound_vec2(&binding, MediaTime(*time), &context)
                            .unwrap_or_default(),
                        interpolation,
                        out_tangent: Vec2::default(),
                        in_tangent: Vec2::default(),
                    })
                    .collect(),
            };
        }
        Slot::Param(value, declared) => {
            *value = Animated::Keyframes {
                keyframes: times
                    .iter()
                    .map(|time| {
                        let at = MediaTime(*time);
                        let sampled =
                            if points || declared == Some(alpha2_model::EffectParamType::Vec2) {
                                let point =
                                    crate::bound_vec2(&binding, at, &context).unwrap_or_default();
                                alpha2_model::EffectValue::Vec2([point.x, point.y])
                            } else {
                                alpha2_model::EffectValue::Float(
                                    crate::bound_scalar(&binding, at, &context).unwrap_or(0.0),
                                )
                            };
                        Keyframe {
                            time: at,
                            value: sampled,
                            interpolation,
                        }
                    })
                    .collect(),
            };
        }
    }
    Ok(())
}
