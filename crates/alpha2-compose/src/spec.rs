//! The compose spec: a video described by intent, which [`crate::compile`] turns into a
//! project document.
//!
//! Times are seconds, rounded to the nearest tick (1/120 000 s). Lengths are canvas pixels.
//! Colors are CSS hex strings or the project's `{red, green, blue, alpha}` object. Nodes are
//! tagged by `type`, and a node with an `id` can be named by another (a music bed ducks under
//! it).

use std::collections::BTreeMap;

use alpha2_model::{
    Animated, Composition, Easing, EffectValue, LayoutNode, MediaSource, Rgba, TextShadow,
    TextStroke, Track,
};
use serde::{Deserialize, Serialize};

/// The newest compose spec version this build reads.
pub const COMPOSE_VERSION: u32 = 1;

/// A whole video: a body, what is drawn over it, and what plays under it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct VideoSpec {
    /// Spec version. This build reads only [`COMPOSE_VERSION`].
    #[serde(default = "compose_version")]
    #[cfg_attr(feature = "schema", schemars(extend("const" = COMPOSE_VERSION)))]
    pub compose_version: u32,
    /// The project's name. Default `video`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Size, rate and background. Default `youtube-1080p`.
    #[serde(default)]
    pub canvas: Canvas,
    /// Fonts, colors, sizes and default timings. Default `clean-dark`.
    #[serde(default)]
    pub theme: ThemeChoice,
    /// The main picture, from time zero. Its length is the video's unless `duration` says
    /// otherwise.
    pub body: Node,
    /// Nodes drawn over the body in order, placed by their own `at` (default 0) and lasting to
    /// the end unless they say otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlays: Vec<Node>,
    /// Sound beside the body's: `audio` and `music_bed` nodes, placed as overlays are.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio: Vec<Node>,
    /// The video's length in seconds. Default: the body's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// The loudness an export normalizes the mix to. Returned as an export option, not stored
    /// in the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loudness: Option<Loudness>,
}

const fn compose_version() -> u32 {
    COMPOSE_VERSION
}

/// The canvas: a preset name, or its parts.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum Canvas {
    /// A preset at 30 fps: `youtube-1080p` (1920x1080), `youtube-4k` (3840x2160), `youtube-720p`
    /// (1280x720), `shorts` (1080x1920), `square` (1080x1080) or `portrait` (1080x1350).
    Preset(String),
    /// A preset with some parts replaced, or every part given.
    Custom(CustomCanvas),
}

impl Default for Canvas {
    fn default() -> Self {
        Self::Preset("youtube-1080p".to_owned())
    }
}

/// A canvas by its parts.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CustomCanvas {
    /// The preset the other parts replace parts of. Default `youtube-1080p`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Width in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Height in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Frames per second: an integer, 23.976, 29.97 or 59.94, or the project's exact
    /// `{num, den}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<FrameRateChoice>,
    /// The opaque color under everything. Default: the theme's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Rgba>,
}

/// A frame rate, as a number or exactly.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum FrameRateChoice {
    /// Frames per second: an integer, or 23.976, 29.97 or 59.94 for the NTSC rates.
    Number(f64),
    /// The project's exact rate.
    Exact(alpha2_time::FrameRate),
}

/// A theme by name, or one built on a named theme.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum ThemeChoice {
    /// A built-in theme: `clean-dark` or `clean-light`.
    Named(String),
    /// A theme of its own.
    Custom(Box<Theme>),
}

impl Default for ThemeChoice {
    fn default() -> Self {
        Self::Named("clean-dark".to_owned())
    }
}

