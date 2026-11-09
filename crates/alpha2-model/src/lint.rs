//! Non-fatal diagnostics: what a valid project probably does not mean to do.
//!
//! [`Project::lint`] never refuses a project. Checks are conservative: every reported issue
//! is certain, and a missed one is not an error.

use crate::animation::Keyed;
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
    /// A keyframe that no shown time depends on, checked in the owner's own time (timeline,
    /// composition, or picture time).
    UnreachedKeyframe {
        /// What the keyframe animates.
        owner: LintOwner,
        /// The animated property as a path, such as `visual.opacity`, `masks[0].feather`,
        /// `effect.params.radius` or `items[0].transform.opacity`.
        property: String,
        /// The keyframe's time.
        time: MediaTime,
    },
    /// A clip wholly covered for its whole time by an opaque full-frame clip on a later track.
    /// Matte clips are never reported, since they are never composited.
    HiddenClip {
        /// The covered clip.
        clip: ClipId,
        /// The clip that covers it.
        by: ClipId,
    },
    /// A transition no project frame shows as a mix: shorter than a frame, or about a frame long
    /// and starting between frames.
    ShortTransition {
        /// Its track.
        track: TrackId,
        /// Its first clip.
        from_clip: ClipId,
        /// Its second clip.
        to_clip: ClipId,
    },
    /// A boundary that is not where a project frame starts, so the change shows at the next
    /// frame. A frame starts at its exact start rounded down to a tick.
    OffFrameBoundary {
        /// Whose boundary.
        owner: LintOwner,
        /// Which end.
        edge: LintEdge,
        /// The boundary's time.
        time: MediaTime,
    },
    /// A source no render reads: no clip shows it and no `media` node of a shown composition
    /// reads it.
    UnusedSource {
        /// The source.
        source: SourceId,
    },
    /// A high-dynamic-range source in a project whose color pipeline refuses HDR, so rendering
    /// it fails.
    HdrSource {
        /// The source.
        source: SourceId,
    },
    /// A path's subpath without vertices, which draws nothing.
    EmptySubpath {
        /// The shapes generator.
        owner: ShapeLintOwner,
        /// The empty subpath, like `items[0].path.subpaths[1]` or, in a layout,
        /// `root.children[0].shape.items[0].path.subpaths[1]`.
        at: String,
    },
    /// A fill or stroke with no geometry before it in its group, which paints nothing.
    PaintWithoutGeometry {
        /// The shapes generator.
        owner: ShapeLintOwner,
        /// The paint item, like `items[1].items[0]`.
        at: String,
    },
    /// A caption style no cue names and no cue's speaker matches.
    UnusedCaptionStyle {
        /// The caption track.
        track: TrackId,
        /// The style's name.
        style: String,
    },
    /// A source-anchored data set whose `made_from` differs from its source's current
    /// fingerprint. Reported only by [`Project::stale_data`], where the host compares.
    StaleData {
        /// The data set.
        data: crate::DataSetId,
        /// Its source.
        source: SourceId,
    },
    /// A text animator that sets no property, which changes nothing.
    EmptyTextAnimator {
        /// What holds the text.
        owner: LintOwner,
        /// The animator, like `style.animators[0]` or, in a layout,
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
    /// A generated source, whose shapes or layout tree is keyed in picture time.
    Source {
        /// The source.
        source: SourceId,
    },
    /// A caption track, whose style's animators are keyed in each cue's time from its start.
    CaptionTrack {
        /// The track.
        track: TrackId,
    },
    /// A timed run of a caption cue, whose times are from the cue's start.
    CueRun {
        /// Its caption track.
        track: TrackId,
        /// Its cue's position among the track's cues.
        cue: usize,
        /// Its position among the cue's runs.
        run: usize,
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
    /// [`LintIssue::StaleData`] for each source-anchored data set with a `made_from` that
    /// `is_stale` judges different from its source's current fingerprint, in data order.
    /// Hosts pass their fingerprint comparison; the model holds no current fingerprints.
    #[must_use]
    pub fn stale_data(
        &self,
        mut is_stale: impl FnMut(&SourceId, &crate::MediaFingerprint) -> bool,
    ) -> Vec<LintIssue> {
        self.data
            .iter()
            .filter_map(|set| {
                let source = set.anchor.source()?;
                let made_from = set.made_from.as_ref()?;
                is_stale(source, made_from).then(|| LintIssue::StaleData {
                    data: set.id.clone(),
                    source: source.clone(),
                })
            })
            .collect()
    }

    /// Diagnostics that do not stop the project rendering, in a fixed order: unused sources,
    /// shapes generators (sources', then composition nodes'), HDR sources, each track's clips,
    /// transitions and cues, composition nodes, then generated sources' trees.
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
            lint_cues(&grid, track, &mut issues);
            if let Some(captions) = &track.captions {
                lint_caption_animators(&track.id, captions, &mut issues);
            }
        }
        self.lint_keyframes_in_other_times(&mut issues);
        self.lint_text_animators(&mut issues);
        issues
    }

    /// Every text animator that sets nothing, in text generators and layout text nodes.
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

    /// Unreached keyframes in the other two time bases: composition nodes' settings in
    /// composition time and generated sources' trees in picture time. Compositions or sources
    /// that no clip shows, or whose shown times cannot be bounded, are skipped.
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
                node_keyed(&node.op, &mut |property, keyed| {
                    if let Keyed::Times(times) = keyed {
                        keyframes.check_times(property, times);
                    }
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
            let mut check = |property: &str, keyed: Keyed<'_>| {
                if let Keyed::Times(times) = keyed {
                    keyframes.check_times(property, times);
                }
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

    /// Empty subpaths and paints without geometry in every shapes generator and layout shape
    /// node: sources' first, then composition nodes'.
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

    /// The video-track clips that certainly cover the whole frame opaquely outside their
    /// transitions: composited (not a matte) and [`is_opaque_full_frame`]. In track then clip
    /// order.
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

/// A clip that covers the whole frame opaquely, except during its transitions.
struct Cover<'a> {
    /// Its track's position.
    order: usize,
    clip: &'a crate::Clip,
    /// Its transitions' ranges, where it is mixed with another clip.
    mixed: Vec<TimeRange>,
}

/// A clip on a track after `order` that covers the frame for all of `range`.
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

/// Whether a clip draws an opaque fill over the whole frame: a generated fill of opaque
/// paint with no transform, blend, mask or composition.
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

/// The project's frames as the engine renders them: frame `k` at
/// [`FrameRate::frame_start`], its exact start rounded down to a tick.
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

    /// Whether a frame shows a transition over `range` as a mix, i.e. the evaluator's progress
    /// `clamp((t - start + F/2) / D, 0, 1)` is below 1 at some frame start `t` in the range.
    /// Progress grows with `t`, so the first frame start decides. Unsure counts as shown.
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

/// Finds keyframes no shown time depends on. `shown` holds every tick span (first, last) the
/// owner may be shown at.
struct Keyframes<'a> {
    owner: &'a LintOwner,
    shown: &'a [(i64, i64)],
    issues: &'a mut Vec<LintIssue>,
}

