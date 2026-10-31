//! Encoder sink and the background encode worker.

#[cfg(feature = "hwencode")]
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::audio::audio_block_bytes;
#[cfg(test)]
use crate::heartbeat::InjectedStall;
use crate::heartbeat::{CommandProgress, STALL_POLL, WorkerExit, WorkerHeartbeat};
use crate::metrics::{ExportMetrics, frames_per_second};
use crate::protocol::HostError;
use alpha2_media::{CancelToken, EncodeEvent, EncodeSink};
use alpha2_native::{
    EncodeStageTimings, NativeAvEncodeSink, NativeVideoEncodeSink, NativeVideoEncoderSelection,
    Yuv420pFrameBuffer,
};
use alpha2_time::MediaTime;

/// An encoder or muxer failure during `step`, as the text that crosses the
/// worker boundary: with the code its type decides ([`crate::error_message`]),
/// so a typed GPU fault (a timed-out wait, a lost device) keeps its protocol
/// code, and `ENCODE_ERROR` otherwise.
pub(crate) fn encode_failure(step: &str, error: impl std::error::Error + 'static) -> String {
    encode_error(format!(
        "native encode failed while {step}: {}",
        crate::error_message(error)
    ))
}

/// `message`, a failure of the encode path: `ENCODE_ERROR` unless what it
/// quotes carries a code of its own.
pub(crate) fn encode_error(message: impl Into<String>) -> String {
    crate::protocol::code_tag("ENCODE_ERROR", message)
}

/// One encoder frame on loan from [`FramePool`].
///
/// Reuse one frame per pipeline slot to avoid large allocations and page faults.
pub struct PooledFrame {
    /// Present for the whole loan; taken back out when it ends.
    frame: Option<Yuv420pFrameBuffer>,
    pool: Arc<FramePool>,
}

impl PooledFrame {
    /// The encoder frame to pack into and hand over.
    ///
    /// # Panics
    /// Panics only if called after the loan has been returned, which cannot
    /// happen because returning it consumes the loan.
    pub fn buffer_mut(&mut self) -> &mut Yuv420pFrameBuffer {
        self.frame
            .as_mut()
            .expect("a pooled frame is present for the whole loan")
    }
}

impl Drop for PooledFrame {
    fn drop(&mut self) {
        // Returning here rather than at a call site makes the loan end cheaply on any path, unwind
        // included.
        if let Some(frame) = self.frame.take() {
            self.pool.put(frame);
        }
    }
}

/// Encoder frames reused across the export.
///
/// Sized by the pipeline: one frame per encode-channel slot, the one the
/// coordinator is packing, and the one the worker is encoding
/// ([`crate::metrics::readback_encode_frame_bound`]), all reserved from the
/// in-flight budget. A miss allocates rather than blocking, because a frame
/// is cheaper than a stalled coordinator.
#[derive(Debug)]
pub struct FramePool {
    /// The pictures its frames hold.
    pixels: alpha2_media::PixelLayout,
    idle: Mutex<Vec<Yuv420pFrameBuffer>>,
    width: i32,
    height: i32,
    allocated: AtomicU64,
}

impl FramePool {
    /// Creates a pool for frames of one coded size, in `pixels`.
    pub fn new(
        width: u32,
        height: u32,
        pixels: alpha2_media::PixelLayout,
    ) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            pixels,
            idle: Mutex::new(Vec::new()),
            width: i32::try_from(width).map_err(|_| "output width does not fit".to_owned())?,
            height: i32::try_from(height).map_err(|_| "output height does not fit".to_owned())?,
            allocated: AtomicU64::new(0),
        }))
    }

    /// Takes an idle frame, or allocates one.
    ///
    /// # Errors
    /// Returns an error when `FFmpeg` cannot allocate the frame.
    pub fn take(self: &Arc<Self>) -> Result<PooledFrame, String> {
        let idle = self
            .idle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop();
        let frame = if let Some(frame) = idle {
            frame
        } else {
            self.allocated.fetch_add(1, Ordering::Relaxed);
            alpha2_native::packed_frame(
                self.width.unsigned_abs(),
                self.height.unsigned_abs(),
                self.pixels,
            )
            .map_err(|error| encode_error(format!("cannot allocate an encoder frame: {error}")))?
        };
        Ok(PooledFrame {
            frame: Some(frame),
            pool: Arc::clone(self),
        })
    }

    /// Frames this pool had to allocate. One per pipeline slot is expected;
    /// growth beyond that means frames are being held longer than planned.
    pub fn allocated(&self) -> u64 {
        self.allocated.load(Ordering::Relaxed)
    }

    fn put(&self, frame: Yuv420pFrameBuffer) {
        self.idle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(frame);
    }
}

/// The ceiling on encoder-input frames the GPU path may hold at once.
///
/// A hardware session's pool allocates on demand (`FFmpeg`'s with
/// `initial_pool_size = 0`) and recycles a backing once the last reference to
/// it is dropped. The encode runs on a worker, so frames queued for it stay
/// referenced while the coordinator fills more, and nothing else caps the
/// pool: the number of images it ever allocates is the peak number of frames
/// outstanding, and that is what this bounds.
#[cfg(feature = "hwencode")]
#[derive(Debug)]
pub struct GpuFrameBound {
    live: Mutex<usize>,
    released: Condvar,
    limit: usize,
    peak: AtomicU64,
}

