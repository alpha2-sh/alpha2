//! The interop contract every platform backend keeps, as one suite each backend runs on its own
//! surfaces.
//!
//! Backends hold surfaces differently (Vulkan acquires on the decoder's timeline, D3D12 copies
//! under a fence, Metal wraps the pixel buffer's planes), but [`check`] asserts the same three
//! guarantees for each:
//!
//! 1. **Typed.** A platform call that fails during import returns [`BridgeError::NativeGpu`]
//!    with the backend, the raw status, and the [`GpuFault`] it names, if any. The host fails
//!    the frame on a fault and falls back to a download otherwise, so a misread status either
//!    hangs on a lost device or drops a frame that could be shown.
//! 2. **Gives back.** A failure before any GPU work is submitted returns the surface to the
//!    decoder at once and keeps none of the helper objects.
//! 3. **Recovers.** After such a failure the next surface on the same context imports,
//!    completes its sampling submission, and goes back to the decoder.
//!
//! Each backend tests the rest of the lifetime contract itself: a failure after submission, or
//! a timed-out wait, keeps whatever the GPU may still read (the Vulkan import ticket, the D3D12
//! surface; Metal submits no import work and its lease holds the `CoreVideo` textures until
//! sampling completes), and every GPU wait ends within the configured timeout.

use std::sync::{Arc, Weak};

use alpha2_ffmpeg_bridge::{BridgeError, GpuFault, NativeGpuBackend};

/// One backend's import, as the suite drives it.
pub(crate) trait Backend {
    /// The shared device the decoder and the renderer use.
    type Context;
    /// A decoder surface as the decoder hands it to an import.
    type Surface;
    /// The backend whose failures the errors must name.
    const BACKEND: NativeGpuBackend;
    /// Statuses the import's failing call can return, each with the fault it names.
    const STATUSES: &'static [(i32, Option<GpuFault>)];

    /// The shared device, or `None` where this machine cannot make one.
    fn context() -> Option<Self::Context>;
    /// A fresh decoder surface of `context`.
    fn surface(context: &Self::Context) -> Arc<Self::Surface>;
    /// Makes the next import on this thread fail with `status`, at the call that would first
    /// submit GPU work for it.
    fn fail_next_import(status: i32);
    /// Imports `surface`, runs a sampling submission to its end, and releases the import.
    fn import(context: &Self::Context, surface: Arc<Self::Surface>) -> Result<(), BridgeError>;
    /// Helper objects this process has leaked rather than freed.
    fn leaked() -> u64;
}

/// Imports a fresh surface and returns the result with a probe for whether anything still
/// holds the surface.
fn import<B: Backend>(context: &B::Context) -> (Result<(), BridgeError>, Weak<B::Surface>) {
    let surface = B::surface(context);
    let probe = Arc::downgrade(&surface);
    (B::import(context, surface), probe)
}

/// Runs the suite on `B`, or skips it where there is no device.
pub(crate) fn check<B: Backend>() {
    let Some(context) = B::context() else {
        return;
    };
    let backend = B::BACKEND.as_str();
    let leaked = B::leaked();
    let (imported, probe) = import::<B>(&context);
    imported.unwrap_or_else(|error| panic!("{backend}: a healthy import fails: {error}"));
    assert!(
        probe.upgrade().is_none(),
        "{backend}: a finished import keeps its surface"
    );
    for &(status, fault) in B::STATUSES {
        B::fail_next_import(status);
        let (imported, probe) = import::<B>(&context);
        let error = imported.expect_err("the injected failure fails the import");
        let BridgeError::NativeGpu(failure) = &error else {
            panic!("{backend} status {status}: not a native GPU failure: {error:?}");
        };
        assert_eq!(failure.backend, B::BACKEND, "{error}");
        assert_eq!(failure.status, status, "{error}");
        assert_eq!(
            failure.fault, fault,
            "{backend} status {status} names the wrong fault: {error}"
        );
        assert!(
            probe.upgrade().is_none(),
            "{backend} status {status}: an import that submitted nothing keeps its surface"
        );
        assert_eq!(
            B::leaked(),
            leaked,
            "{backend} status {status}: an import that submitted nothing leaks"
        );
        let (imported, probe) = import::<B>(&context);
        imported.unwrap_or_else(|error| {
            panic!("{backend} status {status}: the next import fails: {error}")
        });
        assert!(
            probe.upgrade().is_none(),
            "{backend} status {status}: the next import keeps its surface"
        );
    }
    assert_eq!(B::leaked(), leaked, "{backend}: the suite leaks");
}
