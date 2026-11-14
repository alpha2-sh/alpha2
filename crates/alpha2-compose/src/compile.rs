//! Placing primitives on the timeline: lengths, areas, lanes, and the
//! project they become.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use alpha2_media::SourceId;
use alpha2_model::{
    Animated, AnimatedPoint, CaptionCue, Captions, Clip, ClipComposition, ClipEffect, ClipId,
    ClipTransition, Composition, CompositionId, CompositionNode, Ducking, Easing, EffectValue,
    FontSizeFit, Generator, Interpolation, Keyframe, LayoutNode, LineClamp, LoopMode,
    MediaKind as ModelKind, MediaSource, MergeLayer, NodeId, NodeOp, Paint, Project, ProjectId,
    ProjectSettings, RangeShape, RangeUnits, RectangleGeometry, Rgb, Rgba, ShapeGeometry,
    ShapeMask, TextAlign, TextAnimator, TextAnimatorProperties, TextBackground, TextSelector,
    TextStyle, TimeMap, TimeScale, Timeline, Track, TrackId, TrackKind, Vec2, VisualProperties,
};
use alpha2_model::{
    Dash, EllipseGeometry, FillRule, GradientStop, PathGeometry, PolystarGeometry, ShapeItem,
    Spread, StrokeItem, TrimItem, TrimMode,
};
use alpha2_text::{TextFont, TextRasterizer};
use alpha2_time::{FrameRate, MediaTime, TICKS_PER_SECOND, TimeRange};
use serde_json::json;

use crate::lower::{
    AnimPrim, AudioPrim, BoxContent, BoxPrim, BoxTree, CaptionsPrim, Content, CounterContent,
    Coverage, CoveragePrim, EffectPrim, GeometryPrim, GridLayout, GroupPrim, Head, KeyPrim,
    KeyTime, MaskShapePrim, NodeMotion, PaintPrim, Preset, Prim, RevealPrim, SequencePrim,
    ShapeContent, StackPrim, Visual, align_point, ticks_scaled,
};
use crate::spec::{
    Canvas, CustomCanvas, Fit, FrameRateChoice, GridLength, Loudness, Transition, VideoSpec,
    WhenLong, WhenShort,
};
use crate::theme::Resolved;
use crate::{Compiled, ComposeError, Facts, LoudnessTarget};

#[allow(clippy::cast_precision_loss)]
pub(crate) fn seconds(ticks: i64) -> f64 {
    ticks as f64 / TICKS_PER_SECOND as f64
}

fn range(start: i64, end: i64) -> TimeRange {
    TimeRange::new(MediaTime::from_ticks(start), MediaTime::from_ticks(end))
        .expect("the compiler orders every range it makes")
}

/// A cue over `start..end`.
pub(crate) fn cue(start: i64, end: i64, text: String) -> CaptionCue {
    CaptionCue {
        timeline_range: range(start, end),
        text: text.into(),
        speaker: None,
        style: None,
    }
}

const fn constant<T>(value: T) -> Animated<T> {
    Animated::Constant { value }
}

/// A canvas area in pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl Rect {
    /// The area with `[top, right, bottom, left]` kept clear inside it.
    fn inset(self, [top, right, bottom, left]: [f32; 4]) -> Self {
        Self {
            x: self.x + left,
            y: self.y + top,
            width: (self.width - left - right).max(1.0),
            height: (self.height - top - bottom).max(1.0),
        }
    }
}

/// How a clip shorter than the time it is given fills the rest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Hold {
    None,
    Freeze,
    Loop,
}

/// Where and when a primitive is placed.
#[derive(Clone, Debug)]
struct Slot {
    start: i64,
    /// The time it may take. A node with its own length plays that, trimmed to it; one
    /// without fills it.
    len: i64,
    rect: Rect,
    hold: Hold,
    /// A fade in over `start..start + duration` multiplied into every picture, used for a
    /// crossfade into a node that is not one picture.
    ramp: Option<(i64, i64)>,
    owners: BTreeSet<String>,
    /// Entrances and exits of the node and its ancestors, at timeline times.
    motions: Vec<Ramp>,
    /// A staggered parent's motion, resolved over this node's own span.
    pending: Vec<NodeMotion>,
    /// Ticks this node's entrances wait after its start.
    delay: i64,
}

/// A node's own length, or the one it takes when nothing gives it one.
#[derive(Clone, Copy, Debug)]
enum Length {
    Fixed(i64),
    Flexible(i64),
}

impl Length {
    const fn resolve(self) -> i64 {
        match self {
            Self::Fixed(ticks) | Self::Flexible(ticks) => ticks,
        }
    }
}

/// One track's worth of placed content.
#[derive(Clone, Debug)]
struct Lane {
    kind: TrackKind,
    clips: Vec<Clip>,
    transitions: Vec<ClipTransition>,
    captions: Option<Captions>,
    /// The ids of the nodes whose content it holds.
    owners: BTreeSet<String>,
    /// A ducked audio lane's sidechain: the node it ducks under, the depth, and the request's
    /// path.
    duck: Option<(String, f32, String)>,
    /// A raw fragment's track, kept as written.
    raw: Option<Track>,
}

impl Lane {
    fn new(kind: TrackKind, owners: &BTreeSet<String>) -> Self {
        Self {
            kind,
            clips: Vec::new(),
            transitions: Vec::new(),
            captions: None,
            owners: owners.clone(),
            duck: None,
            raw: None,
        }
    }

    fn extent(&self) -> Option<(i64, i64)> {
        let clips = self.clips.iter().map(|clip| clip.timeline_range);
        let cues = self
            .captions
            .iter()
            .flat_map(|captions| captions.cues.iter().map(|cue| cue.timeline_range));
        clips.chain(cues).fold(None, |extent, range| {
            let (start, end) = (range.start().ticks(), range.end().ticks());
            Some(extent.map_or((start, end), |(low, high): (i64, i64)| {
                (low.min(start), high.max(end))
            }))
        })
    }

    /// Whether `other` can join this lane: same kind and style, starting after everything here.
    fn takes(&self, other: &Self) -> bool {
        let style = |lane: &Self| {
            lane.captions
                .as_ref()
                .map(|captions| captions.style.clone())
        };
        self.kind == other.kind
            && self.raw.is_none()
            && other.raw.is_none()
            && self.duck.is_none()
            && other.duck.is_none()
            && style(self) == style(other)
            && match (self.extent(), other.extent()) {
                (Some((_, end)), Some((start, _))) => end <= start,
                _ => true,
            }
    }

    fn absorb(&mut self, other: Self) {
        self.clips.extend(other.clips);
        self.transitions.extend(other.transitions);
        if let Some(captions) = other.captions {
            match &mut self.captions {
                Some(own) => own.cues.extend(captions.cues),
                None => self.captions = Some(captions),
            }
        }
        self.owners.extend(other.owners);
    }
}

/// Placed content, as tracks to be.
#[derive(Clone, Debug, Default)]
struct Lanes {
    /// Video and caption lanes, back to front.
    visual: Vec<Lane>,
    audio: Vec<Lane>,
}

impl Lanes {
    /// `other`'s lanes over these.
    fn stack(&mut self, other: Self) {
        self.visual.extend(other.visual);
        self.audio.extend(other.audio);
    }

    /// Moves `other`'s lanes into lanes here they can share (starting after what those hold)
    /// or into new ones. Returns where each of its visual lanes went.
    fn share(&mut self, other: Self) -> Vec<usize> {
        fn share_into(into: &mut Vec<Lane>, lanes: Vec<Lane>) -> Vec<usize> {
            let mut cursor = 0;
            let mut placed = Vec::with_capacity(lanes.len());
            for lane in lanes {
                let found = (cursor..into.len()).find(|&index| into[index].takes(&lane));
                let index = if let Some(index) = found {
                    into[index].absorb(lane);
                    index
                } else {
                    into.insert(cursor, lane);
                    for index in &mut placed {
                        if *index >= cursor {
                            *index += 1;
                        }
                    }
                    cursor
                };
                placed.push(index);
                cursor = index + 1;
            }
            placed
        }
        share_into(&mut self.audio, other.audio);
        share_into(&mut self.visual, other.visual)
    }
}

/// Everything compiling one spec builds up.
pub(crate) struct Context<'a> {
    pub(crate) facts: &'a Facts,
    pub(crate) theme: Resolved,
    width: u32,
    height: u32,
    pub(crate) rate: FrameRate,
    measurer: Option<TextRasterizer>,
    sources: Vec<MediaSource>,
    media_sources: BTreeMap<(String, &'static str), SourceId>,
    assets: BTreeMap<String, String>,
    compositions: Vec<Composition>,
    blur_compositions: BTreeMap<u32, CompositionId>,
    reserved: HashSet<String>,
    next_source: u32,
    next_clip: u32,
    next_composition: u32,
    warnings: Vec<String>,
}

/// A canvas preset: width, height.
fn preset(name: &str) -> Option<(u32, u32)> {
    Some(match name {
        "youtube-1080p" | "landscape" => (1920, 1080),
        "youtube-4k" => (3840, 2160),
        "youtube-720p" => (1280, 720),
        "shorts" | "vertical" | "reels" | "tiktok" => (1080, 1920),
        "square" => (1080, 1080),
        "portrait" => (1080, 1350),
        _ => return None,
    })
}

const PRESETS: &str = "youtube-1080p, youtube-4k, youtube-720p, shorts, square, portrait";

fn frame_rate(choice: FrameRateChoice) -> Result<FrameRate, String> {
    let exact = |num, den| FrameRate::new(num, den).map_err(|error| error.to_string());
    match choice {
        FrameRateChoice::Exact(rate) => Ok(rate),
        FrameRateChoice::Number(rate) => {
            for (approximate, num) in [(23.976, 24_000), (29.97, 30_000), (59.94, 60_000)] {
                if (rate - approximate).abs() < 0.001 {
                    return exact(num, 1001);
                }
            }
            if rate.fract() == 0.0 && (1.0..=1000.0).contains(&rate) {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                return exact(rate as u32, 1);
            }
            Err(format!(
                "{rate} fps is not a rate: give an integer, 23.976, 29.97, 59.94, or {{\"num\", \"den\"}}"
            ))
        }
    }
}

impl<'a> Context<'a> {
    fn new(
        spec: &VideoSpec,
        facts: &'a Facts,
        fonts: &[Arc<[u8]>],
    ) -> Result<(Self, Rgb), ComposeError> {
        let theme = crate::theme::resolve(&spec.theme)
            .map_err(|error| ComposeError::new("spec.theme", error))?;
        let custom = match &spec.canvas {
            Canvas::Preset(name) => CustomCanvas {
                preset: Some(name.clone()),
                ..CustomCanvas::default()
            },
            Canvas::Custom(custom) => custom.clone(),
        };
        let name = custom.preset.as_deref().unwrap_or("youtube-1080p");
        let (width, height) = preset(name).ok_or_else(|| {
            ComposeError::new(
                "spec.canvas",
                format!("unknown canvas preset \"{name}\"; the presets are {PRESETS}"),
            )
        })?;
        let (width, height) = (
            custom.width.unwrap_or(width),
            custom.height.unwrap_or(height),
        );
        let rate = custom
            .frame_rate
            .map_or(
                FrameRate::new(30, 1).map_err(|error| error.to_string()),
                frame_rate,
            )
            .map_err(|error| ComposeError::new("spec.canvas.frame_rate", error))?;
        let background = custom.background.unwrap_or(theme.background);
        let measurer = if fonts.is_empty() {
            None
        } else {
            let fonts = fonts
                .iter()
                .enumerate()
                .map(|(index, bytes)| TextFont {
                    family: alpha2_text::font_family(bytes, 0)
                        .unwrap_or_else(|| format!("font {index}")),
                    bytes: Arc::clone(bytes),
                    face_index: 0,
                })
                .collect();
            Some(
                TextRasterizer::new(fonts)
                    .map_err(|error| ComposeError::new("fonts", error.to_string()))?,
            )
        };
        let reserved = spec
            .overlays
            .iter()
            .filter_map(|node| match node {
                crate::spec::Node::Raw(raw) => Some(raw),
                _ => None,
            })
            .flat_map(|raw| {
                let sources = raw.sources.iter().map(|source| source.id.to_string());
                let tracks = raw.tracks.iter().flat_map(|track| {
                    std::iter::once(track.id.to_string())
                        .chain(track.clips.iter().map(|clip| clip.id.to_string()))
                });
                let compositions = raw
                    .compositions
                    .iter()
                    .map(|composition| composition.id.to_string());
                sources
                    .chain(tracks)
                    .chain(compositions)
                    .collect::<Vec<_>>()
            })
            .collect();
        Ok((
            Self {
                facts,
                theme,
                width,
                height,
                rate,
                measurer,
                sources: Vec::new(),
                media_sources: BTreeMap::new(),
                assets: BTreeMap::new(),
                compositions: Vec::new(),
                blur_compositions: BTreeMap::new(),
                reserved,
                next_source: 0,
                next_clip: 0,
                next_composition: 0,
                warnings: Vec::new(),
            },
            Rgb::new(background.red, background.green, background.blue),
        ))
    }

    #[allow(clippy::cast_precision_loss)]
    pub(crate) const fn canvas_px(&self) -> (f32, f32) {
        (self.width as f32, self.height as f32)
    }

    pub(crate) fn short_side(&self) -> f32 {
        let (width, height) = self.canvas_px();
        width.min(height)
    }

    pub(crate) const fn vertical(&self) -> bool {
        self.height > self.width
    }

    pub(crate) fn warn(&mut self, warning: String) {
        self.warnings.push(warning);
    }

    fn canvas_rect(&self) -> Rect {
        let (width, height) = self.canvas_px();
        Rect {
            x: 0.0,
            y: 0.0,
            width,
            height,
        }
    }

    /// The next id of the form `{prefix}{n}` that nothing else uses.
    fn fresh(reserved: &HashSet<String>, counter: &mut u32, prefix: &str) -> String {
        loop {
            *counter += 1;
            let id = format!("{prefix}{counter}");
            if !reserved.contains(&id) {
                return id;
            }
        }
    }

    fn clip_id(&mut self) -> ClipId {
        ClipId::new(Self::fresh(&self.reserved, &mut self.next_clip, "c"))
            .expect("a generated id is not empty")
    }

    fn source_id(&mut self) -> SourceId {
        SourceId::new(Self::fresh(&self.reserved, &mut self.next_source, "s"))
            .expect("a generated id is not empty")
    }

    /// The source of the file at `path`, read as `kind`.
    fn media_source(&mut self, path: &str, kind: ModelKind, has_audio: bool) -> SourceId {
        let name = match kind {
            ModelKind::Image => "image",
            ModelKind::Audio => "audio",
            _ => "video",
        };
        if let Some(id) = self.media_sources.get(&(path.to_owned(), name)) {
            return id.clone();
        }
        let id = self.source_id();
        let mut source = MediaSource::new(id.clone(), path, kind);
        source.has_audio = has_audio && kind == ModelKind::Video;
        self.sources.push(source);
        self.media_sources
            .insert((path.to_owned(), name), id.clone());
        self.assets.insert(id.to_string(), path.to_owned());
        id
    }

    fn generated_source(&mut self, generator: Generator) -> SourceId {
        let id = self.source_id();
        self.sources
            .push(MediaSource::generated(id.clone(), generator));
        id
    }

    /// The composition that blurs a clip's picture by `radius`.
    fn blur_composition(&mut self, radius: f32) -> CompositionId {
        if let Some(id) = self.blur_compositions.get(&radius.to_bits()) {
            return id.clone();
        }
        let id = CompositionId::new(Self::fresh(
            &self.reserved,
            &mut self.next_composition,
            "blur",
        ))
        .expect("a generated id is not empty");
        let clip = NodeId::new("clip").expect("a literal id");
        let blur = NodeId::new("blur").expect("a literal id");
        let mut composition = Composition::new(id.clone());
        composition.nodes = vec![
            CompositionNode::new(clip.clone(), NodeOp::Clip {}),
            CompositionNode::new(
                blur.clone(),
                NodeOp::Effect {
                    input: Some(clip),
                    inputs: BTreeMap::new(),
                    effect: ClipEffect {
                        id: "alpha2.effect.gaussian-blur".to_owned(),
                        version: 1,
                        params: BTreeMap::from([(
                            "radius".to_owned(),
                            constant(EffectValue::Float(radius)),
                        )]),
                        resources: BTreeMap::new(),
                    },
                },
            ),
        ];
        composition.output = Some(blur);
        self.compositions.push(composition);
        self.blur_compositions.insert(radius.to_bits(), id.clone());
        id
    }

