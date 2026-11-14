//! Text in a text style: its text host request, its identity, and where it lies on a picture.

use alpha2_media::EmojiPolicy;
use alpha2_model::{Paint, Rgba, Spread, TextAlign, TextShadow, TextStroke, TextStyle};
use alpha2_text::{
    ShapedText, TextFill, TextGradient, TextGradientKind, TextGradientStop, TextRasterError,
    TextRasterRequest, TextRasterizer, TextSpread,
};

use crate::{Baselines, CanvasRect, PictureSize};

/// The largest font, stroke extent, and padding the text host draws, in pixels.
pub const MAX_FONT_PIXELS: f32 = 2_048.0;
/// The most a text's strokes reach together, in pixels. See [`MAX_FONT_PIXELS`].
pub const MAX_STROKE_PIXELS: f32 = 64.0;
/// See [`MAX_FONT_PIXELS`].
pub const MAX_PADDING_PIXELS: f32 = 256.0;

/// Where a text lies, as fractions of the canvas from `(0, 0)` top-left to `(1, 1)`
/// bottom-right. For a text generator this is on its picture, before the clip's transform
/// and effects; for a caption it is on the canvas.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TextBounds {
    /// The box the style's anchor and position place: the wrapping width wide, every line's
    /// em box tall, plus whole pixels of outline and background padding.
    pub layout: CanvasRect,
    /// The span of the lines' advances, as tall as the layout and with the same decorations.
    /// It moves only with the advances, not with glyph ink.
    pub content: CanvasRect,
    /// What the text draws (glyphs, outline, background boxes), as
    /// [`alpha2_text::TextMeasure::ink`] bounds it, within the texture's sampling spread.
    /// `None` when the text draws nothing.
    pub ink: Option<CanvasRect>,
    /// Number of laid-out lines.
    pub line_count: u32,
    /// The first and last lines' alphabetic baselines.
    pub baselines: Baselines,
    /// The font size drawn after any `font_size_fit`, a fraction of the canvas's shorter side.
    pub font_size: f32,
    /// Whether the style's `line_clamp` discarded lines.
    pub truncated: bool,
    /// Whether the drawn text still splits a word or is taller than `max_height`.
    pub overflow: bool,
}

/// A layout text's raster request, shadows and animators hashed as a text store key
/// (FNV-1a over a domain byte and every field). Never zero.
#[must_use]
pub fn layout_text_key(
    request: &TextRasterRequest,
    shadows: &[ShadowPixels],
    animators: &[alpha2_eval::SampledAnimator],
) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    // Layout texts take keys apart from captions' and text generators'.
    feed(&[0x4c]);
    feed_request(&mut feed, request);
    if !shadows.is_empty() {
        feed(&[0xfd]);
        feed_count(&mut feed, shadows.len());
        for shadow in shadows {
            for value in [shadow.offset[0], shadow.offset[1], shadow.sigma] {
                feed(&value.to_bits().to_le_bytes());
            }
            for value in shadow.color {
                feed(&value.to_bits().to_le_bytes());
            }
        }
    }
    if !animators.is_empty() {
        let mut stable = alpha2_geometry::StableHash::new();
        alpha2_eval::hash_sampled_animators(&mut stable, animators);
        feed(&[0xfa]);
        feed(&stable.finish().to_le_bytes());
    }
    hash.max(1)
}

/// What a request is shaped from, as a shaped-text cache key. Equal keys shape alike with
/// the same fonts.
#[must_use]
pub fn shaped_key(request: &TextRasterRequest) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    feed(request.text.as_bytes());
    feed(&[0xff]);
    feed(
        request
            .font_family
            .as_deref()
            .unwrap_or_default()
            .as_bytes(),
    );
    feed(&[0xff, u8::from(request.font_family.is_some())]);
    feed(&request.font_size.to_bits().to_le_bytes());
    feed(&request.line_height.to_bits().to_le_bytes());
    feed(&request.letter_spacing.to_bits().to_le_bytes());
    feed(&request.font_weight.to_le_bytes());
    hash
}