/// The first and last ticks of `range`.
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
        geometry.for_each_keyed(at, &mut |property, keyed| {
            if let Keyed::Times(times) = keyed {
                self.check_times(property, times);
            }
        });
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

    /// Keyframe `i` affects values only strictly between its neighbours' times (open-ended for
    /// the first and last), so outside every shown tick it has no effect.
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

/// Every tick span (first, last) each composition and each generated or composition source
/// may be shown at.
///
/// Composition time comes from a clip's [`crate::ClipComposition`] mapping, a composition
/// source's picture time, or a parent composition drawing it as a group. Picture time comes
/// from a clip's source range through its time map, or a media node's `start` plus its
/// composition's time. Spans may over-cover, so a keyframe outside all of them is certainly
/// unreached. Anything whose time cannot be represented is left out with all it shows.
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

    /// What the clips show directly (sources at their time-mapped picture times, compositions at
    /// their mapped times), and the compositions mapped to unrepresentable times.
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

    /// Extends `self` with what it shows in turn: composition sources' compositions, groups'
    /// compositions, and media nodes' sources. Each pass adds one level; the graph is acyclic,
    /// so one pass per composition and source reaches every level.
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

    /// Leaves out the compositions in `unbounded` and what they draw (groups' compositions and
    /// media nodes' sources).
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

    /// Sorts each list and merges spans that meet or touch.
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

/// Calls `visit` with each keyframed setting of a node, by its path in the node
/// (`effect.params.radius`, `generator.items[0].transform.scale`, ...), and its times.
pub(crate) fn node_keyed(
    op: &crate::NodeOp,
    visit: &mut impl FnMut(&str, crate::animation::Keyed<'_>),
) {
    use crate::animation::{visit_keyed, visit_keyed_point};
    let params = |effect: &crate::ClipEffect,
                  visit: &mut dyn FnMut(&str, crate::animation::Keyed<'_>)| {
        for (name, value) in &effect.params {
            visit_keyed(&format!("effect.params.{name}"), value, &mut |at, keyed| {
                visit(at, keyed);
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

/// Reports a content tree's empty subpaths and paints with nothing before them, in document
/// order. Returns whether the items hold any geometry, for an enclosing group's paints.
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
                    Animated::Data(_) => Vec::new(),
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

/// Each cue's timeline range, and each timed run's, against the frame grid.
fn lint_cues(grid: &FrameGrid, track: &crate::Track, issues: &mut Vec<LintIssue>) {
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
        grid.check(&owner, cue.timeline_range, issues);
        // A karaoke switch between frames shows at the next frame, so check each run's ends on the
        // timeline.
        for (run, timed) in cue.text.runs().iter().enumerate() {
            let Some(time) = timed.time else {
                continue;
            };
            let start = cue.timeline_range.start().ticks();
            let Ok(range) = alpha2_time::TimeRange::new(
                MediaTime(start.saturating_add(time.start.ticks())),
                MediaTime(start.saturating_add(time.end.ticks())),
            ) else {
                continue;
            };
            let owner = LintOwner::CueRun {
                track: track.id.clone(),
                cue: index,
                run,
            };
            grid.check(&owner, range, issues);
        }
    }
}

/// A caption track's animators: keyframes in cue time against every cue's span, empty
/// animators, then styles no cue takes.
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
        &mut |property, keyed| {
            if let Keyed::Times(times) = keyed {
                keyframes.check_times(property, times);
            }
        },
    );
    empty_animators(&owner, &captions.style.animators, "style.animators", issues);
    for name in captions.styles.keys() {
        let used = captions.cues.iter().any(|cue| {
            cue.style.as_deref() == Some(name.as_str())
                || (cue.style.is_none() && cue.speaker.as_deref() == Some(name.as_str()))
        });
        if !used {
            issues.push(LintIssue::UnusedCaptionStyle {
                track: track.clone(),
                style: name.clone(),
            });
        }
    }
}
