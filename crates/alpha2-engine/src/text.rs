//! Text rendering: caption tracks' cues and text generators, rasterized by
//! the portable text host.
//!
//! The engine draws text itself, from font bytes its host registers once,
//! so a browser preview and a native export of one project, given the same
//! fonts, draw the same text pixels. A host keeps one [`TextTextures`]
//! beside its frame provider, calls [`TextTextures::prepare`] for each frame
//! before rendering it, and answers [`crate::FrameProvider::text`] from it.
//! A caption is composited as a layer between tracks; a text generator's
//! texture is placed on the transparent picture the generator draws. A
//! layout generator is resolved here too, with the same fonts, and its
//! texts rasterized beside the others ([`crate::layout`]).

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use alpha2_eval::{FrameDesc, GeneratorPlan, LayoutPlan, StepOp};
use alpha2_layout::{
    PictureSize, ShadowPixels, TextBounds, TextDraw, shaped_key, text_draw, text_key,
};
use alpha2_model::{Generator, TextStyle};
use alpha2_render::{
    CpuImage, CpuRenderer, CpuSource, RenderCacheKey, RenderError, RenderSize, Renderer,
    SourceView, UploadTexture,
};
use alpha2_text::{ShapedText, TextFont, TextRasterError, TextRasterRequest, TextRasterizer};

use crate::layout::PreparedLayout;

/// `size` as the picture text and layouts are placed on.
pub(crate) fn picture(size: RenderSize) -> PictureSize {
    PictureSize::new(size.width, size.height).expect("a render size is a picture size")
}

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

/// How many resolved layouts a host keeps between frames; the current
/// frame's are kept however many it shows.
const RETAINED_LAYOUTS: usize = 8;

/// How many shaped texts a host keeps for layout, least recently used
/// first out.
const RETAINED_SHAPED: usize = 256;

/// Where [`TextTextures`] keeps the pixels of a rasterized text: the GPU
/// renderer uploads them to a texture, the CPU renderer keeps them.
pub trait TextUpload {
    /// Takes `rgba8`, premultiplied sRGB-encoded RGBA of `size` with rows
    /// packed, as the renderer's source for that text.
    ///
    /// # Errors
    /// Returns [`RenderError::InvalidImageData`] when `rgba8` does not hold
    /// `size`, or the GPU's upload failure.
    fn upload_text(&self, size: RenderSize, rgba8: &[u8]) -> Result<TextPixels, RenderError>;
}

/// The pixels of one prepared text, as the renderer that drew it keeps
/// them.
#[derive(Debug)]
pub struct TextPixels(TextStorage);

#[derive(Debug)]
enum TextStorage {
    Gpu(UploadTexture),
    Cpu(CpuSource),
}

impl TextPixels {
    /// The text as a graph source.
    fn view(&self) -> SourceView<'_> {
        match &self.0 {
            TextStorage::Gpu(texture) => SourceView::from(texture),
            TextStorage::Cpu(source) => SourceView::cpu(source),
        }
    }
}

impl TextUpload for Renderer {
    fn upload_text(&self, size: RenderSize, rgba8: &[u8]) -> Result<TextPixels, RenderError> {
        self.upload_rgba8(size, rgba8)
            .map(|texture| TextPixels(TextStorage::Gpu(texture)))
    }
}

impl TextUpload for CpuRenderer {
    fn upload_text(&self, size: RenderSize, rgba8: &[u8]) -> Result<TextPixels, RenderError> {
        let picture = CpuImage::from_premultiplied_rgba8(size, rgba8)?;
        Ok(TextPixels(TextStorage::Cpu(CpuSource::image(Arc::new(
            picture,
        )))))
    }
}

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
    /// The background's boxes, the size of `texture`, when the text has both
    /// boxes and shadows, which lie between them and the glyphs: `texture`
    /// then holds no boxes. With the key results drawn from it are cached
    /// under, as `cache_key` is for `texture`.
    pub background: Option<(SourceView<'a>, Option<RenderCacheKey>)>,
    /// The shadows of the glyphs, first on top, in pixels of the picture the
    /// text is placed on.
    pub shadows: &'a [ShadowPixels],
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
    texture: TextPixels,
    background: Option<TextPixels>,
    shadows: Vec<ShadowPixels>,
    width: u32,
    height: u32,
    margin: [u32; 2],
    /// Where the layout box's top-left corner lies in the texture.
    corner: [u32; 2],
    last_used: u64,
}

