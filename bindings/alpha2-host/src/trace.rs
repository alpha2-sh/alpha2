//! Optional per-frame export trace.
//!
//! Stage sums cannot express overlap. Two threads that each report 10 s of
//! work inside a 12 s run may have run almost in lockstep or almost entirely
//! in parallel, and the sums are identical either way; only the intervals
//! separate them. One row per output frame carrying the timestamps of its
//! handoffs does, so a run reduces to per-thread occupancy and a critical
//! path rather than to a column of totals that overlap.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// One output frame's handoffs, measured from the start of the export.
#[derive(Clone, Copy, Debug, Default)]
pub struct VideoFrameTrace {
    /// Zero-based position in the export.
    pub index: u64,
    /// The coordinator began waiting for this frame's decoded sources.
    pub decode_wait_started: Duration,
    /// The decode pool handed its frame set over.
    pub decoded: Duration,
    /// The sources were resident on the render device.
    pub installed: Duration,
    /// The render and readback commands were submitted.
    pub submitted: Duration,
    /// The coordinator began waiting for the readback to map.
    pub readback_wait_started: Duration,
    /// The readback buffer was mapped.
    pub readback_ready: Duration,
    /// The packed planes were ready for the encoder.
    pub packed: Duration,
    /// The frame was accepted by the encode channel.
    pub encode_queued: Duration,
    /// The encode worker began writing it.
    pub encode_started: Duration,
    /// `avcodec_send_frame` returned: the boundary between the codec and the
    /// packet drain, which is where a codec change and a queue change show up
    /// separately.
    pub encode_sent: Duration,
    /// The encode worker finished writing it.
    pub encode_finished: Duration,
    /// GPU-resident path: the coordinator held an encoder surface, after any
    /// wait on the lease bound.
    pub surface_acquired: Duration,
    /// GPU-resident path: the plane pair was free and the render signal
    /// staged, after any wait on the handoff that last read the pair.
    pub planes_ready: Duration,
    /// How the frame's GPU work was submitted and what it was predicted to
    /// cost, when its graph was drawn.
    pub submissions: Option<alpha2_render::FrameSubmissions>,
}

/// One audio block's trip through the encode worker it shares with video.
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioBlockTrace {
    /// The block was accepted by the encode channel.
    pub queued: Duration,
    /// The encode worker began writing it.
    pub started: Duration,
    /// `avcodec_send_frame` returned.
    pub sent: Duration,
    /// The encode worker finished writing it.
    pub finished: Duration,
}

/// One pipeline the renderer created during the export.
#[derive(Clone, Debug)]
struct PipelineTrace {
    /// The frame whose drawing created it (0 for one created before).
    frame: u64,
    label: String,
    started: Duration,
    finished: Duration,
}

/// Rows collected during an export and written once it has finished.
///
/// Rows are buffered rather than streamed because the two threads this
/// measures are the coordinator and the encode worker; writing a line from
/// either one would change what it reports.
pub struct ExportTrace {
    path: PathBuf,
    epoch: Instant,
    video: Vec<VideoFrameTrace>,
    audio: Vec<AudioBlockTrace>,
    pipelines: Vec<PipelineTrace>,
    next_audio: usize,
}

impl ExportTrace {
    /// Creates a trace when `ALPHA2_EXPORT_TRACE` names an output path.
    pub fn from_env(epoch: Instant) -> Option<Self> {
        let path = std::env::var_os("ALPHA2_EXPORT_TRACE")?;
        if path.is_empty() {
            return None;
        }
        Some(Self::for_path(PathBuf::from(path), epoch))
    }

