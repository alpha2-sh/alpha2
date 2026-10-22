//! Text rendering: caption tracks' cues and text generators, rasterized by
//! the portable text host.
//!
//! The engine draws text itself, from font bytes its host registers once,
//! so a browser preview and a native export of one project, given the same
//! fonts, draw the same text pixels. A host keeps one [`TextTextures`]
//! beside its frame provider, calls [`TextTextures::prepare`] for each frame
//! before rendering it, and answers [`crate::FrameProvider::text`] from it.
//! A caption is composited as a layer between tracks; a text generator's
//! texture is placed on the transparent picture the generator draws.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use alpha2_eval::{FrameDesc, StepOp};
use alpha2_media::EmojiPolicy;
use alpha2_model::{Generator, Rgba, TextAlign, TextStyle};
use alpha2_render::{RenderCacheKey, RenderError, RenderSize, Renderer, SourceView, UploadTexture};
use alpha2_text::{TextFont, TextRasterError, TextRasterRequest, TextRasterizer};

/// The high 64 bits of a text's result cache key, before its font set is
/// mixed in: font sets are counted from 1 and stay far below this, so those
/// bits are never zero, as a host's source keys ([`RenderCacheKey::source`])
/// leave them, and the two never meet.
const TEXT_CACHE_DOMAIN: u64 = 0x6361_7074_696f_6e73;

/// The next font set's identity: each store, and each font a store adds,
/// takes one, so no two font sets that might draw text differently in one
/// process share a text cache key.
static NEXT_FONT_SET: AtomicU64 = AtomicU64::new(1);

/// How many rasterized texts a host keeps between frames; the current
/// frame's are kept however many it shows.
const RETAINED_TEXTS: usize = 16;

/// The largest font, outline, and padding the text host draws, in pixels.
const MAX_FONT_PIXELS: f32 = 2_048.0;
const MAX_OUTLINE_PIXELS: f32 = 64.0;
const MAX_PADDING_PIXELS: f32 = 256.0;

/// One prepared text: its texture, pixel size, and where its layout box
/// lies in it.
#[derive(Clone, Copy, Debug)]
pub struct TextTexture<'a> {
    /// Premultiplied, sRGB-encoded RGBA, as every source texture.
    pub texture: SourceView<'a>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The margin, in pixels, the same on both sides of each axis, that the
    /// texture adds around the text and its decorations for ink reaching
    /// past them: an accent above the first line, say. Placement leaves it
    /// out, so such ink never moves the text.
    pub margin: [u32; 2],
    /// The key the renderer's result cache holds results drawn from this
    /// texture under, or `None` to keep every frame showing it out of that
    /// cache. It is the provider's promise that the texture's pixels depend
    /// on nothing it leaves out, as [`crate::FrameProvider::source_key`] is
    /// for a source: [`TextTextures`] keys each text by its [`text_key`] and
    /// the fonts it was drawn with.
    pub cache_key: Option<RenderCacheKey>,
}

/// Where a text lies, as fractions of the canvas, `(0, 0)` top-left to
/// `(1, 1)` bottom-right, as its style's position is: on the picture a text
/// generator draws, before its clip's transform and effects, and on the
/// canvas for a caption. [`measure_text`] measures it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TextBounds {
    /// The box the style's anchor and position place: the wrapping width
    /// wide, every line's em box tall, and the whole pixels of outline and
    /// background padding around that.
    pub layout: CanvasRect,
    /// The lines' advances: from the start of the line that starts farthest
    /// left to the end of the one that ends farthest right, as tall as the
    /// layout, with the same decorations around it. It moves only as the
    /// advances do, not as glyphs with more or less ink are typed.
    pub content: CanvasRect,
    /// What the text draws: its glyphs, their outline, and the background's
    /// boxes, as [`alpha2_text::TextMeasure::ink`] bounds them at the measured
    /// size. Every pixel the text gives any alpha there meets it, give or
    /// take the pixel sampling the texture spreads it by. `None` when the text
    /// draws nothing.
    pub ink: Option<CanvasRect>,
    /// Number of laid-out lines.
    pub line_count: u32,
}

/// A rectangle as fractions of the canvas, y down.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct CanvasRect {
    /// Left edge, a fraction of the canvas width.
    pub left: f32,
    /// Top edge, a fraction of the canvas height.
    pub top: f32,
    /// Right edge, a fraction of the canvas width.
    pub right: f32,
    /// Bottom edge, a fraction of the canvas height.
    pub bottom: f32,
}

