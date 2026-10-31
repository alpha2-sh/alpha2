//! The "Stylize" group's effects on the CPU against the GPU.

use alpha2_time::MediaTime;

use super::{Params, check_effect, effect, float, params, size, small_layer};
use crate::builtins::{DROP_SHADOW_ID, SHADOW_OFFSET_ID};
use crate::tests::cpu_parity::TestPicture;
use crate::tests::cpu_parity::{Parity, Tolerance, test_card};
use crate::{
    BlendMode, EffectId, EffectRuntime, GraphLayer, LayerTransform, ParamValue, RenderGraph,
};

/// The times the time-reading effects are checked at: the start, a few
/// minutes in, and far enough that seconds in f32 lose their fraction.
const TICKS: [i64; 3] = [0, 12_345_678, FAR];

/// About 9.2e6 s in, where `time()`, seconds as an `f32`, steps by a whole
/// second. Value noise fed that time lands on a different noise cell on two
/// devices whose `time() * speed` differs in its last bit, so an effect built
/// on it is held there to drawing the same picture twice on the GPU and to the
/// CPU's alpha ([`Parity::check_gpu_determinism`]), not to the CPU's color.
const FAR: i64 = (1_i64 << 40) + 7;

/// [`check_effect`], but at [`FAR`] each case only as
/// [`Parity::check_gpu_determinism`] checks it; `keeps_alpha` says the effect
/// passes its source's alpha through (light leak), where one that moves pixels
/// by its noise (VHS) moves the alpha too.
fn check_noise_effect(
    parity: &Parity,
    picture: &TestPicture,
    effect: &EffectId,
    cases: &[(Params, Tolerance)],
    ticks: i64,
    keeps_alpha: bool,
) {
    if ticks != FAR {
        check_effect(parity, picture, effect, cases, at(ticks));
        return;
    }
    let name = effect.as_str().rsplit('.').next().unwrap_or("effect");
    for (index, (values, _)) in cases.iter().enumerate() {
        for small in [false, true] {
            let name = format!(
                "{name}-{index}{}-at-{ticks}",
                if small { "-small" } else { "" }
            );
            parity.check_gpu_determinism(&name, size(), keeps_alpha, |graph, side| {
                let mut source = graph.add_source(picture.view(side));
                if small {
                    source = small_layer(graph, source)?;
                }
                graph.add_effect(effect.clone(), source, params(values), at(ticks))
            });
        }
    }
}

fn at(ticks: i64) -> EffectRuntime {
    EffectRuntime {
        time: MediaTime::from_ticks(ticks),
        ..EffectRuntime::default()
    }
}

/// [`check_effect`] with the picture first drawn onto the canvas, as a clip
/// is before its effects: a half-float picture, not the 8-bit upload, which
/// a software rasterizer filters in 8 bits and a filter that amplifies
/// differences of its taps turns into many codes.
fn check_on_canvas(
    parity: &Parity,
    picture: &TestPicture,
    effect: &EffectId,
    cases: &[(Params, Tolerance)],
) {
    for (index, (values, tolerance)) in cases.iter().enumerate() {
        for small in [false, true] {
            let name = format!(
                "{}-canvas-{index}{}",
                effect.as_str().rsplit('.').next().unwrap_or("effect"),
                if small { "-small" } else { "" }
            );
            parity.check(&name, size(), *tolerance, |graph, side| {
                let upload = graph.add_source(picture.view(side));
                let mut source = graph.add_composite(vec![GraphLayer {
                    source: upload,
                    visual: LayerTransform {
                        position: [0.5, 0.5],
                        anchor: [0.5, 0.5],
                        scale: [1.0, 1.0],
                        rotation_degrees: 0.0,
                        opacity: 1.0,
                    },
                    blend_mode: BlendMode::Normal,
                }])?;
                if small {
                    source = small_layer(graph, source)?;
                }
                graph.add_effect(
                    effect.clone(),
                    source,
                    params(values),
                    EffectRuntime::default(),
                )
            });
        }
    }
}

