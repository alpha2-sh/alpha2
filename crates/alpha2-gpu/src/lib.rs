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
mod capabilities;
mod context;
mod diagnostic;
mod error;

pub use acquisition::{AcquisitionGuard, serialize_acquisition};
pub use capabilities::{Capabilities, CapabilityTier, IntermediateFormat};
pub use context::{AcceptedDevice, AdapterRefusals, ExternalQueueLock, GpuContext, SubmitTimings};
pub use diagnostic::{
    GpuDiagnostic, GpuDiagnosticHandler, GpuDiagnosticLog, GpuDiagnosticSnapshot, GpuErrorKind,
    MAX_DIAGNOSTIC_MESSAGE_BYTES,
};
pub use error::{GpuAcquisitionStep, GpuError};
/// The `wgpu` this crate is built on, with the `wgpu-interop` feature. Every
/// `wgpu` type the interop surface takes or returns, here and in the crates
/// built on this one, is this crate's, so a caller names them through this
/// path instead of depending on a `wgpu` version of their own. When this
/// crate moves to a new `wgpu` major, the types behind this path change with
/// it, and so does the interop surface.
#[cfg(feature = "wgpu-interop")]
pub use wgpu;
