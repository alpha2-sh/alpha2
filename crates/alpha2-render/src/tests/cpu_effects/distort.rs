//! The "Distort" group's effects on the CPU against the GPU.

use alpha2_time::MediaTime;

use super::{
    EffectInputs, EffectParams, EffectResources, EffectRuntime, ParamValue, Params, Parity,
    Tolerance, check_effect, coverage_picture, effect, float, params, size, small_layer, test_card,
};

fn vec2(value: [f32; 2]) -> ParamValue {
    ParamValue::Vec2(value)
}

fn choice(value: u32) -> ParamValue {
    ParamValue::Enum(value)
}

/// `cases` of `name` on the test card at the default time.
fn check_card(name: &str, cases: &[(Params, Tolerance)]) {
    check_card_at(name, cases, 0);
}

fn check_card_at(name: &str, cases: &[(Params, Tolerance)], ticks: i64) {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    check_effect(
        &parity,
        &card,
        &effect(name),
        cases,
        EffectRuntime {
            time: MediaTime::from_ticks(ticks),
            ..EffectRuntime::default()
        },
    );
}

/// Times to check an effect that reads the time at: zero, a few seconds,
/// and about 25 hours.
const TIMES: [i64; 3] = [0, 870_000, 10_800_000_123];

#[test]
fn pixelate_hexagons_match() {
    let hexagon = |blocks| vec![("blocks", float(blocks)), ("shape", choice(1))];
    check_card(
        "pixelate",
        &[
            (hexagon(48.0), Tolerance::Unstable),
            (hexagon(2.0), Tolerance::Unstable),
            (hexagon(13.7), Tolerance::Unstable),
            (hexagon(400.0), Tolerance::Unstable),
        ],
    );
}

#[test]
fn swirl_matches() {
    let swirl = |center, radius, angle, edge| {
        vec![
            ("center", vec2(center)),
            ("radius", float(radius)),
            ("angle", float(angle)),
            ("edge", choice(edge)),
        ]
    };
    check_card(
        "swirl",
        &[
            (Vec::new(), Tolerance::Edges),
            (swirl([0.3, 0.6], 0.8, -540.0, 1), Tolerance::Edges),
            (swirl([0.5, 0.5], 2.0, 1080.0, 2), Tolerance::Edges),
            (swirl([0.7, 0.4], 0.0, 90.0, 3), Tolerance::Edges),
        ],
    );
}

#[test]
fn bulge_matches() {
    let bulge = |center, radius, strength, edge| {
        vec![
            ("center", vec2(center)),
            ("radius", float(radius)),
            ("strength", float(strength)),
            ("edge", choice(edge)),
        ]
    };
    check_card(
        "bulge",
        &[
            (Vec::new(), Tolerance::Edges),
            (bulge([0.4, 0.6], 0.7, -1.0, 1), Tolerance::Edges),
            (bulge([0.5, 0.5], 2.0, 1.0, 0), Tolerance::Edges),
            (bulge([0.2, 0.3], 0.3, 0.6, 3), Tolerance::Edges),
        ],
    );
}

#[test]
fn wave_warp_matches_at_several_times() {
    let wave = |amplitude, wavelength, angle, speed, phase, edge| {
        vec![
            ("amplitude", float(amplitude)),
            ("wavelength", float(wavelength)),
            ("angle", float(angle)),
            ("speed", float(speed)),
            ("phase", float(phase)),
            ("edge", choice(edge)),
        ]
    };
    for ticks in TIMES {
        check_card_at(
            "wave-warp",
            &[
                (Vec::new(), Tolerance::Edges),
                (wave(0.05, 0.3, 30.0, 1.0, 45.0, 0), Tolerance::Edges),
                (wave(0.2, 2.0, -120.0, -7.5, -200.0, 2), Tolerance::Edges),
                (wave(0.02, 0.05, 90.0, 0.35, 0.0, 3), Tolerance::Edges),
            ],
            ticks,
        );
    }
}

