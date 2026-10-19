use std::fmt;

/// Render input or output failure.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RenderError {
    /// Render targets must have non-zero dimensions.
    ZeroDimension,
    /// Render dimensions exceed the configured or physical device limit.
    DimensionTooLarge,
    /// Colors must be finite, inside the unit interval, and premultiplied:
    /// no color component above alpha.
    InvalidColor,
    /// A layer transform is non-finite, has zero scale, or invalid opacity.
    InvalidVisual,
    /// An effect parameter is non-finite or outside its supported range.
    InvalidEffectParameter,
    /// No effect with the requested identity is registered.
    UnknownEffect,
    /// Supplied parameter names or types do not match the effect schema.
    InvalidEffectParams,
    /// An uploaded effect resource has an invalid identity, shape, or payload.
    InvalidEffectResource,
    /// An effect invocation omitted a required registered resource.
    MissingEffectResource,
    /// A resource identity resolves to a different representation than required.
    EffectResourceKindMismatch,
    /// A transition invocation omitted its second texture.
    MissingEffectInput,
    /// A frame shows a caption or a text generator its host did not prepare
    /// at the frame's size.
    MissingText,
    /// RGBA input byte length does not match the declared image size.
    InvalidImageData,
    /// A rendered texture could not be read back to host memory, and where.
    TextureReadback(ReadbackFailure),
    /// An internally constructed render graph was structurally invalid.
    InvalidGraph,
    /// A native texture could not synchronize with the render submission,
    /// and what the platform's status for that said about the device, when
    /// it said anything.
    ExternalTextureSync(Option<alpha2_media::GpuFault>),
    /// Caller-supplied NV12 planes are not an R8 luma and an RG8 chroma
    /// render attachment at the output size and half of it.
    InvalidPlaneTarget,
    /// GPU work did not complete within the renderer's wait timeout.
    GpuTimeout,
    /// The device completed a submission without running it: a readback came
    /// back without the bytes its submission copied. The renderer refuses
    /// work from then on; only a new process gets a device that runs it.
    GpuWorkDropped,
    /// The GPU rejected this frame's work, for this kind of error: an
    /// allocation it could not make, or invalid use (often of what failed to
    /// allocate). Nothing in the submission ran; the device is fine, and the
    /// renderer kept nothing of the frame.
    Gpu {
        /// The class of error the GPU reported.
        kind: alpha2_gpu::GpuErrorKind,
    },
    /// The device is lost: it can no longer make the resources a frame
    /// needs. The renderer refuses work from then on.
    GpuDeviceLost,
    /// An HDR (PQ or HLG) source met [`crate::ColorPolicy::RejectHdr`]: the
    /// project's color pipeline refuses it rather than show it.
    UnsupportedHdr,
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroDimension => formatter.write_str("render dimensions must be non-zero"),
            Self::DimensionTooLarge => {
                formatter.write_str("render dimensions exceed the supported texture budget")
            }
            Self::InvalidColor => {
                formatter.write_str(
                    "color components must be finite values from zero to one, premultiplied: none above alpha",
                )
            }
            Self::InvalidVisual => formatter.write_str("layer presentation values are invalid"),
            Self::InvalidEffectParameter => formatter.write_str("effect parameter is invalid"),
            Self::UnknownEffect => formatter.write_str("effect is not registered"),
            Self::InvalidEffectParams => {
                formatter.write_str("effect parameters do not match schema")
            }
            Self::InvalidEffectResource => formatter.write_str("effect resource is invalid"),
            Self::MissingEffectResource => {
                formatter.write_str("effect resource is missing or not registered")
            }
            Self::MissingText => {
                formatter.write_str("text was not prepared for this frame and size")
            }
            Self::EffectResourceKindMismatch => {
                formatter.write_str("effect resource kind does not match schema")
            }
            Self::MissingEffectInput => formatter.write_str("effect input texture is missing"),
            Self::InvalidImageData => {
                formatter.write_str("RGBA byte length does not match the image size")
            }
            Self::TextureReadback(failure) => {
                write!(formatter, "GPU texture readback failed: {failure}")
            }
            Self::InvalidGraph => formatter.write_str("render graph is structurally invalid"),
            Self::ExternalTextureSync(None) => {
                formatter.write_str("external texture synchronization failed")
            }
            Self::ExternalTextureSync(Some(fault)) => write!(
                formatter,
                "external texture synchronization failed ({})",
                fault.as_str()
            ),
            Self::InvalidPlaneTarget => formatter.write_str(
                "NV12 plane targets must be an R8 luma and an RG8 chroma render attachment at the output size and half of it",
            ),
            Self::GpuTimeout => formatter.write_str(GPU_TIMEOUT_MESSAGE),
            Self::GpuWorkDropped => formatter.write_str(GPU_WORK_DROPPED_MESSAGE),
            Self::Gpu { kind } => write!(
                formatter,
                "the GPU rejected this frame's work with a {} error",
                kind.as_str()
            ),
            Self::GpuDeviceLost => formatter.write_str(GPU_DEVICE_LOST_MESSAGE),
            Self::UnsupportedHdr => {
                formatter.write_str("the color policy refuses HDR (PQ or HLG) sources")
            }
        }
    }
}