/// How text, panels and timings look. Parts left out come from the base theme. Sizes are
/// fractions of the canvas's shorter side, so a theme looks the same on wide and tall canvases.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Theme {
    /// The built-in theme this one changes. Default `clean-dark`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Font family, as the font names it. Unset, text draws in the first font an export
    /// registers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// Text color.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<Rgba>,
    /// The canvas background.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Rgba>,
    /// What title and end cards are drawn on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<Rgba>,
    /// The boxes behind labels, lower thirds and boxed captions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel: Option<Rgba>,
    /// A highlight: a lower third's bar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<Rgba>,
    /// Title size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_size: Option<f32>,
    /// Subtitle size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle_size: Option<f32>,
    /// Body text size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_size: Option<f32>,
    /// Caption size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption_size: Option<f32>,
    /// Label size: a grid cell's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_size: Option<f32>,
    /// The space kept clear at the canvas's edges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin: Option<f32>,
    /// The transition a slideshow, or a sequence asking for `theme`, puts between children.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition: Option<Transition>,
    /// How long a still shows when nothing says, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub still_seconds: Option<f64>,
    /// How long a card or text shows when nothing says, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_seconds: Option<f64>,
    /// How long text and cards fade in and out, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade_seconds: Option<f64>,
}

/// The loudness an export normalizes to: a target, or a target and a ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum Loudness {
    /// Integrated loudness in LUFS, under a -1 dBTP ceiling.
    Lufs(f64),
    /// Integrated loudness and a true-peak ceiling.
    Target(crate::LoudnessTarget),
}

/// The transition between two neighbours of a sequence.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Transition {
    /// `crossfade`, a built-in transition's short name (`wipe-left`, `circle-open`, ...), or a
    /// full effect id (`alpha2.transition.fade`).
    #[serde(rename = "type")]
    pub kind: String,
    /// How long the neighbours overlap, in seconds.
    pub duration: f64,
    /// The timing curve. Default linear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub easing: Option<Easing>,
    /// The effect's parameters by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, Animated<EffectValue>>,
    /// The effect definition's version. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
}

/// A sequence's transitions: none, the theme's, or one given.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum TransitionChoice {
    /// `none` for cuts, `theme` for the theme's transition, or a transition name at the theme's
    /// duration.
    Named(String),
    /// A transition.
    Given(Transition),
}

/// Space kept clear inside an area.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum Inset {
    /// The same on every side.
    Uniform(f32),
    /// `[top, right, bottom, left]`.
    Sides([f32; 4]),
}

/// How a picture fills its area.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    /// All of it, as large as fits; the rest of the area stays empty.
    #[default]
    Contain,
    /// All of the area, cropped to it.
    Cover,
    /// Stretched to the area.
    Fill,
    /// At its own size in pixels, cropped to the area.
    None,
}

/// A point of an area: its center, an edge's middle or a corner.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum Align {
    /// The center.
    #[default]
    Center,
    /// The top edge's middle.
    Top,
    /// The bottom edge's middle.
    Bottom,
    /// The left edge's middle.
    Left,
    /// The right edge's middle.
    Right,
    /// The top-left corner.
    TopLeft,
    /// The top-right corner.
    TopRight,
    /// The bottom-left corner.
    BottomLeft,
    /// The bottom-right corner.
    BottomRight,
}

/// What a media file holds, when its extension does not say.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// Moving pictures, perhaps with sound.
    Video,
    /// A still picture.
    Image,
    /// Sound alone.
    Audio,
}

