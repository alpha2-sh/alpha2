//! Reading data sets at render time: the `data` value mode and picture corrections.
//!
//! A source-anchored set is read at the source time its reader shows (a clip's picture time
//! map, or a media node's start plus composition time), so a bound value follows the picture
//! through any time map.

use alpha2_model::{
    Clip, ClipId, DataAnchor, DataBinding, DataInterpolation, DataKind, DataSamples, DataSet,
    DataSetId, DataSpace, DataValues, DataVia, NodeId, Project, Vec2,
};
use alpha2_time::MediaTime;
use std::collections::HashMap;

/// Data sets and clips by id, with each set's displayed picture size.
#[derive(Clone, Debug, Default)]
pub struct DataIndex {
    sets: HashMap<DataSetId, usize>,
    clips: HashMap<ClipId, (usize, usize)>,
    /// Per set, the displayed size of its source's picture: `made_from`'s video stream, else the
    /// source fingerprint's.
    picture_sizes: Vec<Option<(u32, u32)>>,
}

impl DataIndex {
    /// Indexes `project`'s data sets and clips. Empty when the project has no data.
    #[must_use]
    pub fn new(project: &Project) -> Self {
        if project.data.is_empty() {
            return Self::default();
        }
        let sources = project
            .sources
            .iter()
            .map(|source| (&source.id, source))
            .collect::<HashMap<_, _>>();
        let video_size = |fingerprint: Option<&alpha2_model::MediaFingerprint>| {
            fingerprint
                .and_then(|fingerprint| fingerprint.streams.as_ref())
                .and_then(|streams| streams.video.as_ref())
                .filter(|video| video.width > 0 && video.height > 0)
                .map(|video| (video.width, video.height))
        };
        Self {
            sets: project
                .data
                .iter()
                .enumerate()
                .map(|(index, set)| (set.id.clone(), index))
                .collect(),
            clips: project
                .timeline
                .tracks
                .iter()
                .enumerate()
                .flat_map(|(track, entry)| {
                    entry
                        .clips
                        .iter()
                        .enumerate()
                        .map(move |(clip, value)| (value.id.clone(), (track, clip)))
                })
                .collect(),
            picture_sizes: project
                .data
                .iter()
                .map(|set| {
                    video_size(set.made_from.as_ref()).or_else(|| {
                        set.anchor
                            .source()
                            .and_then(|source| sources.get(source))
                            .and_then(|source| video_size(source.fingerprint.as_ref()))
                    })
                })
                .collect(),
        }
    }

    fn set<'p>(&self, project: &'p Project, id: &DataSetId) -> Option<(&'p DataSet, usize)> {
        let index = *self.sets.get(id)?;
        project.data.get(index).map(|set| (set, index))
    }

    fn clip<'p>(&self, project: &'p Project, id: &ClipId) -> Option<&'p Clip> {
        let (track, clip) = *self.clips.get(id)?;
        project.timeline.tracks.get(track)?.clips.get(clip)
    }
}

/// What a sampled value can read data through: the project's data sets and the clock of the
/// place being sampled.
#[derive(Clone, Copy)]
pub struct SampleContext<'a> {
    scope: Option<Scope<'a>>,
}

#[derive(Clone, Copy)]
struct Scope<'a> {
    project: &'a Project,
    index: &'a DataIndex,
    nodes: Option<Nodes<'a>>,
}

/// Where a composition-time value finds the `media` node it reads through.
#[derive(Clone, Copy)]
enum Nodes<'a> {
    /// A step of a compiled plan, whose inlined path names the node's siblings.
    Step(&'a crate::CompositionPlan, usize),
    /// A composition's own nodes.
    Composition(&'a alpha2_model::Composition),
}

impl Nodes<'_> {
    fn media_start(&self, via: &NodeId) -> Option<MediaTime> {
        match self {
            Self::Step(plan, step) => plan.media_start(*step, via),
            Self::Composition(composition) => {
                composition.nodes.iter().find_map(|node| match &node.op {
                    alpha2_model::NodeOp::Media { start, .. } if &node.id == via => Some(*start),
                    _ => None,
                })
            }
        }
    }

    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Step(a, i), Self::Step(b, j)) => std::ptr::eq(*a, *b) && i == j,
            (Self::Composition(a), Self::Composition(b)) => std::ptr::eq(*a, *b),
            _ => false,
        }
    }
}

