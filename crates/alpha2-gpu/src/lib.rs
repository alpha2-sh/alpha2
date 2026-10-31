//! GPU context and capability negotiation: acquiring a GPU device, classifying
//! the adapter ([`Capabilities`]), serializing queue access shared with native
//! code ([`ExternalQueueLock`]), and collecting the device's uncaptured errors
//! and losses ([`GpuDiagnosticLog`]).
//!
//! # `wgpu` interop
//!
//! The API names no `wgpu` type by default. The items that hand out or adopt
//! `wgpu` handles (the device, queue, and adapter, submissions, surfaces) are
//! the interop surface, documented with the `wgpu-interop` feature, which also
//! re-exports `wgpu` itself. That surface follows `wgpu`'s major version.
//! Without the feature those items are hidden: alpha2's own crates share
//! handles through them, and they are not part of this crate's API.

mod acquisition;
mod adapter_choice;
mod adapter_pin;
mod capabilities;
mod context;
mod diagnostic;
mod error;
mod instance;
pub mod validation_log;
mod watchdog;

pub use acquisition::{AcquisitionGuard, serialize_acquisition};
pub use adapter_choice::{
    AdapterChoiceReport, AdapterOrder, AdapterRecord, ChoiceReason, GPU_ENCODE_CODECS,
    decode_refusals, encode_elsewhere, explain_choice, rank_adapters,
};
pub use adapter_pin::{AdapterPolicy, gl_version_supported, high_performance_rank, product_order};
pub use capabilities::{Capabilities, CapabilityTier, IntermediateFormat};
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
pub use watchdog::{
    WATCHDOG_MS, hung_threshold_ms, set_watchdog_ms, submission_budget_ms, watchdog_ms,
};
/// The `wgpu` this crate is built on, with the `wgpu-interop` feature. Every
/// `wgpu` type the interop surface takes or returns, here and in the crates
/// built on this one, is this crate's, so a caller names them through this
/// path instead of depending on a `wgpu` version of their own. When this
/// crate moves to a new `wgpu` major, the types behind this path change with
/// it, and so does the interop surface.
#[cfg(feature = "wgpu-interop")]
pub use wgpu;
