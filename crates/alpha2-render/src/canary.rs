//! Proof that the GPU ran a readback's submission.
//!
//! A macOS process whose work hangs the GPU twice has every later submission
//! ignored by the kernel (`Deny submissions/ignore app[...] with 2
//! GPURestarts`). `wgpu` 30's Metal backend treats a command buffer that
//! ended in error as completed, so nothing reports it: no device loss, no
//! validation error, no timeout. Each readback maps on time, and holds
//! whatever its staging buffer held before, zeros or an earlier frame, and
//! an export would write those as frames.
//!
//! So each readback's submission also copies sixteen bytes the CPU wrote,
//! a fixed tag and a number no earlier readback of this renderer used, to
//! the end of its staging buffer. A readback whose staging buffer does not
//! end in them was not run by the GPU. The renderer then refuses further
//! work, since the device no longer runs it, and the caller has to start a
//! new process.
//!
//! A submission that reads nothing back, the NV12 pack and repack that feed
//! a hardware encoder, copies its canary into a small buffer of its own,
//! which is mapped as soon as it is submitted and checked at the next such
//! submission ([`SubmissionCanaries`]). Its frame has gone to the encoder by
//! then, so a missing canary fails the export rather than the frame, and
//! [`crate::Renderer::confirm_gpu_work`] checks the last ones before an
//! export reports success.
//!
//! A canary is missing for one more reason: the GPU rejected the
//! submission, for an allocation that failed or a validation error, and ran
//! none of it. The device is fine then, and the frame's error is that error
//! ([`crate::gpu_errors`]). Natively the frame's own error scopes catch it,
//! and such a frame never gets as far as having its canary checked. On the
//! browser the device reports it later, and a canary missing after such a
//! report is read as the rejection: it fails the frame with the reported
//! error and leaves the renderer usable.
//!
//! A device that can no longer take the canary's sixteen bytes is lost too
//! ([`RenderError::GpuDeviceLost`]), and the renderer refuses further work
//! the same way.
//!
//! The buffers a canary is copied from, and those a submission's canary is
//! copied into, are pooled ([`CanaryBuffers`]): on the browser's WebGPU a
//! buffer that is only dropped stays until garbage collection, and each
//! frame would otherwise make one or two. The CPU writes a canary into its
//! source with `queue.write_buffer`, which lands before the next submission
//! on the queue runs. So a source is not handed to another canary while the
//! canary it holds may still be copied: it goes back to the pool when its
//! [`Canary`] is dropped, which is after the copy's submission, or after the
//! encoder that would have copied it was given up. A later write to it then
//! lands after that submission, since submissions run in queue order.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::retire::{Retired, Retirement};
use crate::{GpuErrorReports, RenderError, Shared};

/// Bytes a canary adds to the end of a staging buffer.
pub(crate) const CANARY_BYTES: u64 = 16;

/// Tells a canary from bytes that happen to hold a small number.
const TAG: u64 = 0xa1fa_2ca7_5e17_0001;

/// The most idle buffers each canary pool keeps; more are destroyed. A
/// renderer has at most a few readbacks and submissions in flight, so this
/// is never reached in steady work, and it bounds what a burst leaves.
const MAX_IDLE_CANARY_BUFFERS: usize = 32;

/// The renderer's canaries: the numbers issued, whether the device was
/// found unable to run work, and the buffers canaries travel in.
#[derive(Debug)]
pub(crate) struct Canaries {
    next: AtomicU64,
    lost: Arc<LostState>,
    reports: GpuErrorReports,
    /// What a canary is copied from: written by the CPU, read by the copy.
    sources: Shared<CanaryBuffers>,
    /// What a submission's canary is copied into, for a submission with no
    /// staging buffer of its own.
    submission_buffers: Shared<CanaryBuffers>,
}

/// A pool of sixteen-byte canary buffers of one usage.
#[derive(Debug)]
pub(crate) struct CanaryBuffers {
    label: &'static str,
    usage: wgpu::BufferUsages,
    retirement: Shared<Retirement>,
    state: Mutex<CanaryBuffersState>,
}

