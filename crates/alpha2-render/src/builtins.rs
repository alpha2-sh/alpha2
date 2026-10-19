use alpha2_gpu::CapabilityTier;
use alpha2_shaders::{
    ANGULAR, CHROMA_KEY_BODY, CIRCLE, CIRCLE_CROP, CIRCLE_OPEN, COLOR_CONTROLS, COLOR_PHASE,
    COLOUR_DISTANCE, CROP, CROSS_WARP, DIRECTIONAL, DIRECTIONAL_WARP, DIRECTIONAL_WIPE, DREAMY,
    EXPANDED_EFFECT, FADE, FADE_COLOR, FADE_GRAYSCALE, FILM_GRAIN, GAUSSIAN_BLUR, GLOW_COMBINE,
    GLOW_THRESHOLD, HEART, LUT_3D_BODY, MIRROR, MORPH, PINWHEEL, PIXELATE_EFFECT, PIXELIZE,
    POLKA_DOTS_CURTAIN, RADIAL, RIPPLE, SHARPEN, SIMPLE_ZOOM, SQUEEZE, TRANSITION_HELPERS,
    VIGNETTE, WIND, WINDOW_BLINDS, WINDOW_SLICE, WIPE_DOWN, WIPE_LEFT, WIPE_RIGHT, WIPE_UP,
};

use crate::{
    EffectDef, EffectId, EffectInputs, EffectKind, EffectResourceKind, ParamSpec, ParamType,
    ParamUnit, ParamValue, Renderer, ResourceSpec, ShaderDiagnostics,
};

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

/// `EffectDef::license` of the effects Alpha2 itself wrote.
pub(crate) const ALPHA2_EFFECT_LICENSE: &str = "MIT; Copyright Alpha2 contributors";

/// Stable identifier of the resource-backed 3D LUT effect.
pub const LUT_3D_ID: &str = "alpha2.effect.lut-3d";

/// Returns fresh definitions for every public built-in clip effect.
#[must_use]
pub fn builtin_effect_definitions() -> Vec<EffectDef> {
    vec![
        gaussian_blur_definition(),
        chroma_key_definition(),
        lut_3d_definition(),
        simple_effect(
            "color-controls",
            COLOR_CONTROLS,
            vec![
                float("exposure", 0.0, -3.0, 3.0),
                float("contrast", 1.0, 0.0, 2.0),
                float("saturation", 1.0, 0.0, 2.0),
                float("temperature", 0.0, -1.0, 1.0),
                float("tint", 0.0, -1.0, 1.0),
            ],
        ),
        simple_effect(
            "vignette",
            VIGNETTE,
            vec![
                float("amount", 0.45, 0.0, 1.0),
                float("radius", 0.72, 0.0, 1.5),
                float("softness", 0.45, 0.01, 1.0),
            ],
        ),
        simple_effect("sharpen", SHARPEN, vec![float("amount", 0.6, 0.0, 3.0)]),
        simple_effect(
            "pixelate",
            PIXELATE_EFFECT,
            vec![float("blocks", 48.0, 2.0, 400.0)],
        ),
        glow_definition(),
        simple_effect(
            "film-grain",
            FILM_GRAIN,
            vec![
                float("amount", 0.18, 0.0, 1.0),
                float("size", 1.4, 0.5, 6.0),
            ],
        ),
        simple_effect(
            "crop",
            CROP,
            vec![
                vec2_range("top_left", [0.0, 0.0], 0.0, 1.0, 0.005),
                vec2_range("bottom_right", [0.0, 0.0], 0.0, 1.0, 0.005),
                float_step("feather", 0.004, 0.0, 0.2, 0.002),
            ],
        ),
        simple_effect(
            "mirror",
            MIRROR,
            vec![
                choice("axis", 0, &["horizontal", "vertical"]),
                boolean("keep_second_half", false),
            ],
        ),
    ]
}

const GL_TRANSITIONS_BASE: &str = "https://github.com/gl-transitions/gl-transitions/blob/902218a1b63773ac0d0d9f491951da3392365bfe/transitions/";

/// Returns fresh, data-only definitions for all built-in transitions.
#[must_use]
pub fn builtin_transition_definitions() -> Vec<EffectDef> {
    let mut definitions = classic_transition_definitions();
    definitions.extend(wipe_transition_definitions());
    definitions.extend(stylized_transition_definitions());
    definitions
}