/// Two contexts are equal when they read the same project, index and node starts.
impl PartialEq for SampleContext<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (&self.scope, &other.scope) {
            (None, None) => true,
            (Some(left), Some(right)) => {
                std::ptr::eq(left.project, right.project)
                    && std::ptr::eq(left.index, right.index)
                    && match (left.nodes, right.nodes) {
                        (None, None) => true,
                        (Some(a), Some(b)) => a.same(&b),
                        _ => false,
                    }
            }
            _ => false,
        }
    }
}

impl std::fmt::Debug for SampleContext<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SampleContext")
            .field("data", &self.scope.is_some())
            .finish()
    }
}

impl<'a> SampleContext<'a> {
    /// No data: a bound value evaluates as having no samples.
    pub const NONE: SampleContext<'static> = SampleContext { scope: None };

    /// Timeline-time places of `project`.
    #[must_use]
    pub const fn timeline(project: &'a Project, index: &'a DataIndex) -> Self {
        Self {
            scope: Some(Scope {
                project,
                index,
                nodes: None,
            }),
        }
    }

    /// Composition-time places of `composition`, read through its own `media` nodes.
    #[must_use]
    pub const fn composition(
        project: &'a Project,
        index: &'a DataIndex,
        composition: &'a alpha2_model::Composition,
    ) -> Self {
        Self {
            scope: Some(Scope {
                project,
                index,
                nodes: Some(Nodes::Composition(composition)),
            }),
        }
    }

    /// The same data read at step `step` of `plan`: its settings are in composition time and
    /// read through the `media` steps inlined beside it.
    #[must_use]
    pub fn for_step(&self, plan: &'a crate::CompositionPlan, step: usize) -> Self {
        Self {
            scope: self.scope.map(|scope| Scope {
                nodes: Some(Nodes::Step(plan, step)),
                ..scope
            }),
        }
    }

    /// The same data read at timeline-time places.
    #[must_use]
    pub fn for_timeline(&self) -> Self {
        Self {
            scope: self.scope.map(|scope| Scope {
                nodes: None,
                ..scope
            }),
        }
    }

    /// The canvas width over height, or 1 without a project.
    fn canvas_aspect(&self) -> f64 {
        self.scope.map_or(1.0, |scope| {
            f64::from(scope.project.settings.width) / f64::from(scope.project.settings.height)
        })
    }
}

/// A similarity of a clip's picture, sampled from a correction's data set.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Correction {
    /// Translation in fractions of the displayed picture's width and height.
    pub translate: Vec2,
    /// Clockwise rotation in degrees about the picture's center.
    pub rotation_degrees: f32,
    /// Uniform scale about the picture's center.
    pub scale: f32,
    /// The picture's size as a fraction of the canvas-shaped layer it is contain-fitted into.
    pub picture: Vec2,
}

impl Correction {
    /// Applies the correction to a point in the layer's normalized coordinates. `aspect` is the
    /// layer's width over height; rotation and scale act in pixel space about the center.
    #[must_use]
    pub fn apply(&self, point: Vec2, aspect: f64) -> Vec2 {
        let (sin, cos) = libm::sincos(f64::from(self.rotation_degrees).to_radians());
        let scale = f64::from(self.scale);
        let x = (f64::from(point.x) - 0.5) * aspect;
        let y = f64::from(point.y) - 0.5;
        let turned_x = scale * (cos * x - sin * y);
        let turned_y = scale * (sin * x + cos * y);
        Vec2::new(
            finite(
                0.5 + turned_x / aspect + f64::from(self.translate.x) * f64::from(self.picture.x),
            ),
            finite(0.5 + turned_y + f64::from(self.translate.y) * f64::from(self.picture.y)),
        )
    }
}

