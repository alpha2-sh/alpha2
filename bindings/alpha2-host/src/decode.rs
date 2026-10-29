//! Decode workers, budgets, and the export decode scheduler.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::encode::PipelineFailure;
#[cfg(test)]
pub use crate::heartbeat::InjectedStall;
use crate::heartbeat::{CommandProgress, STALL_POLL, WorkerHeartbeat};
use crate::metrics::ExportMetrics;
use crate::{WORKER_RETRY_INTERVAL, error_message};
use alpha2_engine::{EngineProject, ExportFrameNeeds, Exporter};
use alpha2_eval::FrameNeed;
use alpha2_media::{CancelToken, DecodeError, DecodeRequest, DecodeRequestKind, SourceId};
use alpha2_model::{ClipId, MediaKind, NodeId, Project};
use alpha2_native::{
    DecodeStageTimings, HardwareDecodeFallback, NativeExportDecoder, NativeVideoFrame,
};
#[cfg(all(target_os = "windows", feature = "hwdecode"))]
use alpha2_native_gpu::SharedD3D12Context;
#[cfg(all(target_os = "macos", feature = "hwdecode"))]
use alpha2_native_gpu::SharedMetalContext;
#[cfg(all(target_os = "linux", feature = "hwdecode"))]
use alpha2_native_gpu::SharedVulkanContext;
use alpha2_time::{MediaTime, TimeRange};

/// One output frame's sources for the decode pool to decode.
pub struct DecodeJob {
    pub generation: u64,
    pub index: u64,
    pub time: MediaTime,
    pub needs: Vec<FrameNeed>,
    /// Backward runs that start at this frame: for each cursor whose source
    /// time runs backward from here, every time it will ask for until it
    /// turns forward or stops, in the order it asks. A shard decodes a run a
    /// window at a time, from the keyframe before the window's lowest time,
    /// so each source frame of a long run is decoded about once.
    pub reverse_runs: HashMap<DecodeCursorKey, Vec<MediaTime>>,
}

/// One source picture decoded for a frame, as the clip asked for it.
pub struct DecodedSourceFrame {
    pub source: SourceId,
    pub clip: ClipId,
    pub requested: MediaTime,
    pub kind: MediaKind,
    pub frame: Arc<NativeVideoFrame>,
}

/// Every source picture one output frame reads, with what decoding it cost.
pub struct DecodedFrameSet {
    pub generation: u64,
    pub index: u64,
    pub time: MediaTime,
    pub frames: Vec<DecodedSourceFrame>,
    /// Every still this frame shows, sorted, whether or not `frames` carries
    /// its picture. A still the previous frame also showed is not decoded
    /// again: the host still holds it. The host keeps exactly these stills,
    /// so one that leaves the picture is let go and decoded again if it
    /// comes back (see [`still_needs_for_job`]).
    pub stills: Vec<SourceId>,
    /// Bytes the distinct decoded pictures retain, charged to the decode
    /// queue budget until the host releases them.
    pub bytes: u64,
    pub decode_time: Duration,
    /// Where the decode sessions this set touched spent their time.
    pub stage_timings: DecodeStageTimings,
    pub reverse_cache_hits: u64,
    pub reverse_windows_built: u64,
    /// What the reverse windows hold once the set is decoded: a shard's,
    /// or for a set the pool hands over, every shard's, the ones this frame
    /// asked nothing of included.
    pub reverse_cache_bytes: u64,
    pub software_decoded_frames: u64,
    pub hardware_decoded_frames: u64,
    pub hardware_fallbacks: HardwareFallbackCounts,
    /// Decoder sessions reopened for this frame
    /// ([`ExportDecodeState::session_reopens`]).
    pub decoder_session_reopens: u64,
}

/// Software fallbacks counted per reason.
///
/// A fixed slot per reason keeps per-shard totals cheap to add and subtract,
/// which a map keyed by reason would not be.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HardwareFallbackCounts([u64; Self::REASONS.len()]);

impl HardwareFallbackCounts {
    const REASONS: [HardwareDecodeFallback; 10] = [
        HardwareDecodeFallback::ContextUnavailable,
        HardwareDecodeFallback::NoHardwareConfig,
        HardwareDecodeFallback::UnsupportedColor,
        HardwareDecodeFallback::UnsupportedProfile,
        HardwareDecodeFallback::UnsupportedSurfaceFormat,
        HardwareDecodeFallback::SourceUnavailable,
        HardwareDecodeFallback::DecoderOpenFailed,
        HardwareDecodeFallback::ReverseWindow,
        HardwareDecodeFallback::UnsupportedCrop,
        HardwareDecodeFallback::ImportFailed,
    ];

    fn slot(reason: HardwareDecodeFallback) -> usize {
        Self::REASONS
            .iter()
            .position(|candidate| *candidate == reason)
            .unwrap_or(0)
    }

    pub fn record(&mut self, reason: HardwareDecodeFallback) {
        self.record_many(reason, 1);
    }

    pub fn record_many(&mut self, reason: HardwareDecodeFallback, count: u64) {
        let slot = Self::slot(reason);
        self.0[slot] = self.0[slot].saturating_add(count);
    }

    #[must_use]
    pub fn merged(self, other: Self) -> Self {
        let mut merged = self;
        for (total, added) in merged.0.iter_mut().zip(other.0) {
            *total = total.saturating_add(added);
        }
        merged
    }

    #[must_use]
    pub fn since(self, before: Self) -> Self {
        let mut delta = self;
        for (total, earlier) in delta.0.iter_mut().zip(before.0) {
            *total = total.saturating_sub(earlier);
        }
        delta
    }

    /// Non-zero reasons paired with their stable report key.
    pub fn entries(self) -> impl Iterator<Item = (&'static str, u64)> {
        Self::REASONS
            .into_iter()
            .zip(self.0)
            .filter(|(_, count)| *count > 0)
            .map(|(reason, count)| (reason.as_str(), count))
    }
}

/// Why one cursor's decode failed, and whether software can still answer it.
enum DecodeVideoFailure {
    /// The hardware session cannot go on, for a reason software does not
    /// share: it could not start on this stream, or produced a surface this
    /// build cannot import.
    Hardware {
        reason: HardwareDecodeFallback,
        message: String,
    },
    /// The request cannot be satisfied by any decoder.
    Fatal(String),
}

impl DecodeVideoFailure {
    /// Classifies a decode error, treating what the hardware session reported
    /// as unsupported as recoverable and everything else as fatal.
    fn classify(error: &DecodeError, decoder: &NativeExportDecoder) -> Self {
        if decoder.is_hardware() && matches!(error, DecodeError::Unsupported(_)) {
            Self::Hardware {
                reason: decoder
                    .hardware_failure()
                    .unwrap_or(HardwareDecodeFallback::UnsupportedSurfaceFormat),
                message: error.to_string(),
            }
        } else {
            Self::Fatal(crate::error_message(error.clone()))
        }
    }

    fn into_message(self) -> String {
        match self {
            Self::Hardware { message, .. } | Self::Fatal(message) => message,
        }
    }
}

/// Decode-path counts gathered while a decoder is mutably borrowed.
#[derive(Clone, Copy, Debug, Default)]
struct DecodeTally {
    software: u64,
    hardware: u64,
}

impl DecodeTally {
    fn count(&mut self, frame: &NativeVideoFrame) {
        if frame.is_hardware() {
            self.hardware = self.hardware.saturating_add(1);
        } else {
            self.software = self.software.saturating_add(1);
        }
    }
}

/// Work the coordinator hands the decode pool.
pub enum DecodeCommand {
    Frame(DecodeJob),
    Finish,
}

/// How the decode pool stopped.
pub enum DecodeWorkerStatus {
    Finished,
    Failed(String),
}

#[derive(Default)]
pub struct DecodeBudgetState {
    pub bytes: u64,
    pub peak_bytes: u64,
}

/// The byte budget on decoded frame sets queued for the host but not yet
/// released.
pub struct DecodeBudget {
    /// The decode queue budget; a frame set larger than it still fits alone
    /// up to the budget before any `memory.scale` ([`ByteBudget::holding`](crate::memory::ByteBudget::holding)).
    pub limit: crate::memory::ByteBudget,
    pub state: Mutex<DecodeBudgetState>,
    pub available: Condvar,
}

