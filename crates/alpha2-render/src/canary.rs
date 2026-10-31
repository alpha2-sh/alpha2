//! Proof that the GPU ran a readback's submission.
//!
//! After two GPU hangs macOS ignores every later submission from the process, and `wgpu`'s Metal
//! backend reports such command buffers as completed. Readbacks then map on time holding stale
//! bytes. So each readback also copies sixteen CPU-written bytes (a tag and a fresh number) to
//! the end of its staging buffer; if they are missing, the GPU did not run the work and the
//! renderer refuses further work.
//!
//! Submissions with no readback (NV12 pack and repack) copy their canary into a small buffer of
//! their own, checked at the next such submission ([`SubmissionCanaries`]) and by
//! [`crate::Renderer::confirm_gpu_work`] at the end of an export.
//!
//! A canary is also missing when the GPU rejected the submission (failed allocation or
//! validation). Natively the frame's error scopes catch that first. In a browser the error is
//! reported later, and a missing canary after such a report fails only the frame.
//!
//! Canary buffers are pooled ([`CanaryBuffers`]) because browser `WebGPU` frees dropped buffers
//! only at garbage collection. A source buffer returns to the pool only when its [`Canary`] is
//! dropped, after the copy's submission, so the next `write_buffer` cannot overtake that copy.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::retire::{Retired, Retirement};
use crate::{GpuErrorReports, RenderError, Shared};

/// Bytes a canary adds to the end of a staging buffer.
pub(crate) const CANARY_BYTES: u64 = 16;

/// Tells a canary from bytes that happen to hold a small number.
const TAG: u64 = 0xa1fa_2ca7_5e17_0001;

/// Most idle buffers each pool keeps; only bounds what a burst leaves behind.
const MAX_IDLE_CANARY_BUFFERS: usize = 32;

#[derive(Debug)]
pub(crate) struct Canaries {
    next: AtomicU64,
    lost: Arc<LostState>,
    reports: GpuErrorReports,
    /// Written by the CPU, read by the copy.
    sources: Shared<CanaryBuffers>,
    /// Destinations for canaries of submissions without a staging buffer.
    submission_buffers: Shared<CanaryBuffers>,
}

#[derive(Debug)]
pub(crate) struct CanaryBuffers {
    label: &'static str,
    usage: wgpu::BufferUsages,
    retirement: Shared<Retirement>,
    state: Mutex<CanaryBuffersState>,
}

#[derive(Debug, Default)]
struct CanaryBuffersState {
    idle: Vec<(wgpu::Buffer, u64)>,
    created: u64,
}

/// A canary buffer taken from its pool; destroyed on drop unless recycled.
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

    /// An idle buffer created since the last reported GPU error, or a new one. Older idle buffers
    /// are destroyed since one may be a failed allocation.
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

    pub(crate) fn clear(&self) {
        let idle = std::mem::take(&mut self.lock().idle);
        self.retirement.retire(
            idle.into_iter()
                .map(|(buffer, _)| (Retired::Buffer(buffer), CANARY_BYTES)),
        );
    }

    #[cfg(test)]
    pub(crate) fn created(&self) -> u64 {
        self.lock().created
    }

    #[cfg(test)]
    pub(crate) fn idle(&self) -> usize {
        self.lock().idle.len()
    }
}

impl CanaryBuffer {
    pub(crate) fn buffer(&self) -> &wgpu::Buffer {
        self.buffer.as_ref().expect("live canary buffer")
    }

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

/// Whether and how the device was found unable to run work (a `LOST_*` value), and whether a
/// long submission caused it.
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

    /// Records the first loss only. A loss found while a recent submission ran past the hung
    /// threshold is recorded as hung.
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

pub(crate) fn hung_error(report: &crate::SubmissionReport) -> RenderError {
    RenderError::GpuDeviceHung {
        frame: report.frame.and_then(|frame| frame.frame_index),
        time_ticks: report.frame.map(|frame| frame.time_ticks),
        // Whole milliseconds; overflowing `u32` would take 49 days.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        gpu_ms: report.gpu_ms.round().clamp(0.0, f64::from(u32::MAX)) as u32,
        watchdog_ms: alpha2_gpu::watchdog_ms(),
    }
}

impl Canaries {
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