#[allow(clippy::cast_possible_truncation)]
fn finite(value: f64) -> f32 {
    if value.is_finite() {
        value.clamp(f64::from(f32::MIN), f64::from(f32::MAX)) as f32
    } else {
        0.0
    }
}

/// The displayed picture's size as a fraction of a canvas-shaped layer it is contain-fitted
/// into, as the renderer centres it.
#[must_use]
pub fn contained_picture(picture: (u32, u32), canvas_aspect: f64) -> Vec2 {
    let picture_aspect = f64::from(picture.0) / f64::from(picture.1);
    if picture_aspect >= canvas_aspect {
        Vec2::new(1.0, finite(canvas_aspect / picture_aspect))
    } else {
        Vec2::new(finite(picture_aspect / canvas_aspect), 1.0)
    }
}

/// A point normalized to the displayed picture, in the normalized coordinates of the layer the
/// picture is contain-fitted into.
fn picture_to_layer(point: Vec2, picture: Vec2) -> Vec2 {
    Vec2::new(
        finite(0.5 + (f64::from(point.x) - 0.5) * f64::from(picture.x)),
        finite(0.5 + (f64::from(point.y) - 0.5) * f64::from(picture.y)),
    )
}

/// The value of `channel` at `time`, `arity` numbers, with the data rules: `hold` takes the
/// sample at or before `time`, `linear` interpolates between two known neighbours, a lost
/// sample holds the last known value, and times outside the samples clamp. `None` when the
/// channel has no known value.
#[must_use]
pub fn sample_channel(
    samples: &DataSamples,
    channel: usize,
    arity: usize,
    time: MediaTime,
) -> Option<Vec<f32>> {
    let width = samples.channels.len().max(1) * arity;
    let at = |sample: usize| -> Option<&[Option<f32>]> {
        let start = sample * width + channel * arity;
        samples.values.get(start..start + arity)
    };
    let known = |sample: usize| at(sample).filter(|values| values[0].is_some());
    let collect =
        |values: &[Option<f32>]| values.iter().map(|value| value.unwrap_or(0.0)).collect();
    let next = samples.times.partition_point(|sample| *sample <= time);
    let Some(current) = next.checked_sub(1) else {
        // Before the first sample: the first known one.
        return (0..samples.times.len()).find_map(known).map(collect);
    };
    if let Some(left) = known(current) {
        if samples.interpolation == DataInterpolation::Linear
            && samples.times[current] < time
            && let Some(right) = known(next)
        {
            // The keyframe interpolation, so a baked binding evaluates alike.
            let amount = crate::animation::interpolation_amount(
                samples.times[current],
                samples.times[next],
                time,
            );
            return Some(
                left.iter()
                    .zip(right)
                    .map(|(from, to)| {
                        crate::animation::interpolate_f32(
                            from.unwrap_or(0.0),
                            to.unwrap_or(0.0),
                            amount,
                        )
                    })
                    .collect(),
            );
        }
        return Some(collect(left));
    }
    // Lost here: the last known value before, else the first known one after.
    (0..current)
        .rev()
        .find_map(known)
        .or_else(|| (next..samples.times.len()).find_map(known))
        .map(collect)
}

/// The anchor time a binding reads at when its place is sampled at `time`.
fn anchor_time(
    scope: &Scope<'_>,
    set: &DataSet,
    binding: &DataBinding,
    time: MediaTime,
) -> Option<MediaTime> {
    match (&set.anchor, &binding.via) {
        (DataAnchor::Timeline, _) => Some(time),
        (DataAnchor::Source { .. }, Some(DataVia::Clip(clip))) => {
            let clip = scope.index.clip(scope.project, clip)?;
            crate::picture_source_time(
                clip.timeline_range,
                clip.source_range,
                clip.time_map,
                scope.project.settings.frame_rate,
                time,
            )
        }
        (DataAnchor::Source { .. }, Some(DataVia::Node(node))) => {
            let start = scope.nodes?.media_start(node)?;
            Some(MediaTime(start.ticks().saturating_add(time.ticks())))
        }
        (DataAnchor::Source { .. }, None) => None,
    }
}

