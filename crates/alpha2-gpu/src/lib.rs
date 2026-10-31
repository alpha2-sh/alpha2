//! GPU device acquisition, adapter classification ([`Capabilities`]), queue access shared with
//! native code ([`ExternalQueueLock`]), and collection of uncaptured errors and device losses
//! ([`GpuDiagnosticLog`]).
//!
//! # `wgpu` interop
//!
//! The default API names no `wgpu` type. Items that hand out or adopt `wgpu` handles are
//! documented only with the `wgpu-interop` feature, which also re-exports `wgpu` and follows its
//! major version. Without the feature they are hidden and not part of the public API.

mod acquisition;
mod adapter_choice;
mod adapter_pin;
mod capabilities;
mod context;
mod diagnostic;
mod error;
mod instance;
mod rate;
pub mod validation_log;
mod watchdog;

pub use acquisition::{AcquisitionGuard, serialize_acquisition};
pub use adapter_choice::{
    AdapterChoiceReport, AdapterOrder, AdapterRecord, ChoiceReason, GPU_ENCODE_CODECS,
    decode_refusals, encode_elsewhere, explain_choice, rank_adapters,
};
pub use adapter_pin::{AdapterPolicy, gl_version_supported, high_performance_rank, product_order};
pub use capabilities::{
    Capabilities, CapabilityTier, IntermediateFormat, software_adapter, software_renderer_name,
};
pub use context::{
    AcceptedDevice, AdapterRefusals, ExternalQueueLock, GpuContext, ProductRequest,
    SoftwareAdapters, SubmitTimings, product_backends,
};
pub use diagnostic::{
    GpuDiagnostic, GpuDiagnosticHandler, GpuDiagnosticLog, GpuDiagnosticSnapshot, GpuErrorKind,
    MAX_DIAGNOSTIC_MESSAGE_BYTES,
};
pub use error::{GpuAcquisitionStep, GpuError};
pub use instance::{
    Dx12Compiler, InstanceSettings, InstanceSettingsError, ShaderCompiler, configure_instances,
    dx12_shader_compiler, instance_descriptor, instance_settings, new_instance, shader_compiler_of,
};
pub use rate::{GpuRate, UNCALIBRATED_MS_PER_UNIT_PIXEL};
pub use watchdog::{
    WATCHDOG_MS, hung_threshold_ms, set_watchdog_ms, submission_budget_ms, watchdog_ms,
};
/// The `wgpu` this crate is built on. Interop callers should name `wgpu` types through this path
/// rather than depend on their own `wgpu` version.
#[cfg(feature = "wgpu-interop")]
pub use wgpu;