fn feed_request(feed: &mut impl FnMut(&[u8]), request: &TextRasterRequest) {
    // No `..`: a new field must decide whether it is part of the key.
    let TextRasterRequest {
        text,
        font_size,
        max_width,
        line_height,
        align,
        fill,
        emoji_policy,
        content_version: _,
        strokes,
        background,
        split_background,
        font_family,
        letter_spacing,
        font_weight,
        clamp,
    } = request;
    feed(text.as_bytes());
    feed(&[0xff]);
    feed(font_family.as_deref().unwrap_or_default().as_bytes());
    feed(&[0xff, u8::from(font_family.is_some())]);
    for value in [*font_size, *max_width, *line_height, *letter_spacing] {
        feed(&value.to_bits().to_le_bytes());
    }
    feed(&font_weight.to_le_bytes());
    match fill {
        TextFill::Solid(color) => feed(color),
        TextFill::Gradient(gradient) => feed_text_gradient(feed, gradient),
    }
    feed(&[match align {
        alpha2_media::TextAlign::Start => 0,
        alpha2_media::TextAlign::Center => 1,
        alpha2_media::TextAlign::End => 2,
    }]);
    feed_count(feed, strokes.len());
    for stroke in strokes {
        feed(&stroke.color);
        feed(&stroke.width.to_bits().to_le_bytes());
    }
    if *split_background {
        feed(&[0xfd]);
    }
    if let Some(background) = background {
        feed(&[0xfb]);
        feed(&background.color);
        feed(&background.padding.to_bits().to_le_bytes());
    }
    // The default policy feeds nothing, so keys of requests that never set it stay as they were.
    match emoji_policy {
        alpha2_media::EmojiPolicy::Color => {}
        alpha2_media::EmojiPolicy::Monochrome => feed(&[0xf9, 1]),
        alpha2_media::EmojiPolicy::Replace => feed(&[0xf9, 2]),
    }
    if let Some(clamp) = clamp {
        feed(&[0xfc]);
        feed(&clamp.max_lines.unwrap_or(0).to_le_bytes());
        feed(&clamp.max_height.to_bits().to_le_bytes());
        feed(clamp.ellipsis.as_deref().unwrap_or_default().as_bytes());
        feed(&[0xff, u8::from(clamp.ellipsis.is_some())]);
    }
}

/// A list's length as one byte. A list of one feeds the same byte the single optional value
/// it replaced fed, which keeps existing keys stable.
#[allow(clippy::cast_possible_truncation)]
fn feed_count(feed: &mut impl FnMut(&[u8]), count: usize) {
    feed(&[count.min(255) as u8]);
}

fn feed_text_gradient(feed: &mut impl FnMut(&[u8]), gradient: &TextGradient) {
    // A tag no color's first byte can be mistaken for: a solid fill feeds four bytes only.
    feed(&[0xfe]);
    let points: &[f32] = match &gradient.kind {
        TextGradientKind::Linear { start, end } => &[0.0, start[0], start[1], end[0], end[1]],
        TextGradientKind::Radial { center, radius } => &[1.0, center[0], center[1], *radius],
        TextGradientKind::Conic { center, angle } => &[2.0, center[0], center[1], *angle],
    };
    for value in points {
        feed(&value.to_bits().to_le_bytes());
    }
    feed(&[gradient.spread as u8]);
    feed_count(feed, gradient.stops.len());
    for stop in &gradient.stops {
        feed(&stop.offset.to_bits().to_le_bytes());
        for value in stop.color {
            feed(&value.to_bits().to_le_bytes());
        }
    }
}

/// A paint's identity for a text key: a solid color as its bytes, a gradient by every field.
fn feed_paint(feed: &mut impl FnMut(&[u8]), paint: &Paint) {
    match paint {
        Paint::Solid { color } => feed(&rgba8(*color)),
        gradient => feed_text_gradient(feed, &text_gradient(gradient)),
    }
}

