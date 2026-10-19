/// The action a host should take after its renderer reports missing frames.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MissingFramePolicy {
    /// Reuse the prior rendered output and continue real-time playback.
    HoldPrevious,
    /// Wait for host decode/upload before advancing export.
    Wait,
}

/// The renderer's color policy for a project's color pipeline: tone-map HDR
/// sources from its peak, or refuse them.
#[must_use]
pub fn color_policy(color: &alpha2_model::ColorPipeline) -> alpha2_render::ColorPolicy {
    match color.hdr {
        alpha2_model::HdrSources::Reject => alpha2_render::ColorPolicy::RejectHdr,
        alpha2_model::HdrSources::ToneMap => {
            alpha2_render::ColorPolicy::ToneMapHdr(alpha2_render::hdr::HdrToneMap {
                source_peak_nits: color.hdr_peak_nits,
            })
        }
    }
}
