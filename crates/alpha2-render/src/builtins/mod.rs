//! The built-in effects and transitions as data: one module per catalog
//! group, whose definitions the catalog lists under that group's name.

use alpha2_gpu::CapabilityTier;
use alpha2_shaders::{
    CHROMA_KEY_BODY, EFFECT_HELPERS, EXPANDED_EFFECT, FOCUS_COMBINE, GAUSSIAN_BLUR,
    GL_TRANSITION_ENTRY, GLOW_COMBINE, GLOW_THRESHOLD, LUT_3D_BODY, SHADOW_COMBINE, SHADOW_OFFSET,
    TRANSITION_HELPERS, UNSHARP_COMBINE,
};

use crate::{
    EffectDef, EffectId, EffectKind, EffectResourceKind, InputSpec, ParamSpec, ParamType,
    ParamUnit, ParamValue, Renderer, ResourceSpec, ShaderDiagnostics,
};

mod effects;
mod transitions;

pub(crate) use effects::blur::{
    directional_blur_definition, tilt_shift_definition, unsharp_mask_definition,
};
pub(crate) use effects::stylize::drop_shadow_definition;

/// Stable identifier of the built-in chroma-key effect.
pub const CHROMA_KEY_ID: &str = "alpha2.effect.chroma-key";

/// Stable identifier of the atomic Gaussian-blur effect.
pub const GAUSSIAN_BLUR_ID: &str = "alpha2.effect.gaussian-blur";

/// Internal stable identifier of one separable Gaussian-blur pass.
pub(crate) const GAUSSIAN_BLUR_PASS_ID: &str = "alpha2.internal.gaussian-blur-pass";

/// Stable identifier of the built-in glow effect.
pub const GLOW_ID: &str = "alpha2.effect.glow";

/// Internal stable identifier of glow's threshold pass.
pub(crate) const GLOW_THRESHOLD_ID: &str = "alpha2.internal.glow-threshold";

/// Internal stable identifier of glow's two-input combining pass.
pub(crate) const GLOW_COMBINE_ID: &str = "alpha2.internal.glow-combine";

/// Stable identifier of the built-in directional (one-axis Gaussian) blur.
pub(crate) const DIRECTIONAL_BLUR_ID: &str = "alpha2.effect.directional-blur";

/// Stable identifier of the built-in unsharp mask.
pub(crate) const UNSHARP_MASK_ID: &str = "alpha2.effect.unsharp-mask";

/// Internal stable identifier of unsharp mask's two-input combining pass.
pub(crate) const UNSHARP_COMBINE_ID: &str = "alpha2.internal.unsharp-combine";

/// Stable identifier of the built-in tilt-shift.
pub(crate) const TILT_SHIFT_ID: &str = "alpha2.effect.tilt-shift";

/// Internal stable identifier of tilt-shift's pass mixing the source and
/// its two blurs.
pub(crate) const FOCUS_COMBINE_ID: &str = "alpha2.internal.focus-combine";

/// Stable identifier of the built-in drop shadow.
pub(crate) const DROP_SHADOW_ID: &str = "alpha2.effect.drop-shadow";

/// Internal stable identifier of drop shadow's offset pass.
pub(crate) const SHADOW_OFFSET_ID: &str = "alpha2.internal.shadow-offset";

/// Internal stable identifier of drop shadow's two-input combining pass.
pub(crate) const SHADOW_COMBINE_ID: &str = "alpha2.internal.shadow-combine";

/// `EffectDef::license` of the effects Alpha2 itself wrote.
pub(crate) const ALPHA2_EFFECT_LICENSE: &str = "MIT; Copyright Alpha2 contributors";

/// Stable identifier of the resource-backed 3D LUT effect.
pub const LUT_3D_ID: &str = "alpha2.effect.lut-3d";

/// The commit of gl-transitions the ports come from, as a link to a file.
pub(crate) const GL_TRANSITIONS_BASE: &str = "https://github.com/gl-transitions/gl-transitions/blob/902218a1b63773ac0d0d9f491951da3392365bfe/transitions/";

/// Returns fresh definitions for every public built-in clip effect.
#[must_use]
pub fn builtin_effect_definitions() -> Vec<EffectDef> {
    builtin_effect_groups()
        .into_iter()
        .flat_map(|(_, definitions)| definitions)
        .collect()
}