#[test]
fn ripple_matches_at_several_times() {
    let ripple = |center, amplitude, wavelength, speed, decay, edge| {
        vec![
            ("center", vec2(center)),
            ("amplitude", float(amplitude)),
            ("wavelength", float(wavelength)),
            ("speed", float(speed)),
            ("decay", float(decay)),
            ("edge", choice(edge)),
        ]
    };
    for ticks in TIMES {
        check_card_at(
            "ripple",
            &[
                (Vec::new(), Tolerance::Edges),
                (
                    ripple([0.3, 0.6], 0.05, 0.2, -3.0, 0.0, 0),
                    Tolerance::Edges,
                ),
                (
                    ripple([0.5, 0.5], 0.1, 1.0, 10.0, 10.0, 2),
                    Tolerance::Edges,
                ),
                (
                    ripple([0.8, 0.2], 0.02, 0.02, 0.0, 1.0, 3),
                    Tolerance::Edges,
                ),
            ],
            ticks,
        );
    }
}

#[test]
fn lens_distortion_matches() {
    let lens = |k1, k2, zoom, edge| {
        vec![
            ("k1", float(k1)),
            ("k2", float(k2)),
            ("zoom", float(zoom)),
            ("edge", choice(edge)),
        ]
    };
    check_card(
        "lens-distortion",
        &[
            (Vec::new(), Tolerance::Edges),
            (lens(-0.6, 0.3, 1.2, 1), Tolerance::Edges),
            (lens(1.0, 1.0, 0.5, 2), Tolerance::Edges),
            (lens(-1.0, -1.0, 2.0, 3), Tolerance::Edges),
        ],
    );
}

#[test]
fn polar_coordinates_match() {
    let polar = |mode, amount, edge| {
        vec![
            ("mode", choice(mode)),
            ("amount", float(amount)),
            ("edge", choice(edge)),
        ]
    };
    check_card(
        "polar-coordinates",
        &[
            (Vec::new(), Tolerance::Edges),
            (polar(0, 0.6, 1), Tolerance::Edges),
            (polar(1, 1.0, 0), Tolerance::Edges),
            (polar(1, 0.3, 2), Tolerance::Edges),
            (polar(0, 1.0, 3), Tolerance::Edges),
        ],
    );
}

#[test]
fn offset_matches() {
    // 96 x 64 pixels: a quarter and an eighth are whole pixels.
    let offset = |shift, edge| vec![("shift", vec2(shift)), ("edge", choice(edge))];
    check_card(
        "offset",
        &[
            (Vec::new(), Tolerance::Close),
            (offset([0.25, -0.125], 2), Tolerance::Close),
            (offset([-0.5, 0.25], 3), Tolerance::Close),
            (offset([0.125, 0.5], 0), Tolerance::Close),
            (offset([0.3, -0.17], 2), Tolerance::Edges),
            (offset([-0.77, 0.41], 0), Tolerance::Edges),
            (offset([0.05, 0.93], 1), Tolerance::Edges),
        ],
    );
}

#[test]
fn corner_pin_matches() {
    let pin = |top_left, top_right, bottom_right, bottom_left| {
        vec![
            ("top_left", vec2(top_left)),
            ("top_right", vec2(top_right)),
            ("bottom_right", vec2(bottom_right)),
            ("bottom_left", vec2(bottom_left)),
        ]
    };
    check_card(
        "corner-pin",
        &[
            (Vec::new(), Tolerance::Close),
            // A trapezoid.
            (
                pin([0.2, 0.1], [0.8, 0.0], [1.0, 1.0], [0.0, 0.9]),
                Tolerance::Edges,
            ),
            // A parallelogram: no perspective.
            (
                pin([0.1, 0.2], [0.7, 0.1], [0.9, 0.8], [0.3, 0.9]),
                Tolerance::Edges,
            ),
            // Strong perspective, its horizon inside the picture.
            (
                pin([-1.0, -0.5], [2.0, 0.3], [0.6, 0.9], [0.4, 0.8]),
                Tolerance::Edges,
            ),
            // Twisted: two corners swapped.
            (
                pin([0.0, 0.0], [1.0, 1.0], [1.0, 0.0], [0.0, 1.0]),
                Tolerance::Edges,
            ),
            // Degenerate: every corner on one line.
            (
                pin([0.0, 0.0], [0.5, 0.5], [1.0, 1.0], [0.25, 0.25]),
                Tolerance::Close,
            ),
        ],
    );
}