    /// Encodes a copy of a fresh canary into `destination` at `offset`.
    ///
    /// # Errors [`RenderError::GpuDeviceLost`] when the device no longer accepts the write; the
    /// renderer then refuses work.
    pub(crate) fn encode(
        &self,
        gpu: &alpha2_gpu::GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        destination: &wgpu::Buffer,
        offset: u64,
    ) -> Result<Canary, RenderError> {
        let mut canary = self.issue();
        let source = self.sources.acquire(gpu.device());
        // Written before the submission runs, so only the copy depends on the GPU.
        let Some(mut write) = gpu.queue().write_buffer_with(
            source.buffer(),
            0,
            wgpu::BufferSize::new(CANARY_BYTES).expect("a canary has bytes"),
        ) else {
            self.lost.mark(LOST_DEVICE);
            return Err(self.lost.error().unwrap_or(RenderError::GpuDeviceLost));
        };
        write.copy_from_slice(&canary.expected());
        drop(write);
        encoder.copy_buffer_to_buffer(source.buffer(), 0, destination, offset, CANARY_BYTES);
        canary.source = Some(Box::new(source));
        Ok(canary)
    }

    /// Encodes a fresh canary into a buffer of its own, for a submission with no staging buffer.
    ///
    /// # Errors
    /// [`RenderError::GpuDeviceLost`] as [`Canaries::encode`].
    pub(crate) fn encode_own(
        &self,
        gpu: &alpha2_gpu::GpuContext,
        encoder: &mut wgpu::CommandEncoder,
    ) -> Result<(CanaryBuffer, Canary), RenderError> {
        let buffer = self.submission_buffers.acquire(gpu.device());
        let canary = self.encode(gpu, encoder, buffer.buffer(), 0)?;
        Ok((buffer, canary))
    }

    pub(crate) fn clear(&self) {
        self.sources.clear();
        self.submission_buffers.clear();
    }

    #[cfg(test)]
    pub(crate) fn sources(&self) -> &CanaryBuffers {
        &self.sources
    }

    #[cfg(test)]
    pub(crate) fn submission_buffers(&self) -> &CanaryBuffers {
        &self.submission_buffers
    }

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

    #[cfg(test)]
    pub(crate) fn issued(&self) -> u64 {
        self.next.load(Ordering::Relaxed)
    }

    pub(crate) fn lost(&self) -> Option<RenderError> {
        self.lost.error()
    }
}

#[derive(Debug)]
pub(crate) struct Canary {
    /// The canary is the tag followed by this, little-endian.
    number: u64,
    lost: Arc<LostState>,
    reports: GpuErrorReports,
    /// Error count reported before the canary's work was encoded.
    reported_before: u64,
    /// Returned to its pool on drop, after the copy was submitted or abandoned. Boxed to keep
    /// readbacks small.
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
    fn expected(&self) -> [u8; 16] {
        let mut expected = [0; 16];
        expected[..8].copy_from_slice(&TAG.to_le_bytes());
        expected[8..].copy_from_slice(&self.number.to_le_bytes());
        expected
    }

    /// Checks the canary in a mapped staging buffer.
    ///
    /// # Errors
    /// [`RenderError::Gpu`] when an error was reported since encoding (the submission was rejected
    /// and the device is fine); otherwise [`RenderError::GpuWorkDropped`] if the bytes are missing,
    /// marking the device as dropping work.
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

/// Canaries of submitted work without a readback, oldest first, each mapping as submitted.
#[derive(Debug, Default)]
pub(crate) struct SubmissionCanaries {
    pending: Mutex<VecDeque<PendingCanary>>,
    /// Notified by each mapping callback, on whatever thread runs it.
    pub(crate) reports: Arc<crate::settle::CallbackSignal>,
    /// Test-only: report each mapping from another thread after this delay.
    #[cfg(test)]
    pub(crate) report_elsewhere_after: Mutex<Option<std::time::Duration>>,
}

#[derive(Debug)]
struct PendingCanary {
    buffer: CanaryBuffer,
    canary: Canary,
    mapped: Arc<Mutex<Option<bool>>>,
}

impl SubmissionCanaries {
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

    /// Checks mapped canaries in order, stopping at the first unmapped one. A failed map counts as
    /// missing.
    ///
    /// # Errors
    /// [`RenderError::GpuWorkDropped`] for a missing canary.
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
            if mapped {
                buffer.buffer().unmap();
                buffer.recycle();
            }
            checked?;
        }
        Ok(())
    }

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
        // A browser reports a failed texture allocation only through its uncaptured-error event.
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
        // Encoded after the report with no error since, so this is dropped work.
        let after = canaries.issue();
        assert_eq!(
            after.check(&before.expected()),
            Err(RenderError::GpuWorkDropped)
        );
        assert_eq!(canaries.lost(), Some(RenderError::GpuWorkDropped));
    }
}