impl DecodeBudget {
    /// Charges `bytes` to the budget, waiting until enough is released.
    ///
    /// # Errors
    /// A frame set larger than the whole budget, or a cancelled export.
    pub fn reserve(&self, bytes: u64, cancel: &CancelToken) -> Result<(), String> {
        let limit = self.limit.holding(bytes);
        if bytes > limit {
            return Err(crate::memory::resource_limit(format!(
                "one decoded frame set requires {bytes} bytes, exceeding the {limit} byte decode queue budget"
            )));
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A plain wait suffices: `cancel` takes this mutex before notifying, so a cancellation
        // between the check and the wait cannot be missed.
        while state.bytes.saturating_add(bytes) > limit {
            if cancel.is_cancelled() {
                return Err("decode worker was cancelled".to_owned());
            }
            state = self
                .available
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        state.bytes = state.bytes.saturating_add(bytes);
        state.peak_bytes = state.peak_bytes.max(state.bytes);
        Ok(())
    }

    pub fn release(&self, bytes: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.bytes = state.bytes.saturating_sub(bytes);
        self.available.notify_all();
    }

    pub fn cancel(&self) {
        // Taking the mutex first is what makes `reserve`'s plain wait safe.
        drop(
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        self.available.notify_all();
    }

    pub fn peak_bytes(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .peak_bytes
    }
}

/// Waits the decode pool takes after a frame set is already decoded.
///
/// Both are structurally zero while the host keeps up, which is exactly when
/// they are worth having: a nonzero value says the queue, not the decoder,
/// paced the run.
#[derive(Default)]
pub struct DecodePoolWaits {
    budget_nanos: AtomicU64,
    handoff_nanos: AtomicU64,
}

impl DecodePoolWaits {
    fn add(counter: &AtomicU64, waited: Duration) {
        counter.fetch_add(
            u64::try_from(waited.as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    /// Records time blocked reserving queue bytes for a decoded frame set.
    pub fn record_budget(&self, waited: Duration) {
        Self::add(&self.budget_nanos, waited);
    }

    /// Records time blocked handing a decoded frame set to the host.
    pub fn record_handoff(&self, waited: Duration) {
        Self::add(&self.handoff_nanos, waited);
    }

    /// Time blocked on the decode queue byte budget.
    pub fn budget(&self) -> Duration {
        Duration::from_nanos(self.budget_nanos.load(Ordering::Relaxed))
    }

    /// Time blocked handing decoded frame sets over.
    pub fn handoff(&self) -> Duration {
        Duration::from_nanos(self.handoff_nanos.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
thread_local! {
    /// Makes every job of the next decode pool started on this thread take
    /// this long, or until the export is cancelled, before it decodes: the
    /// pool is busy when a failure elsewhere cancels it, as it is decoding
    /// large frames.
    pub static SLOW_DECODE_JOBS: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// The coordinator's handle on the decode pool thread and its shards.
/// Dropping it cancels the pool and joins it, or abandons it once a shard is
/// found stuck.
pub struct DecodeWorker {
    pub commands: Option<SyncSender<DecodeCommand>>,
    /// Taken before the worker is joined. The pool blocks handing a decoded
    /// frame set over, so dropping this receiver is what releases it.
    pub frames: Option<Receiver<DecodedFrameSet>>,
    pub status: Receiver<DecodeWorkerStatus>,
    pub join: Option<JoinHandle<()>>,
    pub cancel: CancelToken,
    pub budget: Arc<DecodeBudget>,
    pub waits: Arc<DecodePoolWaits>,
    pub failure: PipelineFailure,
    /// One per shard.
    pub heartbeats: Vec<Arc<WorkerHeartbeat>>,
    pub progress: Arc<CommandProgress>,
    pub stall_timeout: Duration,
    /// Set once a shard was found stuck: its thread will never return, so it
    /// is left behind rather than joined.
    pub stalled: AtomicBool,
}

/// How a decode pool is sized and which devices its sessions may use.
#[derive(Clone)]
pub struct DecodeWorkerConfig {
    /// Sources an extension provider draws, shared by every decode session.
    pub providers: Arc<crate::providers::ProviderMedia>,
    /// The export's decode policy, and where its decoders report.
    pub decoding: Arc<crate::recovery::ExportDecoding>,
    pub depth: usize,
    pub byte_budget: crate::memory::ByteBudget,
    pub concurrency: usize,
    pub decoder_threads: i64,
    /// Workers a hardware session opens with; `decoder_threads` is a software one's.
    pub hardware_decoder_threads: i64,
    pub max_sessions: usize,
    /// When each cursor reads over the export range.
    pub reads: Arc<CursorReads>,
    pub reverse_cache_bytes: u64,
    pub reverse_hardware_surfaces: usize,
    /// See [`crate::config::configured_stall_timeout`].
    pub stall_timeout: Duration,
    #[cfg(all(target_os = "linux", feature = "hwdecode"))]
    pub shared_vulkan: Option<SharedVulkanContext>,
    #[cfg(all(target_os = "macos", feature = "hwdecode"))]
    pub shared_metal: Option<SharedMetalContext>,
    #[cfg(all(target_os = "windows", feature = "hwdecode"))]
    pub shared_d3d12: Option<SharedD3D12Context>,
    #[cfg(all(
        any(target_os = "linux", target_os = "macos", target_os = "windows"),
        feature = "hwdecode"
    ))]
    pub hardware_required: bool,
    /// Where every shard blocks forever, panics, or slows down.
    #[cfg(test)]
    pub stall: Option<InjectedStall>,
}

impl DecodeWorker {
    /// Starts the pool thread, which starts one shard thread per
    /// `config.concurrency`, splitting the session and reverse cache limits
    /// between them.
    ///
    /// # Errors
    /// A source without an uploaded asset, or an OS refusal of the pool
    /// thread.
    pub fn spawn(
        project: &Project,
        assets: &HashMap<String, PathBuf>,
        config: &DecodeWorkerConfig,
        cancel: CancelToken,
        failure: PipelineFailure,
    ) -> Result<Self, String> {
        let concurrency = config.concurrency.max(1);
        #[cfg(test)]
        let slow_jobs = SLOW_DECODE_JOBS.with(std::cell::Cell::take);
        let mut states = Vec::with_capacity(concurrency);
        let max_sessions = config.max_sessions.max(concurrency);
        for shard in 0..concurrency {
            let shard_sessions =
                max_sessions / concurrency + usize::from(shard < max_sessions % concurrency);
            states.push(ExportDecodeState::new_with_limits_and_vulkan(
                project,
                assets,
                config.decoder_threads,
                shard_sessions,
                config.reverse_cache_bytes / u64::try_from(concurrency).unwrap_or(1),
                #[cfg(all(target_os = "linux", feature = "hwdecode"))]
                config.shared_vulkan.clone(),
                #[cfg(all(target_os = "macos", feature = "hwdecode"))]
                config.shared_metal.clone(),
                #[cfg(all(target_os = "windows", feature = "hwdecode"))]
                config.shared_d3d12.clone(),
                #[cfg(all(
                    any(target_os = "linux", target_os = "macos", target_os = "windows"),
                    feature = "hwdecode"
                ))]
                config.hardware_required,
            )?);
            if let Some(state) = states.last_mut() {
                state.reverse_hardware_surfaces = config.reverse_hardware_surfaces;
                state.hardware_decoder_threads = config.hardware_decoder_threads;
                state.providers = Arc::clone(&config.providers);
                state.decoding = Arc::clone(&config.decoding);
                state.reads = Arc::clone(&config.reads);
                #[cfg(test)]
                {
                    state.heartbeat = Arc::new(WorkerHeartbeat::stalling(config.stall));
                    state.slow_jobs = slow_jobs;
                }
            }
        }
        let heartbeats = states
            .iter()
            .map(|state| Arc::clone(&state.heartbeat))
            .collect();
        let progress = Arc::new(CommandProgress::default());
        let worker_progress = Arc::clone(&progress);
        let (command_sender, command_receiver) = mpsc::sync_channel(config.depth);
        let (frame_sender, frame_receiver) = mpsc::sync_channel(config.depth);
        let (status_sender, status_receiver) = mpsc::channel();
        let budget = Arc::new(DecodeBudget {
            limit: config.byte_budget,
            state: Mutex::new(DecodeBudgetState::default()),
            available: Condvar::new(),
        });
        let waits = Arc::new(DecodePoolWaits::default());
        let worker_budget = Arc::clone(&budget);
        let worker_waits = Arc::clone(&waits);
        let worker_cancel = cancel.clone();
        let worker_failure = failure.clone();
        let join = thread::Builder::new()
            .name("alpha2-decode".to_owned())
            .spawn(move || {
                decode_pool_loop(
                    states,
                    &command_receiver,
                    &frame_sender,
                    &DecodePoolContext {
                        status: &status_sender,
                        cancel: &worker_cancel,
                        budget: &worker_budget,
                        waits: &worker_waits,
                        failure: &worker_failure,
                        progress: &worker_progress,
                    },
                );
            })
            .map_err(|error| format!("failed to start native decode worker: {error}"))?;
        Ok(Self {
            commands: Some(command_sender),
            frames: Some(frame_receiver),
            status: status_receiver,
            join: Some(join),
            cancel,
            budget,
            waits,
            failure,
            heartbeats,
            progress,
            stall_timeout: config.stall_timeout,
            stalled: AtomicBool::new(false),
        })
    }

    /// Hands one frame set to the decode pool, waiting while the queue is
    /// full. Returns whether it had to wait.
    ///
    /// The wait ends when the pool takes a command, not on a fixed interval,
    /// for the reason the encode worker's does: polling a saturated pool
    /// rounds every wait up to the interval. It is not a blocking send, which
    /// a pool stuck inside the decoder would hold forever; the wait checks
    /// for that stall as it goes. The pool drops its command receiver on
    /// every exit path, so cancellation and failure both arrive here as a
    /// channel disconnect.
    pub fn send(&self, job: DecodeJob) -> Result<bool, String> {
        let sender = self
            .commands
            .as_ref()
            .ok_or_else(|| "decode worker is closed".to_owned())?;
        self.hand_over(sender, DecodeCommand::Frame(job), "a frame")
    }

    /// Hands `command` to the pool as [`Self::send`] describes; `what` names
    /// it in the error for a pool that stopped first.
    fn hand_over(
        &self,
        sender: &SyncSender<DecodeCommand>,
        command: DecodeCommand,
        what: &str,
    ) -> Result<bool, String> {
        self.check_status()?;
        if self.cancel.is_cancelled() {
            return Err(self
                .failure
                .cause_or("decode worker was cancelled".to_owned()));
        }
        let mut pending = command;
        let mut waited = false;
        loop {
            // Read before trying, so a command the pool takes in between still ends the wait.
            let seen = self.progress.current();
            match sender.try_send(pending) {
                Ok(()) => return Ok(waited),
                Err(TrySendError::Full(command)) => pending = command,
                Err(TrySendError::Disconnected(_)) => {
                    self.check_status()?;
                    return Err(format!("decode worker stopped before accepting {what}"));
                }
            }
            waited = true;
            self.progress.wait_past(seen, STALL_POLL);
            self.check_status()?;
            self.check_stall()?;
            if self.cancel.is_cancelled() {
                return Err(self
                    .failure
                    .cause_or("decode worker was cancelled".to_owned()));
            }
        }
    }

    pub fn try_result(&self) -> Result<Option<DecodedFrameSet>, String> {
        self.check_status()?;
        let frames = self
            .frames
            .as_ref()
            .ok_or_else(|| "decode worker is closed".to_owned())?;
        match frames.try_recv() {
            Ok(frame) => Ok(Some(frame)),
            Err(TryRecvError::Empty) => self.check_stall().map(|()| None),
            Err(TryRecvError::Disconnected) => {
                self.check_status()?;
                Err("decode worker stopped without a frame".to_owned())
            }
        }
    }

    /// Waits up to `timeout` for the next decoded frame set, checking the
    /// pool's status and for a stall.
    pub fn result_timeout(&self, timeout: Duration) -> Result<Option<DecodedFrameSet>, String> {
        self.check_status()?;
        let frames = self
            .frames
            .as_ref()
            .ok_or_else(|| "decode worker is closed".to_owned())?;
        match frames.recv_timeout(timeout) {
            Ok(frame) => Ok(Some(frame)),
            Err(RecvTimeoutError::Timeout) => self.check_stall().map(|()| None),
            Err(RecvTimeoutError::Disconnected) => {
                self.check_status()?;
                Err("decode worker stopped without a frame".to_owned())
            }
        }
    }

    pub fn release(&self, bytes: u64) {
        self.budget.release(bytes);
    }

    /// Tells the pool to close its shards and joins it once it answers.
    pub fn finish(&mut self) -> Result<(), String> {
        let sender = self
            .commands
            .take()
            .ok_or_else(|| "decode worker is already closed".to_owned())?;
        // Releasing the receiver first makes a pool blocked on an unrequested frame report an error
        // instead of hanging here.
        drop(self.frames.take());
        self.hand_over(&sender, DecodeCommand::Finish, "shutdown")?;
        // Keeps its timeout deliberately: the escape hatch for a wedged pool or a shard stuck
        // closing its decoders, which would otherwise hang the export here.
        loop {
            match self.status.recv_timeout(WORKER_RETRY_INTERVAL) {
                Ok(DecodeWorkerStatus::Finished) => break,
                Ok(DecodeWorkerStatus::Failed(error)) => return Err(self.failure.cause_or(error)),
                Err(RecvTimeoutError::Timeout) if self.cancel.is_cancelled() => {
                    return Err(self
                        .failure
                        .cause_or("decode worker was cancelled".to_owned()));
                }
                Err(RecvTimeoutError::Timeout) => self.check_stall()?,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("decode worker stopped without a terminal result".to_owned());
                }
            }
        }
        if self.join.take().is_some_and(|join| join.join().is_err()) {
            return Err("decode worker panicked".to_owned());
        }
        Ok(())
    }

    /// Fails once the pool has stopped. A failure it sent is reported as the
    /// export's first failure, which may be another part's.
    pub fn check_status(&self) -> Result<(), String> {
        match self.status.try_recv() {
            Ok(DecodeWorkerStatus::Failed(error)) => Err(self.failure.cause_or(error)),
            Ok(DecodeWorkerStatus::Finished) => {
                Err("decode worker finished before scheduling closed".to_owned())
            }
            Err(TryRecvError::Empty) => Ok(()),
            Err(TryRecvError::Disconnected) => Err(self
                .failure
                .cause_or("decode worker status channel disconnected".to_owned())),
        }
    }

    /// Fails once a shard has been inside one decoder operation for longer
    /// than [`Self::stall_timeout`], and stops the pipeline: nothing it holds
    /// will come back.
    ///
    /// The message says whether the shard had opened a hardware session
    /// ([`crate::protocol::HARDWARE_DECODE_STALL_MARKER`], and the
    /// `HARDWARE_STALL` evidence its `resource` is read from), which is what
    /// a caller needs to know to retry in software: a shard with only
    /// software sessions touched no GPU, and would block the same way again.
    pub fn check_stall(&self) -> Result<(), String> {
        let Some((shard, stalled_for, hardware, activity)) = self
            .heartbeats
            .iter()
            .enumerate()
            .find_map(|(shard, beat)| {
                beat.stalled_for()
                    .filter(|stalled_for| *stalled_for > self.stall_timeout)
                    .map(|stalled_for| (shard, stalled_for, beat.holds_hardware(), beat.activity()))
            })
        else {
            return Ok(());
        };
        self.stalled.store(true, Ordering::Release);
        let reading = activity
            .as_ref()
            .map(|activity| format!(" reading {}", activity.what))
            .unwrap_or_default();
        let provider = activity.as_ref().is_some_and(|activity| activity.provider);
        let cause = if provider {
            // An extension provider is this host's own code, with no FFmpeg under it: it is still
            // working on what its file asks for, or blocked reading the file.
            "inside an extension provider, which is still drawing or opening a file that costs this \
             long within its budgets, or is blocked reading a file that stopped answering"
                .to_owned()
        } else if hardware {
            format!(
                "{}: a hardware decoder waiting for a surface that never returns does this. \
                 ALPHA2_VIDEO_DECODE=software decodes on the CPU instead",
                crate::protocol::HARDWARE_DECODE_STALL_MARKER
            )
        } else {
            "with only software sessions open: FFmpeg is blocked, most likely reading a source \
             that stopped answering"
                .to_owned()
        };
        let error = crate::failure::tag_phase(
            crate::failure::ExportPhase::Decode,
            None,
            crate::failure::hardware_stall(
                hardware && !provider,
                format!(
                    "{}decode shard {shard}{reading} has been inside one decoder operation for {} ms, past the {} ms limit, {cause}",
                    crate::protocol::decode_stalled_tag(),
                    stalled_for.as_millis(),
                    self.stall_timeout.as_millis(),
                ),
            ),
        );
        self.failure.record(&error);
        self.cancel.cancel();
        self.budget.cancel();
        Err(self.failure.cause_or(error))
    }

    pub fn peak_bytes(&self) -> u64 {
        self.budget.peak_bytes()
    }
}

impl Drop for DecodeWorker {
    fn drop(&mut self) {
        if self.join.is_none() {
            return;
        }
        self.cancel.cancel();
        self.budget.cancel();
        drop(self.commands.take());
        // The pool may be blocked handing over a decoded frame set; dropping the receiver is what
        // lets it observe the disconnect and unwind.
        drop(self.frames.take());
        let Some(join) = self.join.take() else {
            return;
        };
        // A cancelled pool finishes within one decoder operation. One that does not waits on a
        // shard stuck inside the decoder, which no cancellation reaches; that happens on any way
        // out of an export, not only after a stall was reported.
        let limit = if self.stalled.load(Ordering::Acquire) {
            Duration::ZERO
        } else {
            self.stall_timeout
        };
        crate::heartbeat::join_or_abandon(join, &self.heartbeats, limit, "decode worker");
    }
}

/// The most frames one backward run is planned over: the exporter's
/// prefetch limit. A run that goes on longer is planned again where the last
/// plan ended.
const REVERSE_RUN_FRAMES: u32 = 120;

/// Feeds the decode pool the frames ahead of the coordinator, up to `depth`
/// outstanding, and plans backward runs for cursors that read in reverse.
pub struct DecodeScheduler {
    pub generation: u64,
    pub next_index: u64,
    pub depth: usize,
    pub outstanding: usize,
    pub visual_sources: HashSet<SourceId>,
    pub last_cursor_times: HashMap<DecodeCursorKey, MediaTime>,
    /// For each cursor in a backward run, the last frame index of the run
    /// already handed to the decode pool.
    pub planned_runs: HashMap<DecodeCursorKey, u64>,
}

impl DecodeScheduler {
    pub fn new(depth: usize, project: &Project) -> Self {
        Self {
            generation: 0,
            next_index: 0,
            depth,
            outstanding: 0,
            visual_sources: project
                .sources
                .iter()
                .filter(|source| matches!(source.kind, MediaKind::Video | MediaKind::Image))
                .map(|source| source.id.clone())
                .collect(),
            last_cursor_times: HashMap::new(),
            planned_runs: HashMap::new(),
        }
    }

    /// Hands the decode pool the frames it has room for.
    ///
    /// Lookahead stays at its configured depth for backward cursors too: a
    /// shard serves a backward run from windows planned over the whole run,
    /// so decoding a later frame early costs nothing extra.
    pub fn refill(
        &mut self,
        exporter: &Exporter,
        project: &EngineProject,
        worker: &DecodeWorker,
        metrics: &mut ExportMetrics,
    ) -> Result<(), String> {
        let evaluate_started = Instant::now();
        let mut candidates = exporter
            .prefetch_needs(project, u32::try_from(self.depth).unwrap_or(u32::MAX))
            .map_err(error_message)?
            .into_iter()
            .filter(|frame| frame.index >= self.next_index)
            .collect::<Vec<_>>();
        for frame in &mut candidates {
            frame
                .needs
                .retain(|need| self.visual_sources.contains(&need.source_id));
        }
        let sending = self
            .depth
            .saturating_sub(self.outstanding)
            .min(candidates.len());
        let mut horizon = None;
        let mut runs = Vec::with_capacity(sending);
        for position in 0..sending {
            let planned = self.plan_backward_runs(&candidates, position, &mut horizon, || {
                exporter
                    .prefetch_needs(project, REVERSE_RUN_FRAMES)
                    .map_err(error_message)
            })?;
            for need in &candidates[position].needs {
                self.last_cursor_times
                    .insert(decode_cursor_key(need), need.media_time);
            }
            runs.push(planned);
        }
        metrics.evaluate = metrics.evaluate.saturating_add(evaluate_started.elapsed());
        for (frame, reverse_runs) in candidates.into_iter().zip(runs) {
            let ExportFrameNeeds { index, time, needs } = frame;
            let backpressured = worker.send(DecodeJob {
                generation: self.generation,
                index,
                time,
                needs,
                reverse_runs,
            })?;
            self.next_index = index.saturating_add(1);
            self.outstanding = self.outstanding.saturating_add(1);
            metrics.decode_backpressure = metrics
                .decode_backpressure
                .saturating_add(u64::from(backpressured));
            metrics.max_decode_queue = metrics.max_decode_queue.max(self.outstanding);
        }
        Ok(())
    }

    /// The backward runs that begin at `candidates[position]`.
    ///
    /// A cursor runs backward at a frame when its time is below the one it
    /// last asked for, or above the one it asks for next. The run is planned
    /// once, over `horizon` (evaluated on first use), and covers the frames up
    /// to where the cursor turns forward, stops, or the horizon ends. Only a
    /// run of two steps or more is planned: a single step back, such as a
    /// repeating loop's wrap seen from the pass's last frame or from the wrap
    /// itself, is an ordinary seek, and a window over it would decode the
    /// whole pass for two frames.
    pub(crate) fn plan_backward_runs(
        &mut self,
        candidates: &[ExportFrameNeeds],
        position: usize,
        horizon: &mut Option<Vec<ExportFrameNeeds>>,
        mut evaluate_horizon: impl FnMut() -> Result<Vec<ExportFrameNeeds>, String>,
    ) -> Result<HashMap<DecodeCursorKey, Vec<MediaTime>>, String> {
        let frame = &candidates[position];
        let mut runs = HashMap::new();
        for need in &frame.needs {
            let cursor = decode_cursor_key(need);
            if self
                .planned_runs
                .get(&cursor)
                .is_some_and(|until| *until >= frame.index)
            {
                continue;
            }
            let after = candidates
                .get(position + 1)
                .and_then(|next| cursor_time(next, &cursor));
            let backward = self
                .last_cursor_times
                .get(&cursor)
                .is_some_and(|previous| need.media_time < *previous)
                || after.is_some_and(|after| after < need.media_time);
            if !backward {
                continue;
            }
            if horizon.is_none() {
                *horizon = Some(evaluate_horizon()?);
            }
            let run = backward_run(horizon.as_deref().unwrap_or_default(), frame.index, &cursor);
            if run.times.len() > 2 {
                self.planned_runs.insert(cursor.clone(), run.until);
                runs.insert(cursor, run.times);
            }
        }
        Ok(runs)
    }

    /// Accounts for a frame set the host took, failing when it is not the
    /// one expected next.
    pub fn accept(&mut self, frame: &DecodedFrameSet, expected_index: u64) -> Result<(), String> {
        self.outstanding = self.outstanding.saturating_sub(1);
        if frame.generation != self.generation || frame.index != expected_index {
            return Err(format!(
                "decode worker returned generation {} frame {} while expecting generation {} frame {}",
                frame.generation, frame.index, self.generation, expected_index
            ));
        }
        Ok(())
    }
}

/// The times one cursor asks for over a backward run.
#[derive(Debug, Eq, PartialEq)]
pub struct BackwardRun {
    /// Distinct times in the order the cursor asks for them: non-increasing.
    pub times: Vec<MediaTime>,
    /// The index of the run's last frame.
    pub until: u64,
}

/// The run of `cursor` that starts at frame `start` of `frames`: its times
/// while they do not increase and every frame asks for it.
#[must_use]
pub fn backward_run(
    frames: &[ExportFrameNeeds],
    start: u64,
    cursor: &DecodeCursorKey,
) -> BackwardRun {
    let mut times = Vec::<MediaTime>::new();
    let mut until = start;
    for frame in frames.iter().skip_while(|frame| frame.index < start) {
        let Some(time) = cursor_time(frame, cursor) else {
            break;
        };
        match times.last() {
            Some(last) if time > *last => break,
            Some(last) if time == *last => {}
            _ => times.push(time),
        }
        until = frame.index;
    }
    BackwardRun { times, until }
}

fn cursor_time(frame: &ExportFrameNeeds, cursor: &DecodeCursorKey) -> Option<MediaTime> {
    frame
        .needs
        .iter()
        .find(|need| decode_cursor_key(need) == *cursor)
        .map(|need| need.media_time)
}

/// The furthest a cursor's next request may be ahead of its last and still
/// be decoded to frame by frame: two seconds, as the browser's video
/// adapters keep. Further on, a seek from the keyframe before it decodes
/// less than every frame between, a sped-up clip's every source frame.
pub const MAX_SEQUENTIAL_DECODE_GAP: MediaTime =
    MediaTime::from_ticks(2 * alpha2_time::TICKS_PER_SECOND);

/// Whether a cursor whose last request was `previous` decodes on to
/// `requested` rather than seeking: not backward, and at most
/// [`MAX_SEQUENTIAL_DECODE_GAP`] ahead. Either way the frame presented at
/// `requested` is the one answered.
#[must_use]
pub fn decodes_on(previous: MediaTime, requested: MediaTime) -> bool {
    requested >= previous
        && requested.ticks().saturating_sub(previous.ticks()) <= MAX_SEQUENTIAL_DECODE_GAP.ticks()
}

/// Who reads a source sequentially: the source, the clip, and the
/// composition step reading it (none for the clip's own source).
pub type DecodeCursorKey = (SourceId, ClipId, Option<NodeId>);

/// The cursor that reads `need`.
pub fn decode_cursor_key(need: &FrameNeed) -> DecodeCursorKey {
    (
        need.source_id.clone(),
        need.clip_id.clone(),
        need.reader.clone(),
    )
}

/// Where on the timeline each decode cursor of an export may read
/// ([`alpha2_eval::ActiveIndex::source_reads`]), which tells a shard which
/// session it can close and which one it reads last. A cursor it does not
/// know (none, when the index could not be built) is taken to be read now,
/// so it is never closed early and is evicted only by recency.
#[derive(Debug, Default)]
pub struct CursorReads {
    /// Each cursor's spans, by start.
    spans: HashMap<DecodeCursorKey, Vec<TimeRange>>,
}

impl CursorReads {
    pub fn new(reads: Vec<alpha2_eval::SourceRead>) -> Self {
        let mut spans = HashMap::<DecodeCursorKey, Vec<TimeRange>>::new();
        for read in reads {
            spans
                .entry((read.source_id, read.clip_id, read.reader))
                .or_default()
                .push(read.span);
        }
        for cursor_spans in spans.values_mut() {
            cursor_spans.sort_unstable_by_key(|span| span.start());
        }
        Self { spans }
    }

    /// Whether `cursor`'s reads are known.
    pub fn knows(&self, cursor: &DecodeCursorKey) -> bool {
        self.spans.contains_key(cursor)
    }

    /// The earliest time at or after `time` that `cursor` may read: `time`
    /// itself inside one of its spans, else the start of its next one;
    /// `None` when it reads nothing from `time` on, or is not known.
    pub fn next_read(&self, cursor: &DecodeCursorKey, time: MediaTime) -> Option<MediaTime> {
        self.spans
            .get(cursor)?
            .iter()
            .find(|span| span.end() > time)
            .map(|span| span.start().max(time))
    }

    /// Whether `cursor` is known to read nothing at `time` or after it.
    pub fn finished(&self, cursor: &DecodeCursorKey, time: MediaTime) -> bool {
        self.knows(cursor) && self.next_read(cursor, time).is_none()
    }
}

/// `error`, met decoding what `what` names, saying so. The code and
/// evidence it carries stay its own: the words of a source or clip ID
/// joining it decide nothing.
fn attributed(what: &str, error: &str) -> String {
    format!("decoding {what}: {error}")
}

/// `need` in words, for an error or a stall report: the source, who reads
/// it, and the source time.
pub fn describe_need(need: &FrameNeed) -> String {
    let reader = need
        .reader
        .as_ref()
        .map(|reader| format!(", read by {}", reader.as_str()))
        .unwrap_or_default();
    #[allow(clippy::cast_precision_loss)] // A time in words: three decimals of a second.
    let seconds = need.media_time.ticks() as f64 / alpha2_time::TICKS_PER_SECOND as f64;
    format!(
        "source {} (clip {}{reader}) at {seconds:.3} s",
        need.source_id.as_str(),
        need.clip_id.as_str(),
    )
}

/// When a cursor with an open session is read next, in the order a shard
/// evicts by: the later, the greater.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum NextRead {
    /// Later in the current frame, at this place in its order.
    ThisFrame(usize),
    /// In the next frame, at this place in its order.
    NextFrame(usize),
    /// First at this timeline time, after the next frame.
    Later(MediaTime),
    /// Not at all.
    Never,
}

/// One decode shard's sessions, reverse windows, and counters.
pub struct ExportDecodeState {
    /// Sources an extension provider draws, which no decoder opens.
    pub providers: Arc<crate::providers::ProviderMedia>,
    /// The export's decode policy, and where its decoders report.
    pub decoding: Arc<crate::recovery::ExportDecoding>,
    pub paths: HashMap<SourceId, PathBuf>,
    pub kinds: HashMap<SourceId, MediaKind>,
    /// The color each video source's project declares, which its frames carry.
    pub declared_colors: HashMap<SourceId, alpha2_media::ColorMetadata>,
    pub decoders: HashMap<DecodeCursorKey, NativeExportDecoder>,
    pub decoded: HashMap<DecodeCursorKey, ExportDecodedPosition>,
    /// Sources whose hardware session already failed once this run.
    pub software_only: HashSet<SourceId>,
    /// Workers a software session opens with, including a fallback from hardware.
    pub decoder_threads: i64,
    /// Workers a hardware session opens with; `decoder_threads` until a pool sets it.
    pub hardware_decoder_threads: i64,
    pub max_sessions: usize,
    pub session_clock: u64,
    pub session_use: HashMap<DecodeCursorKey, u64>,
    /// When each cursor reads, which decides what closes and what is evicted.
    pub reads: Arc<CursorReads>,
    /// Every video cursor this shard opened a session for this run.
    pub opened: HashSet<DecodeCursorKey>,
    /// Sessions opened again for a cursor that had one before this run: what
    /// running short of sessions cost.
    pub session_reopens: u64,
    /// The timeline time of the job being decoded.
    pub job_time: MediaTime,
    /// Where each cursor the current job reads comes in its order.
    pub job_order: HashMap<DecodeCursorKey, usize>,
    /// The place in that order of the need being decoded.
    pub job_position: usize,
    /// Times each backward cursor has still to ask for, ascending: the rest
    /// of the run its latest job planned.
    pub reverse_plans: HashMap<DecodeCursorKey, Vec<MediaTime>>,
    /// Bytes one decoded frame of each backward cursor retained, measured by
    /// its last window, which sizes the next one.
    pub reverse_frame_bytes: HashMap<DecodeCursorKey, u64>,
    /// The cursors the current job asks for: those whose plans share the
    /// reverse cache now.
    pub job_cursors: HashSet<DecodeCursorKey>,
    pub reverse_windows: HashMap<DecodeCursorKey, ReverseWindow>,
    pub reverse_cache_bytes: u64,
    pub max_reverse_cache_bytes: u64,
    /// Decoder surfaces a single reverse window may retain before the cursor
    /// falls back to software. Zero keeps every reverse cursor in software.
    pub reverse_hardware_surfaces: usize,
    pub reverse_cache_hits: u64,
    pub reverse_windows_built: u64,
    pub software_decoded_frames: u64,
    pub hardware_decoded_frames: u64,
    pub hardware_fallbacks: HardwareFallbackCounts,
    #[cfg(all(target_os = "linux", feature = "hwdecode"))]
    pub shared_vulkan: Option<SharedVulkanContext>,
    #[cfg(all(target_os = "macos", feature = "hwdecode"))]
    pub shared_metal: Option<SharedMetalContext>,
    #[cfg(all(target_os = "windows", feature = "hwdecode"))]
    pub shared_d3d12: Option<SharedD3D12Context>,
    #[cfg(all(
        any(target_os = "linux", target_os = "macos", target_os = "windows"),
        feature = "hwdecode"
    ))]
    pub hardware_required: bool,
    pub generation: u64,
    /// The output frame the job being decoded supplies.
    pub frame_index: u64,
    /// Beaten before every decoder operation; see [`WorkerHeartbeat`].
    pub heartbeat: Arc<WorkerHeartbeat>,
    /// Decodes into a reverse window after which one reports an unimportable
    /// surface. The fixtures decode cleanly on every machine that runs these
    /// tests, so the recoverable failure has to be injected to be covered.
    #[cfg(test)]
    pub reverse_window_unsupported_after: Option<usize>,
    /// How long each job takes before it decodes, as a large frame's decode
    /// does; see [`SLOW_DECODE_JOBS`].
    #[cfg(test)]
    pub slow_jobs: Option<Duration>,
}

