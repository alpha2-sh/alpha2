//! Placing primitives on the timeline: lengths, areas, lanes, and the
//! project they become.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use alpha2_media::SourceId;
use alpha2_model::{
    Animated, AnimatedPoint, CaptionCue, Captions, Clip, ClipComposition, ClipEffect, ClipId,
    ClipTransition, Composition, CompositionId, CompositionNode, Ducking, EffectValue, Generator,
    Interpolation, Keyframe, LoopMode, MediaKind as ModelKind, MediaSource, NodeId, NodeOp, Paint,
    Project, ProjectId, ProjectSettings, RectangleGeometry, Rgb, Rgba, ShapeGeometry, ShapeMask,
    TextAlign, TextBackground, TextStyle, TimeMap, Timeline, Track, TrackId, TrackKind, Vec2,
    VisualProperties,
};
use alpha2_text::{TextFont, TextRasterizer};
use alpha2_time::{FrameRate, MediaTime, TICKS_PER_SECOND, TimeRange};

use crate::lower::{
    AudioPrim, CaptionsPrim, Content, GridLayout, Prim, SequencePrim, StackPrim, Visual,
    align_point, ticks_scaled,
};
use crate::spec::{
    Canvas, CustomCanvas, Fit, FrameRateChoice, GridLength, Loudness, Transition, VideoSpec,
};
use crate::theme::Resolved;
use crate::{Compiled, ComposeError, Facts, LoudnessTarget};