#[test]
fn shake_matches_at_several_times() {
    let shake = |amplitude, frequency, rotation, zoom, seed, edge| {
        vec![
            ("amplitude", float(amplitude)),
            ("frequency", float(frequency)),
            ("rotation", float(rotation)),
            ("zoom", float(zoom)),
            ("seed", float(seed)),
            ("edge", choice(edge)),
        ]
    };
    for ticks in TIMES {
        check_card_at(
            "shake",
            &[
                (Vec::new(), Tolerance::Edges),
                (shake(0.2, 30.0, 30.0, 1.0, 17.0, 0), Tolerance::Edges),
                (shake(0.05, 0.1, 10.0, 2.0, 999.0, 3), Tolerance::Edges),
            ],
            ticks,
        );
    }
}

#[test]
fn turbulent_displace_matches_at_several_times() {
    let turbulence = |amount, size, complexity, evolution, seed, edge| {
        vec![
            ("amount", float(amount)),
            ("size", float(size)),
            ("complexity", float(complexity)),
            ("evolution", float(evolution)),
            ("seed", float(seed)),
            ("edge", choice(edge)),
        ]
    };
    for ticks in TIMES {
        check_card_at(
            "turbulent-displace",
            &[
                (Vec::new(), Tolerance::Edges),
                (turbulence(0.2, 0.05, 4.0, 5.0, 3.0, 0), Tolerance::Edges),
                (turbulence(0.05, 2.0, 1.0, 0.37, 640.0, 3), Tolerance::Edges),
                (turbulence(0.1, 0.3, 2.0, 1.0, 1000.0, 2), Tolerance::Edges),
            ],
            ticks,
        );
    }
}

#[test]
fn displacement_map_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let map = parity.picture(size(), &coverage_picture(size(), 3));
    let displace = |x, y, amount_x, amount_y, edge| {
        vec![
            ("channel_x", choice(x)),
            ("channel_y", choice(y)),
            ("amount_x", float(amount_x)),
            ("amount_y", float(amount_y)),
            ("edge", choice(edge)),
        ]
    };
    let cases = [
        Vec::new(),
        displace(4, 3, 0.2, -0.2, 0),
        displace(2, 0, -0.1, 0.15, 2),
        displace(1, 4, 0.05, 0.05, 3),
    ];
    for (index, values) in cases.iter().enumerate() {
        for small in [false, true] {
            let name = format!(
                "displacement-map-{index}{}",
                if small { "-small" } else { "" }
            );
            parity.check(&name, size(), Tolerance::Edges, |graph, side| {
                // The map is never composited: a displacement magnifies its
                // rounding, one 16-bit float step a fraction of a pixel.
                let mut source = graph.add_source(card.view(side));
                let displacement = graph.add_source(map.view(side));
                if small {
                    source = small_layer(graph, source)?;
                }
                graph.add_effect_with_inputs(
                    effect("displacement-map"),
                    source,
                    EffectInputs::new().with("map", displacement),
                    params(values),
                    EffectResources::new(),
                    EffectRuntime::default(),
                )
            });
        }
    }
}

#[test]
fn kaleidoscope_matches() {
    let kaleidoscope = |segments, angle, center, edge| {
        vec![
            ("segments", float(segments)),
            ("angle", float(angle)),
            ("center", vec2(center)),
            ("edge", choice(edge)),
        ]
    };
    check_card(
        "kaleidoscope",
        &[
            (Vec::new(), Tolerance::Edges),
            (kaleidoscope(2.0, -90.0, [0.3, 0.6], 0), Tolerance::Edges),
            (kaleidoscope(5.4, 200.0, [0.5, 0.5], 1), Tolerance::Edges),
            (kaleidoscope(32.0, 15.0, [0.8, 0.1], 2), Tolerance::Edges),
        ],
    );
}