#[derive(Debug, Default)]
struct CanaryBuffersState {
    /// Each idle buffer and the generation it was created in.
    idle: Vec<(wgpu::Buffer, u64)>,
    /// Buffers created since the renderer was.
    created: u64,
}

/// A canary buffer taken from its pool. Dropped, it is destroyed; recycled,
/// it goes back to the pool.
#[derive(Debug)]
pub(crate) struct CanaryBuffer {
    pool: Shared<CanaryBuffers>,
    buffer: Option<wgpu::Buffer>,
    generation: u64,
}

impl CanaryBuffers {
    fn new(label: &'static str, usage: wgpu::BufferUsages, retirement: Shared<Retirement>) -> Self {
        Self {
            label,
            usage,
            retirement,
            state: Mutex::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CanaryBuffersState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// An idle buffer created since the GPU last reported an error, or a new
    /// one. Idle buffers created before such a report are destroyed: one may
    /// be an allocation that failed (see [`crate::gpu_errors`]).
    fn acquire(self: &Shared<Self>, device: &wgpu::Device) -> CanaryBuffer {
        let generation = self.retirement.generation();
        let mut state = self.lock();
        let mut untrusted = Vec::new();
        let mut reused = None;
        while let Some((buffer, created_in)) = state.idle.pop() {
            if created_in == generation {
                reused = Some(buffer);
                break;
            }
            untrusted.push((Retired::Buffer(buffer), CANARY_BYTES));
        }
        if reused.is_none() {
            state.created = state.created.saturating_add(1);
        }
        drop(state);
        self.retirement.retire(untrusted);
        let buffer = reused.unwrap_or_else(|| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(self.label),
                size: CANARY_BYTES,
                usage: self.usage,
                mapped_at_creation: false,
            })
        });
        CanaryBuffer {
            pool: Shared::clone(self),
            buffer: Some(buffer),
            generation,
        }
    }

    /// Keeps `buffer` for reuse, unless it is untrusted or the pool is full.
    fn release(&self, buffer: wgpu::Buffer, generation: u64) {
        let mut retired = None;
        if self.retirement.trusts(generation) {
            let mut state = self.lock();
            if state.idle.len() < MAX_IDLE_CANARY_BUFFERS {
                state.idle.push((buffer, generation));
            } else {
                retired = Some(buffer);
            }
        } else {
            retired = Some(buffer);
        }
        self.retirement
            .retire(retired.map(|buffer| (Retired::Buffer(buffer), CANARY_BYTES)));
    }

    /// Destroys every idle buffer.
    pub(crate) fn clear(&self) {
        let idle = std::mem::take(&mut self.lock().idle);
        self.retirement.retire(
            idle.into_iter()
                .map(|(buffer, _)| (Retired::Buffer(buffer), CANARY_BYTES)),
        );
    }

    /// Buffers created since the renderer was.
    #[cfg(test)]
    pub(crate) fn created(&self) -> u64 {
        self.lock().created
    }

    /// Idle buffers kept for reuse.
    #[cfg(test)]
    pub(crate) fn idle(&self) -> usize {
        self.lock().idle.len()
    }
}

impl CanaryBuffer {
    pub(crate) fn buffer(&self) -> &wgpu::Buffer {
        self.buffer.as_ref().expect("live canary buffer")
    }

    /// Gives the buffer back to its pool for another canary.
    pub(crate) fn recycle(mut self) {
        if let Some(buffer) = self.buffer.take() {
            self.pool.release(buffer, self.generation);
        }
    }
}

impl Drop for CanaryBuffer {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            self.pool
                .retirement
                .retire([(Retired::Buffer(buffer), CANARY_BYTES)]);
        }
    }
}

/// Whether the device was found unable to run work, and how: one of the
/// `LOST_*` values, and whether a long submission of this renderer's caused
/// it, as the submission clock said when the loss was found.
#[derive(Debug, Default)]
pub(crate) struct LostState {
    how: AtomicU8,
    clock: Arc<crate::submission_clock::SubmissionClock>,
    hung: Mutex<Option<RenderError>>,
}

const NOT_LOST: u8 = 0;
const LOST_WORK_DROPPED: u8 = 1;
const LOST_DEVICE: u8 = 2;