/// One part of a video.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Node {
    /// Children one after another, with transitions between them.
    Sequence(Sequence),
    /// Children over each other, each over the ones before.
    Stack(Stack),
    /// Children in the cells of a grid, row by row.
    Grid(Grid),
    /// A video or still file.
    Media(Media),
    /// Text.
    Text(Text),
    /// An area painted one color.
    Color(ColorFill),
    /// Timed text from cues, a SubRip or WebVTT file, or word timestamps.
    Captions(Captions),
    /// A layout generator: boxes, text and shapes laid out by CSS flexbox and grid.
    Layout(Layout),
    /// A sound file.
    Audio(Audio),
    /// Part of a project (sources, tracks, compositions) at its own times. Overlays only.
    Raw(Raw),
    /// Stills or clips in turn, with transitions and slow zooms.
    Slideshow(Slideshow),
    /// A video under its captions.
    CaptionedVideo(CaptionedVideo),
    /// A title (and subtitle) over what is under it, laid out on the whole canvas.
    Title(Title),
    /// A title (and subtitle) on the theme's surface, over the whole canvas.
    TitleCard(Title),
    /// A name and role in the canvas's lower left.
    LowerThird(LowerThird),
    /// A closing card over the whole canvas: a logo, a title and a subtitle on the theme's
    /// surface.
    EndCard(EndCard),
    /// Clips side by side in a grid, each labeled.
    ComparisonGrid(ComparisonGrid),
    /// A video fitted to a canvas of another shape, over a blurred copy of itself.
    Reframe(Reframe),
    /// A picture in a corner.
    Logo(Logo),
    /// Music under the video, faded in and out and ducked under speech.
    MusicBed(MusicBed),
    /// A number counting from one value to another, as text.
    Counter(Counter),
}

/// Children one after another.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Sequence {
    /// The node's name, for other nodes to refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The children, in play order.
    pub children: Vec<Node>,
    /// What joins neighbours: a transition overlaps them by its duration.
    /// Default `none`. Only single pictures (media, text, color, layout)
    /// can transition into each other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition: Option<TransitionChoice>,
    /// Seconds from the parent's start (an overlay's: the video's). Ignored in a sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    /// Length in seconds. Default: the node's own length, else what its parent gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// How it comes in at its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enter: Option<Motion>,
    /// How it goes out, ending at its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Motion>,
}

/// Children over each other.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Stack {
    /// The node's name, for other nodes to refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The children, each drawn over the ones before and starting at its own `at`.
    pub children: Vec<Node>,
    /// Seconds from the parent's start (an overlay's: the video's). Ignored in a sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    /// Length in seconds. Default: the node's own length, else what its parent gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Pixels kept clear inside the parent's area: one number, or `[top, right, bottom, left]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inset: Option<Inset>,
    /// How it comes in at its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enter: Option<Motion>,
    /// How it goes out, ending at its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Motion>,
    /// Seconds between the children's entrances. With it, the node's `enter` and `exit` move
    /// each child rather than the whole, and child `i`'s entrance (its own `enter` too) starts
    /// `i * stagger` after the child does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stagger: Option<f64>,
}

/// Children in grid cells.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Grid {
    /// The node's name, for other nodes to refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The cells' contents, row by row.
    pub children: Vec<Node>,
    /// Columns. Default: as square a grid as the children make.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<u32>,
    /// Pixels between cells.
    #[serde(default)]
    pub gap: f32,
    /// How long the grid lasts when its cells differ, and what a cell that ends early shows.
    /// Default `shortest`.
    #[serde(default)]
    pub length: GridLength,
    /// Seconds from the parent's start (an overlay's: the video's). Ignored in a sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    /// Length in seconds. Default: the node's own length, else what its parent gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Pixels kept clear inside the parent's area: one number, or `[top, right, bottom, left]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inset: Option<Inset>,
    /// How it comes in at its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enter: Option<Motion>,
    /// How it goes out, ending at its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Motion>,
    /// Seconds between the children's entrances. With it, the node's `enter` and `exit` move
    /// each child rather than the whole, and child `i`'s entrance (its own `enter` too) starts
    /// `i * stagger` after the child does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stagger: Option<f64>,
}

/// How long a grid of clips of different lengths lasts.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GridLength {
    /// As long as its shortest cell.
    #[default]
    Shortest,
    /// As long as its longest; shorter clips hold their last frame.
    LongestFreeze,
    /// As long as its longest; shorter clips play again from their start.
    LongestLoop,
}