/// A layout resolved at one size, with the requests of its texts, to
/// rasterize again one the text store has let go.
struct LayoutEntry {
    prepared: Arc<PreparedLayout>,
    requests: Vec<(u64, TextDraw)>,
    last_used: u64,
}

/// A text shaped for layout, and when it was last asked for.
struct ShapedEntry {
    shaped: Arc<ShapedText>,
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
    /// Resolved layouts by [`layout_key`].
    layouts: HashMap<u128, LayoutEntry>,
    /// Texts shaped for layout, by [`shaped_key`].
    shaped: HashMap<u64, ShapedEntry>,
    /// How many layouts have been resolved: a static layout resolves once.
    resolves: u64,
    frame: u64,
}

impl fmt::Debug for TextTextures {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TextTextures")
            .field("fonts", &self.rasterizer.font_count())
            .field("texts", &self.entries.len())
            .field("layouts", &self.layouts.len())
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
            layouts: HashMap::new(),
            shaped: HashMap::new(),
            resolves: 0,
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
        self.layouts.clear();
        self.shaped.clear();
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
    /// Resolves every layout it draws that is not already held, and
    /// rasterizes its texts. Forgets the least recently shown beyond a small
    /// number.
    ///
    /// # Errors
    /// Returns [`TextError`] when a text cannot be rasterized or uploaded;
    /// with no fonts registered, only if the frame shows one.
    pub fn prepare(
        &mut self,
        renderer: &dyn TextUpload,
        frame: &FrameDesc,
        size: RenderSize,
    ) -> Result<(), TextError> {
        self.frame += 1;
        let picture = picture(size);
        for (text, style) in frame_texts(frame) {
            let key = text_key(text, style, picture);
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.last_used = self.frame;
                continue;
            }
            let draw = text_draw(text, style, picture, key);
            self.rasterize(renderer, key, &draw)?;
        }
        for (plan, time) in frame_layouts(frame) {
            self.prepare_layout(renderer, plan, time, size)?;
        }
        while self.layouts.len() > RETAINED_LAYOUTS {
            let Some(oldest) = self
                .layouts
                .iter()
                .filter(|(_, entry)| entry.last_used < self.frame)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            self.layouts.remove(&oldest);
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

    /// Rasterizes `draw` and uploads it as the text with `key`.
    fn rasterize(
        &mut self,
        renderer: &dyn TextUpload,
        key: u64,
        draw: &TextDraw,
    ) -> Result<(), TextError> {
        let request = &draw.request;
        let raster = self
            .rasterizer
            .rasterize(request)
            .map_err(TextError::Raster)?;
        // Each text is rasterized once per key; keep no copy.
        self.rasterizer.invalidate(key);
        let texture_size = &raster.descriptor.texture;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let corner =
            [raster.descriptor.layout_x, raster.descriptor.layout_y].map(|corner| corner as u32);
        // The anchor places the decorated text; the margin, which holds only ink reaching past
        // it, takes no part in placement.
        let margin = corner.map(|corner| corner.saturating_sub(request.decoration_margin()));
        let size =
            RenderSize::new(texture_size.width, texture_size.height).map_err(TextError::Render)?;
        let texture = renderer
            .upload_text(size, &raster.pixels.rgba8)
            .map_err(TextError::Render)?;
        let background = raster
            .background
            .as_deref()
            .map(|rgba8| renderer.upload_text(size, rgba8))
            .transpose()
            .map_err(TextError::Render)?;
        self.entries.insert(
            key,
            TextEntry {
                texture,
                background,
                shadows: draw.shadows.clone(),
                width: texture_size.width,
                height: texture_size.height,
                margin,
                corner,
                last_used: self.frame,
            },
        );
        Ok(())
    }

    /// Resolves `plan` at picture time `time` and `size`, and rasterizes its
    /// texts, unless that layout is already held: then only texts the store
    /// let go are rasterized again.
    fn prepare_layout(
        &mut self,
        renderer: &dyn TextUpload,
        plan: &LayoutPlan,
        time: alpha2_time::MediaTime,
        size: RenderSize,
    ) -> Result<(), TextError> {
        let evaluated = plan.evaluate(time);
        let key = layout_key(evaluated.key, size);
        if let Some(entry) = self.layouts.get_mut(&key) {
            entry.last_used = self.frame;
            let requests = entry.requests.clone();
            for (text, request) in &requests {
                match self.entries.get_mut(text) {
                    Some(held) => held.last_used = self.frame,
                    None => self.rasterize(renderer, *text, request)?,
                }
            }
            return Ok(());
        }
        self.resolves += 1;
        let frame = self.frame;
        let picture = picture(size);
        let shaped =
            alpha2_layout::shape_texts(&evaluated, picture, |request| self.shape(request, frame))
                .map_err(TextError::Raster)?;
        let resolved = alpha2_layout::resolve(&evaluated, picture, &shaped);
        let (texts, requests) = alpha2_layout::layout_texts(&evaluated, &resolved, picture);
        let mut boxes = Vec::with_capacity(texts.len());
        for text in &texts {
            boxes.push(match text {
                None => None,
                Some(text_key) => {
                    let draw = &requests
                        .iter()
                        .find(|(key, _)| key == text_key)
                        .expect("a request per text")
                        .1;
                    match self.entries.get_mut(text_key) {
                        Some(held) => held.last_used = self.frame,
                        None => self.rasterize(renderer, *text_key, draw)?,
                    }
                    let entry = &self.entries[text_key];
                    Some(crate::layout::paint::TextBox {
                        corner: entry.corner,
                        size: [entry.width, entry.height],
                    })
                }
            });
        }
        let items = crate::layout::paint::paint_list(&evaluated, &resolved, size, &boxes);
        self.layouts.insert(
            key,
            LayoutEntry {
                prepared: Arc::new(PreparedLayout { items, texts }),
                requests,
                last_used: self.frame,
            },
        );
        Ok(())
    }

    /// `request`'s text shaped for layout, from the cache when it holds it.
    fn shape(
        &mut self,
        request: &TextRasterRequest,
        frame: u64,
    ) -> Result<Arc<ShapedText>, TextRasterError> {
        let key = shaped_key(request);
        if let Some(entry) = self.shaped.get_mut(&key) {
            entry.last_used = frame;
            return Ok(Arc::clone(&entry.shaped));
        }
        let shaped = Arc::new(self.rasterizer.shape(request)?);
        if self.shaped.len() >= RETAINED_SHAPED
            && let Some(oldest) = self
                .shaped
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
        {
            self.shaped.remove(&oldest);
        }
        self.shaped.insert(
            key,
            ShapedEntry {
                shaped: Arc::clone(&shaped),
                last_used: frame,
            },
        );
        Ok(shaped)
    }

    /// How many layouts have been resolved, held or not.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn layout_resolves(&self) -> u64 {
        self.resolves
    }

