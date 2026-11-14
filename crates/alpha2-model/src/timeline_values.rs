//! Every value keyed in timeline time and every reference to a clip, for operations that cut or
//! move timeline time.

use crate::{
    Animated, AnimatedPoint, AudioProperties, Clip, ClipId, ClipTransition, DataBinding, DataVia,
    Project, ShapeGeometry, ShapeMask, TrackId, VisualProperties,
};
use alpha2_time::{MediaTime, TimeRange};

/// What holds a value keyed in timeline time or a reference to a clip. `track` is a position in
/// `timeline.tracks`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TimelineOwner<'a> {
    /// A clip's visual, masks, audio or matte.
    Clip {
        /// Its track.
        track: usize,
        /// Its id.
        id: &'a ClipId,
        /// Its timeline range.
        range: TimeRange,
    },
    /// A transition's clips or effect parameters.
    Transition {
        /// Its track.
        track: usize,
        /// Its position in the track's `transitions`.
        index: usize,
        /// Its timeline range.
        range: TimeRange,
    },
    /// A track's bus.
    Track {
        /// Its track.
        track: usize,
        /// Its id.
        id: &'a TrackId,
    },
    /// The master bus.
    Master,
}

/// What a reference to a clip is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ClipReference {
    /// A clip's `matte.source`.
    Matte,
    /// A transition's `from_clip`.
    TransitionFrom,
    /// A transition's `to_clip`.
    TransitionTo,
    /// A data binding's `via: { clip }`.
    Via,
}

/// A value whose keyframes are in timeline time.
pub trait TimelineValue {
    /// Moves every keyframe by `delta` ticks.
    fn shift(&mut self, delta: i64);
    /// Keeps the keyframes that decide values inside `window` (ticks, end exclusive): those
    /// inside it and the nearest one on each side.
    fn keep_window(&mut self, window: (i64, i64));
    /// The clip a data binding reads through, if any.
    fn via_clip(&self) -> Option<&ClipId>;
    /// [`Self::via_clip`], to point it at another clip.
    fn via_clip_mut(&mut self) -> Option<&mut ClipId>;
}

/// The positions of the keyframes that decide values inside `window`, as `first..last`.
fn decisive<K>(keys: &[K], time: impl Fn(&K) -> MediaTime, window: (i64, i64)) -> (usize, usize) {
    let first = keys
        .partition_point(|key| time(key).ticks() <= window.0)
        .saturating_sub(1);
    let last = (keys.partition_point(|key| time(key).ticks() < window.1) + 1).min(keys.len());
    (first, last.max(first))
}

const fn via_of(binding: &DataBinding) -> Option<&ClipId> {
    match &binding.via {
        Some(DataVia::Clip(clip)) => Some(clip),
        _ => None,
    }
}

const fn via_of_mut(binding: &mut DataBinding) -> Option<&mut ClipId> {
    match &mut binding.via {
        Some(DataVia::Clip(clip)) => Some(clip),
        _ => None,
    }
}

impl<T> TimelineValue for Animated<T> {
    fn shift(&mut self, delta: i64) {
        if let Self::Keyframes { keyframes } = self {
            for key in keyframes {
                key.time = MediaTime(key.time.ticks() + delta);
            }
        }
    }

    fn keep_window(&mut self, window: (i64, i64)) {
        if let Self::Keyframes { keyframes } = self {
            let (first, last) = decisive(keyframes, |key| key.time, window);
            keyframes.truncate(last);
            keyframes.drain(..first);
        }
    }

    fn via_clip(&self) -> Option<&ClipId> {
        self.data().and_then(via_of)
    }

    fn via_clip_mut(&mut self) -> Option<&mut ClipId> {
        match self {
            Self::Data(binding) => via_of_mut(binding),
            _ => None,
        }
    }
}

impl TimelineValue for AnimatedPoint {
    fn shift(&mut self, delta: i64) {
        if let Self::Keyframes { keyframes } = self {
            for key in keyframes {
                key.time = MediaTime(key.time.ticks() + delta);
            }
        }
    }