/// A video or still file.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Media {
    /// The node's name, for other nodes to refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The file. It becomes the source's locator and the export's asset.
    pub path: String,
    /// What the file holds. Default: by its extension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<FileKind>,
    /// Where in the file a video starts, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trim_start: Option<f64>,
    /// Where in the file a video ends, in seconds. Default: its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trim_end: Option<f64>,
    /// Playback speed of a video: 2 plays it twice as fast. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    /// Keeps a video's sound at its pitch when `speed` is not 1 (time-stretched instead of
    /// resampled). Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserve_pitch: Option<bool>,
    /// Plays a video again from its start while the node lasts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub r#loop: bool,
    /// Linear gain of a video's sound. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<f32>,
    /// Silences a video's sound.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub muted: bool,
    /// Opacity, 0..=1. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    /// Seconds of fade in from transparent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade_in: Option<f64>,
    /// Seconds of fade out to transparent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade_out: Option<f64>,
    /// A slow zoom over the node's length, `[from, to]`, as multiples of the fitted size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoom: Option<[f32; 2]>,
    /// A Gaussian blur's radius, as a fraction of the picture's shorter side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blur: Option<f32>,
    /// A label a comparison grid shows on the cell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Seconds from the parent's start (an overlay's: the video's). Ignored in a sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    /// Length in seconds. Default: the node's own length, else what its parent gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Pixels kept clear inside the parent's area: one number, or `[top, right, bottom, left]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inset: Option<Inset>,
    /// How a picture fills its area. Default `contain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<Fit>,
    /// Where in its area the picture or text lies. Default `center`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<Align>,
    /// How it comes in at its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enter: Option<Motion>,
    /// How it goes out, ending at its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Motion>,
}

impl Media {
    /// A node of the file at `path`, everything else by default.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            id: None,
            path: path.into(),
            kind: None,
            trim_start: None,
            trim_end: None,
            speed: None,
            preserve_pitch: None,
            r#loop: false,
            volume: None,
            muted: false,
            opacity: None,
            fade_in: None,
            fade_out: None,
            zoom: None,
            blur: None,
            label: None,
            at: None,
            duration: None,
            inset: None,
            fit: None,
            align: None,
            enter: None,
            exit: None,
        }
    }
}

/// A file given by its path alone, or a media node.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum MediaItem {
    /// The file, everything else by default.
    Path(String),
    /// A `media` node.
    Node(Box<Node>),
}

/// Text in one of the theme's styles.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Text {
    /// The node's name, for other nodes to refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The text; `\n` starts a line.
    pub text: String,
    /// The theme style it starts from. Default `body`.
    #[serde(default)]
    pub style: TextRole,
    /// Font size in pixels. Default: the style's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    /// Family. Default: the theme's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// Color. Default: the theme's text color.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Rgba>,
    /// A box of this color behind each line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Rgba>,
    /// Bands around the glyphs, innermost first, widths as fractions of the font size (as a
    /// text style's `strokes`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub strokes: Vec<TextStroke>,
    /// Shadows of the glyphs, the first on top, lengths as fractions of the font size (as a
    /// text style's `shadows`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shadows: Vec<TextShadow>,
    /// The most lines it may take. It first shrinks within `fit`; text still longer is cut after
    /// its last line, ended by `ellipsis`. Default: no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lines: Option<u32>,
    /// The font sizes in pixels it is fitted between, the largest at which it fits its area and
    /// `max_lines`. Default with `max_lines`: 0.3 times its font size up to its font size;
    /// otherwise its font size alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<TextFit>,
    /// What ends a line `max_lines` cuts: `true` an ellipsis, `false` nothing, or this text.
    /// Default `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ellipsis: Option<Ellipsis>,
    /// Opacity, 0..=1. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    /// Seconds of fade in and out. Default none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade: Option<f64>,
    /// Seconds from the parent's start (an overlay's: the video's). Ignored in a sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    /// Length in seconds. Default: the node's own length, else what its parent gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Pixels kept clear inside the parent's area: one number, or `[top, right, bottom, left]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inset: Option<Inset>,
    /// Where in its area the picture or text lies. Default `center`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<Align>,
    /// How it comes in at its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enter: Option<Motion>,
    /// How it goes out, ending at its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Motion>,
    /// Reveals the text a word or a line at a time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reveal: Option<Reveal>,
}