    /// `prim`'s own length, or the one it takes by default.
    fn length(&self, prim: &Prim) -> Length {
        if let Some(duration) = prim.head().duration {
            return Length::Fixed(duration);
        }
        match prim {
            Prim::Sequence(sequence) => {
                let overlap = sequence.transition.as_ref().map_or(0, transition_ticks);
                let total = sequence
                    .children
                    .iter()
                    .enumerate()
                    .map(|(index, child)| {
                        self.length(child).resolve() - if index > 0 { overlap } else { 0 }
                    })
                    .sum::<i64>();
                Length::Fixed(total.max(0))
            }
            Prim::Stack(stack) => self.stack_length(stack),
            Prim::Group(group) => self.stack_length(&group.stack),
            Prim::Coverage(coverage) => self.length(&coverage.content),
            Prim::Box(node) => self.stack_length(&StackPrim {
                head: Head::default(),
                children: box_prims(&node.tree),
                inset: [0.0; 4],
                grid: None,
            }),
            Prim::Visual(visual) => match &visual.content {
                Content::Video(video) if video.looped => Length::Flexible(video.natural()),
                Content::Video(video) => Length::Fixed(video.natural()),
                Content::Image { .. } => {
                    Length::Flexible(crate::lower::seconds_to_ticks(self.theme.still_seconds))
                }
                Content::Text(_)
                | Content::Color(_)
                | Content::Shape(_)
                | Content::Layout(_)
                | Content::Counter(_) => {
                    Length::Flexible(crate::lower::seconds_to_ticks(self.theme.text_seconds))
                }
            },
            Prim::Captions(captions) => Length::Fixed(
                captions
                    .cues
                    .iter()
                    .map(|cue| cue.timeline_range.end().ticks())
                    .max()
                    .unwrap_or(0),
            ),
            Prim::Audio(audio) if audio.looped => {
                Length::Flexible(audio.trim_end - audio.trim_start)
            }
            Prim::Audio(audio) => Length::Fixed(audio.trim_end - audio.trim_start),
            Prim::Raw(..) => Length::Flexible(0),
        }
    }

    /// A stack's own length, or the one it takes by default.
    fn stack_length(&self, stack: &StackPrim) -> Length {
        let lengths = stack
            .children
            .iter()
            .map(|child| (child.head().at.unwrap_or(0), self.length(child)));
        let fixed = lengths.clone().filter_map(|(at, length)| match length {
            Length::Fixed(ticks) => Some(at + ticks),
            Length::Flexible(_) => None,
        });
        let shortest = matches!(
            stack.grid,
            Some(GridLayout {
                length: GridLength::Shortest,
                ..
            })
        );
        let fixed = if shortest { fixed.min() } else { fixed.max() };
        fixed.map_or_else(
            || {
                Length::Flexible(
                    lengths
                        .map(|(at, length)| at + length.resolve())
                        .max()
                        .unwrap_or(0),
                )
            },
            Length::Fixed,
        )
    }

    /// Places `prim` in `slot`, adding its lanes to `out`.
    fn emit(&mut self, prim: &Prim, slot: &Slot, out: &mut Lanes) -> Result<(), ComposeError> {
        if slot.len <= 0 {
            return Ok(());
        }
        let mut slot = slot.clone();
        slot.owners.extend(prim.head().id.clone());
        self.resolve_motion(prim.head(), &mut slot);
        match prim {
            Prim::Sequence(sequence) => self.emit_sequence(sequence, &slot, out),
            Prim::Stack(stack) => self.emit_stack(stack, &slot, out),
            Prim::Group(group) => self.emit_group(group, &slot, out),
            Prim::Coverage(coverage) => self.emit_coverage(coverage, &slot, out),
            Prim::Box(node) => self.emit_box(node, &slot, out),
            Prim::Visual(visual) => self.emit_visual(visual, &slot, out),
            Prim::Captions(captions) => {
                self.emit_captions(captions, &slot, out);
                Ok(())
            }
            Prim::Audio(audio) => {
                self.emit_audio(audio, &slot, out);
                Ok(())
            }
            Prim::Raw(head, _) => Err(ComposeError::new(
                &head.path,
                "a raw fragment keeps its own times, so it goes in `overlays`",
            )),
        }
    }

    /// Adds the node's entrance and exit (and a staggered parent's) to `slot.motions`, timed
    /// over the slot.
    fn resolve_motion(&mut self, head: &Head, slot: &mut Slot) {
        let mut motions = std::mem::take(&mut slot.pending);
        if head.stagger == 0 {
            motions.extend(head.motion.as_deref().cloned());
        }
        let (start, end) = (slot.start, slot.start + slot.len);
        let mut trimmed = false;
        for motion in motions {
            let mut entered = start;
            if let Some(preset) = motion.enter {
                let from = (start + slot.delay).min(end);
                entered = (from + preset.duration).min(end);
                trimmed |= from + preset.duration > end;
                if entered > from {
                    slot.motions.push(Ramp::enter(&preset, from, entered));
                }
            }
            if let Some(preset) = motion.exit {
                let from = (end - preset.duration).max(start);
                trimmed |= from < entered || end - preset.duration < start;
                slot.motions.push(Ramp::exit(&preset, from, end));
            }
        }
        slot.delay = 0;
        if trimmed {
            self.warn(format!(
                "{}: it is shorter than its entrance and exit, so they overlap or are cut short",
                head.path
            ));
        }
    }

    fn emit_sequence(
        &mut self,
        sequence: &SequencePrim,
        slot: &Slot,
        out: &mut Lanes,
    ) -> Result<(), ComposeError> {
        let mut lanes = Lanes::default();
        let end = slot.start + slot.len;
        let overlap = sequence.transition.as_ref().map_or(0, transition_ticks);
        let mut cursor = slot.start;
        // The lane and clip of the previous child, when it was one picture.
        let mut previous: Option<(usize, ClipId, i64)> = None;
        for (index, child) in sequence.children.iter().enumerate() {
            let length = self.length(child).resolve();
            let joined = index > 0 && overlap > 0;
            if joined {
                let previous_length = previous.as_ref().map_or(i64::MAX, |(_, _, length)| *length);
                if length < overlap || previous_length < overlap {
                    return Err(ComposeError::new(
                        &child.head().path,
                        format!(
                            "it is shorter than the {} s transition into it",
                            seconds(overlap)
                        ),
                    ));
                }
            }
            let start = if joined { cursor - overlap } else { cursor };
            if start >= end {
                break;
            }
            let single = matches!(child, Prim::Visual(_));
            let engine_transition = joined && single && previous.is_some();
            let transition = sequence.transition.as_ref().filter(|_| joined);
            if let Some(transition) = transition
                && !engine_transition
                && !is_crossfade(transition)
            {
                return Err(ComposeError::new(
                    &child.head().path,
                    format!(
                        "a {} transition joins two single pictures (media, text, color, layout); use crossfade, or none",
                        transition.kind
                    ),
                ));
            }
            let mut fresh = Lanes::default();
            let child_slot = Slot {
                start,
                len: length.min(end - start),
                ramp: (joined && !engine_transition).then_some((start, overlap)),
                hold: Hold::None,
                ..slot.clone()
            };
            self.emit(child, &child_slot, &mut fresh)?;
            let placed = if engine_transition {
                let (lane, from, _) = previous.take().expect("a transition follows a picture");
                let mut visual = std::mem::take(&mut fresh.visual).into_iter();
                let incoming = visual.next().expect("a picture makes one lane");
                let to = incoming.clips[0].id.clone();
                lanes.visual[lane].absorb(incoming);
                let transition = transition.expect("joined");
                lanes.visual[lane].transitions.push(ClipTransition {
                    from_clip: from,
                    to_clip: to,
                    timeline_range: range(start, (start + overlap).min(end)),
                    easing: transition.easing.unwrap_or(alpha2_model::Easing::Linear),
                    effect: ClipEffect {
                        id: transition_effect_id(&transition.kind),
                        version: transition.version.unwrap_or(1),
                        params: transition.params.clone(),
                        resources: BTreeMap::new(),
                    },
                });
                lanes.share(fresh);
                vec![lane]
            } else if joined {
                // A crossfade over what is not one picture: the incoming node fades in over the
                // outgoing one, in lanes above it.
                let first = lanes.visual.len();
                let count = fresh.visual.len();
                lanes.stack(fresh);
                (first..first + count).collect()
            } else {
                lanes.share(fresh)
            };
            previous = match (single, placed.first()) {
                (true, Some(&lane)) => lanes.visual[lane]
                    .clips
                    .last()
                    .map(|clip| (lane, clip.id.clone(), length)),
                _ => None,
            };
            cursor = start + length;
        }
        out.stack(lanes);
        Ok(())
    }

    fn emit_stack(
        &mut self,
        stack: &StackPrim,
        slot: &Slot,
        out: &mut Lanes,
    ) -> Result<(), ComposeError> {
        let rect = slot.rect.inset(stack.inset);
        let (cells, hold) = match stack.grid {
            None => (None, slot.hold),
            Some(grid) => (
                Some(grid_cells(rect, grid, stack.children.len())),
                match grid.length {
                    GridLength::Shortest => Hold::None,
                    GridLength::LongestFreeze => Hold::Freeze,
                    GridLength::LongestLoop => Hold::Loop,
                },
            ),
        };
        for (index, child) in stack.children.iter().enumerate() {
            let at = child.head().at.unwrap_or(0);
            if at >= slot.len {
                self.warn(format!(
                    "{}: it starts after its parent ends, so it is not shown",
                    child.head().path
                ));
                continue;
            }
            let remaining = slot.len - at;
            let len = match self.length(child) {
                Length::Fixed(_) if hold != Hold::None => remaining,
                Length::Fixed(ticks) => ticks.min(remaining),
                Length::Flexible(_) => remaining,
            };
            let staggered = stack.head.stagger > 0;
            let child_slot = Slot {
                start: slot.start + at,
                len,
                rect: cells.as_ref().map_or(rect, |cells| cells[index]),
                hold,
                pending: if staggered {
                    stack.head.motion.as_deref().cloned().into_iter().collect()
                } else {
                    Vec::new()
                },
                delay: if staggered {
                    i64::try_from(index)
                        .unwrap_or(i64::MAX)
                        .saturating_mul(stack.head.stagger)
                } else {
                    0
                },
                ..slot.clone()
            };
            let mut lanes = Lanes::default();
            self.emit(child, &child_slot, &mut lanes)?;
            out.stack(lanes);
        }
        Ok(())
    }

    /// Draws a group's children over its own time into one composition, and places that as one
    /// clip with the group's motion and blend mode. Their sound goes on audio lanes.
    fn emit_group(
        &mut self,
        group: &GroupPrim,
        slot: &Slot,
        out: &mut Lanes,
    ) -> Result<(), ComposeError> {
        let path = group.stack.head.path.clone();
        let len = slot.len;
        let rect = slot.rect.inset(group.stack.inset);
        let inner = Slot {
            start: 0,
            len,
            rect: slot.rect,
            hold: Hold::None,
            ramp: None,
            owners: slot.owners.clone(),
            motions: Vec::new(),
            pending: Vec::new(),
            delay: 0,
        };
        let mut lanes = Lanes::default();
        self.emit_stack(&group.stack, &inner, &mut lanes)?;
        let mut graph = Graph::default();
        let picture = self.group_picture(&mut graph, &lanes, len, &path)?;
        self.move_sound(&lanes, slot.start, out);
        let mut node = picture;
        for effect in &group.effects {
            node = self.group_effect(&mut graph, effect, node, rect, len)?;
        }
        node = graph.add(NodeOp::Transform {
            input: Some(node),
            transform: self.group_transform(group, rect, len),
        });
        let id = CompositionId::new(Self::fresh(
            &self.reserved,
            &mut self.next_composition,
            "group",
        ))
        .expect("a generated id is not empty");
        let mut composition = Composition::new(id.clone());
        composition.nodes = graph.nodes;
        composition.output = Some(node);
        self.compositions.push(composition);
        let source = self.source_id();
        let mut shown = MediaSource::new(source.clone(), "", ModelKind::Composition);
        shown.composition = Some(id);
        self.sources.push(shown);
        let (start, end) = (slot.start, slot.start + len);
        let mut clip = Clip::new(self.clip_id(), source, range(start, end), range(0, len));
        clip.blend_mode = group.blend;
        let (width, height) = self.canvas_px();
        let anchor = Vec2::new(
            (rect.x + group.transform.anchor[0] * rect.width) / width,
            (rect.y + group.transform.anchor[1] * rect.height) / height,
        );
        clip.visual.position = AnimatedPoint::Constant { value: anchor };
        clip.visual.anchor = anchor;
        let opacity = Opacity {
            base: 1.0,
            start,
            end,
            fade_in: 0,
            fade_out: 0,
            ramp: slot.ramp,
        };
        clip.visual.opacity = opacity.animation(clip.timeline_range);
        if !slot.motions.is_empty() {
            let range = clip.timeline_range;
            self.apply_motion(&mut clip.visual, &opacity, &slot.motions, range);
        }
        let mut lane = Lane::new(TrackKind::Video, &slot.owners);
        lane.clips.push(clip);
        out.visual.push(lane);
        Ok(())
    }

    /// Places a matte's or mask's content with every visual clip of it matted by one coverage
    /// clip. Content that layers pictures over each other is first drawn as one picture.
    fn emit_coverage(
        &mut self,
        coverage: &CoveragePrim,
        slot: &Slot,
        out: &mut Lanes,
    ) -> Result<(), ComposeError> {
        let path = &coverage.head.path;
        let rect = slot.rect.inset(coverage.inset);
        let inner = Slot {
            rect,
            ..slot.clone()
        };
        let mut content = Lanes::default();
        self.emit(&coverage.content, &inner, &mut content)?;
        if !Self::takes_one_matte(&content, path)? {
            content = self.isolate(&coverage.content, &inner)?;
        }
        // Coverage moves with its node but does not fade: the content already does.
        let still = Slot {
            rect,
            ramp: None,
            motions: slot
                .motions
                .iter()
                .map(|ramp| Ramp {
                    opacity: [1.0, 1.0],
                    ..*ramp
                })
                .collect(),
            ..slot.clone()
        };
        let (matte, mode) = match &coverage.by {
            Coverage::Matte { by, mode } => {
                let mut lanes = Lanes::default();
                self.emit(by, &still, &mut lanes)?;
                if !Self::one_picture(&lanes) {
                    lanes = self.isolate(by, &still)?;
                }
                let lane = lanes
                    .visual
                    .iter_mut()
                    .find(|lane| !lane.clips.is_empty())
                    .ok_or_else(|| ComposeError::new(path, "a matte's `by` draws a picture"))?;
                let clip = &mut lane.clips[0];
                clip.audio.muted = true;
                let id = clip.id.clone();
                out.visual.extend(lanes.visual);
                (id, *mode)
            }
            Coverage::Mask(shapes) => {
                let (start, end) = (still.start, still.start + still.len);
                let source = self.generated_source(Generator::Fill {
                    paint: Paint::Solid {
                        color: Rgba::new(1.0, 1.0, 1.0, 1.0),
                    },
                });
                let mut clip = Clip::new(
                    self.clip_id(),
                    source,
                    range(start, end),
                    range(0, still.len),
                );
                let place = Placement::new(rect, self.canvas_px());
                clip.masks = shapes
                    .iter()
                    .map(|shape| ShapeMask {
                        shape: place.geometry(&shape.geometry, clip.timeline_range),
                        feather: map_animated(
                            animated(&shape.feather, clip.timeline_range),
                            |feather| place.length(feather),
                        ),
                        invert: shape.invert,
                        mode: shape.mode,
                        fill_rule: alpha2_model::FillRule::default(),
                    })
                    .collect();
                if !still.motions.is_empty() {
                    let opacity = Opacity {
                        base: 1.0,
                        start,
                        end,
                        fade_in: 0,
                        fade_out: 0,
                        ramp: None,
                    };
                    let whole = clip.timeline_range;
                    self.apply_motion(&mut clip.visual, &opacity, &still.motions, whole);
                }
                let id = clip.id.clone();
                let mut lane = Lane::new(TrackKind::Video, &slot.owners);
                lane.clips.push(clip);
                out.visual.push(lane);
                (id, alpha2_model::MatteMode::Alpha)
            }
        };
        for clip in content.visual.iter_mut().flat_map(|lane| &mut lane.clips) {
            clip.matte = Some(alpha2_model::Matte {
                source: matte.clone(),
                mode,
            });
        }
        out.stack(content);
        Ok(())
    }

