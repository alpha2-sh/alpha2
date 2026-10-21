use super::*;

const RED: Color = [1.0, 0.0, 0.0, 1.0];
const GREEN: Color = [0.0, 1.0, 0.0, 1.0];
const BLUE: Color = [0.0, 0.0, 1.0, 1.0];
const CLEAR: Color = [0.0; 4];

fn close(actual: Color, expected: Color) -> bool {
    actual
        .iter()
        .zip(expected)
        .all(|(actual, expected)| (actual - expected).abs() < 1e-5)
}

#[test]
fn porter_duff_operators_weigh_each_side_by_the_other_alpha() {
    let half_red = RED.map(|channel| channel * 0.5);
    for (mode, expected) in [
        (CompositeMode::Clear, CLEAR),
        (CompositeMode::Source, half_red),
        (CompositeMode::Destination, BLUE),
        (CompositeMode::SourceOver, [0.5, 0.0, 0.5, 1.0]),
        (CompositeMode::DestinationOver, BLUE),
        (CompositeMode::SourceIn, half_red),
        (CompositeMode::DestinationIn, [0.0, 0.0, 0.5, 0.5]),
        (CompositeMode::SourceOut, CLEAR),
        (CompositeMode::DestinationOut, [0.0, 0.0, 0.5, 0.5]),
        (CompositeMode::SourceAtop, [0.5, 0.0, 0.5, 1.0]),
        (CompositeMode::DestinationAtop, [0.0, 0.0, 0.5, 0.5]),
        (CompositeMode::Xor, [0.0, 0.0, 0.5, 0.5]),
        (CompositeMode::Plus, [0.5, 0.0, 1.0, 1.0]),
    ] {
        let actual = composite(mode, half_red, BLUE);
        assert!(close(actual, expected), "{mode:?}: {actual:?}");
    }
}

#[test]
fn blend_modes_mix_colors_where_both_have_ink() {
    let grey = [0.5, 0.5, 0.5, 1.0];
    for (mode, source, backdrop, expected) in [
        (CompositeMode::Multiply, RED, GREEN, [0.0, 0.0, 0.0, 1.0]),
        (CompositeMode::Screen, RED, GREEN, [1.0, 1.0, 0.0, 1.0]),
        (CompositeMode::Darken, grey, RED, [0.5, 0.0, 0.0, 1.0]),
        (CompositeMode::Lighten, grey, RED, [1.0, 0.5, 0.5, 1.0]),
        (CompositeMode::Difference, grey, RED, [0.5, 0.5, 0.5, 1.0]),
        (CompositeMode::Exclusion, grey, RED, [0.5, 0.5, 0.5, 1.0]),
        (CompositeMode::HardLight, RED, grey, [1.0, 0.0, 0.0, 1.0]),
        (CompositeMode::Overlay, grey, RED, [1.0, 0.0, 0.0, 1.0]),
        // Grey has no saturation to give, and the luminosity of red is 0.3; red lifted to grey's
        // 0.5 is (1.2, 0.2, 0.2), brought back into gamut about its luminosity.
        (CompositeMode::Saturation, grey, RED, [0.3, 0.3, 0.3, 1.0]),
        (
            CompositeMode::Luminosity,
            grey,
            RED,
            [1.0, 2.0 / 7.0, 2.0 / 7.0, 1.0],
        ),
        (
            CompositeMode::Color,
            RED,
            grey,
            [1.0, 2.0 / 7.0, 2.0 / 7.0, 1.0],
        ),
    ] {
        let actual = composite(mode, source, backdrop);
        assert!(close(actual, expected), "{mode:?}: {actual:?}");
    }
    // Over nothing, a blend mode is the source over.
    let actual = composite(CompositeMode::Multiply, RED, CLEAR);
    assert!(close(actual, RED), "{actual:?}");
}

fn line(extend: GradientExtend) -> ColorLine {
    ColorLine {
        stops: vec![(0.25, RED), (0.75, BLUE)],
        extend,
    }
}

