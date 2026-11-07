//! Fitting a text's font size to its box, shared by text styles and layout texts.

use alpha2_model::{FONT_SIZE_FIT_STEPS, FontSizeFit};
use alpha2_text::{ShapedText, TextClamp};

use crate::text::MAX_FONT_PIXELS;

/// The font size a text draws at.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct FittedSize {
    /// As a fraction of the picture's shorter side.
    pub font_size: f32,
    /// In pixels, clamped to what the text host draws.
    pub font_pixels: f32,
}

/// The pixel size of a font `font_size` of a picture's shorter side of `unit` pixels.
pub(crate) fn pixels(font_size: f32, unit: f32) -> f32 {
    (font_size * unit).clamp(1.0, MAX_FONT_PIXELS)
}

/// The largest candidate size in `fit`'s range at which `fits` holds, else its `min`; `base`
/// alone without a fit. See [`FontSizeFit`] for the candidates.
///
/// Bisection compares only exact multiples of `1 / FONT_SIZE_FIT_STEPS`, so the sizes tried,
/// and the size found, are the same on every platform.
///
/// # Errors
/// Returns the first error of `fits`.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub(crate) fn fit_font_size<E>(
    base: f32,
    fit: Option<FontSizeFit>,
    unit: f32,
    mut fits: impl FnMut(f32) -> Result<bool, E>,
) -> Result<FittedSize, E> {
    let size = |font_size: f32| FittedSize {
        font_size,
        font_pixels: pixels(font_size, unit),
    };
    let Some(fit) = fit else {
        return Ok(size(base));
    };
    let low = fit.min;
    let high = fit.max.unwrap_or(base).max(low);
    if fits(pixels(high, unit))? {
        return Ok(size(high));
    }
    if low >= high || !fits(pixels(low, unit))? {
        return Ok(size(low));
    }
    // Candidates are `low`, the steps strictly between, then `high`: `low` fits, `high` not.
    let first = (low * FONT_SIZE_FIT_STEPS).floor() as i64 + 1;
    let last = (high * FONT_SIZE_FIT_STEPS).ceil() as i64 - 1;
    let candidate = |index: i64| {
        if index == 0 {
            low
        } else {
            (first + index - 1) as f32 / FONT_SIZE_FIT_STEPS
        }
    };
    let (mut fitting, mut over) = (0_i64, (last - first + 1).max(0) + 1);
    while over - fitting > 1 {
        let middle = fitting + (over - fitting) / 2;
        if fits(pixels(candidate(middle), unit))? {
            fitting = middle;
        } else {
            over = middle;
        }
    }
    Ok(size(candidate(fitting)))
}

/// Lines of `shaped` in a box `max_width` by `max_height` pixels (non-positive for none).
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct BoxedLines {
    /// The lines drawn: cut at the clamp point when `clamp`.
    pub lines: alpha2_text::LineMetrics,
    /// Whether the clamp discarded lines.
    pub truncated: bool,
    /// Whether the drawn lines split a word or are taller than the box.
    pub overflow: bool,
    /// Whether every line fits the box uncut: within `max_lines`, the height and the width.
    pub fits: bool,
}

/// How `shaped` lies in its box, cut by `clamp` when it has one. `max_lines` also counts for
/// [`BoxedLines::fits`] when there is no clamp.
pub(crate) fn boxed_lines(
    shaped: &ShapedText,
    max_width: f32,
    max_height: f32,
    max_lines: Option<u32>,
    clamp: bool,
) -> BoxedLines {
    let limit = shaped.line_limit(max_lines, max_height);
    let cut = shaped.lines_within(max_width, limit);
    let drawn = if clamp {
        cut
    } else {
        shaped.lines_within(max_width, None)
    };
    let taller = |height: f32| max_height > 0.0 && height > max_height;
    BoxedLines {
        lines: drawn.lines,
        truncated: drawn.truncated,
        overflow: drawn.splits_word || taller(drawn.lines.height),
        fits: !cut.truncated && !cut.splits_word && !taller(cut.lines.height),
    }
}

/// A text host clamp for `clamp` in a box `max_height` pixels tall (non-positive for none).
pub(crate) fn text_clamp(clamp: &alpha2_model::LineClamp, max_height: f32) -> TextClamp {
    TextClamp {
        max_lines: clamp.max_lines,
        max_height,
        ellipsis: clamp.block_ellipsis.text().map(str::to_owned),
    }
}

#[cfg(test)]
#[path = "fit_tests.rs"]
mod tests;
