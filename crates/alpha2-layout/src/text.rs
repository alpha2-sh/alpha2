//! Text in a text style: its request to the text host, its identity, and
//! where it lies on a picture.

use alpha2_media::EmojiPolicy;
use alpha2_model::{Rgba, TextAlign, TextStyle};
use alpha2_text::{TextRasterError, TextRasterRequest, TextRasterizer};

use crate::{Baselines, CanvasRect, PictureSize};

/// The largest font, outline, and padding the text host draws, in pixels.
pub const MAX_FONT_PIXELS: f32 = 2_048.0;
/// See [`MAX_FONT_PIXELS`].
pub const MAX_OUTLINE_PIXELS: f32 = 64.0;
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

/// A layout text's raster request as a text store key: FNV-1a over a domain
/// byte and every field of the request. Never zero.
#[must_use]
pub fn layout_text_key(request: &TextRasterRequest) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    // Layout texts take keys apart from captions' and text generators'.
    feed(&[0x4c]);
    feed_request(&mut feed, request);
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
    feed(&request.color);
    feed(&[match request.align {
        alpha2_media::TextAlign::Start => 0,
        alpha2_media::TextAlign::Center => 1,
        alpha2_media::TextAlign::End => 2,
    }]);
    match request.outline {
        Some(outline) => {
            feed(&[1]);
            feed(&outline.color);
            feed(&outline.width.to_bits().to_le_bytes());
        }
        None => feed(&[0]),
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
    let font_size = (style.font_size * size.unit()).clamp(1.0, MAX_FONT_PIXELS);
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
        letter_spacing: style.letter_spacing * font_size,
        font_weight: style.font_weight,
    }
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod tests;
