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
    /// A keyframe that no time its owner is shown at depends on.
    UnreachedKeyframe {
        /// What the keyframe animates.
        owner: LintOwner,
        /// The animated property, as a path such as `visual.opacity` or
        /// `masks[0].feather`.
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
        /// The path, like `items[0].path`, and the subpath in a keyframe.
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
}

/// The shapes generator a shape [`LintIssue`] is about.
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
    /// sources, then shapes generators (sources', then composition nodes'),
    /// then tracks in order and each track's clips, transitions, and cues in
    /// order.
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
                    active: clip.timeline_range,
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
                    active: transition.timeline_range,
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
        }
        issues
    }

    /// Every shapes generator's empty subpaths and paints without geometry:
    /// sources' first, then composition nodes', in order.
    fn lint_shapes(&self, issues: &mut Vec<LintIssue>) {
        for source in &self.sources {
            if let Some(Generator::Shapes { items }) = &source.generator {
                let owner = ShapeLintOwner::Source {
                    source: source.id.clone(),
                };
                lint_shape_items(&owner, items, "items", issues);
            }
        }
        for composition in &self.compositions {
            for node in &composition.nodes {
                if let crate::NodeOp::Generator {
                    generator: Generator::Shapes { items },
                } = &node.op
                {
                    let owner = ShapeLintOwner::Node {
                        composition: composition.id.clone(),
                        node: node.id.clone(),
                    };
                    lint_shape_items(&owner, items, "items", issues);
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

/// Finds keyframes no time in `active` depends on.
struct Keyframes<'a> {
    owner: &'a LintOwner,
    active: TimeRange,
    issues: &'a mut Vec<LintIssue>,
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
        match geometry {
            ShapeGeometry::Rectangle(rectangle) => {
                self.check_point(&format!("{at}.center"), &rectangle.center);
                self.check(&format!("{at}.size"), &rectangle.size);
                self.check(&format!("{at}.corner_radius"), &rectangle.corner_radius);
                self.check(
                    &format!("{at}.rotation_degrees"),
                    &rectangle.rotation_degrees,
                );
            }
            ShapeGeometry::Ellipse(ellipse) => {
                self.check_point(&format!("{at}.center"), &ellipse.center);
                self.check(&format!("{at}.size"), &ellipse.size);
                self.check(&format!("{at}.rotation_degrees"), &ellipse.rotation_degrees);
            }
            ShapeGeometry::Polystar(star) => {
                self.check_point(&format!("{at}.center"), &star.center);
                self.check(&format!("{at}.points"), &star.points);
                self.check(&format!("{at}.outer_radius"), &star.outer_radius);
                self.check(&format!("{at}.inner_radius"), &star.inner_radius);
                self.check(&format!("{at}.outer_roundness"), &star.outer_roundness);
                self.check(&format!("{at}.inner_roundness"), &star.inner_roundness);
                self.check(&format!("{at}.rotation_degrees"), &star.rotation_degrees);
            }
            ShapeGeometry::Path(path) => self.check(&format!("{at}.path"), &path.path),
        }
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
    /// its end); outside every shown tick of `active`, it has no effect.
    fn check_times(&mut self, property: &str, times: &[MediaTime]) {
        let (start, last) = (self.active.start().ticks(), self.active.end().ticks() - 1);
        for (index, time) in times.iter().enumerate() {
            let after = index.checked_sub(1).map_or(i64::MIN, |previous| {
                times[previous].ticks().saturating_add(1)
            });
            let before = times
                .get(index + 1)
                .map_or(i64::MAX, |next| next.ticks().saturating_sub(1));
            if after.max(start) > before.min(last) {
                self.issues.push(LintIssue::UnreachedKeyframe {
                    owner: self.owner.clone(),
                    property: property.to_owned(),
                    time: *time,
                });
            }
        }
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