/// Why text could not be prepared.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TextError {
    /// The text host refused: no fonts registered, a malformed font, or a
    /// text too large to rasterize.
    Raster(TextRasterError),
    /// The rasterized text could not be uploaded.
    Render(RenderError),
}

impl fmt::Display for TextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Raster(TextRasterError::NoFonts) => {
                formatter.write_str("the project shows text, but no fonts were registered")
            }
            Self::Raster(error) => write!(formatter, "text failed: {error}"),
            Self::Render(error) => write!(formatter, "text upload failed: {error}"),
        }
    }
}

impl std::error::Error for TextError {}

struct TextEntry {
    texture: UploadTexture,
    width: u32,
    height: u32,
    margin: [u32; 2],
    last_used: u64,
}

/// Fonts and the texts rasterized with them, keyed by content and render
/// size.
pub struct TextTextures {
    rasterizer: TextRasterizer,
    /// The identity of the fonts the texts are drawn with, from
    /// [`NEXT_FONT_SET`].
    font_set: u64,
    entries: HashMap<u64, TextEntry>,
    frame: u64,
}

impl fmt::Debug for TextTextures {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TextTextures")
            .field("fonts", &self.rasterizer.font_count())
            .field("texts", &self.entries.len())
            .finish_non_exhaustive()
    }
}

impl Default for TextTextures {
    fn default() -> Self {
        Self::new()
    }
}

impl TextTextures {
    /// No fonts and no texts.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rasterizer: TextRasterizer::empty(),
            font_set: NEXT_FONT_SET.fetch_add(1, Ordering::Relaxed),
            entries: HashMap::new(),
            frame: 0,
        }
    }

    /// Appends an OpenType font to the fallback stack text draws with. Texts
    /// already rasterized are dropped.
    ///
    /// # Errors
    /// Returns [`TextError::Raster`] for malformed font bytes.
    pub fn add_font(
        &mut self,
        family: impl Into<String>,
        bytes: impl Into<Arc<[u8]>>,
        face_index: u32,
    ) -> Result<(), TextError> {
        self.rasterizer
            .add_font(TextFont {
                family: family.into(),
                bytes: bytes.into(),
                face_index,
            })
            .map_err(TextError::Raster)?;
        self.entries.clear();
        // The same text may now draw other glyphs.
        self.font_set = NEXT_FONT_SET.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Fonts in the fallback stack.
    #[must_use]
    pub fn font_count(&self) -> usize {
        self.rasterizer.font_count()
    }

    /// Rasterizes and uploads every text `frame` shows at `size` that is not
    /// already held: its captions and the text generators its clips draw.
    /// Forgets the least recently shown beyond a small number.
    ///
    /// # Errors
    /// Returns [`TextError`] when a text cannot be rasterized or uploaded;
    /// with no fonts registered, only if the frame shows one.
    pub fn prepare(
        &mut self,
        renderer: &Renderer,
        frame: &FrameDesc,
        size: RenderSize,
    ) -> Result<(), TextError> {
        self.frame += 1;
        for (text, style) in frame_texts(frame) {
            let key = text_key(text, style, size);
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.last_used = self.frame;
                continue;
            }
            let request = text_request(text, style, size, key);
            let raster = self
                .rasterizer
                .rasterize(&request)
                .map_err(TextError::Raster)?;
            // Each text is rasterized once per key; keep no copy.
            self.rasterizer.invalidate(key);
            let texture_size = &raster.descriptor.texture;
            // The anchor places the decorated text; the margin, which holds only ink reaching past
            // it, takes no part in placement.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let margin = [raster.descriptor.layout_x, raster.descriptor.layout_y]
                .map(|margin| (margin as u32).saturating_sub(request.decoration_margin()));
            let texture = renderer
                .upload_rgba8(
                    RenderSize::new(texture_size.width, texture_size.height)
                        .map_err(TextError::Render)?,
                    &raster.pixels.rgba8,
                )
                .map_err(TextError::Render)?;
            self.entries.insert(
                key,
                TextEntry {
                    texture,
                    width: texture_size.width,
                    height: texture_size.height,
                    margin,
                    last_used: self.frame,
                },
            );
        }
        while self.entries.len() > RETAINED_TEXTS {
            let Some(oldest) = self
                .entries
                .iter()
                .filter(|(_, entry)| entry.last_used < self.frame)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
        Ok(())
    }

    /// [`Self::prepare`] for the frame of `project` at `time`, rendered at
    /// `size`; nothing to do, and no evaluation, when the project draws no
    /// text ([`crate::EngineProject::draws_text`]).
    ///
    /// # Errors
    /// Returns [`crate::EngineError::Evaluate`] or
    /// [`crate::EngineError::Text`].
    pub fn prepare_at(
        &mut self,
        renderer: &Renderer,
        project: &crate::EngineProject,
        time: alpha2_time::MediaTime,
        size: RenderSize,
    ) -> Result<(), crate::EngineError> {
        if !project.draws_text() {
            return Ok(());
        }
        let frame = project.evaluate(time)?;
        self.prepare(renderer, &frame, size)
            .map_err(crate::EngineError::Text)
    }

    /// Where `text` in `style` lies on a canvas of `size`, drawn with these
    /// fonts: [`measure_text`].
    ///
    /// # Errors
    /// Returns [`TextError::Raster`] as [`Self::prepare`] would for the text.
    pub fn measure(
        &self,
        text: &str,
        style: &TextStyle,
        size: RenderSize,
    ) -> Result<TextBounds, TextError> {
        measure_text(&self.rasterizer, text, style, size)
    }

    /// The prepared text with `key`, as [`text_key`] names it.
    #[must_use]
    pub fn get(&self, key: u64) -> Option<TextTexture<'_>> {
        self.entries.get(&key).map(|entry| TextTexture {
            texture: SourceView::from(&entry.texture),
            width: entry.width,
            height: entry.height,
            margin: entry.margin,
            cache_key: Some(self.cache_key(key)),
        })
    }

    /// The result cache key of the text with `key`, drawn with these fonts:
    /// the font set in the high half, beside the text domain, and the text
    /// in the low half, so two keys are equal only for the same text drawn
    /// with the same fonts.
    fn cache_key(&self, key: u64) -> RenderCacheKey {
        let high = TEXT_CACHE_DOMAIN ^ self.font_set;
        RenderCacheKey::from_value((u128::from(high) << 64) | u128::from(key))
    }
}

