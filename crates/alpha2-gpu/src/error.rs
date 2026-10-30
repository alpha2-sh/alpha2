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
    /// A backend's instance did not start (on Windows, DX12 with its shader compiler).
    Instance,
    /// `ALPHA2_GPU_ADAPTER` names no adapter.
    Pin,
    /// No hardware adapter made a device, and software adapters were not wanted.
    NoHardwareAdapter,
}

// Browser WebGPU has one instance, one adapter and no pin, so the browser build makes only the
// first two.
#[derive(Debug)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
enum Cause {
    Adapter(wgpu::RequestAdapterError),
    Device(wgpu::RequestDeviceError),
    Instance {
        backend: &'static str,
        compiler: String,
    },
    Pin(String),
    NoHardwareAdapter {
        software: Vec<String>,
        refusals: Vec<(String, String)>,
    },
}

impl GpuError {
    pub(crate) const fn adapter(error: wgpu::RequestAdapterError) -> Self {
        Self(Cause::Adapter(error))
    }

    pub(crate) const fn device(error: wgpu::RequestDeviceError) -> Self {
        Self(Cause::Device(error))
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) const fn instance(backend: &'static str, compiler: String) -> Self {
        Self(Cause::Instance { backend, compiler })
    }

    /// A DX12 backend that lists no adapter: it did not start, which on
    /// Windows means its shader compiler did not load. An acquisition step
    /// of [`GpuAcquisitionStep::Instance`], whatever adapter was pinned.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn no_dx12_adapter() -> Self {
        Self::instance("DX12", crate::dx12_shader_compiler().to_string())
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) const fn pin(reason: String) -> Self {
        Self(Cause::Pin(reason))
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) const fn no_hardware(
        software: Vec<String>,
        refusals: Vec<(String, String)>,
    ) -> Self {
        Self(Cause::NoHardwareAdapter { software, refusals })
    }

    /// The step that failed.
    #[must_use]
    pub const fn step(&self) -> GpuAcquisitionStep {
        match self.0 {
            Cause::Adapter(_) => GpuAcquisitionStep::Adapter,
            Cause::Device(_) => GpuAcquisitionStep::Device,
            Cause::Instance { .. } => GpuAcquisitionStep::Instance,
            Cause::Pin(_) => GpuAcquisitionStep::Pin,
            Cause::NoHardwareAdapter { .. } => GpuAcquisitionStep::NoHardwareAdapter,
        }
    }
}

impl std::error::Error for GpuError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.0 {
            Cause::Adapter(error) => Some(error),
            Cause::Device(error) => Some(error),
            Cause::Instance { .. } | Cause::Pin(_) | Cause::NoHardwareAdapter { .. } => None,
        }
    }
}

impl fmt::Display for GpuError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Cause::Adapter(error) => write!(formatter, "could not acquire a GPU adapter: {error}"),
            Cause::Device(error) => write!(formatter, "could not acquire a GPU device: {error}"),
            Cause::Instance { backend, compiler } if compiler.is_empty() => {
                write!(formatter, "the {backend} backend did not start")
            }
            Cause::Instance { backend, compiler } => write!(
                formatter,
                "the {backend} backend did not start (shader compiler: {compiler}): is the file there?"
            ),
            Cause::Pin(reason) => formatter.write_str(reason),
            Cause::NoHardwareAdapter { software, refusals } => {
                formatter.write_str("no hardware GPU")?;
                if !software.is_empty() {
                    write!(
                        formatter,
                        "; only software adapters ({}), which are not used here",
                        software.join(", ")
                    )?;
                }
                if refusals.is_empty() && software.is_empty() {
                    formatter.write_str(": no GPU adapter was found")?;
                }
                for (adapter, reason) in refusals {
                    write!(formatter, "; {adapter}: {reason}")?;
                }
                Ok(())
            }
        }
    }
}