impl LostState {
    fn new(clock: Arc<crate::submission_clock::SubmissionClock>) -> Self {
        Self {
            how: AtomicU8::new(NOT_LOST),
            clock,
            hung: Mutex::new(None),
        }
    }

    /// Records the first way the device was found lost; a later one does not
    /// replace it. A loss found while a recent submission had run for the hung
    /// threshold is kept as hung.
    fn mark(&self, how: u8) {
        if self
            .how
            .compare_exchange(NOT_LOST, how, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            && let Some(report) = self.clock.hung()
        {
            *self
                .hung
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hung_error(&report));
        }
    }

    fn error(&self) -> Option<RenderError> {
        let lost = match self.how.load(Ordering::Acquire) {
            LOST_WORK_DROPPED => RenderError::GpuWorkDropped,
            LOST_DEVICE => RenderError::GpuDeviceLost,
            _ => return None,
        };
        Some(
            self.hung
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .unwrap_or(lost),
        )
    }
}

/// The [`RenderError::GpuDeviceHung`] a long submission is evidence of.
pub(crate) fn hung_error(report: &crate::SubmissionReport) -> RenderError {
    RenderError::GpuDeviceHung {
        frame: report.frame.and_then(|frame| frame.frame_index),
        time_ticks: report.frame.map(|frame| frame.time_ticks),
        // Whole milliseconds; a span past `u32::MAX` ms (49 days) cannot happen.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        gpu_ms: report.gpu_ms.round().clamp(0.0, f64::from(u32::MAX)) as u32,
        watchdog_ms: alpha2_gpu::watchdog_ms(),
    }
}

impl Canaries {
    /// Canaries that read a missing one after an error in `reports` as that
    /// error, with buffers that `retirement` destroys, and that take a loss
    /// after a long submission on `clock` as hung.
    pub(crate) fn new(
        reports: GpuErrorReports,
        retirement: &Shared<Retirement>,
        clock: Arc<crate::submission_clock::SubmissionClock>,
    ) -> Self {
        Self {
            next: AtomicU64::new(0),
            lost: Arc::new(LostState::new(clock)),
            reports,
            sources: Shared::new(CanaryBuffers::new(
                "alpha2 readback canary",
                wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
                Shared::clone(retirement),
            )),
            submission_buffers: Shared::new(CanaryBuffers::new(
                "alpha2 submission canary",
                wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                Shared::clone(retirement),
            )),
        }
    }

    /// Encodes a copy of a fresh canary into `destination` at `offset`,
    /// which the GPU performs only if it runs the submission.
    ///
    /// # Errors
    /// Returns [`RenderError::GpuDeviceLost`] when the device can no longer
    /// take the canary's bytes, and from then on the renderer refuses work.
    pub(crate) fn encode(
        &self,
        gpu: &alpha2_gpu::GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        destination: &wgpu::Buffer,
        offset: u64,
    ) -> Result<Canary, RenderError> {
        let mut canary = self.issue();
        let source = self.sources.acquire(gpu.device());
        // Written by the CPU before the submission runs, so it is the copy alone that
        // the GPU has to run.
        let Some(mut write) = gpu.queue().write_buffer_with(
            source.buffer(),
            0,
            wgpu::BufferSize::new(CANARY_BYTES).expect("a canary has bytes"),
        ) else {
            // `wgpu` refuses the write once the device is lost.
            self.lost.mark(LOST_DEVICE);
            return Err(self.lost.error().unwrap_or(RenderError::GpuDeviceLost));
        };
        write.copy_from_slice(&canary.expected());
        drop(write);
        encoder.copy_buffer_to_buffer(source.buffer(), 0, destination, offset, CANARY_BYTES);
        canary.source = Some(Box::new(source));
        Ok(canary)
    }

    /// Encodes a copy of a fresh canary into a buffer of its own, for a
    /// submission with no staging buffer to carry it.
    ///
    /// # Errors
    /// Returns [`RenderError::GpuDeviceLost`] as [`Canaries::encode`] does.
    pub(crate) fn encode_own(
        &self,
        gpu: &alpha2_gpu::GpuContext,
        encoder: &mut wgpu::CommandEncoder,
    ) -> Result<(CanaryBuffer, Canary), RenderError> {
        let buffer = self.submission_buffers.acquire(gpu.device());
        let canary = self.encode(gpu, encoder, buffer.buffer(), 0)?;
        Ok((buffer, canary))
    }