impl std::error::Error for RenderError {}

/// Where a texture readback failed.
///
/// A failed buffer mapping carries no cause in `wgpu` 30
/// (`BufferAsyncError` is a unit), so [`Self::MapFailed`] says only that the
/// mapping was refused. Whether the device was lost or out of memory at the
/// time is for the device's diagnostics (`alpha2_gpu::GpuDiagnosticLog`) to
/// show; none of these is itself evidence of memory exhaustion.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ReadbackFailure {
    /// The rendered texture's format or size is not one the readback packs.
    UnsupportedTarget,
    /// A row, plane, or buffer size overflowed or could not be represented.
    Layout,
    /// Mapping was requested twice for one readback.
    DuplicateMapRequest,
    /// The GPU refused to map the staging buffer.
    MapFailed,
    /// The mapping callback was dropped without reporting.
    CallbackDropped,
    /// The staging buffer was read before its mapping succeeded.
    NotMapped,
    /// A caller-supplied plane destination is too small for the frame.
    DestinationTooSmall,
    /// The device was polled for a submission it did not recognise.
    SubmissionMismatch,
}

impl ReadbackFailure {
    /// Stable lowercase name for logs and protocols.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedTarget => "unsupported-target",
            Self::Layout => "layout",
            Self::DuplicateMapRequest => "duplicate-map-request",
            Self::MapFailed => "map-failed",
            Self::CallbackDropped => "callback-dropped",
            Self::NotMapped => "not-mapped",
            Self::DestinationTooSmall => "destination-too-small",
            Self::SubmissionMismatch => "submission-mismatch",
        }
    }
}

impl fmt::Display for ReadbackFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The message every GPU wait timeout carries, so a host that sees only error
/// text can still classify it: one sentence for the renderer, the native
/// interop helpers, and encoder inputs alike.
pub use alpha2_media::GPU_TIMEOUT_MESSAGE;

/// The message of [`RenderError::GpuWorkDropped`]. It names a lost device,
/// which is what the host classifies it as.
pub const GPU_WORK_DROPPED_MESSAGE: &str = "GPU device lost: it completed submitted work without \
     running it (a readback lacked its canary), so this process can no longer render";

/// The message of [`RenderError::GpuDeviceLost`].
pub const GPU_DEVICE_LOST_MESSAGE: &str = "GPU device lost: it can no longer make the resources \
     a frame needs, so this process can no longer render";

/// How long a native caller waits for GPU work before giving up, unless the
/// renderer is told otherwise.
pub const DEFAULT_GPU_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::needless_pass_by_value)] // Shaped for `map_err`.
pub(crate) fn poll_error(error: wgpu::PollError) -> RenderError {
    match error {
        wgpu::PollError::Timeout => RenderError::GpuTimeout,
        wgpu::PollError::WrongSubmissionIndex(..) => {
            RenderError::TextureReadback(ReadbackFailure::SubmissionMismatch)
        }
    }
}