/// What a frame of a backward cursor is expected to retain before anything
/// has measured it: 1080p at 8-bit 4:2:0.
const DEFAULT_REVERSE_FRAME_BYTES: u64 = 1920 * 1080 * 3 / 2;

/// Frames of one backward cursor decoded ahead, keyed by the time each
/// presents at.
pub struct ReverseWindow {
    pub frames: BTreeMap<MediaTime, Arc<NativeVideoFrame>>,
    pub bytes: u64,
}

/// The frame a decode cursor last presented, and the time it was asked for.
pub struct ExportDecodedPosition {
    pub requested: MediaTime,
    pub frame: Arc<NativeVideoFrame>,
}

impl ExportDecodeState {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(project: &Project, assets: &HashMap<String, PathBuf>) -> Result<Self, String> {
        Self::new_with_limits(
            project,
            assets,
            alpha2_native::codec_thread_budget(),
            usize::MAX,
            128 * 1024 * 1024,
        )
    }

    pub fn new_with_limits(
        project: &Project,
        assets: &HashMap<String, PathBuf>,
        decoder_threads: i64,
        max_sessions: usize,
        max_reverse_cache_bytes: u64,
    ) -> Result<Self, String> {
        Self::new_with_limits_and_vulkan(
            project,
            assets,
            decoder_threads,
            max_sessions,
            max_reverse_cache_bytes,
            #[cfg(all(target_os = "linux", feature = "hwdecode"))]
            None,
            #[cfg(all(target_os = "macos", feature = "hwdecode"))]
            None,
            #[cfg(all(target_os = "windows", feature = "hwdecode"))]
            None,
            #[cfg(all(
                any(target_os = "linux", target_os = "macos", target_os = "windows"),
                feature = "hwdecode"
            ))]
            false,
        )
    }

    pub fn new_with_limits_and_vulkan(
        project: &Project,
        assets: &HashMap<String, PathBuf>,
        decoder_threads: i64,
        max_sessions: usize,
        max_reverse_cache_bytes: u64,
        #[cfg(all(target_os = "linux", feature = "hwdecode"))] shared_vulkan: Option<
            SharedVulkanContext,
        >,
        #[cfg(all(target_os = "macos", feature = "hwdecode"))] shared_metal: Option<
            SharedMetalContext,
        >,
        #[cfg(all(target_os = "windows", feature = "hwdecode"))] shared_d3d12: Option<
            SharedD3D12Context,
        >,
        #[cfg(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        ))]
        hardware_required: bool,
    ) -> Result<Self, String> {
        let mut paths = HashMap::new();
        let mut kinds = HashMap::new();
        for source in project
            .used_sources()
            .into_iter()
            .filter(|source| source.is_host_supplied())
        {
            kinds.insert(source.id.clone(), source.kind);
            if source.provider.is_some() {
                continue;
            }
            let path = assets.get(source.id.as_str()).ok_or_else(|| {
                crate::protocol::missing_input(format!(
                    "missing uploaded asset {}",
                    source.id.as_str()
                ))
            })?;
            paths.insert(source.id.clone(), path.clone());
        }
        Ok(Self {
            providers: crate::providers::ProviderMedia::none(),
            decoding: Arc::default(),
            paths,
            kinds,
            declared_colors: crate::frames::declared_video_colors(project),
            decoders: HashMap::new(),
            decoded: HashMap::new(),
            software_only: HashSet::new(),
            decoder_threads,
            hardware_decoder_threads: decoder_threads,
            max_sessions: max_sessions.max(1),
            session_clock: 0,
            session_use: HashMap::new(),
            reads: Arc::default(),
            opened: HashSet::new(),
            session_reopens: 0,
            job_time: MediaTime::ZERO,
            job_order: HashMap::new(),
            job_position: 0,
            reverse_plans: HashMap::new(),
            reverse_frame_bytes: HashMap::new(),
            job_cursors: HashSet::new(),
            reverse_windows: HashMap::new(),
            reverse_cache_bytes: 0,
            max_reverse_cache_bytes,
            reverse_hardware_surfaces: 0,
            reverse_cache_hits: 0,
            reverse_windows_built: 0,
            software_decoded_frames: 0,
            hardware_decoded_frames: 0,
            hardware_fallbacks: HardwareFallbackCounts::default(),
            #[cfg(all(target_os = "linux", feature = "hwdecode"))]
            shared_vulkan,
            #[cfg(all(target_os = "macos", feature = "hwdecode"))]
            shared_metal,
            #[cfg(all(target_os = "windows", feature = "hwdecode"))]
            shared_d3d12,
            #[cfg(all(
                any(target_os = "linux", target_os = "macos", target_os = "windows"),
                feature = "hwdecode"
            ))]
            hardware_required,
            generation: 0,
            frame_index: 0,
            heartbeat: Arc::default(),
            #[cfg(test)]
            reverse_window_unsupported_after: None,
            #[cfg(test)]
            slow_jobs: None,
        })
    }

    pub fn decode_job(
        &mut self,
        job: DecodeJob,
        cancel: &CancelToken,
    ) -> Result<DecodedFrameSet, String> {
        let _work = self.heartbeat.begin_work();
        #[cfg(test)]
        if let Some(slow) = self.slow_jobs {
            let started = Instant::now();
            while started.elapsed() < slow && !cancel.is_cancelled() {
                thread::sleep(Duration::from_millis(1));
            }
        }
        let decoded = self.decode_job_frames(job, cancel);
        self.heartbeat.set_activity(None);
        decoded
    }

    /// Closes every decoder this shard holds, watched as a decoder operation
    /// (see [`WorkerHeartbeat::begin_close`]). Unwatched, a shard stuck here
    /// would hang the pool's join, and an export that had decoded every frame
    /// would never finish.
    pub fn close(self) {
        let _work = self.heartbeat.begin_close();
        drop(self);
    }

    fn decode_job_frames(
        &mut self,
        mut job: DecodeJob,
        cancel: &CancelToken,
    ) -> Result<DecodedFrameSet, String> {
        self.generation = job.generation;
        self.frame_index = job.index;
        let started = Instant::now();
        let hits_before = self.reverse_cache_hits;
        let windows_before = self.reverse_windows_built;
        let software_before = self.software_decoded_frames;
        let hardware_before = self.hardware_decoded_frames;
        let fallbacks_before = self.hardware_fallbacks;
        let reopens_before = self.session_reopens;
        self.job_time = job.time;
        self.job_cursors = job.needs.iter().map(decode_cursor_key).collect();
        for (cursor, mut times) in std::mem::take(&mut job.reverse_runs) {
            times.sort_unstable();
            times.dedup();
            self.reverse_plans.insert(cursor, times);
        }
        job.needs.sort_by(|left, right| {
            left.source_id
                .as_str()
                .cmp(right.source_id.as_str())
                .then(left.clip_id.as_str().cmp(right.clip_id.as_str()))
                .then(left.media_time.cmp(&right.media_time))
        });
        let stills = shown_stills(&job.needs, &self.kinds);
        self.job_order.clear();
        for (position, need) in job.needs.iter().enumerate() {
            self.job_order
                .entry(decode_cursor_key(need))
                .or_insert(position);
        }
        let mut frames = Vec::<DecodedSourceFrame>::with_capacity(job.needs.len());
        for (position, need) in job.needs.into_iter().enumerate() {
            self.job_position = position;
            if cancel.is_cancelled() {
                return Err("decode worker was cancelled".to_owned());
            }
            if self.kinds.get(&need.source_id) == Some(&MediaKind::Image)
                && frames.iter().any(|frame| frame.source == need.source_id)
            {
                continue;
            }
            let what = describe_need(&need);
            self.heartbeat
                .set_activity(Some(crate::heartbeat::Activity {
                    what: what.clone(),
                    provider: self.providers.supplies(&need.source_id),
                }));
            // A decoder's own message names a stage, not the file: which of an export's sources
            // failed, read by which clip at which time, is known only here.
            let decoded = self
                .decode_need(&need, cancel)
                .map_err(|error| attributed(&what, &error))?;
            if let Some(frame) = decoded {
                frames.push(frame);
            }
        }
        // Past the job, each of its cursors is read next frame, as far as anything knows.
        self.job_order.clear();
        self.close_finished_sessions(job.time);
        let bytes = unique_decoded_frame_bytes(&frames);
        // Untouched sessions report zero, so draining all of them gives the same total.
        let stage_timings = self
            .decoders
            .values_mut()
            .fold(DecodeStageTimings::default(), |total, decoder| {
                total.merged(decoder.take_stage_timings())
            });
        Ok(DecodedFrameSet {
            generation: job.generation,
            index: job.index,
            time: job.time,
            frames,
            stills,
            bytes,
            decode_time: started.elapsed(),
            stage_timings,
            reverse_cache_hits: self.reverse_cache_hits.saturating_sub(hits_before),
            reverse_windows_built: self.reverse_windows_built.saturating_sub(windows_before),
            reverse_cache_bytes: self.reverse_cache_bytes,
            software_decoded_frames: self.software_decoded_frames.saturating_sub(software_before),
            hardware_decoded_frames: self.hardware_decoded_frames.saturating_sub(hardware_before),
            hardware_fallbacks: self.hardware_fallbacks.since(fallbacks_before),
            decoder_session_reopens: self.session_reopens.saturating_sub(reopens_before),
        })
    }

    /// Opens a session for `cursor`, after making room for it, counting it
    /// when the cursor had one before.
    fn open_session(&mut self, cursor: &DecodeCursorKey, kind: MediaKind) -> Result<(), String> {
        self.evict_decoder_session();
        let decoder = self.open_decoder(&cursor.0, kind)?;
        self.insert_session(cursor, kind, decoder);
        Ok(())
    }

    /// Keeps `decoder` as `cursor`'s session, counting a reopen.
    fn insert_session(
        &mut self,
        cursor: &DecodeCursorKey,
        kind: MediaKind,
        decoder: NativeExportDecoder,
    ) {
        // A still's session closes once its picture is decoded, every time: only videos reopen.
        if kind == MediaKind::Video && !self.opened.insert(cursor.clone()) {
            self.session_reopens = self.session_reopens.saturating_add(1);
        }
        self.decoders.insert(cursor.clone(), decoder);
    }

    /// Closes `cursor`'s session: its decoder, its position, and its reverse
    /// window. A backward run keeps its plan, so a window evicted mid-run is
    /// built again from what the run has left.
    fn close_session(&mut self, cursor: &DecodeCursorKey) {
        self.decoders.remove(cursor);
        self.decoded.remove(cursor);
        self.session_use.remove(cursor);
        self.drop_reverse_window(cursor);
    }

    /// Closes the session of every cursor that reads nothing from `time` on.
    fn close_finished_sessions(&mut self, time: MediaTime) {
        let finished = self
            .decoders
            .keys()
            .filter(|cursor| self.reads.finished(cursor, time))
            .cloned()
            .collect::<Vec<_>>();
        for cursor in finished {
            self.close_session(&cursor);
            self.reverse_plans.remove(&cursor);
            self.reverse_frame_bytes.remove(&cursor);
        }
    }

    /// Opens one decode session, preferring the shared GPU decoder.
    ///
    /// Every software fallback is recorded with a typed reason so a run can
    /// report what prevented the hardware path. In `hardware-required` mode a
    /// hardware failure is an error instead of a fallback.
    fn open_decoder(
        &mut self,
        source: &SourceId,
        kind: MediaKind,
    ) -> Result<NativeExportDecoder, String> {
        let path = self
            .paths
            .get(source)
            .ok_or_else(|| {
                crate::protocol::missing_input(format!(
                    "missing source path for {}",
                    source.as_str()
                ))
            })?
            .clone();
        self.heartbeat.beat();
        #[cfg(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        ))]
        let declared = self.declared_colors.get(source).copied();
        #[cfg(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        ))]
        if kind == MediaKind::Video && !self.software_only.contains(source) {
            #[cfg(all(target_os = "linux", feature = "hwdecode"))]
            let attempt = self.shared_vulkan.as_ref().map(|context| {
                let opening = self.heartbeat.begin_hardware_open();
                let opened = NativeExportDecoder::open_vulkan(
                    &path,
                    self.hardware_decoder_threads,
                    context,
                    self.generation,
                )
                .and_then(|decoder| decoder.with_declared_color(declared));
                if opened.is_ok() {
                    opening.opened();
                }
                opened
            });
            #[cfg(all(target_os = "macos", feature = "hwdecode"))]
            let attempt = self.shared_metal.as_ref().map(|context| {
                let opening = self.heartbeat.begin_hardware_open();
                let opened = NativeExportDecoder::open_videotoolbox(
                    &path,
                    self.hardware_decoder_threads,
                    context,
                    self.generation,
                )
                .and_then(|decoder| decoder.with_declared_color(declared));
                if opened.is_ok() {
                    opening.opened();
                }
                opened
            });
            #[cfg(all(target_os = "windows", feature = "hwdecode"))]
            let attempt = self.shared_d3d12.as_ref().map(|context| {
                let opening = self.heartbeat.begin_hardware_open();
                let opened = NativeExportDecoder::open_d3d12(
                    &path,
                    self.hardware_decoder_threads,
                    context,
                    self.generation,
                )
                .and_then(|decoder| decoder.with_declared_color(declared));
                if opened.is_ok() {
                    opening.opened();
                }
                opened
            });
            match attempt {
                Some(Ok(decoder)) => {
                    return Ok(crate::recovery::export(
                        decoder,
                        source,
                        &path,
                        &self.decoding,
                    ));
                }
                Some(Err(error)) => {
                    if self.hardware_required {
                        return Err(format!("hardware decode is required: {error}"));
                    }
                    self.record_fallback(error.reason);
                    // A stream that cannot open in hardware will not for the next cursor or LRU
                    // re-open either, and each attempt pays a full hardware `avcodec_open2`.
                    self.software_only.insert(source.clone());
                }
                None => {}
            }
        }
        #[cfg(not(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        )))]
        let _ = kind;
        self.heartbeat.beat();
        Ok(crate::recovery::export(
            self.open_software(source, &path)?,
            source,
            &path,
            &self.decoding,
        ))
    }

    /// A software session for `source`, carrying the color its project declares.
    fn open_software(
        &self,
        source: &SourceId,
        path: &std::path::Path,
    ) -> Result<NativeExportDecoder, String> {
        NativeExportDecoder::open_with_threads(path, self.decoder_threads)
            .map_err(error_message)?
            .with_declared_color(self.declared_colors.get(source).copied())
            .map_err(|error| error_message(error.source))
    }

    fn record_fallback(&mut self, reason: HardwareDecodeFallback) {
        self.hardware_fallbacks.record(reason);
    }

    /// Whether this run must fail rather than decode a video frame in software.
    #[cfg_attr(
        not(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        )),
        allow(clippy::unused_self)
    )]
    const fn requires_hardware(&self) -> bool {
        #[cfg(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        ))]
        {
            self.hardware_required
        }
        #[cfg(not(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        )))]
        {
            false
        }
    }

    /// Moves one cursor's source to software after a hardware surface failed.
    ///
    /// The session is discarded rather than fed the same packet again, and the
    /// source is pinned to software so later cursors and seeks do not retry
    /// hardware for every frame.
    fn fall_back_to_software(
        &mut self,
        cursor: &DecodeCursorKey,
        reason: HardwareDecodeFallback,
    ) -> Result<(), String> {
        self.record_fallback(reason);
        self.software_only.insert(cursor.0.clone());
        self.decoders.remove(cursor);
        self.decoded.remove(cursor);
        self.drop_reverse_window(cursor);
        let path = self.paths.get(&cursor.0).ok_or_else(|| {
            crate::protocol::missing_input(format!("missing source path for {}", cursor.0.as_str()))
        })?;
        self.heartbeat.beat();
        let decoder = crate::recovery::export(
            self.open_software(&cursor.0, path)?,
            &cursor.0,
            path,
            &self.decoding,
        );
        self.decoders.insert(cursor.clone(), decoder);
        Ok(())
    }

    fn apply_tally(&mut self, tally: DecodeTally) {
        self.software_decoded_frames = self.software_decoded_frames.saturating_add(tally.software);
        self.hardware_decoded_frames = self.hardware_decoded_frames.saturating_add(tally.hardware);
    }

    #[allow(clippy::too_many_lines)]
    pub fn decode_need(
        &mut self,
        need: &FrameNeed,
        cancel: &CancelToken,
    ) -> Result<Option<DecodedSourceFrame>, String> {
        let kind = self
            .kinds
            .get(&need.source_id)
            .copied()
            .ok_or_else(|| format!("unknown source {}", need.source_id.as_str()))?;
        if kind == MediaKind::Audio {
            return Ok(None);
        }
        if self.providers.supplies(&need.source_id) {
            let frame = self.providers.frame_watched(
                &decode_cursor_key(need),
                need.media_time,
                self.frame_index,
                Some(&self.heartbeat),
            )?;
            return Ok(Some(DecodedSourceFrame {
                source: need.source_id.clone(),
                clip: need.clip_id.clone(),
                requested: need.media_time,
                kind,
                frame: Arc::new(NativeVideoFrame::Cpu(frame)),
            }));
        }
        let cursor = decode_cursor_key(need);
        if !self.decoders.contains_key(&cursor) {
            self.open_session(&cursor, kind)?;
        }
        self.session_clock = self.session_clock.saturating_add(1);
        self.session_use.insert(cursor.clone(), self.session_clock);
        let frame = if kind == MediaKind::Image {
            let decoder = self
                .decoders
                .get_mut(&cursor)
                .ok_or_else(|| "decoder disappeared".to_owned())?;
            self.heartbeat.beat();
            let Some(frame) = decoder.decode_video_next(cancel).map_err(error_message)? else {
                return Err(format!(
                    "image source {} did not decode a frame",
                    need.source_id.as_str()
                ));
            };
            self.decoders.remove(&cursor);
            self.session_use.remove(&cursor);
            // Stills are decoded on the CPU by design, so they are not in the hardware/software
            // counts and must not make a hardware-required run look like a fallback.
            Arc::new(frame)
        } else {
            self.decode_video(&decode_cursor_key(need), need.media_time, cancel)?
        };
        Ok(Some(DecodedSourceFrame {
            source: need.source_id.clone(),
            clip: need.clip_id.clone(),
            requested: need.media_time,
            kind,
            frame,
        }))
    }

    /// Decodes one cursor's frame, retrying in software after a surface the
    /// hardware path cannot import.
    ///
    /// A failed hardware surface is not re-sent to a software decoder packet by
    /// packet: the session is reopened and the request is decoded again from a
    /// keyframe, which keeps timestamps and frame selection identical to a run
    /// that never used hardware.
    pub fn decode_video(
        &mut self,
        cursor: &DecodeCursorKey,
        requested: MediaTime,
        cancel: &CancelToken,
    ) -> Result<Arc<NativeVideoFrame>, String> {
        let source = &cursor.0;
        match self.decode_video_once(cursor, requested, cancel) {
            Ok(frame) => Ok(frame),
            Err(DecodeVideoFailure::Fatal(message)) => Err(message),
            Err(DecodeVideoFailure::Hardware { reason, message }) => {
                if self.requires_hardware() {
                    return Err(format!(
                        "hardware decode is required but source {} cannot use it ({}): {message}",
                        source.as_str(),
                        reason.as_str()
                    ));
                }
                self.fall_back_to_software(cursor, reason)?;
                self.decode_video_once(cursor, requested, cancel)
                    .map_err(DecodeVideoFailure::into_message)
            }
        }
    }

    fn decode_video_once(
        &mut self,
        cursor: &DecodeCursorKey,
        requested: MediaTime,
        cancel: &CancelToken,
    ) -> Result<Arc<NativeVideoFrame>, DecodeVideoFailure> {
        let source = &cursor.0;
        if let Some(frame) = self.reverse_window_frame(cursor, requested) {
            return Ok(frame);
        }
        if self.plans_next(cursor, requested) {
            self.build_reverse_window(cursor, requested, cancel)?;
            if let Some(frame) = self.reverse_window_frame(cursor, requested) {
                return Ok(frame);
            }
        }
        let decoder = self
            .decoders
            .get_mut(cursor)
            .ok_or_else(|| DecodeVideoFailure::Fatal("decoder disappeared".to_owned()))?;
        let mut tally = DecodeTally::default();
        let previous = self.decoded.remove(cursor);
        let mut position =
            if let Some(position) = previous.filter(|item| decodes_on(item.requested, requested)) {
                position
            } else {
                self.heartbeat.beat();
                let frame = decoder
                    .seek_video(
                        DecodeRequest {
                            source: source.clone(),
                            time: requested,
                            kind: DecodeRequestKind::Seek,
                            generation: 0,
                        },
                        cancel,
                    )
                    .map_err(|error| DecodeVideoFailure::classify(&error, decoder))?;
                tally.count(&frame);
                ExportDecodedPosition {
                    requested,
                    frame: Arc::new(frame),
                }
            };
        // The frame presented at `requested` is the last one at or before its deadline; the next
        // stays in the decoder.
        let deadline = decoder.presentation_deadline(requested);
        while {
            self.heartbeat.beat();
            decoder
                .peek_video_pts(cancel)
                .map_err(|error| DecodeVideoFailure::classify(&error, decoder))?
                .is_some_and(|pts| pts <= deadline)
        } {
            self.heartbeat.beat();
            let frame = decoder
                .decode_video_next(cancel)
                .map_err(|error| DecodeVideoFailure::classify(&error, decoder))?
                .ok_or_else(|| {
                    DecodeVideoFailure::Fatal("a peeked frame disappeared".to_owned())
                })?;
            tally.count(&frame);
            position.frame = Arc::new(frame);
        }
        position.requested = requested;
        let frame = Arc::clone(&position.frame);
        self.decoded.insert(cursor.clone(), position);
        self.apply_tally(tally);
        Ok(frame)
    }

    /// Closes one session when the shard holds all it may: the one read
    /// next the latest, as the timeline says (Belady's rule). Reading every
    /// cursor of a frame in one order, closing the least recently used one
    /// instead would close the very next one each time, and reopen every
    /// session every frame.
    fn evict_decoder_session(&mut self) {
        if self.decoders.len() < self.max_sessions {
            return;
        }
        let Some(cursor) = self
            .decoders
            .keys()
            .max_by_key(|cursor| {
                (
                    self.next_read_rank(cursor),
                    std::cmp::Reverse(self.session_use.get(*cursor).copied().unwrap_or(0)),
                    std::cmp::Reverse(*cursor),
                )
            })
            .cloned()
        else {
            return;
        };
        self.close_session(&cursor);
    }

    /// How late `cursor` is read next, from the current need on; the greater,
    /// the later. Within this frame a cursor is read in the job's order, so
    /// one still to come this frame is read before one already read, which is
    /// read next frame; one this frame does not read is read when the
    /// timeline says, and one read no more is the latest of all.
    fn next_read_rank(&self, cursor: &DecodeCursorKey) -> NextRead {
        if let Some(&position) = self.job_order.get(cursor) {
            return if position >= self.job_position {
                NextRead::ThisFrame(position)
            } else {
                NextRead::NextFrame(position)
            };
        }
        if !self.reads.knows(cursor) {
            // Not known: as soon as can be after this frame.
            return NextRead::NextFrame(usize::MAX);
        }
        match self.reads.next_read(cursor, self.job_time) {
            None => NextRead::Never,
            Some(time) if time > self.job_time => NextRead::Later(time),
            // Read now, by the spans, though not by this frame: a frame another reader of the
            // same source frame asked for.
            Some(_) => NextRead::NextFrame(usize::MAX),
        }
    }

    /// The frame `cursor`'s reverse window holds for `requested`, counted as
    /// a hit.
    ///
    /// A request above everything the window holds means the cursor has
    /// turned forward, so its run is over and the window and plan are
    /// dropped.
    fn reverse_window_frame(
        &mut self,
        cursor: &DecodeCursorKey,
        requested: MediaTime,
    ) -> Option<Arc<NativeVideoFrame>> {
        let window = self.reverse_windows.get(cursor)?;
        if let Some(frame) = window.frames.get(&requested) {
            let frame = Arc::clone(frame);
            self.reverse_cache_hits = self.reverse_cache_hits.saturating_add(1);
            return Some(frame);
        }
        if window
            .frames
            .last_key_value()
            .is_some_and(|(top, _)| requested > *top)
        {
            self.drop_reverse_window(cursor);
            self.reverse_plans.remove(cursor);
        }
        None
    }

    /// Whether `requested` is the next time `cursor`'s backward run plans,
    /// after dropping the times the run has passed.
    fn plans_next(&mut self, cursor: &DecodeCursorKey, requested: MediaTime) -> bool {
        let Some(plan) = self.reverse_plans.get_mut(cursor) else {
            return false;
        };
        plan.truncate(plan.partition_point(|time| *time <= requested));
        if plan.is_empty() {
            self.reverse_plans.remove(cursor);
            return false;
        }
        plan.last() == Some(&requested)
    }

    fn drop_reverse_window(&mut self, cursor: &DecodeCursorKey) {
        if let Some(window) = self.reverse_windows.remove(cursor) {
            self.reverse_cache_bytes = self.reverse_cache_bytes.saturating_sub(window.bytes);
        }
    }

    /// Bytes one decoded frame of `cursor` is expected to retain: what its
    /// last window measured, else its current frame, else its stream's size
    /// as 8-bit 4:2:0.
    fn expected_frame_bytes(&self, cursor: &DecodeCursorKey) -> u64 {
        self.reverse_frame_bytes
            .get(cursor)
            .copied()
            .or_else(|| {
                self.decoded
                    .get(cursor)
                    .map(|position| position.frame.retained_bytes())
            })
            .or_else(|| {
                self.decoders.get(cursor).and_then(|decoder| {
                    decoder
                        .streams()
                        .iter()
                        .find_map(|stream| stream.dimensions)
                        .map(|(width, height)| {
                            (u64::from(width) * u64::from(height) * 3).div_ceil(2)
                        })
                })
            })
            .filter(|bytes| *bytes > 0)
            .unwrap_or(DEFAULT_REVERSE_FRAME_BYTES)
    }

    /// Decodes the next window of `cursor`'s backward run, which asks for
    /// `requested` now and for lower planned times after it.
    ///
    /// The window holds the highest planned times at or below `requested`
    /// that fit this cursor's share of the reverse cache. It is decoded
    /// forward once, from the keyframe before its lowest time, keeping the
    /// frame presented at each time, and replaces the cursor's previous
    /// window, whose times the run has passed. A run longer than one window
    /// decodes each keyframe interval about `interval / window` times rather
    /// than once per frame.
    ///
    /// A hardware session keeps decoding only while the window fits the
    /// configured decoder surface cap, as one window holds a surface per
    /// frame; a run that requires hardware is windowed to that cap.
    #[allow(clippy::too_many_lines)]
    fn build_reverse_window(
        &mut self,
        cursor: &DecodeCursorKey,
        requested: MediaTime,
        cancel: &CancelToken,
    ) -> Result<(), DecodeVideoFailure> {
        let source = &cursor.0;
        if self.kinds.get(source) != Some(&MediaKind::Video) {
            return Ok(());
        }
        // Every cursor this frame runs backward gets an equal share; a plan whose cursor this frame
        // does not ask for is over, or waiting, and takes none.
        let backward_cursors = self
            .reverse_plans
            .keys()
            .filter(|planned| self.job_cursors.contains(*planned))
            .count()
            .max(1);
        let backward_cursors = u64::try_from(backward_cursors).unwrap_or(u64::MAX);
        let share = (self.max_reverse_cache_bytes / backward_cursors).max(1);
        let mut capacity = usize::try_from(share / self.expected_frame_bytes(cursor).max(1))
            .unwrap_or(usize::MAX)
            .max(1);
        if self.requires_hardware() {
            capacity = capacity.min(self.reverse_hardware_surfaces.max(1));
        }
        let ordered = self
            .reverse_plans
            .get(cursor)
            .map(|plan| plan[plan.len().saturating_sub(capacity)..].to_vec())
            .unwrap_or_default();
        if ordered.last() != Some(&requested) {
            return Ok(());
        }
        let hardware_window = ordered.len() <= self.reverse_hardware_surfaces;
        if !hardware_window && self.requires_hardware() {
            return Err(DecodeVideoFailure::Fatal(format!(
                "hardware decode is required but source {} runs backward and ALPHA2_REVERSE_DECODE_SURFACES allows no reverse window",
                source.as_str()
            )));
        }
        if !hardware_window
            && self
                .decoders
                .get(cursor)
                .is_some_and(NativeExportDecoder::is_hardware)
        {
            self.decoders.remove(cursor);
            self.decoded.remove(cursor);
            self.record_fallback(HardwareDecodeFallback::ReverseWindow);
        }
        if !self.decoders.contains_key(cursor) {
            self.evict_decoder_session();
            let decoder = if hardware_window {
                self.open_decoder(source, MediaKind::Video)
                    .map_err(DecodeVideoFailure::Fatal)?
            } else {
                let path = self.paths.get(source).ok_or_else(|| {
                    DecodeVideoFailure::Fatal(crate::protocol::missing_input(format!(
                        "missing source path for {}",
                        source.as_str()
                    )))
                })?;
                self.heartbeat.beat();
                crate::recovery::export(
                    self.open_software(source, path)
                        .map_err(DecodeVideoFailure::Fatal)?,
                    source,
                    path,
                    &self.decoding,
                )
            };
            self.insert_session(cursor, MediaKind::Video, decoder);
        }
        self.drop_reverse_window(cursor);
        let decoder = self
            .decoders
            .get_mut(cursor)
            .ok_or_else(|| DecodeVideoFailure::Fatal("decoder disappeared".to_owned()))?;
        self.heartbeat.beat();
        let first = decoder
            .seek_video(
                DecodeRequest {
                    source: source.clone(),
                    time: ordered[0],
                    kind: DecodeRequestKind::Seek,
                    generation: 0,
                },
                cancel,
            )
            .map_err(|error| DecodeVideoFailure::classify(&error, decoder))?;
        let mut tally = DecodeTally::default();
        tally.count(&first);
        let mut current = Arc::new(first);
        let mut frames = BTreeMap::<MediaTime, Arc<NativeVideoFrame>>::new();
        let mut bytes = 0_u64;
        for target in ordered {
            // As on the forward path, each target keeps the last frame at or before its deadline.
            let deadline = decoder.presentation_deadline(target);
            while {
                self.heartbeat.beat();
                decoder
                    .peek_video_pts(cancel)
                    .map_err(|error| DecodeVideoFailure::classify(&error, decoder))?
                    .is_some_and(|pts| pts <= deadline)
            } {
                #[cfg(test)]
                if let Some(remaining) = self.reverse_window_unsupported_after.as_mut() {
                    if *remaining == 0 {
                        // Consumed here so the software retry must succeed: a fallback, not a loop.
                        self.reverse_window_unsupported_after = None;
                        return Err(DecodeVideoFailure::Hardware {
                            reason: HardwareDecodeFallback::UnsupportedSurfaceFormat,
                            message: "injected surface format".to_owned(),
                        });
                    }
                    *remaining -= 1;
                }
                self.heartbeat.beat();
                let frame = decoder
                    .decode_video_next(cancel)
                    .map_err(|error| DecodeVideoFailure::classify(&error, decoder))?
                    .ok_or_else(|| {
                        DecodeVideoFailure::Fatal("a peeked frame disappeared".to_owned())
                    })?;
                tally.count(&frame);
                current = Arc::new(frame);
            }
            if !frames
                .last_key_value()
                .is_some_and(|(_, last)| Arc::ptr_eq(last, &current))
            {
                bytes = bytes.saturating_add(current.retained_bytes());
            }
            frames.insert(target, Arc::clone(&current));
            // The lowest times go first, as the run reaches them last.
            while bytes > share && frames.len() > 1 {
                if let Some((_, evicted)) = frames.pop_first()
                    && !frames
                        .first_key_value()
                        .is_some_and(|(_, first)| Arc::ptr_eq(first, &evicted))
                {
                    bytes = bytes.saturating_sub(evicted.retained_bytes());
                }
            }
        }
        self.apply_tally(tally);
        let distinct = frames
            .values()
            .zip(frames.values().skip(1))
            .filter(|(low, high)| !Arc::ptr_eq(low, high))
            .count()
            .saturating_add(1);
        self.reverse_frame_bytes.insert(
            cursor.clone(),
            bytes / u64::try_from(distinct).unwrap_or(1).max(1),
        );
        self.reverse_windows
            .insert(cursor.clone(), ReverseWindow { frames, bytes });
        self.reverse_cache_bytes = self.reverse_cache_bytes.saturating_add(bytes);
        self.reverse_windows_built = self.reverse_windows_built.saturating_add(1);
        while self.reverse_cache_bytes > self.max_reverse_cache_bytes
            && self.reverse_windows.len() > 1
        {
            let Some(victim) = self
                .reverse_windows
                .keys()
                .find(|key| *key != cursor)
                .cloned()
            else {
                break;
            };
            self.drop_reverse_window(&victim);
        }
        self.decoded.insert(
            cursor.clone(),
            ExportDecodedPosition {
                requested,
                frame: current,
            },
        );
        Ok(())
    }
}