#[cfg(feature = "hwencode")]
impl GpuFrameBound {
    /// Creates a bound of `limit` simultaneous frames.
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            live: Mutex::new(0),
            released: Condvar::new(),
            limit: limit.max(1),
            peak: AtomicU64::new(0),
        })
    }

    /// Takes one frame from `pool`, waiting while the bound is reached.
    ///
    /// The wait is bounded and re-checks the cancel token and `stalled`,
    /// because the only thing that releases a frame is the encode worker: a
    /// worker that has already failed will never release another one, and
    /// one stuck inside the encoder neither releases nor fails.
    ///
    /// # Errors
    /// Returns an error when the export was cancelled while waiting, when
    /// `stalled` finds the worker stuck ([`EncodeWorker::check_stall`]), or
    /// the pool cannot allocate.
    pub fn acquire(
        self: &Arc<Self>,
        cancel: &CancelToken,
        stalled: &dyn Fn() -> Result<(), String>,
        acquire: impl FnOnce() -> Result<alpha2_native::EncoderSurface, alpha2_media::EncodeError>,
    ) -> Result<GpuFrameLease, String> {
        let mut live = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *live >= self.limit {
            if cancel.is_cancelled() {
                return Err(encode_error("native encode was cancelled"));
            }
            stalled()?;
            let (guard, _) = self
                .released
                .wait_timeout(live, crate::WORKER_RETRY_INTERVAL)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            live = guard;
        }
        *live = live.saturating_add(1);
        self.peak
            .fetch_max(u64::try_from(*live).unwrap_or(u64::MAX), Ordering::Relaxed);
        drop(live);
        let surface = acquire().map_err(|error| {
            self.release();
            encode_error(format!("cannot allocate an encoder surface: {error}"))
        })?;
        Ok(GpuFrameLease {
            surface: Some(surface),
            bound: Arc::clone(self),
        })
    }

    /// The most surfaces held at once. The pool may have allocated more:
    /// the handoff and the codec keep references of their own.
    pub fn peak(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }

    fn release(&self) {
        let mut live = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *live = live.saturating_sub(1);
        drop(live);
        self.released.notify_one();
    }
}

/// One encoder-input frame on loan from [`GpuFrameBound`].
#[cfg(feature = "hwencode")]
pub struct GpuFrameLease {
    surface: Option<alpha2_native::EncoderSurface>,
    bound: Arc<GpuFrameBound>,
}

#[cfg(feature = "hwencode")]
impl GpuFrameLease {
    /// The surface to hand the packed frame to and then to the encoder.
    ///
    /// # Panics
    /// Panics only if called after the loan has ended, which cannot happen
    /// because ending it consumes the loan.
    pub fn surface(&self) -> &alpha2_native::EncoderSurface {
        self.surface
            .as_ref()
            .expect("a leased surface is present for the whole loan")
    }

    /// The same surface, to write into.
    ///
    /// # Panics
    /// As [`Self::surface`].
    pub fn surface_mut(&mut self) -> &mut alpha2_native::EncoderSurface {
        self.surface
            .as_mut()
            .expect("a leased surface is present for the whole loan")
    }
}

#[cfg(feature = "hwencode")]
impl Drop for GpuFrameLease {
    fn drop(&mut self) {
        // Dropping the surface gives the host's reference back to the pool, so the bound is
        // released after.
        self.surface = None;
        self.bound.release();
    }
}

// An export makes one sink and keeps it where it was made, so the variants' sizes cost nothing;
// they differ by platform (which encoder session `GpuVideo` holds), and the lint fires on Windows
// only.
#[allow(
    clippy::large_enum_variant,
    reason = "one sink per export, moved once into its worker"
)]
/// The output an export encodes into: a software or hardware encoder behind
/// `FFmpeg`'s muxer, with or without audio, or a GPU-resident hardware session.
pub enum NativeExportSink {
    AudioVideo(NativeAvEncodeSink),
    Video(NativeVideoEncodeSink),
    /// A GPU-resident hardware session. It takes encoder surfaces the
    /// coordinator already handed a frame to, and audio blocks, and nothing
    /// else.
    #[cfg(feature = "hwencode")]
    GpuVideo(alpha2_native::HardwareEncodeSession),
}

impl NativeExportSink {
    /// The video encoder this sink opened, and the candidates it passed over.
    ///
    /// The capability list says what the runtime registers; this says what
    /// `avcodec_open2` accepted on this machine, which is the only one of the
    /// two a report can call the encoder the run used.
    pub fn video_selection(&self) -> &NativeVideoEncoderSelection {
        match self {
            Self::AudioVideo(sink) => sink.video_selection(),
            Self::Video(sink) => sink.selection(),
            #[cfg(feature = "hwencode")]
            Self::GpuVideo(session) => session.selection(),
        }
    }

    /// Whether the sink drives a hardware encoder, whose calls wait on the
    /// GPU.
    pub fn is_hardware(&self) -> bool {
        #[cfg(feature = "hwencode")]
        if matches!(self, Self::GpuVideo(_)) {
            return true;
        }
        self.video_selection().hardware_accelerated
    }

    /// Where the sink's time has gone so far.
    pub fn stage_timings(&self) -> EncodeStageTimings {
        match self {
            Self::AudioVideo(sink) => sink.stage_timings(),
            Self::Video(sink) => sink.stage_timings(),
            #[cfg(feature = "hwencode")]
            Self::GpuVideo(session) => session.stage_timings(),
        }
    }

    /// Encodes one audio block or software video frame.
    ///
    /// # Errors
    /// Returns an error when the export was cancelled, the event does not fit
    /// the sink (a software frame for a GPU session), or the encoder or muxer
    /// failed.
    pub fn write(&mut self, event: EncodeEvent, cancel: &CancelToken) -> Result<(), String> {
        #[cfg(feature = "hwencode")]
        if let Self::GpuVideo(session) = self {
            if cancel.is_cancelled() {
                return Err(encode_error("native encode was cancelled"));
            }
            let EncodeEvent::Audio(block) = &event else {
                return Err(encode_error(
                    "the GPU encode path does not take software video frames",
                ));
            };
            return session
                .write_audio(block)
                .map_err(|error| encode_failure("writing media", error));
        }
        match self {
            Self::AudioVideo(sink) => sink.write(event, cancel),
            Self::Video(sink) => sink.write(event, cancel),
            #[cfg(feature = "hwencode")]
            Self::GpuVideo(_) => unreachable!("handled above"),
        }
        .map_err(|error| encode_failure("writing media", error))
    }

    /// Encodes one frame the coordinator already copied into on the GPU.
    ///
    /// # Errors
    /// Returns an error when the export was cancelled, the sink is not the
    /// GPU one, or the encoder rejected the frame.
    #[cfg(feature = "hwencode")]
    pub fn write_gpu_video(
        &mut self,
        surface: &alpha2_native::EncoderSurface,
        cancel: &CancelToken,
    ) -> Result<(), String> {
        let Self::GpuVideo(session) = self else {
            return Err("this sink does not take GPU-resident frames".to_owned());
        };
        if cancel.is_cancelled() {
            session.abort();
            return Err(encode_error("native encode was cancelled"));
        }
        #[cfg(test)]
        if let Some(error) = rejected_for_tests() {
            return Err(encode_failure("writing media", error));
        }
        session
            .encode(surface)
            .map_err(|error| encode_failure("writing media", error))
    }

