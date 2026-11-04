//! LaTeX math typeset by latex-rust with its bundled STIX Two Math font, drawn as SVG.

use std::sync::LazyLock;

use alpha2_model::{MAX_LATEX_BYTES, MAX_LATEX_FONT_SIZE, Rgba};
use latex_rust::{Color, Dim, MathFont, MathStyle, SvgOptions};

use crate::api::{ExtensionError, Format, NoResources, VisualSource};
use crate::svg::{open_document, unit_byte};

static FONT: LazyLock<Result<MathFont, String>> =
    LazyLock::new(|| MathFont::stix_two_math().map_err(|error| error.to_string()));

/// Typesets `latex` at `font_size` pixels per em. The canvas covers the formula's box and any ink
/// past it, in whole pixels.
///
/// # Errors
/// [`ExtensionError::Malformed`] for settings out of range or a formula that does not typeset
/// (including a glyph STIX Two Math lacks), or [`ExtensionError::Budget`] for one too large.
pub fn open_latex(
    latex: &str,
    font_size: f32,
    color: Rgba,
    display: bool,
) -> Result<Box<dyn VisualSource>, ExtensionError> {
    if latex.len() > MAX_LATEX_BYTES {
        return Err(ExtensionError::Budget(format!(
            "the formula is {} bytes, more than the {MAX_LATEX_BYTES} a source holds",
            latex.len()
        )));
    }
    if !(font_size > 0.0 && font_size <= MAX_LATEX_FONT_SIZE) {
        return Err(ExtensionError::Malformed(format!(
            "the font size {font_size} is not above 0 and at most {MAX_LATEX_FONT_SIZE}"
        )));
    }
    let malformed = |error: &dyn std::fmt::Display| ExtensionError::Malformed(error.to_string());
    let font = FONT.as_ref().map_err(|error| malformed(error))?;
    let formula = latex_rust::parse(latex).map_err(|error| malformed(&error))?;
    let style = if display {
        MathStyle::Display
    } else {
        MathStyle::Text
    };
    // latex-rust sizes the SVG in points, which usvg reads at 96 dpi (4/3 px each).
    let em = &Dim::from_ieee32_bits(font_size.to_bits()) * &Dim::ratio(3, 4);
    let laid_out = latex_rust::layout_with_em_size_pt(&formula, font, style, &em)
        .map_err(|error| malformed(&error))?;
    let [red, green, blue] = [color.red, color.green, color.blue].map(unit_byte);
    let svg = latex_rust::render_svg(
        &laid_out,
        font,
        &SvgOptions {
            font_size_pt: em,
            color: Color::rgb(red, green, blue),
            display,
        },
    )
    .map_err(|error| malformed(&error))?;
    let mut source = open_document(svg.as_bytes(), &NoResources)?
        .including_ink()?
        .with_opacity(color.alpha);
    source.report_mut().format = Format::Latex;
    Ok(Box::new(source))
}