/// The public built-in effects by catalog group, in catalog order.
pub(crate) fn builtin_effect_groups() -> Vec<(&'static str, Vec<EffectDef>)> {
    vec![
        ("Color", effects::color::definitions()),
        ("Keying", effects::keying::definitions()),
        ("Blur & Sharpen", effects::blur::definitions()),
        ("Stylize", effects::stylize::definitions()),
        ("Distort", effects::distort::definitions()),
    ]
}

/// Returns fresh, data-only definitions for all built-in transitions.
#[must_use]
pub fn builtin_transition_definitions() -> Vec<EffectDef> {
    builtin_transition_groups()
        .into_iter()
        .flat_map(|(_, definitions)| definitions)
        .collect()
}

/// The built-in transitions by catalog group, in catalog order.
pub(crate) fn builtin_transition_groups() -> Vec<(&'static str, Vec<EffectDef>)> {
    vec![
        ("Classic", transitions::classic::definitions()),
        ("Wipes", transitions::wipes::definitions()),
        ("Stylized", transitions::stylized::definitions()),
        ("Shapes", transitions::shapes::definitions()),
        ("3D", transitions::three_d::definitions()),
        ("Zoom & Blur", transitions::zoom_blur::definitions()),
        ("Glitch", transitions::glitch::definitions()),
    ]
}

/// Returns the data-only definition of the built-in chroma-key effect.
///
/// `key_color` is compared with each pixel's straight color as the renderer
/// stores it: sRGB-encoded, not linear light. Pick the key as a color picker
/// shows the screen. With `distance_mode` at `rgb` a pixel within `threshold`
/// of the key in that RGB space is removed, `softness` wide edges are partly
/// transparent and `spill_suppression` desaturates them; `chroma` compares
/// only the chroma plane, so a key survives shadows on a screen.
#[must_use]
pub fn chroma_key_definition() -> EffectDef {
    EffectDef {
        id: CHROMA_KEY_ID.into(),
        version: 1,
        kind: EffectKind::Effect,
        inputs: Vec::new(),
        params: vec![
            color("key_color", [0.0, 1.0, 0.0, 1.0]),
            float("threshold", 0.35, 0.0, 3.0_f32.sqrt()),
            float("softness", 0.1, f32::EPSILON, 1.0),
            float("spill_suppression", 0.2, 0.0, 1.0),
            choice("distance_mode", 0, &["rgb", "chroma"]),
        ],
        resources: Vec::new(),
        wgsl_body: effect_body(CHROMA_KEY_BODY),
        tier_min: CapabilityTier::TierB,
        license: Some(ALPHA2_EFFECT_LICENSE.into()),
    }
}

/// Returns the data-only definition of the built-in Gaussian blur.
///
/// Its standard deviation is half its `radius`, a fraction of the output
/// height. A render graph draws it as a cascade of separable passes
/// ([`crate::RenderGraph::add_effect`]); its own body is never drawn.
#[must_use]
pub fn gaussian_blur_definition() -> EffectDef {
    EffectDef {
        id: GAUSSIAN_BLUR_ID.into(),
        version: 1,
        kind: EffectKind::Effect,
        inputs: Vec::new(),
        params: vec![float_step("radius", 0.012, 0.0, 1.0, 0.001)],
        resources: Vec::new(),
        wgsl_body: effect_body(EXPANDED_EFFECT),
        tier_min: CapabilityTier::TierB,
        license: Some(ALPHA2_EFFECT_LICENSE.into()),
    }
}

pub(crate) fn gaussian_blur_pass_definition() -> EffectDef {
    EffectDef {
        id: GAUSSIAN_BLUR_PASS_ID.into(),
        version: 1,
        kind: EffectKind::Effect,
        inputs: Vec::new(),
        params: vec![
            vec2("step", [0.0, 0.0]),
            float_step(
                "sigma",
                1.0,
                0.0,
                crate::gaussian_blur::MAX_PASS_SIGMA_TAPS,
                0.001,
            ),
        ],
        resources: Vec::new(),
        wgsl_body: effect_body(GAUSSIAN_BLUR),
        tier_min: CapabilityTier::TierB,
        license: Some(ALPHA2_EFFECT_LICENSE.into()),
    }
}