    /// Encodes a frame the coordinator already packed into encoder memory.
    pub fn write_prepared_video(
        &mut self,
        frame: &mut Yuv420pFrameBuffer,
        pts: MediaTime,
        cancel: &CancelToken,
    ) -> Result<(), String> {
        if cancel.is_cancelled() {
            self.abort();
            return Err(encode_error("native encode was cancelled"));
        }
        match self {
            Self::AudioVideo(sink) => sink.write_prepared_video(frame, pts),
            Self::Video(sink) => sink.write_prepared_video(frame, pts),
            #[cfg(feature = "hwencode")]
            Self::GpuVideo(_) => {
                return Err(encode_error(
                    "the GPU encode path does not take packed CPU frames",
                ));
            }
        }
        .map_err(|error| encode_failure("writing media", error))
    }

    /// How the finished output was published; `None` until [`Self::finish`]
    /// has published it.
    pub fn publish_report(&self) -> Option<alpha2_native::PublishReport> {
        match self {
            Self::AudioVideo(sink) => sink.publish_report(),
            Self::Video(sink) => sink.publish_report(),
            #[cfg(feature = "hwencode")]
            Self::GpuVideo(session) => session.publish_report(),
        }
    }

    /// Flushes the encoders and closes the output, which moves the partial
    /// file into place.
    ///
    /// # Errors
    /// Returns an error when the export was cancelled or flushing, closing
    /// or publishing failed.
    pub fn finish(&mut self, cancel: &CancelToken) -> Result<(), String> {
        #[cfg(feature = "hwencode")]
        if let Self::GpuVideo(session) = self {
            if cancel.is_cancelled() {
                session.abort();
                return Err(encode_error("native encode was cancelled"));
            }
            // Flush and close are one step: only a closed output renames the partial file into
            // place.
            return session
                .finish(cancel)
                .map_err(|error| encode_failure("finalizing output", error));
        }
        match self {
            Self::AudioVideo(sink) => sink.finish(cancel),
            Self::Video(sink) => sink.finish(cancel),
            #[cfg(feature = "hwencode")]
            Self::GpuVideo(_) => unreachable!("handled above"),
        }
        .map_err(|error| encode_failure("finalizing output", error))
    }

    /// Abandons the output and removes its partial file. A cleanup failure is
    /// logged, not returned.
    pub fn abort(&mut self) {
        let result = match self {
            Self::AudioVideo(sink) => sink.abort(),
            Self::Video(sink) => sink.abort(),
            #[cfg(feature = "hwencode")]
            Self::GpuVideo(session) => {
                session.abort();
                Ok(())
            }
        };
        if let Err(error) = result {
            alpha2_os::diagnostic!("alpha2 native export cleanup failed: {error}");
        }
    }
}

/// Work the coordinator hands the encode worker, in output order.
pub enum EncodeCommand {
    /// An audio block, or a software video frame with its output index.
    Event {
        event: EncodeEvent,
        video_index: Option<u64>,
    },
    /// One output frame the coordinator packed straight into encoder memory.
    /// The buffer travels back to the pool with its completion.
    PreparedVideo {
        frame: PooledFrame,
        video_index: u64,
        pts: MediaTime,
    },
    /// One output frame already resident in encoder memory, with its pts
    /// already set. Nothing about it travels over the CPU.
    #[cfg(feature = "hwencode")]
    GpuVideo {
        frame: GpuFrameLease,
        video_index: u64,
        /// Luma-plus-chroma bytes the frame occupies, for the queue accounting.
        bytes: u64,
    },
    /// Flush and close the output; the worker exits after it.
    Finish,
}

/// What the encode worker reports back to the coordinator.
pub enum EncodeWorkerEvent {
    /// A packed frame the worker has finished with, on its way back to the
    /// pool. Sent before the completion so the coordinator can reuse it as
    /// soon as the encoder is done reading.
    FrameReturned(PooledFrame),
    /// One command finished.
    MediaComplete {
        /// The output video frame, or `None` for audio.
        video_index: Option<u64>,
        bytes: u64,
        /// When the worker picked the command up, from the export's start.
        started_at: Duration,
        duration: Duration,
        /// How long `avcodec_send_frame` took inside that span, which is the
        /// boundary between the codec and the drain.
        sent: Duration,
        /// The sink's running totals after this command.
        stages: EncodeStageTimings,
    },
    /// The output was closed and published, with the sink's final totals
    /// and how it was published.
    Finished(EncodeStageTimings, Option<alpha2_native::PublishReport>),
    /// The worker stopped on this error, already recorded and phase-tagged.
    Failed(String),
}

/// Bytes one packed frame occupies, for the queue accounting.
fn packed_frame_bytes(frame: &Yuv420pFrameBuffer) -> u64 {
    let (width, height) = frame.size();
    let luma = u64::try_from(width.saturating_mul(height)).unwrap_or(u64::MAX);
    luma.saturating_add(luma / 2)
}

/// Bytes an event's payload occupies, for the queue accounting.
pub fn encode_event_bytes(event: &EncodeEvent) -> u64 {
    match event {
        EncodeEvent::Video(frame) => frame.planes.iter().fold(0_u64, |bytes, plane| {
            bytes.saturating_add(u64::try_from(plane.bytes.len()).unwrap_or(u64::MAX))
        }),
        EncodeEvent::Audio(block) => audio_block_bytes(block),
    }
}

/// The export's one record of its first failure.
///
/// Every part of an export (the coordinator, the decode pool, the encode
/// worker) records the failure it found, and the first one recorded is what
/// the export reports: a cancellation it caused elsewhere is not. A part that fails
/// records before it cancels the export's token, so a "cancelled" any other
/// part meets afterwards finds the cause already there; such a notice, seen on
/// a token that was already cancelled, is not recorded at all
/// ([`Self::record_unless_cancelled`]). Every `Err` a worker hands the
/// coordinator, and every `Err` an export returns, goes through
/// [`Self::cause_or`] or [`Self::report`].
#[derive(Clone, Default)]
pub struct PipelineFailure {
    pub cause: Arc<Mutex<Option<String>>>,
}

