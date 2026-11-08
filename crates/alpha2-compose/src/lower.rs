//! Lowers every spec node, components included, to the few primitives the emitter places
//! ([`Prim`]), with theme defaults filled in, times in ticks, and caption files parsed.

use std::collections::BTreeMap;

use alpha2_model::{
    CaptionCue, CubicBezier, Easing, EffectValue, LayoutNode, Paint, Rgba, RunState, TextShadow,
    TextStroke, TextStyle, TextUnit,
};
use alpha2_time::MediaTime;

use crate::ComposeError;
use crate::compile::Context;
use crate::spec::{
    Align, Anim, Audio, BoxNode, CaptionStyle, Captions, ConicPaint, Counter, Cue, Direction,
    EasingChoice, EffectUse, Ellipsis, FileKind, Fit, Geometry, Gradient, GridLength, Group, Inset,
    Key, Keys, LengthToken, LinearPaint, MatteUse, Media, MediaItem, Motion, MotionKind, MusicBed,
    Node, PaintChoice, RadialPaint, Ramp, Raw, RevealKind, RevealUnit, Shape, Size, Slideshow,
    SolidColor, Stop, Text, TextRole, ThemeColor, ThemeLength, ThemeToken, Transition,
    TransitionChoice,
};

/// A node, ready to place.
#[derive(Clone, Debug)]
pub(crate) enum Prim {
    Sequence(SequencePrim),
    Stack(StackPrim),
    Group(Box<GroupPrim>),
    Coverage(Box<CoveragePrim>),
    Box(Box<BoxPrim>),
    Visual(Box<Visual>),
    Captions(Box<CaptionsPrim>),
    Audio(Box<AudioPrim>),
    Raw(Head, Box<Raw>),
}

/// What every primitive has: where errors point, its name, its timing, and its motion.
#[derive(Clone, Debug, Default)]
pub(crate) struct Head {
    pub path: String,
    pub id: Option<String>,
    pub at: Option<i64>,
    pub duration: Option<i64>,
    pub motion: Option<Box<NodeMotion>>,
    /// Ticks between the children's entrances; 0 moves the node as a whole.
    pub stagger: i64,
}

/// A node's entrance and exit.
#[derive(Clone, Debug)]
pub(crate) struct NodeMotion {
    pub enter: Option<Preset>,
    pub exit: Option<Preset>,
}

/// One entrance or exit.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Preset {
    pub duration: i64,
    pub easing: Easing,
    pub fade: bool,
    /// The travel in canvas pixels: an entrance starts this far back, an exit ends this far on.
    pub travel: [f32; 2],
    /// The size an entrance starts at or an exit ends at, as a multiple.
    pub scale: Option<f32>,
}

#[derive(Clone, Debug)]
pub(crate) struct SequencePrim {
    pub head: Head,
    pub children: Vec<Prim>,
    pub transition: Option<Transition>,
}

#[derive(Clone, Debug)]
pub(crate) struct StackPrim {
    pub head: Head,
    pub children: Vec<Prim>,
    pub inset: [f32; 4],
    /// Children in grid cells rather than over each other.
    pub grid: Option<GridLayout>,
}

/// Children composited into one picture before it meets what is under it.
#[derive(Clone, Debug)]
pub(crate) struct GroupPrim {
    /// The children, the group's name, time and area.
    pub stack: StackPrim,
    pub opacity: Option<AnimPrim<f32>>,
    pub blend: alpha2_model::ClipBlendMode,
    pub effects: Vec<EffectPrim>,
    pub transform: TransformPrim,
}

/// A box layout in the node's area.
#[derive(Clone, Debug)]
pub(crate) struct BoxPrim {
    pub head: Head,
    pub inset: [f32; 4],
    pub tree: BoxTree,
}

/// One layout box with its container fields as written, its paints and its children.
#[derive(Clone, Debug)]
pub(crate) struct BoxTree {
    pub css: BTreeMap<String, serde_json::Value>,
    pub background: Option<PaintPrim>,
    pub border: Option<(serde_json::Value, PaintPrim)>,
    pub children: Vec<BoxKid>,
}

#[derive(Clone, Debug)]
pub(crate) struct BoxKid {
    /// Item fields as written.
    pub item: BTreeMap<String, serde_json::Value>,
    pub content: BoxContent,
}

#[derive(Clone, Debug)]
pub(crate) enum BoxContent {
    /// A nested box.
    Tree(BoxTree),
    /// Text, a shape or a color, drawn by the layout.
    Native(Box<Visual>),
    /// Anything else, drawn in its place.
    Foreign(Prim),
}

/// A node shown only where a matte's picture or mask outlines say.
#[derive(Clone, Debug)]
pub(crate) struct CoveragePrim {
    pub head: Head,
    pub inset: [f32; 4],
    pub content: Prim,
    pub by: Coverage,
}

#[derive(Clone, Debug)]
pub(crate) enum Coverage {
    Matte {
        by: Prim,
        mode: alpha2_model::MatteMode,
    },
    Mask(Vec<MaskShapePrim>),
}

#[derive(Clone, Debug)]
pub(crate) struct MaskShapePrim {
    pub geometry: GeometryPrim,
    pub mode: alpha2_model::MaskMode,
    pub feather: AnimPrim<f32>,
    pub invert: bool,
}

/// One effect on a group, checked against its definition when it is a built-in.
#[derive(Clone, Debug)]
pub(crate) struct EffectPrim {
    pub path: String,
    pub id: String,
    pub version: u32,
    pub params: BTreeMap<String, AnimPrim<EffectValue>>,
    /// Named image inputs, as nodes drawn in the group's area and time.
    pub inputs: BTreeMap<String, Prim>,
}

/// A group's movement, with positions in pixels and the anchor in its area's unit square.
#[derive(Clone, Debug)]
pub(crate) struct TransformPrim {
    pub position: Option<AnimPrim<[f32; 2]>>,
    pub scale: Option<AnimPrim<f32>>,
    pub rotation: Option<AnimPrim<f32>>,
    pub anchor: [f32; 2],
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct GridLayout {
    pub columns: u32,
    pub gap: f32,
    pub length: GridLength,
}

/// A lowered [`Anim`], one value or keys whose times are ticks from the node's start.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AnimPrim<T> {
    Value(T),
    Keys(Vec<KeyPrim<T>>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct KeyPrim<T> {
    pub at: KeyTime,
    pub value: T,
    /// The curve to the next key.
    pub easing: Easing,
}

/// When a key falls, in ticks from the node's start or from its end once its length is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum KeyTime {
    Ticks(i64),
    End,
}

#[derive(Clone, Debug)]
pub(crate) struct Visual {
    pub head: Head,
    pub inset: [f32; 4],
    pub fit: Fit,
    pub align: Align,
    pub opacity: f32,
    pub fade_in: i64,
    pub fade_out: i64,
    pub zoom: Option<AnimPrim<f32>>,
    pub content: Content,
}

#[derive(Clone, Debug)]
pub(crate) enum Content {
    Video(VideoContent),
    Image {
        file: String,
        size: (u32, u32),
        blur: Option<f32>,
    },
    Text(TextContent),
    Color(Rgba),
    Shape(Box<ShapeContent>),
    Layout(Box<LayoutNode>),
    Counter(Box<CounterContent>),
}

/// Outlines in their area's unit square, with what fills, strokes and trims them.
#[derive(Clone, Debug)]
pub(crate) struct ShapeContent {
    pub geometry: Vec<GeometryPrim>,
    pub fill: Option<PaintPrim>,
    pub stroke: Option<StrokePrim>,
    pub trim: Option<TrimPrim>,
}

/// One outline, with points in the area's unit square and lengths in its shorter side.
#[derive(Clone, Debug)]
pub(crate) enum GeometryPrim {
    Rect {
        center: AnimPrim<[f32; 2]>,
        size: AnimPrim<[f32; 2]>,
        corner: AnimPrim<f32>,
        rotation: AnimPrim<f32>,
    },
    Ellipse {
        center: AnimPrim<[f32; 2]>,
        size: AnimPrim<[f32; 2]>,
        rotation: AnimPrim<f32>,
    },
    Polystar {
        kind: alpha2_model::PolystarKind,
        center: AnimPrim<[f32; 2]>,
        points: u32,
        outer: AnimPrim<f32>,
        inner: AnimPrim<f32>,
        roundness: AnimPrim<f32>,
        rotation: AnimPrim<f32>,
    },
    Path(alpha2_model::Path),
}

/// A paint with its theme colors resolved and positions in the area's unit square.
#[derive(Clone, Debug)]
pub(crate) enum PaintPrim {
    Solid(Rgba),
    Linear {
        stops: Vec<(f32, Rgba)>,
        angle: f32,
    },
    Radial {
        stops: Vec<(f32, Rgba)>,
        center: [f32; 2],
        radius: f32,
    },
    Conic {
        stops: Vec<(f32, Rgba)>,
        center: [f32; 2],
        angle: f32,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct StrokePrim {
    pub paint: PaintPrim,
    pub width: AnimPrim<f32>,
    pub dash: Vec<f32>,
    pub cap: alpha2_model::LineCap,
    pub join: alpha2_model::LineJoin,
}

#[derive(Clone, Debug)]
pub(crate) struct TrimPrim {
    pub start: AnimPrim<f32>,
    pub end: AnimPrim<f32>,
    pub offset: AnimPrim<f32>,
}

#[derive(Clone, Debug)]
pub(crate) struct CounterContent {
    pub start: f64,
    pub end: f64,
    /// Ticks the count takes; `None` is the node's length.
    pub count: Option<i64>,
    pub easing: Easing,
    pub decimals: u32,
    pub thousands: String,
    pub point: String,
    pub prefix: String,
    pub suffix: String,
    pub font_px: f32,
    pub family: Option<String>,
    pub color: Rgba,
    pub strokes: Vec<TextStroke>,
    pub shadows: Vec<TextShadow>,
}

#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct VideoContent {
    pub file: String,
    pub size: (u32, u32),
    pub trim_start: i64,
    pub trim_end: i64,
    /// Source ticks per timeline tick.
    pub speed: f64,
    pub preserve_pitch: bool,
    pub looped: bool,
    pub volume: f32,
    pub muted: bool,
    pub has_audio: bool,
    pub blur: Option<f32>,
    /// One frame of the file, for a hold on its last.
    pub frame_ticks: i64,
    /// What it does past its end; `None` follows the parent.
    pub when_short: Option<crate::spec::WhenShort>,
    pub when_long: crate::spec::WhenLong,
}

impl VideoContent {
    /// Its timeline length played once.
    pub(crate) fn natural(&self) -> i64 {
        ticks_scaled(self.trim_end - self.trim_start, 1.0 / self.speed)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TextContent {
    pub text: String,
    /// Font size in pixels.
    pub font_px: f32,
    pub family: Option<String>,
    pub color: Rgba,
    pub background: Option<Rgba>,
    pub strokes: Vec<TextStroke>,
    pub shadows: Vec<TextShadow>,
    pub max_lines: Option<u32>,
    /// Font sizes in pixels the text is fitted between.
    pub fit: Option<(f32, f32)>,
    /// What ends a cut line, when lines are cut.
    pub ellipsis: Option<alpha2_model::BlockEllipsis>,
    pub reveal: Option<RevealPrim>,
}

/// The font sizes in pixels a text is fitted between, and what ends a line it cuts.
type TextFitting = (Option<(f32, f32)>, Option<alpha2_model::BlockEllipsis>);

/// A text revealed a unit at a time: unit `i` changes over
/// `i * stagger..i * stagger + duration` of the clip's picture time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RevealPrim {
    pub unit: TextUnit,
    pub stagger: i64,
    pub duration: i64,
    /// Pixels a unit rises; 0 fades alone.
    pub rise: f32,
}

#[derive(Clone, Debug)]
pub(crate) struct CaptionsPrim {
    pub head: Head,
    /// Cues from the node's start, in start order; they may overlap.
    pub cues: Vec<CaptionCue>,
    pub style: TextStyle,
    /// Speakers' looks, by name.
    pub styles: std::collections::BTreeMap<String, alpha2_model::TextStylePatch>,
    /// A bar behind the text: its center and height as fractions of the canvas height, and its
    /// color.
    pub bar: Option<(f32, f32, Rgba)>,
}

#[derive(Clone, Debug)]
pub(crate) struct AudioPrim {
    pub head: Head,
    pub file: String,
    pub trim_start: i64,
    pub trim_end: i64,
    pub looped: bool,
    pub volume: f32,
    pub fade_in: i64,
    pub fade_out: i64,
    pub duck_under: Option<String>,
    pub duck_depth_db: f32,
}

impl Prim {
    pub(crate) fn head(&self) -> &Head {
        match self {
            Self::Sequence(sequence) => &sequence.head,
            Self::Stack(stack) => &stack.head,
            Self::Group(group) => &group.stack.head,
            Self::Coverage(coverage) => &coverage.head,
            Self::Box(node) => &node.head,
            Self::Visual(visual) => &visual.head,
            Self::Captions(captions) => &captions.head,
            Self::Audio(audio) => &audio.head,
            Self::Raw(head, _) => head,
        }
    }

