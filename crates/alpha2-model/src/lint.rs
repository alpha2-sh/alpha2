//! Non-fatal diagnostics: what a valid project probably does not mean to do.
//!
//! [`Project::lint`] never refuses a project. Every check is conservative: an
//! issue it reports is certain, and something it misses is not an error.

use crate::{
    Animated, AnimatedPoint, ClipId, CompositionId, Generator, MediaKind, NodeId, Paint, Project,
    ShapeGeometry, ShapeItem, TrackId, TrackKind,
};
use alpha2_media::SourceId;
use alpha2_time::{FrameRate, MediaTime, TICKS_PER_SECOND, TimeRange};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// Something in a project that is valid but probably not meant.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LintIssue {
    /// A keyframe that no time its owner is shown at depends on, checked in
    /// the owner's own time: a clip's and a transition's in timeline time, a
    /// composition node's in composition time, a generated source's tree in
    /// its picture time.
    UnreachedKeyframe {
        /// What the keyframe animates.
        owner: LintOwner,
        /// The animated property, as a path such as `visual.opacity` or
        /// `masks[0].feather` (a clip), `effect.params.radius` (a composition
        /// node) or `items[0].transform.opacity` (a generated source).
        property: String,
        /// The keyframe's time.
        time: MediaTime,
    },
    /// A clip wholly covered, for its whole time, by an opaque full-frame
    /// clip on a later track. A matte clip is never reported, as hidden or
    /// hiding: it is never composited itself.
    HiddenClip {
        /// The covered clip.
        clip: ClipId,
        /// The clip that covers it.
        by: ClipId,
    },
    /// A transition no project frame shows between its two clips: shorter
    /// than a frame, or about a frame long and starting between frames, so
    /// each frame shows one clip or the other, never a mix.
    ShortTransition {
        /// Its track.
        track: TrackId,
        /// Its first clip.
        from_clip: ClipId,
        /// Its second clip.
        to_clip: ClipId,
    },
    /// A boundary that is not where a project frame starts: the change it
    /// marks shows at the next frame. A frame starts at the tick its exact
    /// start rounds down to, as the engine renders it.
    OffFrameBoundary {
        /// Whose boundary.
        owner: LintOwner,
        /// Which end.
        edge: LintEdge,
        /// The boundary's time.
        time: MediaTime,
    },
    /// A source no render reads: no clip shows it, and no `media` node of a
    /// composition a clip shows reads it.
    UnusedSource {
        /// The source.
        source: SourceId,
    },
    /// A high-dynamic-range source in a project whose color pipeline refuses
    /// HDR, so rendering it fails.
    HdrSource {
        /// The source.
        source: SourceId,
    },
    /// A path's subpath without vertices, which draws nothing.
    EmptySubpath {
        /// The shapes generator.
        owner: ShapeLintOwner,
        /// The empty subpath, like `items[0].path.subpaths[1]`, or in a
        /// layout `root.children[0].shape.items[0].path.subpaths[1]`.
        at: String,
    },
    /// A fill or stroke with no geometry before it in its group, which
    /// paints nothing.
    PaintWithoutGeometry {
        /// The shapes generator.
        owner: ShapeLintOwner,
        /// The paint item, like `items[1].items[0]`.
        at: String,
    },
    /// A text animator that sets no property, which changes nothing.
    EmptyTextAnimator {
        /// What holds the text.
        owner: LintOwner,
        /// The animator, like `style.animators[0]`, or in a layout
        /// `root.children[1].text_animators[0]`.
        at: String,
    },
}

/// The shapes or layout generator a shape [`LintIssue`] is about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ShapeLintOwner {
    /// A generated source's.
    Source {
        /// The source.
        source: SourceId,
    },
    /// A composition's generator node's.
    Node {
        /// The composition.
        composition: CompositionId,
        /// The node.
        node: NodeId,
    },
}