/// A text style's strokes and shadows for a text key. One stroke feeds what an outline fed,
/// and no shadows feed nothing.
fn feed_look(feed: &mut impl FnMut(&[u8]), strokes: &[TextStroke], shadows: &[TextShadow]) {
    feed_count(feed, strokes.len());
    for stroke in strokes {
        feed(&rgba8(stroke.color));
        feed(&stroke.width.to_bits().to_le_bytes());
    }
    if !shadows.is_empty() {
        feed(&[0xfd]);
        feed_count(feed, shadows.len());
        for shadow in shadows {
            feed(&rgba8(shadow.color));
            for value in [shadow.offset.x, shadow.offset.y, shadow.blur] {
                feed(&value.to_bits().to_le_bytes());
            }
        }
    }
}

/// A text's identity at a render size: the text, its style and the size, hashed with FNV-1a.
/// Never zero.
#[must_use]
pub fn text_key(text: &str, style: &TextStyle, size: PictureSize) -> u64 {
    // No `..`: a new field must decide whether it is part of the key.
    let TextStyle {
        font_family,
        font_size,
        fill,
        line_height,
        letter_spacing,
        font_weight,
        align,
        max_width,
        max_height,
        font_size_fit,
        line_clamp,
        position,
        anchor,
        strokes,
        shadows,
        background,
        // `animated_text_key` adds the sampled animators.
        animators: _,
    } = style;
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    feed(text.as_bytes());
    feed(&[0xff]);
    // Family names cannot hold 0xff (they are UTF-8), so the text and family stay apart.
    feed(font_family.as_deref().unwrap_or_default().as_bytes());
    feed(&[0xff, u8::from(font_family.is_some())]);
    for value in [
        *font_size,
        *line_height,
        *letter_spacing,
        *max_width,
        position.x,
        position.y,
        anchor.x,
        anchor.y,
    ] {
        feed(&value.to_bits().to_le_bytes());
    }
    feed(&font_weight.to_le_bytes());
    feed_paint(&mut feed, fill);
    feed(&[match align {
        TextAlign::Start => 0,
        TextAlign::Center => 1,
        TextAlign::End => 2,
    }]);
    feed_look(&mut feed, strokes, shadows);
    match background {
        Some(background) => {
            feed(&[1]);
            feed(&rgba8(background.color));
            feed(&background.padding.to_bits().to_le_bytes());
        }
        None => feed(&[0]),
    }
    // Only fields that are set are fed.
    if let Some(height) = max_height {
        feed(&[0xf0]);
        feed(&height.to_bits().to_le_bytes());
    }
    if let Some(fit) = font_size_fit {
        feed(&[0xf1]);
        feed(&fit.min.to_bits().to_le_bytes());
        feed(&fit.max.unwrap_or(-1.0).to_bits().to_le_bytes());
    }
    if let Some(clamp) = line_clamp {
        feed(&[0xf2]);
        feed(&clamp.max_lines.unwrap_or(0).to_le_bytes());
        feed(clamp.block_ellipsis.text().unwrap_or_default().as_bytes());
        feed(&[0xff, u8::from(clamp.block_ellipsis.text().is_some())]);
    }
    feed(&size.width().to_le_bytes());
    feed(&size.height().to_le_bytes());
    hash.max(1)
}

/// Where `text` in `style` lies on a canvas of `size`, laid out with `rasterizer`'s fonts and
/// placed exactly as a frame places it, without drawing it.
///
/// Measured at the frame's render size it agrees with that frame to the pixel; at another
/// size lines can break elsewhere.
///
/// # Errors
/// Returns the text host's refusal: no fonts, or a style out of range.
#[allow(clippy::cast_precision_loss)]
pub fn measure_text(
    rasterizer: &TextRasterizer,
    text: &str,
    style: &TextStyle,
    size: PictureSize,
) -> Result<TextBounds, TextRasterError> {
    let fitted = fit_text(text, style, size, |request| {
        rasterizer.shape(request).map(std::sync::Arc::new)
    })?;
    let request = text_request_at(
        text,
        style,
        size,
        text_key(text, style, size),
        fitted.font_pixels,
    );
    let measure = rasterizer.measure(&request)?;
    let shaped = rasterizer.shape(&request)?;
    let boxed = style_lines(&shaped, style, size, &request);
    let (width, height) = (size.width() as f32, size.height() as f32);
    let margin = measure.decoration_margin as f32;
    let (box_width, box_height) = (measure.box_width as f32, measure.box_height as f32);
    // The layout box's top-left corner on the canvas. The anchor of the decorated box lies at the
    // position, as `text_placement` places the texture.
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
        baselines: Baselines {
            first: (origin[1] + measure.first_baseline) / height,
            last: (origin[1] + measure.last_baseline) / height,
        },
        font_size: fitted.font_size,
        truncated: measure.truncated,
        overflow: boxed.overflow,
    })
}

