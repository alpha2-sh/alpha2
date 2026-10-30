//! Lowering: every spec node, components included, as the few primitives
//! the emitter places ([`Prim`]), with the theme's defaults filled in, times
//! in ticks, and caption files parsed.

use alpha2_model::{CaptionCue, LayoutNode, Paint, Rgba, TextShadow, TextStroke, TextStyle};

use crate::ComposeError;
use crate::compile::Context;
use crate::spec::{
    Align, Audio, CaptionStyle, Captions, ComparisonGrid, Cue, EndCard, FileKind, Fit, GridLength,
    Inset, LowerThird, Media, MediaItem, MusicBed, Node, Raw, Slideshow, Text, TextRole, Title,
    Transition, TransitionChoice,
};

/// A node, ready to place.
#[derive(Clone, Debug)]
pub(crate) enum Prim {
    Sequence(SequencePrim),
    Stack(StackPrim),
    Visual(Box<Visual>),
    Captions(Box<CaptionsPrim>),
    Audio(Box<AudioPrim>),
    Raw(Head, Box<Raw>),
}

/// What every primitive has: where errors point, its name, and its timing.
#[derive(Clone, Debug, Default)]
pub(crate) struct Head {
    pub path: String,
    pub id: Option<String>,
    pub at: Option<i64>,
    pub duration: Option<i64>,
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

#[derive(Clone, Copy, Debug)]
pub(crate) struct GridLayout {
    pub columns: u32,
    pub gap: f32,
    pub length: GridLength,
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
    pub zoom: Option<[f32; 2]>,
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
    Layout(Box<LayoutNode>),
}

#[derive(Clone, Debug)]
pub(crate) struct VideoContent {
    pub file: String,
    pub size: (u32, u32),
    pub trim_start: i64,
    pub trim_end: i64,
    /// Source ticks per timeline tick.
    pub speed: f64,
    pub looped: bool,
    pub volume: f32,
    pub muted: bool,
    pub has_audio: bool,
    pub blur: Option<f32>,
    /// One frame of the file, for a hold on its last.
    pub frame_ticks: i64,
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
}

#[derive(Clone, Debug)]
pub(crate) struct CaptionsPrim {
    pub head: Head,
    /// Cues from the node's start, in start order; they may overlap.
    pub cues: Vec<CaptionCue>,
    pub style: TextStyle,
    /// A bar behind the text: its center and height as fractions of the
    /// canvas height, and its color.
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
            Self::Visual(visual) => &mut visual.head,
            Self::Captions(captions) => &mut captions.head,
            Self::Audio(audio) => &mut audio.head,
            Self::Raw(head, _) => head,
        }
    }
}

/// `ticks` times `factor`, rounded.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub(crate) fn ticks_scaled(ticks: i64, factor: f64) -> i64 {
    (ticks as f64 * factor).round() as i64
}

