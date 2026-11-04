use alpha2_model::Rgba;

use super::*;

fn latex(formula: &str, font_size: f32, color: Rgba, display: bool) -> ProviderSource {
    ProviderSource::Latex {
        latex: formula.into(),
        font_size,
        color,
        display,
    }
}

const WHITE: Rgba = Rgba::new(1.0, 1.0, 1.0, 1.0);

fn open(source: &ProviderSource) -> Result<Box<dyn crate::VisualSource>, ExtensionError> {
    Registry::with_defaults().open(source, None, Arc::new(NoResources))
}

fn size(formula: &str, font_size: f32, display: bool) -> (u32, u32) {
    let opened = open(&latex(formula, font_size, WHITE, display)).expect("open");
    (opened.report().width, opened.report().height)
}

#[test]
fn a_formula_is_a_still_image_sized_by_its_font_size() {
    let registry = Registry::with_defaults();
    assert!(registry.supports(&latex("x", 64.0, WHITE, true)));
    let opened = open(&latex(r"\frac{a}{b}", 64.0, WHITE, true)).expect("open");
    let report = opened.report();
    assert_eq!(report.format, Format::Latex);
    assert_eq!(report.timing, Timing::Static);
    assert_eq!(report.diagnostics, Vec::new());
    // `x` is 0.559 em wide in STIX Two Math.
    assert_eq!(size("x", 64.0, true).0, 36);
    let (small, large) = (
        size(r"\sqrt{x^2+1}", 32.0, true),
        size(r"\sqrt{x^2+1}", 64.0, true),
    );
    assert!(large.0.abs_diff(small.0 * 2) <= 2 && large.1.abs_diff(small.1 * 2) <= 2);
    let (display, text) = (
        size(r"\sum_{i=0}^n i", 64.0, true),
        size(r"\sum_{i=0}^n i", 64.0, false),
    );
    assert!(display.1 > text.1, "{display:?} {text:?}");
}

#[test]
fn ink_past_the_formula_box_is_not_cut_off() {
    let mut opened =
        open(&latex(r"\widehat{ABC}+\widetilde{xyz}", 64.0, WHITE, true)).expect("open");
    let (width, height) = (opened.report().width, opened.report().height);
    let frame = opened.render(ms(0), (0, 0)).expect("frame");
    let pixels = frame.pixels.rgba8().expect("rgba8");
    let column = |x: u32| (0..height).map(move |y| pixels[((y * width + x) * 4 + 3) as usize]);
    assert!(
        column(width - 1).all(|alpha| alpha < 255),
        "the last column is only an edge"
    );
    assert!((0..width).any(|x| column(x).any(|alpha| alpha > 0)));
}

#[test]
fn the_color_and_its_alpha_paint_the_ink_and_a_formula_color_wins() {
    let darkest = |formula: &str, color: Rgba| {
        let mut opened = open(&latex(formula, 64.0, color, true)).expect("open");
        let frame = opened.render(ms(0), (0, 0)).expect("frame");
        frame
            .pixels
            .rgba8()
            .expect("rgba8")
            .chunks(4)
            .max_by_key(|pixel| pixel[3])
            .map(|pixel| <[u8; 4]>::try_from(pixel).expect("pixel"))
            .expect("a pixel")
    };
    assert_eq!(
        darkest(r"\blacksquare", Rgba::new(0.0, 1.0, 0.0, 1.0)),
        [0, 255, 0, 255]
    );
    assert_eq!(
        darkest(r"\blacksquare", Rgba::new(0.0, 1.0, 0.0, 0.5)),
        [0, 128, 0, 128]
    );
    assert_eq!(
        darkest(r"\color{red}\blacksquare", Rgba::new(0.0, 1.0, 0.0, 1.0)),
        [255, 0, 0, 255]
    );
}

#[test]
fn formulas_that_do_not_typeset_are_refused_with_the_reason() {
    let refused = |formula: &str| match open(&latex(formula, 64.0, WHITE, true)) {
        Err(ExtensionError::Malformed(reason)) => reason,
        other => panic!("{formula}: {:?}", other.map(|_| ())),
    };
    assert!(refused(r"\frac{1}").contains("missing argument"));
    assert!(refused(r"\nosuchcommand").contains("nosuchcommand"));
    assert!(refused(r"\text{한}").contains("missing glyph"));
    assert!(refused(&"{".repeat(100)).contains("deeper"));
    assert!(matches!(
        open(&latex(
            &"x".repeat(alpha2_model::MAX_LATEX_BYTES + 1),
            64.0,
            WHITE,
            true
        )),
        Err(ExtensionError::Budget(_))
    ));
    assert!(matches!(
        open(&latex(&"x+".repeat(2_048), 64.0, WHITE, true)),
        Err(ExtensionError::Budget(_))
    ));
    assert!(matches!(
        open(&latex("x", f32::NAN, WHITE, true)),
        Err(ExtensionError::Malformed(_))
    ));
}