/// The height in pixels of `style`'s box on a picture of `size`, zero for none.
#[allow(clippy::cast_precision_loss)]
fn box_height(style: &TextStyle, size: PictureSize) -> f32 {
    style
        .max_height
        .map_or(0.0, |height| height * size.height() as f32)
}

/// How `shaped`, shaped for `request`, lies in `style`'s box.
fn style_lines(
    shaped: &ShapedText,
    style: &TextStyle,
    size: PictureSize,
    request: &TextRasterRequest,
) -> crate::fit::BoxedLines {
    crate::fit::boxed_lines(
        shaped,
        request.max_width,
        box_height(style, size),
        style.line_clamp.as_ref().and_then(|clamp| clamp.max_lines),
        style.line_clamp.is_some(),
    )
}

/// The font size `text` in `style` draws at on a picture of `size`: the largest its
/// `font_size_fit` allows at which the text fits its box, with lines from `shape`.
///
/// The box is the wrapping width by `max_height`, and `line_clamp.max_lines` also bounds it.
/// Without a fit it is the style's own size and `shape` is not called.
///
/// # Errors
/// Returns the first refusal of `shape`.
pub fn fit_text<S: std::ops::Deref<Target = ShapedText>>(
    text: &str,
    style: &TextStyle,
    size: PictureSize,
    mut shape: impl FnMut(&TextRasterRequest) -> Result<S, TextRasterError>,
) -> Result<crate::FittedSize, TextRasterError> {
    crate::fit::fit_font_size(
        style.font_size,
        style.font_size_fit,
        size.unit(),
        |pixels| {
            let request = text_request_at(text, style, size, 1, pixels);
            let shaped = shape(&request)?;
            Ok(style_lines(&shaped, style, size, &request).fits)
        },
    )
}