enum DecodeShardCommand {
    Frame(DecodeJob),
    Finish,
}

struct DecodeShardResult {
    shard: usize,
    result: Result<DecodedFrameSet, String>,
}

/// The state every decode-pool exit path needs to unwind cleanly.
pub struct DecodePoolContext<'a> {
    pub status: &'a mpsc::Sender<DecodeWorkerStatus>,
    pub cancel: &'a CancelToken,
    pub budget: &'a DecodeBudget,
    pub waits: &'a DecodePoolWaits,
    pub failure: &'a PipelineFailure,
    pub progress: &'a CommandProgress,
}

/// The shard threads of one decode pool, joined on every way out of it.
///
/// Join shards before exit: their decoders retain the shared GPU context,
/// whose destruction must not race the driver's exit-time teardown.
struct DecodeShards {
    senders: Vec<SyncSender<DecodeShardCommand>>,
    /// Each ends with why closing its decoders failed, if it did.
    joins: Vec<thread::JoinHandle<Result<(), String>>>,
}

impl DecodeShards {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            senders: Vec::with_capacity(capacity),
            joins: Vec::with_capacity(capacity),
        }
    }

    /// Closes every shard's command channel and waits for all of them.
    ///
    /// # Errors
    /// The first shard's failure to close its decoders.
    fn join(&mut self) -> Result<(), String> {
        self.senders.clear();
        let mut result = Ok(());
        for join in self.joins.drain(..) {
            let closed = join
                .join()
                .unwrap_or_else(|_| Err("decode shard panicked".to_owned()));
            result = result.and(closed);
        }
        result
    }
}