    /// Draws a box as layout segments in paint order, each placed child in its measured place
    /// between the segment under it and the one over it.
    fn emit_box(
        &mut self,
        node: &BoxPrim,
        slot: &Slot,
        out: &mut Lanes,
    ) -> Result<(), ComposeError> {
        let path = node.head.path.clone();
        let rect = slot.rect.inset(node.inset);
        let (width, height) = self.canvas_px();
        let mut build = BoxBuild::default();
        let mut inner = self.box_json(&node.tree, slot.len, &mut build, &[0])?;
        inner["flex_grow"] = json!(1);
        // The layout picture is the canvas; the root's padding puts the box in `rect`.
        let root = json!({
            "padding": format!(
                "{}px {}px {}px {}px",
                rect.y,
                width - rect.x - rect.width,
                height - rect.y - rect.height,
                rect.x
            ),
            "children": [inner],
        });
        let mut root = root;
        normalize_layout(&mut root, slot.len, &path)?;
        let root: LayoutNode = serde_json::from_value(root).map_err(|error| {
            ComposeError::new(&path, format!("a box field is invalid: {error}"))
        })?;
        if build.places.is_empty() {
            self.box_clip(root, rect, slot, out);
            return Ok(());
        }
        if slot.motions.iter().any(Ramp::scales) {
            return Err(ComposeError::new(
                &path,
                "a box with pictures in it scales as one in a group",
            ));
        }
        let bounds = self.measure_box(&root, slot.len, &build, &path)?;
        let painted = preorder(&root)
            .iter()
            .map(|node| {
                node.background.is_some()
                    || node.border.is_some()
                    || node.text.is_some()
                    || node.shape.is_some()
            })
            .collect::<Vec<_>>();
        let marks = build
            .places
            .iter()
            .map(|place| place.index)
            .collect::<Vec<_>>();
        for segment in 0..=marks.len() {
            let low = segment.checked_sub(1).map_or(0, |index| marks[index] + 1);
            let high = marks.get(segment).copied().unwrap_or(painted.len());
            if painted[low.min(high)..high].iter().any(|paints| *paints) {
                let mut layer = root.clone();
                hide_outside(&mut layer, &mut 0, low..high);
                self.box_clip(layer, rect, slot, out);
            }
            if let Some(place) = build.places.get(segment) {
                self.emit_place(place, &bounds, slot, out)?;
            }
        }
        Ok(())
    }

    /// One layout generator over the whole canvas, with the box's opacity and motion.
    fn box_clip(&mut self, root: LayoutNode, rect: Rect, slot: &Slot, out: &mut Lanes) {
        let (start, end) = (slot.start, slot.start + slot.len);
        let source = self.generated_source(Generator::Layout {
            root: Box::new(root),
        });
        let mut clip = Clip::new(
            self.clip_id(),
            source,
            range(start, end),
            range(0, slot.len),
        );
        let opacity = Opacity {
            base: 1.0,
            start,
            end,
            fade_in: 0,
            fade_out: 0,
            ramp: slot.ramp,
        };
        clip.visual.opacity = opacity.animation(clip.timeline_range);
        if !slot.motions.is_empty() {
            let (width, height) = self.canvas_px();
            let center = Vec2::new(
                (rect.x + rect.width / 2.0) / width,
                (rect.y + rect.height / 2.0) / height,
            );
            clip.visual.position = AnimatedPoint::Constant { value: center };
            clip.visual.anchor = center;
            let whole = clip.timeline_range;
            self.apply_motion(&mut clip.visual, &opacity, &slot.motions, whole);
        }
        let mut lane = Lane::new(TrackKind::Video, &slot.owners);
        lane.clips.push(clip);
        out.visual.push(lane);
    }

    /// Every node's place at the box's start, refusing a placed child or an ancestor of one that
    /// moves, resizes or is drawn changed.
    fn measure_box(
        &self,
        root: &LayoutNode,
        len: i64,
        build: &BoxBuild,
        path: &str,
    ) -> Result<Vec<alpha2_layout::NodeBounds>, ComposeError> {
        // Without fonts, a box without text is still measured; its text, if any, is refused.
        let empty;
        let measurer = if let Some(measurer) = &self.measurer {
            measurer
        } else {
            empty = TextRasterizer::empty();
            &empty
        };
        let size = alpha2_layout::PictureSize::new(self.width, self.height)
            .map_err(|_| ComposeError::new(path, "the canvas is too large for a layout"))?;
        let at = |ticks: i64| {
            alpha2_layout::measure_layout(
                measurer,
                root,
                MediaTime::from_ticks(ticks),
                size,
                (self.width, self.height),
            )
            .map(|bounds| bounds.nodes)
            .map_err(|error| {
                if self.measurer.is_none() {
                    ComposeError::new(
                        path,
                        "a box with text and pictures in it is measured with fonts; pass them",
                    )
                } else {
                    ComposeError::new(path, error.to_string())
                }
            })
        };
        let first = at(0)?;
        let last = at((len - 1).max(0))?;
        let nodes = preorder(root);
        for place in &build.places {
            for &index in place.ancestors.iter().chain([&place.index]) {
                let node = nodes[index];
                let drawn_as_laid = node.opacity.is_none()
                    && node.translate.is_none()
                    && node.scale.is_none()
                    && node.rotation_degrees.is_none();
                if first[index].border != last[index].border
                    || (index != place.index && !drawn_as_laid)
                {
                    return Err(ComposeError::new(
                        &place.path,
                        "a picture's place in a box keeps still: its size, position, and its \
                         boxes' opacity and transforms do not change",
                    ));
                }
            }
        }
        Ok(first)
    }

    /// A placed child, drawn in its measured border box and clipped to its rounded corners and
    /// to the boxes around it that hide their overflow.
    fn emit_place(
        &mut self,
        place: &Place,
        bounds: &[alpha2_layout::NodeBounds],
        slot: &Slot,
        out: &mut Lanes,
    ) -> Result<(), ComposeError> {
        let (width, height) = self.canvas_px();
        let px = |rect: &alpha2_layout::CanvasRect| Rect {
            x: rect.left * width,
            y: rect.top * height,
            width: (rect.right - rect.left) * width,
            height: (rect.bottom - rect.top) * height,
        };
        let own = px(&bounds[place.index].border);
        let shorter = own.width.min(own.height).max(1.0);
        let mut shapes = Vec::new();
        if place.radius > 0.0 || !place.clips.is_empty() {
            shapes.push(MaskShapePrim {
                geometry: GeometryPrim::Rect {
                    center: AnimPrim::Value([0.5, 0.5]),
                    size: AnimPrim::Value([1.0, 1.0]),
                    corner: AnimPrim::Value(place.radius / shorter),
                    rotation: AnimPrim::Value(0.0),
                },
                mode: alpha2_model::MaskMode::Add,
                feather: AnimPrim::Value(0.0),
                invert: false,
            });
        }
        for (index, radius) in &place.clips {
            let clip = px(&bounds[*index].border);
            shapes.push(MaskShapePrim {
                geometry: GeometryPrim::Rect {
                    center: AnimPrim::Value([
                        (clip.x + clip.width / 2.0 - own.x) / own.width.max(1.0),
                        (clip.y + clip.height / 2.0 - own.y) / own.height.max(1.0),
                    ]),
                    size: AnimPrim::Value([
                        clip.width / own.width.max(1.0),
                        clip.height / own.height.max(1.0),
                    ]),
                    corner: AnimPrim::Value(radius / shorter),
                    rotation: AnimPrim::Value(0.0),
                },
                mode: alpha2_model::MaskMode::Intersect,
                feather: AnimPrim::Value(0.0),
                invert: false,
            });
        }
        let child = if shapes.is_empty() {
            place.prim.clone()
        } else {
            Prim::Coverage(Box::new(CoveragePrim {
                head: Head {
                    path: place.path.clone(),
                    at: place.prim.head().at,
                    duration: place.prim.head().duration,
                    ..Head::default()
                },
                inset: [0.0; 4],
                content: place.prim.clone(),
                by: Coverage::Mask(shapes),
            }))
        };
        let stack = StackPrim {
            head: Head {
                path: place.path.clone(),
                ..Head::default()
            },
            children: vec![child],
            inset: [0.0; 4],
            grid: None,
        };
        let placed = Slot {
            rect: own,
            ..slot.clone()
        };
        self.emit_stack(&stack, &placed, out)
    }

    /// A box as layout JSON, numbering its nodes in paint order and noting placed children.
    fn box_json(
        &mut self,
        tree: &BoxTree,
        len: i64,
        build: &mut BoxBuild,
        ancestors: &[usize],
    ) -> Result<serde_json::Value, ComposeError> {
        let index = build.next();
        let mut node = serde_json::Map::from_iter(tree.css.clone());
        let unit = Placement::new(
            Rect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
            (1.0, 1.0),
        );
        if let Some(paint) = &tree.background {
            node.insert("background".to_owned(), to_json(&unit.paint(paint)));
        }
        if let Some((width, paint)) = &tree.border {
            node.insert(
                "border".to_owned(),
                json!({"width": width, "paint": to_json(&unit.paint(paint))}),
            );
        }
        let mut lineage = ancestors.to_vec();
        lineage.push(index);
        let hides = tree.css.get("overflow").and_then(serde_json::Value::as_str) == Some("hidden");
        let radius = self.layout_px(tree.css.get("border_radius"));
        let mut children = Vec::with_capacity(tree.children.len());
        for kid in &tree.children {
            let mut child = match &kid.content {
                BoxContent::Tree(inner) => self.box_json(inner, len, build, &lineage)?,
                BoxContent::Native(visual) => {
                    build.next();
                    self.native_json(visual, len)
                }
                BoxContent::Foreign(prim) => {
                    let place = build.next();
                    let mut placeholder = json!({});
                    if let Prim::Visual(visual) = prim
                        && let Some((w, h)) = self.picture_size(visual)
                        && !kid.item.contains_key("width")
                        && !kid.item.contains_key("height")
                        && !kid.item.contains_key("aspect_ratio")
                    {
                        #[allow(clippy::cast_precision_loss)]
                        let ratio = w as f32 / h.max(1) as f32;
                        placeholder["aspect_ratio"] = json!(ratio);
                    }
                    let mut clips = build.clips.clone();
                    if hides {
                        clips.push((index, radius));
                    }
                    build.places.push(Place {
                        index: place,
                        ancestors: lineage.clone(),
                        path: prim.head().path.clone(),
                        prim: prim.clone(),
                        radius: self.layout_px(kid.item.get("border_radius")),
                        clips,
                    });
                    placeholder
                }
            };
            if let Some(object) = child.as_object_mut() {
                object.extend(kid.item.clone());
            }
            children.push(child);
        }
        if hides {
            build.clips.push((index, radius));
        }
        node.insert("children".to_owned(), serde_json::Value::Array(children));
        if hides {
            build.clips.pop();
        }
        Ok(serde_json::Value::Object(node))
    }

    /// A layout size in canvas pixels: a number is a fraction of the canvas's shorter side.
    fn layout_px(&self, value: Option<&serde_json::Value>) -> f32 {
        #[allow(clippy::cast_possible_truncation)]
        match value {
            Some(serde_json::Value::Number(number)) => {
                number.as_f64().unwrap_or(0.0) as f32 * self.short_side()
            }
            Some(serde_json::Value::String(text)) => text
                .trim()
                .trim_end_matches("px")
                .trim()
                .parse::<f32>()
                .unwrap_or(0.0),
            _ => 0.0,
        }
    }

    /// The pixel size of a picture node's file, if it shows one.
    fn picture_size(&self, visual: &Visual) -> Option<(u32, u32)> {
        match &visual.content {
            Content::Video(video) => self
                .facts
                .media
                .get(&video.file)
                .and_then(|facts| Some((facts.width?, facts.height?))),
            Content::Image { size, .. } => Some(*size),
            _ => None,
        }
    }

    /// What a text, shape or color leaf draws, as layout fields.
    fn leaf_json(visual: &Visual, len: i64) -> serde_json::Value {
        match &visual.content {
            Content::Text(text) => {
                let (x, _) = align_point(visual.align);
                json!({
                    "text": text.text,
                    "font_size": format!("{}px", text.font_px),
                    "font_family": text.family,
                    "fill": to_json(&Paint::Solid { color: text.color }),
                    "text_align": if x < 0.25 { "start" } else if x > 0.75 { "end" } else { "center" },
                    "strokes": to_json(&text.strokes),
                    "shadows": to_json(&text.shadows),
                    "line_clamp": text.ellipsis.clone().map(|block_ellipsis| to_json(&LineClamp {
                        max_lines: text.max_lines,
                        block_ellipsis,
                    })),
                    "font_size_fit": text.fit.map(|(min, max)| json!({
                        "min": format!("{min}px"),
                        "max": format!("{max}px"),
                    })),
                    "background": text.background.map(|color| to_json(&Paint::Solid { color })),
                    // A text style's background pads 0.3 em at the sides; the line box pads
                    // above and below.
                    "padding": text.background.map(|_| format!(
                        "{}px {}px",
                        0.05 * text.font_px,
                        0.3 * text.font_px
                    )),
                    "text_animators": text.reveal.map(|reveal| vec![to_json(&reveal_animator(reveal, text.font_px, len))]),
                })
            }
            Content::Shape(shape) => {
                let unit = Placement::new(
                    Rect {
                        x: 0.0,
                        y: 0.0,
                        width: 1.0,
                        height: 1.0,
                    },
                    (1.0, 1.0),
                );
                json!({"shape": {"items": to_json(&shape_items_placed(shape, unit, len))}})
            }
            Content::Color(color) => {
                json!({"background": to_json(&Paint::Solid { color: *color })})
            }
            _ => json!({}),
        }
    }

