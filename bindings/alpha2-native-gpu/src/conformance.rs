//! The interop contract every platform backend keeps, as one suite each
//! backend's tests run on its own surfaces.
//!
//! The backends import differently (Vulkan acquires the decoder's image on
//! its timeline, D3D12 copies the surface under a fence, Metal wraps the
//! pixel buffer's planes), so what holds a surface while the GPU may read it
//! differs, and so do the platform statuses. What the host relies on does
//! not, and [`check`] asserts it the same way for each:
//!
//! 1. **Typed.** A platform call that fails while a surface is imported comes
//!    back as [`BridgeError::NativeGpu`] naming the backend and the raw
//!    status, with the [`GpuFault`] that status names, or none when it names
//!    none. The host fails the frame on a fault and falls back to a download
//!    otherwise, so a status read the wrong way either hangs on a lost device
//!    or gives up on a frame that could be shown.
//! 2. **Gives back.** A failure before any GPU work was submitted gives the
//!    decoder its surface back at once and keeps none of the helper objects
//!    made for it.
//! 3. **Recovers.** After any such failure the next surface on the same
//!    context imports, its sampling submission completes, and it too goes
//!    back to the decoder.
//!
//! Each backend checks the rest of the lifetime contract in tests of its own,
//! because the object that holds the work is the backend's: a failure after
//! work was submitted, or a wait that times out, keeps what the GPU may still
//! read (Vulkan: the import ticket, `vulkan::failure_tests`
//! `a_ticket_whose_acquire_may_still_run_waits_in_the_pool_until_it_has`;
//! D3D12: the surface, `d3d12::tests`
//! `a_copy_that_may_still_be_on_the_gpu_keeps_its_surface_from_the_decoder`
//! and `a_copy_that_does_not_finish_in_time_keeps_its_surface_from_the_decoder`;
//! Metal submits no work of its own for an import, so there is none to wait
//! for, and its lease holds the `CoreVideo` textures until the host's sampling
//! submission completes), and every wait on the GPU ends within the configured
//! timeout.

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
    /// Statuses the import's failing call can return, each with the fault it
    /// names.
    const STATUSES: &'static [(i32, Option<GpuFault>)];

    /// The shared device, or `None` where this machine cannot make one.
    fn context() -> Option<Self::Context>;
    /// A fresh decoder surface of `context`.
    fn surface(context: &Self::Context) -> Arc<Self::Surface>;
    /// Makes the next import on this thread fail with `status`, at the call
    /// that would first submit GPU work for it.
    fn fail_next_import(status: i32);
    /// Imports `surface`, runs a sampling submission to its end, and lets go
    /// of the import.
    fn import(context: &Self::Context, surface: Arc<Self::Surface>) -> Result<(), BridgeError>;
    /// Helper objects this process has leaked rather than freed.
    fn leaked() -> u64;
}

/// Imports a fresh surface of `context` and returns the result with a probe
/// that says whether anything still holds the surface.
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