    fn keep_window(&mut self, window: (i64, i64)) {
        if let Self::Keyframes { keyframes } = self {
            let (first, last) = decisive(keyframes, |key| key.time, window);
            keyframes.truncate(last);
            keyframes.drain(..first);
        }
    }

    fn via_clip(&self) -> Option<&ClipId> {
        self.data().and_then(via_of)
    }

    fn via_clip_mut(&mut self) -> Option<&mut ClipId> {
        match self {
            Self::Data(binding) => via_of_mut(binding),
            _ => None,
        }
    }
}

/// A visitor of timeline-clock values, given each value's place as `Type.field` (`Type[tag]`
/// for an enum variant), the names of the model's JSON schema.
pub type TimelineVisit<'v> = dyn FnMut(&'static str, &mut dyn TimelineValue) + 'v;

fn clip_values(
    visual: &mut VisualProperties,
    masks: &mut [ShapeMask],
    audio: &mut AudioProperties,
    visit: &mut TimelineVisit<'_>,
) {
    visit("VisualProperties.position", &mut visual.position);
    visit("VisualProperties.scale", &mut visual.scale);
    visit(
        "VisualProperties.rotation_degrees",
        &mut visual.rotation_degrees,
    );
    visit("VisualProperties.opacity", &mut visual.opacity);
    for mask in masks {
        visit("ShapeMask.feather", &mut mask.feather);
        geometry_values(&mut mask.shape, visit);
    }
    visit("AudioProperties.gain", &mut audio.gain);
    visit("AudioProperties.pan", &mut audio.pan);
}

fn geometry_values(geometry: &mut ShapeGeometry, visit: &mut TimelineVisit<'_>) {
    match geometry {
        ShapeGeometry::Rectangle(shape) => {
            visit("ShapeGeometry[rectangle].center", &mut shape.center);
            visit("ShapeGeometry[rectangle].size", &mut shape.size);
            visit(
                "ShapeGeometry[rectangle].corner_radius",
                &mut shape.corner_radius,
            );
            visit(
                "ShapeGeometry[rectangle].rotation_degrees",
                &mut shape.rotation_degrees,
            );
        }
        ShapeGeometry::Ellipse(shape) => {
            visit("ShapeGeometry[ellipse].center", &mut shape.center);
            visit("ShapeGeometry[ellipse].size", &mut shape.size);
            visit(
                "ShapeGeometry[ellipse].rotation_degrees",
                &mut shape.rotation_degrees,
            );
        }
        ShapeGeometry::Polystar(shape) => {
            visit("ShapeGeometry[polystar].center", &mut shape.center);
            visit("ShapeGeometry[polystar].points", &mut shape.points);
            visit(
                "ShapeGeometry[polystar].outer_radius",
                &mut shape.outer_radius,
            );
            visit(
                "ShapeGeometry[polystar].inner_radius",
                &mut shape.inner_radius,
            );
            visit(
                "ShapeGeometry[polystar].outer_roundness",
                &mut shape.outer_roundness,
            );
            visit(
                "ShapeGeometry[polystar].inner_roundness",
                &mut shape.inner_roundness,
            );
            visit(
                "ShapeGeometry[polystar].rotation_degrees",
                &mut shape.rotation_degrees,
            );
        }
        ShapeGeometry::Path(shape) => visit("ShapeGeometry[path].path", &mut shape.path),
    }
}

impl Clip {
    /// Calls `visit` with each of the clip's values keyed in timeline time.
    pub fn for_each_timeline_value_mut(&mut self, visit: &mut TimelineVisit<'_>) {
        clip_values(&mut self.visual, &mut self.masks, &mut self.audio, visit);
    }
}

impl ClipTransition {
    /// Calls `visit` with each of the transition's values keyed in timeline time.
    pub fn for_each_timeline_value_mut(&mut self, visit: &mut TimelineVisit<'_>) {
        for value in self.effect.params.values_mut() {
            visit("ClipEffect.params", value);
        }
    }
}