#[test]
fn tile_matches() {
    let tile = |tiles, offset, mirror| {
        vec![
            ("tiles", vec2(tiles)),
            ("offset", vec2(offset)),
            ("mirror", ParamValue::Bool(mirror)),
        ]
    };
    check_card(
        "tile",
        &[
            (Vec::new(), Tolerance::Edges),
            (tile([3.0, 2.0], [0.1, -0.2], true), Tolerance::Edges),
            (tile([0.5, 0.1], [0.0, 0.0], false), Tolerance::Edges),
            (tile([32.0, 7.3], [-1.0, 1.0], true), Tolerance::Edges),
        ],
    );
}

/// The effects that read the time stand still where their motion is off.
#[test]
fn distortions_stand_still_without_motion() {
    let draw = |name: &str, values: &Params, ticks: i64| {
        let renderer = crate::CpuRenderer::new(1);
        let card = test_card(size(), false);
        let picture = crate::CpuSource::image(std::sync::Arc::new(
            crate::CpuImage::from_premultiplied_rgba8(size(), &card).expect("picture"),
        ));
        let mut graph = crate::RenderGraph::new(size());
        let source = graph.add_source(crate::SourceView::cpu(&picture));
        let output = graph
            .add_effect(
                effect(name),
                source,
                params(values),
                EffectRuntime {
                    time: MediaTime::from_ticks(ticks),
                    ..EffectRuntime::default()
                },
            )
            .expect("effect");
        graph.set_output(output).expect("output");
        renderer.execute_graph(&graph).expect("draws").read_rgba8()
    };
    for (name, values) in [
        ("wave-warp", vec![("amplitude", float(0.05))]),
        ("ripple", vec![("speed", float(0.0))]),
        ("turbulent-displace", Vec::new()),
    ] {
        let first = draw(name, &values, 0);
        assert_ne!(first, test_card(size(), false), "{name} moves pixels");
        for ticks in TIMES {
            assert_eq!(draw(name, &values, ticks), first, "{name} at {ticks}");
        }
    }
}

/// The corner pin's twisted, degenerate and far-behind quads leave no NaN
/// and no value outside premultiplied range.
#[test]
fn corner_pin_stays_finite_on_any_quad() {
    let renderer = crate::CpuRenderer::new(1);
    let card = test_card(size(), false);
    let picture = crate::CpuSource::image(std::sync::Arc::new(
        crate::CpuImage::from_premultiplied_rgba8(size(), &card).expect("picture"),
    ));
    let quads: [[[f32; 2]; 4]; 5] = [
        [[0.0, 0.0], [1.0, 1.0], [1.0, 0.0], [0.0, 1.0]],
        [[0.5, 0.5]; 4],
        [[0.0, 0.0], [1.0, 0.0], [2.0, 0.0], [-1.0, 0.0]],
        [[2.0, 2.0], [-1.0, 2.0], [-1.0, -1.0], [2.0, -1.0]],
        [[0.0, 0.0], [1.0, 0.0], [0.5, 1.0e-9], [0.5, 1.0e-9]],
    ];
    for corners in quads {
        let mut graph = crate::RenderGraph::new(size());
        let source = graph.add_source(crate::SourceView::cpu(&picture));
        let values = EffectParams::new()
            .with("top_left", vec2(corners[0]))
            .with("top_right", vec2(corners[1]))
            .with("bottom_right", vec2(corners[2]))
            .with("bottom_left", vec2(corners[3]));
        let output = graph
            .add_effect(
                effect("corner-pin"),
                source,
                values,
                EffectRuntime::default(),
            )
            .expect("effect");
        graph.set_output(output).expect("output");
        let image = renderer.execute_graph(&graph).expect("draws");
        for y in 0..size().height {
            for x in 0..size().width {
                let [r, g, b, a] = image.texel(x, y);
                assert!(
                    (0.0..=1.0).contains(&a) && [r, g, b].iter().all(|c| (0.0..=a).contains(c)),
                    "{corners:?} at {x}, {y}: {:?}",
                    [r, g, b, a]
                );
            }
        }
    }
}
