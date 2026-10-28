//! The "Keying" group's effects on the CPU against the GPU.

use super::{check_effect, coverage_picture, effect, float, params, size, small_layer};
use crate::tests::cpu_parity::{Parity, Tolerance, test_card};
use crate::{EffectInputs, EffectResources, EffectRuntime, ParamValue};

#[test]
fn luma_key_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let key = |key_out, threshold, softness| {
        vec![
            ("key_out", ParamValue::Enum(key_out)),
            ("threshold", float(threshold)),
            ("softness", float(softness)),
        ]
    };
    check_effect(
        &parity,
        &card,
        &effect("luma-key"),
        &[
            (Vec::new(), Tolerance::Close),
            (key(0, 0.4, 0.3), Tolerance::Close),
            (key(1, 0.2, 0.4), Tolerance::Close),
            (key(1, 0.0, 0.0), Tolerance::Close),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn spill_suppress_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let spill = |screen, amount| {
        vec![
            ("screen", ParamValue::Enum(screen)),
            ("amount", float(amount)),
        ]
    };
    check_effect(
        &parity,
        &card,
        &effect("spill-suppress"),
        &[
            (Vec::new(), Tolerance::Close),
            (spill(0, 0.4), Tolerance::Close),
            (spill(1, 1.0), Tolerance::Close),
            (spill(1, 0.0), Tolerance::Close),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn alpha_adjust_matches() {
    let parity = Parity::new();
    let ramp = parity.picture(size(), &coverage_picture(size(), 0));
    let levels = |black, white, gamma, invert| {
        vec![
            ("black", float(black)),
            ("white", float(white)),
            ("gamma", float(gamma)),
            ("invert", ParamValue::Bool(invert)),
        ]
    };
    check_effect(
        &parity,
        &ramp,
        &effect("alpha-adjust"),
        &[
            (Vec::new(), Tolerance::Close),
            // A gamma above one is infinitely steep at the black point: a rounding of the
            // layer's alpha there is many codes of output.
            (levels(0.2, 0.8, 2.2, false), Tolerance::Unstable),
            // White below black is a hard step at black.
            (levels(0.7, 0.3, 1.0, false), Tolerance::Unstable),
        ],
        EffectRuntime::default(),
    );
    // Inverted, barely covered pixels turn opaque in their straight color, a division by
    // almost nothing: only the picture both renderers read alike, not a layer each filters.
    for (index, values) in [levels(0.1, 0.6, 0.4, true), levels(0.0, 1.0, 1.0, true)]
        .iter()
        .enumerate()
    {
        let name = format!("alpha-adjust-inverted-{index}");
        parity.check(&name, size(), Tolerance::Close, |graph, side| {
            let source = graph.add_source(ramp.view(side));
            graph.add_effect(
                effect("alpha-adjust"),
                source,
                params(values),
                EffectRuntime::default(),
            )
        });
    }
}

#[test]
fn matte_choker_matches() {
    let parity = Parity::new();
    let ramp = parity.picture(size(), &coverage_picture(size(), 0));
    let choke = |choke, radius| vec![("choke", float(choke)), ("radius", float(radius))];
    check_effect(
        &parity,
        &ramp,
        &effect("matte-choker"),
        &[
            (Vec::new(), Tolerance::Edges),
            (choke(1.0, 0.03), Tolerance::Edges),
            (choke(-1.0, 0.03), Tolerance::Edges),
            (choke(-0.4, 0.01), Tolerance::Edges),
            (choke(0.7, 0.0), Tolerance::Edges),
        ],
        EffectRuntime::default(),
    );
}

#[test]
fn set_matte_matches() {
    let parity = Parity::new();
    let card = parity.picture(size(), &test_card(size(), false));
    let matte = parity.picture(size(), &coverage_picture(size(), 2));
    for mode in [0, 1] {
        for invert in [false, true] {
            for small in [false, true] {
                let name = format!(
                    "set-matte-{mode}{}{}",
                    if invert { "-inverted" } else { "" },
                    if small { "-small" } else { "" }
                );
                let values = vec![
                    ("mode", ParamValue::Enum(mode)),
                    ("invert", ParamValue::Bool(invert)),
                ];
                parity.check(&name, size(), Tolerance::Close, |graph, side| {
                    let mut source = graph.add_source(card.view(side));
                    let mut coverage = graph.add_source(matte.view(side));
                    if small {
                        source = small_layer(graph, source)?;
                        coverage = small_layer(graph, coverage)?;
                    }
                    graph.add_effect_with_inputs(
                        effect("set-matte"),
                        source,
                        EffectInputs::new().with("matte", coverage),
                        params(&values),
                        EffectResources::new(),
                        EffectRuntime::default(),
                    )
                });
            }
        }
    }
}