/// What a [`LintIssue`] is about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LintOwner {
    /// A clip.
    Clip {
        /// The clip.
        clip: ClipId,
    },
    /// A transition.
    Transition {
        /// Its track.
        track: TrackId,
        /// Its first clip.
        from_clip: ClipId,
        /// Its second clip.
        to_clip: ClipId,
    },
    /// A caption cue.
    Cue {
        /// Its caption track.
        track: TrackId,
        /// Its position among the track's cues.
        index: usize,
    },
    /// A composition's node, whose settings are keyed in composition time.
    CompositionNode {
        /// The composition.
        composition: CompositionId,
        /// The node.
        node: NodeId,
    },
    /// A generated source, whose shapes or layout tree is keyed in its
    /// picture's time.
    Source {
        /// The source.
        source: SourceId,
    },
    /// A caption track, whose style's animators are keyed in each cue's
    /// own time, from its start.
    CaptionTrack {
        /// The track.
        track: TrackId,
    },
}

/// The end of a range a [`LintIssue`] is about.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LintEdge {
    /// The start.
    Start,
    /// The end.
    End,
}

impl Project {
    /// Diagnostics that do not stop the project rendering, in a fixed order:
    /// unused sources, then shapes generators (sources', then composition
    /// nodes'), then HDR sources, then tracks in order and each track's clips,
    /// transitions, and cues in order, then compositions' nodes in order, then
    /// generated sources' trees in order.
    #[must_use]
    pub fn lint(&self) -> Vec<LintIssue> {
        let mut issues = Vec::new();
        let used = self
            .used_sources()
            .into_iter()
            .map(|source| &source.id)
            .collect::<std::collections::HashSet<_>>();
        for source in &self.sources {
            if !used.contains(&source.id) {
                issues.push(LintIssue::UnusedSource {
                    source: source.id.clone(),
                });
            }
        }
        self.lint_shapes(&mut issues);
        let refused = self.settings.color.hdr == crate::HdrSources::Reject;
        for source in self.hdr_sources().into_iter().filter(|_| refused) {
            issues.push(LintIssue::HdrSource {
                source: source.clone(),
            });
        }
        let mattes = self
            .timeline
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .filter_map(|clip| clip.matte.as_ref().map(|matte| &matte.source))
            .collect::<HashSet<_>>();
        let grid = FrameGrid::new(self);
        let covers = self.covering_clips(&mattes);
        for (order, track) in self.timeline.tracks.iter().enumerate() {
            for clip in &track.clips {
                let owner = LintOwner::Clip {
                    clip: clip.id.clone(),
                };
                grid.check(&owner, clip.timeline_range, &mut issues);
                let mut keyframes = Keyframes {
                    owner: &owner,
                    shown: &[span(clip.timeline_range)],
                    issues: &mut issues,
                };
                keyframes.check_clip(clip);
                if track.kind == TrackKind::Video
                    && !mattes.contains(&clip.id)
                    && let Some(by) = covering_clip(&covers, order, clip.timeline_range)
                {
                    issues.push(LintIssue::HiddenClip {
                        clip: clip.id.clone(),
                        by: by.clone(),
                    });
                }
            }
            for transition in &track.transitions {
                let owner = LintOwner::Transition {
                    track: track.id.clone(),
                    from_clip: transition.from_clip.clone(),
                    to_clip: transition.to_clip.clone(),
                };
                if !grid.shows_mixed(transition.timeline_range) {
                    issues.push(LintIssue::ShortTransition {
                        track: track.id.clone(),
                        from_clip: transition.from_clip.clone(),
                        to_clip: transition.to_clip.clone(),
                    });
                }
                grid.check(&owner, transition.timeline_range, &mut issues);
                let mut keyframes = Keyframes {
                    owner: &owner,
                    shown: &[span(transition.timeline_range)],
                    issues: &mut issues,
                };
                for (name, value) in &transition.effect.params {
                    keyframes.check(&format!("effect.params.{name}"), value);
                }
            }
            for (index, cue) in track
                .captions
                .iter()
                .flat_map(|captions| captions.cues.iter())
                .enumerate()
            {
                let owner = LintOwner::Cue {
                    track: track.id.clone(),
                    index,
                };
                grid.check(&owner, cue.timeline_range, &mut issues);
            }
            if let Some(captions) = &track.captions {
                lint_caption_animators(&track.id, captions, &mut issues);
            }
        }
        self.lint_keyframes_in_other_times(&mut issues);
        self.lint_text_animators(&mut issues);
        issues
    }

