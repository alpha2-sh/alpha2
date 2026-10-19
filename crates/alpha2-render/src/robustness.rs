//! Capability-driven, observable degradation policy.

use alpha2_gpu::{Capabilities, CapabilityTier, IntermediateFormat};

/// One deterministic fallback selected from adapter facts and caller budgets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityDiagnostic {
    /// Half-float filtering is unavailable; the renderer uses normalized RGBA8.
    Rgba8Intermediate,
    /// Caller memory pressure reduced the idle texture budget.
    TextureBudgetReduced {
        /// Original caller request.
        requested: u64,
        /// Budget accepted by the policy.
        applied: u64,
    },
    /// Caller decoder-session limit prevents opening another decoder.
    DecoderSessionLimit {
        /// Caller-configured maximum session count.
        limit: u32,
    },
    /// A filter sample budget past what a pass takes was reduced to it:
    /// [`crate::RenderQualityControls::MAX_FILTER_SAMPLES`], or less on a
    /// Tier-B (WebGL2, GL ES) device.
    FilterSamplesReduced {
        /// Original caller request.
        requested: u32,
        /// The samples a pass takes.
        applied: u32,
    },
}

/// Caller-owned resource envelope; zero means the corresponding resource is unavailable.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ResourceBudget {
    /// Soft GPU texture bytes.
    pub texture_bytes: u64,
    /// Maximum concurrent decoder sessions.
    pub decoder_sessions: u32,
    /// Decoder sessions already active in the host.
    pub active_decoder_sessions: u32,
}

impl ResourceBudget {
    /// Creates a caller-owned resource envelope.
    #[must_use]
    pub const fn new(
        texture_bytes: u64,
        decoder_sessions: u32,
        active_decoder_sessions: u32,
    ) -> Self {
        Self {
            texture_bytes,
            decoder_sessions,
            active_decoder_sessions,
        }
    }
}

/// Effective controls and every observable fallback decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DegradationDecision {
    /// Applied texture budget.
    pub texture_bytes: u64,
    /// Applied maximum filter sample count.
    pub max_filter_samples: u32,
    /// Whether another decoder may be opened.
    pub decoder_available: bool,
    /// Explicit fallback diagnostics.
    pub diagnostics: Vec<CapabilityDiagnostic>,
}

/// The most filter samples a pass takes on a device of `tier`:
/// [`crate::RenderQualityControls::MAX_FILTER_SAMPLES`], and 16 on Tier B
/// (WebGL2, GL ES), whose fill rate the larger budgets outrun.
#[must_use]
pub const fn filter_sample_limit(tier: CapabilityTier) -> u32 {
    match tier {
        CapabilityTier::TierB => 16,
        _ => crate::RenderQualityControls::MAX_FILTER_SAMPLES,
    }
}

/// Resolves deterministic degradation without product- or device-name assumptions.
#[must_use]
pub fn resolve_degradation(
    capabilities: &Capabilities,
    requested_texture_bytes: u64,
    requested_filter_samples: u32,
    budget: ResourceBudget,
) -> DegradationDecision {
    let texture_bytes = requested_texture_bytes.min(budget.texture_bytes);
    let max_filter_samples = requested_filter_samples
        .min(filter_sample_limit(capabilities.tier))
        .max(1);
    let decoder_available = budget.active_decoder_sessions < budget.decoder_sessions;
    let mut diagnostics = Vec::new();
    if capabilities.intermediate_format == IntermediateFormat::Rgba8Unorm {
        diagnostics.push(CapabilityDiagnostic::Rgba8Intermediate);
    }
    if texture_bytes < requested_texture_bytes {
        diagnostics.push(CapabilityDiagnostic::TextureBudgetReduced {
            requested: requested_texture_bytes,
            applied: texture_bytes,
        });
    }
    if max_filter_samples < requested_filter_samples {
        diagnostics.push(CapabilityDiagnostic::FilterSamplesReduced {
            requested: requested_filter_samples,
            applied: max_filter_samples,
        });
    }
    if !decoder_available {
        diagnostics.push(CapabilityDiagnostic::DecoderSessionLimit {
            limit: budget.decoder_sessions,
        });
    }
    DegradationDecision {
        texture_bytes,
        max_filter_samples,
        decoder_available,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_b_and_caller_budgets_degrade_observably() {
        let capabilities = Capabilities::new(
            CapabilityTier::TierB,
            IntermediateFormat::Rgba8Unorm,
            "test",
            4096,
            false,
            "Gl",
            "software",
            "fixture",
        );
        let decision = resolve_degradation(
            &capabilities,
            1_000,
            64,
            ResourceBudget {
                texture_bytes: 500,
                decoder_sessions: 2,
                active_decoder_sessions: 2,
            },
        );
        assert_eq!(decision.texture_bytes, 500);
        assert_eq!(decision.max_filter_samples, 16);
        assert!(!decision.decoder_available);
        assert_eq!(decision.diagnostics.len(), 4);
    }

    #[test]
    fn the_filter_sample_limit_is_the_renderers_on_every_tier() {
        assert_eq!(filter_sample_limit(CapabilityTier::TierB), 16);
        assert_eq!(
            filter_sample_limit(CapabilityTier::TierA),
            crate::RenderQualityControls::MAX_FILTER_SAMPLES
        );
        let capabilities = Capabilities::new(
            CapabilityTier::TierA,
            IntermediateFormat::Rgba16Float,
            "test",
            4096,
            false,
            "Vulkan",
            "driver",
            "fixture",
        );
        let budget = ResourceBudget::new(1_000, 1, 0);
        let decision = resolve_degradation(&capabilities, 1_000, 256, budget);
        assert_eq!(decision.max_filter_samples, 64);
        assert_eq!(
            decision.diagnostics,
            [CapabilityDiagnostic::FilterSamplesReduced {
                requested: 256,
                applied: 64
            }]
        );
        assert_eq!(
            resolve_degradation(&capabilities, 1_000, 64, budget).diagnostics,
            [] as [CapabilityDiagnostic; 0]
        );
    }
}