/// The font sizes a text is fitted between, in pixels.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TextFit {
    /// Smallest font size; when nothing larger fits, the text draws at this size.
    pub min: f32,
    /// Largest font size. Default: the text's font size, so it only shrinks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f32>,
}

/// What ends a cut line: an ellipsis or nothing (`true`, `false`), or a text.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum Ellipsis {
    /// An ellipsis, or nothing.
    Shown(bool),
    /// This text, 1 to 64 bytes without line breaks or control characters.
    Text(String),
}

/// A theme text style.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TextRole {
    /// The largest.
    Title,
    /// Under a title.
    Subtitle,
    /// Running text.
    #[default]
    Body,
    /// A cell's label.
    Label,
}

/// An area painted one color.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ColorFill {
    /// The node's name, for other nodes to refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The color. Default: the theme's surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Rgba>,
    /// Seconds from the parent's start (an overlay's: the video's). Ignored in a sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    /// Length in seconds. Default: the node's own length, else what its parent gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Pixels kept clear inside the parent's area: one number, or `[top, right, bottom, left]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inset: Option<Inset>,
    /// How it comes in at its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enter: Option<Motion>,
    /// How it goes out, ending at its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Motion>,
}

/// Timed text.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "CaptionsNode"))]
#[serde(deny_unknown_fields)]
pub struct Captions {
    /// The node's name, for other nodes to refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Cues as given, in seconds from the node's start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cues: Vec<Cue>,
    /// Words with their times, as speech recognition gives them, grouped into cues of up to
    /// `max_chars` characters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<Cue>,
    /// A SubRip (`.srt`) or WebVTT (`.vtt`) file, read by the binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// SubRip text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srt: Option<String>,
    /// WebVTT text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vtt: Option<String>,
    /// How the captions look. Default `outline`.
    #[serde(default)]
    pub style: CaptionStyle,
    /// The longest cue words are grouped into, in characters. Default 32 on a tall canvas, 42
    /// on a wide one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chars: Option<u32>,
    /// Font size in pixels. Default: the theme's caption size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    /// Text color. Default: the theme's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Rgba>,
    /// The color `karaoke` and `pop` give the word being said. Default: the theme's accent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlight: Option<Rgba>,
    /// Each speaker's look, keyed by a WebVTT voice or a cue's `speaker`; it overrides the
    /// style.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub speakers: BTreeMap<String, SpeakerLook>,
    /// Where the text's middle lies, as a fraction of the canvas height. Default depends on the
    /// style and the canvas's shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<f32>,
    /// Seconds from the parent's start (an overlay's: the video's). Ignored in a sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    /// Length in seconds. Default: the node's own length, else what its parent gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
}

/// Text over an interval.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Cue {
    /// Start in seconds.
    pub start: f64,
    /// End in seconds.
    pub end: f64,
    /// The text.
    pub text: String,
    /// Who says it: a key of the captions' `speakers` gives the cue its look.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
}

/// How one speaker's captions look.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SpeakerLook {
    /// Their text color.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Rgba>,
    /// Where their text's middle lies, as fractions of the canvas, `(0, 0)` top-left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<alpha2_model::Vec2>,
}

/// How captions look.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum CaptionStyle {
    /// Text with a dark band around the glyphs, near the bottom.
    #[default]
    Outline,
    /// Text on a panel behind each line, near the bottom.
    Boxed,
    /// Text centered on a translucent bar across the canvas near the bottom, shown as long as
    /// the captions.
    BarBottom,
    /// Larger outlined text a little below the middle, as short-form video draws it.
    BoldCenter,
    /// White text with a black stroke and a soft shadow, near the bottom.
    Bold,
    /// `bold`, each word turning the highlight color as it is said. Needs word times (`words`
    /// or WebVTT inline timestamps), else draws as `bold`.
    Karaoke,
    /// `bold`, the word being said growing a little in the highlight color. Needs word times.
    Pop,
}