    /// Destroys the idle canary buffers.
    pub(crate) fn clear(&self) {
        self.sources.clear();
        self.submission_buffers.clear();
    }

    /// The pool of buffers canaries are copied from.
    #[cfg(test)]
    pub(crate) fn sources(&self) -> &CanaryBuffers {
        &self.sources
    }

    /// The pool of buffers submissions' canaries are copied into.
    #[cfg(test)]
    pub(crate) fn submission_buffers(&self) -> &CanaryBuffers {
        &self.submission_buffers
    }

    /// A canary with a number no earlier one had.
    pub(crate) fn issue(&self) -> Canary {
        let number = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        Canary {
            number,
            lost: Arc::clone(&self.lost),
            reports: self.reports.clone(),
            reported_before: self.reports.count(),
            source: None,
        }
    }

    /// How many canaries have been issued.
    #[cfg(test)]
    pub(crate) fn issued(&self) -> u64 {
        self.next.load(Ordering::Relaxed)
    }

    /// How the device was found unable to run work, after which the renderer
    /// refuses work.
    pub(crate) fn lost(&self) -> Option<RenderError> {
        self.lost.error()
    }
}

/// One readback's canary: what its staging buffer must end in.
#[derive(Debug)]
pub(crate) struct Canary {
    /// Its number: the canary is the tag, then this, little-endian.
    number: u64,
    lost: Arc<LostState>,
    reports: GpuErrorReports,
    /// Errors reported before the canary's work was encoded.
    reported_before: u64,
    /// The buffer the canary is copied from, back to its pool when the
    /// canary is dropped: by then its copy was submitted or given up. Boxed,
    /// so that a readback that carries a canary stays small to move.
    source: Option<Box<CanaryBuffer>>,
}

impl Drop for Canary {
    fn drop(&mut self) {
        if let Some(source) = self.source.take() {
            source.recycle();
        }
    }
}

impl Canary {
    /// The sixteen bytes the canary is.
    fn expected(&self) -> [u8; 16] {
        let mut expected = [0; 16];
        expected[..8].copy_from_slice(&TAG.to_le_bytes());
        expected[8..].copy_from_slice(&self.number.to_le_bytes());
        expected
    }

    /// Checks the canary's bytes in a mapped staging buffer, and marks the
    /// renderer's device as dropping work when they are not there and no
    /// GPU error was reported since the work was encoded.
    ///
    /// # Errors
    /// Returns [`RenderError::Gpu`] with the reported error's kind when one
    /// was reported since: the GPU rejected the submission, and the device is
    /// fine. Otherwise [`RenderError::GpuWorkDropped`] when the bytes are not
    /// there.
    pub(crate) fn check(&self, found: &[u8]) -> Result<(), RenderError> {
        if found == self.expected() {
            return Ok(());
        }
        if self.reports.count() > self.reported_before
            && let Some(kind) = self.reports.last_kind()
        {
            return Err(RenderError::Gpu { kind });
        }
        self.lost.mark(LOST_WORK_DROPPED);
        Err(self.lost.error().unwrap_or(RenderError::GpuWorkDropped))
    }
}

/// The canaries of submitted work that reads nothing back, oldest first,
/// each in a buffer whose mapping was requested when it was submitted.
#[derive(Debug, Default)]
pub(crate) struct SubmissionCanaries {
    pending: Mutex<VecDeque<PendingCanary>>,
    /// Notified by each mapping's callback, on whichever thread runs it.
    pub(crate) reports: Arc<crate::settle::CallbackSignal>,
    /// Test-only: each mapping reports from a thread of its own this long
    /// after its callback ran, as one another thread's poll collected does.
    #[cfg(test)]
    pub(crate) report_elsewhere_after: Mutex<Option<std::time::Duration>>,
}