    /// Text, a shape or a color as a layout leaf, shown over its own time of the box with its
    /// fades and motions as the leaf's opacity, translation and scale.
    fn native_json(&mut self, visual: &Visual, len: i64) -> serde_json::Value {
        let mut node = Self::leaf_json(visual, len);
        if let Some(object) = node.as_object_mut() {
            object.retain(|_, value| !value.is_null());
        }
        let start = visual.head.at.unwrap_or(0).clamp(0, len);
        let end = visual
            .head
            .duration
            .map_or(len, |duration| (start + duration).min(len));
        let mut timing = Slot {
            start,
            len: end - start,
            rect: self.canvas_rect(),
            hold: Hold::None,
            ramp: None,
            owners: BTreeSet::new(),
            motions: Vec::new(),
            pending: Vec::new(),
            delay: 0,
        };
        self.resolve_motion(&visual.head, &mut timing);
        let opacity = Opacity {
            base: visual.opacity,
            start,
            end,
            fade_in: visual.fade_in,
            fade_out: visual.fade_out,
            ramp: None,
        };
        let center = Vec2::new(0.5, 0.5);
        let mut moved = VisualProperties {
            position: AnimatedPoint::Constant { value: center },
            anchor: center,
            ..VisualProperties::default()
        };
        let shown = range(start, end);
        moved.opacity = opacity.animation(shown);
        if !timing.motions.is_empty() {
            self.apply_motion(&mut moved, &opacity, &timing.motions, shown);
        }
        let visible = windowed(&moved.opacity, start, end, len);
        if visible != constant(1.0) {
            node["opacity"] = to_json(&visible);
        }
        let (width, height) = self.canvas_px();
        let position = match &moved.position {
            AnimatedPoint::Constant { value } => Animated::Constant { value: *value },
            AnimatedPoint::Data(binding) => Animated::Data(binding.clone()),
            AnimatedPoint::Keyframes { keyframes } => Animated::Keyframes {
                keyframes: keyframes
                    .iter()
                    .map(|keyframe| Keyframe {
                        time: keyframe.time,
                        value: keyframe.value,
                        interpolation: keyframe.interpolation,
                    })
                    .collect(),
            },
        };
        let travel = map_animated(
            position,
            |point: Vec2| json!({"x": format!("{}px", (point.x - 0.5) * width), "y": format!("{}px", (point.y - 0.5) * height)}),
        );
        if !matches!(&moved.position, AnimatedPoint::Constant { value } if *value == center) {
            node["translate"] = to_json(&travel);
        }
        if moved.scale != constant(Vec2::new(1.0, 1.0)) {
            node["scale"] = to_json(&moved.scale);
        }
        node
    }

    /// Whether every visual clip of `lanes` can take the same matte with the result the whole
    /// would: pictures one at a time, joined by cuts or crossfades, none matted already.
    fn takes_one_matte(lanes: &Lanes, path: &str) -> Result<bool, ComposeError> {
        if lanes
            .visual
            .iter()
            .any(|lane| lane.captions.is_some() || lane.raw.is_some())
        {
            return Err(ComposeError::new(
                path,
                "captions and raw fragments go outside a matte or mask",
            ));
        }
        let transitions = lanes.visual.iter().flat_map(|lane| &lane.transitions);
        if let Some(transition) = transitions
            .clone()
            .find(|transition| !coverage_commutes(&transition.effect))
        {
            return Err(ComposeError::new(
                path,
                format!(
                    "a matte or mask over a sequence takes cuts and transitions that mix in place \
                     (`coverage_commutes` in schema/effects.v1.json); {} moves or adds to the \
                     picture under it",
                    transition.effect.id
                ),
            ));
        }
        let filled = lanes
            .visual
            .iter()
            .filter(|lane| !lane.clips.is_empty())
            .collect::<Vec<_>>();
        let matted = filled
            .iter()
            .flat_map(|lane| &lane.clips)
            .any(|clip| clip.matte.is_some());
        if matted {
            return Ok(false);
        }
        // Lanes that never show at once are one picture at a time.
        let overlaps = filled.iter().enumerate().any(|(index, lane)| {
            filled[index + 1..].iter().any(|other| {
                lane.clips.iter().any(|a| {
                    other.clips.iter().any(|b| {
                        a.timeline_range.start() < b.timeline_range.end()
                            && b.timeline_range.start() < a.timeline_range.end()
                    })
                })
            })
        });
        Ok(!overlaps)
    }

    /// Whether `lanes` hold exactly one visual clip that can be a matte.
    fn one_picture(lanes: &Lanes) -> bool {
        let clips = lanes
            .visual
            .iter()
            .flat_map(|lane| &lane.clips)
            .collect::<Vec<_>>();
        clips.len() == 1
            && clips[0].matte.is_none()
            && lanes
                .visual
                .iter()
                .all(|lane| lane.transitions.is_empty() && lane.captions.is_none())
    }

    /// `prim` drawn as one picture: a group of it alone.
    fn isolate(&mut self, prim: &Prim, slot: &Slot) -> Result<Lanes, ComposeError> {
        let group = GroupPrim {
            stack: StackPrim {
                head: Head {
                    path: prim.head().path.clone(),
                    ..Head::default()
                },
                children: vec![prim.clone()],
                inset: [0.0; 4],
                grid: None,
            },
            opacity: None,
            blend: alpha2_model::ClipBlendMode::Normal,
            effects: Vec::new(),
            transform: crate::lower::TransformPrim {
                position: None,
                scale: None,
                rotation: None,
                anchor: [0.5, 0.5],
            },
        };
        let mut lanes = Lanes::default();
        self.emit_group(&group, slot, &mut lanes)?;
        Ok(lanes)
    }

    /// The merged picture of `lanes`, placed over `0..len`, as nodes of `graph`.
    fn group_picture(
        &mut self,
        graph: &mut Graph,
        lanes: &Lanes,
        len: i64,
        path: &str,
    ) -> Result<NodeId, ComposeError> {
        if lanes.visual.iter().any(|lane| !lane.transitions.is_empty()) {
            return Err(ComposeError::new(
                path,
                "a group draws its children as one picture over one time, so it holds no \
                 transitions; put the group in the sequence instead",
            ));
        }
        if lanes
            .visual
            .iter()
            .any(|lane| lane.captions.is_some() || lane.raw.is_some())
        {
            return Err(ComposeError::new(
                path,
                "captions and raw fragments go outside a group",
            ));
        }
        let clips = lanes
            .visual
            .iter()
            .flat_map(|lane| &lane.clips)
            .collect::<Vec<_>>();
        let mattes = clips
            .iter()
            .filter_map(|clip| clip.matte.as_ref().map(|matte| matte.source.clone()))
            .collect::<HashSet<_>>();
        let mut layers = Vec::with_capacity(clips.len());
        for clip in &clips {
            if mattes.contains(&clip.id) {
                continue;
            }
            let mut node = self.group_layer(graph, clip, len, path)?;
            if let Some(matte) = &clip.matte {
                let matte_clip = clips
                    .iter()
                    .find(|other| other.id == matte.source)
                    .expect("a matte names a clip of the same lanes");
                let coverage = self.group_layer(graph, matte_clip, len, path)?;
                node = graph.add(NodeOp::Matte {
                    input: Some(node),
                    matte: Some(coverage),
                    mode: matte.mode,
                });
            }
            layers.push(MergeLayer {
                input: Some(node),
                blend_mode: clip.blend_mode,
                opacity: constant(1.0),
            });
        }
        Ok(graph.add(NodeOp::Merge { layers }))
    }

    /// One clip of a group as nodes: its picture at the group's time, its composition, masks
    /// and transform, shown only over its own range.
    fn group_layer(
        &self,
        graph: &mut Graph,
        clip: &Clip,
        len: i64,
        path: &str,
    ) -> Result<NodeId, ComposeError> {
        let source = self
            .sources
            .iter()
            .find(|source| source.id == clip.source_id)
            .expect("a clip's source is in the project");
        let (start, end) = (
            clip.timeline_range.start().ticks(),
            clip.timeline_range.end().ticks(),
        );
        let plays = matches!(clip.time_map, TimeMap::Forward)
            && clip.source_range.end().ticks() - clip.source_range.start().ticks() == end - start;
        let still = still_source(source);
        if !still && (!plays || start != 0 || end < len) {
            return Err(ComposeError::new(
                path,
                "a video or animated picture in a group plays at its own speed over the group's \
                 whole time; speed, loop, freeze and a later start or earlier end are not available \
                 in a group yet",
            ));
        }
        let picture = if source.kind == ModelKind::Composition {
            graph.add(NodeOp::Group {
                composition: source
                    .composition
                    .clone()
                    .expect("a composition source names its composition"),
                inputs: Vec::new(),
                controls: BTreeMap::new(),
            })
        } else {
            let from = (clip.source_range.start().ticks() - start).max(0);
            graph.add(NodeOp::Media {
                source_id: clip.source_id.clone(),
                start: MediaTime::from_ticks(if still { 0 } else { from }),
            })
        };
        let mut node = picture.clone();
        if let Some(used) = &clip.composition {
            let composition = self
                .compositions
                .iter()
                .find(|composition| composition.id == used.id)
                .expect("a clip's composition is in the project");
            if used.offset != MediaTime::ZERO || used.scale != TimeScale::ONE {
                return Err(ComposeError::new(
                    path,
                    "a picture in a group takes its composition at the group's own time",
                ));
            }
            node = graph.inline(composition, &picture);
        }
        if !clip.masks.is_empty() {
            node = graph.add(NodeOp::ShapeMask {
                input: Some(node),
                masks: clip.masks.clone(),
            });
        }
        let mut transform = clip.visual.clone();
        transform.opacity = windowed(&clip.visual.opacity, start, end, len);
        Ok(graph.add(NodeOp::Transform {
            input: Some(node),
            transform,
        }))
    }

    /// Puts the sound of `lanes`, placed from 0, on audio lanes of `out` from `start`: audio
    /// lanes as they are, and each video's sound read from its file.
    fn move_sound(&mut self, lanes: &Lanes, start: i64, out: &mut Lanes) {
        let shift = |clip: &mut Clip| {
            clip.timeline_range = range(
                clip.timeline_range.start().ticks() + start,
                clip.timeline_range.end().ticks() + start,
            );
        };
        for lane in &lanes.audio {
            let mut lane = lane.clone();
            lane.clips.iter_mut().for_each(shift);
            out.audio.push(lane);
        }
        for clip in lanes.visual.iter().flat_map(|lane| &lane.clips) {
            let Some(file) = self
                .sources
                .iter()
                .find(|source| source.id == clip.source_id && source.has_audio)
                .map(|source| source.locator.clone())
            else {
                continue;
            };
            if clip.audio.muted {
                continue;
            }
            let source = self.media_source(&file, ModelKind::Audio, true);
            let mut sound = Clip::new(
                self.clip_id(),
                source,
                clip.timeline_range,
                clip.source_range,
            );
            sound.audio = clip.audio.clone();
            sound.time_map = clip.time_map;
            shift(&mut sound);
            let mut lane = Lane::new(TrackKind::Audio, &BTreeSet::new());
            lane.clips.push(sound);
            out.audio.push(lane);
        }
    }

    /// `effect` over `input`, its named inputs drawn as nodes of the group.
    fn group_effect(
        &mut self,
        graph: &mut Graph,
        effect: &EffectPrim,
        input: NodeId,
        rect: Rect,
        len: i64,
    ) -> Result<NodeId, ComposeError> {
        let mut inputs = BTreeMap::new();
        for (name, prim) in &effect.inputs {
            let slot = Slot {
                start: 0,
                len,
                rect,
                hold: Hold::None,
                ramp: None,
                owners: BTreeSet::new(),
                motions: Vec::new(),
                pending: Vec::new(),
                delay: 0,
            };
            let mut lanes = Lanes::default();
            self.emit(prim, &slot, &mut lanes)?;
            let picture =
                self.group_picture(graph, &lanes, len, &format!("{}.{name}", effect.path))?;
            inputs.insert(name.clone(), Some(picture));
        }
        let whole = range(0, len);
        let params = effect
            .params
            .iter()
            .map(|(name, anim)| (name.clone(), animated(anim, whole)))
            .collect();
        Ok(graph.add(NodeOp::Effect {
            input: Some(input),
            inputs,
            effect: ClipEffect {
                id: effect.id.clone(),
                version: effect.version,
                params,
                resources: BTreeMap::new(),
            },
        }))
    }