    /// Every text animator that sets nothing: of text generators (sources'
    /// and composition nodes') and of layout text nodes.
    fn lint_text_animators(&self, issues: &mut Vec<LintIssue>) {
        let lint =
            |owner: &LintOwner, generator: &Generator, at: &str, issues: &mut Vec<LintIssue>| {
                match generator {
                    Generator::Text { style, .. } => {
                        empty_animators(
                            owner,
                            &style.animators,
                            &format!("{at}style.animators"),
                            issues,
                        );
                    }
                    Generator::Layout { root } => root.walk(|path, node| {
                        empty_animators(
                            owner,
                            &node.text_animators,
                            &format!("{at}{path}.text_animators"),
                            issues,
                        );
                    }),
                    Generator::Fill { .. } | Generator::Shapes { .. } => {}
                }
            };
        for source in &self.sources {
            if let Some(generator) = &source.generator {
                let owner = LintOwner::Source {
                    source: source.id.clone(),
                };
                lint(&owner, generator, "", issues);
            }
        }
        for composition in &self.compositions {
            for node in &composition.nodes {
                if let crate::NodeOp::Generator { generator } = &node.op {
                    let owner = LintOwner::CompositionNode {
                        composition: composition.id.clone(),
                        node: node.id.clone(),
                    };
                    lint(&owner, generator, "generator.", issues);
                }
            }
        }
    }

    /// Keyframes no shown time reaches in the two other time bases:
    /// composition nodes' settings (and generator nodes' trees) in
    /// composition time, generated sources' trees in picture time. A
    /// composition or source no clip shows, or shown at times that cannot be
    /// bounded, is not checked.
    fn lint_keyframes_in_other_times(&self, issues: &mut Vec<LintIssue>) {
        let shown = Shown::of(self);
        for composition in &self.compositions {
            let Some(spans) = shown.compositions.get(&composition.id) else {
                continue;
            };
            for node in &composition.nodes {
                let owner = LintOwner::CompositionNode {
                    composition: composition.id.clone(),
                    node: node.id.clone(),
                };
                let mut keyframes = Keyframes {
                    owner: &owner,
                    shown: spans,
                    issues,
                };
                node_keyed(&node.op, &mut |property, times| {
                    keyframes.check_times(property, times);
                });
            }
        }
        for source in &self.sources {
            let Some(spans) = shown.sources.get(&source.id) else {
                continue;
            };
            let owner = LintOwner::Source {
                source: source.id.clone(),
            };
            let mut keyframes = Keyframes {
                owner: &owner,
                shown: spans,
                issues,
            };
            let mut check = |property: &str, times: &[MediaTime]| {
                keyframes.check_times(property, times);
            };
            match &source.generator {
                Some(Generator::Shapes { items }) => {
                    crate::shapes::for_each_keyed_item(items, "items", &mut check);
                }
                Some(Generator::Layout { root }) => root.for_each_keyed("root", &mut check),
                Some(Generator::Text { style, .. }) => {
                    crate::text_animation::for_each_keyed_animator(
                        &style.animators,
                        "style.animators",
                        &mut check,
                    );
                }
                _ => {}
            }
        }
    }

    /// Every shapes generator's and layout shape node's empty subpaths and
    /// paints without geometry: sources' first, then composition nodes', in
    /// order.
    fn lint_shapes(&self, issues: &mut Vec<LintIssue>) {
        let lint = |owner: &ShapeLintOwner, generator: &Generator, issues: &mut Vec<LintIssue>| {
            match generator {
                Generator::Shapes { items } => {
                    lint_shape_items(owner, items, "items", issues);
                }
                Generator::Layout { root } => root.walk(|at, node| {
                    if let Some(shape) = &node.shape {
                        lint_shape_items(owner, &shape.items, &format!("{at}.shape.items"), issues);
                    }
                }),
                Generator::Fill { .. } | Generator::Text { .. } => {}
            }
        };
        for source in &self.sources {
            if let Some(generator) = &source.generator {
                let owner = ShapeLintOwner::Source {
                    source: source.id.clone(),
                };
                lint(&owner, generator, issues);
            }
        }
        for composition in &self.compositions {
            for node in &composition.nodes {
                if let crate::NodeOp::Generator { generator } = &node.op {
                    let owner = ShapeLintOwner::Node {
                        composition: composition.id.clone(),
                        node: node.id.clone(),
                    };
                    lint(&owner, generator, issues);
                }
            }
        }
    }