/// Returns the data-only definition of the built-in glow.
///
/// The straight color above `threshold` is blurred like
/// [`gaussian_blur_definition`] with this `radius` and added back to the
/// source, scaled by `intensity`. A render graph draws it as internal passes;
/// its own body is never drawn.
#[must_use]
pub fn glow_definition() -> EffectDef {
    EffectDef {
        id: GLOW_ID.into(),
        version: 1,
        kind: EffectKind::Effect,
        inputs: Vec::new(),
        params: vec![
            float("threshold", 0.6, 0.0, 1.0),
            float("intensity", 0.8, 0.0, 3.0),
            float("radius", 0.012, 0.0, 0.06),
        ],
        resources: Vec::new(),
        wgsl_body: effect_body(EXPANDED_EFFECT),
        tier_min: CapabilityTier::TierB,
        license: Some(ALPHA2_EFFECT_LICENSE.into()),
    }
}

fn glow_threshold_definition() -> EffectDef {
    EffectDef {
        id: GLOW_THRESHOLD_ID.into(),
        ..simple_effect(
            "glow-threshold",
            GLOW_THRESHOLD,
            vec![float("threshold", 0.6, 0.0, 1.0)],
        )
    }
}

// Two inputs make it a transition to the registry; the graph draws it at progress 0.5, and
// its body reads no progress.
fn glow_combine_definition() -> EffectDef {
    EffectDef {
        id: GLOW_COMBINE_ID.into(),
        version: 1,
        kind: EffectKind::Transition,
        inputs: Vec::new(),
        params: vec![float("intensity", 0.8, 0.0, 3.0)],
        resources: Vec::new(),
        wgsl_body: effect_body(GLOW_COMBINE),
        tier_min: CapabilityTier::TierB,
        license: Some(ALPHA2_EFFECT_LICENSE.into()),
    }
}

// Two inputs make it a transition to the registry, as glow's combine is.
fn unsharp_combine_definition() -> EffectDef {
    EffectDef {
        id: UNSHARP_COMBINE_ID.into(),
        version: 1,
        kind: EffectKind::Transition,
        inputs: Vec::new(),
        params: vec![
            float("amount", 1.0, 0.0, 5.0),
            float("threshold", 0.0, 0.0, 1.0),
        ],
        resources: Vec::new(),
        wgsl_body: effect_body(UNSHARP_COMBINE),
        tier_min: CapabilityTier::TierB,
        license: Some(ALPHA2_EFFECT_LICENSE.into()),
    }
}

// The source and its two blurs, `soft` and `blurred`, as named inputs.
fn focus_combine_definition() -> EffectDef {
    EffectDef {
        id: FOCUS_COMBINE_ID.into(),
        version: 1,
        kind: EffectKind::Effect,
        inputs: vec![
            InputSpec {
                name: "soft".into(),
            },
            InputSpec {
                name: "blurred".into(),
            },
        ],
        params: vec![
            float("position", 0.5, 0.0, 1.0),
            degrees("angle", 0.0, -90.0, 90.0),
            float("focus", 0.15, 0.0, 1.0),
            float_step("falloff", 0.25, 0.001, 1.0, 0.005),
        ],
        resources: Vec::new(),
        wgsl_body: effect_body(FOCUS_COMBINE),
        tier_min: CapabilityTier::TierB,
        license: Some(ALPHA2_EFFECT_LICENSE.into()),
    }
}

fn shadow_offset_definition() -> EffectDef {
    EffectDef {
        id: SHADOW_OFFSET_ID.into(),
        ..simple_effect(
            "shadow-offset",
            SHADOW_OFFSET,
            vec![
                vec2("offset", [0.0, 0.0]),
                color("color", [0.0, 0.0, 0.0, 1.0]),
                float("opacity", 0.6, 0.0, 1.0),
            ],
        )
    }
}

// A transition to the registry for its two inputs, as glow's combine.
fn shadow_combine_definition() -> EffectDef {
    EffectDef {
        id: SHADOW_COMBINE_ID.into(),
        version: 1,
        kind: EffectKind::Transition,
        inputs: Vec::new(),
        params: vec![boolean("shadow_only", false)],
        resources: Vec::new(),
        wgsl_body: effect_body(SHADOW_COMBINE),
        tier_min: CapabilityTier::TierB,
        license: Some(ALPHA2_EFFECT_LICENSE.into()),
    }
}

/// Returns the data-only definition of the resource-backed 3D LUT effect.
#[must_use]
pub fn lut_3d_definition() -> EffectDef {
    EffectDef {
        id: LUT_3D_ID.into(),
        version: 1,
        kind: EffectKind::Effect,
        inputs: Vec::new(),
        params: vec![float("strength", 1.0, 0.0, 1.0)],
        resources: vec![ResourceSpec {
            name: "lut".into(),
            kind: EffectResourceKind::Lut3d,
        }],
        wgsl_body: effect_body(LUT_3D_BODY),
        tier_min: CapabilityTier::TierB,
        license: Some(ALPHA2_EFFECT_LICENSE.into()),
    }
}