#[test]
fn rgb_split_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let split = |mode, amount, angle| {
        vec![
            ("mode", ParamValue::Enum(mode)),
            ("amount", float(amount)),
            ("angle", float(angle)),
        ]
    };
    check_effect(
        &parity,
        &card,
        &effect("rgb-split"),
        &[
            (Vec::new(), Tolerance::Close),
            (split(0, 0.05, 30.0), Tolerance::Close),
            (split(1, 0.03, 0.0), Tolerance::Close),
            (split(0, 0.02, -135.0), Tolerance::Close),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn glitch_matches_at_several_times() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let glitch = |amount, block_size, speed, color_split, seed| {
        vec![
            ("amount", float(amount)),
            ("block_size", float(block_size)),
            ("speed", float(speed)),
            ("color_split", float(color_split)),
            ("seed", float(seed)),
        ]
    };
    for ticks in TICKS {
        check_effect(
            &parity,
            &card,
            &effect("glitch"),
            &[
                (Vec::new(), Tolerance::Unstable),
                (glitch(1.0, 0.1, 30.0, 0.05, 7.0), Tolerance::Unstable),
                (glitch(0.8, 0.005, 0.0, 0.02, 400.0), Tolerance::Unstable),
                (glitch(0.0, 0.5, 60.0, 0.0, 0.0), Tolerance::Unstable),
            ],
            at(ticks),
        );
    }
}

#[test]
fn crt_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let crt = |curvature, scanlines, line_count, mask, vignette| {
        vec![
            ("curvature", float(curvature)),
            ("scanlines", float(scanlines)),
            ("line_count", float(line_count)),
            ("mask", float(mask)),
            ("vignette", float(vignette)),
        ]
    };
    check_effect(
        &parity,
        &card,
        &effect("crt"),
        &[
            (Vec::new(), Tolerance::Edges),
            (crt(0.5, 1.0, 1080.0, 1.0, 1.0), Tolerance::Edges),
            (crt(0.0, 0.6, 50.0, 0.5, 0.0), Tolerance::Edges),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn vhs_matches_at_several_times() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let vhs = |amount, noise, bleed, speed, seed| {
        vec![
            ("amount", float(amount)),
            ("noise", float(noise)),
            ("bleed", float(bleed)),
            ("speed", float(speed)),
            ("seed", float(seed)),
        ]
    };
    for ticks in TICKS {
        check_noise_effect(
            &parity,
            &card,
            &effect("vhs"),
            &[
                (Vec::new(), Tolerance::Unstable),
                (vhs(1.0, 1.0, 1.0, 10.0, 3.0), Tolerance::Unstable),
                (vhs(0.7, 0.3, 0.2, 0.0, 1000.0), Tolerance::Unstable),
                (vhs(0.0, 0.0, 0.0, 1.0, 0.0), Tolerance::Unstable),
            ],
            ticks,
            false,
        );
    }
}

#[test]
fn halftone_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let halftone = |mode, cells, angle| {
        vec![
            ("mode", ParamValue::Enum(mode)),
            ("cells", float(cells)),
            ("angle", float(angle)),
        ]
    };
    // A pixel on a cell's edge takes either cell's color as the GPU's turn rounds, and a
    // dot's radius, the square root of its color, is steep near black.
    check_on_canvas(
        &parity,
        &card,
        &effect("halftone"),
        &[
            (Vec::new(), Tolerance::Unstable),
            (halftone(1, 10.0, 30.0), Tolerance::Unstable),
            (halftone(0, 37.0, -90.0), Tolerance::Unstable),
            (halftone(1, 23.0, 0.0), Tolerance::Unstable),
        ],
    );
}

#[test]
fn emboss_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let emboss = |angle, distance, strength, keep_color| {
        vec![
            ("angle", float(angle)),
            ("distance", float(distance)),
            ("strength", float(strength)),
            ("keep_color", ParamValue::Bool(keep_color)),
        ]
    };
    check_effect(
        &parity,
        &card,
        &effect("emboss"),
        &[
            (Vec::new(), Tolerance::Close),
            (emboss(30.0, 0.01, 5.0, true), Tolerance::Close),
            (emboss(135.0, 0.0, 1.0, false), Tolerance::Close),
            (emboss(-200.0, 0.004, 0.3, true), Tolerance::Close),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn edge_detect_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let edges = |width, strength, invert, colored| {
        vec![
            ("width", float(width)),
            ("strength", float(strength)),
            ("invert", ParamValue::Bool(invert)),
            ("colored", ParamValue::Bool(colored)),
        ]
    };
    check_on_canvas(
        &parity,
        &card,
        &effect("edge-detect"),
        &[
            (Vec::new(), Tolerance::Edges),
            (edges(0.01, 5.0, true, true), Tolerance::Edges),
            (edges(0.0002, 1.0, true, false), Tolerance::Edges),
            (edges(0.005, 2.0, false, true), Tolerance::Edges),
        ],
    );
}

#[test]
fn drop_shadow_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let shadow = |color: [f32; 4], opacity, angle, distance, softness, shadow_only| {
        vec![
            ("color", ParamValue::Color(color)),
            ("opacity", float(opacity)),
            ("angle", float(angle)),
            ("distance", float(distance)),
            ("softness", float(softness)),
            ("shadow_only", ParamValue::Bool(shadow_only)),
        ]
    };
    check_effect(
        &parity,
        &card,
        &EffectId::from_static(DROP_SHADOW_ID, 1),
        &[
            (Vec::new(), Tolerance::Edges),
            (
                shadow([1.0, 0.2, 0.1, 0.5], 1.0, 30.0, 0.2, 0.0, true),
                Tolerance::Edges,
            ),
            (
                shadow([0.1, 0.2, 0.9, 1.0], 0.8, 200.0, 0.05, 0.2, false),
                Tolerance::Edges,
            ),
            (
                shadow([0.0, 0.0, 0.0, 1.0], 0.5, 0.0, 0.0, 0.05, false),
                Tolerance::Edges,
            ),
        ],
        EffectRuntime::default(),
    );
    let offset = |offset: [f32; 2], color: [f32; 4], opacity| {
        vec![
            ("offset", ParamValue::Vec2(offset)),
            ("color", ParamValue::Color(color)),
            ("opacity", float(opacity)),
        ]
    };
    check_effect(
        &parity,
        &card,
        &EffectId::from_static(SHADOW_OFFSET_ID, 1),
        &[
            (Vec::new(), Tolerance::Close),
            (
                offset([0.13, -0.07], [0.3, 0.6, 0.9, 0.8], 1.0),
                Tolerance::Close,
            ),
            (
                offset([-0.6, 0.4], [1.0, 1.0, 1.0, 1.0], 0.3),
                Tolerance::Close,
            ),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn outline_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let outline =
        |color: [f32; 4], width| vec![("color", ParamValue::Color(color)), ("width", float(width))];
    check_effect(
        &parity,
        &card,
        &effect("outline"),
        &[
            (Vec::new(), Tolerance::Edges),
            (outline([0.2, 0.6, 1.0, 0.7], 0.03), Tolerance::Edges),
            (outline([1.0, 0.0, 0.0, 1.0], 0.0), Tolerance::Edges),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn light_leak_matches_at_several_times() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let leak = |color: [f32; 4], intensity, position: [f32; 2], size, speed, seed| {
        vec![
            ("color", ParamValue::Color(color)),
            ("intensity", float(intensity)),
            ("position", ParamValue::Vec2(position)),
            ("size", float(size)),
            ("speed", float(speed)),
            ("seed", float(seed)),
        ]
    };
    for ticks in TICKS {
        check_noise_effect(
            &parity,
            &card,
            &effect("light-leak"),
            &[
                (Vec::new(), Tolerance::Edges),
                (
                    leak([0.3, 0.8, 1.0, 1.0], 2.0, [0.7, 0.6], 0.05, 5.0, 9.0),
                    Tolerance::Edges,
                ),
                (
                    leak([1.0, 0.2, 0.6, 0.5], 1.0, [-0.5, 1.5], 2.0, 0.0, 500.0),
                    Tolerance::Edges,
                ),
            ],
            ticks,
            true,
        );
    }
}

#[test]
fn letterbox_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let bars = |aspect, color: [f32; 4]| {
        vec![
            ("aspect", float(aspect)),
            ("color", ParamValue::Color(color)),
        ]
    };
    check_effect(
        &parity,
        &card,
        &effect("letterbox"),
        &[
            (Vec::new(), Tolerance::Edges),
            (bars(1.0, [0.2, 0.4, 0.6, 1.0]), Tolerance::Edges),
            // The test picture's own aspect: no bars.
            (bars(1.5, [0.0, 0.0, 0.0, 1.0]), Tolerance::Edges),
            (bars(4.0, [1.0, 0.0, 0.0, 0.5]), Tolerance::Edges),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn oil_paint_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    check_effect(
        &parity,
        &card,
        &effect("oil-paint"),
        &[
            (Vec::new(), Tolerance::Edges),
            (vec![("radius", float(0.015))], Tolerance::Edges),
            (vec![("radius", float(0.0))], Tolerance::Edges),
        ],
        EffectRuntime::default(),
    );
}

/// At speed zero the moving effects draw the same picture at every time.
#[test]
fn moving_stylize_effects_hold_still_at_speed_zero() {
    let card = test_card(size(), false);
    let renderer = crate::CpuRenderer::new(1);
    let picture = crate::CpuSource::image(std::sync::Arc::new(
        crate::CpuImage::from_premultiplied_rgba8(size(), &card).expect("picture"),
    ));
    let draw = |name: &str, values: &Params, ticks: i64| {
        let mut graph = RenderGraph::new(size());
        let source = graph.add_source(crate::SourceView::cpu(&picture));
        let source = small_layer(&mut graph, source).expect("layer");
        let output = graph
            .add_effect(effect(name), source, params(values), at(ticks))
            .expect("effect");
        graph.set_output(output).expect("output");
        renderer.execute_graph(&graph).expect("draws").read_rgba8()
    };
    for (name, values) in [
        (
            "glitch",
            vec![("speed", float(0.0)), ("amount", float(1.0))],
        ),
        ("vhs", vec![("speed", float(0.0)), ("noise", float(1.0))]),
        (
            "light-leak",
            vec![("speed", float(0.0)), ("size", float(0.2))],
        ),
    ] {
        let first = draw(name, &values, 0);
        for ticks in [1_000, 870_000, (1_i64 << 40) + 7] {
            assert_eq!(first, draw(name, &values, ticks), "{name} at {ticks}");
        }
        let moving = vec![("speed", float(5.0))];
        assert_ne!(
            draw(name, &moving, 0),
            draw(name, &moving, 870_000),
            "{name} moves at speed 5"
        );
    }
}