fn classic_transition_definitions() -> Vec<EffectDef> {
    vec![
        transition("fade", FADE, "fade.glsl", "gre", Vec::new()),
        transition(
            "directional",
            DIRECTIONAL,
            "Directional.glsl",
            "Gaëtan Renaudeau",
            vec![vec2_range("direction", [0.0, 1.0], -1.0, 1.0, 0.1)],
        ),
        transition(
            "circle-open",
            CIRCLE_OPEN,
            "circleopen.glsl",
            "gre",
            vec![float("smoothness", 0.3, 0.0, 1.0), boolean("opening", true)],
        ),
        transition(
            "angular",
            ANGULAR,
            "angular.glsl",
            "Fernando Kuteken",
            vec![degrees("starting_angle", 90.0, -360.0, 360.0)],
        ),
        transition(
            "pixelize",
            PIXELIZE,
            "pixelize.glsl",
            "gre",
            vec![
                vec2_range("squares_min", [20.0, 20.0], 1.0, 100.0, 1.0),
                float("steps", 50.0, 0.0, 200.0),
            ],
        ),
        transition(
            "directional-wipe",
            DIRECTIONAL_WIPE,
            "directionalwipe.glsl",
            "gre",
            vec![
                vec2_range("direction", [1.0, -1.0], -1.0, 1.0, 0.1),
                float("smoothness", 0.5, 0.0, 1.0),
            ],
        ),
        transition(
            "circle",
            CIRCLE,
            "circle.glsl",
            "Fernando Kuteken",
            vec![
                vec2_range("center", [0.5, 0.5], 0.0, 1.0, 0.01),
                color("back_color", [0.1, 0.1, 0.1, 1.0]),
            ],
        ),
        transition(
            "cross-warp",
            CROSS_WARP,
            "crosswarp.glsl",
            "Eke Péter",
            Vec::new(),
        ),
        transition("dreamy", DREAMY, "Dreamy.glsl", "mikolalysenko", Vec::new()),
        transition(
            "radial",
            RADIAL,
            "Radial.glsl",
            "Xaychru",
            vec![float("smoothness", 1.0, 0.0001, 2.0)],
        ),
    ]
}

fn wipe_transition_definitions() -> Vec<EffectDef> {
    vec![
        transition("wipe-up", WIPE_UP, "wipeUp.glsl", "Jake Nelson", Vec::new()),
        transition(
            "wipe-down",
            WIPE_DOWN,
            "wipeDown.glsl",
            "Jake Nelson",
            Vec::new(),
        ),
        transition(
            "wipe-left",
            WIPE_LEFT,
            "wipeLeft.glsl",
            "Jake Nelson",
            Vec::new(),
        ),
        transition(
            "wipe-right",
            WIPE_RIGHT,
            "wipeRight.glsl",
            "Jake Nelson",
            Vec::new(),
        ),
        transition(
            "window-blinds",
            WINDOW_BLINDS,
            "windowblinds.glsl",
            "Fabien Benetou",
            Vec::new(),
        ),
        transition(
            "window-slice",
            WINDOW_SLICE,
            "windowslice.glsl",
            "gre",
            vec![
                float("count", 10.0, 1.0, 100.0),
                float("smoothness", 0.5, 0.0, 1.0),
            ],
        ),
        transition(
            "wind",
            WIND,
            "wind.glsl",
            "gre",
            vec![float("size", 0.2, 0.0001, 1.0), boolean("reversed", false)],
        ),
        transition(
            "pinwheel",
            PINWHEEL,
            "pinwheel.glsl",
            "Mr Speaker",
            vec![float("speed", 2.0, 0.0, 20.0)],
        ),
        transition("heart", HEART, "heart.glsl", "gre", Vec::new()),
    ]
}

fn stylized_transition_definitions() -> Vec<EffectDef> {
    vec![
        transition(
            "color-phase",
            COLOR_PHASE,
            "colorphase.glsl",
            "gre",
            vec![
                thresholds("from_step", [0.0, 0.2, 0.4, 0.0]),
                thresholds("to_step", [0.6, 0.8, 1.0, 1.0]),
            ],
        ),
        transition(
            "fade-color",
            FADE_COLOR,
            "fadecolor.glsl",
            "gre",
            vec![
                color("color", [0.0, 0.0, 0.0, 1.0]),
                float("color_phase", 0.4, 0.0, 1.0),
            ],
        ),
        transition(
            "fade-grayscale",
            FADE_GRAYSCALE,
            "fadegrayscale.glsl",
            "gre",
            vec![float("intensity", 0.3, 0.0, 1.0)],
        ),
        transition(
            "morph",
            MORPH,
            "morph.glsl",
            "paniq",
            vec![float("strength", 0.1, 0.0, 1.0)],
        ),
        transition(
            "ripple",
            RIPPLE,
            "ripple.glsl",
            "gre",
            vec![
                float("amplitude", 100.0, 0.0, 200.0),
                float("speed", 50.0, 0.0, 100.0),
            ],
        ),
        transition(
            "squeeze",
            SQUEEZE,
            "squeeze.glsl",
            "gre",
            vec![float("color_separation", 0.04, 0.0, 0.5)],
        ),
        transition(
            "simple-zoom",
            SIMPLE_ZOOM,
            "SimpleZoom.glsl",
            "0gust1",
            vec![float("zoom_quickness", 0.8, 0.2, 1.0)],
        ),
        transition(
            "circle-crop",
            CIRCLE_CROP,
            "CircleCrop.glsl",
            "fkuteken",
            vec![color("back_color", [0.0, 0.0, 0.0, 1.0])],
        ),
        transition(
            "colour-distance",
            COLOUR_DISTANCE,
            "ColourDistance.glsl",
            "P-Seebauer",
            vec![float("power", 5.0, 0.0, 20.0)],
        ),
        transition(
            "polka-dots-curtain",
            POLKA_DOTS_CURTAIN,
            "PolkaDotsCurtain.glsl",
            "bobylito",
            vec![
                float("dots", 20.0, 1.0, 100.0),
                vec2_range("center", [0.0, 0.0], 0.0, 1.0, 0.01),
            ],
        ),
        transition(
            "directional-warp",
            DIRECTIONAL_WARP,
            "directionalwarp.glsl",
            "pschroen",
            vec![
                float("smoothness", 0.1, 0.0001, 1.0),
                vec2_range("direction", [-1.0, 1.0], -1.0, 1.0, 0.1),
            ],
        ),
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
        inputs: EffectInputs::One,
        params: vec![
            color("key_color", [0.0, 1.0, 0.0, 1.0]),
            float("threshold", 0.35, 0.0, 3.0_f32.sqrt()),
            float("softness", 0.1, f32::EPSILON, 1.0),
            float("spill_suppression", 0.2, 0.0, 1.0),
            choice("distance_mode", 0, &["rgb", "chroma"]),
        ],
        resources: Vec::new(),
        wgsl_body: CHROMA_KEY_BODY.into(),
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
        inputs: EffectInputs::One,
        params: vec![float_step("radius", 0.012, 0.0, 1.0, 0.001)],
        resources: Vec::new(),
        wgsl_body: EXPANDED_EFFECT.into(),
        tier_min: CapabilityTier::TierB,
        license: Some(ALPHA2_EFFECT_LICENSE.into()),
    }
}