/// The kind of the data set `binding` follows, if it exists.
#[must_use]
pub fn bound_kind(binding: &DataBinding, context: &SampleContext<'_>) -> Option<DataKind> {
    let scope = context.scope?;
    scope
        .index
        .set(scope.project, &binding.data)
        .map(|(set, _)| set.kind())
}

/// A scalar binding's value at `time`, remapped; `None` without a value.
#[must_use]
pub fn bound_scalar(
    binding: &DataBinding,
    time: MediaTime,
    context: &SampleContext<'_>,
) -> Option<f32> {
    let scope = context.scope?;
    let (set, _) = scope.index.set(scope.project, &binding.data)?;
    let DataValues::Scalars(samples) = &set.values else {
        return None;
    };
    let at = anchor_time(&scope, set, binding, time)?;
    let value = *sample_channel(samples, samples.channel(&binding.channel)?, 1, at)?.first()?;
    Some(binding.remap.map_or(value, |remap| remap.apply(value)))
}

/// A two-component binding's value at `time`: a point in its space plus its offset, or a scalar
/// in both components. `None` without a value.
#[must_use]
pub fn bound_vec2(
    binding: &DataBinding,
    time: MediaTime,
    context: &SampleContext<'_>,
) -> Option<Vec2> {
    let scope = context.scope?;
    let (set, index) = scope.index.set(scope.project, &binding.data)?;
    match &set.values {
        DataValues::Scalars(_) => {
            bound_scalar(binding, time, context).map(|value| Vec2::new(value, value))
        }
        DataValues::Points(samples) => {
            let at = anchor_time(&scope, set, binding, time)?;
            let values = sample_channel(samples, samples.channel(&binding.channel)?, 2, at)?;
            let point = Vec2::new(values[0], values[1]);
            let placed = match (&set.anchor, binding.space) {
                (DataAnchor::Timeline, _) | (_, None) => point,
                (DataAnchor::Source { .. }, Some(space)) => {
                    let aspect = context.canvas_aspect();
                    let picture = scope.index.picture_sizes[index]
                        .map_or(Vec2::new(1.0, 1.0), |size| contained_picture(size, aspect));
                    let layer = picture_to_layer(point, picture);
                    match (space, &binding.via) {
                        (DataSpace::Canvas, Some(DataVia::Clip(clip))) => {
                            let clip = scope.index.clip(scope.project, clip)?;
                            layer_to_canvas(clip, layer, time, context)
                        }
                        _ => layer,
                    }
                }
            };
            let [x, y] = binding.offset.unwrap_or_default();
            Some(Vec2::new(placed.x + x, placed.y + y))
        }
        DataValues::Similarity(_) | DataValues::Words(_) | DataValues::Intervals(_) => None,
    }
}

/// A point of `clip`'s layer at `time` in normalized canvas coordinates: through its correction,
/// then its evaluated visual, as the compositor places the layer.
fn layer_to_canvas(clip: &Clip, layer: Vec2, time: MediaTime, context: &SampleContext<'_>) -> Vec2 {
    let aspect = context.canvas_aspect();
    let corrected = correction_at(clip, time, context)
        .map_or(layer, |correction| correction.apply(layer, aspect));
    let visual = crate::sample_visual(&clip.visual, time, aspect, context);
    let (sin, cos) = libm::sincos(f64::from(visual.rotation_degrees).to_radians());
    let x =
        (f64::from(corrected.x) - f64::from(visual.anchor.x)) * f64::from(visual.scale.x) * aspect;
    let y = (f64::from(corrected.y) - f64::from(visual.anchor.y)) * f64::from(visual.scale.y);
    Vec2::new(
        finite(f64::from(visual.position.x) + (cos * x - sin * y) / aspect),
        finite(f64::from(visual.position.y) + sin * x + cos * y),
    )
}