/// Every text `frame` may draw, with its style: each caption, then each text
/// generator of a clip it draws, the clip's own source or a step of its
/// compositions. A text drawn twice is named twice.
pub(crate) fn frame_texts<'f>(
    frame: &'f FrameDesc<'_>,
) -> impl Iterator<Item = (&'f str, &'f TextStyle)> {
    let captions = frame
        .captions
        .iter()
        .map(|caption| (caption.text, caption.style));
    let generators = frame
        .visual
        .iter()
        .filter(|clip| frame.draws(clip))
        .flat_map(|clip| {
            let steps = clip
                .composition
                .iter()
                .chain(&clip.source_composition)
                .flat_map(|composition| &composition.plan().steps)
                .filter_map(|step| match &step.op {
                    StepOp::Generator(generator) => Some(generator),
                    _ => None,
                });
            clip.generator.into_iter().chain(steps)
        })
        .filter_map(|generator| match generator {
            Generator::Text { text, style } => Some((text.as_str(), style)),
            _ => None,
        });
    captions.chain(generators)
}

/// A host's frame provider together with its text store: frames come from
/// the one and text from the other.
pub struct FramesWithText<'a> {
    frames: &'a dyn crate::FrameProvider,
    texts: &'a TextTextures,
}

impl<'a> FramesWithText<'a> {
    /// `frames` with the texts `texts` holds.
    #[must_use]
    pub fn new(frames: &'a dyn crate::FrameProvider, texts: &'a TextTextures) -> Self {
        Self { frames, texts }
    }
}

impl crate::FrameProvider for FramesWithText<'_> {
    fn texture_for(
        &self,
        source: &alpha2_media::SourceId,
        time: alpha2_time::MediaTime,
    ) -> Option<SourceView<'_>> {
        self.frames.texture_for(source, time)
    }

    fn pending_source_work(
        &self,
        source: &alpha2_media::SourceId,
        time: alpha2_time::MediaTime,
    ) -> Option<&alpha2_render::PendingSourceWork> {
        self.frames.pending_source_work(source, time)
    }

    fn source_key(
        &self,
        source: &alpha2_media::SourceId,
        time: alpha2_time::MediaTime,
    ) -> Option<alpha2_render::RenderCacheKey> {
        self.frames.source_key(source, time)
    }

    fn text(&self, key: u64) -> Option<TextTexture<'_>> {
        self.texts.get(key)
    }
}