pub(crate) fn gaussian_blur_pass_definition() -> EffectDef {
    EffectDef {
        id: GAUSSIAN_BLUR_PASS_ID.into(),
        version: 1,
        kind: EffectKind::Effect,
        inputs: EffectInputs::One,
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
        wgsl_body: GAUSSIAN_BLUR.into(),
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
        inputs: EffectInputs::One,
        params: vec![
            float("threshold", 0.6, 0.0, 1.0),
            float("intensity", 0.8, 0.0, 3.0),
            float("radius", 0.012, 0.0, 0.06),
        ],
        resources: Vec::new(),
        wgsl_body: EXPANDED_EFFECT.into(),
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
        inputs: EffectInputs::Two,
        params: vec![float("intensity", 0.8, 0.0, 3.0)],
        resources: Vec::new(),
        wgsl_body: GLOW_COMBINE.into(),
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
        inputs: EffectInputs::One,
        params: vec![float("strength", 1.0, 0.0, 1.0)],
        resources: vec![ResourceSpec {
            name: "lut".into(),
            kind: EffectResourceKind::Lut3d,
        }],
        wgsl_body: LUT_3D_BODY.into(),
        tier_min: CapabilityTier::TierB,
        license: Some(ALPHA2_EFFECT_LICENSE.into()),
    }
}

/// The passes the engine inserts itself, hidden from the effect catalog.
pub(crate) fn internal_effect_definitions() -> [EffectDef; 6] {
    [
        gaussian_blur_pass_definition(),
        glow_threshold_definition(),
        glow_combine_definition(),
        crate::masking::shape_mask_definition(),
        crate::masking::track_matte_definition(),
        crate::generator::generator_definition(),
    ]
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

fn simple_effect(name: &str, body: &str, params: Vec<ParamSpec>) -> EffectDef {
    EffectDef {
        id: format!("alpha2.effect.{name}"),
        version: 1,
        kind: EffectKind::Effect,
        inputs: EffectInputs::One,
        params,
        resources: Vec::new(),
        wgsl_body: body.into(),
        tier_min: CapabilityTier::TierB,
        license: Some(ALPHA2_EFFECT_LICENSE.into()),
    }
}

fn transition(
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
        inputs: EffectInputs::Two,
        params,
        resources: Vec::new(),
        wgsl_body: format!("{TRANSITION_HELPERS}\n{body}"),
        tier_min: CapabilityTier::TierB,
        license: Some(format!(
            "MIT; Author: {author}; Source: {GL_TRANSITIONS_BASE}{source}"
        )),
    }
}

fn float(name: &str, default: f32, min: f32, max: f32) -> ParamSpec {
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

fn float_step(name: &str, default: f32, min: f32, max: f32, step: f32) -> ParamSpec {
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

fn degrees(name: &str, default: f32, min: f32, max: f32) -> ParamSpec {
    ParamSpec {
        unit: ParamUnit::Degrees,
        step: Some(1.0),
        ..float(name, default, min, max)
    }
}

fn vec2(name: &str, default: [f32; 2]) -> ParamSpec {
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

fn vec2_range(name: &str, default: [f32; 2], min: f32, max: f32, step: f32) -> ParamSpec {
    ParamSpec {
        min: Some(min),
        max: Some(max),
        step: Some(step),
        ..vec2(name, default)
    }
}

fn color(name: &str, default: [f32; 4]) -> ParamSpec {
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
fn thresholds(name: &str, default: [f32; 4]) -> ParamSpec {
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

fn choice(name: &str, default: u32, values: &[&str]) -> ParamSpec {
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

fn boolean(name: &str, default: bool) -> ParamSpec {
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
