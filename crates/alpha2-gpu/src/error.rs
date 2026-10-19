use std::fmt;

/// Failure while acquiring a GPU context: which step failed, and the
/// platform's reason as its [`source`](std::error::Error::source).
#[derive(Debug)]
pub struct GpuError(Cause);

/// The step of acquiring a GPU context that failed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GpuAcquisitionStep {
    /// No suitable adapter was found.
    Adapter,
    /// The adapter could not create a device.
    Device,
}

#[derive(Debug)]
enum Cause {
    Adapter(wgpu::RequestAdapterError),
    Device(wgpu::RequestDeviceError),
}

impl GpuError {
    pub(crate) const fn adapter(error: wgpu::RequestAdapterError) -> Self {
        Self(Cause::Adapter(error))
    }

    pub(crate) const fn device(error: wgpu::RequestDeviceError) -> Self {
        Self(Cause::Device(error))
    }

    /// The step that failed.
    #[must_use]
    pub const fn step(&self) -> GpuAcquisitionStep {
        match self.0 {
            Cause::Adapter(_) => GpuAcquisitionStep::Adapter,
            Cause::Device(_) => GpuAcquisitionStep::Device,
        }
    }
}

impl std::error::Error for GpuError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.0 {
            Cause::Adapter(error) => Some(error),
            Cause::Device(error) => Some(error),
        }
    }
}

impl fmt::Display for GpuError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Cause::Adapter(error) => write!(formatter, "could not acquire a GPU adapter: {error}"),
            Cause::Device(error) => write!(formatter, "could not acquire a GPU device: {error}"),
        }
    }
}
