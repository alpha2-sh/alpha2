//! Internal effects behind clip shape masks and track mattes.
//!
//! Neither is visible to projects as an effect: the timeline engine inserts
//! them from a clip's `masks` and `matte`. A mask's outline is drawn by the
//! vector pass as coverage and feathered by the Gaussian blur; the passes
//! here combine those coverages and keep the clip by them.

use std::fmt::Write as _;

use alpha2_gpu::CapabilityTier;

use crate::{EffectDef, EffectKind, InputSpec, ParamSpec, ParamType, ParamUnit, ParamValue};

/// Internal identifier of the track-matte pass.
pub const TRACK_MATTE_EFFECT_ID: &str = "alpha2.internal.track-matte";

/// Masks one mask-combine pass combines at most. The project model allows as
/// many, and each is one of its named inputs.
pub const MAX_SHAPE_MASKS: usize = 8;

const _: () = assert!(MAX_SHAPE_MASKS <= crate::MAX_EFFECT_INPUTS);

/// Enum values of a mask's `mode{i}` parameter.
pub const MASK_MODES: [&str; 3] = ["add", "subtract", "intersect"];

/// Internal identifiers of the mask-combine passes, by mask count minus one:
/// the pass for `n` masks is `MASK_COMBINE_EFFECT_IDS[n - 1]`.
pub const MASK_COMBINE_EFFECT_IDS: [&str; MAX_SHAPE_MASKS] = [
    "alpha2.internal.mask-combine-1",
    "alpha2.internal.mask-combine-2",
    "alpha2.internal.mask-combine-3",
    "alpha2.internal.mask-combine-4",
    "alpha2.internal.mask-combine-5",
    "alpha2.internal.mask-combine-6",
    "alpha2.internal.mask-combine-7",
    "alpha2.internal.mask-combine-8",
];

/// The names of one mask of a mask-combine pass: its coverage input and
/// its parameters, as [`mask_combine_definition`] declares them.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MaskCombineNames {
    /// The named input holding the mask's coverage in its alpha: `m{i}`.
    pub input: &'static str,
    /// `invert{i}`.
    pub invert: &'static str,
    /// `mode{i}`.
    pub mode: &'static str,
}

macro_rules! mask_combine_names {
    ($($index:literal),*) => {
        [$(MaskCombineNames {
            input: concat!("m", $index),
            invert: concat!("invert", $index),
            mode: concat!("mode", $index),
        }),*]
    };
}

/// Each mask's names, by mask index: static, so a caller sets a frame's
/// masks without formatting a name.
pub const MASK_COMBINE_NAMES: [MaskCombineNames; MAX_SHAPE_MASKS] =
    mask_combine_names!(0, 1, 2, 3, 4, 5, 6, 7);

fn param(name: String, param_type: ParamType, default: ParamValue) -> ParamSpec {
    ParamSpec {
        name,
        param_type,
        default,
        min: None,
        max: None,
        step: None,
        unit: match param_type {
            ParamType::Bool => ParamUnit::Bool,
            ParamType::Enum => ParamUnit::Enum,
            _ => ParamUnit::Normalized,
        },
        enum_values: if param_type == ParamType::Enum {
            MASK_MODES.map(str::to_owned).to_vec()
        } else {
            Vec::new()
        },
    }
}

/// The pass combining `count` masks (1 to [`MAX_SHAPE_MASKS`]) into its
/// source's coverage. Mask `i` is the named input `m{i}`, whose alpha is
/// the mask's coverage (drawn by the vector pass, feathered by a blur),
/// inverted when `invert{i}` is set, and combined with the masks before it
/// by `mode{i}`: coverage starts empty when the first mask adds and full
/// otherwise; add is `c + m - c m`, subtract `c (1 - m)`, intersect `c m`.
/// The source is kept by the result.
///
/// # Panics
/// When `count` is zero or more than [`MAX_SHAPE_MASKS`].
#[must_use]
pub fn mask_combine_definition(count: usize) -> EffectDef {
    assert!(
        (1..=MAX_SHAPE_MASKS).contains(&count),
        "a mask-combine pass takes 1 to {MAX_SHAPE_MASKS} masks"
    );
    let names = &MASK_COMBINE_NAMES[..count];
    let mut params = Vec::with_capacity(count * 2);
    let mut steps = String::new();
    for names in names {
        params.push(param(
            names.invert.into(),
            ParamType::Bool,
            ParamValue::Bool(false),
        ));
        params.push(param(
            names.mode.into(),
            ParamType::Enum,
            ParamValue::Enum(0),
        ));
        writeln!(
            steps,
            "    coverage = builtin_mask_step(coverage, sample_{input}(uv).a, params.{invert}, params.{mode});",
            input = names.input,
            invert = names.invert,
            mode = names.mode,
        )
        .expect("writing to a String cannot fail");
    }
    EffectDef {
        id: MASK_COMBINE_EFFECT_IDS[count - 1].into(),
        version: 1,
        kind: EffectKind::Effect,
        inputs: names
            .iter()
            .map(|names| InputSpec {
                name: names.input.into(),
            })
            .collect(),
        params,
        resources: Vec::new(),
        wgsl_body: format!(
            r"fn builtin_mask_step(coverage: f32, mask: f32, invert: u32, mode: u32) -> f32 {{
    var m = clamp(mask, 0.0, 1.0);
    if (invert != 0u) {{ m = 1.0 - m; }}
    if (mode == 1u) {{ return coverage * (1.0 - m); }}
    if (mode == 2u) {{ return coverage * m; }}
    return coverage + m - coverage * m;
}}

fn effect(uv: vec2f, src: vec4f) -> vec4f {{
    // Empty when the first mask adds, full otherwise.
    var coverage = select(1.0, 0.0, params.mode0 == 0u);
{steps}    return src * clamp(coverage, 0.0, 1.0);
}}
"
        ),
        tier_min: CapabilityTier::TierB,
        license: Some(crate::builtins::ALPHA2_EFFECT_LICENSE.into()),
    }
}

