#[derive(Debug)]
pub struct GpuOutputRefusal {
    /// Reported as `gpuOutputPathUnavailable`.
    pub reason: String,
    /// The request pinned an encoder that only takes GPU frames, so no other path can serve it.
    pub required: bool,
}

impl GpuOutputRefusal {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            required: false,
        }
    }
}