    /// The group's opacity and movement over `0..len`, about its anchor in `rect`.
    fn group_transform(&self, group: &GroupPrim, rect: Rect, len: i64) -> VisualProperties {
        let (width, height) = self.canvas_px();
        let whole = range(0, len);
        let anchor = Vec2::new(
            (rect.x + group.transform.anchor[0] * rect.width) / width,
            (rect.y + group.transform.anchor[1] * rect.height) / height,
        );
        let position = group.transform.position.as_ref().map_or(
            AnimatedPoint::Constant { value: anchor },
            |position| {
                AnimatedPoint::from(map_animated(animated(position, whole), |[x, y]| {
                    Vec2::new(anchor.x + x / width, anchor.y + y / height)
                }))
            },
        );
        VisualProperties {
            correction: None,
            position,
            anchor,
            scale: group
                .transform
                .scale
                .as_ref()
                .map_or(constant(Vec2::new(1.0, 1.0)), |scale| {
                    map_animated(animated(scale, whole), |scale| Vec2::new(scale, scale))
                }),
            rotation_degrees: group
                .transform
                .rotation
                .as_ref()
                .map_or(constant(0.0), |rotation| animated(rotation, whole)),
            opacity: group
                .opacity
                .as_ref()
                .map_or(constant(1.0), |opacity| animated(opacity, whole)),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn emit_visual(
        &mut self,
        visual: &Visual,
        slot: &Slot,
        out: &mut Lanes,
    ) -> Result<(), ComposeError> {
        let rect = slot.rect.inset(visual.inset);
        let (start, end) = (slot.start, slot.start + slot.len);
        let opacity = Opacity {
            base: visual.opacity,
            start,
            end,
            fade_in: visual.fade_in,
            fade_out: visual.fade_out,
            ramp: slot.ramp,
        };
        let mut lane = Lane::new(TrackKind::Video, &slot.owners);
        match &visual.content {
            Content::Video(video) => {
                let source = self.media_source(&video.file, ModelKind::Video, video.has_audio);
                let natural = video.natural();
                let short = match video.when_short {
                    Some(policy) => policy,
                    None => match slot.hold {
                        Hold::Loop => WhenShort::Loop,
                        Hold::Freeze | Hold::None => WhenShort::Freeze,
                    },
                };
                let refuse = |what: &str| {
                    ComposeError::new(
                        &visual.head.path,
                        format!("the video is {what} than its node, and it says to refuse that"),
                    )
                };
                if slot.len > natural && !video.looped && short == WhenShort::Error {
                    return Err(refuse("shorter"));
                }
                if slot.len < natural && video.when_long == WhenLong::Error {
                    return Err(refuse("longer"));
                }
                let stretches = !video.looped
                    && ((slot.len > natural && short == WhenShort::Stretch)
                        || (slot.len < natural && video.when_long == WhenLong::Stretch));
                let looped = video.looped || (short == WhenShort::Loop && slot.len > natural);
                let mut clips = Vec::new();
                if stretches {
                    clips.push(Clip::new(
                        self.clip_id(),
                        source.clone(),
                        range(start, end),
                        range(video.trim_start, video.trim_end),
                    ));
                } else if looped {
                    #[allow(clippy::float_cmp)]
                    let sped = video.speed != 1.0;
                    if sped {
                        return Err(ComposeError::new(
                            &format!("{}.speed", visual.head.path),
                            "a video that loops (loop, when_short loop or a longest_loop grid) \
                             plays at speed 1",
                        ));
                    }
                    let mut clip = Clip::new(
                        self.clip_id(),
                        source,
                        range(start, end),
                        range(video.trim_start, video.trim_end),
                    );
                    clip.time_map = TimeMap::Loop {
                        mode: LoopMode::Repeat,
                    };
                    clips.push(clip);
                } else {
                    let play = slot.len.min(natural);
                    let source_end =
                        (video.trim_start + ticks_scaled(play, video.speed)).min(video.trim_end);
                    clips.push(Clip::new(
                        self.clip_id(),
                        source.clone(),
                        range(start, start + play),
                        range(video.trim_start, source_end.max(video.trim_start + 1)),
                    ));
                    if short == WhenShort::Freeze && slot.len > play {
                        let last = (video.trim_end - video.frame_ticks).max(video.trim_start);
                        let mut freeze = Clip::new(
                            self.clip_id(),
                            source,
                            range(start + play, end),
                            range(last, video.trim_end),
                        );
                        freeze.time_map = TimeMap::Freeze {
                            source_time: MediaTime::from_ticks(last),
                        };
                        clips.push(freeze);
                    }
                }
                let composition = video.blur.map(|radius| self.blur_composition(radius));
                for clip in &mut clips {
                    clip.visual = self.picture_visual(
                        video.size,
                        rect,
                        visual,
                        &opacity,
                        clip.timeline_range,
                    );
                    clip.masks = self.picture_masks(video.size, rect, visual, clip.timeline_range);
                    clip.composition = composition.clone().map(ClipComposition::new);
                    clip.audio.gain = constant(video.volume);
                    clip.audio.muted = video.muted;
                    // At speed 1 the document stays unchanged.
                    clip.audio.preserve_pitch = video.preserve_pitch;
                    clip.audio.preserve_pitch = clip.stretches_audio();
                }
                lane.clips = clips;
            }
            Content::Image { file, size, blur } => {
                let source = self.media_source(file, ModelKind::Image, false);
                let mut clip = Clip::new(
                    self.clip_id(),
                    source,
                    range(start, end),
                    range(0, slot.len),
                );
                clip.visual =
                    self.picture_visual(*size, rect, visual, &opacity, clip.timeline_range);
                clip.masks = self.picture_masks(*size, rect, visual, clip.timeline_range);
                clip.composition =
                    blur.map(|radius| ClipComposition::new(self.blur_composition(radius)));
                lane.clips.push(clip);
            }
            Content::Shape(shape) => {
                let items = self.shape_items(shape, rect, slot.len);
                let source = self.generated_source(Generator::Shapes { items });
                let mut clip = Clip::new(
                    self.clip_id(),
                    source,
                    range(start, end),
                    range(0, slot.len),
                );
                clip.visual.opacity = opacity.animation(clip.timeline_range);
                if slot.motions.iter().any(Ramp::scales) {
                    let (width, height) = self.canvas_px();
                    let point = Vec2::new(
                        (rect.x + 0.5 * rect.width) / width,
                        (rect.y + 0.5 * rect.height) / height,
                    );
                    clip.visual.position = AnimatedPoint::Constant { value: point };
                    clip.visual.anchor = point;
                }
                lane.clips.push(clip);
            }
            Content::Color(color) => {
                let (width, height) = self.canvas_px();
                let source = self.generated_source(Generator::Fill {
                    paint: Paint::Solid { color: *color },
                });
                let mut clip = Clip::new(
                    self.clip_id(),
                    source,
                    range(start, end),
                    range(0, slot.len),
                );
                clip.visual = VisualProperties {
                    correction: None,
                    position: AnimatedPoint::Constant {
                        value: Vec2::new(
                            (rect.x + 0.5 * rect.width) / width,
                            (rect.y + 0.5 * rect.height) / height,
                        ),
                    },
                    anchor: Vec2::new(0.5, 0.5),
                    scale: constant(Vec2::new(rect.width / width, rect.height / height)),
                    rotation_degrees: constant(0.0),
                    opacity: opacity.animation(clip.timeline_range),
                };
                lane.clips.push(clip);
            }
            Content::Layout(root) => {
                let (width, height) = self.canvas_px();
                let source = self.generated_source(Generator::Layout { root: root.clone() });
                let mut clip = Clip::new(
                    self.clip_id(),
                    source,
                    range(start, end),
                    range(0, slot.len),
                );
                let (x, y) = align_point(visual.align);
                let scale = (rect.width / width).min(rect.height / height);
                clip.visual = VisualProperties {
                    correction: None,
                    position: AnimatedPoint::Constant {
                        value: Vec2::new(
                            (rect.x + x * rect.width) / width,
                            (rect.y + y * rect.height) / height,
                        ),
                    },
                    anchor: Vec2::new(x, y),
                    scale: constant(Vec2::new(scale, scale)),
                    rotation_degrees: constant(0.0),
                    opacity: opacity.animation(clip.timeline_range),
                };
                lane.clips.push(clip);
            }
            Content::Text(text) => {
                let (width, height) = self.canvas_px();
                let (x, y) = align_point(visual.align);
                let font_px = text.font_px;
                let font_size = |px: f32| {
                    (px / self.short_side()).min(alpha2_model::MAX_TEXT_GENERATOR_FONT_SIZE)
                };
                let style = TextStyle {
                    font_family: text.family.clone(),
                    font_size: font_size(font_px),
                    font_size_fit: text.fit.map(|(min, max)| FontSizeFit {
                        min: font_size(min),
                        max: Some(font_size(max)),
                    }),
                    line_clamp: text.ellipsis.clone().map(|block_ellipsis| LineClamp {
                        max_lines: text.max_lines,
                        block_ellipsis,
                    }),
                    max_height: (text.fit.is_some() || text.ellipsis.is_some())
                        .then(|| (rect.height / height).clamp(0.001, 1.0)),
                    fill: Paint::Solid { color: text.color },
                    align: match x {
                        x if x < 0.25 => TextAlign::Start,
                        x if x > 0.75 => TextAlign::End,
                        _ => TextAlign::Center,
                    },
                    max_width: (rect.width / width).clamp(0.001, 1.0),
                    position: Vec2::new(
                        (rect.x + x * rect.width) / width,
                        (rect.y + y * rect.height) / height,
                    ),
                    anchor: Vec2::new(x, y),
                    strokes: text.strokes.clone(),
                    shadows: text.shadows.clone(),
                    background: text.background.map(|color| TextBackground {
                        color,
                        padding: 0.3,
                    }),
                    animators: text
                        .reveal
                        .map(|reveal| vec![reveal_animator(reveal, font_px, slot.len)])
                        .unwrap_or_default(),
                    ..TextStyle::default()
                };
                let text_point = style.position;
                let source = self.generated_source(Generator::Text {
                    text: text.text.clone().into(),
                    style: Box::new(style),
                });
                let mut clip = Clip::new(
                    self.clip_id(),
                    source,
                    range(start, end),
                    range(0, slot.len),
                );
                clip.visual.opacity = opacity.animation(clip.timeline_range);
                // A scale grows about the text, not about the canvas centre.
                if slot.motions.iter().any(Ramp::scales) {
                    clip.visual.position = AnimatedPoint::Constant { value: text_point };
                    clip.visual.anchor = text_point;
                }
                lane.clips.push(clip);
            }
            Content::Counter(counter) => {
                let root = self.counter_root(counter, rect, visual, start, slot.len);
                let source = self.generated_source(Generator::Layout {
                    root: Box::new(root),
                });
                let mut clip = Clip::new(
                    self.clip_id(),
                    source,
                    range(start, end),
                    range(0, slot.len),
                );
                clip.visual.opacity = opacity.animation(clip.timeline_range);
                let (x, y) = align_point(visual.align);
                if slot.motions.iter().any(Ramp::scales) {
                    let point = Vec2::new(
                        (rect.x + x * rect.width) / self.canvas_px().0,
                        (rect.y + y * rect.height) / self.canvas_px().1,
                    );
                    clip.visual.position = AnimatedPoint::Constant { value: point };
                    clip.visual.anchor = point;
                }
                lane.clips.push(clip);
            }
        }
        if !slot.motions.is_empty() {
            for clip in &mut lane.clips {
                let range = clip.timeline_range;
                self.apply_motion(&mut clip.visual, &opacity, &slot.motions, range);
            }
        }
        out.visual.push(lane);
        Ok(())
    }

    /// A shape's items placed in `rect`, animated over `0..len` of the picture's time.
    fn shape_items(&self, shape: &ShapeContent, rect: Rect, len: i64) -> Vec<ShapeItem> {
        shape_items_placed(shape, Placement::new(rect, self.canvas_px()), len)
    }

    /// Multiplies the entrances' and exits' opacity and scale into `visual` and adds their
    /// travel to its position, over `clip`.
    fn apply_motion(
        &self,
        visual: &mut VisualProperties,
        opacity: &Opacity,
        ramps: &[Ramp],
        clip: TimeRange,
    ) {
        let (start, end) = (clip.start().ticks(), clip.end().ticks());
        let times = |affects: fn(&Ramp) -> bool, base: &[i64]| {
            let mut times = BTreeSet::from([start, end]);
            times.extend(base.iter().copied());
            for ramp in ramps.iter().filter(|ramp| affects(ramp)) {
                times.extend([ramp.start, ramp.end]);
            }
            times
                .into_iter()
                .filter(|time| (start..=end).contains(time))
                .collect::<Vec<_>>()
        };
        let segment = |affects: fn(&Ramp) -> bool, base_changes: bool, a: i64, b: i64| {
            let mut active = ramps
                .iter()
                .filter(|ramp| affects(ramp) && ramp.start < b && ramp.end > a);
            match (active.next(), active.next()) {
                (None, _) => Segment::Eased(Interpolation::Linear),
                (Some(ramp), None) if !base_changes => Segment::Eased(match ramp.easing {
                    Easing::Linear => Interpolation::Linear,
                    Easing::CubicBezier(curve) => Interpolation::CubicBezier(curve),
                }),
                (Some(first), second) => {
                    if matches!(first.easing, Easing::Linear)
                        && second.is_none_or(|ramp| matches!(ramp.easing, Easing::Linear))
                        && ramps
                            .iter()
                            .filter(|ramp| affects(ramp) && ramp.start < b && ramp.end > a)
                            .all(|ramp| matches!(ramp.easing, Easing::Linear))
                    {
                        Segment::Eased(Interpolation::Linear)
                    } else {
                        Segment::Subdivide
                    }
                }
            }
        };
        if ramps.iter().any(Ramp::fades) {
            let at = |time: i64| {
                let factor = ramps.iter().map(|ramp| ramp.opacity(time)).product::<f64>();
                #[allow(clippy::cast_possible_truncation)]
                let value = (f64::from(opacity.at(time)) * factor) as f32;
                value
            };
            #[allow(clippy::float_cmp)]
            let base_changes = |a: i64, b: i64| opacity.at(a) != opacity.at(b);
            visual.opacity = keyed(&times(Ramp::fades, &opacity.breakpoints()), at, |a, b| {
                segment(Ramp::fades, base_changes(a, b), a, b)
            });
        }
        if ramps.iter().any(Ramp::travels)
            && let AnimatedPoint::Constant { value: base } = visual.position
        {
            let (width, height) = self.canvas_px();
            let at = |time: i64| {
                let [x, y] = ramps.iter().fold([0.0_f32, 0.0], |[x, y], ramp| {
                    let [dx, dy] = ramp.offset(time);
                    [x + dx, y + dy]
                });
                Vec2::new(base.x + x / width, base.y + y / height)
            };
            visual.position = AnimatedPoint::from(keyed(&times(Ramp::travels, &[]), at, |a, b| {
                segment(Ramp::travels, false, a, b)
            }));
        }
        if ramps.iter().any(Ramp::scales) {
            let base = visual.scale.clone();
            let at = |time: i64| {
                let factor = ramps.iter().map(|ramp| ramp.scale(time)).product::<f64>();
                let value = sample_linear(&base, time);
                #[allow(clippy::cast_possible_truncation)]
                let factor = factor as f32;
                Vec2::new(value.x * factor, value.y * factor)
            };
            let breakpoints = match &base {
                Animated::Constant { .. } | Animated::Data(_) => Vec::new(),
                Animated::Keyframes { keyframes } => keyframes
                    .iter()
                    .map(|keyframe| keyframe.time.ticks())
                    .collect(),
            };
            #[allow(clippy::float_cmp)]
            let base_changes = |a: i64, b: i64| sample_linear(&base, a) != sample_linear(&base, b);
            visual.scale = keyed(&times(Ramp::scales, &breakpoints), at, |a, b| {
                segment(Ramp::scales, base_changes(a, b), a, b)
            });
        }
    }

    /// A counter's layout tree: its text, keyed on the frame grid, placed in `rect` by padding the
    /// canvas-sized root.
    fn counter_root(
        &self,
        counter: &CounterContent,
        rect: Rect,
        visual: &Visual,
        start: i64,
        len: i64,
    ) -> LayoutNode {
        let (width, height) = self.canvas_px();
        let frame = self.rate.frame_start(1).map_or(1, MediaTime::ticks).max(1);
        let count = counter.count.unwrap_or(len - frame).clamp(1, len.max(1));
        let ease = |progress: f64| match counter.easing {
            Easing::Linear => progress,
            Easing::CubicBezier(curve) => curve.apply(progress),
        };
        let value_at = |picture: i64| {
            #[allow(clippy::cast_precision_loss)]
            let progress = (picture as f64 / count as f64).clamp(0.0, 1.0);
            format_number(
                counter.start + (counter.end - counter.start) * ease(progress),
                counter,
            )
        };
        // Frame starts on the timeline grid, as picture times from the clip's start.
        let mut keyframes: Vec<Keyframe<String>> = Vec::new();
        let mut push = |picture: i64, value: String| {
            if keyframes.last().is_none_or(|last| last.value != value) {
                keyframes.push(Keyframe {
                    time: MediaTime::from_ticks(picture),
                    value,
                    interpolation: Interpolation::Hold,
                });
            }
        };
        push(0, value_at(0));
        let first = self
            .rate
            .frame_at_or_before(MediaTime::from_ticks(start))
            .unwrap_or(0);
        for index in first.. {
            let Some(time) = self.rate.frame_start(index) else {
                break;
            };
            let picture = time.ticks() - start;
            if picture <= 0 {
                continue;
            }
            if picture >= count {
                break;
            }
            push(picture, value_at(picture));
        }
        if count < len {
            push(count, value_at(count));
        }
        let text = if keyframes.len() == 1 {
            constant(keyframes.remove(0).value)
        } else {
            Animated::Keyframes { keyframes }
        };
        let (x, y) = align_point(visual.align);
        let place = |fraction: f32| match fraction {
            f if f < 0.25 => "start",
            f if f > 0.75 => "end",
            _ => "center",
        };
        let pixels = |value: f32| format!("{}px", value.max(0.0));
        let mut leaf = serde_json::json!({
            "text": text,
            "font_size": constant(counter.font_px / self.short_side()),
            "fill": Paint::Solid { color: counter.color },
            "text_align": place(x),
        });
        if let Some(family) = &counter.family {
            leaf["font_family"] = serde_json::json!(family);
        }
        if !counter.strokes.is_empty() {
            leaf["strokes"] = serde_json::json!(counter.strokes);
        }
        if !counter.shadows.is_empty() {
            leaf["shadows"] = serde_json::json!(counter.shadows);
        }
        let root = serde_json::json!({
            "padding": constant(format!(
                "{} {} {} {}",
                pixels(rect.y),
                pixels(width - rect.x - rect.width),
                pixels(height - rect.y - rect.height),
                pixels(rect.x),
            )),
            "justify_content": place(x),
            "align_items": place(y),
            "children": [leaf],
        });
        serde_json::from_value(root).expect("a counter's layout tree is well formed")
    }

    /// Where a picture `size` pixels at scale 1 lies to fill `rect` as `visual` asks, and its
    /// opacity over `clip`.
    fn picture_visual(
        &self,
        size: (u32, u32),
        rect: Rect,
        visual: &Visual,
        opacity: &Opacity,
        clip: TimeRange,
    ) -> VisualProperties {
        let (width, height) = self.canvas_px();
        let fitted = Fitted::new(size, (width, height), rect, visual.fit);
        let (x, y) = align_point(visual.align);
        let scaled = |zoom: f32| Vec2::new(fitted.scale.x * zoom, fitted.scale.y * zoom);
        let scale = match &visual.zoom {
            None => constant(fitted.scale),
            Some(AnimPrim::Value(zoom)) => constant(scaled(*zoom)),
            Some(AnimPrim::Keys(keys)) => Animated::Keyframes {
                keyframes: resolve_keys(keys, clip)
                    .into_iter()
                    .map(|(time, zoom, easing)| Keyframe {
                        time,
                        value: scaled(zoom),
                        interpolation: interpolation(easing),
                    })
                    .collect(),
            },
        };
        VisualProperties {
            correction: None,
            position: AnimatedPoint::Constant {
                value: Vec2::new(
                    (rect.x + x * rect.width) / width,
                    (rect.y + y * rect.height) / height,
                ),
            },
            anchor: fitted.in_frame(Vec2::new(x, y)),
            scale,
            rotation_degrees: constant(0.0),
            opacity: opacity.animation(clip),
        }
    }

    /// The mask that crops a picture to `rect` where it overflows inside the canvas (the
    /// canvas crops the rest).
    fn picture_masks(
        &self,
        size: (u32, u32),
        rect: Rect,
        visual: &Visual,
        clip: TimeRange,
    ) -> Vec<ShapeMask> {
        let (width, height) = self.canvas_px();
        let fitted = Fitted::new(size, (width, height), rect, visual.fit);
        let (x, y) = align_point(visual.align);
        let full = rect == self.canvas_rect();
        let keys = match &visual.zoom {
            None => vec![(clip.start(), 1.0, Easing::Linear)],
            Some(AnimPrim::Value(zoom)) => vec![(clip.start(), *zoom, Easing::Linear)],
            Some(AnimPrim::Keys(keys)) => resolve_keys(keys, clip),
        };
        let overflows = |zoom: f32| {
            fitted.displayed.0 * zoom > rect.width * 1.0001
                || fitted.displayed.1 * zoom > rect.height * 1.0001
        };
        if full || !keys.iter().any(|(_, zoom, _)| overflows(*zoom)) {
            return Vec::new();
        }
        // The part of the picture inside `rect`, in its own unit square, at zoom `zoom`.
        let visible = |zoom: f32| {
            let (u, v) = (
                (rect.width / (fitted.displayed.0 * zoom)).min(1.0),
                (rect.height / (fitted.displayed.1 * zoom)).min(1.0),
            );
            (
                fitted.in_frame(Vec2::new(x + (0.5 - x) * u, y + (0.5 - y) * v)),
                Vec2::new(u * fitted.frame_part.x, v * fitted.frame_part.y),
            )
        };
        // Exactly equal factors make no animation.
        #[allow(clippy::float_cmp)]
        let still = keys.iter().all(|(_, zoom, _)| *zoom == keys[0].1);
        let (center, size) = if still {
            let (center, size) = visible(keys[0].1);
            (AnimatedPoint::Constant { value: center }, constant(size))
        } else {
            // The visible part shrinks as one over the zoom; eight linear pieces per key span
            // follow it closely enough.
            let steps = 8_i64;
            let mut samples = Vec::with_capacity(keys.len() * 8 + 1);
            for pair in keys.windows(2) {
                let ((start, from, easing), (end, to, _)) = (pair[0], pair[1]);
                let length = end.ticks() - start.ticks();
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                for step in 0..steps {
                    let progress = easing.apply(step as f64 / steps as f64) as f32;
                    let time = MediaTime::from_ticks(start.ticks() + length * step / steps);
                    samples.push((time, visible(from + (to - from) * progress)));
                }
            }
            let (end, last, _) = keys[keys.len() - 1];
            samples.push((end, visible(last)));
            (
                AnimatedPoint::from(Animated::Keyframes {
                    keyframes: samples
                        .iter()
                        .map(|(time, (center, _))| linear(*time, *center))
                        .collect(),
                }),
                Animated::Keyframes {
                    keyframes: samples
                        .iter()
                        .map(|(time, (_, size))| linear(*time, *size))
                        .collect(),
                },
            )
        };
        vec![ShapeMask {
            shape: ShapeGeometry::Rectangle(RectangleGeometry {
                center,
                size,
                corner_radius: constant(0.0),
                rotation_degrees: constant(0.0),
            }),
            feather: constant(0.0),
            invert: false,
            mode: alpha2_model::MaskMode::Add,
            fill_rule: alpha2_model::FillRule::default(),
        }]
    }

    fn emit_captions(&mut self, captions: &CaptionsPrim, slot: &Slot, out: &mut Lanes) {
        let end = slot.start + slot.len;
        let cues = captions
            .cues
            .iter()
            .filter_map(|cue| {
                let start = slot.start + cue.timeline_range.start().ticks();
                let stop = (slot.start + cue.timeline_range.end().ticks()).min(end);
                (start < stop).then(|| {
                    // The cue as given, moved to its place and cut at the slot's end.
                    let mut placed = cue.clone();
                    placed.timeline_range = range(start, stop);
                    crate::lower::clip_runs(&mut placed);
                    placed
                })
            })
            .collect::<Vec<_>>();
        if let Some((center, bar_height, color)) = captions.bar {
            let (width, _) = self.canvas_px();
            let source = self.generated_source(Generator::Fill {
                paint: Paint::Solid {
                    color: Rgba::new(color.red, color.green, color.blue, 1.0),
                },
            });
            let mut clip = Clip::new(
                self.clip_id(),
                source,
                range(slot.start, end),
                range(0, slot.len),
            );
            clip.visual = VisualProperties {
                correction: None,
                position: AnimatedPoint::Constant {
                    value: Vec2::new(0.5, center),
                },
                anchor: Vec2::new(0.5, 0.5),
                scale: constant(Vec2::new(1.0, bar_height)),
                rotation_degrees: constant(0.0),
                opacity: constant(color.alpha),
            };
            let _ = width;
            let mut lane = Lane::new(TrackKind::Video, &slot.owners);
            lane.clips.push(clip);
            out.visual.push(lane);
        }
        // Overlapping cues take a track each, as a caption track shows one at a time.
        let rows = alpha2_model::caption_rows(&cues).unwrap_or_else(|_| vec![cues]);
        for row in rows {
            let mut lane = Lane::new(TrackKind::Caption, &slot.owners);
            lane.captions = Some(Captions {
                from_words: None,
                style: captions.style.clone(),
                styles: captions.styles.clone(),
                cues: row,
            });
            out.visual.push(lane);
        }
    }

    fn emit_audio(&mut self, audio: &AudioPrim, slot: &Slot, out: &mut Lanes) {
        let source = self.media_source(&audio.file, ModelKind::Audio, true);
        let natural = audio.trim_end - audio.trim_start;
        let looped = audio.looped && slot.len > natural;
        let play = if looped {
            slot.len
        } else {
            slot.len.min(natural)
        };
        let source_range = if looped {
            range(audio.trim_start, audio.trim_end)
        } else {
            range(audio.trim_start, audio.trim_start + play)
        };
        let mut clip = Clip::new(
            self.clip_id(),
            source,
            range(slot.start, slot.start + play),
            source_range,
        );
        if looped {
            clip.time_map = TimeMap::Loop {
                mode: LoopMode::Repeat,
            };
        }
        clip.audio.gain = constant(audio.volume);
        clip.audio.fade_in = MediaTime::from_ticks(audio.fade_in.min(play));
        clip.audio.fade_out =
            MediaTime::from_ticks(audio.fade_out.min(play - clip.audio.fade_in.ticks()));
        let mut lane = Lane::new(TrackKind::Audio, &slot.owners);
        lane.clips.push(clip);
        lane.duck = audio
            .duck_under
            .clone()
            .map(|target| (target, audio.duck_depth_db, audio.head.path.clone()));
        out.audio.push(lane);
    }
}

fn transition_ticks(transition: &Transition) -> i64 {
    crate::lower::seconds_to_ticks(transition.duration)
}

fn is_crossfade(transition: &Transition) -> bool {
    matches!(
        transition.kind.as_str(),
        "crossfade" | "fade" | "alpha2.transition.fade"
    )
}

/// Whether `effect` mixes its two pictures at each pixel's own place and adds nothing, so a
/// matte over each is a matte over the mix.
fn coverage_commutes(effect: &ClipEffect) -> bool {
    alpha2_model::builtin_effect(&effect.id).is_some_and(|spec| spec.coverage_commutes)
}

fn transition_effect_id(kind: &str) -> String {
    match kind {
        "crossfade" | "fade" => "alpha2.transition.fade".to_owned(),
        kind if kind.contains('.') => kind.to_owned(),
        kind => format!("alpha2.transition.{kind}"),
    }
}

/// The cells of a grid of `count` children in `rect`, row by row.
#[allow(clippy::cast_precision_loss)]
fn grid_cells(rect: Rect, grid: GridLayout, count: usize) -> Vec<Rect> {
    let columns = grid.columns.max(1) as usize;
    let rows = count.div_ceil(columns).max(1);
    let width = ((rect.width - grid.gap * (columns - 1) as f32) / columns as f32).max(1.0);
    let height = ((rect.height - grid.gap * (rows - 1) as f32) / rows as f32).max(1.0);
    (0..count)
        .map(|index| Rect {
            x: rect.x + (index % columns) as f32 * (width + grid.gap),
            y: rect.y + (index / columns) as f32 * (height + grid.gap),
            width,
            height,
        })
        .collect()
}

/// A picture fitted into an area.
///
/// A clip's source is a canvas-sized frame the engine contain-places its picture in. The
/// clip's anchor and masks are points of that frame, and its scale is that frame's.
struct Fitted {
    /// The clip scale that gives it.
    scale: Vec2,
    /// Its size in canvas pixels, unzoomed.
    displayed: (f32, f32),
    /// The part of the frame the picture covers: its size and start, as fractions of the frame.
    frame_part: Vec2,
    frame_offset: Vec2,
}

impl Fitted {
    /// A picture `size` pixels, contain-placed in a `canvas`-sized frame, fitted into `rect`
    /// as `fit` says.
    #[allow(clippy::cast_precision_loss)]
    fn new(size: (u32, u32), canvas: (f32, f32), rect: Rect, fit: Fit) -> Self {
        let (width, height) = (size.0.max(1) as f32, size.1.max(1) as f32);
        let unit = (canvas.0 / width).min(canvas.1 / height);
        let frame_part = Vec2::new(width * unit / canvas.0, height * unit / canvas.1);
        let frame_offset = Vec2::new(0.5 * (1.0 - frame_part.x), 0.5 * (1.0 - frame_part.y));
        let fitted = |scale: Vec2, displayed: (f32, f32)| Self {
            scale,
            displayed,
            frame_part,
            frame_offset,
        };
        let uniform = |factor: f32| {
            fitted(
                Vec2::new(factor / unit, factor / unit),
                (width * factor, height * factor),
            )
        };
        match fit {
            Fit::Contain => uniform((rect.width / width).min(rect.height / height)),
            Fit::Cover => uniform((rect.width / width).max(rect.height / height)),
            Fit::None => uniform(1.0),
            Fit::Fill => fitted(
                Vec2::new(rect.width / width / unit, rect.height / height / unit),
                (rect.width, rect.height),
            ),
        }
    }

    /// The frame point at `point` of the picture.
    fn in_frame(&self, point: Vec2) -> Vec2 {
        Vec2::new(
            self.frame_offset.x + point.x * self.frame_part.x,
            self.frame_offset.y + point.y * self.frame_part.y,
        )
    }
}

/// `keys` placed on `clip`: each key's timeline time, value and curve to the next. A key at the
/// node's end that would not follow the key before it falls one tick after it.
fn resolve_keys<T: Clone>(keys: &[KeyPrim<T>], clip: TimeRange) -> Vec<(MediaTime, T, Easing)> {
    let length = clip.end().ticks() - clip.start().ticks();
    let mut previous = None;
    keys.iter()
        .map(|key| {
            let mut ticks = match key.at {
                KeyTime::Ticks(ticks) => ticks,
                KeyTime::End => length,
            };
            if let Some(previous) = previous
                && ticks <= previous
            {
                ticks = previous + 1;
            }
            previous = Some(ticks);
            (
                MediaTime::from_ticks(clip.start().ticks() + ticks),
                key.value.clone(),
                key.easing,
            )
        })
        .collect()
}

/// `anim` as a property animation over `clip`.
fn animated<T: Copy>(anim: &AnimPrim<T>, clip: TimeRange) -> Animated<T> {
    match anim {
        AnimPrim::Value(value) => constant(*value),
        AnimPrim::Keys(keys) => Animated::Keyframes {
            keyframes: resolve_keys(keys, clip)
                .into_iter()
                .map(|(time, value, easing)| Keyframe {
                    time,
                    value,
                    interpolation: interpolation(easing),
                })
                .collect(),
        },
    }
}

fn map_animated<T, U>(animation: Animated<T>, map: impl Fn(T) -> U) -> Animated<U> {
    match animation {
        Animated::Constant { value } => Animated::Constant { value: map(value) },
        Animated::Data(binding) => Animated::Data(binding),
        Animated::Keyframes { keyframes } => Animated::Keyframes {
            keyframes: keyframes
                .into_iter()
                .map(|keyframe| Keyframe {
                    time: keyframe.time,
                    value: map(keyframe.value),
                    interpolation: keyframe.interpolation,
                })
                .collect(),
        },
    }
}

/// `opacity` shown over `start..end` of `0..len` and zero outside it.
fn windowed(opacity: &Animated<f32>, start: i64, end: i64, len: i64) -> Animated<f32> {
    if start <= 0 && end >= len {
        return opacity.clone();
    }
    let hold = |time: i64, value: f32| Keyframe {
        time: MediaTime::from_ticks(time),
        value,
        interpolation: Interpolation::Hold,
    };
    let mut keyframes = Vec::new();
    if start > 0 {
        keyframes.push(hold(0, 0.0));
    }
    match opacity {
        Animated::Constant { value } => keyframes.push(hold(start, *value)),
        Animated::Data(_) => keyframes.push(hold(start, sample_scalar(opacity, start))),
        Animated::Keyframes { keyframes: own } => {
            let inside = own
                .iter()
                .filter(|keyframe| (start..end).contains(&keyframe.time.ticks()))
                .cloned()
                .collect::<Vec<_>>();
            if inside
                .first()
                .is_none_or(|first| first.time.ticks() > start)
            {
                keyframes.push(hold(start, sample_scalar(opacity, start)));
            }
            keyframes.extend(inside);
        }
    }
    if end < len {
        if let Some(last) = keyframes.last_mut() {
            last.interpolation = Interpolation::Hold;
        }
        keyframes.push(hold(end, 0.0));
    }
    Animated::Keyframes { keyframes }
}

/// `animation` at `time`, linear between keys, holding outside them.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn sample_scalar(animation: &Animated<f32>, time: i64) -> f32 {
    match animation {
        Animated::Constant { value } => *value,
        Animated::Data(_) => 0.0,
        Animated::Keyframes { keyframes } => {
            let after = keyframes.partition_point(|keyframe| keyframe.time.ticks() <= time);
            match (
                after.checked_sub(1).map(|index| &keyframes[index]),
                keyframes.get(after),
            ) {
                (Some(before), Some(next)) if before.interpolation != Interpolation::Hold => {
                    let span = (next.time.ticks() - before.time.ticks()) as f64;
                    let progress = (time - before.time.ticks()) as f64 / span;
                    (f64::from(before.value)
                        + (f64::from(next.value) - f64::from(before.value)) * progress)
                        as f32
                }
                (Some(before), _) => before.value,
                (None, Some(next)) => next.value,
                (None, None) => 1.0,
            }
        }
    }
}

/// Whether `source` shows the same picture at every time: a still, or a generated picture with
/// nothing keyed or animated.
fn still_source(source: &MediaSource) -> bool {
    match source.kind {
        ModelKind::Image => true,
        ModelKind::Generated => {
            source
                .generator
                .as_ref()
                .is_some_and(|generator| match generator {
                    Generator::Fill { .. } => true,
                    Generator::Text { style, .. } => style.animators.is_empty(),
                    other => !serde_json::to_string(other)
                        .is_ok_and(|text| text.contains("\"keyframes\"")),
                })
        }
        _ => false,
    }
}

/// A shape's outlines, trim, stroke and fill as shape items, placed by `place`, animated over
/// `0..len` of the picture's time.
fn shape_items_placed(shape: &ShapeContent, place: Placement, len: i64) -> Vec<ShapeItem> {
    let whole = range(0, len);
    let length =
        |anim: &AnimPrim<f32>| map_animated(animated(anim, whole), |value| place.length(value));
    let plain = |anim: &AnimPrim<f32>| animated(anim, whole);
    let mut items = shape
        .geometry
        .iter()
        .map(|geometry| match place.geometry(geometry, whole) {
            ShapeGeometry::Rectangle(rectangle) => ShapeItem::Rectangle(rectangle),
            ShapeGeometry::Ellipse(ellipse) => ShapeItem::Ellipse(ellipse),
            ShapeGeometry::Polystar(polystar) => ShapeItem::Polystar(polystar),
            ShapeGeometry::Path(path) => ShapeItem::Path(path),
        })
        .collect::<Vec<_>>();
    if let Some(trim) = &shape.trim {
        items.push(ShapeItem::Trim(TrimItem {
            start: plain(&trim.start),
            end: plain(&trim.end),
            offset: plain(&trim.offset),
            mode: TrimMode::default(),
        }));
    }
    if let Some(stroke) = &shape.stroke {
        items.push(ShapeItem::Stroke(StrokeItem {
            paint: constant(place.paint(&stroke.paint)),
            width: length(&stroke.width),
            cap: stroke.cap,
            join: stroke.join,
            dash: (!stroke.dash.is_empty()).then(|| Dash {
                pattern: stroke
                    .dash
                    .iter()
                    .map(|dash| constant(place.length(*dash)))
                    .collect(),
                offset: constant(0.0),
            }),
            opacity: constant(1.0),
            miter_limit: 4.0,
        }));
    }
    if let Some(fill) = &shape.fill {
        items.push(ShapeItem::Fill {
            paint: constant(place.paint(fill)),
            opacity: constant(1.0),
            rule: FillRule::default(),
        });
    }
    items
}

/// Node numbers in paint order, placed children, and the enclosing boxes that hide their
/// overflow, collected while a box is built.
#[derive(Default)]
struct BoxBuild {
    count: usize,
    places: Vec<Place>,
    /// Boxes that hide their overflow, by number, with their corner radius in pixels.
    clips: Vec<(usize, f32)>,
}

impl BoxBuild {
    /// The next node's number; the root is 0 and the box 1.
    fn next(&mut self) -> usize {
        self.count += 1;
        self.count
    }
}

/// A child a box places and draws in its measured box.
struct Place {
    index: usize,
    ancestors: Vec<usize>,
    path: String,
    prim: Prim,
    /// Its corner radius in pixels.
    radius: f32,
    /// The boxes around it that hide their overflow.
    clips: Vec<(usize, f32)>,
}

/// A layout tree's nodes in paint order.
fn preorder(root: &LayoutNode) -> Vec<&LayoutNode> {
    let mut nodes = vec![root];
    let mut index = 0;
    while index < nodes.len() {
        let node = nodes[index];
        let at = index + 1;
        let mut children = node.children.iter().collect::<Vec<_>>();
        // Depth first: a node's children come right after it.
        let tail = nodes.split_off(at);
        nodes.append(&mut children);
        nodes.extend(tail);
        index += 1;
    }
    nodes
}

/// Makes every node outside `keep` paint nothing without moving anything: leaves turn
/// transparent, boxes drop their background and border paint.
fn hide_outside(node: &mut LayoutNode, counter: &mut usize, keep: std::ops::Range<usize>) {
    let index = *counter;
    *counter += 1;
    if !keep.contains(&index) {
        if node.children.is_empty() {
            node.opacity = Some(constant(0.0));
        } else {
            node.background = None;
            if let Some(border) = &mut node.border {
                border.paint = constant(Paint::Solid {
                    color: Rgba::new(0.0, 0.0, 0.0, 0.0),
                });
            }
        }
    }
    for child in &mut node.children {
        hide_outside(child, counter, keep.clone());
    }
}

/// The layout box fields that animate.
const ANIMATED_LAYOUT_FIELDS: [&str; 22] = [
    "background",
    "border_radius",
    "column_gap",
    "flex_basis",
    "flex_grow",
    "flex_shrink",
    "font_size",
    "gap",
    "height",
    "margin",
    "max_height",
    "max_width",
    "min_height",
    "min_width",
    "opacity",
    "padding",
    "rotation_degrees",
    "row_gap",
    "scale",
    "text",
    "translate",
    "width",
];

/// Writes every animated field of a layout tree as the engine reads it: a bare value as a
/// constant, a compose ramp or keys as keyframes over `0..len`, an engine animation as it is.
fn normalize_layout(
    node: &mut serde_json::Value,
    len: i64,
    path: &str,
) -> Result<(), ComposeError> {
    let Some(object) = node.as_object_mut() else {
        return Ok(());
    };
    for field in ANIMATED_LAYOUT_FIELDS {
        if let Some(value) = object.get_mut(field) {
            *value = layout_animation(value.take(), len, &format!("{path}.{field}"))?;
        }
    }
    if let Some(border) = object
        .get_mut("border")
        .and_then(serde_json::Value::as_object_mut)
    {
        for field in ["width", "paint"] {
            if let Some(value) = border.get_mut(field) {
                *value = layout_animation(value.take(), len, &format!("{path}.border.{field}"))?;
            }
        }
    }
    if let Some(children) = object
        .get_mut("children")
        .and_then(serde_json::Value::as_array_mut)
    {
        for (index, child) in children.iter_mut().enumerate() {
            normalize_layout(child, len, &format!("{path}.children[{index}]"))?;
        }
    }
    Ok(())
}

fn layout_animation(
    value: serde_json::Value,
    len: i64,
    path: &str,
) -> Result<serde_json::Value, ComposeError> {
    let object = value.as_object();
    if value.is_null() || object.is_some_and(|object| object.contains_key("mode")) {
        return Ok(value);
    }
    if !object.is_some_and(|object| object.contains_key("from") || object.contains_key("keys")) {
        return Ok(json!({"mode": "constant", "value": value}));
    }
    let anim: crate::spec::Anim<serde_json::Value> = serde_json::from_value(value)
        .map_err(|error| ComposeError::new(path, error.to_string()))?;
    Ok(match Context::anim(path, &anim, |_| true)? {
        AnimPrim::Value(value) => json!({"mode": "constant", "value": value}),
        AnimPrim::Keys(keys) => json!({
            "mode": "keyframes",
            "keyframes": resolve_keys(&keys, range(0, len))
                .into_iter()
                .map(|(time, value, easing)| json!({
                    "time": time.ticks(),
                    "value": value,
                    "interpolation": to_json(&interpolation(easing)),
                }))
                .collect::<Vec<_>>(),
        }),
    })
}

/// The primitives a box's length depends on: its placed children and its leaves.
fn box_prims(tree: &BoxTree) -> Vec<Prim> {
    tree.children
        .iter()
        .flat_map(|kid| match &kid.content {
            BoxContent::Tree(inner) => box_prims(inner),
            BoxContent::Native(visual) => vec![Prim::Visual(visual.clone())],
            BoxContent::Foreign(prim) => vec![prim.clone()],
        })
        .collect()
}

fn to_json<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).expect("a model value serializes")
}