impl Drop for DecodeShards {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

/// Runs the decode pool: spreads each frame's needs over the shards, gathers
/// their results into one [`DecodedFrameSet`], charges it to the budget, and
/// hands it to the host. Any failure is recorded, cancels the export, and
/// ends the loop.
#[allow(clippy::too_many_lines)]
pub fn decode_pool_loop(
    decode_states: Vec<ExportDecodeState>,
    commands: &Receiver<DecodeCommand>,
    frames: &SyncSender<DecodedFrameSet>,
    context: &DecodePoolContext<'_>,
) {
    let &DecodePoolContext {
        status: status_sender,
        cancel,
        budget,
        waits,
        failure,
        progress,
    } = context;
    let kinds = decode_states
        .first()
        .map_or_else(HashMap::new, |state| state.kinds.clone());
    let mut placement = ShardPlacement::new(
        decode_states
            .iter()
            .map(|state| state.max_sessions)
            .collect(),
    );
    let (result_sender, result_receiver) = mpsc::channel::<DecodeShardResult>();
    let mut shards = DecodeShards::with_capacity(decode_states.len());
    for (shard, mut state) in decode_states.into_iter().enumerate() {
        let (sender, receiver) = mpsc::sync_channel::<DecodeShardCommand>(1);
        let results = result_sender.clone();
        let shard_cancel = cancel.clone();
        let join = match thread::Builder::new()
            .name(format!("alpha2-decode-{shard}"))
            .spawn(move || {
                let heartbeat = Arc::clone(&state.heartbeat);
                let _watch = alpha2_native::watch_decode_progress(move || heartbeat.progressed());
                let heartbeat = Arc::clone(&state.heartbeat);
                let _provider_watch =
                    alpha2_extensions::watch_progress(move || heartbeat.progressed());
                // A panic is this shard's result like any error, so the waiting pool fails the
                // export at once.
                while let Ok(command) = receiver.recv() {
                    match command {
                        DecodeShardCommand::Frame(job) => {
                            let (result, panicked) = match catch_unwind(AssertUnwindSafe(|| {
                                state.decode_job(job, &shard_cancel)
                            })) {
                                Ok(result) => (result, false),
                                Err(payload) => (
                                    Err(shard_panicked(shard, "decoding a frame", &*payload)),
                                    true,
                                ),
                            };
                            if results.send(DecodeShardResult { shard, result }).is_err()
                                || panicked
                            {
                                break;
                            }
                        }
                        DecodeShardCommand::Finish => break,
                    }
                }
                catch_unwind(AssertUnwindSafe(|| state.close()))
                    .map_err(|payload| shard_panicked(shard, "closing its decoders", &*payload))
            }) {
            Ok(join) => join,
            Err(error) => {
                let message = crate::failure::tag_phase(
                    crate::failure::ExportPhase::Decode,
                    None,
                    format!("failed to start native decode shard: {error}"),
                );
                failure.record_unless_cancelled(cancel, &message);
                let _ = status_sender.send(DecodeWorkerStatus::Failed(message));
                cancel.cancel();
                budget.cancel();
                return;
            }
        };
        shards.senders.push(sender);
        shards.joins.push(join);
    }
    drop(result_sender);
    let shard_senders = &shards.senders;
    // The stills the previous frame showed, which the host still holds.
    let mut previous_stills = None::<(u64, Vec<SourceId>)>;
    // Per shard, since a shard a frame asks nothing of still holds its windows.
    let mut reverse_held = vec![0_u64; shard_senders.len()];
    while let Ok(command) = commands.recv() {
        progress.took_one();
        match command {
            DecodeCommand::Frame(job) => {
                let started = Instant::now();
                let shard_count = shard_senders.len();
                let mut needs = (0..shard_count).map(|_| Vec::new()).collect::<Vec<_>>();
                let held = previous_stills
                    .as_ref()
                    .filter(|(generation, _)| *generation == job.generation)
                    .map(|(_, stills)| stills.as_slice());
                let (stills, job_needs) = still_needs_for_job(job.needs, &kinds, held);
                previous_stills = Some((job.generation, stills.clone()));
                let runs = job.reverse_runs.into_iter().collect::<Vec<_>>();
                let run_cursors = runs.iter().map(|(cursor, _)| cursor).collect::<Vec<_>>();
                let (need_shards, run_shards) = placement.place(&job_needs, &run_cursors, &kinds);
                for (need, shard) in job_needs.into_iter().zip(need_shards) {
                    needs[shard].push(need);
                }
                let mut reverse = (0..shard_count).map(|_| HashMap::new()).collect::<Vec<_>>();
                for ((cursor, run), shard) in runs.into_iter().zip(run_shards) {
                    reverse[shard].insert(cursor, run);
                }
                let mut active = Vec::new();
                for shard in 0..shard_count {
                    if needs[shard].is_empty() && reverse[shard].is_empty() {
                        continue;
                    }
                    let shard_job = DecodeJob {
                        generation: job.generation,
                        index: job.index,
                        time: job.time,
                        needs: std::mem::take(&mut needs[shard]),
                        reverse_runs: std::mem::take(&mut reverse[shard]),
                    };
                    if shard_senders[shard]
                        .send(DecodeShardCommand::Frame(shard_job))
                        .is_err()
                    {
                        fail_decode_pool(
                            "decode shard stopped before accepting a frame".to_owned(),
                            Some(job.index),
                            status_sender,
                            cancel,
                            budget,
                            failure,
                        );
                        return;
                    }
                    active.push(shard);
                }
                let mut decoded_frames = Vec::new();
                let mut reverse_cache_hits = 0_u64;
                let mut reverse_windows_built = 0_u64;
                let mut software_decoded_frames = 0_u64;
                let mut hardware_decoded_frames = 0_u64;
                let mut hardware_fallbacks = HardwareFallbackCounts::default();
                let mut decoder_session_reopens = 0_u64;
                let mut stage_timings = DecodeStageTimings::default();
                for _ in 0..active.len() {
                    let Ok(shard_result) = result_receiver.recv() else {
                        fail_decode_pool(
                            "decode shard stopped without a result".to_owned(),
                            Some(job.index),
                            status_sender,
                            cancel,
                            budget,
                            failure,
                        );
                        return;
                    };
                    if !active.contains(&shard_result.shard) {
                        fail_decode_pool(
                            "decode shard returned an unexpected result".to_owned(),
                            Some(job.index),
                            status_sender,
                            cancel,
                            budget,
                            failure,
                        );
                        return;
                    }
                    match shard_result.result {
                        Ok(mut decoded) => {
                            reverse_cache_hits =
                                reverse_cache_hits.saturating_add(decoded.reverse_cache_hits);
                            reverse_windows_built =
                                reverse_windows_built.saturating_add(decoded.reverse_windows_built);
                            reverse_held[shard_result.shard] = decoded.reverse_cache_bytes;
                            software_decoded_frames = software_decoded_frames
                                .saturating_add(decoded.software_decoded_frames);
                            hardware_decoded_frames = hardware_decoded_frames
                                .saturating_add(decoded.hardware_decoded_frames);
                            hardware_fallbacks =
                                hardware_fallbacks.merged(decoded.hardware_fallbacks);
                            decoder_session_reopens = decoder_session_reopens
                                .saturating_add(decoded.decoder_session_reopens);
                            stage_timings = stage_timings.merged(decoded.stage_timings);
                            decoded_frames.append(&mut decoded.frames);
                        }
                        Err(error) => {
                            fail_decode_pool(
                                error,
                                Some(job.index),
                                status_sender,
                                cancel,
                                budget,
                                failure,
                            );
                            return;
                        }
                    }
                }
                let reverse_cache_bytes = reverse_held
                    .iter()
                    .fold(0_u64, |total, held| total.saturating_add(*held));
                decoded_frames.sort_by(|left, right| {
                    left.source
                        .as_str()
                        .cmp(right.source.as_str())
                        .then(left.clip.as_str().cmp(right.clip.as_str()))
                        .then(left.requested.cmp(&right.requested))
                });
                let decoded = DecodedFrameSet {
                    generation: job.generation,
                    index: job.index,
                    time: job.time,
                    bytes: unique_decoded_frame_bytes(&decoded_frames),
                    frames: decoded_frames,
                    stills,
                    decode_time: started.elapsed(),
                    stage_timings,
                    reverse_cache_hits,
                    reverse_windows_built,
                    reverse_cache_bytes,
                    software_decoded_frames,
                    hardware_decoded_frames,
                    hardware_fallbacks,
                    decoder_session_reopens,
                };
                let budget_started = Instant::now();
                let reserved = budget.reserve(decoded.bytes, cancel);
                waits.record_budget(budget_started.elapsed());
                if let Err(error) = reserved {
                    fail_decode_pool(
                        error,
                        Some(job.index),
                        status_sender,
                        cancel,
                        budget,
                        failure,
                    );
                    return;
                }
                let handoff_started = Instant::now();
                let handed_over = send_decoded_frame(frames, decoded, budget);
                waits.record_handoff(handoff_started.elapsed());
                if !handed_over {
                    return;
                }
            }
            DecodeCommand::Finish => {
                for sender in shard_senders {
                    let _ = sender.send(DecodeShardCommand::Finish);
                }
                if let Err(error) = shards.join() {
                    fail_decode_pool(error, None, status_sender, cancel, budget, failure);
                    return;
                }
                let _ = status_sender.send(DecodeWorkerStatus::Finished);
                return;
            }
        }
    }
}

/// What a shard that panicked reports: a defect, not the media's or the
/// device's fault, with the panic's own message.
fn shard_panicked(shard: usize, doing: &str, payload: &(dyn std::any::Any + Send)) -> String {
    format!(
        "{}decode shard {shard} panicked {doing}: {}",
        crate::protocol::internal_tag(),
        crate::failure::panic_message(payload)
    )
}

/// Fails the export with `error`, which `frame`'s decode met when one did.
fn fail_decode_pool(
    error: String,
    frame: Option<u64>,
    status: &mpsc::Sender<DecodeWorkerStatus>,
    cancel: &CancelToken,
    budget: &DecodeBudget,
    failure: &PipelineFailure,
) {
    let error = crate::failure::tag_phase(crate::failure::ExportPhase::Decode, frame, error);
    // Recorded before the cancel, so a shard's own "cancelled" is not reported as the cause.
    failure.record_unless_cancelled(cancel, &error);
    let _ = status.send(DecodeWorkerStatus::Failed(error));
    cancel.cancel();
    budget.cancel();
}

/// The stills `needs` shows: each image source once, sorted.
fn shown_stills(needs: &[FrameNeed], kinds: &HashMap<SourceId, MediaKind>) -> Vec<SourceId> {
    let mut stills = needs
        .iter()
        .filter(|need| kinds.get(&need.source_id) == Some(&MediaKind::Image))
        .map(|need| need.source_id.clone())
        .collect::<Vec<_>>();
    stills.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
    stills.dedup();
    stills
}

/// Splits one frame's needs into the stills it shows and the needs to
/// decode.
///
/// A still is decoded once each time it comes into the picture: a still in
/// `held`, the stills the previous frame of the same generation showed, is
/// left out, because the host keeps every still of the frame it last took
/// and only those. A still shown by several clips is asked for once. The
/// host's half of the rule is [`crate::frames::NativeGpuFrames::install`]
/// and [`crate::frames::NativeGpuFrames::pass`]; they hold memory for the
/// stills of one frame, where keeping every still an export ever showed would
/// hold one output-sized texture per still until the export ends.
pub(crate) fn still_needs_for_job(
    needs: Vec<FrameNeed>,
    kinds: &HashMap<SourceId, MediaKind>,
    held: Option<&[SourceId]>,
) -> (Vec<SourceId>, Vec<FrameNeed>) {
    let stills = shown_stills(&needs, kinds);
    let mut asked = HashSet::new();
    let needs = needs
        .into_iter()
        .filter(|need| {
            kinds.get(&need.source_id) != Some(&MediaKind::Image)
                || (!held.is_some_and(|held| held.contains(&need.source_id))
                    && asked.insert(need.source_id.clone()))
        })
        .collect();
    (stills, needs)
}

/// Which decode shard reads each cursor.
///
/// A cursor's decoder lives in one shard, so a cursor stays where it was
/// first placed. It is placed, the first frame that reads it, in the shard
/// with the fewest cursors that frame reads, preferring one with a session
/// to spare: cursors that read together decode in parallel, and no shard
/// holds more than its share of sessions while another has room. A clip's
/// cursor reads over one stretch of the timeline, so cursors that read
/// together are placed together, and the shares hold for as long as they
/// read. Placing by a hash of the cursor would put two clips on one shard
/// half the time, leaving the other idle, and could put more cursors on one
/// shard than its sessions, evicting the least recently used one every
/// frame. A still holds no session after it is decoded, so it goes
/// to the shard with the least to do in its frame.
pub(crate) struct ShardPlacement {
    /// The decoder sessions each shard may hold.
    shares: Vec<usize>,
    placed: HashMap<DecodeCursorKey, usize>,
    /// Cursors ever placed on each shard, to spread ties.
    placed_on: Vec<usize>,
}

impl ShardPlacement {
    pub(crate) fn new(shares: Vec<usize>) -> Self {
        let count = shares.len().max(1);
        Self {
            shares,
            placed: HashMap::new(),
            placed_on: vec![0; count],
        }
    }