    /// The clips on video tracks that certainly cover the whole frame,
    /// opaquely, outside their transitions: composited (not a matte), and
    /// [`is_opaque_full_frame`]. In track order, then clip order.
    fn covering_clips<'a>(&'a self, mattes: &HashSet<&ClipId>) -> Vec<Cover<'a>> {
        let sources = self
            .sources
            .iter()
            .map(|source| (&source.id, source))
            .collect::<HashMap<_, _>>();
        let mut covers = Vec::new();
        for (order, track) in self.timeline.tracks.iter().enumerate() {
            if track.kind != TrackKind::Video {
                continue;
            }
            for clip in &track.clips {
                if mattes.contains(&clip.id)
                    || !sources
                        .get(&clip.source_id)
                        .is_some_and(|source| is_opaque_full_frame(source, clip))
                {
                    continue;
                }
                let mixed = track
                    .transitions
                    .iter()
                    .filter(|transition| {
                        transition.from_clip == clip.id || transition.to_clip == clip.id
                    })
                    .map(|transition| transition.timeline_range)
                    .collect();
                covers.push(Cover { order, clip, mixed });
            }
        }
        covers
    }
}

/// A clip that covers the whole frame opaquely, except while it is in one
/// of its transitions.
struct Cover<'a> {
    /// Its track's position.
    order: usize,
    clip: &'a crate::Clip,
    /// Its transitions' ranges, where it is mixed with another clip.
    mixed: Vec<TimeRange>,
}

/// A clip on a track after `order` that covers the frame for all of `range`.
/// Only the few clips that cover the frame at all are looked at.
fn covering_clip<'a>(covers: &[Cover<'a>], order: usize, range: TimeRange) -> Option<&'a ClipId> {
    covers
        .iter()
        .filter(|cover| cover.order > order)
        .find(|cover| {
            cover.clip.timeline_range.start() <= range.start()
                && cover.clip.timeline_range.end() >= range.end()
                && !cover
                    .mixed
                    .iter()
                    .any(|mixed| mixed.intersection(range).is_some())
        })
        .map(|cover| &cover.clip.id)
}

/// Whether a clip draws an opaque fill over the whole frame: a generated
/// fill of opaque paint, shown untransformed, unblended, unmasked, with no
/// composition.
fn is_opaque_full_frame(source: &crate::MediaSource, clip: &crate::Clip) -> bool {
    let opaque = source.kind == MediaKind::Generated
        && matches!(&source.generator, Some(Generator::Fill { paint }) if paint_is_opaque(paint));
    opaque
        && clip.visual == crate::VisualProperties::default()
        && clip.blend_mode == crate::ClipBlendMode::Normal
        && clip.composition.is_none()
        && clip.masks.is_empty()
        && clip.matte.is_none()
}

fn paint_is_opaque(paint: &Paint) -> bool {
    match paint {
        Paint::Solid { color } => color.alpha >= 1.0,
        Paint::LinearGradient { stops, .. }
        | Paint::RadialGradient { stops, .. }
        | Paint::ConicGradient { stops, .. } => stops.iter().all(|stop| stop.color.alpha >= 1.0),
    }
}

/// The project's frames, starting where the engine renders them: frame `k`
/// at [`FrameRate::frame_start`], its exact start rounded down to a tick.
struct FrameGrid {
    rate: FrameRate,
}

impl FrameGrid {
    fn new(project: &Project) -> Self {
        Self {
            rate: project.settings.frame_rate,
        }
    }

