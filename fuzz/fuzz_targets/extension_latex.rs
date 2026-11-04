#![no_main]

//! LaTeX formulas typeset, opened, and drawn at a few sizes; refusals are fine, panics are not.

use alpha2_extensions::open_latex;
use alpha2_model::Rgba;
use alpha2_time::MediaTime;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let Some((&style, formula)) = bytes.split_first() else {
        return;
    };
    let Ok(formula) = std::str::from_utf8(formula) else {
        return;
    };
    let color = Rgba::new(1.0, 0.5, 0.0, f32::from(style) / 255.0);
    let Ok(mut source) = open_latex(formula, 8.0 + f32::from(style & 63), color, style & 1 == 0)
    else {
        return;
    };
    let (width, height) = (source.report().width, source.report().height);
    for fit in [(0, 0), (37, 23)] {
        if let Ok(frame) = source.render(MediaTime::from_ticks(0), fit) {
            if fit == (0, 0) {
                assert_eq!((frame.width, frame.height), (width, height));
            }
            assert!(frame.width <= fit.0.max(width) && frame.height <= fit.1.max(height));
        }
    }
});
