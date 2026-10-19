use alpha2_model::ProjectSettings;
use alpha2_render::RenderSize;

use crate::EngineError;

// The project model and the renderer bound a canvas independently. They must stay the same limit
// so a canvas the model accepts renders at full scale.
const _: () = assert!(
    alpha2_model::MAX_CANVAS_DIMENSION == RenderSize::MAX_DIMENSION
        && alpha2_model::MAX_CANVAS_PIXELS == RenderSize::MAX_PIXELS
);

/// Runtime quality controls shared by real-time playback and export.
#[derive(Copy, Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct QualityProfile {
    /// Output scale relative to project dimensions.
    pub resolution_scale: f32,
    /// Maximum sampling work available to a filter pass.
    pub max_filter_samples: u32,
    /// Temporal samples used by motion blur implementations.
    pub motion_blur_subframes: u32,
    /// Soft budget available to reusable GPU textures.
    pub texture_budget_bytes: u64,
}

impl Default for QualityProfile {
    fn default() -> Self {
        Self::export()
    }
}

impl QualityProfile {
    /// Largest supported multiplier for project dimensions.
    pub const MAX_RESOLUTION_SCALE: f32 = 8.0;
    /// Largest per-pass filter sampling budget: the most any pass takes
    /// ([`alpha2_render::RenderQualityControls::MAX_FILTER_SAMPLES`]). A
    /// Tier-B renderer applies less, and reports it.
    pub const MAX_FILTER_SAMPLES: u32 = alpha2_render::RenderQualityControls::MAX_FILTER_SAMPLES;
    /// Largest temporal sample count allocated for one render plan.
    pub const MAX_MOTION_BLUR_SUBFRAMES: u32 = 256;
    /// Smallest useful reusable-texture pool budget.
    pub const MIN_TEXTURE_BUDGET_BYTES: u64 = 1024 * 1024;
    /// Largest accepted reusable-texture pool budget.
    pub const MAX_TEXTURE_BUDGET_BYTES: u64 = 64 * 1024 * 1024 * 1024;

    /// Full-resolution deterministic export defaults.
    #[must_use]
    pub const fn export() -> Self {
        Self {
            resolution_scale: 1.0,
            max_filter_samples: 64,
            motion_blur_subframes: 1,
            texture_budget_bytes: 512 * 1024 * 1024,
        }
    }

    /// Responsive half-resolution playback defaults.
    #[must_use]
    pub const fn preview() -> Self {
        Self {
            resolution_scale: 0.5,
            max_filter_samples: 32,
            motion_blur_subframes: 1,
            texture_budget_bytes: 256 * 1024 * 1024,
        }
    }

    /// Checks that all controls are finite and within runtime-safe bounds.
    ///
    /// # Errors
    /// Returns [`EngineError::InvalidQualityProfile`] for out-of-range values.
    pub fn validate(self) -> Result<(), EngineError> {
        if self.resolution_scale.is_finite()
            && self.resolution_scale > 0.0
            && self.resolution_scale <= Self::MAX_RESOLUTION_SCALE
            && (1..=Self::MAX_FILTER_SAMPLES).contains(&self.max_filter_samples)
            && (1..=Self::MAX_MOTION_BLUR_SUBFRAMES).contains(&self.motion_blur_subframes)
            && (Self::MIN_TEXTURE_BUDGET_BYTES..=Self::MAX_TEXTURE_BUDGET_BYTES)
                .contains(&self.texture_budget_bytes)
        {
            Ok(())
        } else {
            Err(EngineError::InvalidQualityProfile)
        }
    }

    /// Resolves scaled project dimensions, rounding to the nearest pixel.
    ///
    /// # Errors
    /// Returns [`EngineError`] for an invalid profile or unrepresentable size.
    pub fn render_size(self, settings: &ProjectSettings) -> Result<RenderSize, EngineError> {
        self.validate()?;
        let width = scaled_dimension(settings.width, self.resolution_scale)?;
        let height = scaled_dimension(settings.height, self.resolution_scale)?;
        RenderSize::new(width, height).map_err(EngineError::from)
    }

    /// The size of the 4:2:0 frames an export at this profile renders: the
    /// [render size](Self::render_size), which must be even in both
    /// dimensions. Every export entry point refuses an odd one with this
    /// before it renders, so the error names the nearest even size instead
    /// of the readback failing deep inside the GPU work.
    ///
    /// # Errors
    /// Returns [`EngineError::OddExportDimensions`], or what
    /// [`Self::render_size`] returns.
    pub fn export_size(self, settings: &ProjectSettings) -> Result<RenderSize, EngineError> {
        let size = self.render_size(settings)?;
        if size.width % 2 != 0 || size.height % 2 != 0 {
            let even = |value: u32| match value {
                1 => 2,
                odd if odd % 2 != 0 => odd - 1,
                even => even,
            };
            return Err(EngineError::OddExportDimensions {
                width: size.width,
                height: size.height,
                suggested: (even(size.width), even(size.height)),
            });
        }
        Ok(size)
    }

    /// Captures project, adapter, driver, backend, and quality facts for a benchmark.
    ///
    /// # Errors
    /// Returns [`EngineError`] when the profile cannot resolve output dimensions.
    pub fn benchmark_metadata(
        self,
        project: impl Into<String>,
        settings: &ProjectSettings,
        renderer: &alpha2_render::Renderer,
    ) -> Result<alpha2_render::BenchmarkMetadata, EngineError> {
        let size = self.render_size(settings)?;
        Ok(alpha2_render::BenchmarkMetadata {
            project: project.into(),
            alpha2_version: env!("CARGO_PKG_VERSION"),
            adapter: renderer.diagnostics().adapter,
            dimensions: (size.width, size.height),
            resolution_scale: self.resolution_scale,
            max_filter_samples: self.max_filter_samples,
            motion_blur_subframes: self.motion_blur_subframes,
            texture_budget_bytes: self.texture_budget_bytes,
        })
    }
}

fn scaled_dimension(value: u32, scale: f32) -> Result<u32, EngineError> {
    let scaled = f64::from(value) * f64::from(scale);
    if scaled.is_finite() && scaled <= f64::from(u32::MAX) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok((scaled.round() as u32).max(1))
    } else {
        Err(EngineError::OutOfRange)
    }
}

#[cfg(test)]
mod tests {
    use alpha2_time::FrameRate;

    use super::*;

    #[test]
    fn quality_scales_dimensions_and_rejects_zero() {
        let settings = ProjectSettings::new(
            1920,
            1080,
            FrameRate::new(30, 1).expect("valid rate"),
            48_000,
        )
        .expect("valid settings");
        assert_eq!(
            QualityProfile::preview()
                .render_size(&settings)
                .expect("valid size"),
            RenderSize::new(960, 540).expect("valid size")
        );
        assert_eq!(
            QualityProfile {
                resolution_scale: 0.0,
                ..QualityProfile::default()
            }
            .validate(),
            Err(EngineError::InvalidQualityProfile)
        );
        assert_eq!(
            QualityProfile {
                motion_blur_subframes: u32::MAX,
                ..QualityProfile::default()
            }
            .validate(),
            Err(EngineError::InvalidQualityProfile)
        );
        assert_eq!(
            QualityProfile {
                texture_budget_bytes: 1,
                ..QualityProfile::default()
            }
            .validate(),
            Err(EngineError::InvalidQualityProfile)
        );
    }
}