    /// Places one frame's `needs` and the cursors of its backward `runs`,
    /// returning each need's shard and each run's, in order.
    pub(crate) fn place(
        &mut self,
        needs: &[FrameNeed],
        runs: &[&DecodeCursorKey],
        kinds: &HashMap<SourceId, MediaKind>,
    ) -> (Vec<usize>, Vec<usize>) {
        let is_still = |need: &FrameNeed| kinds.get(&need.source_id) == Some(&MediaKind::Image);
        let mut cursors = needs
            .iter()
            .filter(|need| !is_still(need))
            .map(decode_cursor_key)
            .chain(runs.iter().map(|&cursor| cursor.clone()))
            .collect::<Vec<_>>();
        cursors.sort_unstable();
        cursors.dedup();
        let mut load = vec![0_usize; self.placed_on.len()];
        for cursor in &cursors {
            if let Some(&shard) = self.placed.get(cursor) {
                load[shard] += 1;
            }
        }
        for cursor in cursors {
            if self.placed.contains_key(&cursor) {
                continue;
            }
            let shard = self.least_loaded(&load);
            load[shard] += 1;
            self.placed_on[shard] += 1;
            self.placed.insert(cursor, shard);
        }
        let mut stills = HashMap::<&SourceId, usize>::new();
        let need_shards = needs
            .iter()
            .map(|need| {
                if is_still(need) {
                    *stills.entry(&need.source_id).or_insert_with(|| {
                        let shard = self.least_loaded(&load);
                        load[shard] += 1;
                        shard
                    })
                } else {
                    self.placed
                        .get(&decode_cursor_key(need))
                        .copied()
                        .unwrap_or(0)
                }
            })
            .collect();
        let run_shards = runs
            .iter()
            .map(|cursor| self.placed.get(*cursor).copied().unwrap_or(0))
            .collect();
        (need_shards, run_shards)
    }

