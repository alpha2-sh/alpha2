//! GPU error tracking for a frame's work.
//!
//! An out-of-memory texture or invalid pass makes its whole submission rejected, and
//! `queue.submit` does not say so; the error goes to an error scope or the uncaptured-error
//! handler. Unchecked, the renderer would keep results never drawn, reuse failed allocations,
//! and misread the missing readback canary as a device dropping work.
//!
//! Every renderer encoder therefore opens a [`FrameErrorScope`] that its submission closes.
//! Natively scopes resolve at once and an error fails the frame ([`crate::RenderError::Gpu`]).
//! In a browser scopes resolve only on the event loop, so the embedding reports uncaptured
//! errors ([`GpuErrorReports::report`]) and the renderer drops kept state before its next frame.
//!
//! Either way, everything pooled before the error is distrusted
//! ([`crate::retire::Retirement::doubt`]), since `wgpu` cannot say which object is invalid.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use alpha2_gpu::GpuErrorKind;

use crate::Shared;
use crate::retire::Retirement;

/// Validation and out-of-memory scopes of one renderer encoder, from creation to submission.
///
/// Scopes are per thread and close in reverse order, so this is neither `Send` nor `Sync`, and
/// a nested encoder must be submitted or dropped first.
#[derive(Debug)]
pub(crate) struct FrameErrorScope {
    #[cfg(not(target_arch = "wasm32"))]
    scopes: Option<[wgpu::ErrorScopeGuard; 2]>,
    /// Told when an encoder is dropped unsubmitted with an error, since it may have pooled an
    /// invalid resource.
    retirement: Shared<Retirement>,
    #[cfg(target_arch = "wasm32")]
    _thread_bound: std::marker::PhantomData<*mut ()>,
}

impl FrameErrorScope {
    /// Opens the scopes on this thread; a no-op in a browser.
    pub(crate) fn open(
        #[cfg_attr(target_arch = "wasm32", allow(unused_variables))] device: &wgpu::Device,
        retirement: Shared<Retirement>,
    ) -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            scopes: Some([
                device.push_error_scope(wgpu::ErrorFilter::OutOfMemory),
                device.push_error_scope(wgpu::ErrorFilter::Validation),
            ]),
            retirement,
            #[cfg(target_arch = "wasm32")]
            _thread_bound: std::marker::PhantomData,
        }
    }

    /// Closes the scopes and returns the error, out-of-memory first because a failed allocation
    /// also causes validation errors downstream.
    pub(crate) fn close(mut self) -> Option<GpuErrorKind> {
        self.pop()
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn pop(&mut self) -> Option<GpuErrorKind> {
        let [out_of_memory, validation] = self.scopes.take()?;
        let validation = resolved(validation.pop());
        let out_of_memory = resolved(out_of_memory.pop());
        out_of_memory
            .or(validation)
            .map(|error| GpuErrorKind::of(&error))
    }

    #[cfg(target_arch = "wasm32")]
    #[allow(clippy::unused_self)] // The browser opens no scope to pop.
    const fn pop(&mut self) -> Option<GpuErrorKind> {
        None
    }
}

impl Drop for FrameErrorScope {
    fn drop(&mut self) {
        if self.pop().is_some() {
            self.retirement.doubt();
        }
    }
}

/// Runs `create` under out-of-memory and validation scopes, returning what they caught as
/// [`FrameErrorScope::close`] does.
///
/// For resources made outside a frame, whose failure must fail the call. A no-op in a browser,
/// where the uncaptured-error report covers it.
pub(crate) fn capture_creation<T>(
    #[cfg_attr(target_arch = "wasm32", allow(unused_variables))] device: &wgpu::Device,
    create: impl FnOnce() -> T,
) -> (T, Option<GpuErrorKind>) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let out_of_memory = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let created = create();
        let validation = resolved(validation.pop());
        let out_of_memory = resolved(out_of_memory.pop());
        (
            created,
            out_of_memory
                .or(validation)
                .map(|error| GpuErrorKind::of(&error)),
        )
    }
    #[cfg(target_arch = "wasm32")]
    {
        (create(), None)
    }
}

/// A native scope's result, ready when popped on `wgpu-core`.
#[cfg(not(target_arch = "wasm32"))]
fn resolved<T>(future: impl std::future::Future<Output = Option<T>>) -> Option<T> {
    let mut future = std::pin::pin!(future);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut context) {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => None,
    }
}

/// GPU errors the embedding saw outside the renderer's scopes (in a browser, all of them).
///
/// Clones share one record, so the embedding keeps one in its error handler. The renderer acts
/// on reports at its next encoder.
#[derive(Clone, Debug, Default)]
pub struct GpuErrorReports {
    inner: Arc<ReportState>,
}

#[derive(Debug, Default)]
struct ReportState {
    /// Errors reported so far.
    reported: AtomicU64,
    /// How many reports the renderer has dropped its kept state for.
    handled: AtomicU64,
    /// The last kind, encoded by [`kind_code`].
    last: AtomicU8,
}

impl GpuErrorReports {
    /// Records an error reported outside the renderer's scopes. Lock-free; callable from any
    /// thread.
    pub fn report(&self, kind: GpuErrorKind) {
        self.inner.last.store(kind_code(kind), Ordering::Release);
        self.inner.reported.fetch_add(1, Ordering::AcqRel);
    }

    /// How many errors have been reported.
    pub(crate) fn count(&self) -> u64 {
        self.inner.reported.load(Ordering::Acquire)
    }

    pub(crate) fn last_kind(&self) -> Option<GpuErrorKind> {
        match self.inner.last.load(Ordering::Acquire) {
            1 => Some(GpuErrorKind::OutOfMemory),
            2 => Some(GpuErrorKind::Validation),
            3 => Some(GpuErrorKind::Internal),
            _ => None,
        }
    }

    /// Whether errors were reported since the last call, marking them handled.
    pub(crate) fn take_unhandled(&self) -> bool {
        let reported = self.count();
        self.inner.handled.swap(reported, Ordering::AcqRel) != reported
    }
}

const fn kind_code(kind: GpuErrorKind) -> u8 {
    match kind {
        GpuErrorKind::OutOfMemory => 1,
        GpuErrorKind::Validation => 2,
        _ => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_are_counted_and_handled_once() {
        let reports = GpuErrorReports::default();
        assert!(!reports.take_unhandled());
        assert_eq!(reports.last_kind(), None);
        let handler = reports.clone();
        handler.report(GpuErrorKind::OutOfMemory);
        handler.report(GpuErrorKind::Validation);
        assert_eq!(reports.count(), 2);
        assert_eq!(reports.last_kind(), Some(GpuErrorKind::Validation));
        assert!(reports.take_unhandled());
        assert!(!reports.take_unhandled());
        handler.report(GpuErrorKind::Internal);
        assert_eq!(reports.last_kind(), Some(GpuErrorKind::Internal));
        assert!(reports.take_unhandled());
    }
}
