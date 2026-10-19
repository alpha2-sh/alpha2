//! Synchronization values transferred with imported native images.

/// Timeline synchronization point required before sampling an image.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ExternalSyncPoint {
    /// Backend-native semaphore or fence identity.
    pub handle: u64,
    /// Timeline value to wait for.
    pub wait_value: u64,
    /// Timeline value returned to the decoder after rendering.
    pub signal_value: u64,
}

/// Native image state observed while the decoder surface is locked.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NativeImageState {
    /// Backend-native layout/state value.
    pub layout: u32,
    /// Backend-native access mask.
    pub access: u32,
    /// Current queue-family owner, when meaningful for the backend.
    pub queue_family: u32,
}