/// A layout generator.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Layout {
    /// The node's name, for other nodes to refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The root box, as a project's layout generator writes it.
    pub root: Box<LayoutNode>,
    /// Seconds from the parent's start (an overlay's: the video's). Ignored in a sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    /// Length in seconds. Default: the node's own length, else what its parent gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Pixels kept clear inside the parent's area: one number, or `[top, right, bottom, left]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inset: Option<Inset>,
    /// Where in its area the picture or text lies. Default `center`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<Align>,
    /// How it comes in at its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enter: Option<Motion>,
    /// How it goes out, ending at its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Motion>,
}

/// A sound file.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Audio {
    /// The node's name, for other nodes to refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The file.
    pub path: String,
    /// Where in the file it starts, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trim_start: Option<f64>,
    /// Where in the file it ends, in seconds. Default: its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trim_end: Option<f64>,
    /// Plays the file again from its start while the node lasts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub r#loop: bool,
    /// Linear gain. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<f32>,
    /// Seconds of fade in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade_in: Option<f64>,
    /// Seconds of fade out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade_out: Option<f64>,
    /// The `id` of a node (or `body`) whose sound this ducks under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duck_under: Option<String>,
    /// How far ducking lowers it, in dB (negative). Default -12.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duck_depth_db: Option<f32>,
    /// Seconds from the parent's start (an overlay's: the video's). Ignored in a sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    /// Length in seconds. Default: the node's own length, else what its parent gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
}

/// Part of a project at its own times.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Raw {
    /// Sources its tracks read. Ids must not collide with the compiler's (`s1`, `s2`, ...); a
    /// `src-` prefix keeps them apart.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<MediaSource>,
    /// Tracks drawn over what comes before, at timeline times.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracks: Vec<Track>,
    /// Compositions its clips show.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compositions: Vec<Composition>,
    /// Each file a media source of the fragment reads, by source id: the export's assets.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub assets: BTreeMap<String, String>,
}

/// Stills or clips in turn.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Slideshow {
    /// The node's name, for other nodes to refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The slides.
    pub items: Vec<MediaItem>,
    /// How long each slide shows, transitions included, in seconds. Default: `fit_to`'s length
    /// shared out, else the theme's still length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f64>,
    /// A sound file whose length the slides share out. It is not played; add it as a music bed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit_to: Option<String>,
    /// What joins slides. Default `theme`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition: Option<TransitionChoice>,
    /// How far each slide zooms in over its time, as a multiple (1.15 ends 15% larger). Default
    /// none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ken_burns: Option<f32>,
    /// How slides fill the canvas. Default `cover`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<Fit>,
    /// When it starts in its parent; its length is the slides'.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
}

/// A video under its captions.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CaptionedVideo {
    /// The node's name, for other nodes to refer to. Its sound is the video's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The video.
    pub video: MediaItem,
    /// The captions, timed from the video's start.
    pub captions: Box<Captions>,
    /// How the video fills the canvas. Default `cover`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<Fit>,
    /// When it starts in its parent; its length is the video's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
}

/// A title and subtitle.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Title {
    /// The node's name, for other nodes to refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The title.
    pub text: String,
    /// A smaller line under it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Where it lies. Default `center`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<Align>,
    /// Seconds of fade in and out. Default: the theme's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade: Option<f64>,
    /// A card's color. Default: the theme's surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Rgba>,
    /// Seconds from the parent's start (an overlay's: the video's). Ignored in a sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    /// Length in seconds. Default: the node's own length, else what its parent gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// How it comes in at its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enter: Option<Motion>,
    /// How it goes out, ending at its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Motion>,
}