    fn on_grid(&self, time: MediaTime) -> bool {
        self.rate
            .frame_at_or_before(time)
            .and_then(|frame| self.rate.frame_start(frame))
            == Some(time)
    }

    /// Whether a frame shows a transition over `range` between its two
    /// clips, as the evaluator's progress `clamp((t - start + F/2) / D, 0, 1)`
    /// below 1 at a frame start `t` in the range. Progress grows with `t`, so
    /// the first frame start in the range decides. Unsure is shown.
    fn shows_mixed(&self, range: TimeRange) -> bool {
        let first = self
            .rate
            .frame_at_or_before(range.start())
            .and_then(|frame| {
                let start = self.rate.frame_start(frame)?;
                if start == range.start() {
                    Some(start)
                } else {
                    self.rate.frame_start(frame.checked_add(1)?)
                }
            });
        let Some(first) = first else {
            return true;
        };
        let num = i128::from(self.rate.numerator());
        let elapsed = i128::from(first.ticks()) - i128::from(range.start().ticks());
        let duration = i128::from(range.end().ticks()) - i128::from(range.start().ticks());
        // Scaled by 2 * num, as the evaluator's `centred_progress` is.
        first < range.end()
            && 2 * num * elapsed
                + i128::from(TICKS_PER_SECOND) * i128::from(self.rate.denominator())
                < 2 * num * duration
    }

    fn check(&self, owner: &LintOwner, range: TimeRange, issues: &mut Vec<LintIssue>) {
        for (edge, time) in [
            (LintEdge::Start, range.start()),
            (LintEdge::End, range.end()),
        ] {
            if !self.on_grid(time) {
                issues.push(LintIssue::OffFrameBoundary {
                    owner: owner.clone(),
                    edge,
                    time,
                });
            }
        }
    }
}

/// Finds keyframes no time its owner is shown at depends on: `shown` holds
/// every span of ticks, first and last, it may be shown at.
struct Keyframes<'a> {
    owner: &'a LintOwner,
    shown: &'a [(i64, i64)],
    issues: &'a mut Vec<LintIssue>,
}

/// The ticks of `range`, first and last.
fn span(range: TimeRange) -> (i64, i64) {
    (range.start().ticks(), range.end().ticks() - 1)
}

impl Keyframes<'_> {
    fn check_clip(&mut self, clip: &crate::Clip) {
        let visual = &clip.visual;
        self.check_point("visual.position", &visual.position);
        self.check("visual.scale", &visual.scale);
        self.check("visual.rotation_degrees", &visual.rotation_degrees);
        self.check("visual.opacity", &visual.opacity);
        for (index, mask) in clip.masks.iter().enumerate() {
            self.check_geometry(&format!("masks[{index}].shape"), &mask.shape);
            self.check(&format!("masks[{index}].feather"), &mask.feather);
        }
        self.check("audio.gain", &clip.audio.gain);
        self.check("audio.pan", &clip.audio.pan);
    }

    fn check_geometry(&mut self, at: &str, geometry: &ShapeGeometry) {
        geometry.for_each_keyed(at, &mut |property, times| self.check_times(property, times));
    }

    fn check_point(&mut self, property: &str, animated: &AnimatedPoint) {
        if let AnimatedPoint::Keyframes { keyframes } = animated {
            let times: Vec<MediaTime> = keyframes.iter().map(|keyframe| keyframe.time).collect();
            self.check_times(property, &times);
        }
    }

    fn check<T>(&mut self, property: &str, animated: &Animated<T>) {
        if let Animated::Keyframes { keyframes } = animated {
            let times: Vec<MediaTime> = keyframes.iter().map(|keyframe| keyframe.time).collect();
            self.check_times(property, &times);
        }
    }

    /// Keyframe `i` shapes values only strictly between its neighbours'
    /// times (the first one back to the start of time, the last one on to
    /// its end); outside every shown tick, it has no effect.
    fn check_times(&mut self, property: &str, times: &[MediaTime]) {
        for (index, time) in times.iter().enumerate() {
            let after = index.checked_sub(1).map_or(i64::MIN, |previous| {
                times[previous].ticks().saturating_add(1)
            });
            let before = times
                .get(index + 1)
                .map_or(i64::MAX, |next| next.ticks().saturating_sub(1));
            let reached = self
                .shown
                .iter()
                .any(|&(first, last)| after.max(first) <= before.min(last));
            if !reached {
                self.issues.push(LintIssue::UnreachedKeyframe {
                    owner: self.owner.clone(),
                    property: property.to_owned(),
                    time: *time,
                });
            }
        }
    }
}

