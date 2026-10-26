//! The CPU effect kernels against the GPU's shaders: every built-in effect
//! and internal pass at its defaults and at the ends of its ranges.

use alpha2_time::MediaTime;

use super::cpu_parity::{Parity, TestPicture, Tolerance, test_card};
use crate::builtins::{GAUSSIAN_BLUR_PASS_ID, GLOW_THRESHOLD_ID};
use crate::{
    BlendMode, CHROMA_KEY_ID, EffectId, EffectInputs, EffectParams, EffectResources, EffectRuntime,
    GAUSSIAN_BLUR_ID, GLOW_ID, GraphError, GraphLayer, LUT_3D_ID, LayerTransform, Lut3dData,
    MASK_COMBINE_EFFECT_IDS, MASK_COMBINE_NAMES, ParamValue, RenderGraph, RenderNodeId, RenderSize,
    TRACK_MATTE_EFFECT_ID,
};

fn size() -> RenderSize {
    RenderSize::new(96, 64).expect("size")
}

fn float(value: f32) -> ParamValue {
    ParamValue::Float(value)
}

fn effect(name: &str) -> EffectId {
    EffectId::new(format!("alpha2.effect.{name}"), 1)
}

type Params = Vec<(&'static str, ParamValue)>;

fn params(values: &Params) -> EffectParams<'static> {
    values
        .iter()
        .fold(EffectParams::new(), |params, (name, value)| {
            params.with(*name, value.clone())
        })
}

/// A picture whose alpha runs through every value: a diagonal ramp with
/// a hole and a few hard steps, colored, premultiplied.
fn coverage_picture(size: RenderSize, turn: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(size.width as usize * size.height as usize * 4);
    for y in 0..size.height {
        for x in 0..size.width {
            let (u, v) = ((x + turn * 17) % size.width, (y + turn * 11) % size.height);
            let ramp = (u * 255 / size.width + v * 128 / size.height).min(255);
            let hole = (u / 12 + v / 9 + turn).is_multiple_of(5);
            let alpha = if hole { 0 } else { ramp };
            let color = [
                (u * 255 / size.width),
                255 - (v * 255 / size.height),
                if (u / 6 + v / 6) % 2 == 0 { 220 } else { 30 },
            ];
            for channel in color {
                #[allow(clippy::cast_possible_truncation)]
                pixels.push((channel * alpha / 255) as u8);
            }
            #[allow(clippy::cast_possible_truncation)]
            pixels.push(alpha as u8);
        }
    }
    pixels
}

/// A layer scaled to a part of the canvas over transparency: a source whose
/// content covers only a part of the picture.
fn small_layer(
    graph: &mut RenderGraph<'_>,
    source: RenderNodeId,
) -> Result<RenderNodeId, GraphError> {
    graph.add_composite(vec![GraphLayer {
        source,
        visual: LayerTransform {
            position: [0.4, 0.55],
            anchor: [0.5, 0.5],
            scale: [0.35, 0.4],
            rotation_degrees: 0.0,
            opacity: 1.0,
        },
        blend_mode: BlendMode::Normal,
    }])
}

/// Checks `effect` on `picture` (whole, then as a small layer) at each of
/// `cases`.
fn check_effect(
    parity: &Parity,
    picture: &TestPicture,
    effect: &EffectId,
    cases: &[(Params, Tolerance)],
    runtime: EffectRuntime,
) {
    for (index, (values, tolerance)) in cases.iter().enumerate() {
        for small in [false, true] {
            let name = format!(
                "{}-{index}{}",
                effect.as_str().rsplit('.').next().unwrap_or("effect"),
                if small { "-small" } else { "" }
            );
            parity.check(&name, size(), *tolerance, |graph, side| {
                let mut source = graph.add_source(picture.view(side));
                if small {
                    source = small_layer(graph, source)?;
                }
                graph.add_effect(effect.clone(), source, params(values), runtime)
            });
        }
    }
}