/// A straight, sRGB-encoded color as bytes.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn rgba8(color: Rgba) -> [u8; 4] {
    [color.red, color.green, color.blue, color.alpha]
        .map(|value| (value.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// The text host request for `text` in `style` on a picture of `size`, with content version
/// `key`. Sizes are scaled from canvas fractions to pixels and clamped to what the host draws.
#[must_use]
pub fn text_request(
    text: &str,
    style: &TextStyle,
    size: PictureSize,
    key: u64,
) -> TextRasterRequest {
    text_request_at(text, style, size, key, font_pixels(style, size))
}

/// [`text_request`] at a font size of `font_size` pixels, such as [`fit_text`] finds.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn text_request_at(
    text: &str,
    style: &TextStyle,
    size: PictureSize,
    key: u64,
    font_size: f32,
) -> TextRasterRequest {
    TextRasterRequest {
        text: text.to_owned(),
        font_size,
        max_width: (style.max_width * size.width() as f32).max(1.0),
        line_height: style.line_height,
        align: match style.align {
            TextAlign::Start => alpha2_media::TextAlign::Start,
            TextAlign::Center => alpha2_media::TextAlign::Center,
            TextAlign::End => alpha2_media::TextAlign::End,
        },
        fill: text_fill(&style.fill),
        emoji_policy: EmojiPolicy::Color,
        content_version: key,
        strokes: stroke_pixels(&style.strokes, font_size),
        split_background: !style.shadows.is_empty() && style.background.is_some(),
        background: style
            .background
            .map(|background| alpha2_text::TextBackground {
                color: rgba8(background.color),
                padding: (background.padding * font_size).clamp(0.0, MAX_PADDING_PIXELS),
            }),
        font_family: style.font_family.clone(),
        letter_spacing: style.letter_spacing * font_size,
        font_weight: style.font_weight,
        clamp: style
            .line_clamp
            .as_ref()
            .map(|clamp| crate::fit::text_clamp(clamp, box_height(style, size))),
    }
}

/// A paint as the text host's fill: a color as bytes, or a gradient over the layout box with
/// the same stops and the conic angle in radians.
#[must_use]
pub fn text_fill(paint: &Paint) -> TextFill {
    match paint {
        Paint::Solid { color } => TextFill::Solid(rgba8(*color)),
        gradient => TextFill::Gradient(text_gradient(gradient)),
    }
}

fn text_gradient(paint: &Paint) -> TextGradient {
    let pair = |value: alpha2_model::Vec2| [value.x, value.y];
    let stops = |stops: &[alpha2_model::GradientStop]| {
        stops
            .iter()
            .map(|stop| TextGradientStop {
                offset: stop.offset,
                color: [
                    stop.color.red,
                    stop.color.green,
                    stop.color.blue,
                    stop.color.alpha,
                ],
            })
            .collect()
    };
    let spread = |spread: Spread| match spread {
        Spread::Pad => TextSpread::Pad,
        Spread::Repeat => TextSpread::Repeat,
        Spread::Reflect => TextSpread::Reflect,
    };
    match paint {
        // A solid paint is a gradient of one color, for a caller that needs the gradient form.
        Paint::Solid { color } => TextGradient {
            kind: TextGradientKind::Linear {
                start: [0.0, 0.0],
                end: [1.0, 0.0],
            },
            stops: stops(&[
                alpha2_model::GradientStop {
                    offset: 0.0,
                    color: *color,
                },
                alpha2_model::GradientStop {
                    offset: 1.0,
                    color: *color,
                },
            ]),
            spread: TextSpread::Pad,
        },
        Paint::LinearGradient {
            start,
            end,
            stops: list,
            spread: mode,
        } => TextGradient {
            kind: TextGradientKind::Linear {
                start: pair(*start),
                end: pair(*end),
            },
            stops: stops(list),
            spread: spread(*mode),
        },
        Paint::RadialGradient {
            center,
            radius,
            stops: list,
            spread: mode,
        } => TextGradient {
            kind: TextGradientKind::Radial {
                center: pair(*center),
                radius: *radius,
            },
            stops: stops(list),
            spread: spread(*mode),
        },
        Paint::ConicGradient {
            center,
            angle_degrees,
            stops: list,
        } => TextGradient {
            kind: TextGradientKind::Conic {
                center: pair(*center),
                angle: angle_degrees.to_radians(),
            },
            stops: stops(list),
            spread: TextSpread::Pad,
        },
    }
}

/// `strokes` in pixels of a `font_size`-pixel font (widths are fractions of the font size).
/// Each is at least the smallest positive width, and the last is shortened so together they
/// reach at most [`MAX_STROKE_PIXELS`].
#[must_use]
pub fn stroke_pixels(strokes: &[TextStroke], font_size: f32) -> Vec<alpha2_text::TextStroke> {
    let mut reach = 0.0_f32;
    let mut pixels = Vec::with_capacity(strokes.len());
    for stroke in strokes {
        let room = MAX_STROKE_PIXELS - reach;
        if room <= 0.0 {
            break;
        }
        let width = (stroke.width * font_size).clamp(f32::MIN_POSITIVE, room);
        reach += width;
        pixels.push(alpha2_text::TextStroke {
            color: rgba8(stroke.color),
            width,
        });
    }
    pixels
}

/// What the engine draws for one text: the raster, and the shadows composited under its
/// glyphs.
#[derive(Clone, Debug, PartialEq)]
pub struct TextDraw {
    /// The raster request.
    pub request: TextRasterRequest,
    /// The shadows, first on top.
    pub shadows: Vec<ShadowPixels>,
    /// The text's animators at its time, for a text that has them.
    pub animation: Option<TextAnimation>,
}

/// A text's animators sampled at one time, with what they select from.
#[derive(Clone, Debug, PartialEq)]
pub struct TextAnimation {
    /// The animators, in order.
    pub animators: Vec<alpha2_eval::SampledAnimator>,
    /// The text's runs, none for a plain string.
    pub runs: Vec<alpha2_model::TextRun>,
    /// The time Timing selectors compare run times with.
    pub time: alpha2_time::MediaTime,
}

impl TextDraw {
    /// Each cluster's state and the states' hash
    /// ([`crate::text_animation::cluster_states`]), `None` without animators.
    ///
    /// # Errors
    /// Returns the text host's refusal to lay the text out.
    pub fn states(
        &self,
        rasterizer: &TextRasterizer,
    ) -> Result<Option<(Vec<alpha2_text::ClusterState>, u64)>, TextRasterError> {
        let Some(animation) = &self.animation else {
            return Ok(None);
        };
        let layout = rasterizer.layout(&self.request)?;
        Ok(Some(crate::text_animation::cluster_states(
            &layout,
            &animation.runs,
            &animation.animators,
            animation.time,
            self.request.font_size,
        )))
    }
}

/// [`text_key`] for `content` in `style` shown `time` after its holder started. Without
/// animators this is the static key; otherwise the runs and time are mixed in, so each moment
/// of an animation is a distinct text. Never zero.
#[must_use]
pub fn animated_text_key(
    content: &alpha2_model::TextContent,
    style: &TextStyle,
    size: PictureSize,
    time: alpha2_time::MediaTime,
) -> u64 {
    let key = text_key(&content.text(), style, size);
    if style.animators.is_empty() {
        return key;
    }
    let mut hash = key;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    feed(&[0xfa]);
    let mut stable = alpha2_geometry::StableHash::new();
    alpha2_eval::hash_sampled_animators(
        &mut stable,
        &alpha2_eval::sample_text_animators(&style.animators, time),
    );
    feed(&stable.finish().to_le_bytes());
    for run in content.runs() {
        feed(&run.text.len().to_le_bytes());
        for class in &run.class {
            feed(class.as_bytes());
            feed(&[0xff]);
        }
        if let Some(span) = run.time {
            feed(&span.start.ticks().to_le_bytes());
            feed(&span.end.ticks().to_le_bytes());
        }
        feed(&[0xfe]);
    }
    feed(&time.ticks().to_le_bytes());
    hash.max(1)
}

/// The [`TextDraw`] of `text` in `style` on a picture of `size` at a font size of
/// `font_size` pixels ([`fit_text`]), with content version `key`.
#[must_use]
pub fn text_draw(
    text: &str,
    style: &TextStyle,
    size: PictureSize,
    key: u64,
    font_size: f32,
) -> TextDraw {
    let request = text_request_at(text, style, size, key, font_size);
    let shadows = shadow_pixels(&style.shadows, request.font_size);
    TextDraw {
        request,
        shadows,
        animation: None,
    }
}

/// A text shadow in pixels of the picture its text is placed on.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ShadowPixels {
    /// Offset in pixels, +x right, +y down.
    pub offset: [f32; 2],
    /// The Gaussian's standard deviation in pixels: half the blur radius.
    pub sigma: f32,
    /// Straight, sRGB-encoded RGBA in `0..=1`.
    pub color: [f32; 4],
}

/// `shadows` in pixels of a `font_size`-pixel font (lengths are fractions of the font size),
/// first on top.
#[must_use]
pub fn shadow_pixels(shadows: &[TextShadow], font_size: f32) -> Vec<ShadowPixels> {
    shadows
        .iter()
        .map(|shadow| ShadowPixels {
            offset: [shadow.offset.x * font_size, shadow.offset.y * font_size],
            sigma: shadow.blur * font_size * 0.5,
            color: [
                shadow.color.red,
                shadow.color.green,
                shadow.color.blue,
                shadow.color.alpha,
            ],
        })
        .collect()
}

/// The pixel font size `style` sets on a picture of `size`, before any fit, as
/// [`text_request`] sets it.
#[must_use]
pub fn font_pixels(style: &TextStyle, size: PictureSize) -> f32 {
    crate::fit::pixels(style.font_size, size.unit())
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod tests;
