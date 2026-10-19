//! What the GPU said about a frame's work, and what the renderer stops
//! trusting when it said no.
//!
//! An out-of-memory texture or an invalid pass makes the submission that
//! uses it invalid, and WebGPU and `wgpu-core` alike reject such a
//! submission whole: nothing in it runs. `queue.submit` returns nothing to
//! say so; the error goes to an error scope, or else to the device's
//! uncaptured-error handler. A renderer that does not look would keep the
//! results the frame never drew, hand the textures that failed to allocate
//! to later frames, and read the readback canary the rejected submission
//! never wrote as a device that drops work.
//!
//! So every renderer command encoder opens a [`FrameErrorScope`], one
//! out-of-memory and one validation scope, which its submission closes.
//! Natively a scope resolves at once: an error it caught is the frame's
//! failure ([`crate::RenderError::Gpu`]), and the frame stops before it keeps
//! a result, tracks a canary, or hands back a readback. A browser resolves
//! scopes only on its event loop, so there the embedding reports the
//! uncaptured errors it receives ([`GpuErrorReports::report`]) instead, and
//! the renderer drops what it kept before its next frame and reads a missing
//! canary after such a report as the rejection, not as a device that drops
//! work.
//!
//! Either way, what the renderer pooled before the error is no longer
//! trusted ([`crate::retire::Retirement::doubt`]): a pooled texture or buffer
//! created then may be the invalid one, and nothing can ask `wgpu` which.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use alpha2_gpu::GpuErrorKind;

use crate::Shared;
use crate::retire::Retirement;

/// The validation and out-of-memory scopes of one renderer command encoder,
/// from its creation until its submission.
///
/// Error scopes belong to the thread that pushed them and close in reverse
/// order, so a renderer encoder stays on the thread that created it (this is
/// neither `Send` nor `Sync`), and one opened inside another's lifetime is
/// submitted or dropped first.
#[derive(Debug)]
pub(crate) struct FrameErrorScope {
    #[cfg(not(target_arch = "wasm32"))]
    scopes: Option<[wgpu::ErrorScopeGuard; 2]>,
    /// Told when an encoder is dropped unsubmitted with an error caught: its
    /// failed encode may have pooled an invalid resource.
    retirement: Shared<Retirement>,
    #[cfg(target_arch = "wasm32")]
    _thread_bound: std::marker::PhantomData<*mut ()>,
}

impl FrameErrorScope {
    /// Opens the scopes on this thread. On the browser nothing is opened:
    /// its scopes resolve on the event loop, after the frame has returned.
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

    /// Closes the scopes and returns what the GPU said about the work done
    /// under them, out of memory before validation: an allocation that
    /// failed makes every later use of it a validation error too, and the
    /// allocation is the cause.
    pub(crate) fn close(mut self) -> Option<GpuErrorKind> {
        self.pop()
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn pop(&mut self) -> Option<GpuErrorKind> {
        let [out_of_memory, validation] = self.scopes.take()?;
        // Reverse order of the pushes.
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

/// Runs `create` under out-of-memory and validation scopes and returns what
/// they caught, out of memory first, as [`FrameErrorScope::close`] does.
///
/// For resources made outside a frame, such as an effect's lookup table,
/// whose failure must refuse the call instead of reaching a later frame. On
/// the browser no scope opens, as for a frame: its scopes resolve only on the
/// event loop, and the uncaptured-error report covers it instead.
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

/// A native scope's result, which `wgpu-core` has ready when it is popped.
#[cfg(not(target_arch = "wasm32"))]
fn resolved<T>(future: impl std::future::Future<Output = Option<T>>) -> Option<T> {
    let mut future = std::pin::pin!(future);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut context) {
        std::task::Poll::Ready(result) => result,
        // Not on wgpu-core; nothing to report then.
        std::task::Poll::Pending => None,
    }
}

/// GPU errors the embedding saw that no scope of the renderer's caught:
/// on the browser, every uncaptured error its device reports.
///
/// Cloning shares one record, so the embedding keeps a clone in its
/// device's error handler. Reporting only records; the renderer acts on it
/// at its next encoder, on its own thread.
#[derive(Clone, Debug, Default)]
pub struct GpuErrorReports {
    inner: Arc<ReportState>,
}

#[derive(Debug, Default)]
struct ReportState {
    /// Errors reported so far.
    reported: AtomicU64,
    /// Of those, how many the renderer has dropped its kept state for.
    handled: AtomicU64,
    /// The last one's kind, as [`kind_code`] encodes it.
    last: AtomicU8,
}

impl GpuErrorReports {
    /// Records an error the device reported outside any of the renderer's
    /// scopes. Cheap and lock-free: a device's error handler may call it
    /// from any thread.
    pub fn report(&self, kind: GpuErrorKind) {
        self.inner.last.store(kind_code(kind), Ordering::Release);
        self.inner.reported.fetch_add(1, Ordering::AcqRel);
    }

    /// How many errors have been reported.
    pub(crate) fn count(&self) -> u64 {
        self.inner.reported.load(Ordering::Acquire)
    }

    /// The kind of the last error reported, if any was.
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
