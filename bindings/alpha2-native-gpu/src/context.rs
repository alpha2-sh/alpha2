//! Device identity shared by decode and render imports.

/// Stable identity of a native GPU device for cache and import validation.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub struct NativeGpuIdentity {
    /// Backend-specific physical-device identity.
    pub physical_device: u64,
    /// Logical-device identity. Handles from different values cannot be mixed.
    pub device: u64,
    /// Monotonic epoch changed whenever the render device is recreated.
    pub epoch: u64,
}