/// Maps an area's unit square, and lengths in its shorter side, to canvas fractions.
#[derive(Clone, Copy)]
struct Placement {
    rect: Rect,
    canvas: (f32, f32),
}

impl Placement {
    const fn new(rect: Rect, canvas: (f32, f32)) -> Self {
        Self { rect, canvas }
    }

    fn point(self, [x, y]: [f32; 2]) -> Vec2 {
        Vec2::new(
            (self.rect.x + x * self.rect.width) / self.canvas.0,
            (self.rect.y + y * self.rect.height) / self.canvas.1,
        )
    }

    fn size(self, [x, y]: [f32; 2]) -> Vec2 {
        Vec2::new(
            x * self.rect.width / self.canvas.0,
            y * self.rect.height / self.canvas.1,
        )
    }

    fn length(self, length: f32) -> f32 {
        length * self.rect.width.min(self.rect.height) / self.canvas.0.min(self.canvas.1)
    }

    fn path(self, path: &alpha2_model::Path) -> alpha2_model::Path {
        let mut placed = path.clone();
        for vertex in placed
            .subpaths
            .iter_mut()
            .flat_map(|subpath| &mut subpath.vertices)
        {
            vertex.point = self.point([vertex.point.x, vertex.point.y]);
            vertex.in_tangent = self.size([vertex.in_tangent.x, vertex.in_tangent.y]);
            vertex.out_tangent = self.size([vertex.out_tangent.x, vertex.out_tangent.y]);
        }
        placed
    }