/// A text's identity at a render size: the text, its style, and the size,
/// hashed (FNV-1a). Never zero.
#[must_use]
pub fn text_key(text: &str, style: &TextStyle, size: RenderSize) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    feed(text.as_bytes());
    feed(&[0xff]);
    // Family names cannot hold 0xff (they are UTF-8), so the text and family stay apart.
    feed(style.font_family.as_deref().unwrap_or_default().as_bytes());
    feed(&[0xff, u8::from(style.font_family.is_some())]);
    for value in [
        style.font_size,
        style.line_height,
        style.max_width,
        style.position.x,
        style.position.y,
        style.anchor.x,
        style.anchor.y,
    ] {
        feed(&value.to_bits().to_le_bytes());
    }
    feed(&rgba8(style.color));
    feed(&[match style.align {
        TextAlign::Start => 0,
        TextAlign::Center => 1,
        TextAlign::End => 2,
    }]);
    match style.outline {
        Some(outline) => {
            feed(&[1]);
            feed(&rgba8(outline.color));
            feed(&outline.width.to_bits().to_le_bytes());
        }
        None => feed(&[0]),
    }
    match style.background {
        Some(background) => {
            feed(&[1]);
            feed(&rgba8(background.color));
            feed(&background.padding.to_bits().to_le_bytes());
        }
        None => feed(&[0]),
    }
    feed(&size.width.to_le_bytes());
    feed(&size.height.to_le_bytes());
    hash.max(1)
}

/// Where `text` in `style` lies on a canvas of `size` ([`TextBounds`]), laid
/// out with `rasterizer`'s fonts as [`TextTextures::prepare`] lays it out and
/// placed as a frame places it, without drawing it. Measured at the size a
/// frame is drawn at, it agrees with that frame to the pixel; text laid out
/// at another size can break its lines elsewhere.
///
/// # Errors
/// Returns [`TextError::Raster`] when the text host refuses the text: no
/// fonts, or a style out of range.
#[allow(clippy::cast_precision_loss)]
pub fn measure_text(
    rasterizer: &TextRasterizer,
    text: &str,
    style: &TextStyle,
    size: RenderSize,
) -> Result<TextBounds, TextError> {
    let request = text_request(text, style, size, text_key(text, style, size));
    let measure = rasterizer.measure(&request).map_err(TextError::Raster)?;
    let (width, height) = (size.width as f32, size.height as f32);
    let margin = measure.decoration_margin as f32;
    let (box_width, box_height) = (measure.box_width as f32, measure.box_height as f32);
    // The layout box's top-left corner on the canvas: the anchor of the box with its decorations
    // lies at the position, as `text_placement` places the texture.
    let origin = [
        style.position.x * width - style.anchor.x * (box_width + 2.0 * margin) + margin,
        style.position.y * height - style.anchor.y * (box_height + 2.0 * margin) + margin,
    ];
    let rect = |[left, top, right, bottom]: [f32; 4]| CanvasRect {
        left: (origin[0] + left) / width,
        top: (origin[1] + top) / height,
        right: (origin[0] + right) / width,
        bottom: (origin[1] + bottom) / height,
    };
    Ok(TextBounds {
        layout: rect([-margin, -margin, box_width + margin, box_height + margin]),
        content: rect(measure.content),
        ink: measure.ink.map(rect),
        line_count: measure.line_count,
    })
}

/// A straight, sRGB-encoded color as bytes.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rgba8(color: Rgba) -> [u8; 4] {
    [color.red, color.green, color.blue, color.alpha]
        .map(|value| (value.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// The text host request for `text` at `size`: sizes scaled from canvas
/// fractions to pixels, within what the text host draws.
#[allow(clippy::cast_precision_loss)]
fn text_request(text: &str, style: &TextStyle, size: RenderSize, key: u64) -> TextRasterRequest {
    let font_size = (style.font_size * size.height as f32).clamp(1.0, MAX_FONT_PIXELS);
    TextRasterRequest {
        text: text.to_owned(),
        font_size,
        max_width: (style.max_width * size.width as f32).max(1.0),
        line_height: style.line_height,
        align: match style.align {
            TextAlign::Start => alpha2_media::TextAlign::Start,
            TextAlign::Center => alpha2_media::TextAlign::Center,
            TextAlign::End => alpha2_media::TextAlign::End,
        },
        color: rgba8(style.color),
        emoji_policy: EmojiPolicy::Color,
        content_version: key,
        outline: style.outline.map(|outline| alpha2_text::TextOutline {
            color: rgba8(outline.color),
            width: (outline.width * font_size).clamp(f32::MIN_POSITIVE, MAX_OUTLINE_PIXELS),
        }),
        background: style
            .background
            .map(|background| alpha2_text::TextBackground {
                color: rgba8(background.color),
                padding: (background.padding * font_size).clamp(0.0, MAX_PADDING_PIXELS),
            }),
        font_family: style.font_family.clone(),
    }
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod tests;