/// A name and role in the lower left.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct LowerThird {
    /// The node's name, for other nodes to refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The name.
    pub text: String,
    /// The role, under it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Seconds of fade in and out. Default: the theme's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade: Option<f64>,
    /// Seconds from the parent's start (an overlay's: the video's). Ignored in a sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    /// Length in seconds. Default: the node's own length, else what its parent gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// How it comes in at its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enter: Option<Motion>,
    /// How it goes out, ending at its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Motion>,
}

/// A closing card.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct EndCard {
    /// The node's name, for other nodes to refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The title.
    pub text: String,
    /// A smaller line under it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// A picture above the title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    /// The card's color. Default: the theme's surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Rgba>,
    /// Seconds of fade in and out. Default: the theme's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade: Option<f64>,
    /// Seconds from the parent's start (an overlay's: the video's). Ignored in a sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    /// Length in seconds. Default: the node's own length, else what its parent gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// How it comes in at its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enter: Option<Motion>,
    /// How it goes out, ending at its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Motion>,
}

/// Clips in a labeled grid.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ComparisonGrid {
    /// The node's name, for other nodes to refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The clips, row by row; each one's `label` names its cell.
    pub items: Vec<MediaItem>,
    /// Columns. Default: as square a grid as the items make.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<u32>,
    /// Pixels between cells.
    #[serde(default)]
    pub gap: f32,
    /// How long the grid lasts when its clips differ. Default `shortest`.
    #[serde(default)]
    pub length: GridLength,
    /// Which item's sound plays, by index. Default: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_from: Option<u32>,
    /// Seconds from the parent's start (an overlay's: the video's). Ignored in a sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    /// Length in seconds. Default: the node's own length, else what its parent gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
}

/// A video over a blurred copy of itself.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Reframe {
    /// The node's name, for other nodes to refer to. Its sound is the video's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The video.
    pub video: MediaItem,
    /// The background's blur radius, as a fraction of its shorter side. Default 0.05.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blur: Option<f32>,
    /// When it starts in its parent; its length is the video's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
}

/// A picture in a corner.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Logo {
    /// The node's name, for other nodes to refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The picture.
    pub path: String,
    /// The corner. Default `top-right`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corner: Option<Align>,
    /// Its longer side, in pixels. Default: 15% of the canvas's shorter side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<f32>,
    /// Pixels from the edges. Default: the theme's margin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin: Option<f32>,
    /// Opacity, 0..=1. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    /// Seconds from the parent's start (an overlay's: the video's). Ignored in a sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    /// Length in seconds. Default: the node's own length, else what its parent gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// How it comes in at its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enter: Option<Motion>,
    /// How it goes out, ending at its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Motion>,
}

/// Music under the video.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MusicBed {
    /// The node's name, for other nodes to refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The file.
    pub path: String,
    /// Linear gain. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<f32>,
    /// Seconds of fade in. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade_in: Option<f64>,
    /// Seconds of fade out at the end. Default 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade_out: Option<f64>,
    /// The `id` of a node (or `body`) whose sound it ducks under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duck_under: Option<String>,
    /// How far ducking lowers it, in dB (negative). Default -12.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duck_depth_db: Option<f32>,
    /// Plays the file again from its start while the video lasts. Default true.
    #[serde(default = "yes")]
    pub r#loop: bool,
    /// Seconds from the parent's start (an overlay's: the video's). Ignored in a sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    /// Length in seconds. Default: the node's own length, else what its parent gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
}

const fn yes() -> bool {
    true
}

/// A timing curve: `linear`, a CSS name (`ease`, `ease-in`, `ease-out`, `ease-in-out`) or a
/// project easing object.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum EasingChoice {
    /// `linear`, `ease`, `ease-in`, `ease-out` or `ease-in-out`, with the CSS curves.
    Named(String),
    /// A project easing.
    Curve(Easing),
}