#[test]
fn color_controls_match() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let grade = |exposure, contrast, saturation, temperature, tint| {
        vec![
            ("exposure", float(exposure)),
            ("contrast", float(contrast)),
            ("saturation", float(saturation)),
            ("temperature", float(temperature)),
            ("tint", float(tint)),
        ]
    };
    check_effect(
        &parity,
        &card,
        &effect("color-controls"),
        &[
            (Vec::new(), Tolerance::Close),
            (grade(0.7, 1.4, 0.4, 0.5, -0.3), Tolerance::Close),
            (grade(-3.0, 0.0, 2.0, -1.0, 1.0), Tolerance::Close),
            (grade(3.0, 2.0, 0.0, 1.0, -1.0), Tolerance::Close),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn chroma_key_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let key = |color: [f32; 4], threshold, softness, spill, mode| {
        vec![
            ("key_color", ParamValue::Color(color)),
            ("threshold", float(threshold)),
            ("softness", float(softness)),
            ("spill_suppression", float(spill)),
            ("distance_mode", ParamValue::Enum(mode)),
        ]
    };
    check_effect(
        &parity,
        &card,
        &EffectId::from_static(CHROMA_KEY_ID, 1),
        &[
            (Vec::new(), Tolerance::Close),
            (
                key([0.6, 0.4, 0.9, 1.0], 0.3, 0.2, 1.0, 0),
                Tolerance::Close,
            ),
            (
                key([0.2, 0.7, 0.15, 1.0], 0.1, 0.3, 0.5, 1),
                Tolerance::Close,
            ),
            (
                key([0.5, 0.5, 0.5, 1.0], 0.0, 1.0, 0.0, 0),
                Tolerance::Close,
            ),
            (
                key([1.0, 0.0, 1.0, 1.0], 3.0_f32.sqrt(), 1.0, 1.0, 1),
                Tolerance::Close,
            ),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn vignette_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let shade = |amount, radius, softness| {
        vec![
            ("amount", float(amount)),
            ("radius", float(radius)),
            ("softness", float(softness)),
        ]
    };
    check_effect(
        &parity,
        &card,
        &effect("vignette"),
        &[
            (Vec::new(), Tolerance::Close),
            (shade(1.0, 0.0, 0.01), Tolerance::Close),
            (shade(0.7, 1.5, 1.0), Tolerance::Close),
            (shade(0.0, 0.5, 0.5), Tolerance::Close),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn sharpen_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    check_effect(
        &parity,
        &card,
        &effect("sharpen"),
        &[
            (Vec::new(), Tolerance::Close),
            (vec![("amount", float(0.0))], Tolerance::Close),
            (vec![("amount", float(3.0))], Tolerance::Close),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn pixelate_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    check_effect(
        &parity,
        &card,
        &effect("pixelate"),
        &[
            (Vec::new(), Tolerance::Close),
            (vec![("blocks", float(2.0))], Tolerance::Close),
            (vec![("blocks", float(13.7))], Tolerance::Close),
            (vec![("blocks", float(400.0))], Tolerance::Close),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn film_grain_matches_at_several_times() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let grain = |amount, size| vec![("amount", float(amount)), ("size", float(size))];
    for ticks in [0, 12_345_678, (1_i64 << 40) + 7] {
        check_effect(
            &parity,
            &card,
            &effect("film-grain"),
            &[
                (Vec::new(), Tolerance::Close),
                (grain(1.0, 0.5), Tolerance::Close),
                (grain(0.4, 1.5), Tolerance::Close),
                (grain(0.6, 6.0), Tolerance::Close),
            ],
            EffectRuntime {
                time: MediaTime::from_ticks(ticks),
                ..EffectRuntime::default()
            },
        );
    }
}

#[test]
fn crop_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let crop = |top_left, bottom_right, feather| {
        vec![
            ("top_left", ParamValue::Vec2(top_left)),
            ("bottom_right", ParamValue::Vec2(bottom_right)),
            ("feather", float(feather)),
        ]
    };
    check_effect(
        &parity,
        &card,
        &effect("crop"),
        &[
            (Vec::new(), Tolerance::Close),
            (crop([0.1, 0.2], [0.25, 0.05], 0.05), Tolerance::Close),
            (crop([0.3, 0.0], [0.0, 0.4], 0.0), Tolerance::Close),
            (crop([0.2, 0.1], [0.3, 0.3], 0.2), Tolerance::Close),
            (crop([0.6, 0.0], [0.5, 0.0], 0.01), Tolerance::Close),
            (crop([1.0, 1.0], [0.0, 0.0], 0.0), Tolerance::Close),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn mirror_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let mirror = |axis, second| {
        vec![
            ("axis", ParamValue::Enum(axis)),
            ("keep_second_half", ParamValue::Bool(second)),
        ]
    };
    check_effect(
        &parity,
        &card,
        &effect("mirror"),
        &[
            (Vec::new(), Tolerance::Close),
            (mirror(0, true), Tolerance::Close),
            (mirror(1, false), Tolerance::Close),
            (mirror(1, true), Tolerance::Close),
        ],
        EffectRuntime::default(),
    );
}

/// A 17-point cube of a nonlinear grade over `domain`.
#[allow(clippy::cast_precision_loss)]
fn test_lut(domain_min: [f32; 3], domain_max: [f32; 3]) -> Lut3dData {
    let edge = 17_u32;
    let mut values = Vec::with_capacity((edge * edge * edge) as usize);
    for b in 0..edge {
        for g in 0..edge {
            for r in 0..edge {
                let [r, g, b] = [r, g, b].map(|value| value as f32 / (edge - 1) as f32);
                values.push([
                    (r * r * 0.8 + g * 0.15 + 0.03).min(1.2),
                    (g * 0.9 + b * b * 0.2).sqrt(),
                    1.0 - b * 0.7 - r * 0.2,
                ]);
            }
        }
    }
    Lut3dData {
        edge_length: edge,
        domain_min,
        domain_max,
        values,
    }
}

#[test]
fn lut_3d_matches() {
    let mut parity = Parity::new();
    for (id, lut) in [
        ("unit", test_lut([0.0; 3], [1.0; 3])),
        ("wide", test_lut([-0.1, 0.05, -0.2], [1.2, 0.9, 1.1])),
    ] {
        parity.gpu.register_lut3d(id, &lut).expect("GPU LUT");
        parity.cpu.register_lut3d(id, &lut).expect("CPU LUT");
    }
    let card = parity.picture(size(), &test_card(size(), false));
    for lut in ["unit", "wide"] {
        for (index, strength) in [1.0, 0.0, 0.45].into_iter().enumerate() {
            for small in [false, true] {
                let name = format!("lut-{lut}-{index}{}", if small { "-small" } else { "" });
                parity.check(&name, size(), Tolerance::Edges, |graph, side| {
                    let mut source = graph.add_source(card.view(side));
                    if small {
                        source = small_layer(graph, source)?;
                    }
                    graph.add_effect_with_resources(
                        EffectId::from_static(LUT_3D_ID, 1),
                        source,
                        EffectParams::new().with("strength", float(strength)),
                        EffectResources::new().with("lut", lut),
                        EffectRuntime::default(),
                    )
                });
            }
        }
    }
}

#[test]
fn gaussian_blur_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    check_effect(
        &parity,
        &card,
        &EffectId::from_static(GAUSSIAN_BLUR_ID, 1),
        &[
            (Vec::new(), Tolerance::Edges),
            (vec![("radius", float(0.0))], Tolerance::Edges),
            (vec![("radius", float(0.05))], Tolerance::Edges),
            (vec![("radius", float(0.2))], Tolerance::Edges),
            (vec![("radius", float(1.0))], Tolerance::Edges),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn single_blur_passes_match() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let (width, height) = (96.0_f32, 64.0_f32);
    let pass =
        |step: [f32; 2], sigma| vec![("step", ParamValue::Vec2(step)), ("sigma", float(sigma))];
    check_effect(
        &parity,
        &card,
        &EffectId::from_static(GAUSSIAN_BLUR_PASS_ID, 1),
        &[
            (Vec::new(), Tolerance::Edges),
            (pass([1.0 / width, 0.0], 4.0), Tolerance::Edges),
            (pass([0.0, 1.0 / height], 0.3), Tolerance::Edges),
            (pass([2.7 / width, 0.0], 2.5), Tolerance::Edges),
            (pass([0.0, 3.3 / height], 4.0), Tolerance::Edges),
            (pass([1.5 / width, 1.5 / height], 1.7), Tolerance::Edges),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn glow_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let glow = |threshold, intensity, radius| {
        vec![
            ("threshold", float(threshold)),
            ("intensity", float(intensity)),
            ("radius", float(radius)),
        ]
    };
    check_effect(
        &parity,
        &card,
        &EffectId::from_static(GLOW_ID, 1),
        &[
            (Vec::new(), Tolerance::Edges),
            (glow(0.0, 3.0, 0.06), Tolerance::Edges),
            (glow(1.0, 0.0, 0.0), Tolerance::Edges),
            (glow(0.3, 1.5, 0.03), Tolerance::Edges),
        ],
        EffectRuntime::default(),
    );
    check_effect(
        &parity,
        &card,
        &EffectId::from_static(GLOW_THRESHOLD_ID, 1),
        &[
            (Vec::new(), Tolerance::Close),
            (vec![("threshold", float(0.0))], Tolerance::Close),
            (vec![("threshold", float(1.0))], Tolerance::Close),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn track_matte_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let matte = parity.picture(size(), &coverage_picture(size(), 0));
    for luma in [false, true] {
        for inverted in [false, true] {
            for progress in [0.5, 0.0, 1.0] {
                for small in [false, true] {
                    let name = format!(
                        "track-matte-{}{}-{progress}{}",
                        if luma { "luma" } else { "alpha" },
                        if inverted { "-inverted" } else { "" },
                        if small { "-small" } else { "" }
                    );
                    parity.check(&name, size(), Tolerance::Close, |graph, side| {
                        let mut picture = graph.add_source(card.view(side));
                        let mut coverage = graph.add_source(matte.view(side));
                        if small {
                            picture = small_layer(graph, picture)?;
                            coverage = small_layer(graph, coverage)?;
                        }
                        graph.add_transition(
                            EffectId::from_static(TRACK_MATTE_EFFECT_ID, 1),
                            picture,
                            coverage,
                            EffectParams::new()
                                .with("luma", ParamValue::Bool(luma))
                                .with("inverted", ParamValue::Bool(inverted)),
                            EffectRuntime {
                                progress,
                                ..EffectRuntime::default()
                            },
                        )
                    });
                }
            }
        }
    }
}

#[test]
fn mask_combines_match() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let masks = (0..8_u32)
        .map(|turn| parity.picture(size(), &coverage_picture(size(), turn)))
        .collect::<Vec<_>>();
    // Each mask's (invert, mode), for one to eight masks.
    let cases: [&[(bool, u32)]; 9] = [
        &[(false, 0)],
        &[(true, 0)],
        &[(false, 1)],
        &[(false, 0), (false, 2)],
        &[(false, 2), (true, 1), (false, 0)],
        &[(false, 0), (false, 0), (true, 1), (false, 2)],
        &[(true, 1), (false, 0), (false, 0), (true, 2), (false, 1)],
        &[
            (false, 0),
            (false, 1),
            (false, 0),
            (false, 2),
            (true, 0),
            (false, 1),
        ],
        &[
            (false, 0),
            (false, 0),
            (false, 1),
            (true, 2),
            (false, 0),
            (false, 1),
            (true, 0),
            (false, 2),
        ],
    ];
    for (index, case) in cases.iter().enumerate() {
        for small in [false, true] {
            let name = format!("mask-combine-{index}{}", if small { "-small" } else { "" });
            parity.check(&name, size(), Tolerance::Close, |graph, side| {
                let mut source = graph.add_source(card.view(side));
                if small {
                    source = small_layer(graph, source)?;
                }
                let mut inputs = EffectInputs::new();
                let mut params = EffectParams::new();
                for (mask, (&(invert, mode), picture)) in case.iter().zip(&masks).enumerate() {
                    let names = MASK_COMBINE_NAMES[mask];
                    let mut coverage = graph.add_source(picture.view(side));
                    if small && mask % 2 == 0 {
                        coverage = small_layer(graph, coverage)?;
                    }
                    inputs.insert(names.input, coverage);
                    params.insert(names.invert, ParamValue::Bool(invert));
                    params.insert(names.mode, ParamValue::Enum(mode));
                }
                graph.add_effect_with_inputs(
                    EffectId::from_static(MASK_COMBINE_EFFECT_IDS[case.len() - 1], 1),
                    source,
                    inputs,
                    params,
                    EffectResources::new(),
                    EffectRuntime::default(),
                )
            });
        }
    }
}

#[test]
fn effect_pictures_do_not_depend_on_the_thread_count() {
    let card = test_card(size(), false);
    let draw = |threads, id: &EffectId, values: &Params| {
        let renderer = crate::CpuRenderer::new(threads);
        let picture = crate::CpuSource::image(std::sync::Arc::new(
            crate::CpuImage::from_premultiplied_rgba8(size(), &card).expect("picture"),
        ));
        let mut graph = RenderGraph::new(size());
        let source = graph.add_source(crate::SourceView::cpu(&picture));
        let source = small_layer(&mut graph, source).expect("layer");
        let output = graph
            .add_effect(id.clone(), source, params(values), EffectRuntime::default())
            .expect("effect");
        graph.set_output(output).expect("output");
        renderer.execute_graph(&graph).expect("draws").read_rgba8()
    };
    for (id, values) in [
        (
            EffectId::from_static(GAUSSIAN_BLUR_ID, 1),
            vec![("radius", float(0.1))],
        ),
        (
            EffectId::from_static(GLOW_ID, 1),
            vec![("threshold", float(0.2))],
        ),
        (effect("color-controls"), vec![("contrast", float(1.3))]),
        (effect("film-grain"), Vec::new()),
    ] {
        let one = draw(1, &id, &values);
        assert_eq!(one, draw(3, &id, &values), "{}", id.as_str());
        assert_eq!(one, draw(8, &id, &values), "{}", id.as_str());
    }
}

/// A source larger than the graph, as a preview drawn at half the size its
/// frames were presented at reads them: every effect samples it in uv, as
/// the GPU's shaders do, and covers the whole output, not the corner the
/// source's texels would reach at one texel per pixel.
#[test]
fn effects_on_a_larger_source_match() {
    let parity = Parity::new();
    let large = RenderSize::new(192, 128).expect("size");
    let card = parity.picture(large, &test_card(large, false));
    let cases: [(EffectId, Params, Tolerance); 6] = [
        (
            EffectId::from_static(GAUSSIAN_BLUR_ID, 1),
            vec![("radius", float(0.05))],
            Tolerance::Edges,
        ),
        (
            EffectId::from_static(GAUSSIAN_BLUR_ID, 1),
            vec![("radius", float(0.004))],
            Tolerance::Edges,
        ),
        (
            EffectId::from_static(GLOW_ID, 1),
            Vec::new(),
            Tolerance::Edges,
        ),
        (effect("color-controls"), Vec::new(), Tolerance::Close),
        (effect("mirror"), Vec::new(), Tolerance::Close),
        (effect("vignette"), Vec::new(), Tolerance::Close),
    ];
    for (index, (id, values, tolerance)) in cases.iter().enumerate() {
        let name = format!(
            "larger-source-{}-{index}",
            id.as_str().rsplit('.').next().unwrap_or("effect")
        );
        parity.check(&name, size(), parity.filtered(*tolerance), |graph, side| {
            let source = graph.add_source(card.view(side));
            graph.add_effect(id.clone(), source, params(values), EffectRuntime::default())
        });
    }
}

/// Every built-in effect and internal pass this file covers has a CPU
/// kernel, and the public effects are project effects.
#[test]
fn every_effect_has_a_kernel() {
    use crate::EffectCatalog as _;
    let renderer = crate::CpuRenderer::new(1);
    let unsupported = crate::CpuRenderer::unsupported_builtins();
    let mut ids = crate::builtin_effect_definitions()
        .into_iter()
        .map(|definition| definition.id)
        .collect::<Vec<_>>();
    for id in &ids {
        assert!(
            renderer.project_effect_id(id, 1).is_some(),
            "{id} is not a project effect"
        );
    }
    ids.extend(
        [
            GAUSSIAN_BLUR_PASS_ID,
            GLOW_THRESHOLD_ID,
            crate::builtins::GLOW_COMBINE_ID,
            TRACK_MATTE_EFFECT_ID,
        ]
        .map(str::to_owned),
    );
    ids.extend(MASK_COMBINE_EFFECT_IDS.map(str::to_owned));
    for id in ids {
        assert!(
            !unsupported.iter().any(|(missing, _)| *missing == id),
            "{id} has no CPU kernel"
        );
    }
}