    /// An outline placed on the canvas, animated over `clip`.
    fn geometry(self, geometry: &GeometryPrim, clip: TimeRange) -> ShapeGeometry {
        let point = |anim: &AnimPrim<[f32; 2]>| {
            AnimatedPoint::from(map_animated(animated(anim, clip), |value| {
                self.point(value)
            }))
        };
        let size = |anim: &AnimPrim<[f32; 2]>| {
            map_animated(animated(anim, clip), |value| self.size(value))
        };
        let length =
            |anim: &AnimPrim<f32>| map_animated(animated(anim, clip), |value| self.length(value));
        match geometry {
            GeometryPrim::Rect {
                center,
                size: extent,
                corner,
                rotation,
            } => ShapeGeometry::Rectangle(RectangleGeometry {
                center: point(center),
                size: size(extent),
                corner_radius: length(corner),
                rotation_degrees: animated(rotation, clip),
            }),
            GeometryPrim::Ellipse {
                center,
                size: extent,
                rotation,
            } => ShapeGeometry::Ellipse(EllipseGeometry {
                center: point(center),
                size: size(extent),
                rotation_degrees: animated(rotation, clip),
            }),
            #[allow(clippy::cast_precision_loss)]
            GeometryPrim::Polystar {
                kind,
                center,
                points,
                outer,
                inner,
                roundness,
                rotation,
            } => ShapeGeometry::Polystar(PolystarGeometry {
                kind: *kind,
                center: point(center),
                points: constant(*points as f32),
                outer_radius: length(outer),
                inner_radius: length(inner),
                outer_roundness: animated(roundness, clip),
                inner_roundness: constant(0.0),
                rotation_degrees: animated(rotation, clip),
            }),
            GeometryPrim::Path(path) => ShapeGeometry::Path(PathGeometry {
                path: constant(self.path(path)),
            }),
        }
    }

    fn paint(self, paint: &PaintPrim) -> Paint {
        let stops = |stops: &[(f32, Rgba)]| {
            stops
                .iter()
                .map(|(offset, color)| GradientStop {
                    offset: *offset,
                    color: *color,
                })
                .collect::<Vec<_>>()
        };
        match paint {
            PaintPrim::Solid(color) => Paint::Solid { color: *color },
            PaintPrim::Linear {
                stops: colors,
                angle,
            } => {
                // The CSS gradient line: through the area's centre along `angle`, as long as the
                // area is across that direction.
                let (sin, cos) = libm::sincosf(angle.to_radians());
                let (width, height) = (self.rect.width, self.rect.height);
                let half = f32::midpoint(width * sin.abs(), height * cos.abs());
                let center = (self.rect.x + width / 2.0, self.rect.y + height / 2.0);
                let at = |sign: f32| {
                    Vec2::new(
                        (center.0 + sign * sin * half) / self.canvas.0,
                        (center.1 - sign * cos * half) / self.canvas.1,
                    )
                };
                Paint::LinearGradient {
                    start: at(-1.0),
                    end: at(1.0),
                    stops: stops(colors),
                    spread: Spread::default(),
                }
            }
            PaintPrim::Radial {
                stops: colors,
                center,
                radius,
            } => Paint::RadialGradient {
                center: self.point(*center),
                radius: self.length(*radius),
                stops: stops(colors),
                spread: Spread::default(),
            },
            PaintPrim::Conic {
                stops: colors,
                center,
                angle,
            } => Paint::ConicGradient {
                center: self.point(*center),
                angle_degrees: *angle,
                stops: stops(colors),
            },
        }
    }
}

/// The nodes of one group composition, with fresh ids.
#[derive(Default)]
struct Graph {
    nodes: Vec<CompositionNode>,
    next: u32,
}

impl Graph {
    fn id(&mut self) -> NodeId {
        self.next += 1;
        NodeId::new(format!("n{}", self.next)).expect("a generated id is not empty")
    }

    fn add(&mut self, op: NodeOp) -> NodeId {
        let id = self.id();
        self.nodes.push(CompositionNode::new(id.clone(), op));
        id
    }

    /// Copies `composition`'s nodes in with fresh ids, its clip node read as `picture`; returns
    /// its output.
    fn inline(&mut self, composition: &Composition, picture: &NodeId) -> NodeId {
        let mut ids = BTreeMap::new();
        for node in &composition.nodes {
            let id = if matches!(node.op, NodeOp::Clip {}) {
                picture.clone()
            } else {
                self.id()
            };
            ids.insert(node.id.clone(), id);
        }
        for node in &composition.nodes {
            if matches!(node.op, NodeOp::Clip {}) {
                continue;
            }
            let mut op = node.op.clone();
            remap_inputs(&mut op, &ids);
            let mut copy = CompositionNode::new(ids[&node.id].clone(), op);
            copy.bypass = node.bypass;
            self.nodes.push(copy);
        }
        composition
            .output
            .as_ref()
            .and_then(|output| ids.get(output))
            .cloned()
            .unwrap_or_else(|| picture.clone())
    }
}

/// Renames every input `op` reads through `ids`.
fn remap_inputs(op: &mut NodeOp, ids: &BTreeMap<NodeId, NodeId>) {
    let rename = |input: &mut Option<NodeId>| {
        if let Some(id) = input.as_mut()
            && let Some(renamed) = ids.get(id)
        {
            *id = renamed.clone();
        }
    };
    match op {
        NodeOp::Effect { input, inputs, .. } => {
            rename(input);
            inputs.values_mut().for_each(rename);
        }
        NodeOp::Transition { from, to, .. } => {
            rename(from);
            rename(to);
        }
        NodeOp::Transform { input, .. } | NodeOp::ShapeMask { input, .. } => rename(input),
        NodeOp::Merge { layers } => layers.iter_mut().for_each(|layer| rename(&mut layer.input)),
        NodeOp::Matte { input, matte, .. } => {
            rename(input);
            rename(matte);
        }
        NodeOp::Group { inputs, .. } => inputs.iter_mut().for_each(rename),
        _ => {}
    }
}

const fn interpolation(easing: Easing) -> Interpolation {
    match easing {
        Easing::Linear => Interpolation::Linear,
        Easing::CubicBezier(curve) => Interpolation::CubicBezier(curve),
    }
}

const fn linear<T>(time: MediaTime, value: T) -> Keyframe<T> {
    Keyframe {
        time,
        value,
        interpolation: Interpolation::Linear,
    }
}

/// How a keyframe segment of a motion property is written.
enum Segment {
    /// One key at the segment's start with this interpolation.
    Eased(Interpolation),
    /// Linear pieces, where curves combine and no one interpolation is exact.
    Subdivide,
}

/// Linear pieces one subdivided segment is written as.
const SUBDIVISIONS: i64 = 8;

/// `value` keyed at `times`, each segment as `segment` says; a constant when it never changes.
fn keyed<T: Clone + PartialEq>(
    times: &[i64],
    value: impl Fn(i64) -> T,
    segment: impl Fn(i64, i64) -> Segment,
) -> Animated<T> {
    let mut keyframes = Vec::new();
    for pair in times.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        match segment(a, b) {
            Segment::Eased(interpolation) => keyframes.push(Keyframe {
                time: MediaTime::from_ticks(a),
                value: value(a),
                interpolation,
            }),
            Segment::Subdivide => {
                let pieces = SUBDIVISIONS.min(b - a).max(1);
                for piece in 0..pieces {
                    let time = a + (b - a) * piece / pieces;
                    keyframes.push(linear(MediaTime::from_ticks(time), value(time)));
                }
            }
        }
    }
    if let Some(&last) = times.last() {
        keyframes.push(linear(MediaTime::from_ticks(last), value(last)));
    }
    if keyframes
        .windows(2)
        .all(|pair| pair[0].value == pair[1].value)
    {
        return constant(keyframes.swap_remove(0).value);
    }
    Animated::Keyframes { keyframes }
}

/// `animation` at `time`, its keyframes read as linear.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn sample_linear(animation: &Animated<Vec2>, time: i64) -> Vec2 {
    match animation {
        Animated::Constant { value } => *value,
        Animated::Data(_) => Vec2::default(),
        Animated::Keyframes { keyframes } => {
            let after = keyframes.partition_point(|keyframe| keyframe.time.ticks() <= time);
            match (
                after.checked_sub(1).map(|index| &keyframes[index]),
                keyframes.get(after),
            ) {
                (Some(before), Some(next)) => {
                    let (t0, t1) = (before.time.ticks(), next.time.ticks());
                    let u = ((time - t0) as f64 / (t1 - t0) as f64) as f32;
                    Vec2::new(
                        before.value.x + (next.value.x - before.value.x) * u,
                        before.value.y + (next.value.y - before.value.y) * u,
                    )
                }
                (Some(only), None) | (None, Some(only)) => only.value,
                (None, None) => Vec2::new(1.0, 1.0),
            }
        }
    }
}