    /// The prepared layout with `key`, as [`layout_key`] names it.
    #[must_use]
    pub fn layout(&self, key: u128) -> Option<Arc<PreparedLayout>> {
        self.layouts
            .get(&key)
            .map(|entry| Arc::clone(&entry.prepared))
    }

    /// Where each node of `root` lies on a picture of `size` at picture
    /// time `time`, its pixels those of a `canvas` canvas, laid out with
    /// these fonts: [`crate::measure_layout`].
    ///
    /// # Errors
    /// Returns [`TextError::Raster`] as [`Self::prepare`] would for its
    /// texts.
    pub fn measure_layout(
        &self,
        root: &alpha2_model::LayoutNode,
        time: alpha2_time::MediaTime,
        size: RenderSize,
        canvas: (u32, u32),
    ) -> Result<crate::LayoutBounds, TextError> {
        crate::measure_layout(&self.rasterizer, root, time, picture(size), canvas)
            .map_err(TextError::Raster)
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
        renderer: &dyn TextUpload,
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
    /// fonts: [`crate::measure_text`].
    ///
    /// # Errors
    /// Returns [`TextError::Raster`] as [`Self::prepare`] would for the text.
    pub fn measure(
        &self,
        text: &str,
        style: &TextStyle,
        size: RenderSize,
    ) -> Result<TextBounds, TextError> {
        crate::measure_text(&self.rasterizer, text, style, picture(size)).map_err(TextError::Raster)
    }

    /// The prepared text with `key`, as [`crate::text_key`] names it.
    #[must_use]
    pub fn get(&self, key: u64) -> Option<TextTexture<'_>> {
        self.entries.get(&key).map(|entry| TextTexture {
            texture: entry.texture.view(),
            width: entry.width,
            height: entry.height,
            margin: entry.margin,
            cache_key: Some(self.cache_key(key)),
            background: entry
                .background
                .as_ref()
                .map(|background| (background.view(), Some(self.background_cache_key(key)))),
            shadows: &entry.shadows,
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

    /// The result cache key of the background plane of the text with `key`:
    /// [`Self::cache_key`] in a domain of its own, so the glyphs and the boxes
    /// of one text never share a key.
    fn background_cache_key(&self, key: u64) -> RenderCacheKey {
        let high = (TEXT_CACHE_DOMAIN ^ self.font_set) ^ (1 << 62);
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
                    StepOp::Generator { generator, .. } => Some(generator),
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

    fn layout(&self, key: u128) -> Option<Arc<PreparedLayout>> {
        self.texts.layout(key)
    }
}

/// Every layout `frame` may draw, with its picture time: each drawn clip's
/// layout source at its source time, then each layout step of its
/// compositions at the step's time plus its start.
pub(crate) fn frame_layouts<'f>(
    frame: &'f FrameDesc<'_>,
) -> impl Iterator<Item = (&'f LayoutPlan, alpha2_time::MediaTime)> {
    frame
        .visual
        .iter()
        .filter(|clip| frame.draws(clip))
        .flat_map(|clip| {
            let own = clip
                .plan
                .and_then(GeneratorPlan::layout)
                .map(|plan| (plan, clip.source_time));
            let steps = clip
                .composition
                .iter()
                .chain(&clip.source_composition)
                .flat_map(|composition| {
                    let time = composition.time();
                    composition
                        .plan()
                        .steps
                        .iter()
                        .filter_map(move |step| match &step.op {
                            StepOp::Generator {
                                plan: Some(plan),
                                start,
                                ..
                            } => plan.layout().map(|plan| {
                                (
                                    plan,
                                    alpha2_time::MediaTime(
                                        step.time(time).ticks().saturating_add(start.ticks()),
                                    ),
                                )
                            }),
                            _ => None,
                        })
                });
            own.into_iter().chain(steps)
        })
}

/// A layout's identity at a render size: its evaluated key and the size.
/// Equal keys lay out and draw alike with the same fonts.
#[must_use]
pub fn layout_key(evaluated: u128, size: RenderSize) -> u128 {
    let mut hash = alpha2_geometry::StableHash::new();
    hash.bytes(b"layout");
    hash.bytes(&evaluated.to_le_bytes());
    hash.u32(size.width);
    hash.u32(size.height);
    hash.finish()
}