/// Seconds of `ticks`.
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
    /// The time it may take: a node of a length of its own plays that,
    /// trimmed to it; one without fills it.
    len: i64,
    rect: Rect,
    hold: Hold,
    /// A fade in over `start..start + duration` multiplied into every
    /// picture: a crossfade into a node that is not one picture.
    ramp: Option<(i64, i64)>,
    owners: BTreeSet<String>,
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
    /// A ducked audio lane's sidechain: the node it ducks under, how deep,
    /// and where the request is.
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

    /// Whether `other` can join this lane: the same kind and style, after
    /// everything here ends.
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

    /// `other`'s lanes, in lanes here they can share (they start after
    /// what those hold) or new ones; returns where each of its visual lanes
    /// went.
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
    fit_warned: bool,
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
                fit_warned: false,
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

    /// Lines and height in pixels of `text` at `font_px`, wrapping at
    /// `max_width` pixels, as the fonts measure it on the canvas, through the
    /// request the engine draws a text generator's text with; `None` without
    /// fonts.
    fn measure(&self, text: &str, font_px: f32, max_width: f32) -> Option<(u32, f32)> {
        let measurer = self.measurer.as_ref()?;
        let (width, _) = self.canvas_px();
        let style = TextStyle {
            font_family: self.theme.font_family.clone(),
            font_size: font_px / self.short_side(),
            align: TextAlign::Center,
            max_width: max_width / width,
            ..TextStyle::default()
        };
        let canvas = alpha2_layout::PictureSize::new(self.width, self.height).ok()?;
        let request = alpha2_layout::text_request(text, &style, canvas, 1);
        #[allow(clippy::cast_precision_loss)]
        measurer
            .measure(&request)
            .ok()
            .map(|measure| (measure.line_count, measure.box_height as f32))
    }

    /// The largest font size up to `font_px` at which `text` takes at most
    /// `max_lines` lines `max_width` pixels wide; `font_px` without fonts
    /// to measure by.
    pub(crate) fn fit_font(
        &mut self,
        path: &str,
        text: &str,
        font_px: f32,
        max_width: f32,
        max_lines: u32,
    ) -> f32 {
        if self.measurer.is_none() {
            if !self.fit_warned {
                self.fit_warned = true;
                self.warn(format!(
                    "{path}: no fonts to measure text by, so text is not shrunk to fit"
                ));
            }
            return font_px;
        }
        let floor = 0.3 * font_px;
        let mut size = font_px;
        while size > floor {
            match self.measure(text, size, max_width) {
                Some((lines, _)) if lines > max_lines => size *= 0.92,
                _ => return size,
            }
        }
        self.warn(format!(
            "{path}: the text does not fit in {max_lines} lines even at {floor:.0} px"
        ));
        floor
    }

    /// The height in pixels `text` takes: measured with fonts, else one
    /// line per `\n`-separated line.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn text_height(&self, text: &str, font_px: f32, max_width: f32) -> f32 {
        let line_height = TextStyle::default().line_height;
        self.measure(text, font_px, max_width).map_or_else(
            || {
                (text.split('\n').count() as f32 - 1.0) * font_px * line_height
                    + font_px * line_height.max(1.0)
            },
            |(_, height)| height,
        )
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
            Prim::Stack(stack) => {
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
            Prim::Visual(visual) => match &visual.content {
                Content::Video(video) if video.looped => Length::Flexible(video.natural()),
                Content::Video(video) => Length::Fixed(video.natural()),
                Content::Image { .. } => {
                    Length::Flexible(crate::lower::seconds_to_ticks(self.theme.still_seconds))
                }
                Content::Text(_) | Content::Color(_) | Content::Layout(_) => {
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

    /// Places `prim` in `slot`, adding its lanes to `out`.
    fn emit(&mut self, prim: &Prim, slot: &Slot, out: &mut Lanes) -> Result<(), ComposeError> {
        if slot.len <= 0 {
            return Ok(());
        }
        let mut slot = slot.clone();
        slot.owners.extend(prim.head().id.clone());
        match prim {
            Prim::Sequence(sequence) => self.emit_sequence(sequence, &slot, out),
            Prim::Stack(stack) => self.emit_stack(stack, &slot, out),
            Prim::Visual(visual) => {
                self.emit_visual(visual, &slot, out);
                Ok(())
            }
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
                // outgoing one, in lanes over it.
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
            let child_slot = Slot {
                start: slot.start + at,
                len,
                rect: cells.as_ref().map_or(rect, |cells| cells[index]),
                hold,
                ..slot.clone()
            };
            let mut lanes = Lanes::default();
            self.emit(child, &child_slot, &mut lanes)?;
            out.stack(lanes);
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn emit_visual(&mut self, visual: &Visual, slot: &Slot, out: &mut Lanes) {
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
                let looped = video.looped || (slot.hold == Hold::Loop && slot.len > natural);
                let mut clips = Vec::new();
                if looped {
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
                    if slot.hold == Hold::Freeze && slot.len > play {
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
                let font_px = match text.max_lines {
                    Some(lines) => self.fit_font(
                        &visual.head.path,
                        &text.text,
                        text.font_px,
                        rect.width,
                        lines,
                    ),
                    None => text.font_px,
                };
                let style = TextStyle {
                    font_family: text.family.clone(),
                    font_size: (font_px / self.short_side())
                        .min(alpha2_model::MAX_TEXT_GENERATOR_FONT_SIZE),
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
                    ..TextStyle::default()
                };
                let source = self.generated_source(Generator::Text {
                    text: text.text.clone().into(),
                    style,
                });
                let mut clip = Clip::new(
                    self.clip_id(),
                    source,
                    range(start, end),
                    range(0, slot.len),
                );
                clip.visual.opacity = opacity.animation(clip.timeline_range);
                lane.clips.push(clip);
            }
        }
        out.visual.push(lane);
    }

    /// Where a picture `size` pixels at scale 1 lies to fill `rect` as
    /// `visual` asks, and its opacity over `clip`.
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
        let scale = match visual.zoom {
            None => constant(fitted.scale),
            Some([from, to]) => Animated::Keyframes {
                keyframes: vec![
                    linear(
                        clip.start(),
                        Vec2::new(fitted.scale.x * from, fitted.scale.y * from),
                    ),
                    linear(
                        clip.end(),
                        Vec2::new(fitted.scale.x * to, fitted.scale.y * to),
                    ),
                ],
            },
        };
        VisualProperties {
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

    /// The mask that crops a picture to `rect` where it overflows it inside
    /// the canvas, which crops the rest itself.
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
        let [from, to] = visual.zoom.unwrap_or([1.0, 1.0]);
        let overflows = |zoom: f32| {
            fitted.displayed.0 * zoom > rect.width * 1.0001
                || fitted.displayed.1 * zoom > rect.height * 1.0001
        };
        if full || !(overflows(from) || overflows(to)) {
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
        let still = visual.zoom.is_none() || from == to;
        let (center, size) = if still {
            let (center, size) = visible(from);
            (AnimatedPoint::Constant { value: center }, constant(size))
        } else {
            // The visible part shrinks as one over the zoom, which keyframes follow closely enough
            // in eight straight pieces.
            let steps = 8_i64;
            let (start, length) = (
                clip.start().ticks(),
                clip.end().ticks() - clip.start().ticks(),
            );
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let samples = (0..=steps)
                .map(|step| {
                    let time = MediaTime::from_ticks(start + length * step / steps);
                    let zoom = from + (to - from) * (step as f32 / steps as f32);
                    (time, visible(zoom))
                })
                .collect::<Vec<_>>();
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

/// The effect id a transition kind names.
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
/// A clip's source is a canvas-sized frame the engine contain-places its
/// picture in, transparent around it; the clip's anchor and masks are points
/// of that frame, its scale that frame's.
struct Fitted {
    /// The clip scale that gives it.
    scale: Vec2,
    /// Its size in canvas pixels, unzoomed.
    displayed: (f32, f32),
    /// The part of the frame the picture covers: its size, and where it
    /// starts, as fractions of the frame.
    frame_part: Vec2,
    frame_offset: Vec2,
}

impl Fitted {
    /// A picture `size` pixels, contain-placed in a `canvas`-sized frame,
    /// fitted into `rect` as `fit` says.
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

const fn linear<T>(time: MediaTime, value: T) -> Keyframe<T> {
    Keyframe {
        time,
        value,
        interpolation: Interpolation::Linear,
    }
}

/// A picture's opacity: its own, its fades at its node's ends, and a
/// crossfade's ramp.
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

/// A project id from `name`: its letters and digits, lowercased, words
/// joined by `-`.
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

/// Compiles a parsed spec.
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
