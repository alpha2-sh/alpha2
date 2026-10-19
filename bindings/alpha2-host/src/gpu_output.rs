//! Why an export will not take the GPU-resident path, whichever build this is.

/// Why the GPU-resident path is not a candidate for an export.
#[derive(Debug)]
pub struct GpuOutputRefusal {
    /// What the path lacks, reported as `gpuOutputPathUnavailable`.
    pub reason: String,
    /// The request cannot be served any other way: it pinned the encoder that
    /// only takes GPU frames.
    pub required: bool,
}

impl GpuOutputRefusal {
    /// A refusal the readback path can stand in for.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            required: false,
        }
    }
}