/// Every span of ticks, first and last, each composition and each generated
/// or composition source may be shown at. Composition time is a clip's
/// [`crate::ClipComposition`] mapping of its timeline range, a composition
/// source's picture time, or the time of a composition that draws it as a
/// group; picture time is a clip's source range through its time map (a
/// freeze's instant), or a media node's `start` plus its composition's
/// time. The spans hold every time shown and may hold more, so a keyframe
/// outside them all is certainly unreached. One the spans cannot bound (an
/// unrepresentable time) is left out, and so is everything it shows.
#[derive(Default)]
struct Shown<'a> {
    compositions: HashMap<&'a CompositionId, Vec<(i64, i64)>>,
    sources: HashMap<&'a SourceId, Vec<(i64, i64)>>,
}

impl<'a> Shown<'a> {
    fn of(project: &'a Project) -> Self {
        let (base, unbounded) = Self::clips(project);
        let mut shown = base.propagated(project);
        shown.drop_unbounded(project, unbounded);
        shown
    }

    /// What the clips show directly: their sources at the picture times
    /// their time maps reach, and their compositions at the composition
    /// times their ranges map to; and the compositions a clip maps to times
    /// that cannot be represented.
    fn clips(project: &'a Project) -> (Self, Vec<&'a CompositionId>) {
        let mut base = Self::default();
        let mut unbounded = Vec::new();
        for clip in project
            .timeline
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
        {
            let picture = match clip.time_map {
                crate::TimeMap::Freeze { source_time } => {
                    (source_time.ticks(), source_time.ticks())
                }
                crate::TimeMap::Forward | crate::TimeMap::Reverse | crate::TimeMap::Loop { .. } => {
                    span(clip.source_range)
                }
            };
            base.sources
                .entry(&clip.source_id)
                .or_default()
                .push(picture);
            if let Some(composition) = &clip.composition {
                let start = clip.timeline_range.start();
                let last = MediaTime(clip.timeline_range.end().ticks() - 1);
                match (
                    composition.local_time(start, start),
                    composition.local_time(start, last),
                ) {
                    (Some(first), Some(last)) => base
                        .compositions
                        .entry(&composition.id)
                        .or_default()
                        .push((first.ticks(), last.ticks())),
                    _ => unbounded.push(&composition.id),
                }
            }
        }
        (base, unbounded)
    }

    /// `self` (what the clips show) and what that shows in turn: a
    /// composition source's composition at its picture times, a group's
    /// composition in its parent's time, a media node's source at its start
    /// plus its composition's time. Each pass adds one more level; the graph
    /// is acyclic, so as many passes as there are compositions and sources
    /// reach every level.
    fn propagated(self, project: &'a Project) -> Self {
        let mut shown = Self {
            compositions: self.compositions.clone(),
            sources: self.sources.clone(),
        };
        shown.normalize();
        for _ in 0..=project.compositions.len() + project.sources.len() {
            let mut next = Self {
                compositions: self.compositions.clone(),
                sources: self.sources.clone(),
            };
            for source in &project.sources {
                if let (Some(composition), Some(spans)) =
                    (&source.composition, shown.sources.get(&source.id))
                {
                    next.compositions
                        .entry(composition)
                        .or_default()
                        .extend(spans);
                }
            }
            for composition in &project.compositions {
                let Some(spans) = shown.compositions.get(&composition.id) else {
                    continue;
                };
                for node in &composition.nodes {
                    match &node.op {
                        crate::NodeOp::Group { composition, .. } => {
                            next.compositions
                                .entry(composition)
                                .or_default()
                                .extend(spans);
                        }
                        crate::NodeOp::Media { source_id, start } => {
                            next.sources
                                .entry(source_id)
                                .or_default()
                                .extend(spans.iter().map(|&(first, last)| {
                                    (
                                        first.saturating_add(start.ticks()),
                                        last.saturating_add(start.ticks()),
                                    )
                                }));
                        }
                        _ => {}
                    }
                }
            }
            next.normalize();
            if next.compositions == shown.compositions && next.sources == shown.sources {
                break;
            }
            shown = next;
        }
        shown
    }