#[derive(Debug)]
struct PendingCanary {
    buffer: CanaryBuffer,
    canary: Canary,
    /// `None` until the mapping reports; then whether it mapped.
    mapped: Arc<Mutex<Option<bool>>>,
}

impl SubmissionCanaries {
    /// Tracks `canary`, whose copy into `buffer` was just submitted.
    pub(crate) fn track(&self, gpu: &alpha2_gpu::GpuContext, buffer: CanaryBuffer, canary: Canary) {
        let mapped = Arc::new(Mutex::new(None));
        let reported = Arc::clone(&mapped);
        let reports = Arc::clone(&self.reports);
        #[cfg(test)]
        let elsewhere = *self
            .report_elsewhere_after
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        gpu.map_async(
            &buffer.buffer().slice(..),
            wgpu::MapMode::Read,
            move |result| {
                let report = move || {
                    *reported
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result.is_ok());
                    reports.notify();
                };
                #[cfg(test)]
                if let Some(delay) = elsewhere {
                    std::thread::spawn(move || {
                        std::thread::sleep(delay);
                        report();
                    });
                    return;
                }
                report();
            },
        );
        self.lock().push_back(PendingCanary {
            buffer,
            canary,
            mapped,
        });
    }

    /// Checks every tracked canary whose buffer has mapped, stopping at the
    /// first that has not. One that failed to map counts as missing.
    ///
    /// # Errors
    /// Returns [`RenderError::GpuWorkDropped`] for a missing canary.
    pub(crate) fn check_mapped(&self) -> Result<(), RenderError> {
        let mut pending = self.lock();
        while let Some(front) = pending.front() {
            let state = *front
                .mapped
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(mapped) = state else {
                break;
            };
            let PendingCanary { buffer, canary, .. } =
                pending.pop_front().expect("the front entry exists");
            let checked = match mapped.then(|| buffer.buffer().slice(..).get_mapped_range()) {
                Some(Ok(bytes)) => canary.check(&bytes),
                _ => canary.check(&[]),
            };
            // One that mapped is unmapped and used again; one that did not is destroyed.
            if mapped {
                buffer.buffer().unmap();
                buffer.recycle();
            }
            checked?;
        }
        Ok(())
    }

    /// How many tracked canaries have not been checked yet.
    #[cfg(any(test, not(target_arch = "wasm32")))]
    pub(crate) fn outstanding(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<PendingCanary>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_canary_passes_only_its_own_bytes() {
        let canaries = Canaries::new(
            GpuErrorReports::default(),
            &Shared::default(),
            Arc::default(),
        );
        let first = canaries.issue();
        let second = canaries.issue();
        assert_eq!(first.check(&first.expected()), Ok(()));
        assert_eq!(canaries.lost(), None);
        // A reused staging buffer the GPU never wrote holds the previous readback's canary.
        assert_eq!(
            second.check(&first.expected()),
            Err(RenderError::GpuWorkDropped)
        );
        assert_eq!(canaries.lost(), Some(RenderError::GpuWorkDropped));
        assert_eq!(first.check(&[0; 16]), Err(RenderError::GpuWorkDropped));
    }

    #[test]
    fn a_canary_missing_after_a_reported_error_is_that_error_and_the_device_is_fine() {
        // A browser rejects a submission whose texture failed to allocate, and says so
        // only through its uncaptured-error event.
        let reports = GpuErrorReports::default();
        let canaries = Canaries::new(reports.clone(), &Shared::default(), Arc::default());
        reports.report(alpha2_gpu::GpuErrorKind::Validation);
        let before = canaries.issue();
        let rejected = canaries.issue();
        reports.report(alpha2_gpu::GpuErrorKind::OutOfMemory);
        assert_eq!(
            rejected.check(&[0; 16]),
            Err(RenderError::Gpu {
                kind: alpha2_gpu::GpuErrorKind::OutOfMemory
            })
        );
        assert_eq!(canaries.lost(), None);
        // One encoded after the report, with no error since: a device that drops work.
        let after = canaries.issue();
        assert_eq!(
            after.check(&before.expected()),
            Err(RenderError::GpuWorkDropped)
        );
        assert_eq!(canaries.lost(), Some(RenderError::GpuWorkDropped));
    }
}
