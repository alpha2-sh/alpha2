//! Built-in effects a render graph draws as several internal passes.
//!
//! Gaussians are separable and compose in quadrature, so a blur of any width is a chain of
//! narrow one-axis passes reading at most [`PASS_REACH`] taps per side. The first pass samples
//! every texel; later passes space taps by the blur accumulated so far, over a picture already
//! smooth at that scale, so no gaps appear. Each pass widens the blur about four times.

use std::sync::LazyLock;

use crate::{
    EffectDef, EffectId, EffectInputs, EffectParams, EffectResources, EffectRuntime, GraphError,
    ParamValue, RenderError, RenderGraph, RenderNodeId,
    builtins::{
        DIRECTIONAL_BLUR_ID, DROP_SHADOW_ID, FOCUS_COMBINE_ID, GAUSSIAN_BLUR_ID,
        GAUSSIAN_BLUR_PASS_ID, GLOW_COMBINE_ID, GLOW_ID, GLOW_THRESHOLD_ID, SHADOW_COMBINE_ID,
        SHADOW_OFFSET_ID, TILT_SHIFT_ID, UNSHARP_COMBINE_ID, UNSHARP_MASK_ID,
        directional_blur_definition, drop_shadow_definition, gaussian_blur_definition,
        glow_definition, tilt_shift_definition, unsharp_mask_definition,
    },
};

/// Taps one blur pass reads on either side of its centre.
const PASS_REACH: f32 = 12.0;

/// Widest standard deviation of one pass, in taps; the reach covers three.
pub(crate) const MAX_PASS_SIGMA_TAPS: f32 = PASS_REACH / 3.0;

/// More than any canvas needs; a 4320-pixel deviation takes six.
const MAX_PASSES_PER_AXIS: usize = 16;

/// Definitions expanded built-ins are checked against, built once.
static GAUSSIAN_BLUR: LazyLock<EffectDef> = LazyLock::new(gaussian_blur_definition);
static GLOW: LazyLock<EffectDef> = LazyLock::new(glow_definition);
static DIRECTIONAL_BLUR: LazyLock<EffectDef> = LazyLock::new(directional_blur_definition);
static UNSHARP_MASK: LazyLock<EffectDef> = LazyLock::new(unsharp_mask_definition);
static TILT_SHIFT: LazyLock<EffectDef> = LazyLock::new(tilt_shift_definition);
static DROP_SHADOW: LazyLock<EffectDef> = LazyLock::new(drop_shadow_definition);

/// Standard deviation in pixels of the built-in blur of `radius`: half the radius, as a
/// fraction of the shorter side.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn blur_sigma_pixels(radius: f32, size: crate::RenderSize) -> f32 {
    radius * 0.5 * size.width.min(size.height) as f32
}

/// Internal effects an expanded built-in draws with; `None` for other effects.
pub(crate) fn expanded_passes(effect: &str) -> Option<&'static [&'static str]> {
    match effect {
        GAUSSIAN_BLUR_ID | DIRECTIONAL_BLUR_ID => Some(&[GAUSSIAN_BLUR_PASS_ID]),
        GLOW_ID => Some(&[GLOW_THRESHOLD_ID, GAUSSIAN_BLUR_PASS_ID, GLOW_COMBINE_ID]),
        UNSHARP_MASK_ID => Some(&[GAUSSIAN_BLUR_PASS_ID, UNSHARP_COMBINE_ID]),
        TILT_SHIFT_ID => Some(&[GAUSSIAN_BLUR_PASS_ID, FOCUS_COMBINE_ID]),
        DROP_SHADOW_ID => Some(&[SHADOW_OFFSET_ID, GAUSSIAN_BLUR_PASS_ID, SHADOW_COMBINE_ID]),
        _ => None,
    }
}

impl<'a> RenderGraph<'a> {
    /// Adds `effect` as its internal passes when it is an expanded built-in; `None` otherwise.
    pub(crate) fn add_expanded_effect(
        &mut self,
        effect: &EffectId,
        source: RenderNodeId,
        params: &EffectParams,
        runtime: EffectRuntime,
    ) -> Option<Result<RenderNodeId, GraphError>> {
        match effect.as_str() {
            GAUSSIAN_BLUR_ID => Some(self.add_gaussian_blur(source, params, runtime)),
            GLOW_ID => Some(self.add_glow(source, params, runtime)),
            DIRECTIONAL_BLUR_ID => Some(self.add_directional_blur(source, params, runtime)),
            UNSHARP_MASK_ID => Some(self.add_unsharp_mask(source, params, runtime)),
            TILT_SHIFT_ID => Some(self.add_tilt_shift(source, params, runtime)),
            DROP_SHADOW_ID => Some(self.add_drop_shadow(source, params, runtime)),
            _ => None,
        }
    }