    /// The shard to give one more cursor in a frame whose shards read `load`:
    /// one with a session to spare, then the fewest read, then the fewest
    /// ever placed, then the first.
    fn least_loaded(&self, load: &[usize]) -> usize {
        (0..load.len())
            .min_by_key(|&shard| {
                let full = load[shard] >= self.shares.get(shard).copied().unwrap_or(1);
                (full, load[shard], self.placed_on[shard], shard)
            })
            .unwrap_or(0)
    }

    /// The shard `cursor` was placed on.
    #[cfg(test)]
    pub(crate) fn shard_of(&self, cursor: &DecodeCursorKey) -> Option<usize> {
        self.placed.get(cursor).copied()
    }
}

/// Hands one decoded frame set to the host, blocking while the queue is full.
///
/// Cancellation arrives as a channel disconnect: both the cancel path and the
/// finish path drop the receiver before joining the pool, so there is nothing
/// to poll for. Returns whether the host took ownership of the set's bytes.
pub fn send_decoded_frame(
    sender: &SyncSender<DecodedFrameSet>,
    frame: DecodedFrameSet,
    budget: &DecodeBudget,
) -> bool {
    match sender.send(frame) {
        Ok(()) => true,
        Err(mpsc::SendError(pending)) => {
            budget.release(pending.bytes);
            false
        }
    }
}

/// Bytes `frames` retain, counting a picture shared by several entries once.
pub fn unique_decoded_frame_bytes(frames: &[DecodedSourceFrame]) -> u64 {
    let mut unique = Vec::<&Arc<NativeVideoFrame>>::with_capacity(frames.len());
    frames.iter().fold(0_u64, |bytes, frame| {
        if unique
            .iter()
            .any(|backing| Arc::ptr_eq(backing, &frame.frame))
        {
            bytes
        } else {
            unique.push(&frame.frame);
            bytes.saturating_add(frame.frame.retained_bytes())
        }
    })
}