/// Every mask-combine pass, for one to [`MAX_SHAPE_MASKS`] masks.
#[must_use]
pub fn mask_combine_definitions() -> Vec<EffectDef> {
    (1..=MAX_SHAPE_MASKS).map(mask_combine_definition).collect()
}

/// The track-matte pass: the first input kept where the second input's
/// alpha (or luminance over black: BT.709 weights on linear light, times
/// alpha) says, optionally inverted.
///
/// It is a two-input transition and ignores progress; pass one inside
/// `(0, 1)` so its body runs.
#[must_use]
pub fn track_matte_definition() -> EffectDef {
    EffectDef {
        id: TRACK_MATTE_EFFECT_ID.into(),
        version: 1,
        kind: EffectKind::Transition,
        inputs: Vec::new(),
        params: vec![
            ParamSpec {
                unit: ParamUnit::Bool,
                ..param("luma".into(), ParamType::Bool, ParamValue::Bool(false))
            },
            ParamSpec {
                unit: ParamUnit::Bool,
                ..param("inverted".into(), ParamType::Bool, ParamValue::Bool(false))
            },
        ],
        resources: Vec::new(),
        wgsl_body: r"fn builtin_matte_linear(value: f32) -> f32 {
    if (value <= 0.04045) { return value / 12.92; }
    return pow((value + 0.055) / 1.055, 2.4);
}

fn transition(uv: vec2f) -> vec4f {
    let picture = from_color(uv);
    let matte = to_color(uv);
    var coverage = matte.a;
    if (params.luma != 0u) {
        let straight = clamp(matte.rgb / max(matte.a, 0.000001), vec3f(0.0), vec3f(1.0));
        let linear = vec3f(
            builtin_matte_linear(straight.r),
            builtin_matte_linear(straight.g),
            builtin_matte_linear(straight.b),
        );
        coverage = dot(linear, vec3f(0.2126, 0.7152, 0.0722)) * matte.a;
    }
    if (params.inverted != 0u) { coverage = 1.0 - coverage; }
    return picture * clamp(coverage, 0.0, 1.0);
}
"
        .into(),
        tier_min: CapabilityTier::TierB,
        license: Some(crate::builtins::ALPHA2_EFFECT_LICENSE.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MASK_COMBINE_EFFECT_IDS, MASK_COMBINE_NAMES, MAX_SHAPE_MASKS, mask_combine_definition,
    };

    #[test]
    fn each_masks_static_names_are_the_ones_the_pass_declares() {
        for count in 1..=MAX_SHAPE_MASKS {
            let definition = mask_combine_definition(count);
            assert_eq!(definition.id, MASK_COMBINE_EFFECT_IDS[count - 1]);
            let params = definition
                .params
                .iter()
                .map(|spec| spec.name.clone())
                .collect::<Vec<_>>();
            let inputs = definition
                .inputs
                .iter()
                .map(|input| input.name.clone())
                .collect::<Vec<_>>();
            let mut expected_params = Vec::new();
            let mut expected_inputs = Vec::new();
            for (index, names) in MASK_COMBINE_NAMES[..count].iter().enumerate() {
                assert_eq!(names.input, format!("m{index}"));
                assert_eq!(names.invert, format!("invert{index}"));
                assert_eq!(names.mode, format!("mode{index}"));
                expected_inputs.push(names.input.to_owned());
                expected_params.push(names.invert.to_owned());
                expected_params.push(names.mode.to_owned());
            }
            assert_eq!(inputs, expected_inputs);
            assert_eq!(params, expected_params);
        }
    }
}