/// One entrance or exit at timeline times: opacity, travel (canvas pixels) and scale move from
/// their first value to their second over `start..end` along `easing`.
#[derive(Clone, Copy, Debug)]
struct Ramp {
    start: i64,
    end: i64,
    easing: Easing,
    opacity: [f32; 2],
    offset: [[f32; 2]; 2],
    scale: [f32; 2],
}

impl Ramp {
    fn enter(preset: &Preset, start: i64, end: i64) -> Self {
        let [x, y] = preset.travel;
        Self {
            start,
            end,
            easing: preset.easing,
            opacity: [if preset.fade { 0.0 } else { 1.0 }, 1.0],
            offset: [[-x, -y], [0.0, 0.0]],
            scale: [preset.scale.unwrap_or(1.0), 1.0],
        }
    }

    fn exit(preset: &Preset, start: i64, end: i64) -> Self {
        Self {
            start,
            end,
            easing: preset.easing,
            opacity: [1.0, if preset.fade { 0.0 } else { 1.0 }],
            offset: [[0.0, 0.0], preset.travel],
            scale: [1.0, preset.scale.unwrap_or(1.0)],
        }
    }

    #[allow(clippy::float_cmp)]
    fn fades(&self) -> bool {
        self.opacity[0] != self.opacity[1]
    }

    #[allow(clippy::float_cmp)]
    fn travels(&self) -> bool {
        self.offset[0] != self.offset[1]
    }

    #[allow(clippy::float_cmp)]
    fn scales(&self) -> bool {
        self.scale[0] != self.scale[1]
    }

    #[allow(clippy::cast_precision_loss)]
    fn progress(&self, time: i64) -> f64 {
        if time <= self.start {
            return 0.0;
        }
        if time >= self.end {
            return 1.0;
        }
        let linear = (time - self.start) as f64 / (self.end - self.start) as f64;
        match self.easing {
            Easing::Linear => linear,
            Easing::CubicBezier(curve) => curve.apply(linear),
        }
    }

    fn mix(&self, [from, to]: [f32; 2], time: i64) -> f64 {
        f64::from(from) + (f64::from(to) - f64::from(from)) * self.progress(time)
    }

    fn opacity(&self, time: i64) -> f64 {
        self.mix(self.opacity, time).clamp(0.0, 1.0)
    }

    fn scale(&self, time: i64) -> f64 {
        self.mix(self.scale, time).max(0.0)
    }

    #[allow(clippy::cast_possible_truncation)]
    fn offset(&self, time: i64) -> [f32; 2] {
        let [from, to] = self.offset;
        [
            self.mix([from[0], to[0]], time) as f32,
            self.mix([from[1], to[1]], time) as f32,
        ]
    }
}

/// The Range animator that reveals a text unit by unit: unit `i` is fully selected (hidden, and
/// moved down by the rise) until `i * stagger`, and unselected `duration` later. The selector's
/// offset sweeps linearly, so each unit changes at a constant rate.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn reveal_animator(reveal: RevealPrim, font_px: f32, len: i64) -> TextAnimator {
    let stagger = reveal.stagger as f64;
    let width = reveal.duration as f64 / stagger;
    let offset = |picture: f64| (picture / stagger - width + 0.5) as f32;
    let last = (len as f64)
        .min((f64::from(alpha2_model::MAX_RANGE_INDEX) * 2.0 - 0.5 + width) * stagger)
        .max(1.0);
    TextAnimator {
        selector: TextSelector::Range {
            based_on: reveal.unit,
            units: RangeUnits::Index,
            start: constant(0.0),
            end: constant(width as f32),
            offset: Animated::Keyframes {
                keyframes: vec![
                    linear(MediaTime::from_ticks(0), offset(0.0)),
                    linear(MediaTime::from_ticks(last as i64), offset(last.floor())),
                ],
            },
            shape: RangeShape::RampUp,
            amount: constant(1.0),
        },
        properties: TextAnimatorProperties {
            opacity: Some(constant(0.0)),
            offset: (reveal.rise > 0.0).then(|| constant(Vec2::new(0.0, reveal.rise / font_px))),
            ..TextAnimatorProperties::default()
        },
    }
}

/// `value` rounded to the counter's decimals and written in its format.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn format_number(value: f64, counter: &CounterContent) -> String {
    let scale = 10_u64.pow(counter.decimals);
    #[allow(clippy::cast_precision_loss)]
    let scaled = (value * scale as f64).round();
    let negative = scaled < 0.0;
    let magnitude = scaled.abs() as u128;
    let whole = (magnitude / u128::from(scale)).to_string();
    let mut grouped = String::new();
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index).is_multiple_of(3) {
            grouped.push_str(&counter.thousands);
        }
        grouped.push(digit);
    }
    let mut text = counter.prefix.clone();
    if negative {
        text.push('-');
    }
    text.push_str(&grouped);
    if counter.decimals > 0 {
        text.push_str(&counter.point);
        let fraction = magnitude % u128::from(scale);
        let fraction = fraction.to_string();
        for _ in fraction.len()..counter.decimals as usize {
            text.push('0');
        }
        text.push_str(&fraction);
    }
    text.push_str(&counter.suffix);
    text
}

/// A picture's opacity: its own, its node's end fades, and a crossfade's ramp.
struct Opacity {
    base: f32,
    start: i64,
    end: i64,
    fade_in: i64,
    fade_out: i64,
    ramp: Option<(i64, i64)>,
}

impl Opacity {
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn at(&self, time: i64) -> f32 {
        let ramp = |from: i64, length: i64| {
            if length <= 0 {
                1.0
            } else {
                ((time - from) as f64 / length as f64).clamp(0.0, 1.0)
            }
        };
        let fade_in = ramp(self.start, self.fade_in);
        let fade_out = if self.fade_out > 0 {
            1.0 - ramp(self.end - self.fade_out, self.fade_out)
        } else {
            1.0
        };
        let entry = self.ramp.map_or(1.0, |(from, length)| ramp(from, length));
        (f64::from(self.base) * fade_in * fade_out * entry) as f32
    }

    /// The times its value changes slope.
    fn breakpoints(&self) -> Vec<i64> {
        let mut times = Vec::new();
        if self.fade_in > 0 {
            times.extend([self.start, self.start + self.fade_in]);
        }
        if self.fade_out > 0 {
            times.extend([self.end - self.fade_out, self.end]);
        }
        if let Some((from, length)) = self.ramp {
            times.extend([from, from + length]);
        }
        times
    }

    /// The opacity over `clip`: keyframes where it changes, else constant.
    fn animation(&self, clip: TimeRange) -> Animated<f32> {
        let (start, end) = (clip.start().ticks(), clip.end().ticks());
        let mut times = BTreeSet::from([start, end]);
        if self.fade_in > 0 {
            times.extend([self.start, self.start + self.fade_in]);
        }
        if self.fade_out > 0 {
            times.extend([self.end - self.fade_out, self.end]);
        }
        if let Some((from, length)) = self.ramp {
            times.extend([from, from + length]);
        }
        let keyframes = times
            .into_iter()
            .filter(|time| (start..=end).contains(time))
            .map(|time| linear(MediaTime::from_ticks(time), self.at(time)))
            .collect::<Vec<_>>();
        // Exactly equal values everywhere are a constant.
        #[allow(clippy::float_cmp)]
        let unchanging = keyframes
            .windows(2)
            .all(|pair| pair[0].value == pair[1].value);
        if unchanging {
            constant(
                keyframes
                    .first()
                    .map_or(self.base, |keyframe| keyframe.value),
            )
        } else {
            Animated::Keyframes { keyframes }
        }
    }
}

/// A project id from `name`: letters and digits, lowercased, words joined by `-`.
fn project_id(name: &str) -> ProjectId {
    let slug = name
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join("-");
    ProjectId::new(if slug.is_empty() {
        "video".to_owned()
    } else {
        slug
    })
    .expect("not empty")
}

#[allow(clippy::too_many_lines)]
pub(crate) fn compile(
    spec: &VideoSpec,
    facts: &Facts,
    fonts: &[Arc<[u8]>],
) -> Result<Compiled, ComposeError> {
    if spec.compose_version != crate::COMPOSE_VERSION {
        return Err(ComposeError::new(
            "spec.compose_version",
            format!(
                "this build reads compose version {}, not {}",
                crate::COMPOSE_VERSION,
                spec.compose_version
            ),
        ));
    }
    let (mut context, background) = Context::new(spec, facts, fonts)?;
    let body = context.lower(&spec.body, "spec.body")?;
    let overlays = spec
        .overlays
        .iter()
        .enumerate()
        .map(|(index, node)| context.lower(node, &format!("spec.overlays[{index}]")))
        .collect::<Result<Vec<_>, _>>()?;
    let audio = spec
        .audio
        .iter()
        .enumerate()
        .map(|(index, node)| {
            let path = format!("spec.audio[{index}]");
            match node {
                crate::spec::Node::Audio(_) | crate::spec::Node::MusicBed(_) => {
                    context.lower(node, &path)
                }
                _ => Err(ComposeError::new(
                    &path,
                    "`audio` takes audio and music_bed nodes",
                )),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let total = match spec.duration {
        Some(seconds) => Context::time("spec", "duration", seconds)?,
        None => context.length(&body).resolve(),
    };
    if total <= 0 {
        return Err(ComposeError::new(
            "spec.body",
            "the video lasts no time: give the body a length, or the spec a duration",
        ));
    }
    let root = Slot {
        start: 0,
        len: total,
        rect: context.canvas_rect(),
        hold: Hold::None,
        ramp: None,
        owners: BTreeSet::from(["body".to_owned()]),
        motions: Vec::new(),
        pending: Vec::new(),
        delay: 0,
    };
    let mut lanes = Lanes::default();
    let mut body = body;
    body.head_mut().at = None;
    context.emit(&body, &root, &mut lanes)?;
    let mut raw_assets = BTreeMap::new();
    let mut raw_sources = Vec::new();
    let mut raw_compositions = Vec::new();
    for prim in overlays.iter().chain(&audio) {
        if let Prim::Raw(_, raw) = prim {
            for track in &raw.tracks {
                let mut lane = Lane::new(track.kind, &BTreeSet::new());
                lane.raw = Some(track.clone());
                if track.kind == TrackKind::Audio {
                    lanes.audio.push(lane);
                } else {
                    lanes.visual.push(lane);
                }
            }
            raw_sources.extend(raw.sources.iter().cloned());
            raw_compositions.extend(raw.compositions.iter().cloned());
            raw_assets.extend(raw.assets.clone());
            continue;
        }
        let at = prim.head().at.unwrap_or(0);
        if at >= total {
            context.warn(format!(
                "{}: it starts after the video ends, so it is not shown",
                prim.head().path
            ));
            continue;
        }
        let remaining = total - at;
        let len = match context.length(prim) {
            Length::Fixed(ticks) => ticks.min(remaining),
            Length::Flexible(_) => remaining,
        };
        let slot = Slot {
            start: at,
            len,
            owners: BTreeSet::new(),
            ..root.clone()
        };
        let mut placed = Lanes::default();
        context.emit(prim, &slot, &mut placed)?;
        lanes.stack(placed);
    }
    assemble(
        spec,
        context,
        lanes,
        background,
        raw_sources,
        raw_compositions,
        raw_assets,
    )
}

#[allow(
    clippy::too_many_arguments,
    clippy::needless_pass_by_value,
    clippy::too_many_lines
)]
fn assemble(
    spec: &VideoSpec,
    mut context: Context<'_>,
    lanes: Lanes,
    background: Rgb,
    raw_sources: Vec<MediaSource>,
    raw_compositions: Vec<Composition>,
    raw_assets: BTreeMap<String, String>,
) -> Result<Compiled, ComposeError> {
    let mut next_track = 0;
    let mut counts = BTreeMap::<&str, u32>::new();
    let ordered = lanes
        .visual
        .into_iter()
        .chain(lanes.audio)
        .collect::<Vec<_>>();
    let ids = ordered
        .iter()
        .map(|lane| match &lane.raw {
            Some(track) => track.id.clone(),
            None => TrackId::new(Context::fresh(&context.reserved, &mut next_track, "t"))
                .expect("not empty"),
        })
        .collect::<Vec<_>>();
    let mut tracks = Vec::with_capacity(ordered.len());
    for (index, lane) in ordered.iter().enumerate() {
        if let Some(track) = &lane.raw {
            tracks.push(track.clone());
            continue;
        }
        let name = match lane.kind {
            TrackKind::Audio => "audio",
            TrackKind::Caption => "captions",
            _ => "video",
        };
        let count = counts.entry(name).or_default();
        *count += 1;
        let mut track = Track::new(ids[index].clone(), format!("{name} {count}"), lane.kind);
        track.clips.clone_from(&lane.clips);
        track.transitions.clone_from(&lane.transitions);
        if lane.kind == TrackKind::Caption {
            track.captions.clone_from(&lane.captions);
        }
        if let Some((target, depth, path)) = &lane.duck {
            let audible = |other: &Lane| {
                other.clips.iter().any(|clip| {
                    !clip.audio.muted
                        && context
                            .sources
                            .iter()
                            .any(|source| source.id == clip.source_id && source.is_audible())
                })
            };
            let found = ordered
                .iter()
                .enumerate()
                .filter(|&(other, candidate)| {
                    other != index && candidate.owners.contains(target) && audible(candidate)
                })
                .map(|(other, _)| other)
                .collect::<Vec<_>>();
            let sidechain = match found.as_slice() {
                [only] => ids[*only].clone(),
                [] => {
                    return Err(ComposeError::new(
                        &format!("{path}.duck_under"),
                        format!("nothing named \"{target}\" plays sound"),
                    ));
                }
                _ => {
                    return Err(ComposeError::new(
                        &format!("{path}.duck_under"),
                        format!(
                            "\"{target}\" plays sound on {} tracks, and ducking follows one: name one of its nodes",
                            found.len()
                        ),
                    ));
                }
            };
            track.audio.ducking = Some(Ducking {
                sidechain_track: sidechain,
                threshold: 0.05,
                depth_db: *depth,
                attack_ms: 20.0,
                release_ms: 300.0,
            });
        }
        tracks.push(track);
    }
    let name = spec.name.clone().unwrap_or_else(|| "video".to_owned());
    let settings = ProjectSettings::new(context.width, context.height, context.rate, 48_000)
        .and_then(|settings| settings.with_background(background))
        .map_err(|error| ComposeError::new("spec.canvas", error.to_string()))?;
    let mut sources = std::mem::take(&mut context.sources);
    sources.extend(raw_sources);
    let mut compositions = std::mem::take(&mut context.compositions);
    compositions.extend(raw_compositions);
    let project = Project {
        data: Vec::new(),
        schema_version: alpha2_model::CURRENT_SCHEMA_VERSION,
        id: project_id(&name),
        name,
        settings,
        sources,
        effect_resources: Vec::new(),
        compositions,
        timeline: Timeline { tracks },
        audio: alpha2_model::MasterAudio::default(),
    };
    project.validate().map_err(|error| {
        ComposeError::new(
            "spec",
            format!("the compiled project is not valid: {error}"),
        )
    })?;
    let mut assets = std::mem::take(&mut context.assets);
    assets.extend(raw_assets);
    let loudness = spec.loudness.map(|loudness| match loudness {
        Loudness::Lufs(target_lufs) => LoudnessTarget {
            target_lufs,
            true_peak_ceiling_dbtp: -1.0,
        },
        Loudness::Target(target) => target,
    });
    Ok(Compiled {
        project,
        assets,
        loudness,
        warnings: context.warnings,
    })
}