/// The passes the engine inserts itself, hidden from the effect catalog.
pub(crate) fn internal_effect_definitions() -> Vec<EffectDef> {
    let mut definitions = vec![
        gaussian_blur_pass_definition(),
        glow_threshold_definition(),
        glow_combine_definition(),
        shadow_offset_definition(),
        shadow_combine_definition(),
        unsharp_combine_definition(),
        focus_combine_definition(),
        crate::masking::track_matte_definition(),
        crate::generator::generator_definition(),
    ];
    definitions.extend(crate::masking::mask_combine_definitions());
    definitions
}

impl Renderer {
    async fn register_builtin_definition(
        &mut self,
        definition: EffectDef,
    ) -> Result<EffectId, ShaderDiagnostics> {
        let id = self.register_effect(definition).await?;
        self.effects.mark_builtin(id.clone());
        Ok(id)
    }

    /// Compiles and registers the complete built-in effect and transition catalog.
    ///
    /// Built-ins are engine-owned and cannot be removed through the host-effect
    /// lifecycle methods.
    ///
    /// # Errors
    /// Returns diagnostics from the first definition that fails.
    pub async fn register_builtins(&mut self) -> Result<Vec<EffectId>, ShaderDiagnostics> {
        for definition in internal_effect_definitions() {
            let internal = self.register_builtin_definition(definition).await?;
            self.effects.mark_internal(internal);
        }
        let mut definitions = builtin_effect_definitions();
        definitions.extend(builtin_transition_definitions());
        let mut ids = Vec::with_capacity(definitions.len());
        for definition in definitions {
            ids.push(self.register_builtin_definition(definition).await?);
        }
        Ok(ids)
    }

    /// Compiles and registers the built-in chroma-key effect.
    ///
    /// # Errors
    /// Returns schema or compiler diagnostics when registration fails.
    pub async fn register_builtin_chroma_key(&mut self) -> Result<EffectId, ShaderDiagnostics> {
        self.register_builtin_definition(chroma_key_definition())
            .await
    }

    /// Compiles and registers the public Gaussian blur and its internal pass.
    ///
    /// # Errors
    /// Returns schema or compiler diagnostics when registration fails.
    pub async fn register_builtin_gaussian_blur(&mut self) -> Result<EffectId, ShaderDiagnostics> {
        let internal = self
            .register_builtin_definition(gaussian_blur_pass_definition())
            .await?;
        self.effects.mark_internal(internal);
        self.register_builtin_definition(gaussian_blur_definition())
            .await
    }

    /// Compiles and registers the built-in 3D LUT effect.
    ///
    /// # Errors
    /// Returns schema or compiler diagnostics when registration fails.
    pub async fn register_builtin_lut_3d(&mut self) -> Result<EffectId, ShaderDiagnostics> {
        self.register_builtin_definition(lut_3d_definition()).await
    }

    /// Compiles and registers all built-in transition definitions.
    ///
    /// # Errors
    /// Returns diagnostics from the first definition that fails. Definitions
    /// compiled before that point remain registered.
    pub async fn register_builtin_transitions(
        &mut self,
    ) -> Result<Vec<EffectId>, ShaderDiagnostics> {
        let definitions = builtin_transition_definitions();
        let mut ids = Vec::with_capacity(definitions.len());
        for definition in definitions {
            ids.push(self.register_builtin_definition(definition).await?);
        }
        Ok(ids)
    }
}

/// An effect or internal pass body after the shared effect helpers.
pub(crate) fn effect_body(body: &str) -> String {
    format!("{EFFECT_HELPERS}\n{body}")
}

/// A public effect Alpha2 wrote, `alpha2.effect.<name>`.
pub(crate) fn simple_effect(name: &str, body: &str, params: Vec<ParamSpec>) -> EffectDef {
    EffectDef {
        id: format!("alpha2.effect.{name}"),
        version: 1,
        kind: EffectKind::Effect,
        inputs: Vec::new(),
        params,
        resources: Vec::new(),
        wgsl_body: effect_body(body),
        tier_min: CapabilityTier::TierB,
        license: Some(ALPHA2_EFFECT_LICENSE.into()),
    }
}

