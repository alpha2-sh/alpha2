//! Optional per-frame export trace (`ALPHA2_EXPORT_TRACE`).
//!
//! Stage sums cannot show overlap: two threads that each report 10 s inside a 12 s run may have run
//! in lockstep or in parallel. Per-frame handoff timestamps give per-thread occupancy and a
//! critical path instead.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// One output frame's handoffs, measured from the start of the export.
#[derive(Clone, Copy, Debug, Default)]
pub struct VideoFrameTrace {
    pub index: u64,
    pub decode_wait_started: Duration,
    pub decoded: Duration,
    pub installed: Duration,
    pub submitted: Duration,
    pub readback_wait_started: Duration,
    pub readback_ready: Duration,
    pub packed: Duration,
    pub encode_queued: Duration,
    pub encode_started: Duration,
    /// `avcodec_send_frame` returned; separates codec time from the packet drain.
    pub encode_sent: Duration,
    pub encode_finished: Duration,
    /// GPU-resident path: after any wait on the lease bound.
    pub surface_acquired: Duration,
    /// GPU-resident path: the plane pair was free and the render signal staged.
    pub planes_ready: Duration,
    /// `None` when the graph was not drawn.
    pub submissions: Option<alpha2_render::FrameSubmissions>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AudioBlockTrace {
    pub queued: Duration,
    pub started: Duration,
    pub sent: Duration,
    pub finished: Duration,
}

#[derive(Clone, Debug)]
struct PipelineTrace {
    /// 0 for a pipeline created before the first frame.
    frame: u64,
    label: String,
    started: Duration,
    finished: Duration,
}

/// Rows are buffered, not streamed, because writing a line from the coordinator or the encode
/// worker would change what it measures.
pub struct ExportTrace {
    path: PathBuf,
    epoch: Instant,
    video: Vec<VideoFrameTrace>,
    audio: Vec<AudioBlockTrace>,
    pipelines: Vec<PipelineTrace>,
    next_audio: usize,
}

impl ExportTrace {
    pub fn from_env(epoch: Instant) -> Option<Self> {
        let path = std::env::var_os("ALPHA2_EXPORT_TRACE")?;
        if path.is_empty() {
            return None;
        }
        Some(Self::for_path(PathBuf::from(path), epoch))
    }

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

    pub fn now(&self) -> Duration {
        self.epoch.elapsed()
    }

    pub fn push_video(&mut self, row: &VideoFrameTrace) {
        self.video.push(*row);
    }

    pub fn push_audio(&mut self, row: AudioBlockTrace) {
        self.audio.push(row);
    }

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

    /// Video rows are pushed in frame order, so an index is its position. Audio blocks carry no
    /// index and are matched in completion order, which the single encode channel preserves.
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

    /// Times are in milliseconds from the export's start.
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