    fn add_gaussian_blur(
        &mut self,
        source: RenderNodeId,
        params: &EffectParams,
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        let [radius] = checked_params(&GAUSSIAN_BLUR, params, ["radius"])?;
        self.add_gaussian_cascade(source, blur_sigma_pixels(radius, self.size), runtime)
    }

    #[allow(clippy::cast_precision_loss)]
    fn add_drop_shadow(
        &mut self,
        source: RenderNodeId,
        params: &EffectParams,
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        let [color, opacity, angle, distance, softness, shadow_only] = checked_values(
            &DROP_SHADOW,
            params,
            [
                "color",
                "opacity",
                "angle",
                "distance",
                "softness",
                "shadow_only",
            ],
        )?;
        let (
            ParamValue::Float(opacity),
            ParamValue::Float(angle),
            ParamValue::Float(distance),
            ParamValue::Float(softness),
        ) = (opacity, angle, distance, softness)
        else {
            return Err(RenderError::InvalidEffectParams.into());
        };
        // Counter-clockwise on screen with y down; distance in shorter sides, offset in uv.
        let radians = angle.to_radians();
        let unit = self.unit_pixels();
        let (unit_over_width, unit_over_height) = (
            unit / self.size.width as f32,
            unit / self.size.height as f32,
        );
        let offset = [
            libm::cosf(radians) * distance * unit_over_width,
            -libm::sinf(radians) * distance * unit_over_height,
        ];
        let soft = self.add_shadow_passes(
            source,
            offset,
            softness * 0.5 * unit,
            color,
            opacity,
            runtime,
        )?;
        self.add_transition(
            EffectId::from_static(SHADOW_COMBINE_ID, 1),
            source,
            soft,
            EffectParams::new().with("shadow_only", shadow_only),
            EffectRuntime {
                progress: 0.5,
                ..runtime
            },
        )
    }

    /// The shadow of `source`'s alpha alone, offset by `offset_pixels` (+x right, +y down) and
    /// blurred by `sigma_pixels`, in straight sRGB `color` whose alpha is the opacity.
    ///
    /// Uses the drop-shadow effect's passes so text shadows match it, on the CPU too. A
    /// non-positive deviation leaves the shadow sharp.
    ///
    /// # Errors
    /// [`GraphError::InvalidDependency`] unless `source` is earlier, or
    /// [`RenderError::InvalidEffectParams`] for an out-of-range color.
    #[allow(clippy::cast_precision_loss)]
    pub fn add_shadow(
        &mut self,
        source: RenderNodeId,
        offset_pixels: [f32; 2],
        sigma_pixels: f32,
        color: [f32; 4],
    ) -> Result<RenderNodeId, GraphError> {
        if !color.iter().all(|value| (0.0..=1.0).contains(value))
            || !offset_pixels.iter().all(|value| value.is_finite())
        {
            return Err(RenderError::InvalidEffectParams.into());
        }
        let offset = [
            offset_pixels[0] / self.size.width as f32,
            offset_pixels[1] / self.size.height as f32,
        ];
        let runtime = EffectRuntime::default();
        if sigma_pixels > 0.0 {
            return self.add_shadow_passes(
                source,
                offset,
                sigma_pixels,
                ParamValue::Color(color),
                1.0,
                runtime,
            );
        }
        self.add_internal_effect(
            SHADOW_OFFSET_ID,
            source,
            EffectParams::new()
                .with("offset", ParamValue::Vec2(offset))
                .with("color", ParamValue::Color(color))
                .with("opacity", ParamValue::Float(1.0)),
            runtime,
        )
    }

    /// The drop shadow's first two passes: `source` alpha offset by `offset` (uv) in `color` at
    /// `opacity`, blurred by `sigma_pixels`.
    fn add_shadow_passes(
        &mut self,
        source: RenderNodeId,
        offset: [f32; 2],
        sigma_pixels: f32,
        color: ParamValue,
        opacity: f32,
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        let shadow = self.add_internal_effect(
            SHADOW_OFFSET_ID,
            source,
            EffectParams::new()
                .with("offset", ParamValue::Vec2(offset))
                .with("color", color)
                .with("opacity", ParamValue::Float(opacity)),
            runtime,
        )?;
        self.add_gaussian_cascade(shadow, sigma_pixels, runtime)
    }

