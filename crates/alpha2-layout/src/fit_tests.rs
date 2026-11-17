use super::*;

const UNIT: f32 = 1_000.0;

/// Fits with a font that fits up to `largest` pixels, counting the sizes tried.
fn fit_below(largest: f32, base: f32, fit: Option<FontSizeFit>) -> (FittedSize, Vec<f32>) {
    let mut tried = Vec::new();
    let fitted = fit_font_size::<()>(base, fit, UNIT, |pixels| {
        tried.push(pixels);
        Ok(pixels <= largest)
    })
    .expect("no error");
    (fitted, tried)
}

#[test]
fn without_a_fit_the_base_size_draws_and_nothing_is_tried() {
    let (fitted, tried) = fit_below(0.0, 0.05, None);
    assert_eq!(fitted.font_size, 0.05);
    assert_eq!(fitted.font_pixels, 50.0);
    assert_eq!(tried, [] as [f32; 0]);
}

#[test]
fn text_that_fits_at_its_largest_size_draws_at_it_exactly() {
    let shrink = Some(FontSizeFit {
        min: 0.01,
        max: None,
    });
    let (fitted, tried) = fit_below(1_000.0, 0.05, shrink);
    assert_eq!((fitted.font_size, tried.len()), (0.05, 1));
    let grow = Some(FontSizeFit {
        min: 0.01,
        max: Some(0.3),
    });
    assert_eq!(fit_below(1_000.0, 0.05, grow).0.font_size, 0.3);
}

#[test]
fn the_largest_fitting_step_is_found_by_bisection() {
    let fit = Some(FontSizeFit {
        min: 0.01,
        max: None,
    });
    for largest in [10.5_f32, 17.0, 33.333, 49.99] {
        let (fitted, tried) = fit_below(largest, 0.05, fit);
        let step = fitted.font_size * FONT_SIZE_FIT_STEPS;
        assert_eq!(step, step.round(), "{largest}: a whole step");
        assert!(fitted.font_pixels <= largest, "{largest}");
        assert!(
            (fitted.font_size + 1.0 / FONT_SIZE_FIT_STEPS) * UNIT > largest,
            "{largest}: the next step does not fit"
        );
        // The ends, then at most log2 of the steps between them.
        assert!(tried.len() <= 2 + 9, "{largest}: {} sizes", tried.len());
    }
}

#[test]
fn text_that_fits_at_no_size_draws_at_the_minimum() {
    let fit = Some(FontSizeFit {
        min: 0.02,
        max: None,
    });
    let (fitted, tried) = fit_below(5.0, 0.05, fit);
    assert_eq!(fitted.font_size, 0.02);
    assert_eq!(tried, [50.0, 20.0]);
}

#[test]
fn a_minimum_above_the_maximum_wins() {
    let fit = Some(FontSizeFit {
        min: 0.08,
        max: None,
    });
    let (fitted, _) = fit_below(1_000.0, 0.05, fit);
    assert_eq!(fitted.font_size, 0.08);
    let (fitted, _) = fit_below(1.0, 0.05, fit);
    assert_eq!(fitted.font_size, 0.08);
}

#[test]
fn an_error_while_fitting_is_returned() {
    let fit = Some(FontSizeFit {
        min: 0.01,
        max: None,
    });
    assert_eq!(
        fit_font_size(0.05, fit, UNIT, |_| Err::<bool, _>("no fonts")),
        Err("no fonts")
    );
}

#[test]
fn sizes_are_clamped_to_what_the_text_host_draws() {
    let fit = Some(FontSizeFit {
        min: 0.0001,
        max: Some(1.0),
    });
    let (fitted, tried) = fit_below(1.0e9, 0.05, fit);
    assert_eq!(fitted.font_size, 1.0);
    assert_eq!(fitted.font_pixels, 1_000.0);
    let tiny = fit_font_size::<()>(0.05, fit, 10.0, |pixels| Ok(pixels <= 0.5)).expect("fits");
    assert_eq!((tiny.font_size, tiny.font_pixels), (0.0001, 1.0));
    assert!(
        tried
            .iter()
            .all(|pixels| (1.0..=MAX_FONT_PIXELS).contains(pixels))
    );
}