impl PipelineFailure {
    /// Forgets the recorded failure, for an export that starts over on another path.
    pub fn clear(&self) {
        self.cause
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    /// Records `error` as the first failure, unless one was recorded already.
    pub fn record(&self, error: &str) {
        let mut cause = self
            .cause
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cause.is_none() {
            *cause = Some(error.to_owned());
        }
    }

    /// Records a failure a worker met, unless `cancel` was set before it
    /// looked: then the failure is what that cancel did, and the part that
    /// cancelled recorded its own cause first (or a client cancelled, which
    /// is reported as a cancel).
    pub fn record_unless_cancelled(&self, cancel: &CancelToken, error: &str) {
        if !cancel.is_cancelled() {
            self.record(error);
        }
    }

    /// The first failure recorded, or `fallback` when there is none.
    pub fn cause_or(&self, fallback: impl Into<String>) -> String {
        self.cause
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| fallback.into())
    }

    /// What an export that stops with `error` reports: records `error` if it
    /// is the first failure and returns the first failure.
    pub fn report(&self, error: String) -> String {
        self.record(&error);
        self.cause_or(error)
    }
}

/// How full the encode channel was each time the worker took a command.
///
/// `encodeWorkerIdleMs` says the worker ran dry; it does not say whether the
/// channel was ever full, and those are the two ends of the same question. A
/// histogram of the occupancy the worker observed answers both: all mass at
/// zero is a starved encoder, all mass at the depth is a saturated one.
pub struct EncodeChannelDepth {
    /// Commands accepted by the channel and not yet taken by the worker.
    pending: AtomicU64,
    /// One counter per reachable remaining-depth, `0..=depth`.
    histogram: Vec<AtomicU64>,
}

impl EncodeChannelDepth {
    /// Creates a histogram for a channel of this depth.
    pub fn new(depth: usize) -> Arc<Self> {
        Arc::new(Self {
            pending: AtomicU64::new(0),
            histogram: (0..=depth.saturating_add(1))
                .map(|_| AtomicU64::new(0))
                .collect(),
        })
    }

    /// Hands `command` to the channel, counted before it goes in: the worker
    /// may take it the moment it is there, and [`Self::taken`] must find it
    /// counted. Counted after, a take in between found the count short: one
    /// below zero, which wrapped, or one below the commands still queued,
    /// which recorded too low a depth. A send the channel refuses takes its
    /// count back; a take in that instant counts it, which is one more than
    /// the channel holds, and never wraps.
    pub(crate) fn try_send<T>(
        &self,
        sender: &SyncSender<T>,
        command: T,
    ) -> Result<(), TrySendError<T>> {
        self.pending.fetch_add(1, Ordering::Relaxed);
        sender.try_send(command).inspect_err(|_| {
            self.pending.fetch_sub(1, Ordering::Relaxed);
        })
    }

