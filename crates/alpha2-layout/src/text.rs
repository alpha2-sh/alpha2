//! Text in a text style: its request to the text host, its identity, and
//! where it lies on a picture.

use alpha2_media::EmojiPolicy;
use alpha2_model::{Paint, Rgba, Spread, TextAlign, TextShadow, TextStroke, TextStyle};
use alpha2_text::{
    TextFill, TextGradient, TextGradientKind, TextGradientStop, TextRasterError, TextRasterRequest,
    TextRasterizer, TextSpread,
};

use crate::{Baselines, CanvasRect, PictureSize};

/// The largest font, stroke extent, and padding the text host draws, in
/// pixels.
pub const MAX_FONT_PIXELS: f32 = 2_048.0;
/// See [`MAX_FONT_PIXELS`]: what a text's strokes reach together.
pub const MAX_STROKE_PIXELS: f32 = 64.0;
/// See [`MAX_FONT_PIXELS`].
pub const MAX_PADDING_PIXELS: f32 = 256.0;

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
    /// The first and last lines' alphabetic baselines.
    pub baselines: Baselines,
}

/// A layout text's raster request and shadows as a text store key: FNV-1a
/// over a domain byte, every field of the request, and the shadows. Never
/// zero.
#[must_use]
pub fn layout_text_key(request: &TextRasterRequest, shadows: &[ShadowPixels]) -> u64 {
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
    hash.max(1)
}

/// What a request is shaped from, as a shaped-text cache key: equal keys
/// shape alike with the same fonts.
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
    for value in [
        request.font_size,
        request.max_width,
        request.line_height,
        request.letter_spacing,
    ] {
        feed(&value.to_bits().to_le_bytes());
    }
    feed(&request.font_weight.to_le_bytes());
    match &request.fill {
        TextFill::Solid(color) => feed(color),
        TextFill::Gradient(gradient) => feed_text_gradient(feed, gradient),
    }
    feed(&[match request.align {
        alpha2_media::TextAlign::Start => 0,
        alpha2_media::TextAlign::Center => 1,
        alpha2_media::TextAlign::End => 2,
    }]);
    feed_count(feed, request.strokes.len());
    for stroke in &request.strokes {
        feed(&stroke.color);
        feed(&stroke.width.to_bits().to_le_bytes());
    }
    if request.split_background {
        feed(&[0xfd]);
    }
}

/// A list's length as one byte: lists here hold a handful of items, and a
/// list of one feeds the byte the single optional value it replaced fed.
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

/// A paint's identity for a text key: a solid color as its bytes, as text
/// colors always fed, a gradient by every field.
fn feed_paint(feed: &mut impl FnMut(&[u8]), paint: &Paint) {
    match paint {
        Paint::Solid { color } => feed(&rgba8(*color)),
        gradient => feed_text_gradient(feed, &text_gradient(gradient)),
    }
}

/// A text style's strokes and shadows for a text key: one stroke feeds what
/// an outline fed, and no shadows feed nothing.
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

/// A text's identity at a render size: the text, its style, and the size,
/// hashed (FNV-1a). Never zero.
#[must_use]
pub fn text_key(text: &str, style: &TextStyle, size: PictureSize) -> u64 {
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
        style.letter_spacing,
        style.max_width,
        style.position.x,
        style.position.y,
        style.anchor.x,
        style.anchor.y,
    ] {
        feed(&value.to_bits().to_le_bytes());
    }
    feed(&style.font_weight.to_le_bytes());
    feed_paint(&mut feed, &style.fill);
    feed(&[match style.align {
        TextAlign::Start => 0,
        TextAlign::Center => 1,
        TextAlign::End => 2,
    }]);
    feed_look(&mut feed, &style.strokes, &style.shadows);
    match style.background {
        Some(background) => {
            feed(&[1]);
            feed(&rgba8(background.color));
            feed(&background.padding.to_bits().to_le_bytes());
        }
        None => feed(&[0]),
    }
    feed(&size.width().to_le_bytes());
    feed(&size.height().to_le_bytes());
    hash.max(1)
}

/// Where `text` in `style` lies on a canvas of `size` ([`TextBounds`]), laid
/// out with `rasterizer`'s fonts as the engine's text store lays it out and
/// placed as a frame places it, without drawing it. Measured at the size a
/// frame is drawn at, it agrees with that frame to the pixel; text laid out
/// at another size can break its lines elsewhere.
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
    let request = text_request(text, style, size, text_key(text, style, size));
    let measure = rasterizer.measure(&request)?;
    let (width, height) = (size.width() as f32, size.height() as f32);
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
        baselines: Baselines {
            first: (origin[1] + measure.first_baseline) / height,
            last: (origin[1] + measure.last_baseline) / height,
        },
    })
}

/// A straight, sRGB-encoded color as bytes.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn rgba8(color: Rgba) -> [u8; 4] {
    [color.red, color.green, color.blue, color.alpha]
        .map(|value| (value.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// The text host request for `text` in `style` on a picture of `size`, its
/// content version `key`: sizes scaled from canvas fractions to pixels,
/// within what the text host draws.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn text_request(
    text: &str,
    style: &TextStyle,
    size: PictureSize,
    key: u64,
) -> TextRasterRequest {
    let font_size = font_pixels(style, size);
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
    }
}

/// A paint as the text host's fill: a color as bytes, a gradient over the
/// layout box with the same stops, the conic angle in radians.
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

/// `strokes`, their widths fractions of the font size, in pixels of a
/// `font_size`-pixel font: each at least the smallest positive width, and
/// the last shortened so that together they reach at most
/// [`MAX_STROKE_PIXELS`].
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

/// What the engine draws for one text: the text host's raster, and the
/// shadows it composites under the raster's glyphs once they are placed.
#[derive(Clone, Debug, PartialEq)]
pub struct TextDraw {
    /// The raster request.
    pub request: TextRasterRequest,
    /// The shadows, first on top.
    pub shadows: Vec<ShadowPixels>,
}

/// The [`TextDraw`] of `text` in `style` on a picture of `size`, its content
/// version `key`.
#[must_use]
pub fn text_draw(text: &str, style: &TextStyle, size: PictureSize, key: u64) -> TextDraw {
    let request = text_request(text, style, size, key);
    let shadows = shadow_pixels(&style.shadows, request.font_size);
    TextDraw { request, shadows }
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

/// `shadows`, their lengths fractions of the font size, in pixels of a
/// `font_size`-pixel font, in the order the style lists them (first on top).
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

/// The pixel font size `style` draws at on a picture of `size`, as
/// [`text_request`] sets it.
#[must_use]
pub fn font_pixels(style: &TextStyle, size: PictureSize) -> f32 {
    (style.font_size * size.unit()).clamp(1.0, MAX_FONT_PIXELS)
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod tests;