    /// Creates a trace that will write to `path`, for tests and callers that
    /// choose the destination themselves.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn for_path(path: PathBuf, epoch: Instant) -> Self {
        Self {
            path,
            epoch,
            video: Vec::new(),
            audio: Vec::new(),
            pipelines: Vec::new(),
            next_audio: 0,
        }
    }

    /// Time since the export started.
    pub fn now(&self) -> Duration {
        self.epoch.elapsed()
    }

    /// Records one completed output frame.
    pub fn push_video(&mut self, row: &VideoFrameTrace) {
        self.video.push(*row);
    }

    /// Records one audio block handed to the encode worker.
    pub fn push_audio(&mut self, row: AudioBlockTrace) {
        self.audio.push(row);
    }

    /// Records the pipelines the renderer created while drawing frame `frame`.
    pub fn push_pipelines(&mut self, frame: u64, creations: &[alpha2_render::PipelineCreation]) {
        for creation in creations {
            self.pipelines.push(PipelineTrace {
                frame,
                label: creation.label.clone(),
                started: creation.started.saturating_duration_since(self.epoch),
                finished: creation.finished.saturating_duration_since(self.epoch),
            });
        }
    }

    /// Fills in the encode-worker spans reported for one completed command.
    ///
    /// Video rows are pushed in frame order, so a frame's index is also its
    /// position. Audio blocks carry no index and are matched in completion
    /// order, which is the order the single encode channel delivered them.
    pub fn record_encode(
        &mut self,
        video_index: Option<u64>,
        started: Duration,
        sent: Duration,
        finished: Duration,
    ) {
        match video_index {
            Some(index) => {
                if let Some(row) = usize::try_from(index)
                    .ok()
                    .and_then(|position| self.video.get_mut(position))
                    .filter(|row| row.index == index)
                {
                    row.encode_started = started;
                    row.encode_sent = sent;
                    row.encode_finished = finished;
                }
            }
            None => {
                if let Some(row) = self.audio.get_mut(self.next_audio) {
                    row.started = started;
                    row.sent = sent;
                    row.finished = finished;
                    self.next_audio = self.next_audio.saturating_add(1);
                }
            }
        }
    }

    /// Renders every row as CSV, in milliseconds from the export's start.
    #[must_use]
    pub fn to_csv(&self) -> String {
        let mut out =
            String::with_capacity(self.video.len().saturating_mul(112).saturating_add(128));
        out.push_str(
            "kind,index,decodeWaitStartedMs,decodedMs,installedMs,submittedMs,\
             readbackWaitStartedMs,readbackReadyMs,packedMs,encodeQueuedMs,encodeStartedMs,\
             encodeSentMs,encodeFinishedMs,surfaceAcquiredMs,planesReadyMs,label,startedMs,\
             finishedMs,submissions,predictedMs,costFetches,costTranscendentals,costArithmetic\n",
        );
        for row in &self.video {
            // Costs are in unit pixels: a pass's count per pixel times the pixels it covers.
            let plan = row.submissions.map_or_else(String::new, |plan| {
                format!(
                    "{},{},{:.0},{:.0},{:.0}",
                    plan.submissions,
                    plan.predicted_ms
                        .map_or_else(String::new, |ms| format!("{ms:.3}")),
                    plan.cost.fetches,
                    plan.cost.transcendentals,
                    plan.cost.arithmetic,
                )
            });
            let plan = if plan.is_empty() {
                ",,,,".to_owned()
            } else {
                plan
            };
            let _ = writeln!(
                out,
                "video,{},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},,,,{plan}",
                row.index,
                ms(row.decode_wait_started),
                ms(row.decoded),
                ms(row.installed),
                ms(row.submitted),
                ms(row.readback_wait_started),
                ms(row.readback_ready),
                ms(row.packed),
                ms(row.encode_queued),
                ms(row.encode_started),
                ms(row.encode_sent),
                ms(row.encode_finished),
                ms(row.surface_acquired),
                ms(row.planes_ready),
            );
        }
        for (index, row) in self.audio.iter().enumerate() {
            let _ = writeln!(
                out,
                "audio,{index},,,,,,,,{:.3},{:.3},{:.3},{:.3},,,,,,,,,,",
                ms(row.queued),
                ms(row.started),
                ms(row.sent),
                ms(row.finished),
            );
        }
        // A label is an effect ID or a fixed pipeline name: no comma, quote or line break.
        for row in &self.pipelines {
            let _ = writeln!(
                out,
                "pipeline,{},,,,,,,,,,,,,,{},{:.3},{:.3},,,,,",
                row.frame,
                row.label,
                ms(row.started),
                ms(row.finished),
            );
        }
        out
    }

    /// Writes the CSV to the configured path.
    ///
    /// # Errors
    /// Returns the path and the I/O error when the file cannot be written.
    pub fn write(&self) -> Result<&PathBuf, String> {
        fs::write(&self.path, self.to_csv())
            .map(|()| &self.path)
            .map_err(|error| format!("cannot write {}: {error}", self.path.display()))
    }
}

#[allow(clippy::cast_precision_loss)]
fn ms(value: Duration) -> f64 {
    value.as_secs_f64() * 1_000.0
}