    pub(crate) fn head_mut(&mut self) -> &mut Head {
        match self {
            Self::Sequence(sequence) => &mut sequence.head,
            Self::Stack(stack) => &mut stack.head,
            Self::Group(group) => &mut group.stack.head,
            Self::Coverage(coverage) => &mut coverage.head,
            Self::Box(node) => &mut node.head,
            Self::Visual(visual) => &mut visual.head,
            Self::Captions(captions) => &mut captions.head,
            Self::Audio(audio) => &mut audio.head,
            Self::Raw(head, _) => head,
        }
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub(crate) fn ticks_scaled(ticks: i64, factor: f64) -> i64 {
    (ticks as f64 * factor).round() as i64
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub(crate) fn seconds_to_ticks(seconds: f64) -> i64 {
    (seconds * alpha2_time::TICKS_PER_SECOND as f64).round() as i64
}

/// The most units of a revealed text changing at once, which keeps the range selector's
/// offset inside the indices a project allows.
const REVEAL_MAX_OVERLAP: i64 = 1024;

/// The largest magnitude a counter counts to; past it a 64-bit float loses whole digits.
const COUNTER_MAX: f64 = 1e15;

/// A node's `enter`, `exit` and `stagger`, for the nodes that take them.
fn motion_fields(node: &Node) -> (Option<&Motion>, Option<&Motion>, Option<f64>) {
    match node {
        Node::Sequence(node) => (node.enter.as_ref(), node.exit.as_ref(), None),
        Node::Stack(node) => (node.enter.as_ref(), node.exit.as_ref(), node.stagger),
        Node::Grid(node) => (node.enter.as_ref(), node.exit.as_ref(), node.stagger),
        Node::Media(node) => (node.enter.as_ref(), node.exit.as_ref(), None),
        Node::Text(node) => (node.enter.as_ref(), node.exit.as_ref(), None),
        Node::Color(node) => (node.enter.as_ref(), node.exit.as_ref(), None),
        Node::Layout(node) => (node.enter.as_ref(), node.exit.as_ref(), None),
        Node::Counter(node) => (node.enter.as_ref(), node.exit.as_ref(), None),
        Node::Group(node) => (node.enter.as_ref(), node.exit.as_ref(), None),
        Node::Shape(node) => (node.enter.as_ref(), node.exit.as_ref(), None),
        Node::Matte(_)
        | Node::Mask(_)
        | Node::Box(_)
        | Node::Captions(_)
        | Node::Audio(_)
        | Node::Raw(_)
        | Node::Slideshow(_)
        | Node::CaptionedVideo(_)
        | Node::MusicBed(_) => (None, None, None),
    }
}

const ALIGNS: [(Align, f32, f32); 9] = [
    (Align::Center, 0.5, 0.5),
    (Align::Top, 0.5, 0.0),
    (Align::Bottom, 0.5, 1.0),
    (Align::Left, 0.0, 0.5),
    (Align::Right, 1.0, 0.5),
    (Align::TopLeft, 0.0, 0.0),
    (Align::TopRight, 1.0, 0.0),
    (Align::BottomLeft, 0.0, 1.0),
    (Align::BottomRight, 1.0, 1.0),
];

/// Where `align` lies in a unit square.
pub(crate) fn align_point(align: Align) -> (f32, f32) {
    ALIGNS
        .iter()
        .find(|(candidate, _, _)| *candidate == align)
        .map_or((0.5, 0.5), |&(_, x, y)| (x, y))
}

fn inset_sides(inset: Option<Inset>) -> [f32; 4] {
    match inset {
        None => [0.0; 4],
        Some(Inset::Uniform(value)) => [value; 4],
        Some(Inset::Sides(sides)) => sides,
    }
}

impl Context<'_> {
    fn error(path: &str, message: impl Into<String>) -> ComposeError {
        ComposeError::new(path, message)
    }

    /// Seconds a field holds as ticks: finite and not negative.
    pub(crate) fn time(path: &str, field: &str, seconds: f64) -> Result<i64, ComposeError> {
        if seconds.is_finite() && seconds >= 0.0 {
            Ok(seconds_to_ticks(seconds))
        } else {
            Err(Self::error(
                &format!("{path}.{field}"),
                format!("{seconds} is not a time: seconds are finite and not negative"),
            ))
        }
    }

    fn optional_time(
        path: &str,
        field: &str,
        seconds: Option<f64>,
    ) -> Result<Option<i64>, ComposeError> {
        seconds
            .map(|seconds| Self::time(path, field, seconds))
            .transpose()
    }

    fn head(
        path: &str,
        id: Option<&String>,
        at: Option<f64>,
        duration: Option<f64>,
    ) -> Result<Head, ComposeError> {
        let duration = Self::optional_time(path, "duration", duration)?;
        if duration == Some(0) {
            return Err(Self::error(
                &format!("{path}.duration"),
                "a node lasts longer than zero",
            ));
        }
        Ok(Head {
            path: path.to_owned(),
            id: id.cloned(),
            at: Self::optional_time(path, "at", at)?,
            duration,
            motion: None,
            stagger: 0,
        })
    }

    fn unit(
        path: &str,
        field: &str,
        value: Option<f32>,
        default: f32,
    ) -> Result<f32, ComposeError> {
        match value {
            None => Ok(default),
            Some(value) if value.is_finite() && (0.0..=1.0).contains(&value) => Ok(value),
            Some(value) => Err(Self::error(
                &format!("{path}.{field}"),
                format!("{value} is outside 0..=1"),
            )),
        }
    }

    /// The theme size `fraction` in pixels.
    pub(crate) fn theme_px(&self, fraction: f32) -> f32 {
        fraction * self.short_side()
    }

    /// The transition `choice` asks for, if any.
    fn transition(
        &self,
        path: &str,
        choice: Option<&TransitionChoice>,
        default: Option<&Transition>,
    ) -> Result<Option<Transition>, ComposeError> {
        let transition = match choice {
            None => default.cloned(),
            Some(TransitionChoice::Named(name)) => match name.as_str() {
                "none" | "cut" => None,
                "theme" => Some(self.theme.transition.clone()),
                kind => Some(Transition {
                    kind: kind.to_owned(),
                    ..self.theme.transition.clone()
                }),
            },
            Some(TransitionChoice::Given(transition)) => Some(transition.clone()),
        };
        if let Some(transition) = &transition {
            let ticks = Self::time(
                &format!("{path}.transition"),
                "duration",
                transition.duration,
            )?;
            if ticks == 0 {
                return Err(Self::error(
                    &format!("{path}.transition.duration"),
                    "a transition lasts longer than zero",
                ));
            }
        }
        Ok(transition)
    }

    /// Lowers `node` and adds its entrance, exit and stagger.
    pub(crate) fn lower(&mut self, node: &Node, path: &str) -> Result<Prim, ComposeError> {
        let mut prim = self.lower_node(node, path)?;
        let (enter, exit, stagger) = motion_fields(node);
        // A motion of zero seconds is none.
        let moves = |motion: &&Motion| motion.duration != Some(0.0);
        let (enter, exit) = (enter.filter(moves), exit.filter(moves));
        if enter.is_some() || exit.is_some() {
            let motion = NodeMotion {
                enter: enter
                    .map(|motion| self.preset(path, "enter", motion))
                    .transpose()?,
                exit: exit
                    .map(|motion| self.preset(path, "exit", motion))
                    .transpose()?,
            };
            prim.head_mut().motion = Some(Box::new(motion));
        }
        if let Some(stagger) = stagger {
            prim.head_mut().stagger = Self::time(path, "stagger", stagger)?;
        }
        Ok(prim)
    }

    /// Lowers `anim`, each of whose values `valid` accepts.
    pub(crate) fn anim<T: Clone>(
        path: &str,
        anim: &Anim<T>,
        valid: impl Fn(&T) -> bool,
    ) -> Result<AnimPrim<T>, ComposeError> {
        let invalid = |path: &str| Self::error(path, "a value is out of range");
        // `easing` names its field `easing`; here it is `ease`.
        let ease = |path: &str, choice: Option<&EasingChoice>| {
            Self::easing(path, choice)
                .map_err(|error| Self::error(&format!("{path}.ease"), error.message))
        };
        match anim {
            Anim::Value(value) if valid(value) => Ok(AnimPrim::Value(value.clone())),
            Anim::Value(_) => Err(invalid(path)),
            Anim::Ramp(ramp) => {
                if !valid(&ramp.from) || !valid(&ramp.to) {
                    return Err(invalid(path));
                }
                let at = Self::optional_time(path, "at", ramp.at)?.unwrap_or(0);
                let end = match Self::optional_time(path, "duration", ramp.duration)? {
                    Some(duration) => KeyTime::Ticks(at + duration),
                    None => KeyTime::End,
                };
                Ok(AnimPrim::Keys(vec![
                    KeyPrim {
                        at: KeyTime::Ticks(at),
                        value: ramp.from.clone(),
                        easing: ease(path, ramp.ease.as_ref())?,
                    },
                    KeyPrim {
                        at: end,
                        value: ramp.to.clone(),
                        easing: Easing::Linear,
                    },
                ]))
            }
            Anim::Keys(keys) => {
                if keys.keys.is_empty() {
                    return Err(Self::error(path, "keys hold at least one key"));
                }
                let mut lowered = Vec::with_capacity(keys.keys.len());
                for (index, key) in keys.keys.iter().enumerate() {
                    let at = format!("{path}.keys[{index}]");
                    if !valid(&key.v) {
                        return Err(invalid(&format!("{at}.v")));
                    }
                    let ticks = Self::time(&at, "t", key.t)?;
                    if lowered
                        .last()
                        .is_some_and(|last: &KeyPrim<T>| last.at >= KeyTime::Ticks(ticks))
                    {
                        return Err(Self::error(
                            &format!("{at}.t"),
                            "keys are in increasing time",
                        ));
                    }
                    lowered.push(KeyPrim {
                        at: KeyTime::Ticks(ticks),
                        value: key.v.clone(),
                        easing: ease(&at, key.ease.as_ref())?,
                    });
                }
                Ok(AnimPrim::Keys(lowered))
            }
        }
    }

    /// A curve from `choice`: linear when absent.
    pub(crate) fn easing(
        path: &str,
        choice: Option<&EasingChoice>,
    ) -> Result<Easing, ComposeError> {
        match choice {
            None => Ok(Easing::Linear),
            Some(EasingChoice::Named(name)) => match name.as_str() {
                "linear" => Ok(Easing::Linear),
                "ease" => Ok(Easing::CubicBezier(CubicBezier::new(0.25, 0.1, 0.25, 1.0))),
                "ease-in" => Ok(Easing::CubicBezier(CubicBezier::new(0.42, 0.0, 1.0, 1.0))),
                "ease-out" => Ok(Easing::CubicBezier(CubicBezier::new(0.0, 0.0, 0.58, 1.0))),
                "ease-in-out" => Ok(Easing::CubicBezier(CubicBezier::new(0.42, 0.0, 0.58, 1.0))),
                other => Err(Self::error(
                    &format!("{path}.easing"),
                    format!(
                        "unknown easing \"{other}\"; the names are linear, ease, ease-in, ease-out and ease-in-out"
                    ),
                )),
            },
            Some(EasingChoice::Curve(Easing::CubicBezier(curve))) if !curve.is_valid() => {
                Err(Self::error(
                    &format!("{path}.easing"),
                    "a cubic_bezier has finite points with x1 and x2 in 0..=1",
                ))
            }
            Some(EasingChoice::Curve(easing)) => Ok(*easing),
        }
    }

    fn preset(&self, path: &str, field: &str, motion: &Motion) -> Result<Preset, ComposeError> {
        let path = format!("{path}.{field}");
        let refuse = |name: &str, what: &str| {
            Self::error(
                &format!("{path}.{name}"),
                format!("{name} is for {what} only"),
            )
        };
        let duration = match motion.duration {
            Some(seconds) => Self::time(&path, "duration", seconds)?,
            None => seconds_to_ticks(self.theme.fade_seconds),
        };
        if duration == 0 {
            return Err(Self::error(
                &format!("{path}.duration"),
                "a motion lasts longer than zero",
            ));
        }
        if motion.direction.is_some() && motion.kind != MotionKind::Slide {
            return Err(refuse("direction", "a slide"));
        }
        if motion.distance.is_some() && !matches!(motion.kind, MotionKind::Rise | MotionKind::Slide)
        {
            return Err(refuse("distance", "a rise or slide"));
        }
        if motion.size.is_some() && motion.kind != MotionKind::Scale {
            return Err(refuse("size", "a scale"));
        }
        if motion.fade.is_some() && motion.kind == MotionKind::Fade {
            return Err(refuse("fade", "a rise, slide or scale"));
        }
        let distance = match motion.distance {
            Some(distance) => Self::pixels(&path, "distance", distance)?,
            None => self.theme_px(self.theme.margin),
        };
        let direction = match motion.kind {
            MotionKind::Rise => Some(Direction::Up),
            MotionKind::Slide => Some(motion.direction.unwrap_or_default()),
            MotionKind::Fade | MotionKind::Scale => None,
        };
        let travel = match direction {
            None => [0.0, 0.0],
            Some(Direction::Up) => [0.0, -distance],
            Some(Direction::Down) => [0.0, distance],
            Some(Direction::Left) => [-distance, 0.0],
            Some(Direction::Right) => [distance, 0.0],
        };
        let scale = match (motion.kind, motion.size) {
            (MotionKind::Scale, Some(size)) if !(size.is_finite() && size >= 0.0) => {
                return Err(Self::error(
                    &format!("{path}.size"),
                    format!("{size} is not a size: it is finite and not negative"),
                ));
            }
            (MotionKind::Scale, size) => Some(size.unwrap_or(0.0)),
            _ => None,
        };
        Ok(Preset {
            duration,
            easing: Self::easing(&path, motion.easing.as_ref())?,
            fade: motion.kind == MotionKind::Fade || motion.fade.unwrap_or(true),
            travel,
            scale,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn lower_node(&mut self, node: &Node, path: &str) -> Result<Prim, ComposeError> {
        match node {
            Node::Sequence(sequence) => {
                let children =
                    self.lower_children(&sequence.children, &format!("{path}.children"))?;
                Ok(Prim::Sequence(SequencePrim {
                    head: Self::head(path, sequence.id.as_ref(), sequence.at, sequence.duration)?,
                    children,
                    transition: self.transition(path, sequence.transition.as_ref(), None)?,
                }))
            }
            Node::Stack(stack) => Ok(Prim::Stack(StackPrim {
                head: Self::head(path, stack.id.as_ref(), stack.at, stack.duration)?,
                children: self.lower_children(&stack.children, &format!("{path}.children"))?,
                inset: inset_sides(stack.inset),
                grid: None,
            })),
            Node::Grid(grid) => {
                let children = self.lower_children(&grid.children, &format!("{path}.children"))?;
                Ok(Prim::Stack(StackPrim {
                    head: Self::head(path, grid.id.as_ref(), grid.at, grid.duration)?,
                    inset: inset_sides(grid.inset),
                    grid: Some(GridLayout {
                        columns: Self::columns(path, grid.columns, children.len())?,
                        gap: Self::pixels(path, "gap", grid.gap)?,
                        length: grid.length,
                    }),
                    children,
                }))
            }
            Node::Media(media) => self.lower_media(media, path),
            Node::Group(group) => self.lower_group(group, path),
            Node::Shape(shape) => self.lower_shape(shape, path),
            Node::Box(node) => Ok(Prim::Box(Box::new(BoxPrim {
                head: Self::head(path, node.id.as_ref(), node.at, node.duration)?,
                inset: inset_sides(node.inset),
                tree: self.box_tree(node, path)?,
            }))),
            Node::Matte(matte) => Ok(Prim::Coverage(Box::new(CoveragePrim {
                head: Self::head(path, matte.id.as_ref(), matte.at, matte.duration)?,
                inset: inset_sides(matte.inset),
                content: self.lower(&matte.content, &format!("{path}.content"))?,
                by: Coverage::Matte {
                    by: self.lower(&matte.by, &format!("{path}.by"))?,
                    mode: match (matte.uses.unwrap_or_default(), matte.invert) {
                        (MatteUse::Alpha, false) => alpha2_model::MatteMode::Alpha,
                        (MatteUse::Alpha, true) => alpha2_model::MatteMode::AlphaInverted,
                        (MatteUse::Luma, false) => alpha2_model::MatteMode::Luma,
                        (MatteUse::Luma, true) => alpha2_model::MatteMode::LumaInverted,
                    },
                },
            }))),
            Node::Mask(mask) => {
                if mask.shapes.is_empty() || mask.shapes.len() > alpha2_model::MAX_CLIP_MASKS {
                    return Err(Self::error(
                        &format!("{path}.shapes"),
                        format!("a mask has 1 to {} outlines", alpha2_model::MAX_CLIP_MASKS),
                    ));
                }
                let shapes = mask
                    .shapes
                    .iter()
                    .enumerate()
                    .map(|(index, shape)| {
                        let at = format!("{path}.shapes[{index}]");
                        Ok(MaskShapePrim {
                            geometry: Self::geometry(&shape.geometry, &at)?,
                            mode: shape.mode.unwrap_or_default(),
                            feather: match &shape.feather {
                                Some(feather) => {
                                    Self::anim(&format!("{at}.feather"), feather, |value| {
                                        value.is_finite() && *value >= 0.0
                                    })?
                                }
                                None => AnimPrim::Value(0.0),
                            },
                            invert: shape.invert,
                        })
                    })
                    .collect::<Result<Vec<_>, ComposeError>>()?;
                Ok(Prim::Coverage(Box::new(CoveragePrim {
                    head: Self::head(path, mask.id.as_ref(), mask.at, mask.duration)?,
                    inset: inset_sides(mask.inset),
                    content: self.lower(&mask.content, &format!("{path}.content"))?,
                    by: Coverage::Mask(shapes),
                })))
            }
            Node::Text(text) => self.lower_text(text, path),
            Node::Color(color) => Ok(Prim::Visual(Box::new(Visual {
                head: Self::head(path, color.id.as_ref(), color.at, color.duration)?,
                inset: inset_sides(color.inset),
                fit: Fit::Fill,
                align: Align::Center,
                opacity: 1.0,
                fade_in: 0,
                fade_out: 0,
                zoom: None,
                content: Content::Color(
                    color
                        .color
                        .as_ref()
                        .map_or(self.theme.surface, |color| self.solid(color)),
                ),
            }))),
            Node::Captions(captions) => self.lower_captions(captions, path),
            Node::Layout(layout) => Ok(Prim::Visual(Box::new(Visual {
                head: Self::head(path, layout.id.as_ref(), layout.at, layout.duration)?,
                inset: inset_sides(layout.inset),
                fit: Fit::Contain,
                align: layout.align.unwrap_or_default(),
                opacity: 1.0,
                fade_in: 0,
                fade_out: 0,
                zoom: None,
                content: Content::Layout(layout.root.clone()),
            }))),
            Node::Audio(audio) => self.lower_audio(audio, path),
            Node::Raw(raw) => Ok(Prim::Raw(
                Head {
                    path: path.to_owned(),
                    ..Head::default()
                },
                Box::new(raw.clone()),
            )),
            Node::Slideshow(slideshow) => self.lower_slideshow(slideshow, path),
            Node::CaptionedVideo(captioned) => {
                let mut video = Self::item_media(&captioned.video, &format!("{path}.video"))?;
                video.fit = Some(video.fit.or(captioned.fit).unwrap_or(Fit::Cover));
                let video = self.lower_media(&video, &format!("{path}.video"))?;
                let captions =
                    self.lower_captions(&captioned.captions, &format!("{path}.captions"))?;
                Ok(Prim::Stack(StackPrim {
                    head: Self::head(path, captioned.id.as_ref(), captioned.at, None)?,
                    children: vec![video, captions],
                    inset: [0.0; 4],
                    grid: None,
                }))
            }
            Node::MusicBed(music) => self.lower_music_bed(music, path),
            Node::Counter(counter) => self.lower_counter(counter, path),
        }
    }

    fn lower_group(&mut self, group: &Group, path: &str) -> Result<Prim, ComposeError> {
        let opacity = group
            .opacity
            .as_ref()
            .map(|opacity| {
                Self::anim(&format!("{path}.opacity"), opacity, |value| {
                    (0.0..=1.0).contains(value)
                })
            })
            .transpose()?;
        let transform = group.transform.clone().unwrap_or_default();
        let at = format!("{path}.transform");
        let transform = TransformPrim {
            position: transform
                .position
                .as_ref()
                .map(|position| {
                    Self::anim(&format!("{at}.position"), position, |value| {
                        value.iter().all(|part| part.is_finite())
                    })
                })
                .transpose()?,
            scale: transform
                .scale
                .as_ref()
                .map(|scale| {
                    Self::anim(&format!("{at}.scale"), scale, |value| {
                        value.is_finite() && *value > 0.0
                    })
                })
                .transpose()?,
            rotation: transform
                .rotation
                .as_ref()
                .map(|rotation| {
                    Self::anim(&format!("{at}.rotation"), rotation, |value| {
                        value.is_finite()
                    })
                })
                .transpose()?,
            anchor: match transform.anchor {
                Some(anchor) if anchor.iter().all(|part| part.is_finite()) => anchor,
                Some(_) => return Err(Self::error(&format!("{at}.anchor"), "an anchor is finite")),
                None => [0.5, 0.5],
            },
        };
        let effects = group
            .effects
            .iter()
            .enumerate()
            .map(|(index, effect)| self.lower_effect(effect, &format!("{path}.effects[{index}]")))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Prim::Group(Box::new(GroupPrim {
            stack: StackPrim {
                head: Self::head(path, group.id.as_ref(), group.at, group.duration)?,
                children: self.lower_children(&group.children, &format!("{path}.children"))?,
                inset: inset_sides(group.inset),
                grid: None,
            },
            opacity,
            blend: group.blend.unwrap_or_default(),
            effects,
            transform,
        })))
    }

    fn lower_shape(&mut self, shape: &Shape, path: &str) -> Result<Prim, ComposeError> {
        if shape.shapes.is_empty() {
            return Err(Self::error(
                &format!("{path}.shapes"),
                "a shape has at least one outline",
            ));
        }
        let geometry = shape
            .shapes
            .iter()
            .enumerate()
            .map(|(index, geometry)| Self::geometry(geometry, &format!("{path}.shapes[{index}]")))
            .collect::<Result<Vec<_>, _>>()?;
        let stroke = shape
            .stroke
            .as_ref()
            .map(|stroke| {
                let at = format!("{path}.stroke");
                if !stroke
                    .dash
                    .iter()
                    .all(|length| length.is_finite() && *length >= 0.0)
                {
                    return Err(Self::error(
                        &format!("{at}.dash"),
                        "dash lengths are not negative",
                    ));
                }
                Ok(StrokePrim {
                    paint: match &stroke.paint {
                        Some(paint) => self.paint(paint, &format!("{at}.paint"))?,
                        None => PaintPrim::Solid(self.theme.text),
                    },
                    width: Self::anim(&format!("{at}.width"), &stroke.width, |width| {
                        width.is_finite() && *width >= 0.0
                    })?,
                    dash: stroke.dash.clone(),
                    cap: stroke.cap.unwrap_or_default(),
                    join: stroke.join.unwrap_or_default(),
                })
            })
            .transpose()?;
        let fill = match &shape.fill {
            Some(paint) => Some(self.paint(paint, &format!("{path}.fill"))?),
            None if stroke.is_none() => Some(PaintPrim::Solid(self.theme.accent)),
            None => None,
        };
        let trim = shape
            .trim
            .as_ref()
            .map(|trim| {
                let at = format!("{path}.trim");
                let part = |field: &str, anim: Option<&Anim<f32>>, default: f32| {
                    anim.map_or(Ok(AnimPrim::Value(default)), |anim| {
                        Self::anim(&format!("{at}.{field}"), anim, |value| value.is_finite())
                    })
                };
                Ok::<_, ComposeError>(TrimPrim {
                    start: part("start", trim.start.as_ref(), 0.0)?,
                    end: part("end", trim.end.as_ref(), 1.0)?,
                    offset: part("offset", trim.offset.as_ref(), 0.0)?,
                })
            })
            .transpose()?;
        Ok(Prim::Visual(Box::new(Visual {
            head: Self::head(path, shape.id.as_ref(), shape.at, shape.duration)?,
            inset: inset_sides(shape.inset),
            fit: Fit::Fill,
            align: Align::Center,
            opacity: Self::unit(path, "opacity", shape.opacity, 1.0)?,
            fade_in: 0,
            fade_out: 0,
            zoom: None,
            content: Content::Shape(Box::new(ShapeContent {
                geometry,
                fill,
                stroke,
                trim,
            })),
        })))
    }

    /// Lowers one outline, its omitted values defaulted.
    fn geometry(geometry: &Geometry, path: &str) -> Result<GeometryPrim, ComposeError> {
        let point = |field: &str, anim: Option<&Anim<[f32; 2]>>, default: [f32; 2]| {
            anim.map_or(Ok(AnimPrim::Value(default)), |anim| {
                Self::anim(&format!("{path}.{field}"), anim, |value| {
                    value.iter().all(|part| part.is_finite())
                })
            })
        };
        let size = |anim: Option<&Anim<[f32; 2]>>| {
            anim.map_or(Ok(AnimPrim::Value([1.0, 1.0])), |anim| {
                Self::anim(&format!("{path}.size"), anim, |value| {
                    value.iter().all(|part| part.is_finite() && *part >= 0.0)
                })
            })
        };
        let length = |field: &str, anim: Option<&Anim<f32>>, default: f32| {
            anim.map_or(Ok(AnimPrim::Value(default)), |anim| {
                Self::anim(&format!("{path}.{field}"), anim, |value| {
                    value.is_finite() && *value >= 0.0
                })
            })
        };
        let turn = |anim: Option<&Anim<f32>>| {
            anim.map_or(Ok(AnimPrim::Value(0.0)), |anim| {
                Self::anim(&format!("{path}.rotation"), anim, |value| value.is_finite())
            })
        };
        #[allow(clippy::cast_precision_loss)]
        let points = |points: u32| {
            if (alpha2_model::MIN_POLYSTAR_POINTS..=alpha2_model::MAX_POLYSTAR_POINTS)
                .contains(&(points as f32))
            {
                Ok(points)
            } else {
                Err(Self::error(
                    &format!("{path}.points"),
                    format!(
                        "from {} to {} points",
                        alpha2_model::MIN_POLYSTAR_POINTS,
                        alpha2_model::MAX_POLYSTAR_POINTS
                    ),
                ))
            }
        };
        Ok(match geometry {
            Geometry::Rect {
                center,
                size: extent,
                corner_radius,
                rotation,
            } => GeometryPrim::Rect {
                center: point("center", center.as_ref(), [0.5, 0.5])?,
                size: size(extent.as_ref())?,
                corner: length("corner_radius", corner_radius.as_ref(), 0.0)?,
                rotation: turn(rotation.as_ref())?,
            },
            Geometry::Ellipse {
                center,
                size: extent,
                rotation,
            } => GeometryPrim::Ellipse {
                center: point("center", center.as_ref(), [0.5, 0.5])?,
                size: size(extent.as_ref())?,
                rotation: turn(rotation.as_ref())?,
            },
            Geometry::Polygon {
                center,
                points: count,
                radius,
                roundness,
                rotation,
            } => GeometryPrim::Polystar {
                kind: alpha2_model::PolystarKind::Polygon,
                center: point("center", center.as_ref(), [0.5, 0.5])?,
                points: points(*count)?,
                outer: length("radius", radius.as_ref(), 0.5)?,
                inner: AnimPrim::Value(0.0),
                roundness: length("roundness", roundness.as_ref(), 0.0)?,
                rotation: turn(rotation.as_ref())?,
            },
            Geometry::Star {
                center,
                points: count,
                outer_radius,
                inner_radius,
                rotation,
            } => GeometryPrim::Polystar {
                kind: alpha2_model::PolystarKind::Star,
                center: point("center", center.as_ref(), [0.5, 0.5])?,
                points: points(*count)?,
                outer: length("outer_radius", outer_radius.as_ref(), 0.5)?,
                inner: length("inner_radius", inner_radius.as_ref(), 0.25)?,
                roundness: AnimPrim::Value(0.0),
                rotation: turn(rotation.as_ref())?,
            },
            Geometry::Path { d } => GeometryPrim::Path(
                crate::svg_path::parse(d)
                    .map_err(|message| Self::error(&format!("{path}.d"), message))?,
            ),
        })
    }

    /// A theme color, or a color as written.
    pub(crate) fn solid(&self, color: &SolidColor) -> Rgba {
        match color {
            SolidColor::Color(color) => *color,
            SolidColor::Theme(theme) => self.theme_color(*theme),
        }
    }

    fn theme_color(&self, color: ThemeColor) -> Rgba {
        let mut picked = match color.token {
            ThemeToken::Text => self.theme.text,
            ThemeToken::Background => self.theme.background,
            ThemeToken::Surface => self.theme.surface,
            ThemeToken::Panel => self.theme.panel,
            ThemeToken::Accent => self.theme.accent,
        };
        picked.alpha *= color.alpha.unwrap_or(1.0).clamp(0.0, 1.0);
        picked
    }

    /// Lowers a paint, resolving theme colors and spacing gradient stops.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn paint(&self, paint: &PaintChoice, path: &str) -> Result<PaintPrim, ComposeError> {
        let stops = |gradient: &Gradient| {
            let count = gradient.stops.len();
            if !(2..=alpha2_model::MAX_GRADIENT_STOPS).contains(&count) {
                return Err(Self::error(
                    &format!("{path}.stops"),
                    format!(
                        "a gradient has 2 to {} stops",
                        alpha2_model::MAX_GRADIENT_STOPS
                    ),
                ));
            }
            let mut previous = 0.0_f32;
            gradient
                .stops
                .iter()
                .enumerate()
                .map(|(index, stop)| {
                    let (offset, color) = match stop {
                        Stop::At(stop) => (stop.offset, self.solid(&stop.color)),
                        Stop::Even(color) => (index as f32 / (count - 1) as f32, self.solid(color)),
                    };
                    if !(previous..=1.0).contains(&offset) {
                        return Err(Self::error(
                            &format!("{path}.stops[{index}].offset"),
                            "offsets are in 0..=1, each not before the one before",
                        ));
                    }
                    previous = offset;
                    Ok((offset, color))
                })
                .collect::<Result<Vec<_>, _>>()
        };
        let center = |gradient: &Gradient| gradient.center.unwrap_or([0.5, 0.5]);
        Ok(match paint {
            PaintChoice::Color(color) => PaintPrim::Solid(*color),
            PaintChoice::Theme(theme) => PaintPrim::Solid(self.theme_color(*theme)),
            PaintChoice::Linear(LinearPaint { linear }) => PaintPrim::Linear {
                stops: stops(linear)?,
                angle: linear.angle.unwrap_or(180.0),
            },
            PaintChoice::Radial(RadialPaint { radial }) => PaintPrim::Radial {
                stops: stops(radial)?,
                center: center(radial),
                radius: radial.radius.unwrap_or(0.5),
            },
            PaintChoice::Conic(ConicPaint { conic }) => PaintPrim::Conic {
                stops: stops(conic)?,
                center: center(conic),
                angle: conic.angle.unwrap_or(0.0),
            },
        })
    }

    /// Lowers a box and sorts its children into nested boxes, layout leaves and other nodes.
    fn box_tree(&mut self, node: &BoxNode, path: &str) -> Result<BoxTree, ComposeError> {
        const OWN: [&str; 6] = ["children", "text", "shape", "position", "inset", "type"];
        if let Some(field) = node.css.keys().find(|field| OWN.contains(&field.as_str())) {
            return Err(Self::error(
                &format!("{path}.{field}"),
                format!("a box sets `{field}` itself"),
            ));
        }
        let mut children = Vec::with_capacity(node.children.len());
        for (index, child) in node.children.iter().enumerate() {
            let at = format!("{path}.children[{index}]");
            let content = match child.node() {
                Node::Box(inner) => {
                    if inner.at.is_some() || inner.duration.is_some() || inner.inset.is_some() {
                        return Err(Self::error(
                            &at,
                            "a box in a box shares its time and takes its place from its item \
                             fields, so it has no `at`, `duration` or `inset`",
                        ));
                    }
                    BoxContent::Tree(self.box_tree(inner, &at)?)
                }
                Node::Text(_) | Node::Shape(_) | Node::Color(_) => {
                    match self.lower(child.node(), &at)? {
                        Prim::Visual(visual) => BoxContent::Native(visual),
                        other => BoxContent::Foreign(other),
                    }
                }
                other => BoxContent::Foreign(self.lower(other, &at)?),
            };
            let mut item = child.item();
            for (field, value) in &mut item {
                self.theme_lengths(value, &format!("{at}.box.{field}"))?;
            }
            children.push(BoxKid { item, content });
        }
        let mut css = node.css.clone();
        for (field, value) in &mut css {
            self.theme_lengths(value, &format!("{path}.{field}"))?;
        }
        Ok(BoxTree {
            css,
            background: node
                .background
                .as_ref()
                .map(|paint| self.paint(paint, &format!("{path}.background")))
                .transpose()?,
            border: node
                .border
                .as_ref()
                .map(|border| {
                    Ok::<_, ComposeError>((
                        serde_json::to_value(&border.width).expect("a size serializes"),
                        self.paint(&border.paint, &format!("{path}.border.paint"))?,
                    ))
                })
                .transpose()?,
            children,
        })
    }

    /// A theme length, as a fraction of the picture's shorter side.
    fn theme_length(&self, length: &ThemeLength) -> f32 {
        let token = match length.token {
            LengthToken::Margin => self.theme.margin,
            LengthToken::TitleSize => self.theme.title_size,
            LengthToken::SubtitleSize => self.theme.subtitle_size,
            LengthToken::BodySize => self.theme.body_size,
            LengthToken::CaptionSize => self.theme.caption_size,
            LengthToken::LabelSize => self.theme.label_size,
        };
        token * length.scale.unwrap_or(1.0)
    }

    /// `size` in pixels, above zero.
    fn size_px(&self, path: &str, size: &Size) -> Result<f32, ComposeError> {
        let pixels = match size {
            Size::Pixels(pixels) => *pixels,
            Size::Theme(length) => self.theme_px(self.theme_length(length)),
        };
        if pixels.is_finite() && pixels > 0.0 {
            Ok(pixels)
        } else {
            Err(Self::error(
                path,
                format!("{pixels} is not a size above zero in pixels"),
            ))
        }
    }

    /// `value` with each theme length in it, `{"$theme": "margin", "scale": 2}`, replaced by its
    /// fraction of the picture's shorter side.
    fn theme_lengths(&self, value: &mut serde_json::Value, path: &str) -> Result<(), ComposeError> {
        match value {
            serde_json::Value::Object(object) if object.contains_key("$theme") => {
                let length: ThemeLength =
                    crate::de::from_value(value.clone()).map_err(|error| {
                        Self::error(&crate::de::join(path, &error.path), error.message)
                    })?;
                *value = serde_json::json!(self.theme_length(&length));
            }
            serde_json::Value::Object(object) => {
                for (field, inner) in object {
                    self.theme_lengths(inner, &format!("{path}.{field}"))?;
                }
            }
            serde_json::Value::Array(items) => {
                for (index, inner) in items.iter_mut().enumerate() {
                    self.theme_lengths(inner, &format!("{path}[{index}]"))?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn lower_effect(&mut self, effect: &EffectUse, path: &str) -> Result<EffectPrim, ComposeError> {
        let id = effect_id(&effect.effect);
        let spec = alpha2_model::builtin_effect(&id);
        if spec.is_none() && id.starts_with(alpha2_model::BUILTIN_EFFECT_PREFIX) {
            return Err(Self::error(
                &format!("{path}.effect"),
                format!(
                    "\"{}\" is not a built-in effect; schema/effects.v1.json lists them",
                    effect.effect
                ),
            ));
        }
        if spec.is_some_and(|spec| spec.kind != alpha2_model::EffectKind::Effect) {
            return Err(Self::error(
                &format!("{path}.effect"),
                format!("\"{}\" is a transition, not an effect", effect.effect),
            ));
        }
        let mut params = BTreeMap::new();
        let mut inputs = BTreeMap::new();
        for (name, value) in &effect.params {
            let at = format!("{path}.{name}");
            if spec.is_some_and(|spec| spec.inputs.contains(name)) {
                let node: Node = crate::de::from_value(value.clone()).map_err(|error| {
                    Self::error(&crate::de::join(&at, &error.path), error.message)
                })?;
                inputs.insert(name.clone(), self.lower(&node, &at)?);
                continue;
            }
            let param = match spec {
                Some(spec) => Some(spec.param(name).ok_or_else(|| {
                    let names = spec
                        .params
                        .iter()
                        .map(|param| param.name.as_str())
                        .chain(spec.inputs.iter().map(String::as_str))
                        .collect::<Vec<_>>();
                    Self::error(
                        &at,
                        format!(
                            "{} has no parameter {name}; it takes {}",
                            effect.effect,
                            if names.is_empty() {
                                "none".to_owned()
                            } else {
                                names.join(", ")
                            }
                        ),
                    )
                })?),
                None => None,
            };
            let anim: Anim<serde_json::Value> = crate::de::from_value(value.clone())
                .map_err(|error| Self::error(&at, error.message))?;
            let typed = map_anim(&anim, |value| effect_value(param, value))
                .ok_or_else(|| Self::error(&at, "a value of the parameter's type"))?;
            let lowered = Self::anim(&at, &typed, |value| {
                param.is_none_or(|param| param.accepts(value))
            })?;
            params.insert(name.clone(), lowered);
        }
        if let Some(spec) = spec
            && let Some(missing) = spec.inputs.iter().find(|name| !inputs.contains_key(*name))
        {
            return Err(Self::error(
                path,
                format!(
                    "{} needs its image input {missing} as a node",
                    effect.effect
                ),
            ));
        }
        Ok(EffectPrim {
            path: path.to_owned(),
            version: effect
                .version
                .or_else(|| spec.map(|spec| spec.version))
                .unwrap_or(1),
            id,
            params,
            inputs,
        })
    }

    fn lower_children(&mut self, children: &[Node], path: &str) -> Result<Vec<Prim>, ComposeError> {
        children
            .iter()
            .enumerate()
            .map(|(index, child)| self.lower(child, &format!("{path}[{index}]")))
            .collect()
    }

    fn pixels(path: &str, field: &str, value: f32) -> Result<f32, ComposeError> {
        if value.is_finite() && value >= 0.0 {
            Ok(value)
        } else {
            Err(Self::error(
                &format!("{path}.{field}"),
                format!("{value} is not a length: pixels are finite and not negative"),
            ))
        }
    }

    /// The grid's columns: as given, or the fewest that make it no taller than wide.
    fn columns(path: &str, columns: Option<u32>, cells: usize) -> Result<u32, ComposeError> {
        match columns {
            Some(0) => Err(Self::error(
                &format!("{path}.columns"),
                "a grid has at least one column",
            )),
            Some(columns) => Ok(columns),
            None => {
                let cells = u32::try_from(cells.max(1)).unwrap_or(u32::MAX);
                let mut columns = 1;
                while columns * columns < cells {
                    columns += 1;
                }
                Ok(columns)
            }
        }
    }

    /// The media node an item stands for.
    fn item_media(item: &MediaItem, path: &str) -> Result<Media, ComposeError> {
        match item {
            MediaItem::Path(file) => Ok(Media::new(file.clone())),
            MediaItem::Node(node) => match &**node {
                Node::Media(media) => Ok(media.clone()),
                _ => Err(Self::error(path, "this takes a file path or a media node")),
            },
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn lower_media(&mut self, media: &Media, path: &str) -> Result<Prim, ComposeError> {
        let head = Self::head(path, media.id.as_ref(), media.at, media.duration)?;
        let kind = media
            .kind
            .unwrap_or_else(|| crate::kind_of_path(&media.path));
        let facts = self.facts.media.get(&media.path);
        let size = facts.and_then(|facts| Some((facts.width?, facts.height?)));
        let missing = |what: &str| {
            Self::error(
                &format!("{path}.path"),
                format!(
                    "no {what} for \"{}\" in the facts; gather them for every file `requirements` lists",
                    media.path
                ),
            )
        };
        let blur = match media.blur {
            Some(blur) if !(blur.is_finite() && (0.0..=1.0).contains(&blur)) => {
                return Err(Self::error(
                    &format!("{path}.blur"),
                    format!("{blur} is outside 0..=1"),
                ));
            }
            blur => blur,
        };
        let content = match kind {
            FileKind::Audio => {
                return Err(Self::error(
                    &format!("{path}.path"),
                    format!("\"{}\" is a sound file: use an audio node", media.path),
                ));
            }
            FileKind::Image => Content::Image {
                file: media.path.clone(),
                size: size.ok_or_else(|| missing("picture size"))?,
                blur,
            },
            FileKind::Video => {
                let facts = facts.ok_or_else(|| missing("media facts"))?;
                let end = facts.duration_ticks.ok_or_else(|| missing("duration"))?;
                let trim_start =
                    Self::optional_time(path, "trim_start", media.trim_start)?.unwrap_or(0);
                let trim_end =
                    Self::optional_time(path, "trim_end", media.trim_end)?.unwrap_or(end);
                if trim_end <= trim_start {
                    return Err(Self::error(
                        path,
                        format!(
                            "it plays nothing: trim_end ({} s) is not after trim_start ({} s)",
                            crate::compile::seconds(trim_end),
                            crate::compile::seconds(trim_start)
                        ),
                    ));
                }
                let speed = media.speed.unwrap_or(1.0);
                if !(speed.is_finite() && speed > 0.0) {
                    return Err(Self::error(
                        &format!("{path}.speed"),
                        format!("{speed} is not a speed: it is positive"),
                    ));
                }
                let rate = facts.frame_rate.unwrap_or(self.rate);
                let frame_ticks = rate
                    .frame_start(1)
                    .map_or(1, alpha2_time::MediaTime::ticks)
                    .max(1);
                Content::Video(VideoContent {
                    file: media.path.clone(),
                    size: size.ok_or_else(|| missing("picture size"))?,
                    trim_start,
                    trim_end,
                    speed,
                    preserve_pitch: media.preserve_pitch.unwrap_or(true),
                    looped: media.r#loop,
                    when_short: media.when_short,
                    when_long: media.when_long.unwrap_or(crate::spec::WhenLong::Trim),
                    volume: media
                        .volume
                        .map_or(Ok(1.0), |volume| Self::gain(path, "volume", volume))?,
                    muted: media.muted,
                    has_audio: facts.has_audio,
                    blur,
                    frame_ticks,
                })
            }
        };
        let zoom = media
            .zoom
            .as_ref()
            .map(|zoom| {
                Self::anim(&format!("{path}.zoom"), zoom, |value| {
                    value.is_finite() && *value > 0.0
                })
            })
            .transpose()?;
        Ok(Prim::Visual(Box::new(Visual {
            head,
            inset: inset_sides(media.inset),
            fit: media.fit.unwrap_or_default(),
            align: media.align.unwrap_or_default(),
            opacity: Self::unit(path, "opacity", media.opacity, 1.0)?,
            fade_in: Self::optional_time(path, "fade_in", media.fade_in)?.unwrap_or(0),
            fade_out: Self::optional_time(path, "fade_out", media.fade_out)?.unwrap_or(0),
            zoom,
            content,
        })))
    }

    fn gain(path: &str, field: &str, value: f32) -> Result<f32, ComposeError> {
        if value.is_finite() && (0.0..=alpha2_model::MAX_GAIN).contains(&value) {
            Ok(value)
        } else {
            Err(Self::error(
                &format!("{path}.{field}"),
                format!(
                    "{value} is outside 0..={} (linear gain)",
                    alpha2_model::MAX_GAIN
                ),
            ))
        }
    }

    fn lower_counter(&mut self, counter: &Counter, path: &str) -> Result<Prim, ComposeError> {
        for (field, value) in [("start", counter.start), ("end", counter.end)] {
            if !value.is_finite() || value.abs() > COUNTER_MAX {
                return Err(Self::error(
                    &format!("{path}.{field}"),
                    format!(
                        "{value} is not a number a counter shows (at most {COUNTER_MAX:e} either way)"
                    ),
                ));
            }
        }
        let count = Self::optional_time(path, "count_duration", counter.count_duration)?;
        if count == Some(0) {
            return Err(Self::error(
                &format!("{path}.count_duration"),
                "a count lasts longer than zero",
            ));
        }
        let format = counter.format.clone().unwrap_or_default();
        let decimals = format.decimals.unwrap_or(0);
        if decimals > 6 {
            return Err(Self::error(
                &format!("{path}.format.decimals"),
                format!("{decimals} decimals: at most 6"),
            ));
        }
        let font_px = match &counter.font_size {
            Some(size) => self.size_px(&format!("{path}.font_size"), size)?,
            None => self.role_size(counter.style.unwrap_or(TextRole::Title)),
        };
        Ok(Prim::Visual(Box::new(Visual {
            head: Self::head(path, counter.id.as_ref(), counter.at, counter.duration)?,
            inset: inset_sides(counter.inset),
            fit: Fit::None,
            align: counter.align.unwrap_or_default(),
            opacity: Self::unit(path, "opacity", counter.opacity, 1.0)?,
            fade_in: 0,
            fade_out: 0,
            zoom: None,
            content: Content::Counter(Box::new(CounterContent {
                start: counter.start,
                end: counter.end,
                count,
                easing: Self::easing(path, counter.easing.as_ref())?,
                decimals,
                thousands: format.thousands.unwrap_or_default(),
                point: format.point.unwrap_or_else(|| ".".to_owned()),
                prefix: format.prefix.unwrap_or_default(),
                suffix: format.suffix.unwrap_or_default(),
                font_px,
                family: counter
                    .font_family
                    .clone()
                    .or_else(|| self.theme.font_family.clone()),
                color: counter
                    .color
                    .as_ref()
                    .map_or(self.theme.text, |color| self.solid(color)),
                strokes: counter.strokes.clone(),
                shadows: counter.shadows.clone(),
            })),
        })))
    }

    fn role_size(&self, role: TextRole) -> f32 {
        let fraction = match role {
            TextRole::Title => self.theme.title_size,
            TextRole::Subtitle => self.theme.subtitle_size,
            TextRole::Body => self.theme.body_size,
            TextRole::Label => self.theme.label_size,
        };
        self.theme_px(fraction)
    }

    /// The font sizes in pixels `text` is fitted between, and what ends a line it cuts.
    fn text_fit(text: &Text, font_px: f32, path: &str) -> Result<TextFitting, ComposeError> {
        if text.max_lines == Some(0) {
            return Err(Self::error(
                &format!("{path}.max_lines"),
                "a text takes at least one line",
            ));
        }
        let fit = match text.fit {
            Some(fit) => {
                let max = fit.max.unwrap_or(font_px);
                let valid = |size: f32| size.is_finite() && size > 0.0;
                if !valid(fit.min) || !valid(max) || fit.min > max {
                    return Err(Self::error(
                        &format!("{path}.fit"),
                        "a fit is from a positive font size up to one at least as large",
                    ));
                }
                Some((fit.min, max))
            }
            None => text.max_lines.map(|_| (0.3 * font_px, font_px)),
        };
        let ellipsis = match &text.ellipsis {
            None | Some(Ellipsis::Shown(true)) => alpha2_model::BlockEllipsis::Ellipsis,
            Some(Ellipsis::Shown(false)) => alpha2_model::BlockEllipsis::NoEllipsis,
            Some(Ellipsis::Text(mark)) => alpha2_model::BlockEllipsis::String(mark.clone()),
        };
        Ok((
            fit,
            (text.max_lines.is_some() || text.ellipsis.is_some()).then_some(ellipsis),
        ))
    }

    fn lower_text(&mut self, text: &Text, path: &str) -> Result<Prim, ComposeError> {
        if text.text.is_empty() {
            return Err(Self::error(&format!("{path}.text"), "text is not empty"));
        }
        let font_px = match &text.font_size {
            Some(size) => self.size_px(&format!("{path}.font_size"), size)?,
            None => self.role_size(text.style),
        };
        let fade = Self::optional_time(path, "fade", text.fade)?.unwrap_or(0);
        let (fit, ellipsis) = Self::text_fit(text, font_px, path)?;
        let reveal = text
            .reveal
            .as_ref()
            .map(|reveal| {
                let path = format!("{path}.reveal");
                let duration = match reveal.duration {
                    Some(seconds) => Self::time(&path, "duration", seconds)?,
                    None => seconds_to_ticks(self.theme.fade_seconds),
                };
                let stagger = match reveal.stagger {
                    Some(seconds) => Self::time(&path, "stagger", seconds)?,
                    None => duration / 2,
                };
                if duration == 0 || stagger == 0 {
                    return Err(Self::error(
                        &path,
                        "a reveal's duration and stagger are longer than zero",
                    ));
                }
                if duration / stagger > REVEAL_MAX_OVERLAP {
                    return Err(Self::error(
                        &path,
                        format!(
                            "a unit takes {} staggers; at most {REVEAL_MAX_OVERLAP} units change at once",
                            duration / stagger
                        ),
                    ));
                }
                let rise = match (reveal.kind.unwrap_or(RevealKind::Fade), reveal.distance) {
                    (RevealKind::Fade, Some(_)) => {
                        return Err(Self::error(
                            &format!("{path}.distance"),
                            "distance is for a rise only",
                        ));
                    }
                    (RevealKind::Fade, None) => 0.0,
                    (RevealKind::Rise, Some(distance)) => {
                        Self::pixels(&path, "distance", distance)?
                    }
                    (RevealKind::Rise, None) => 0.5 * font_px,
                };
                Ok(RevealPrim {
                    unit: match reveal.by {
                        RevealUnit::Word => TextUnit::Words,
                        RevealUnit::Line => TextUnit::Lines,
                    },
                    stagger,
                    duration,
                    rise,
                })
            })
            .transpose()?;
        Ok(Prim::Visual(Box::new(Visual {
            head: Self::head(path, text.id.as_ref(), text.at, text.duration)?,
            inset: inset_sides(text.inset),
            fit: Fit::None,
            align: text.align.unwrap_or_default(),
            opacity: Self::unit(path, "opacity", text.opacity, 1.0)?,
            fade_in: fade,
            fade_out: fade,
            zoom: None,
            content: Content::Text(TextContent {
                text: text.text.clone(),
                font_px,
                family: text
                    .font_family
                    .clone()
                    .or_else(|| self.theme.font_family.clone()),
                color: text
                    .color
                    .as_ref()
                    .map_or(self.theme.text, |color| self.solid(color)),
                background: text.background.as_ref().map(|color| self.solid(color)),
                strokes: text.strokes.clone(),
                shadows: text.shadows.clone(),
                max_lines: text.max_lines,
                fit,
                ellipsis,
                reveal,
            }),
        })))
    }

    #[allow(clippy::too_many_lines)]
    fn lower_captions(&mut self, captions: &Captions, path: &str) -> Result<Prim, ComposeError> {
        let head = Self::head(path, captions.id.as_ref(), captions.at, captions.duration)?;
        let given = [
            !captions.cues.is_empty(),
            !captions.words.is_empty(),
            captions.file.is_some(),
            captions.srt.is_some(),
            captions.vtt.is_some(),
        ];
        if given.iter().filter(|given| **given).count() != 1 {
            return Err(Self::error(
                path,
                "captions take exactly one of cues, words, file, srt and vtt",
            ));
        }
        let vertical = self.vertical();
        let cues = if !captions.cues.is_empty() {
            Self::cues(&captions.cues, &format!("{path}.cues"))?
        } else if !captions.words.is_empty() {
            let max_chars = captions.max_chars.unwrap_or(if vertical { 32 } else { 42 });
            Self::group_words(&captions.words, max_chars, &format!("{path}.words"))?
        } else {
            let (text, field, vtt) = if let Some(file) = &captions.file {
                let text = self.facts.texts.get(file).ok_or_else(|| {
                    Self::error(&format!("{path}.file"), format!(
                        "no text for \"{file}\" in the facts; read every file `requirements` lists"
                    ))
                })?;
                let vtt = file.to_ascii_lowercase().ends_with(".vtt")
                    || text.trim_start_matches('\u{feff}').starts_with("WEBVTT");
                (text.as_str(), "file", vtt)
            } else if let Some(srt) = &captions.srt {
                (srt.as_str(), "srt", false)
            } else {
                (captions.vtt.as_deref().unwrap_or_default(), "vtt", true)
            };
            let imported = if vtt {
                alpha2_model::parse_webvtt(text)
            } else {
                alpha2_model::parse_srt(text)
            }
            .map_err(|error| Self::error(&format!("{path}.{field}"), error.to_string()))?;
            if imported.skipped > 0 {
                self.warn(format!(
                    "{path}.{field}: {} blocks that are not cues were skipped",
                    imported.skipped
                ));
            }
            imported.cues
        };
        if cues.is_empty() {
            return Err(Self::error(path, "the captions have no cues"));
        }
        let (width, height) = self.canvas_px();
        let mut font_px = match &captions.font_size {
            Some(size) => self.size_px(&format!("{path}.font_size"), size)?,
            None => self.theme_px(self.theme.caption_size),
        };
        let position = match captions.position {
            Some(position) if !(position.is_finite() && (0.0..=1.0).contains(&position)) => {
                return Err(Self::error(
                    &format!("{path}.position"),
                    format!("{position} is outside 0..=1"),
                ));
            }
            position => position,
        };
        let outline = |width: f32| {
            vec![TextStroke {
                color: Rgba::new(0.0, 0.0, 0.0, 0.85),
                width,
            }]
        };
        let mut style = TextStyle {
            font_family: self.theme.font_family.clone(),
            fill: Paint::Solid {
                color: captions
                    .color
                    .as_ref()
                    .map_or(self.theme.text, |color| self.solid(color)),
            },
            max_width: if vertical { 0.86 } else { 0.8 },
            anchor: alpha2_model::Vec2::new(0.5, 0.5),
            ..TextStyle::default()
        };
        let mut bar = None;
        let low = if vertical { 0.76 } else { 0.88 };
        let timed = cues
            .iter()
            .any(|cue| cue.text.runs().iter().any(|run| run.time.is_some()));
        let mut look = captions.style;
        if matches!(look, CaptionStyle::Karaoke | CaptionStyle::Pop) && !timed {
            self.warn(format!(
                "{path}.style: the captions have no word times, so they draw as bold"
            ));
            look = CaptionStyle::Bold;
        }
        let highlight = captions
            .highlight
            .as_ref()
            .map_or(self.theme.accent, |color| self.solid(color));
        let y = match look {
            CaptionStyle::Outline => {
                style.strokes = outline(0.12);
                position.unwrap_or(low)
            }
            CaptionStyle::Boxed => {
                style.background = Some(alpha2_model::TextBackground {
                    color: self.theme.panel,
                    padding: 0.3,
                });
                position.unwrap_or(low)
            }
            CaptionStyle::BarBottom => {
                let center = position.unwrap_or(if vertical { 0.8125 } else { 0.88 });
                let bar_height = (2.4 * font_px / height).max(if vertical { 0.125 } else { 0.15 });
                bar = Some((center, bar_height, self.theme.panel));
                center
            }
            CaptionStyle::BoldCenter => {
                font_px *= 1.35;
                style.strokes = outline(0.16);
                position.unwrap_or(if vertical { 0.62 } else { 0.7 })
            }
            CaptionStyle::Bold | CaptionStyle::Karaoke | CaptionStyle::Pop => {
                bold_look(
                    &mut style,
                    captions.color.as_ref().map(|color| self.solid(color)),
                );
                match look {
                    CaptionStyle::Karaoke => {
                        style.animators = [RunState::Spoken, RunState::Active]
                            .map(|state| highlight_animator(state, 0, highlight, None))
                            .into();
                    }
                    CaptionStyle::Pop => {
                        style.animators = vec![highlight_animator(
                            RunState::Active,
                            seconds_to_ticks(0.06),
                            highlight,
                            Some(alpha2_model::Vec2::new(1.15, 1.15)),
                        )];
                    }
                    _ => {}
                }
                position.unwrap_or(low)
            }
        };
        style.font_size = (font_px / self.short_side()).min(alpha2_model::MAX_CAPTION_FONT_SIZE);
        style.position = alpha2_model::Vec2::new(0.5, y);
        let _ = width;
        // Each speaker's look is a style named after them, picked up by their cues.
        let mut styles = std::collections::BTreeMap::new();
        for (name, speaker) in &captions.speakers {
            if name.is_empty() || name.len() > alpha2_model::MAX_CAPTION_NAME_BYTES {
                return Err(Self::error(
                    &format!("{path}.speakers"),
                    format!("{name:?} is not a speaker name of 1 to 64 bytes"),
                ));
            }
            styles.insert(
                name.clone(),
                alpha2_model::TextStylePatch {
                    fill: speaker.color.as_ref().map(|color| Paint::Solid {
                        color: self.solid(color),
                    }),
                    position: speaker.position,
                    ..alpha2_model::TextStylePatch::default()
                },
            );
        }
        if styles.len() > alpha2_model::MAX_CAPTION_STYLES {
            return Err(Self::error(
                &format!("{path}.speakers"),
                format!(
                    "{} speakers; at most {} take a look",
                    styles.len(),
                    alpha2_model::MAX_CAPTION_STYLES
                ),
            ));
        }
        Ok(Prim::Captions(Box::new(CaptionsPrim {
            head,
            cues,
            style,
            styles,
            bar,
        })))
    }

    fn cues(cues: &[Cue], path: &str) -> Result<Vec<CaptionCue>, ComposeError> {
        let mut out = Vec::with_capacity(cues.len());
        for (index, cue) in cues.iter().enumerate() {
            let at = format!("{path}[{index}]");
            let start = Self::time(&at, "start", cue.start)?;
            let end = Self::time(&at, "end", cue.end)?;
            if end <= start {
                return Err(Self::error(&at, "a cue ends after it starts"));
            }
            if cue.text.trim().is_empty() {
                return Err(Self::error(&format!("{at}.text"), "a cue has text"));
            }
            let mut caption = crate::compile::cue(start, end, cue.text.clone());
            caption.speaker.clone_from(&cue.speaker);
            out.push(caption);
        }
        out.sort_by_key(|cue| cue.timeline_range.start());
        Ok(out)
    }

    /// Words grouped into cues by [`alpha2_model::group_words`], after the checks cues get.
    fn group_words(
        words: &[Cue],
        max_chars: u32,
        path: &str,
    ) -> Result<Vec<CaptionCue>, ComposeError> {
        let words = Self::cues(words, path)?
            .into_iter()
            .map(|word| alpha2_model::DataWord {
                start: word.timeline_range.start(),
                end: word.timeline_range.end(),
                text: word.text.text().into_owned(),
                speaker: word.speaker,
            })
            .collect::<Vec<_>>();
        Ok(alpha2_model::group_words(&words, max_chars))
    }

    fn lower_audio(&mut self, audio: &Audio, path: &str) -> Result<Prim, ComposeError> {
        let head = Self::head(path, audio.id.as_ref(), audio.at, audio.duration)?;
        self.audio_prim(
            head,
            &audio.path,
            audio.trim_start,
            audio.trim_end,
            audio.r#loop,
            audio.volume,
            audio.fade_in,
            audio.fade_out,
            audio.duck_under.as_ref(),
            audio.duck_depth_db,
            (0.0, 0.0),
        )
    }

    fn lower_music_bed(&mut self, music: &MusicBed, path: &str) -> Result<Prim, ComposeError> {
        let head = Self::head(path, music.id.as_ref(), music.at, music.duration)?;
        self.audio_prim(
            head,
            &music.path,
            None,
            None,
            music.r#loop,
            music.volume,
            music.fade_in,
            music.fade_out,
            music.duck_under.as_ref(),
            music.duck_depth_db,
            (1.0, 2.0),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn audio_prim(
        &mut self,
        head: Head,
        file: &str,
        trim_start: Option<f64>,
        trim_end: Option<f64>,
        looped: bool,
        volume: Option<f32>,
        fade_in: Option<f64>,
        fade_out: Option<f64>,
        duck_under: Option<&String>,
        duck_depth_db: Option<f32>,
        fades: (f64, f64),
    ) -> Result<Prim, ComposeError> {
        let path = head.path.clone();
        let end = self
            .facts
            .media
            .get(file)
            .and_then(|facts| facts.duration_ticks)
            .ok_or_else(|| Self::error(&format!("{path}.path"), format!(
                "no duration for \"{file}\" in the facts; gather them for every file `requirements` lists"
            )))?;
        let trim_start = Self::optional_time(&path, "trim_start", trim_start)?.unwrap_or(0);
        let trim_end = Self::optional_time(&path, "trim_end", trim_end)?.unwrap_or(end);
        if trim_end <= trim_start {
            return Err(Self::error(
                &path,
                "it plays nothing: trim_end is not after trim_start",
            ));
        }
        let depth = duck_depth_db.unwrap_or(-12.0);
        if !(depth.is_finite() && (alpha2_model::MIN_LEVEL_DB..=0.0).contains(&depth)) {
            return Err(Self::error(
                &format!("{path}.duck_depth_db"),
                format!("{depth} is outside {}..=0 dB", alpha2_model::MIN_LEVEL_DB),
            ));
        }
        Ok(Prim::Audio(Box::new(AudioPrim {
            file: file.to_owned(),
            trim_start,
            trim_end,
            looped,
            volume: volume.map_or(Ok(1.0), |volume| Self::gain(&path, "volume", volume))?,
            fade_in: Self::time(&path, "fade_in", fade_in.unwrap_or(fades.0))?,
            fade_out: Self::time(&path, "fade_out", fade_out.unwrap_or(fades.1))?,
            duck_under: duck_under.cloned(),
            duck_depth_db: depth,
            head,
        })))
    }

    fn lower_slideshow(&mut self, slideshow: &Slideshow, path: &str) -> Result<Prim, ComposeError> {
        if slideshow.items.is_empty() {
            return Err(Self::error(
                &format!("{path}.items"),
                "a slideshow has at least one slide",
            ));
        }
        let theme_transition = self.theme.transition.clone();
        let transition =
            self.transition(path, slideshow.transition.as_ref(), Some(&theme_transition))?;
        let overlap = transition
            .as_ref()
            .map_or(0.0, |transition| transition.duration);
        let slides = slideshow.items.len();
        #[allow(clippy::cast_precision_loss)]
        let seconds = match (slideshow.seconds, &slideshow.fit_to) {
            (Some(seconds), _) => seconds,
            (None, Some(file)) => {
                let length = self
                    .facts
                    .media
                    .get(file)
                    .and_then(|facts| facts.duration_ticks)
                    .ok_or_else(|| Self::error(&format!("{path}.fit_to"), format!(
                        "no duration for \"{file}\" in the facts; gather them for every file `requirements` lists"
                    )))?;
                (crate::compile::seconds(length) - overlap) / slides as f64 + overlap
            }
            (None, None) => self.theme.still_seconds,
        };
        if !(seconds.is_finite() && seconds > overlap) {
            return Err(Self::error(
                &format!("{path}.seconds"),
                format!(
                    "each slide lasts {seconds} s, which is not longer than its {overlap} s transition"
                ),
            ));
        }
        let mut children = Vec::with_capacity(slides);
        for (index, item) in slideshow.items.iter().enumerate() {
            let at = format!("{path}.items[{index}]");
            let mut media = Self::item_media(item, &at)?;
            media.fit = Some(media.fit.or(slideshow.fit).unwrap_or(Fit::Cover));
            if media.duration.is_none() {
                media.duration = Some(seconds);
            }
            if let (None, Some(zoom)) = (&media.zoom, slideshow.ken_burns) {
                media.zoom = Some(Anim::Ramp(Ramp {
                    from: 1.0,
                    to: zoom,
                    ease: None,
                    at: None,
                    duration: None,
                }));
            }
            children.push(self.lower_media(&media, &at)?);
        }
        Ok(Prim::Sequence(SequencePrim {
            head: Self::head(path, slideshow.id.as_ref(), slideshow.at, None)?,
            children,
            transition,
        }))
    }
}

/// The `bold` look: white (or `color`) text, a black 0.12 em stroke, and a soft shadow.
fn bold_look(style: &mut TextStyle, color: Option<Rgba>) {
    style.fill = Paint::Solid {
        color: color.unwrap_or(Rgba::new(1.0, 1.0, 1.0, 1.0)),
    };
    style.strokes = vec![TextStroke {
        color: Rgba::new(0.0, 0.0, 0.0, 1.0),
        width: 0.12,
    }];
    style.shadows = vec![TextShadow {
        color: Rgba::new(0.0, 0.0, 0.0, 0.6),
        offset: alpha2_model::Vec2::new(0.0, 0.05),
        blur: 0.15,
    }];
}

/// An animator that turns the words in `state` the `highlight` color, and with `scale`
/// grows them, over a `ramp` of ticks.
fn highlight_animator(
    state: RunState,
    ramp: i64,
    highlight: Rgba,
    scale: Option<alpha2_model::Vec2>,
) -> alpha2_model::TextAnimator {
    alpha2_model::TextAnimator {
        selector: alpha2_model::TextSelector::Timing {
            state,
            ramp: MediaTime(ramp),
            sweep: false,
        },
        properties: alpha2_model::TextAnimatorProperties {
            fill: Some(alpha2_model::Animated::Constant { value: highlight }),
            scale: scale.map(|value| alpha2_model::Animated::Constant { value }),
            ..alpha2_model::TextAnimatorProperties::default()
        },
    }
}

pub(crate) use alpha2_model::clip_runs;

/// A built-in's full id from its short name; a name with a dot is already an id.
pub(crate) fn effect_id(name: &str) -> String {
    if name.contains('.') {
        name.to_owned()
    } else {
        format!("{}effect.{name}", alpha2_model::BUILTIN_EFFECT_PREFIX)
    }
}

/// `anim` with each value mapped by `map`, if every value maps.
fn map_anim<T, U>(anim: &Anim<T>, map: impl Fn(&T) -> Option<U>) -> Option<Anim<U>> {
    Some(match anim {
        Anim::Value(value) => Anim::Value(map(value)?),
        Anim::Ramp(ramp) => Anim::Ramp(Ramp {
            from: map(&ramp.from)?,
            to: map(&ramp.to)?,
            ease: ramp.ease.clone(),
            at: ramp.at,
            duration: ramp.duration,
        }),
        Anim::Keys(keys) => Anim::Keys(Keys {
            keys: keys
                .keys
                .iter()
                .map(|key| {
                    Some(Key {
                        t: key.t,
                        v: map(&key.v)?,
                        ease: key.ease.clone(),
                    })
                })
                .collect::<Option<Vec<_>>>()?,
        }),
    })
}

/// A parameter value from what a spec writes for it: a number, a list of numbers, a color as hex
/// or the color object, a boolean, or an enum choice by name or index. Without a definition the
/// value's JSON type decides.
#[allow(clippy::cast_possible_truncation)]
fn effect_value(
    param: Option<&alpha2_model::EffectParamSpec>,
    value: &serde_json::Value,
) -> Option<EffectValue> {
    use alpha2_model::EffectParamType as Type;
    let numbers = |count: usize| {
        let values = value.as_array()?;
        (values.len() == count)
            .then(|| {
                values
                    .iter()
                    .map(|value| value.as_f64().map(|value| value as f32))
            })?
            .collect::<Option<Vec<f32>>>()
    };
    let Some(param) = param else {
        return match value {
            serde_json::Value::Number(number) => Some(EffectValue::Float(number.as_f64()? as f32)),
            serde_json::Value::Bool(flag) => Some(EffectValue::Bool(*flag)),
            serde_json::Value::Array(values) if values.len() == 2 => {
                Some(EffectValue::Vec2(numbers(2)?.try_into().ok()?))
            }
            serde_json::Value::Array(values) if values.len() == 4 => {
                Some(EffectValue::Vec4(numbers(4)?.try_into().ok()?))
            }
            _ => None,
        };
    };
    Some(match param.param_type {
        Type::Float => EffectValue::Float(value.as_f64()? as f32),
        Type::Vec2 => EffectValue::Vec2(numbers(2)?.try_into().ok()?),
        Type::Vec4 => EffectValue::Vec4(numbers(4)?.try_into().ok()?),
        Type::Color => {
            let color: Rgba = serde_json::from_value(value.clone()).ok()?;
            EffectValue::Color([color.red, color.green, color.blue, color.alpha])
        }
        Type::Enum => EffectValue::Enum(match value {
            serde_json::Value::String(name) => {
                u32::try_from(param.enum_values.iter().position(|choice| choice == name)?).ok()?
            }
            other => u32::try_from(other.as_u64()?).ok()?,
        }),
        Type::Bool => EffectValue::Bool(value.as_bool()?),
    })
}