/// Seconds as the nearest tick.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub(crate) fn seconds_to_ticks(seconds: f64) -> i64 {
    (seconds * alpha2_time::TICKS_PER_SECOND as f64).round() as i64
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

    /// `node` at `path`, lowered.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn lower(&mut self, node: &Node, path: &str) -> Result<Prim, ComposeError> {
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
                content: Content::Color(color.color.unwrap_or(self.theme.surface)),
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
            Node::Title(title) => self.lower_title(title, path, false),
            Node::TitleCard(title) => self.lower_title(title, path, true),
            Node::LowerThird(lower) => self.lower_lower_third(lower, path),
            Node::EndCard(card) => self.lower_end_card(card, path),
            Node::ComparisonGrid(grid) => self.lower_comparison_grid(grid, path),
            Node::Reframe(reframe) => {
                let item = format!("{path}.video");
                let video = Self::item_media(&reframe.video, &item)?;
                let blur = Self::unit(path, "blur", reframe.blur, 0.05)?;
                let mut background = video.clone();
                background.fit = Some(Fit::Cover);
                background.blur = Some(blur);
                background.muted = true;
                let mut front = video;
                front.fit = Some(Fit::Contain);
                Ok(Prim::Stack(StackPrim {
                    head: Self::head(path, reframe.id.as_ref(), reframe.at, None)?,
                    children: vec![
                        self.lower_media(&background, &item)?,
                        self.lower_media(&front, &item)?,
                    ],
                    inset: [0.0; 4],
                    grid: None,
                }))
            }
            Node::Logo(logo) => {
                let (width, height) = self.canvas_px();
                let size = match logo.size {
                    Some(size) => Self::pixels(path, "size", size)?,
                    None => 0.15 * self.short_side(),
                };
                let margin = match logo.margin {
                    Some(margin) => Self::pixels(path, "margin", margin)?,
                    None => self.theme_px(self.theme.margin),
                };
                let corner = logo.corner.unwrap_or(Align::TopRight);
                let (x, y) = align_point(corner);
                // The square box of `size` at the corner, `margin` from its edges.
                let left = margin + x * (width - size - 2.0 * margin);
                let top = margin + y * (height - size - 2.0 * margin);
                let mut media = Media::new(logo.path.clone());
                media.id.clone_from(&logo.id);
                media.at = logo.at;
                media.duration = logo.duration;
                media.opacity = logo.opacity;
                media.fit = Some(Fit::Contain);
                media.align = Some(corner);
                media.inset = Some(Inset::Sides([
                    top,
                    width - left - size,
                    height - top - size,
                    left,
                ]));
                self.lower_media(&media, path)
            }
            Node::MusicBed(music) => self.lower_music_bed(music, path),
        }
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

    /// The grid's columns: as given, or the fewest that make it no taller
    /// than wide.
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
                    looped: media.r#loop,
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
        if let Some(zoom) = media.zoom
            && !zoom.iter().all(|value| value.is_finite() && *value > 0.0)
        {
            return Err(Self::error(
                &format!("{path}.zoom"),
                "zoom factors are positive",
            ));
        }
        Ok(Prim::Visual(Box::new(Visual {
            head,
            inset: inset_sides(media.inset),
            fit: media.fit.unwrap_or_default(),
            align: media.align.unwrap_or_default(),
            opacity: Self::unit(path, "opacity", media.opacity, 1.0)?,
            fade_in: Self::optional_time(path, "fade_in", media.fade_in)?.unwrap_or(0),
            fade_out: Self::optional_time(path, "fade_out", media.fade_out)?.unwrap_or(0),
            zoom: media.zoom,
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

    fn role_size(&self, role: TextRole) -> f32 {
        let fraction = match role {
            TextRole::Title => self.theme.title_size,
            TextRole::Subtitle => self.theme.subtitle_size,
            TextRole::Body => self.theme.body_size,
            TextRole::Label => self.theme.label_size,
        };
        self.theme_px(fraction)
    }

    fn lower_text(&mut self, text: &Text, path: &str) -> Result<Prim, ComposeError> {
        if text.text.is_empty() {
            return Err(Self::error(&format!("{path}.text"), "text is not empty"));
        }
        let font_px = match text.font_size {
            Some(size) if size.is_finite() && size > 0.0 => size,
            Some(size) => {
                return Err(Self::error(
                    &format!("{path}.font_size"),
                    format!("{size} is not a font size in pixels"),
                ));
            }
            None => self.role_size(text.style),
        };
        let fade = Self::optional_time(path, "fade", text.fade)?.unwrap_or(0);
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
                color: text.color.unwrap_or(self.theme.text),
                background: text.background,
                strokes: text.strokes.clone(),
                shadows: text.shadows.clone(),
                max_lines: text.max_lines,
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
        let mut font_px = match captions.font_size {
            Some(size) if size.is_finite() && size > 0.0 => size,
            Some(size) => {
                return Err(Self::error(
                    &format!("{path}.font_size"),
                    format!("{size} is not a font size in pixels"),
                ));
            }
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
                color: captions.color.unwrap_or(self.theme.text),
            },
            max_width: if vertical { 0.86 } else { 0.8 },
            anchor: alpha2_model::Vec2::new(0.5, 0.5),
            ..TextStyle::default()
        };
        let mut bar = None;
        let low = if vertical { 0.76 } else { 0.88 };
        let y = match captions.style {
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
        };
        style.font_size = (font_px / self.short_side()).min(alpha2_model::MAX_CAPTION_FONT_SIZE);
        style.position = alpha2_model::Vec2::new(0.5, y);
        let _ = width;
        Ok(Prim::Captions(Box::new(CaptionsPrim {
            head,
            cues,
            style,
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
            out.push(crate::compile::cue(start, end, cue.text.clone()));
        }
        out.sort_by_key(|cue| cue.timeline_range.start());
        Ok(out)
    }

    /// Words grouped into cues: a cue ends at `max_chars` characters, at a
    /// sentence's end, or before a pause.
    fn group_words(
        words: &[Cue],
        max_chars: u32,
        path: &str,
    ) -> Result<Vec<CaptionCue>, ComposeError> {
        let pause = seconds_to_ticks(0.6);
        let words = Self::cues(words, path)?;
        let mut cues: Vec<CaptionCue> = Vec::new();
        let mut current: Option<(i64, i64, String)> = None;
        for word in words {
            let (start, end) = (
                word.timeline_range.start().ticks(),
                word.timeline_range.end().ticks(),
            );
            let text = word.text.trim().to_owned();
            if let Some((cue_start, cue_end, cue_text)) = &mut current {
                let length = cue_text.chars().count() + 1 + text.chars().count();
                let sentence_end = cue_text.ends_with(['.', '?', '!', '\u{3002}']);
                if length <= max_chars as usize && start - *cue_end <= pause && !sentence_end {
                    cue_text.push(' ');
                    cue_text.push_str(&text);
                    *cue_end = (*cue_end).max(end);
                    continue;
                }
                cues.push(crate::compile::cue(
                    *cue_start,
                    *cue_end,
                    std::mem::take(cue_text),
                ));
            }
            current = Some((start, end, text));
        }
        if let Some((start, end, text)) = current {
            cues.push(crate::compile::cue(start, end, text));
        }
        // Words that overlap make cues that would: each ends where the next starts.
        for index in 1..cues.len() {
            let next = cues[index].timeline_range.start();
            let previous = &mut cues[index - 1];
            if previous.timeline_range.end() > next && previous.timeline_range.start() < next {
                previous.timeline_range =
                    alpha2_time::TimeRange::new(previous.timeline_range.start(), next)
                        .unwrap_or(previous.timeline_range);
            }
        }
        Ok(cues)
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
            if let (None, Some(zoom)) = (media.zoom, slideshow.ken_burns) {
                media.zoom = Some([1.0, zoom]);
            }
            children.push(self.lower_media(&media, &at)?);
        }
        Ok(Prim::Sequence(SequencePrim {
            head: Self::head(path, slideshow.id.as_ref(), slideshow.at, None)?,
            children,
            transition,
        }))
    }

    /// A text primitive at `inset` in the canvas, aligned to `align`.
    #[allow(clippy::too_many_arguments)]
    fn text_prim(
        &self,
        path: &str,
        text: &str,
        font_px: f32,
        inset: [f32; 4],
        align: Align,
        fade: i64,
        max_lines: Option<u32>,
        background: Option<Rgba>,
    ) -> Prim {
        Prim::Visual(Box::new(Visual {
            head: Head {
                path: path.to_owned(),
                ..Head::default()
            },
            inset,
            fit: Fit::None,
            align,
            opacity: 1.0,
            fade_in: fade,
            fade_out: fade,
            zoom: None,
            content: Content::Text(TextContent {
                text: text.to_owned(),
                font_px,
                family: self.theme.font_family.clone(),
                color: self.theme.text,
                background,
                strokes: Vec::new(),
                shadows: Vec::new(),
                max_lines,
            }),
        }))
    }

    /// A title and an optional subtitle, as one group at `align` on the
    /// canvas: its children, and its top.
    fn title_group(
        &mut self,
        path: &str,
        text: &str,
        subtitle: Option<&str>,
        align: Align,
        fade: i64,
        top_offset: f32,
    ) -> Vec<Prim> {
        let (width, height) = self.canvas_px();
        let margin = self.theme_px(self.theme.margin) * 2.0;
        let (title_px, sub_px) = (
            self.role_size(TextRole::Title),
            self.role_size(TextRole::Subtitle),
        );
        let max_width = width - 2.0 * margin;
        let title_px = self.fit_font(path, text, title_px, max_width, 2);
        let title_height = self.text_height(text, title_px, max_width);
        let gap = 0.5 * sub_px;
        let sub_height = subtitle.map_or(0.0, |subtitle| {
            gap + self.text_height(subtitle, sub_px, max_width)
        });
        let group = title_height + sub_height;
        let (x, y) = align_point(align);
        let top = top_offset + margin + y * (height - top_offset - 2.0 * margin - group);
        let horizontal = match x {
            x if x < 0.25 => Align::TopLeft,
            x if x > 0.75 => Align::TopRight,
            _ => Align::Top,
        };
        let inset = |top: f32, height_used: f32| [top, margin, height - top - height_used, margin];
        let mut children = vec![self.text_prim(
            &format!("{path}.text"),
            text,
            title_px,
            inset(top, title_height),
            horizontal,
            fade,
            Some(2),
            None,
        )];
        if let Some(subtitle) = subtitle {
            let sub_top = top + title_height + gap;
            children.push(self.text_prim(
                &format!("{path}.subtitle"),
                subtitle,
                sub_px,
                inset(sub_top, sub_height - gap),
                horizontal,
                fade,
                None,
                None,
            ));
        }
        children
    }

    fn lower_title(&mut self, title: &Title, path: &str, card: bool) -> Result<Prim, ComposeError> {
        if title.text.is_empty() {
            return Err(Self::error(&format!("{path}.text"), "a title is not empty"));
        }
        let head = Self::head(path, title.id.as_ref(), title.at, title.duration)?;
        let fade = Self::time(path, "fade", title.fade.unwrap_or(self.theme.fade_seconds))?;
        let mut children = Vec::new();
        if card {
            children.push(Prim::Visual(Box::new(Visual {
                head: Head {
                    path: path.to_owned(),
                    ..Head::default()
                },
                inset: [0.0; 4],
                fit: Fit::Fill,
                align: Align::Center,
                opacity: 1.0,
                fade_in: 0,
                fade_out: 0,
                zoom: None,
                content: Content::Color(title.background.unwrap_or(self.theme.surface)),
            })));
        }
        let text_fade = if card { 0 } else { fade };
        children.extend(self.title_group(
            path,
            &title.text,
            title.subtitle.as_deref(),
            title.align.unwrap_or_default(),
            text_fade,
            0.0,
        ));
        let mut stack = StackPrim {
            head,
            children,
            inset: [0.0; 4],
            grid: None,
        };
        if card && fade > 0 {
            // A card fades as a whole: each of its pictures by the same ramp.
            for child in &mut stack.children {
                if let Prim::Visual(visual) = child {
                    visual.fade_in = fade;
                    visual.fade_out = fade;
                }
            }
        }
        Ok(Prim::Stack(stack))
    }

    fn lower_lower_third(&mut self, lower: &LowerThird, path: &str) -> Result<Prim, ComposeError> {
        if lower.text.is_empty() {
            return Err(Self::error(
                &format!("{path}.text"),
                "a lower third has a name",
            ));
        }
        let head = Self::head(path, lower.id.as_ref(), lower.at, lower.duration)?;
        let fade = Self::time(path, "fade", lower.fade.unwrap_or(self.theme.fade_seconds))?;
        let (width, height) = self.canvas_px();
        let margin = self.theme_px(self.theme.margin) * 1.5;
        let name_px = self.theme_px(self.theme.subtitle_size) * 1.2;
        let role_px = self.theme_px(self.theme.body_size);
        let bar_width = (0.008 * self.short_side()).max(2.0);
        let left = margin + bar_width * 2.5;
        let max_width = width * 0.6;
        let name_px = self.fit_font(&format!("{path}.text"), &lower.text, name_px, max_width, 1);
        let name_height = self.text_height(&lower.text, name_px, max_width);
        let gap = 0.6 * role_px;
        let role_height = lower
            .subtitle
            .as_deref()
            .map_or(0.0, |role| gap + self.text_height(role, role_px, max_width));
        let bottom = height - margin * 1.5;
        let top = bottom - name_height - role_height;
        let inset =
            |top: f32, used: f32| [top, width - left - max_width, height - top - used, left];
        let mut children = vec![
            Prim::Visual(Box::new(Visual {
                head: Head {
                    path: path.to_owned(),
                    ..Head::default()
                },
                inset: [
                    top - 0.2 * name_px,
                    width - margin - bar_width,
                    height - bottom - 0.2 * name_px,
                    margin,
                ],
                fit: Fit::Fill,
                align: Align::Center,
                opacity: 1.0,
                fade_in: fade,
                fade_out: fade,
                zoom: None,
                content: Content::Color(self.theme.accent),
            })),
            self.text_prim(
                &format!("{path}.text"),
                &lower.text,
                name_px,
                inset(top, name_height),
                Align::TopLeft,
                fade,
                Some(1),
                Some(self.theme.panel),
            ),
        ];
        if let Some(role) = &lower.subtitle {
            let role_top = top + name_height + gap;
            children.push(self.text_prim(
                &format!("{path}.subtitle"),
                role,
                role_px,
                inset(role_top, role_height - gap),
                Align::TopLeft,
                fade,
                None,
                Some(self.theme.panel),
            ));
        }
        Ok(Prim::Stack(StackPrim {
            head,
            children,
            inset: [0.0; 4],
            grid: None,
        }))
    }

    fn lower_end_card(&mut self, card: &EndCard, path: &str) -> Result<Prim, ComposeError> {
        if card.text.is_empty() {
            return Err(Self::error(
                &format!("{path}.text"),
                "an end card has a title",
            ));
        }
        let head = Self::head(path, card.id.as_ref(), card.at, card.duration)?;
        let fade = Self::time(path, "fade", card.fade.unwrap_or(self.theme.fade_seconds))?;
        let (width, height) = self.canvas_px();
        let mut children = vec![Prim::Visual(Box::new(Visual {
            head: Head {
                path: path.to_owned(),
                ..Head::default()
            },
            inset: [0.0; 4],
            fit: Fit::Fill,
            align: Align::Center,
            opacity: 1.0,
            fade_in: fade,
            fade_out: fade,
            zoom: None,
            content: Content::Color(card.background.unwrap_or(self.theme.surface)),
        }))];
        let mut top_offset = 0.0;
        if let Some(logo) = &card.logo {
            let size = 0.22 * self.short_side();
            let top = 0.5 * height - size - 0.04 * self.short_side();
            let mut media = Media::new(logo.clone());
            media.fit = Some(Fit::Contain);
            media.align = Some(Align::Bottom);
            media.inset = Some(Inset::Sides([
                top,
                0.5 * (width - size),
                height - top - size,
                0.5 * (width - size),
            ]));
            media.fade_in = Some(crate::compile::seconds(fade));
            media.fade_out = Some(crate::compile::seconds(fade));
            children.push(self.lower_media(&media, &format!("{path}.logo"))?);
            top_offset = 0.5 * height;
        }
        let align = if card.logo.is_some() {
            Align::Top
        } else {
            Align::Center
        };
        children.extend(self.title_group(
            path,
            &card.text,
            card.subtitle.as_deref(),
            align,
            fade,
            top_offset,
        ));
        Ok(Prim::Stack(StackPrim {
            head,
            children,
            inset: [0.0; 4],
            grid: None,
        }))
    }

    fn lower_comparison_grid(
        &mut self,
        grid: &ComparisonGrid,
        path: &str,
    ) -> Result<Prim, ComposeError> {
        if grid.items.is_empty() {
            return Err(Self::error(
                &format!("{path}.items"),
                "a comparison grid has at least one item",
            ));
        }
        if let Some(index) = grid.audio_from
            && index as usize >= grid.items.len()
        {
            return Err(Self::error(
                &format!("{path}.audio_from"),
                format!("there is no item {index}"),
            ));
        }
        let label_px = self.role_size(TextRole::Label);
        let pad = 0.8 * label_px;
        let mut cells = Vec::with_capacity(grid.items.len());
        for (index, item) in grid.items.iter().enumerate() {
            let at = format!("{path}.items[{index}]");
            let mut media = Self::item_media(item, &at)?;
            media.muted |= grid.audio_from != Some(u32::try_from(index).unwrap_or(u32::MAX));
            let label = media.label.take();
            let mut children = vec![self.lower_media(&media, &at)?];
            if let Some(label) = label.filter(|label| !label.is_empty()) {
                let mut text = self.text_prim(
                    &format!("{at}.label"),
                    &label,
                    label_px,
                    [pad; 4],
                    Align::TopLeft,
                    0,
                    Some(1),
                    Some(self.theme.panel),
                );
                if let Prim::Visual(visual) = &mut text
                    && let Content::Text(content) = &mut visual.content
                {
                    content.max_lines = None;
                }
                children.push(text);
            }
            cells.push(Prim::Stack(StackPrim {
                head: Head {
                    path: at,
                    ..Head::default()
                },
                children,
                inset: [0.0; 4],
                grid: None,
            }));
        }
        Ok(Prim::Stack(StackPrim {
            head: Self::head(path, grid.id.as_ref(), grid.at, grid.duration)?,
            inset: [0.0; 4],
            grid: Some(GridLayout {
                columns: Self::columns(path, grid.columns, cells.len())?,
                gap: Self::pixels(path, "gap", grid.gap)?,
                length: grid.length,
            }),
            children: cells,
        }))
    }
}