/// `clip`'s correction at timeline `time`, if it has one with a value there.
#[must_use]
pub fn correction_at(
    clip: &Clip,
    time: MediaTime,
    context: &SampleContext<'_>,
) -> Option<Correction> {
    clip_correction(
        &clip.visual,
        (clip.timeline_range, clip.source_range, clip.time_map),
        time,
        context,
    )
}

/// The correction `visual` names at timeline `time`, for a clip placed by `placement`
/// (timeline range, source range, time map).
pub(crate) fn clip_correction(
    visual: &alpha2_model::VisualProperties,
    placement: (
        alpha2_time::TimeRange,
        alpha2_time::TimeRange,
        alpha2_model::TimeMap,
    ),
    time: MediaTime,
    context: &SampleContext<'_>,
) -> Option<Correction> {
    let reference = visual.correction.as_ref()?;
    let scope = context.scope?;
    let (set, index) = scope.index.set(scope.project, &reference.data)?;
    let DataValues::Similarity(samples) = &set.values else {
        return None;
    };
    let (timeline_range, source_range, time_map) = placement;
    let at = crate::picture_source_time(
        timeline_range,
        source_range,
        time_map,
        scope.project.settings.frame_rate,
        time,
    )?;
    let values = sample_channel(samples, 0, DataKind::Similarity.arity().unwrap_or(4), at)?;
    let picture = scope.index.picture_sizes[index].map_or(Vec2::new(1.0, 1.0), |size| {
        contained_picture(size, context.canvas_aspect())
    });
    Some(Correction {
        translate: Vec2::new(values[0], values[1]),
        rotation_degrees: values[2],
        scale: values[3],
        picture,
    })
}

/// The first tick after `after` at which a timeline-time binding can read another sample: a
/// sample time passing under the reader's time map, or a loop pass starting. Between two such
/// ticks a `hold` binding is constant and a `linear` one changes linearly with timeline time.
/// `None` when the value never changes again.
#[must_use]
pub fn next_data_change(
    binding: &DataBinding,
    after: MediaTime,
    context: &SampleContext<'_>,
) -> Option<MediaTime> {
    let scope = context.scope?;
    let (set, _) = scope.index.set(scope.project, &binding.data)?;
    let samples = set.values.samples()?;
    let times = &samples.times;
    match (&set.anchor, &binding.via) {
        (DataAnchor::Timeline, _) => times
            .get(times.partition_point(|time| *time <= after))
            .copied(),
        (DataAnchor::Source { .. }, Some(DataVia::Clip(clip))) => {
            let clip = scope.index.clip(scope.project, clip)?;
            let rate = scope.project.settings.frame_rate;
            let (start, end) = (
                clip.timeline_range.start().ticks(),
                clip.timeline_range.end().ticks(),
            );
            if after.ticks() >= end {
                return None;
            }
            if after.ticks() < start {
                return Some(clip.timeline_range.start());
            }
            let shown = |tick: i64| {
                crate::picture_source_time(
                    clip.timeline_range,
                    clip.source_range,
                    clip.time_map,
                    rate,
                    MediaTime(tick),
                )
                .map_or(i64::MIN, MediaTime::ticks)
            };
            // The first tick past `after` whose sample index differs from the current one.
            let index_at = |tick: i64| times.partition_point(|time| time.ticks() <= shown(tick));
            let current = index_at(after.ticks());
            let (mut low, mut high) = (after.ticks() + 1, end);
            // The sample index is monotone within a stretch; a loop pass boundary is a change too.
            if let alpha2_model::TimeMap::Loop { .. } = clip.time_map {
                let length = clip.source_range.duration()?.ticks();
                let pass_end = start + ((after.ticks() - start) / length + 1) * length;
                high = high.min(pass_end);
                if index_at(high.min(end - 1)) == current && high < end {
                    return Some(MediaTime(high));
                }
            }
            if index_at(high - 1) == current {
                return (high < end).then_some(MediaTime(high));
            }
            while low < high {
                let middle = low + (high - low) / 2;
                if index_at(middle) == current {
                    low = middle + 1;
                } else {
                    high = middle;
                }
            }
            Some(MediaTime(low))
        }
        (DataAnchor::Source { .. }, _) => None,
    }
}