impl Project {
    /// Calls `visit` with every value keyed in timeline time and its owner: the clip values and
    /// transition parameters of each track in order, then [`Self::for_each_bus_value_mut`].
    pub fn for_each_timeline_value_mut(
        &mut self,
        visit: &mut dyn FnMut(TimelineOwner<'_>, &'static str, &mut dyn TimelineValue),
    ) {
        for (position, track) in self.timeline.tracks.iter_mut().enumerate() {
            for clip in &mut track.clips {
                let owner = TimelineOwner::Clip {
                    track: position,
                    id: &clip.id,
                    range: clip.timeline_range,
                };
                clip_values(
                    &mut clip.visual,
                    &mut clip.masks,
                    &mut clip.audio,
                    &mut |at, value| visit(owner, at, value),
                );
            }
            for (index, transition) in track.transitions.iter_mut().enumerate() {
                let owner = TimelineOwner::Transition {
                    track: position,
                    index,
                    range: transition.timeline_range,
                };
                transition.for_each_timeline_value_mut(&mut |at, value| visit(owner, at, value));
            }
        }
        self.for_each_bus_value_mut(&mut |owner, at, value| visit(owner, at, value));
    }

    /// Calls `visit` with each bus value keyed in timeline time: the gain and pan of each track
    /// in order, then the master gain.
    pub fn for_each_bus_value_mut(
        &mut self,
        visit: &mut dyn FnMut(TimelineOwner<'_>, &'static str, &mut Animated<f32>),
    ) {
        for (position, track) in self.timeline.tracks.iter_mut().enumerate() {
            let owner = TimelineOwner::Track {
                track: position,
                id: &track.id,
            };
            visit(owner, "TrackAudio.gain", &mut track.audio.gain);
            visit(owner, "TrackAudio.pan", &mut track.audio.pan);
        }
        visit(
            TimelineOwner::Master,
            "MasterAudio.gain",
            &mut self.audio.gain,
        );
    }

    /// Calls `visit` with every reference from one part of the timeline to a clip: mattes,
    /// transition clips and `via: { clip }` bindings, in the order of
    /// [`Self::for_each_timeline_value_mut`].
    pub fn for_each_clip_reference_mut(
        &mut self,
        visit: &mut dyn FnMut(TimelineOwner<'_>, ClipReference, &mut ClipId),
    ) {
        for (position, track) in self.timeline.tracks.iter_mut().enumerate() {
            for clip in &mut track.clips {
                let owner = TimelineOwner::Clip {
                    track: position,
                    id: &clip.id,
                    range: clip.timeline_range,
                };
                if let Some(matte) = &mut clip.matte {
                    visit(owner, ClipReference::Matte, &mut matte.source);
                }
                clip_values(
                    &mut clip.visual,
                    &mut clip.masks,
                    &mut clip.audio,
                    &mut |_, value| {
                        if let Some(via) = value.via_clip_mut() {
                            visit(owner, ClipReference::Via, via);
                        }
                    },
                );
            }
            for (index, transition) in track.transitions.iter_mut().enumerate() {
                let owner = TimelineOwner::Transition {
                    track: position,
                    index,
                    range: transition.timeline_range,
                };
                visit(
                    owner,
                    ClipReference::TransitionFrom,
                    &mut transition.from_clip,
                );
                visit(owner, ClipReference::TransitionTo, &mut transition.to_clip);
                transition.for_each_timeline_value_mut(&mut |_, value| {
                    if let Some(via) = value.via_clip_mut() {
                        visit(owner, ClipReference::Via, via);
                    }
                });
            }
        }
        self.for_each_bus_value_mut(&mut |owner, _, value| {
            if let Some(via) = value.via_clip_mut() {
                visit(owner, ClipReference::Via, via);
            }
        });
    }
}