/// An entrance or exit, timed from the node's own start or back from its own end.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Motion {
    /// What moves: `fade` (opacity alone), `rise` (a slide up), `slide` (toward `direction`) or
    /// `scale` (from `size` times its size). `rise`, `slide` and `scale` fade too unless `fade`
    /// is false.
    #[serde(rename = "type")]
    pub kind: MotionKind,
    /// Length in seconds. Default: the theme's fade length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// The timing curve. Default linear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub easing: Option<EasingChoice>,
    /// Which way a `slide` travels, entering into place and leaving out of it. Default `up`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<Direction>,
    /// How far a `rise` or `slide` travels, in pixels. Default: the theme's margin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distance: Option<f32>,
    /// The size a `scale` enters from or exits to, as a multiple of its own. Default 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<f32>,
    /// Whether a `rise`, `slide` or `scale` also fades. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade: Option<bool>,
}

/// What an entrance or exit moves.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MotionKind {
    /// Opacity alone.
    Fade,
    /// A slide up.
    Rise,
    /// A slide toward `direction`.
    Slide,
    /// A change of size about the node's own anchor.
    Scale,
}

/// A direction of travel on the canvas.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Toward the top.
    #[default]
    Up,
    /// Toward the bottom.
    Down,
    /// Toward the left.
    Left,
    /// Toward the right.
    Right,
}

/// A text revealed a unit at a time. Each unit moves at a constant rate over `duration`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Reveal {
    /// The unit: `word` (words holding a letter or digit) or `line` (laid-out lines).
    pub by: RevealUnit,
    /// How each unit appears: `fade`, or `rise` (fading in while moving up). Default `fade`.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RevealKind>,
    /// Seconds between one unit's start and the next's. Default half the duration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stagger: Option<f64>,
    /// Seconds each unit takes. Default: the theme's fade length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// How far a `rise` moves, in pixels. Default half the font size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distance: Option<f32>,
}

/// The unit a text is revealed by.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum RevealUnit {
    /// Words that hold a letter or digit; other characters show throughout.
    Word,
    /// Laid-out lines.
    Line,
}

/// How a revealed unit appears.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum RevealKind {
    /// Opacity alone.
    Fade,
    /// Fading in while moving up.
    Rise,
}

/// A number counting from `start` to `end`, shown as text that changes on the frame grid.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Counter {
    /// The node's name, for other nodes to refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The value it counts from.
    pub start: f64,
    /// The value it counts to.
    pub end: f64,
    /// Seconds the count takes from the node's start; it then holds `end`. Default: the node's
    /// whole length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count_duration: Option<f64>,
    /// The timing curve of the count. Default linear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub easing: Option<EasingChoice>,
    /// How the number is written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<NumberFormat>,
    /// The theme style it starts from. Default `title`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<TextRole>,
    /// Font size in pixels. Default: the style's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    /// Family. Default: the theme's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// Color. Default: the theme's text color.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Rgba>,
    /// Bands around the glyphs, as a text node's `strokes`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub strokes: Vec<TextStroke>,
    /// Shadows of the glyphs, as a text node's `shadows`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shadows: Vec<TextShadow>,
    /// Opacity, 0..=1. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    /// Seconds from the parent's start (an overlay's: the video's). Ignored in a sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    /// Length in seconds. Default: the node's own length, else what its parent gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Pixels kept clear inside the parent's area: one number, or `[top, right, bottom, left]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inset: Option<Inset>,
    /// Where in its area the number lies. Default `center`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<Align>,
    /// How it comes in at its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enter: Option<Motion>,
    /// How it goes out, ending at its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Motion>,
}

/// How a counter writes its number.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct NumberFormat {
    /// Digits after the decimal point, at most 6. Default 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u32>,
    /// Put between each group of three integer digits, as `,`. Default none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thousands: Option<String>,
    /// The decimal point. Default `.`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point: Option<String>,
    /// Text before the number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Text after the number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
}
