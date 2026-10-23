//! The built-in effects a render graph draws as several internal passes.
//!
//! A Gaussian is separable and the sum of Gaussians is a Gaussian, so a blur
//! of any width is a chain of narrow one-axis passes. Each pass reads at most
//! [`PASS_REACH`] taps either side, `MAX_PASS_SIGMA_TAPS` standard deviations
//! wide. The first pass samples every texel; every later pass samples taps as
//! far apart as the blur accumulated so far, a picture already smooth at that
//! scale, so no pass leaves gaps between its taps. The pass count grows with
//! the logarithm of the width: about four times wider per pass.

use std::sync::LazyLock;

use crate::{
    EffectDef, EffectId, EffectParams, EffectResources, EffectRuntime, GraphError, ParamValue,
    RenderError, RenderGraph, RenderNodeId,
    builtins::{
        GAUSSIAN_BLUR_ID, GAUSSIAN_BLUR_PASS_ID, GLOW_COMBINE_ID, GLOW_ID, GLOW_THRESHOLD_ID,
        gaussian_blur_definition, glow_definition,
    },
};

/// Taps one blur pass reads on either side of its centre.
const PASS_REACH: f32 = 12.0;

/// Widest standard deviation one pass draws, in taps: its reach covers three.
pub(crate) const MAX_PASS_SIGMA_TAPS: f32 = PASS_REACH / 3.0;

/// More passes than any canvas needs: a 4320-pixel deviation takes six.
const MAX_PASSES_PER_AXIS: usize = 16;

/// The definitions whose parameters an expanded built-in is checked
/// against, made once rather than for every graph that draws one.
static GAUSSIAN_BLUR: LazyLock<EffectDef> = LazyLock::new(gaussian_blur_definition);
static GLOW: LazyLock<EffectDef> = LazyLock::new(glow_definition);

/// The internal effects a built-in drawn as several passes draws with, for
/// [`RenderGraph::add_expanded_effect`]'s effects; `None` for every other.
pub(crate) fn expanded_passes(effect: &str) -> Option<&'static [&'static str]> {
    match effect {
        GAUSSIAN_BLUR_ID => Some(&[GAUSSIAN_BLUR_PASS_ID]),
        GLOW_ID => Some(&[GLOW_THRESHOLD_ID, GAUSSIAN_BLUR_PASS_ID, GLOW_COMBINE_ID]),
        _ => None,
    }
}

impl<'a> RenderGraph<'a> {
    /// Adds `effect` as the internal passes that draw it, when it is a
    /// built-in drawn that way; `None` for every other effect.
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
        self.add_gaussian(source, radius * 0.5, runtime)
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

    /// A Gaussian with a standard deviation of `sigma` output heights, as
    /// horizontal then vertical pass cascades.
    #[allow(clippy::cast_precision_loss)]
    fn add_gaussian(
        &mut self,
        source: RenderNodeId,
        sigma: f32,
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        self.add_gaussian_cascade(source, sigma * self.size.height as f32, runtime)
    }

    /// Adds a Gaussian blur of `source` whose standard deviation is
    /// `sigma_pixels` of the graph's pixels on both axes, drawn as the
    /// built-in blur draws: narrow one-axis passes, within the renderer's
    /// filter-sample budget. A mask's feather softens its coverage with it.
    /// A deviation of zero or less returns `source` itself.
    ///
    /// # Errors
    /// Returns [`GraphError::InvalidDependency`] unless `source` is earlier.
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

    /// The pass cascades of a Gaussian of `sigma_pixels`; one identity pass
    /// per axis at zero.
    #[allow(clippy::cast_precision_loss)]
    fn add_gaussian_cascade(
        &mut self,
        source: RenderNodeId,
        sigma_pixels: f32,
        runtime: EffectRuntime,
    ) -> Result<RenderNodeId, GraphError> {
        let mut node = source;
        for (axis, extent) in [(0, self.size.width), (1, self.size.height)] {
            let mut blurred = 0.0_f32;
            let mut spacing = 1.0_f32;
            for _ in 0..MAX_PASSES_PER_AXIS {
                let remaining = (sigma_pixels * sigma_pixels - blurred * blurred)
                    .max(0.0)
                    .sqrt();
                let pass = remaining.min(MAX_PASS_SIGMA_TAPS * spacing);
                let mut step = [0.0; 2];
                step[axis] = spacing / extent as f32;
                node = self.add_internal_effect(
                    GAUSSIAN_BLUR_PASS_ID,
                    node,
                    EffectParams::new()
                        .with("step", ParamValue::Vec2(step))
                        .with("sigma", ParamValue::Float(pass / spacing)),
                    runtime,
                )?;
                blurred = blurred.hypot(pass);
                if pass < MAX_PASS_SIGMA_TAPS * spacing {
                    break;
                }
                spacing = blurred;
            }
        }
        Ok(node)
    }

    fn add_internal_effect(
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

/// The named float parameters of `definition` as `params` gives them, its
/// defaults filling the rest, checked as a pass would check them.
fn checked_params<const N: usize>(
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