    /// Records the depth left behind by the command the worker just took.
    pub(crate) fn taken(&self) {
        // The channel orders the count before the take, so it is never zero here.
        let previous = self.pending.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(
            previous > 0,
            "the encode worker took a command the channel depth did not count"
        );
        let remaining = previous.saturating_sub(1);
        let slot = usize::try_from(remaining).unwrap_or(usize::MAX);
        if let Some(counter) = self.histogram.get(slot) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The observed occupancies, indexed by the depth left behind.
    pub fn samples(&self) -> Vec<u64> {
        self.histogram
            .iter()
            .map(|counter| counter.load(Ordering::Relaxed))
            .collect()
    }
}

/// Time the encode worker spent with nothing to encode.
///
/// This is the counter that separates a saturated encoder from a starved one.
/// `videoEncodeMs` alone cannot: a thread that encodes for 10 s out of 12 s
/// and one that encodes for 10 s out of 10 s report the same number, and only
/// the first has headroom worth moving work into.
#[derive(Default)]
pub struct EncodeWorkerIdle(AtomicU64);

impl EncodeWorkerIdle {
    fn record(&self, waited: Duration) {
        self.0.fetch_add(
            u64::try_from(waited.as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    /// Time blocked waiting for a command.
    pub fn total(&self) -> Duration {
        Duration::from_nanos(self.0.load(Ordering::Relaxed))
    }
}

/// The coordinator's handle on the encode worker thread. Dropping it cancels
/// the worker and joins it, or abandons it once it is found stuck.
pub struct EncodeWorker {
    pub commands: Option<SyncSender<EncodeCommand>>,
    pub results: Receiver<EncodeWorkerEvent>,
    pub join: Option<JoinHandle<()>>,
    pub cancel: CancelToken,
    pub idle: Arc<EncodeWorkerIdle>,
    pub depth: Arc<EncodeChannelDepth>,
    pub failure: PipelineFailure,
    /// Beaten before every command and every encoder or muxer call; see
    /// [`WorkerHeartbeat`].
    pub heartbeat: Arc<WorkerHeartbeat>,
    pub progress: Arc<CommandProgress>,
    /// Set for a depth-0 channel: [`Self::send`] returns only once the worker
    /// has taken the command, so nothing waits queued behind the encoder.
    pub rendezvous: bool,
    /// How long one wait on the worker lasts before it checks for a stall;
    /// [`STALL_POLL`] outside tests.
    pub poll: Duration,
    /// See [`crate::config::configured_stall_timeout`].
    pub stall_timeout: Duration,
    /// Set once the worker was found stuck: its thread will never return, so
    /// it is left behind rather than joined.
    pub stalled: AtomicBool,
    /// Set once `Finish` was handed over: a stall from then on is the mux's.
    pub finishing: AtomicBool,
}

/// Everything one encode worker thread owns.
struct EncodeWorkerState {
    sink: NativeExportSink,
    commands: Receiver<EncodeCommand>,
    results: mpsc::Sender<EncodeWorkerEvent>,
    cancel: CancelToken,
    failure: PipelineFailure,
    idle: Arc<EncodeWorkerIdle>,
    depth: Arc<EncodeChannelDepth>,
    heartbeat: Arc<WorkerHeartbeat>,
    progress: Arc<CommandProgress>,
    epoch: Instant,
    /// The video frame whose write fails as a full disk does; see
    /// [`INJECT_ENCODE_FAILURE`].
    #[cfg(test)]
    fail_video_at: Option<u64>,
    /// How long each video frame takes at least; see [`INJECT_SLOW_ENCODE`].
    #[cfg(test)]
    slow_video: Option<Duration>,
}

/// One command's span on the encode worker, from pickup to completion.
///
/// Every command reports the same four things (when the worker started it,
/// how long it took, how much of that was `avcodec_send_frame`, and the sink's
/// running totals afterwards) and fails the same way. Only the call between
/// them differs, so only the call is written out at each arm.
struct EncodeSpan {
    before: EncodeStageTimings,
    started: Instant,
    started_at: Duration,
    bytes: u64,
    video_index: Option<u64>,
}

impl EncodeSpan {
    fn begin(state: &EncodeWorkerState, bytes: u64, video_index: Option<u64>) -> Self {
        let started = Instant::now();
        Self {
            before: state.sink.stage_timings(),
            started_at: started.saturating_duration_since(state.epoch),
            started,
            bytes,
            video_index,
        }
    }

    /// Reports the outcome. Returns false when the worker must stop.
    fn end(self, state: &mut EncodeWorkerState, written: Result<(), String>) -> bool {
        #[cfg(test)]
        if let (Some(_), Some(slow)) = (self.video_index, state.slow_video) {
            thread::sleep(slow.saturating_sub(self.started.elapsed()));
        }
        #[cfg(test)]
        let written = match (written, state.fail_video_at) {
            (Ok(()), Some(frame)) if self.video_index == Some(frame) => {
                Err(INJECTED_WRITE_FAILURE.to_owned())
            }
            (written, _) => written,
        };
        if let Err(error) = written {
            let error = crate::failure::tag_phase(
                crate::failure::ExportPhase::Encode,
                self.video_index,
                encode_error(error),
            );
            state.failure.record_unless_cancelled(&state.cancel, &error);
            alpha2_os::diagnostic!(
                "alpha2 native encode worker failed: {}",
                crate::tag::untagged(&error)
            );
            let _ = state.results.send(EncodeWorkerEvent::Failed(error));
            state.cancel.cancel();
            state.sink.abort();
            return false;
        }
        let stages = state.sink.stage_timings();
        let call = stages.since(self.before);
        let _ = state.results.send(EncodeWorkerEvent::MediaComplete {
            video_index: self.video_index,
            bytes: self.bytes,
            started_at: self.started_at,
            duration: self.started.elapsed(),
            sent: span_send_time(call),
            stages,
        });
        true
    }
}

/// How much of a command's span went to `avcodec_send_frame` on the worker
/// that ran it: the video encoder's sends. The AAC encoder runs on a thread of
/// its own, which publishes its time whenever it finishes a frame, whatever
/// command the worker is running then; that time is the sink's total, never
/// one command's.
pub(crate) fn span_send_time(call: EncodeStageTimings) -> Duration {
    call.video_send_frame
}

/// The encode command channel for `depth` queued commands. A depth-0
/// (rendezvous) channel still holds one command: [`EncodeWorker::send`] then
/// waits until the worker has taken it, which a zero-capacity channel could
/// only offer through a blocking send or a retry poll.
pub(crate) fn command_channel(
    depth: usize,
) -> (SyncSender<EncodeCommand>, Receiver<EncodeCommand>) {
    mpsc::sync_channel(depth.max(1))
}

/// Waits for the next command and records its take; `None` once the channel
/// is closed.
pub(crate) fn take_command(
    commands: &Receiver<EncodeCommand>,
    depth: &EncodeChannelDepth,
    progress: &CommandProgress,
) -> Option<EncodeCommand> {
    progress.set_waiting(true);
    let received = commands.recv();
    progress.set_waiting(false);
    let command = received.ok()?;
    depth.taken();
    progress.took_one();
    Some(command)
}

/// Encodes commands until the channel closes, a `Finish` arrives, or a write
/// fails. A failure cancels the export and aborts the partial output.
///
/// Each command is work the heartbeat watches, from pickup to completion, and
/// the sink beats it again before every encoder and muxer call inside
/// ([`alpha2_native::watch_encode_calls`]); waiting for the next command is
/// not work.
fn run_encode_worker(state: &mut EncodeWorkerState) {
    loop {
        let idle_started = Instant::now();
        let received = take_command(&state.commands, &state.depth, &state.progress);
        state.idle.record(idle_started.elapsed());
        let Some(command) = received else { break };
        match command {
            EncodeCommand::Event { event, video_index } => {
                let _work = state.heartbeat.begin_work();
                let bytes = encode_event_bytes(&event);
                let span = EncodeSpan::begin(state, bytes, video_index);
                let written = state.sink.write(event, &state.cancel);
                if !span.end(state, written) {
                    return;
                }
            }
            EncodeCommand::PreparedVideo {
                mut frame,
                video_index,
                pts,
            } => {
                let _work = state.heartbeat.begin_work();
                let bytes = packed_frame_bytes(frame.buffer_mut());
                let span = EncodeSpan::begin(state, bytes, Some(video_index));
                let written =
                    state
                        .sink
                        .write_prepared_video(frame.buffer_mut(), pts, &state.cancel);
                // The encoder has read the frame by the time the call returns, so the loan goes
                // back before the completion the coordinator is waiting on.
                let _ = state.results.send(EncodeWorkerEvent::FrameReturned(frame));
                if !span.end(state, written) {
                    return;
                }
            }
            #[cfg(feature = "hwencode")]
            EncodeCommand::GpuVideo {
                frame,
                video_index,
                bytes,
            } => {
                let _work = state.heartbeat.begin_work();
                let span = EncodeSpan::begin(state, bytes, Some(video_index));
                let written = state.sink.write_gpu_video(frame.surface(), &state.cancel);
                // The encoder holds its own reference to the image by now, so the loan (one slot of
                // the frame bound) ends here rather than at the end of the arm.
                drop(frame);
                if !span.end(state, written) {
                    return;
                }
            }
            EncodeCommand::Finish => {
                let _work = state.heartbeat.begin_close();
                match state.sink.finish(&state.cancel) {
                    Ok(()) => {
                        let _ = state.results.send(EncodeWorkerEvent::Finished(
                            state.sink.stage_timings(),
                            state.sink.publish_report(),
                        ));
                    }
                    Err(error) => {
                        let error = crate::failure::tag_phase(
                            crate::failure::ExportPhase::Mux,
                            None,
                            encode_error(error),
                        );
                        state.failure.record_unless_cancelled(&state.cancel, &error);
                        alpha2_os::diagnostic!(
                            "alpha2 native encode worker failed: {}",
                            crate::tag::untagged(&error)
                        );
                        let _ = state.results.send(EncodeWorkerEvent::Failed(error));
                        state.cancel.cancel();
                        state.sink.abort();
                    }
                }
                return;
            }
        }
    }
    let _work = state.heartbeat.begin_work();
    state.sink.abort();
}

#[cfg(test)]
thread_local! {
    /// Makes the next encode worker on this thread fail to start, as a thread
    /// limit does.
    pub static REFUSE_ENCODE_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Makes the next encode worker on this thread block forever where the
    /// stall says, as a stuck `FFmpeg` call does, and watches it with this
    /// limit instead of the configured one.
    pub static INJECT_ENCODE_STALL: std::cell::Cell<Option<(InjectedStall, Duration)>> =
        const { std::cell::Cell::new(None) };
    /// Makes the next encode worker on this thread fail writing this video
    /// frame, with [`INJECTED_WRITE_FAILURE`], as a full disk does.
    pub static INJECT_ENCODE_FAILURE: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
    /// Makes the next encode worker on this thread take at least this long
    /// over each video frame, as a slow encoder does.
    pub static INJECT_SLOW_ENCODE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// What a muxer write on a full disk reports, as the sink words it.
#[cfg(test)]
pub const INJECTED_WRITE_FAILURE: &str =
    "native encode failed while writing media: packet write failed: No space left on device";

impl EncodeWorker {
    /// Starts the encode worker on its own thread, which owns `sink` from
    /// then on.
    ///
    /// `stall_timeout` is how long one command or encoder call may run before
    /// the worker is reported stuck
    /// ([`crate::config::configured_stall_timeout`]).
    ///
    /// # Errors
    /// When the OS refuses the thread (a process or cgroup thread limit, or
    /// no memory for its stack). `sink` is dropped, which removes its partial
    /// output.
    pub fn spawn(
        sink: NativeExportSink,
        cancel: CancelToken,
        failure: PipelineFailure,
        channel_depth: usize,
        epoch: Instant,
        stall_timeout: Duration,
    ) -> Result<Self, String> {
        #[cfg(test)]
        let (heartbeat, stall_timeout) = match INJECT_ENCODE_STALL.with(std::cell::Cell::take) {
            Some((stall, limit)) => (WorkerHeartbeat::stalling(Some(stall)), limit),
            None => (WorkerHeartbeat::default(), stall_timeout),
        };
        #[cfg(not(test))]
        let heartbeat = WorkerHeartbeat::default();
        #[cfg(test)]
        let fail_video_at = INJECT_ENCODE_FAILURE.with(std::cell::Cell::take);
        #[cfg(test)]
        let slow_video = INJECT_SLOW_ENCODE.with(std::cell::Cell::take);
        let heartbeat = Arc::new(heartbeat);
        // A hardware encoder waits on the GPU inside FFmpeg; a stall there is the GPU's, and a
        // stall in a software one is most likely its output's.
        if sink.is_hardware() {
            heartbeat.opened_hardware();
        }
        let (command_sender, command_receiver) = command_channel(channel_depth);
        let (result_sender, result_receiver) = mpsc::channel();
        let worker_cancel = cancel.clone();
        let worker_failure = failure.clone();
        let idle = Arc::new(EncodeWorkerIdle::default());
        let worker_idle = Arc::clone(&idle);
        let depth = EncodeChannelDepth::new(channel_depth);
        let worker_depth = Arc::clone(&depth);
        let progress = Arc::new(CommandProgress::default());
        let worker_progress = Arc::clone(&progress);
        let worker_heartbeat = Arc::clone(&heartbeat);
        let run = move || {
            let _exit = WorkerExit(Arc::clone(&worker_progress));
            let beat = Arc::clone(&worker_heartbeat);
            let _watch = alpha2_native::watch_encode_calls(move || beat.beat());
            let mut state = EncodeWorkerState {
                sink,
                commands: command_receiver,
                results: result_sender,
                cancel: worker_cancel,
                failure: worker_failure,
                idle: worker_idle,
                depth: worker_depth,
                heartbeat: Arc::clone(&worker_heartbeat),
                progress: worker_progress,
                epoch,
                #[cfg(test)]
                fail_video_at,
                #[cfg(test)]
                slow_video,
            };
            run_encode_worker(&mut state);
            // Dropping a hardware session waits on the GPU inside FFmpeg, so it is watched too.
            let _work = worker_heartbeat.begin_work();
            drop(state);
        };
        let builder = thread::Builder::new().name("alpha2-encode".to_owned());
        #[cfg(test)]
        let spawned = if REFUSE_ENCODE_THREAD.with(std::cell::Cell::take) {
            drop(run);
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
        } else {
            builder.spawn(run)
        };
        #[cfg(not(test))]
        let spawned = builder.spawn(run);
        let join = spawned.map_err(|error| {
            encode_error(format!("failed to start native encode worker: {error}"))
        })?;
        Ok(Self {
            commands: Some(command_sender),
            results: result_receiver,
            join: Some(join),
            cancel,
            idle,
            depth,
            failure,
            heartbeat,
            progress,
            rendezvous: channel_depth == 0,
            poll: STALL_POLL,
            stall_timeout,
            stalled: AtomicBool::new(false),
            finishing: AtomicBool::new(false),
        })
    }

    /// A worker handle built as [`Self::spawn`] builds it, without the thread:
    /// tests take commands from the returned receiver themselves and put their
    /// thread's handle in [`Self::join`].
    #[cfg(test)]
    pub(crate) fn detached(channel_depth: usize) -> (Self, Receiver<EncodeCommand>) {
        let (command_sender, command_receiver) = command_channel(channel_depth);
        let (_result_sender, result_receiver) = mpsc::channel();
        let worker = Self {
            commands: Some(command_sender),
            results: result_receiver,
            join: None,
            cancel: CancelToken::default(),
            idle: Arc::default(),
            depth: EncodeChannelDepth::new(channel_depth),
            failure: PipelineFailure::default(),
            heartbeat: Arc::default(),
            progress: Arc::default(),
            rendezvous: channel_depth == 0,
            poll: STALL_POLL,
            stall_timeout: Duration::from_secs(60),
            stalled: AtomicBool::new(false),
            finishing: AtomicBool::new(false),
        };
        (worker, command_receiver)
    }

    /// Hands one command to the encode worker, waiting while the channel is
    /// full. Returns whether the hand-off had to wait for a busy encoder: on a
    /// queued channel, the channel was full; on a rendezvous channel, the
    /// worker was not waiting for a command when this one was ready.
    ///
    /// On a rendezvous channel the call returns once the worker has taken the
    /// command, so each previous command was taken before its own call
    /// returned and the first take after this one's is its own.
    ///
    /// The wait ends when the worker takes a command, not on a fixed
    /// interval: polling the saturated thread on an interval rounds every
    /// queue wait up to it before it is charged to `encodeQueueWaitMs`. It is
    /// not a blocking send, which a worker stuck inside the encoder or the
    /// muxer would hold forever; the wait checks for that stall as it goes.
    /// Cancellation and failure arrive as a channel disconnect, because the
    /// worker drops its receiver on every exit path.
    pub fn send(&self, command: EncodeCommand) -> Result<bool, String> {
        let sender = self
            .commands
            .as_ref()
            .ok_or_else(|| encode_error("encode worker is closed"))?;
        if matches!(command, EncodeCommand::Finish) {
            self.finishing.store(true, Ordering::Release);
        }
        let mut pending = command;
        let mut waited = false;
        loop {
            // Read before trying, so a command the worker takes in between still ends the wait.
            let seen = self.progress.current();
            let worker_waiting = self.progress.waiting();
            match self.depth.try_send(sender, pending) {
                Ok(()) if self.rendezvous => {
                    self.await_take(seen)?;
                    return Ok(waited || !worker_waiting);
                }
                Ok(()) => return Ok(waited),
                Err(TrySendError::Full(command)) => pending = command,
                Err(TrySendError::Disconnected(_)) => {
                    return Err(self
                        .failure
                        .cause_or(encode_error("encode worker stopped before accepting media")));
                }
            }
            waited = true;
            self.progress.wait_past(seen, self.poll);
            self.check_stall()?;
        }
    }

    /// Waits until the worker has taken a command since `seen`, checking for a
    /// stall as it goes. A worker that has stopped never will, so its exit
    /// ends the wait with the export's failure.
    fn await_take(&self, seen: u64) -> Result<(), String> {
        while self.progress.current() == seen {
            if self.progress.is_stopped() || self.join.as_ref().is_none_or(JoinHandle::is_finished)
            {
                // The take may have landed between the count and the exit.
                if self.progress.current() != seen {
                    break;
                }
                return Err(self
                    .failure
                    .cause_or(encode_error("encode worker stopped before accepting media")));
            }
            self.progress.wait_past(seen, self.poll);
            self.check_stall()?;
        }
        Ok(())
    }

    /// The worker's next event, if one is waiting. A failure it sent is
    /// reported as the export's first failure, which may be another part's.
    pub fn try_result(&self) -> Result<Option<EncodeWorkerEvent>, String> {
        match self.results.try_recv() {
            Ok(EncodeWorkerEvent::Failed(error)) => Err(self.failure.cause_or(error)),
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => self.check_stall().map(|()| None),
            Err(TryRecvError::Disconnected) => Err(self.failure.cause_or(encode_error(
                "encode worker stopped without a terminal result",
            ))),
        }
    }

    /// Waits for the worker's next event, checking for a stall as it goes.
    pub fn result(&self) -> Result<EncodeWorkerEvent, String> {
        loop {
            match self.results.recv_timeout(STALL_POLL) {
                Ok(EncodeWorkerEvent::Failed(error)) => return Err(self.failure.cause_or(error)),
                Ok(event) => return Ok(event),
                Err(RecvTimeoutError::Timeout) => self.check_stall()?,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(self.failure.cause_or(encode_error(
                        "encode worker stopped without a terminal result",
                    )));
                }
            }
        }
    }

    /// Joins the worker after it finished, which it does once it has dropped
    /// its sink. A hardware session's drop waits on the GPU, so the join
    /// checks for a stall as it goes.
    pub fn join_success(&mut self) -> Result<(), String> {
        drop(self.commands.take());
        let Some(join) = self.join.as_ref() else {
            return Ok(());
        };
        while !join.is_finished() {
            self.check_stall()?;
            thread::sleep(ENCODE_JOIN_POLL);
        }
        if self.join.take().is_some_and(|join| join.join().is_err()) {
            return Err(encode_error("encode worker panicked"));
        }
        Ok(())
    }

    /// Fails once the worker has been inside one command or encoder call for
    /// longer than [`Self::stall_timeout`], and stops the export: nothing it
    /// holds will come back.
    ///
    /// The message says whether the worker drives a hardware encoder
    /// ([`crate::protocol::HARDWARE_ENCODE_STALL_MARKER`]), which waits on the
    /// GPU, or a software one, which is most likely blocked writing its
    /// output; a hardware one's carries the `HARDWARE_STALL` evidence its
    /// `resource` is read from.
    pub fn check_stall(&self) -> Result<(), String> {
        let Some(stalled_for) = self.heartbeat.stuck_past(self.stall_timeout) else {
            return Ok(());
        };
        self.stalled.store(true, Ordering::Release);
        let hardware = self.heartbeat.holds_hardware();
        let cause = if hardware {
            format!(
                "{}: a GPU encoder waiting on work that never completes does this",
                crate::protocol::HARDWARE_ENCODE_STALL_MARKER
            )
        } else {
            "in a software encoder: FFmpeg is blocked, most likely writing an output that \
             stopped answering"
                .to_owned()
        };
        let phase = if self.finishing.load(Ordering::Acquire) {
            crate::failure::ExportPhase::Mux
        } else {
            crate::failure::ExportPhase::Encode
        };
        let error = crate::failure::tag_phase(
            phase,
            None,
            crate::failure::hardware_stall(
                hardware,
                format!(
                    "{}the encode worker has been inside one encoder call for {} ms, past the {} ms limit, {cause}",
                    crate::protocol::encode_stalled_tag(),
                    stalled_for.as_millis(),
                    self.stall_timeout.as_millis(),
                ),
            ),
        );
        self.failure.record(&error);
        self.cancel.cancel();
        Err(self.failure.cause_or(error))
    }
}

/// How often a host joining a finished encode worker checks it for a stall.
const ENCODE_JOIN_POLL: Duration = Duration::from_millis(1);

impl Drop for EncodeWorker {
    fn drop(&mut self) {
        let Some(join) = self.join.take() else {
            return;
        };
        self.cancel.cancel();
        drop(self.commands.take());
        // A worker stuck inside FFmpeg ignores cancellation, and joining it would hang this thread
        // too.
        let limit = if self.stalled.load(Ordering::Acquire) {
            Duration::ZERO
        } else {
            self.stall_timeout
        };
        crate::heartbeat::join_or_abandon(
            join,
            std::slice::from_ref(&self.heartbeat),
            limit,
            "encode worker",
        );
    }
}

/// What the coordinator knows of the encode worker's progress.
#[derive(Default)]
pub struct EncodeProgressState {
    pub completed_video: u64,
    pub pending_bytes: u64,
    pub pending_events: usize,
}

/// Output frames between progress reports.
///
/// Both the protocol callback and the host's own stderr summary use this.
const PROGRESS_FRAME_CADENCE: u64 = 30;

/// Applies one worker event to `state` and `metrics`, and reports progress
/// every [`PROGRESS_FRAME_CADENCE`] frames and on the last one.
///
/// Returns `true` once the worker finished. Fails on a worker failure, a
/// completion out of output order, or a progress callback error.
pub fn handle_encode_worker_event(
    event: EncodeWorkerEvent,
    state: &mut EncodeProgressState,
    total: u64,
    started: Instant,
    progress: &mut dyn FnMut(crate::protocol::Progress) -> Result<(), HostError>,
    metrics: &mut ExportMetrics,
) -> Result<bool, String> {
    match event {
        EncodeWorkerEvent::FrameReturned(frame) => {
            drop(frame);
            Ok(false)
        }
        EncodeWorkerEvent::MediaComplete {
            video_index,
            bytes,
            started_at,
            duration,
            sent,
            stages,
        } => {
            metrics.encode_stages = stages;
            if let Some(trace) = metrics.trace.as_mut() {
                trace.record_encode(
                    video_index,
                    started_at,
                    started_at.saturating_add(sent),
                    started_at.saturating_add(duration),
                );
            }
            state.pending_bytes = state.pending_bytes.saturating_sub(bytes);
            state.pending_events = state.pending_events.saturating_sub(1);
            if let Some(index) = video_index {
                metrics.video_encode = metrics.video_encode.saturating_add(duration);
                let expected = state.completed_video.saturating_add(1);
                if index.saturating_add(1) != expected {
                    return Err(encode_error(format!(
                        "encode worker completed video {} while expecting {}",
                        index.saturating_add(1),
                        expected
                    )));
                }
                state.completed_video = expected;
                // Per-frame progress at 4K would be a message per 6 ms; share the stderr summary's
                // 30-frame cadence, and always report the final frame.
                if state.completed_video.is_multiple_of(PROGRESS_FRAME_CADENCE)
                    || state.completed_video == total
                {
                    progress(crate::protocol::Progress::frames(
                        state.completed_video,
                        total,
                    ))
                    .map_err(|error| error.message)?;
                    let elapsed = started.elapsed().as_secs_f64();
                    let fps = frames_per_second(state.completed_video, elapsed);
                    alpha2_os::diagnostic!(
                        "alpha2 native export: {}/{total} frames ({fps:.1} fps)",
                        state.completed_video
                    );
                }
            } else {
                metrics.audio_encode = metrics.audio_encode.saturating_add(duration);
            }
            Ok(false)
        }
        EncodeWorkerEvent::Finished(stages, publish) => {
            metrics.encode_stages = stages;
            metrics.publish = publish;
            Ok(true)
        }
        EncodeWorkerEvent::Failed(error) => Err(error),
    }
}

/// Handles every event the worker has already sent, without waiting.
///
/// # Errors
/// As [`handle_encode_worker_event`], and when the worker reports finishing
/// before it was asked to.
pub fn drain_encode_worker(
    worker: &EncodeWorker,
    state: &mut EncodeProgressState,
    total: u64,
    started: Instant,
    progress: &mut dyn FnMut(crate::protocol::Progress) -> Result<(), HostError>,
    metrics: &mut ExportMetrics,
) -> Result<(), String> {
    while let Some(event) = worker.try_result()? {
        if handle_encode_worker_event(event, state, total, started, progress, metrics)? {
            return Err(encode_error(
                "encode worker finished before the export was closed",
            ));
        }
    }
    Ok(())
}

/// The GPU-resident frame index a test makes the encoder reject, and the frames
/// sent so far, process-wide: the encode worker runs on a thread of its own.
#[cfg(all(test, feature = "hwencode"))]
static REJECT_GPU_FRAME: std::sync::Mutex<Option<(u64, u64)>> = std::sync::Mutex::new(None);

/// Makes the GPU-resident encoder reject frame `frame` of the next export, as
/// an Intel integrated GPU's `h264_d3d12va` intermittently does; dropping the
/// guard stops it.
#[cfg(all(test, feature = "hwencode"))]
pub(crate) fn reject_gpu_frame(frame: u64) -> impl Drop {
    struct Stop;
    impl Drop for Stop {
        fn drop(&mut self) {
            *REJECT_GPU_FRAME.lock().expect("injection") = None;
        }
    }
    *REJECT_GPU_FRAME.lock().expect("injection") = Some((frame, 0));
    Stop
}

#[cfg(all(test, feature = "hwencode"))]
fn rejected_for_tests() -> Option<alpha2_media::EncodeError> {
    let mut injected = REJECT_GPU_FRAME.lock().expect("injection");
    let (frame, sent) = injected.as_mut()?;
    let index = *sent;
    *sent += 1;
    if index != *frame {
        return None;
    }
    // As the session's own rejection does, this leaves the session to the export's teardown.
    Some(alpha2_media::EncodeError::EncoderRejected {
        encoder: "injected".to_owned(),
        frame: index,
        message: format!("injected rejected frame {index}"),
    })
}
