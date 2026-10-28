//! The built-in transitions on the CPU against the GPU: every transition at
//! five progress values, both ways between two pictures, with its default
//! parameters and with others.

mod classic;
mod glitch;
mod shapes;
mod stylized;
mod three_d;
mod wipes;
mod zoom_blur;

use super::cpu_parity::{Parity, Side, Tolerance, test_card};
use crate::{EffectId, EffectParams, EffectRuntime, ParamValue, RenderSize};

fn size() -> RenderSize {
    RenderSize::new(96, 64).expect("size")
}

/// A second picture unlike `test_card`: diagonal colour bands under a
/// vertical ramp, a translucent corner and a fully transparent patch,
/// premultiplied.
fn band_card(size: RenderSize) -> Vec<u8> {
    let palette = [
        [220_u32, 40, 60],
        [30, 190, 90],
        [40, 70, 230],
        [240, 220, 30],
    ];
    let mut pixels = Vec::with_capacity(size.width as usize * size.height as usize * 4);
    for y in 0..size.height {
        for x in 0..size.width {
            let band = palette[((x + 2 * y) / 7 % 4) as usize];
            let shade = 96 + 159 * (size.height - y) / size.height;
            let mut alpha = 255;
            if x < size.width / 4 && y < size.height / 2 {
                alpha = 64 + 128 * x / (size.width / 4);
            }
            if (40..52).contains(&x) && (44..56).contains(&y) {
                alpha = 0;
            }
            for channel in band {
                #[allow(clippy::cast_possible_truncation)]
                pixels.push((channel * shade / 255 * alpha / 255) as u8);
            }
            #[allow(clippy::cast_possible_truncation)]
            pixels.push(alpha as u8);
        }
    }
    pixels
}

const PROGRESS: [f32; 5] = [0.0, 0.25, 0.5, 0.75, 1.0];