/// A port of the gl-transitions file `source` by `author`,
/// `alpha2.transition.<name>`.
pub(crate) fn transition(
    name: &str,
    body: &str,
    source: &str,
    author: &str,
    params: Vec<ParamSpec>,
) -> EffectDef {
    EffectDef {
        id: format!("alpha2.transition.{name}"),
        version: 1,
        kind: EffectKind::Transition,
        inputs: Vec::new(),
        params,
        resources: Vec::new(),
        wgsl_body: format!("{TRANSITION_HELPERS}\n{GL_TRANSITION_ENTRY}\n{body}"),
        tier_min: CapabilityTier::TierB,
        license: Some(format!(
            "MIT; Author: {author}; Source: {GL_TRANSITIONS_BASE}{source}"
        )),
    }
}

/// A transition Alpha2 wrote, `alpha2.transition.<name>`: its body defines
/// `transition` over the top-left uv.
pub(crate) fn original_transition(name: &str, body: &str, params: Vec<ParamSpec>) -> EffectDef {
    EffectDef {
        id: format!("alpha2.transition.{name}"),
        version: 1,
        kind: EffectKind::Transition,
        inputs: Vec::new(),
        params,
        resources: Vec::new(),
        wgsl_body: format!("{TRANSITION_HELPERS}\n{body}"),
        tier_min: CapabilityTier::TierB,
        license: Some(ALPHA2_EFFECT_LICENSE.into()),
    }
}

pub(crate) fn float(name: &str, default: f32, min: f32, max: f32) -> ParamSpec {
    let span = max - min;
    let step = if span > 50.0 {
        1.0
    } else if span <= 0.1 {
        0.001
    } else {
        0.01
    };
    float_step(name, default, min, max, step)
}

pub(crate) fn float_step(name: &str, default: f32, min: f32, max: f32, step: f32) -> ParamSpec {
    ParamSpec {
        name: name.into(),
        param_type: ParamType::Float,
        default: ParamValue::Float(default),
        min: Some(min),
        max: Some(max),
        step: Some(step),
        unit: ParamUnit::Normalized,
        enum_values: Vec::new(),
    }
}

pub(crate) fn degrees(name: &str, default: f32, min: f32, max: f32) -> ParamSpec {
    ParamSpec {
        unit: ParamUnit::Degrees,
        step: Some(1.0),
        ..float(name, default, min, max)
    }
}

pub(crate) fn vec2(name: &str, default: [f32; 2]) -> ParamSpec {
    ParamSpec {
        name: name.into(),
        param_type: ParamType::Vec2,
        default: ParamValue::Vec2(default),
        min: None,
        max: None,
        step: None,
        unit: ParamUnit::Normalized,
        enum_values: Vec::new(),
    }
}

pub(crate) fn vec2_range(
    name: &str,
    default: [f32; 2],
    min: f32,
    max: f32,
    step: f32,
) -> ParamSpec {
    ParamSpec {
        min: Some(min),
        max: Some(max),
        step: Some(step),
        ..vec2(name, default)
    }
}

pub(crate) fn color(name: &str, default: [f32; 4]) -> ParamSpec {
    ParamSpec {
        name: name.into(),
        param_type: ParamType::Color,
        default: ParamValue::Color(default),
        min: None,
        max: None,
        step: None,
        unit: ParamUnit::Color,
        enum_values: Vec::new(),
    }
}

/// Four independent progress thresholds, one per channel, in `0..=1`.
pub(crate) fn thresholds(name: &str, default: [f32; 4]) -> ParamSpec {
    ParamSpec {
        name: name.into(),
        param_type: ParamType::Vec4,
        default: ParamValue::Vec4(default),
        min: Some(0.0),
        max: Some(1.0),
        step: Some(0.01),
        unit: ParamUnit::Normalized,
        enum_values: Vec::new(),
    }
}

pub(crate) fn choice(name: &str, default: u32, values: &[&str]) -> ParamSpec {
    ParamSpec {
        name: name.into(),
        param_type: ParamType::Enum,
        default: ParamValue::Enum(default),
        min: None,
        max: None,
        step: None,
        unit: ParamUnit::Enum,
        enum_values: values.iter().map(|value| (*value).into()).collect(),
    }
}

pub(crate) fn boolean(name: &str, default: bool) -> ParamSpec {
    ParamSpec {
        name: name.into(),
        param_type: ParamType::Bool,
        default: ParamValue::Bool(default),
        min: None,
        max: None,
        step: None,
        unit: ParamUnit::Bool,
        enum_values: Vec::new(),
    }
}