#[test]
fn a_color_line_extends_past_its_stops_as_asked() {
    let mixed = |red: f32| [red, 0.0, 1.0 - red, 1.0];
    for (extend, offset, expected) in [
        (GradientExtend::Pad, 0.0, RED),
        (GradientExtend::Pad, 0.5, mixed(0.5)),
        (GradientExtend::Pad, 1.0, BLUE),
        // 0.8 is 1.1 spans along: 0.1 again, or 0.9 back.
        (GradientExtend::Repeat, 0.8, mixed(0.9)),
        (GradientExtend::Reflect, 0.8, mixed(0.1)),
        (GradientExtend::Repeat, 0.2, mixed(0.1)),
        (GradientExtend::Reflect, 0.2, mixed(0.9)),
    ] {
        let actual = line(extend).at(offset);
        assert!(
            close(actual, expected),
            "{extend:?} at {offset}: {actual:?}"
        );
    }
    // Stops at one offset are a hard edge, the later stop after it.
    let edge = ColorLine::of(std::iter::empty(), GradientExtend::Pad);
    assert_eq!(edge.at(0.5), CLEAR);
    let edge = ColorLine {
        stops: vec![(0.0, RED), (0.5, RED), (0.5, BLUE), (1.0, BLUE)],
        extend: GradientExtend::Pad,
    };
    assert_eq!(edge.at(0.49), RED);
    assert_eq!(edge.at(0.5), BLUE);
}

#[test]
fn gradients_run_where_the_table_places_them() {
    let pad = || line(GradientExtend::Pad);
    // p2 straight above p0 makes the color line horizontal, wherever p1 is above it.
    let linear = Fill::Linear {
        start: [0.0, 0.0],
        end: [100.0, 0.0],
        line: pad(),
    };
    assert!(close(linear.at([50.0, 77.0]), [0.5, 0.0, 0.5, 1.0]));
    let radial = Fill::Radial {
        start: [0.0, 0.0],
        start_radius: 0.0,
        end: [0.0, 0.0],
        end_radius: 100.0,
        line: pad(),
    };
    assert!(close(radial.at([0.0, -50.0]), [0.5, 0.0, 0.5, 1.0]));
    // Counter-clockwise from the x axis, y up: a quarter turn of a full sweep is a quarter along.
    let sweep = Fill::Sweep {
        center: [0.0, 0.0],
        start_angle: 0.0,
        end_angle: 360.0,
        line: pad(),
    };
    assert!(close(sweep.at([0.0, 10.0]), RED));
    assert!(close(sweep.at([-10.0, 0.0]), [0.5, 0.0, 0.5, 1.0]));
    assert!(close(sweep.at([0.0, -10.0]), BLUE));
}

#[test]
fn a_two_point_radial_takes_the_larger_circle_through_a_point() {
    // Circles from (0, 0) radius 10 to (100, 0) radius 10: a point on the second is at offset 1.
    assert_eq!(
        radial([0.0, 0.0], 10.0, [100.0, 0.0], 10.0, [100.0, 10.0]),
        Some(1.0)
    );
    // A cone from a point: nothing lies behind its apex.
    assert_eq!(
        radial([0.0, 0.0], 0.0, [100.0, 0.0], 50.0, [-10.0, 0.0]),
        None
    );
}

#[test]
fn a_solid_paint_is_premultiplied_and_a_transform_undoes() {
    let fill = Fill::of(&Paint::Solid(RgbaColor::new(255, 0, 0, 255)));
    assert!(matches!(fill, Fill::Solid(color) if color == RED));
    let transform = Transform {
        a: 2.0,
        b: 1.0,
        c: -1.0,
        d: 3.0,
        e: 5.0,
        f: -7.0,
    };
    let inverse = invert(transform).expect("invertible");
    let back = apply(inverse, apply(transform, [11.0, 13.0]));
    assert!((back[0] - 11.0).abs() < 1e-4 && (back[1] - 13.0).abs() < 1e-4);
}