/// Checks `alpha2.transition.<name>` with its defaults and with each of
/// `param_sets`.
fn check(name: &str, tolerance: Tolerance, param_sets: &[&[(&'static str, ParamValue)]]) {
    check_at(size(), name, tolerance, param_sets);
}

fn check_at(
    size: RenderSize,
    name: &str,
    tolerance: Tolerance,
    param_sets: &[&[(&'static str, ParamValue)]],
) {
    let parity = Parity::new();
    let cards = parity.picture(size, &test_card(size, false));
    let bands = parity.picture(size, &band_card(size));
    let id = EffectId::new(format!("alpha2.transition.{name}"), 1);
    #[allow(clippy::cast_precision_loss)]
    let ratio = size.width as f32 / size.height as f32;
    let defaults: &[(&'static str, ParamValue)] = &[];
    for (set, values) in std::iter::once(defaults)
        .chain(param_sets.iter().copied())
        .enumerate()
    {
        for progress in PROGRESS {
            for (order, (from, to)) in [(&cards, &bands), (&bands, &cards)].into_iter().enumerate()
            {
                parity.check(
                    &format!("transition-{name}-{set}-{progress}-{order}"),
                    size,
                    tolerance,
                    |graph, side: Side| {
                        let from = graph.add_source(from.view(side));
                        let to = graph.add_source(to.view(side));
                        let mut params = EffectParams::new();
                        for (param, value) in values {
                            params.insert(*param, value.clone());
                        }
                        graph.add_transition(
                            id.clone(),
                            from,
                            to,
                            params,
                            EffectRuntime {
                                progress,
                                ratio,
                                ..Default::default()
                            },
                        )
                    },
                );
            }
        }
    }
}

#[test]
fn fade_matches() {
    check("fade", Tolerance::Close, &[]);
}

#[test]
fn directional_matches() {
    // The shader takes one sample where its seam coverage is exactly 0 or 1
    // and sixteen otherwise. Where the exact coverage is 0 or 1 (every pixel
    // off the seam, and a seam on a pixel edge) the choice turns on the last
    // bit of the uv and of a division, so at most sizes GPUs disagree with
    // each other by up to the whole picture's contrast at scattered pixels.
    // At power-of-two sizes uv and the divisions are exact.
    check_at(
        RenderSize::new(128, 64).expect("size"),
        "directional",
        Tolerance::Edges,
        &[
            &[("direction", ParamValue::Vec2([1.0, 0.0]))],
            &[("direction", ParamValue::Vec2([-0.4, 0.7]))],
            &[("direction", ParamValue::Vec2([0.0, 0.0]))],
        ],
    );
}

#[test]
fn circle_open_matches() {
    check(
        "circle-open",
        Tolerance::Edges,
        &[&[
            ("smoothness", ParamValue::Float(0.0)),
            ("opening", ParamValue::Bool(false)),
        ]],
    );
}

#[test]
fn angular_matches() {
    check(
        "angular",
        Tolerance::Edges,
        &[&[("starting_angle", ParamValue::Float(-135.0))]],
    );
}

#[test]
fn pixelize_matches() {
    check(
        "pixelize",
        Tolerance::Edges,
        &[
            &[
                ("squares_min", ParamValue::Vec2([12.0, 7.0])),
                ("steps", ParamValue::Float(7.0)),
            ],
            &[("steps", ParamValue::Float(0.0))],
        ],
    );
}

#[test]
fn directional_wipe_matches() {
    check(
        "directional-wipe",
        Tolerance::Edges,
        &[
            &[
                ("direction", ParamValue::Vec2([0.3, 1.0])),
                ("smoothness", ParamValue::Float(0.0)),
            ],
            &[("direction", ParamValue::Vec2([0.0, 0.0]))],
        ],
    );
}

#[test]
fn circle_matches() {
    check(
        "circle",
        Tolerance::Edges,
        &[&[
            ("center", ParamValue::Vec2([0.3, 0.65])),
            ("back_color", ParamValue::Color([0.8, 0.2, 0.1, 0.5])),
        ]],
    );
}

#[test]
fn cross_warp_matches() {
    check("cross-warp", Tolerance::Edges, &[]);
}

#[test]
fn dreamy_matches() {
    check("dreamy", Tolerance::Edges, &[]);
}

#[test]
fn radial_matches() {
    check(
        "radial",
        Tolerance::Edges,
        &[&[("smoothness", ParamValue::Float(0.05))]],
    );
}

#[test]
fn wipe_up_matches() {
    check("wipe-up", Tolerance::Edges, &[]);
}

#[test]
fn wipe_down_matches() {
    check("wipe-down", Tolerance::Edges, &[]);
}

#[test]
fn wipe_left_matches() {
    check("wipe-left", Tolerance::Edges, &[]);
}

#[test]
fn wipe_right_matches() {
    check("wipe-right", Tolerance::Edges, &[]);
}

#[test]
fn window_blinds_matches() {
    check("window-blinds", Tolerance::Edges, &[]);
}

#[test]
fn window_slice_matches() {
    check(
        "window-slice",
        Tolerance::Edges,
        &[&[
            ("count", ParamValue::Float(7.0)),
            ("smoothness", ParamValue::Float(0.0)),
        ]],
    );
}

#[test]
fn wind_matches() {
    check(
        "wind",
        Tolerance::Edges,
        &[&[
            ("size", ParamValue::Float(0.5)),
            ("reversed", ParamValue::Bool(true)),
        ]],
    );
}

#[test]
fn pinwheel_matches() {
    check(
        "pinwheel",
        Tolerance::Edges,
        &[&[("speed", ParamValue::Float(7.0))]],
    );
}

#[test]
fn heart_matches() {
    check("heart", Tolerance::Edges, &[]);
}

#[test]
fn color_phase_matches() {
    check(
        "color-phase",
        Tolerance::Close,
        &[&[
            ("from_step", ParamValue::Vec4([0.1, 0.3, 0.5, 0.2])),
            ("to_step", ParamValue::Vec4([0.4, 0.6, 0.9, 0.7])),
        ]],
    );
}

#[test]
fn fade_color_matches() {
    check(
        "fade-color",
        Tolerance::Close,
        &[&[
            ("color", ParamValue::Color([0.9, 0.4, 0.1, 0.6])),
            ("color_phase", ParamValue::Float(0.7)),
        ]],
    );
}

#[test]
fn fade_grayscale_matches() {
    check(
        "fade-grayscale",
        Tolerance::Close,
        &[&[("intensity", ParamValue::Float(0.8))]],
    );
}

#[test]
fn morph_matches() {
    check(
        "morph",
        Tolerance::Edges,
        &[&[("strength", ParamValue::Float(0.5))]],
    );
}

#[test]
fn ripple_matches() {
    check(
        "ripple",
        Tolerance::Edges,
        &[&[
            ("amplitude", ParamValue::Float(40.0)),
            ("speed", ParamValue::Float(20.0)),
        ]],
    );
}

#[test]
fn squeeze_matches() {
    check(
        "squeeze",
        Tolerance::Edges,
        &[&[("color_separation", ParamValue::Float(0.2))]],
    );
}

#[test]
fn simple_zoom_matches() {
    check(
        "simple-zoom",
        Tolerance::Edges,
        &[&[("zoom_quickness", ParamValue::Float(0.4))]],
    );
}

#[test]
fn circle_crop_matches() {
    check(
        "circle-crop",
        Tolerance::Edges,
        &[&[("back_color", ParamValue::Color([0.2, 0.6, 0.9, 0.75]))]],
    );
}

#[test]
fn colour_distance_matches() {
    check(
        "colour-distance",
        Tolerance::Edges,
        &[&[("power", ParamValue::Float(1.5))]],
    );
}

#[test]
fn polka_dots_curtain_matches() {
    check(
        "polka-dots-curtain",
        Tolerance::Edges,
        &[&[
            ("dots", ParamValue::Float(7.0)),
            ("center", ParamValue::Vec2([0.4, 0.7])),
        ]],
    );
}

#[test]
fn directional_warp_matches() {
    check(
        "directional-warp",
        Tolerance::Edges,
        &[
            &[
                ("smoothness", ParamValue::Float(0.4)),
                ("direction", ParamValue::Vec2([0.5, -1.0])),
            ],
            &[("direction", ParamValue::Vec2([0.0, 0.0]))],
        ],
    );
}

/// Every built-in transition has a CPU kernel and is a project effect.
#[test]
fn every_transition_has_a_kernel() {
    use crate::EffectCatalog as _;
    let renderer = crate::CpuRenderer::new(1);
    let unsupported = crate::CpuRenderer::unsupported_builtins();
    for definition in crate::builtin_transition_definitions() {
        assert!(
            renderer.project_effect_id(&definition.id, 1).is_some(),
            "{} is not a project effect",
            definition.id
        );
        assert!(
            !unsupported
                .iter()
                .any(|(missing, _)| *missing == definition.id),
            "{} has no CPU kernel",
            definition.id
        );
    }
}