    /// Leaves out the compositions in `unbounded`, and what they draw: their
    /// groups' compositions and their media nodes' sources, unchecked.
    fn drop_unbounded(&mut self, project: &'a Project, mut unbounded: Vec<&'a CompositionId>) {
        while let Some(id) = unbounded.pop() {
            self.compositions.remove(id);
            let Some(composition) = project.compositions.iter().find(|held| held.id == *id) else {
                continue;
            };
            for node in &composition.nodes {
                match &node.op {
                    crate::NodeOp::Group { composition, .. } => {
                        if self.compositions.contains_key(composition) {
                            unbounded.push(composition);
                        }
                    }
                    crate::NodeOp::Media { source_id, .. } => {
                        self.sources.remove(source_id);
                        if let Some(inner) = project
                            .sources
                            .iter()
                            .find(|source| source.id == *source_id)
                            .and_then(|source| source.composition.as_ref())
                            && self.compositions.contains_key(inner)
                        {
                            unbounded.push(inner);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Each list sorted, and spans that meet or touch merged.
    fn normalize(&mut self) {
        fn merge(spans: &mut Vec<(i64, i64)>) {
            spans.sort_unstable();
            let mut merged: Vec<(i64, i64)> = Vec::with_capacity(spans.len());
            for &(first, last) in spans.iter() {
                match merged.last_mut() {
                    Some(held) if first <= held.1.saturating_add(1) => held.1 = held.1.max(last),
                    _ => merged.push((first, last)),
                }
            }
            *spans = merged;
        }
        self.compositions.values_mut().for_each(merge);
        self.sources.values_mut().for_each(merge);
    }
}

/// Calls `visit` with each of a node's animated settings that has keyframes,
/// by its path in the node (`effect.params.radius`, `transform.opacity`,
/// `generator.items[0].transform.scale`), and their times.
fn node_keyed(op: &crate::NodeOp, visit: &mut impl FnMut(&str, &[MediaTime])) {
    use crate::animation::{visit_keyed, visit_keyed_point};
    let params = |effect: &crate::ClipEffect, visit: &mut dyn FnMut(&str, &[MediaTime])| {
        for (name, value) in &effect.params {
            visit_keyed(&format!("effect.params.{name}"), value, &mut |at, times| {
                visit(at, times);
            });
        }
    };
    match op {
        crate::NodeOp::Effect { effect, .. } => params(effect, visit),
        crate::NodeOp::Transition {
            effect, progress, ..
        } => {
            params(effect, visit);
            visit_keyed("progress", progress, visit);
        }
        crate::NodeOp::Transform { transform, .. } => {
            visit_keyed_point("transform.position", &transform.position, visit);
            visit_keyed("transform.scale", &transform.scale, visit);
            visit_keyed(
                "transform.rotation_degrees",
                &transform.rotation_degrees,
                visit,
            );
            visit_keyed("transform.opacity", &transform.opacity, visit);
        }
        crate::NodeOp::Merge { layers } => {
            for (index, layer) in layers.iter().enumerate() {
                visit_keyed(&format!("layers[{index}].opacity"), &layer.opacity, visit);
            }
        }
        crate::NodeOp::ShapeMask { masks, .. } => {
            for (index, mask) in masks.iter().enumerate() {
                mask.shape
                    .for_each_keyed(&format!("masks[{index}].shape"), visit);
                visit_keyed(&format!("masks[{index}].feather"), &mask.feather, visit);
            }
        }
        crate::NodeOp::Group { controls, .. } => {
            for (name, value) in controls {
                visit_keyed(&format!("controls.{name}"), value, visit);
            }
        }
        crate::NodeOp::Generator { generator } => match generator {
            Generator::Shapes { items } => {
                crate::shapes::for_each_keyed_item(items, "generator.items", visit);
            }
            Generator::Layout { root } => root.for_each_keyed("generator.root", visit),
            Generator::Text { style, .. } => crate::text_animation::for_each_keyed_animator(
                &style.animators,
                "generator.style.animators",
                visit,
            ),
            Generator::Fill { .. } => {}
        },
        crate::NodeOp::Clip {}
        | crate::NodeOp::Media { .. }
        | crate::NodeOp::Matte { .. }
        | crate::NodeOp::GroupInput { .. } => {}
    }
}

/// Reports a content tree's empty subpaths and its paints with nothing
/// before them to paint, in document order. Returns whether the items hold
/// any geometry, so an enclosing group knows its paints have some.
fn lint_shape_items(
    owner: &ShapeLintOwner,
    items: &[ShapeItem],
    at: &str,
    issues: &mut Vec<LintIssue>,
) -> bool {
    let mut geometry = false;
    for (index, item) in items.iter().enumerate() {
        let at = format!("{at}[{index}]");
        match item {
            ShapeItem::Group { items, .. } => {
                geometry |= lint_shape_items(owner, items, &format!("{at}.items"), issues);
            }
            ShapeItem::Rectangle(_) | ShapeItem::Ellipse(_) | ShapeItem::Polystar(_) => {
                geometry = true;
            }
            ShapeItem::Path(path) => {
                geometry = true;
                let paths: Vec<&crate::Path> = match &path.path {
                    Animated::Constant { value } => vec![value],
                    Animated::Keyframes { keyframes } => {
                        keyframes.iter().map(|keyframe| &keyframe.value).collect()
                    }
                };
                // Keyframes share their structure, so the first tells.
                if let Some(first) = paths.first() {
                    for (subpath, entry) in first.subpaths.iter().enumerate() {
                        if entry.vertices.is_empty() {
                            issues.push(LintIssue::EmptySubpath {
                                owner: owner.clone(),
                                at: format!("{at}.path.subpaths[{subpath}]"),
                            });
                        }
                    }
                }
            }
            ShapeItem::Fill { .. } | ShapeItem::Stroke(_) => {
                if !geometry {
                    issues.push(LintIssue::PaintWithoutGeometry {
                        owner: owner.clone(),
                        at,
                    });
                }
            }
            ShapeItem::Trim(_) => {}
        }
    }
    geometry
}

/// Each of `animators` that sets nothing, under `at`.
fn empty_animators(
    owner: &LintOwner,
    animators: &[crate::TextAnimator],
    at: &str,
    issues: &mut Vec<LintIssue>,
) {
    for (index, animator) in animators.iter().enumerate() {
        if animator.properties.is_empty() {
            issues.push(LintIssue::EmptyTextAnimator {
                owner: owner.clone(),
                at: format!("{at}[{index}]"),
            });
        }
    }
}

/// A caption track's animators: their keyframes, in cue time, against every
/// cue's span (a keyframe some cue reaches is used), and the empty ones.
fn lint_caption_animators(
    track: &TrackId,
    captions: &crate::Captions,
    issues: &mut Vec<LintIssue>,
) {
    let owner = LintOwner::CaptionTrack {
        track: track.clone(),
    };
    let spans = captions
        .cues
        .iter()
        .filter_map(|cue| cue.timeline_range.duration())
        .map(|duration| (0, duration.ticks().saturating_sub(1)))
        .collect::<Vec<_>>();
    let mut keyframes = Keyframes {
        owner: &owner,
        shown: &spans,
        issues,
    };
    crate::text_animation::for_each_keyed_animator(
        &captions.style.animators,
        "style.animators",
        &mut |property, times| keyframes.check_times(property, times),
    );
    empty_animators(&owner, &captions.style.animators, "style.animators", issues);
}