    fn add_glow(
        &mut self,
        source: RenderNodeId,
        params: &EffectParams,
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        let [threshold, intensity, radius] =
            checked_params(&GLOW, params, ["threshold", "intensity", "radius"])?;
        let bright = self.add_internal_effect(
            GLOW_THRESHOLD_ID,
            source,
            EffectParams::new().with("threshold", ParamValue::Float(threshold)),
            runtime,
        )?;
        let bloom = self.add_gaussian(bright, radius * 0.5, runtime)?;
        self.add_transition(
            EffectId::from_static(GLOW_COMBINE_ID, 1),
            source,
            bloom,
            EffectParams::new().with("intensity", ParamValue::Float(intensity)),
            EffectRuntime {
                progress: 0.5,
                ..runtime
            },
        )
    }

    /// One axis of a Gaussian, `angle` degrees counter-clockwise on screen.
    #[allow(clippy::cast_precision_loss)]
    fn add_directional_blur(
        &mut self,
        source: RenderNodeId,
        params: &EffectParams,
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        let [length, angle] = checked_params(&DIRECTIONAL_BLUR, params, ["length", "angle"])?;
        // libm keeps the CPU renderer's graph identical on every target.
        let radians = angle.to_radians();
        let direction = [libm::cosf(radians), -libm::sinf(radians)];
        let sigma_pixels = length * 0.5 * self.unit_pixels();
        self.add_gaussian_axis(source, sigma_pixels, direction, runtime)
    }

    fn add_unsharp_mask(
        &mut self,
        source: RenderNodeId,
        params: &EffectParams,
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        let [amount, radius, threshold] =
            checked_params(&UNSHARP_MASK, params, ["amount", "radius", "threshold"])?;
        let blurred = self.add_gaussian(source, radius * 0.5, runtime)?;
        self.add_transition(
            EffectId::from_static(UNSHARP_COMBINE_ID, 1),
            source,
            blurred,
            EffectParams::new()
                .with("amount", ParamValue::Float(amount))
                .with("threshold", ParamValue::Float(threshold)),
            EffectRuntime {
                progress: 0.5,
                ..runtime
            },
        )
    }

    /// A quarter-`blur` blur, and a half-`blur` one built from it (Gaussians add in quadrature),
    /// mixed into the source by distance from the band.
    fn add_tilt_shift(
        &mut self,
        source: RenderNodeId,
        params: &EffectParams,
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        let [blur, position, angle, focus, falloff] = checked_params(
            &TILT_SHIFT,
            params,
            ["blur", "position", "angle", "focus", "falloff"],
        )?;
        let (narrow, wide) = (blur * 0.25, blur * 0.5);
        let soft = self.add_gaussian(source, narrow, runtime)?;
        let rest = (wide * wide - narrow * narrow).max(0.0).sqrt();
        let blurred = self.add_gaussian(soft, rest, runtime)?;
        self.add_effect_with_inputs(
            EffectId::from_static(FOCUS_COMBINE_ID, 1),
            source,
            EffectInputs::new()
                .with("soft", soft)
                .with("blurred", blurred),
            EffectParams::new()
                .with("position", ParamValue::Float(position))
                .with("angle", ParamValue::Float(angle))
                .with("focus", ParamValue::Float(focus))
                .with("falloff", ParamValue::Float(falloff)),
            EffectResources::new(),
            runtime,
        )
    }

    /// The output's shorter side in pixels, the unit of these effects' lengths, so portrait and
    /// landscape blur alike.
    #[allow(clippy::cast_precision_loss)]
    fn unit_pixels(&self) -> f32 {
        self.size.width.min(self.size.height) as f32
    }

    /// A Gaussian of `sigma` shorter sides, as horizontal then vertical cascades.
    fn add_gaussian(
        &mut self,
        source: RenderNodeId,
        sigma: f32,
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        self.add_gaussian_cascade(source, sigma * self.unit_pixels(), runtime)
    }

    /// Adds a Gaussian blur of `sigma_pixels` on both axes, drawn like the built-in blur within the
    /// filter-sample budget. Used for mask feathering. A non-finite or non-positive deviation
    /// returns `source`.
    ///
    /// # Errors
    /// [`GraphError::InvalidDependency`] unless `source` is earlier.
    #[allow(clippy::cast_precision_loss)]
    pub fn add_gaussian_pixels(
        &mut self,
        source: RenderNodeId,
        sigma_pixels: f32,
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        if source.graph != self.id || source.index >= self.nodes.len() {
            return Err(GraphError::InvalidDependency);
        }
        if sigma_pixels.is_nan() || sigma_pixels <= 0.0 || !sigma_pixels.is_finite() {
            return Ok(source);
        }
        self.add_gaussian_cascade(source, sigma_pixels, runtime)
    }

