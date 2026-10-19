//! Decoder-surface lifetime contract used by import implementations.

use std::fmt::Debug;

/// Identity used to prevent imported texture reuse across decoder generations.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub struct SurfaceIdentity {
    /// Decoder or pool generation.
    pub generation: u64,
    /// Backend-native picture identity.
    pub picture: u64,
}

/// Native backing retained until every sampling submission has completed.
pub trait NativeSurfaceLease: Debug + Send + Sync + 'static {
    /// Cache identity for the retained picture.
    fn identity(&self) -> SurfaceIdentity;
    /// Approximate bytes reserved from the native surface budget.
    fn retained_bytes(&self) -> u64;
}