    /// Pass cascades of a Gaussian of `sigma_pixels`; one identity pass per axis at zero.
    fn add_gaussian_cascade(
        &mut self,
        source: RenderNodeId,
        sigma_pixels: f32,
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        let node = self.add_gaussian_axis(source, sigma_pixels, [1.0, 0.0], runtime)?;
        self.add_gaussian_axis(node, sigma_pixels, [0.0, 1.0], runtime)
    }

    /// One-axis Gaussian of `sigma_pixels` along unit `direction` (y down) as a cascade; one
    /// identity pass at zero. Tap spacing is in pixels along `direction`, so diagonal blurs match
    /// upright ones.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn add_gaussian_axis(
        &mut self,
        source: RenderNodeId,
        sigma_pixels: f32,
        direction: [f32; 2],
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        let extent = [self.size.width as f32, self.size.height as f32];
        let mut node = source;
        let mut blurred = 0.0_f32;
        let mut spacing = 1.0_f32;
        for _ in 0..MAX_PASSES_PER_AXIS {
            let remaining = (sigma_pixels * sigma_pixels - blurred * blurred)
                .max(0.0)
                .sqrt();
            let pass = remaining.min(MAX_PASS_SIGMA_TAPS * spacing);
            let step = [
                direction[0] * spacing / extent[0],
                direction[1] * spacing / extent[1],
            ];
            node = self.add_internal_effect(
                GAUSSIAN_BLUR_PASS_ID,
                node,
                EffectParams::new()
                    .with("step", ParamValue::Vec2(step))
                    .with("sigma", ParamValue::Float(pass / spacing)),
                runtime,
            )?;
            blurred = libm::hypotf(blurred, pass);
            if pass < MAX_PASS_SIGMA_TAPS * spacing {
                break;
            }
            spacing = blurred;
        }
        Ok(node)
    }

    pub(crate) fn add_internal_effect(
        &mut self,
        id: &'static str,
        source: RenderNodeId,
        params: EffectParams<'a>,
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        self.add_effect_with_resources(
            EffectId::from_static(id, 1),
            source,
            params,
            EffectResources::new(),
            runtime,
        )
    }
}

/// `definition`'s named parameters from `params` with defaults filled in, checked as a pass
/// would, of any type.
pub(crate) fn checked_values<const N: usize>(
    definition: &EffectDef,
    params: &EffectParams,
    names: [&str; N],
) -> Result<[ParamValue; N], GraphError> {
    crate::runtime_effect::encode_params(&definition.params, params)?;
    let mut values = std::array::from_fn(|_| ParamValue::Float(0.0));
    for (value, name) in values.iter_mut().zip(names) {
        let spec = definition
            .params
            .iter()
            .find(|spec| spec.name == name)
            .ok_or(RenderError::InvalidEffectParams)?;
        *value = params.get(name).unwrap_or(&spec.default).clone();
    }
    Ok(values)
}

/// `definition`'s named float parameters from `params` with defaults filled in, checked as a
/// pass would.
pub(crate) fn checked_params<const N: usize>(
    definition: &EffectDef,
    params: &EffectParams,
    names: [&str; N],
) -> Result<[f32; N], GraphError> {
    crate::runtime_effect::encode_params(&definition.params, params)?;
    let mut values = [0.0; N];
    for (value, name) in values.iter_mut().zip(names) {
        let spec = definition
            .params
            .iter()
            .find(|spec| spec.name == name)
            .ok_or(RenderError::InvalidEffectParams)?;
        let ParamValue::Float(supplied) = params.get(name).unwrap_or(&spec.default) else {
            return Err(RenderError::InvalidEffectParams.into());
        };
        *value = *supplied;
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::checked_values;
    use crate::{EffectParams, ParamValue, chroma_key_definition};

    #[test]
    fn checked_values_fill_defaults_and_refuse_a_wrong_type() {
        let definition = chroma_key_definition();
        let supplied = EffectParams::new()
            .with("key_color", ParamValue::Color([0.2, 0.4, 0.6, 1.0]))
            .with("distance_mode", ParamValue::Enum(1));
        let [key, mode, softness] = checked_values(
            &definition,
            &supplied,
            ["key_color", "distance_mode", "softness"],
        )
        .expect("valid parameters");
        assert_eq!(key, ParamValue::Color([0.2, 0.4, 0.6, 1.0]));
        assert_eq!(mode, ParamValue::Enum(1));
        assert_eq!(softness, ParamValue::Float(0.1));
        let wrong = EffectParams::new().with("key_color", ParamValue::Float(1.0));
        assert!(checked_values(&definition, &wrong, ["key_color"]).is_err());
        assert!(checked_values(&definition, &EffectParams::new(), ["missing"]).is_err());
    }
}
