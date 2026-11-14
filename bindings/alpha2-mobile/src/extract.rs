//! Runs extractors an app writes in Swift or Kotlin. The session runs in the library and reads
//! source frames and audio through the platform's decoders, in software so frames carry planes and
//! present alike on every surface. Timeline frames come from the still path, and each delivery
//! goes to the app's [`Extractor`].
//!
//! ```kotlin
//! class Brightness : Extractor {
//!     override fun describe() = """{"id":"com.example.brightness","version":1,
//!         "input":{"media":"video","from":["source"],"format":"luma8"},
//!         "outputs":[{"name":"level","kind":"scalars","channels":["y"]}]}"""
//!     override fun begin(contextJson: String) {}
//!     override fun pass(pass: UInt, backward: Boolean) {}
//!     override fun video(frame: ExtractVideoFrame): String {
//!         val mean = frame.data.sumOf { it.toInt() and 0xff } / (frame.data.size * 255.0)
//!         return """[{"output":"level","time":${frame.time},"values":[$mean]}]"""
//!     }
//!     override fun audio(block: ExtractAudioBlock) = "[]"
//!     override fun end() = "[]"
//! }
//!
//! val sets = engine.extract(ExtractRequest(project, request, mapOf("v" to path), null),
//!     Brightness()).outcome()
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender};

use alpha2_engine::extract::{
    AudioInput, Descriptor, Direction, Emitter, ExtractRequest as RunRequest, Extractor as Engine,
    ExtractorError, InputSpec, Need, PassInfo, RunContext, SuppliedAudio, SuppliedFrame, Supplier,
    VideoInput, client_context_json, delivered_size, emit_from_json,
};
use alpha2_host_core::audio_stream::AudioStream;
use alpha2_host_core::extract::{AudioReader, ReaderSupplier, RunError, VideoReader, run};
use alpha2_host_core::video_stream::{DecodePreference, StreamOptions, VideoStream};
use alpha2_media::{CancelToken, SourceId};
use alpha2_model::{MediaFingerprint, Project};
use alpha2_render::{CpuRenderer, RenderSize};
use alpha2_time::MediaTime;

use crate::error::Alpha2Error;
use crate::job::Job;
use crate::session::{Alpha2, StillsRequest};

/// Why an app's extractor step failed; the run ends with `EXTRACTOR_FAILED`.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ExtractorFailure {
    /// The step failed for `reason`.
    #[error("{reason}")]
    Failed {
        /// Why.
        reason: String,
    },
}

impl From<uniffi::UnexpectedUniFFICallbackError> for ExtractorFailure {
    fn from(error: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Failed {
            reason: error.reason,
        }
    }
}

/// A delivered frame: tightly packed `rgba8` (straight alpha, sRGB) or `luma8`, top row first.
#[derive(Clone, Debug, uniffi::Record)]
pub struct ExtractVideoFrame {
    /// Presentation time in ticks on the input's clock.
    pub time: i64,
    /// In pixels.
    pub width: u32,
    /// In pixels.
    pub height: u32,
    /// `rgba8` or `luma8`.
    pub format: String,
    /// The pixels, a copy the extractor owns.
    pub data: Vec<u8>,
}

/// A delivered audio block: planar f32, channel after channel.
#[derive(Clone, Debug, uniffi::Record)]
pub struct ExtractAudioBlock {
    /// The tick at or before the first sample.
    pub start: i64,
    /// The first sample, at `rate` from the clock's zero.
    pub start_sample: i64,
    /// Frames in the block.
    pub frames: u32,
    /// Samples per second.
    pub rate: u32,
    /// Channels in the block.
    pub channels: u32,
    /// `frames * channels` samples.
    pub samples: Vec<f32>,
}

/// An extractor the app writes. Emits are a JSON array in the host protocol's form:
/// `{ output, time, values }`, `{ output, start, end, text, speaker? }` or
/// `{ output, start, end, label?, score? }`.
#[uniffi::export(with_foreign)]
pub trait Extractor: Send + Sync {
    /// The descriptor, as JSON.
    fn describe(&self) -> String;
    /// The run begins with its context JSON: range, passes, clock, delivered video or audio.
    ///
    /// # Errors
    /// The extractor cannot run.
    fn begin(&self, context_json: String) -> Result<(), ExtractorFailure>;
    /// Pass `pass` begins.
    ///
    /// # Errors
    /// The run cannot go on.
    fn pass(&self, pass: u32, backward: bool) -> Result<(), ExtractorFailure>;
    /// One frame; returns its emits.
    ///
    /// # Errors
    /// The run cannot go on.
    fn video(&self, frame: ExtractVideoFrame) -> Result<String, ExtractorFailure>;
    /// One audio block; returns its emits.
    ///
    /// # Errors
    /// The run cannot go on.
    fn audio(&self, block: ExtractAudioBlock) -> Result<String, ExtractorFailure>;
    /// The run ends; returns emits anywhere in the range.
    ///
    /// # Errors
    /// The run cannot finish.
    fn end(&self) -> Result<String, ExtractorFailure>;
}

/// What to extract.
#[derive(Clone, Debug, uniffi::Record)]
pub struct ExtractRequest {
    /// The project document.
    pub project_json: String,
    /// The run (`extractor`, `version`, `params`, `input`, `outputs`), as JSON.
    pub request_json: String,
    /// Media files by source ID.
    pub assets: HashMap<String, String>,
    /// The original source's fingerprint JSON, recorded as `made_from` on source-anchored data.
    pub made_from_json: Option<String>,
}

/// Calls the app's extractor as the engine's.
struct Foreign {
    extractor: Arc<dyn Extractor>,
    descriptor: Descriptor,
}

fn failed(failure: ExtractorFailure) -> ExtractorError {
    ExtractorError::new(failure.to_string())
}

fn emits(json: &str, out: &mut Emitter) -> Result<(), ExtractorError> {
    if json.trim().is_empty() {
        return Ok(());
    }
    let values: Vec<serde_json::Value> = serde_json::from_str(json).map_err(|error| {
        ExtractorError::with_code(
            "INVALID_EMIT",
            format!("the emits are not a JSON array: {error}"),
        )
    })?;
    for (index, value) in values.iter().enumerate() {
        let emit = emit_from_json(value).map_err(|reason| {
            ExtractorError::with_code("INVALID_EMIT", format!("emit {index} is invalid: {reason}"))
        })?;
        out.push(emit);
    }
    Ok(())
}

impl Engine for Foreign {
    fn describe(&self) -> &Descriptor {
        &self.descriptor
    }

    fn begin(&mut self, context: &RunContext) -> Result<(), ExtractorError> {
        self.extractor
            .begin(client_context_json(context).to_string())
            .map_err(failed)
    }

    fn pass(&mut self, pass: &PassInfo) -> Result<(), ExtractorError> {
        let index = u32::try_from(pass.pass).unwrap_or(u32::MAX);
        self.extractor
            .pass(index, pass.direction == Direction::Backward)
            .map_err(failed)
    }

    fn video(&mut self, frame: &VideoInput, out: &mut Emitter) -> Result<(), ExtractorError> {
        let format = serde_json::to_value(frame.format)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        let json = self
            .extractor
            .video(ExtractVideoFrame {
                time: frame.time.ticks(),
                width: frame.width,
                height: frame.height,
                format,
                data: frame.bytes.clone(),
            })
            .map_err(failed)?;
        emits(&json, out)
    }

    fn audio(&mut self, block: &AudioInput, out: &mut Emitter) -> Result<(), ExtractorError> {
        let json = self
            .extractor
            .audio(ExtractAudioBlock {
                start: block.start.ticks(),
                start_sample: block.start_sample,
                frames: block.frames,
                rate: block.rate,
                channels: block.channels,
                samples: block.samples.clone(),
            })
            .map_err(failed)?;
        emits(&json, out)
    }

    fn end(&mut self, out: &mut Emitter) -> Result<(), ExtractorError> {
        let json = self.extractor.end().map_err(failed)?;
        emits(&json, out)
    }
}

/// What the timeline thread hands each rendered frame to: its key, time, size and premultiplied
/// RGBA8 pixels.
type FrameSink<'a> =
    &'a mut dyn FnMut(usize, MediaTime, RenderSize, Vec<u8>) -> Result<(), Alpha2Error>;

/// Renders timeline frames on the still path on a thread of its own, which keeps the renderer,
/// fonts and decoders between frames.
struct TimelineFrames {
    times: Option<SyncSender<(usize, i64)>>,
    frames: Receiver<Result<SuppliedFrame, Alpha2Error>>,
}

impl TimelineFrames {
    fn start(
        session: &Alpha2,
        project: &Project,
        assets: HashMap<String, String>,
        (width, height): (u32, u32),
        cancel: CancelToken,
    ) -> Result<Self, Alpha2Error> {
        let mut resized = project.clone();
        resized.settings.width = width;
        resized.settings.height = height;
        let request = StillsRequest {
            project_json: resized
                .to_json()
                .map_err(|error| Alpha2Error::invalid(error.to_string()))?,
            assets,
            frames: HashMap::new(),
            resources: HashMap::new(),
            stills: Vec::new(),
            alpha: false,
            overwrite: false,
        };
        let config = session.engine_config().clone();
        let fonts = session.fonts();
        let video = session.video_backends();
        let preference = session.decode_preference();
        let budget = session.frame_budget(alpha2_engine::frame_store::DEFAULT_VIDEO_FRAME_BYTES);
        let gate = session.pause_gate();
        Self::start_with(move |times, sink| {
            crate::stills::render_each(
                &request,
                (&gate, &cancel),
                (&config, &fonts),
                (video, preference),
                &budget,
                times,
                sink,
            )
            .map(drop)
        })
    }

    /// Runs `render` on the timeline thread. It must consume the times it is given, which end
    /// when this drops, and hand each frame to the sink.
    fn start_with(
        render: impl FnOnce(
            std::sync::mpsc::IntoIter<(usize, i64)>,
            FrameSink<'_>,
        ) -> Result<(), Alpha2Error>
        + Send
        + 'static,
    ) -> Result<Self, Alpha2Error> {
        let (times, wanted) = std::sync::mpsc::sync_channel::<(usize, i64)>(0);
        // One slot: a failure is sent after `render` dropped the times, with nobody receiving yet.
        let (deliver, frames) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("alpha2-extract-timeline".into())
            .spawn(move || {
                let failed = deliver.clone();
                let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    render(wanted.into_iter(), &mut |_, time, size, pixels| {
                        let frame = SuppliedFrame {
                            pts: time,
                            width: size.width,
                            height: size.height,
                            rgba: alpha2_host_core::tools::unpremultiplied(pixels),
                        };
                        deliver.send(Ok(frame)).map_err(|_| Alpha2Error::Aborted)
                    })
                }))
                .unwrap_or_else(|payload| {
                    Err(Alpha2Error::Failed {
                        code: "INTERNAL".into(),
                        reason: format!(
                            "the timeline renderer panicked: {}",
                            alpha2_host_core::panic_text(&*payload)
                        ),
                    })
                });
                if let Err(error) = rendered {
                    let _ = failed.send(Err(error));
                }
            })
            .map_err(|error| Alpha2Error::Failed {
                code: "RESOURCE_LIMIT".into(),
                reason: format!("cannot start the timeline thread: {error}"),
            })?;
        Ok(Self {
            times: Some(times),
            frames,
        })
    }

    fn frame(&self, time: MediaTime) -> Result<SuppliedFrame, String> {
        const STOPPED: &str = "the timeline renderer has stopped";
        let times = self.times.as_ref().ok_or(STOPPED)?;
        if times.send((0, time.ticks())).is_err() {
            // The renderer returned; its failure, if it had one, waits in the channel.
            return match self.frames.recv() {
                Ok(Err(error)) => Err(error.to_string()),
                _ => Err(STOPPED.to_owned()),
            };
        }
        match self.frames.recv() {
            Ok(Ok(mut frame)) => {
                frame.pts = time;
                Ok(frame)
            }
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err(STOPPED.to_owned()),
        }
    }
}

impl Drop for TimelineFrames {
    fn drop(&mut self) {
        // Closing the channel ends the renderer's loop.
        self.times.take();
    }
}

/// Source needs from the platform's decoders, timeline frames from the still path.
struct MobileSupplier<'a> {
    reader: ReaderSupplier<'a, VideoStream, AudioStream>,
    timeline: Option<TimelineFrames>,
    cancel: CancelToken,
}

impl Supplier for MobileSupplier<'_> {
    fn frame(&mut self, need: &Need) -> Result<Option<SuppliedFrame>, String> {
        match need {
            Need::TimelineFrame { .. } if self.cancel.is_cancelled() => {
                Err("the extraction was cancelled".into())
            }
            Need::TimelineFrame { time, .. } => self
                .timeline
                .as_ref()
                .ok_or("this run reads no timeline")?
                .frame(*time)
                .map(Some),
            _ => self.reader.frame(need),
        }
    }

    fn audio(&mut self, need: &Need) -> Result<SuppliedAudio, String> {
        match need {
            Need::TimelineAudio { .. } => {
                Err("timeline audio is not supplied on mobile yet; extract from the source".into())
            }
            _ => self.reader.audio(need),
        }
    }
}

/// Runs `extractor` over the request on the calling thread.
#[allow(clippy::too_many_lines)]
fn extract(
    session: &Alpha2,
    request: &ExtractRequest,
    extractor: &mut dyn Engine,
    cancel: &CancelToken,
) -> Result<Vec<alpha2_model::DataSet>, Alpha2Error> {
    let invalid = |error: &dyn std::fmt::Display| Alpha2Error::invalid(error.to_string());
    let project = Project::from_json(&request.project_json).map_err(|error| invalid(&error))?;
    let run_request: RunRequest =
        serde_json::from_str(&request.request_json).map_err(|error| invalid(&error))?;
    let descriptor = extractor.describe().clone();
    let made_from: Option<MediaFingerprint> = request
        .made_from_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| invalid(&error))?;
    let path = |id: &SourceId| {
        request
            .assets
            .get(id.as_str())
            .map(std::path::PathBuf::from)
            .ok_or_else(|| Alpha2Error::Failed {
                code: "MISSING_INPUT".into(),
                reason: format!("no media for source {id}"),
            })
    };
    let source = run_request.input.source_id.clone();
    let video = match (&source, &descriptor.input, session.video_backends()) {
        (Some(id), InputSpec::Video { .. }, Some(backends)) => {
            let options = StreamOptions {
                preference: DecodePreference::Software,
                ..StreamOptions::default()
            };
            let stream = VideoStream::open_file(&path(id)?, backends, options)?;
            Some(VideoReader::new(stream, id.clone(), cancel.clone()))
        }
        (Some(_), InputSpec::Video { .. }, None) => {
            return Err(Alpha2Error::Unsupported {
                reason: "this build has no video decoders".into(),
            });
        }
        _ => None,
    };
    let audio = match (&source, &descriptor.input, session.audio_backends()) {
        (Some(id), InputSpec::Audio { sample_rate, .. }, Some(backends)) => {
            let rate = run_request
                .input
                .sample_rate
                .or(*sample_rate)
                .unwrap_or(project.settings.sample_rate);
            let stream = AudioStream::open_file(
                &path(id)?,
                backends.as_ref(),
                rate,
                run_request.input.stream,
                alpha2_host_core::recovery::DecodePolicy::default(),
            )?;
            Some(AudioReader::new(stream, id.clone(), rate, cancel.clone()))
        }
        (Some(_), InputSpec::Audio { .. }, None) => {
            return Err(Alpha2Error::Unsupported {
                reason: "this build has no audio decoders".into(),
            });
        }
        _ => None,
    };
    let timeline = match (&source, &descriptor.input) {
        (None, InputSpec::Video { max_size, .. }) => {
            let size = run_request.input.size.unwrap_or(*max_size);
            let delivered = delivered_size((project.settings.width, project.settings.height), size);
            Some(TimelineFrames::start(
                session,
                &project,
                request.assets.clone(),
                delivered,
                cancel.clone(),
            )?)
        }
        _ => None,
    };
    let renderer = CpuRenderer::new(1);
    let mut supplier = MobileSupplier {
        reader: ReaderSupplier::new(&renderer, video, audio),
        timeline,
        cancel: cancel.clone(),
    };
    run(
        &project,
        run_request,
        extractor,
        &mut supplier,
        made_from.as_ref(),
    )
    .map_err(|error| match error {
        RunError::Request(error) => Alpha2Error::invalid(error.to_string()),
        RunError::Drive(alpha2_engine::extract::DriveError::Extractor(error))
            if error.code == Some("CANCELLED") =>
        {
            Alpha2Error::Aborted
        }
        RunError::Drive(alpha2_engine::extract::DriveError::Extractor(error)) => {
            Alpha2Error::Failed {
                code: error.code.unwrap_or("EXTRACTOR_FAILED").into(),
                reason: error.message,
            }
        }
        RunError::Drive(alpha2_engine::extract::DriveError::Session(error)) => {
            Alpha2Error::Failed {
                code: "INVALID_EMIT".into(),
                reason: error.to_string(),
            }
        }
        RunError::Drive(error) if cancel.is_cancelled() => {
            let _ = error;
            Alpha2Error::Aborted
        }
        RunError::Drive(error) => Alpha2Error::Failed {
            code: "SOURCE_FAILED".into(),
            reason: error.to_string(),
        },
    })
}

/// The extractor a run uses, made on the run's thread.
/// Makes the run's extractor; the token is the handle's cancel.
type MakeExtractor = Box<dyn FnOnce(&CancelToken) -> Result<RunExtractor, Alpha2Error> + Send>;

/// An extractor, and what its data records as `made_by.plugin`.
pub(crate) struct RunExtractor {
    pub(crate) extractor: Box<dyn Engine>,
    pub(crate) plugin: Option<alpha2_model::MadeByPlugin>,
}

impl Alpha2 {
    /// Checks the request and runs it on a library thread with the extractor `make` builds there.
    pub(crate) fn start_extraction(
        self: Arc<Self>,
        request: ExtractRequest,
        make: MakeExtractor,
    ) -> Result<Arc<ExtractionHandle>, Alpha2Error> {
        let problems = crate::session::project_problems(&request.project_json);
        if !problems.is_empty() {
            return Err(Alpha2Error::InvalidInput { problems });
        }
        let handle = Arc::new(ExtractionHandle {
            job: Job::new("extraction"),
            gate: self.pause_gate(),
        });
        let cancel = handle.job.cancel_token().clone();
        handle.job.spawn("alpha2-extract", move || {
            let mut built = make(&cancel)?;
            let mut sets = extract(&self, &request, &mut *built.extractor, &cancel)?;
            for set in &mut sets {
                if let (Some(plugin), Some(made_by)) = (&built.plugin, &mut set.made_by) {
                    made_by.plugin = Some(plugin.clone());
                }
            }
            serde_json::to_string(&sets).map_err(|error| Alpha2Error::Failed {
                code: "INTERNAL".into(),
                reason: error.to_string(),
            })
        })?;
        Ok(handle)
    }
}

/// A running extraction.
#[derive(uniffi::Object)]
pub struct ExtractionHandle {
    job: Arc<Job<String>>,
    gate: Arc<crate::session::PauseGate>,
}

#[uniffi::export]
impl ExtractionHandle {
    /// Stops the extraction between steps, after which [`Self::outcome`] fails with
    /// [`Alpha2Error::Aborted`] for the caller.
    pub fn cancel(&self) {
        self.job.cancel_token().cancel();
        self.gate.wake();
    }

    /// Whether the extraction has finished, succeeded or not.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.job.is_finished()
    }

    /// The data sets, as JSON, once the extraction finishes.
    ///
    /// # Errors
    /// The extraction's failure (`EXTRACTOR_FAILED`, `INVALID_EMIT`, ...);
    /// [`Alpha2Error::Aborted`] after [`Self::cancel`]; `INTERNAL` when the extraction thread
    /// panicked; [`Alpha2Error::InvalidInput`] when the outcome was already taken.
    pub async fn outcome(&self) -> Result<String, Alpha2Error> {
        self.job.outcome().await
    }
}

#[uniffi::export]
impl Alpha2 {
    /// Starts `extractor` over `request` on a library thread; the extractor is called there.
    ///
    /// # Errors
    /// [`Alpha2Error::InvalidInput`] for an invalid project. Later failures are what
    /// [`ExtractionHandle::outcome`] returns.
    pub fn extract(
        self: Arc<Self>,
        request: ExtractRequest,
        extractor: Arc<dyn Extractor>,
    ) -> Result<Arc<ExtractionHandle>, Alpha2Error> {
        let descriptor: Descriptor = serde_json::from_str(&extractor.describe())
            .map_err(|error| Alpha2Error::invalid(format!("invalid descriptor: {error}")))?;
        self.start_extraction(
            request,
            Box::new(move |_| {
                Ok(RunExtractor {
                    extractor: Box::new(Foreign {
                        extractor,
                        descriptor,
                    }),
                    plugin: None,
                })
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;
    use std::time::Duration;

    use super::*;
    use crate::session::{EngineConfig, RendererMode};

    /// Runs `body` on a thread and fails when it has not returned within `seconds`.
    fn within<R: Send + 'static>(seconds: u64, body: impl FnOnce() -> R + Send + 'static) -> R {
        let (done, wait) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(body());
        });
        wait.recv_timeout(Duration::from_secs(seconds))
            .expect("returned in time")
    }

    fn at(frame: i64) -> MediaTime {
        MediaTime::from_ticks(frame * 4000)
    }

    /// Answers each time with a frame as wide as its position in the run, until `fail_at`.
    fn frames_until(
        fail_at: usize,
    ) -> impl FnOnce(
        std::sync::mpsc::IntoIter<(usize, i64)>,
        FrameSink<'_>,
    ) -> Result<(), Alpha2Error>
    + Send
    + 'static {
        move |times, sink| {
            for (index, (key, time)) in times.enumerate() {
                if index == fail_at {
                    return Err(Alpha2Error::invalid("the renderer broke"));
                }
                let width = u32::try_from(index).expect("small") + 1;
                sink(
                    key,
                    MediaTime::from_ticks(time),
                    RenderSize { width, height: 1 },
                    vec![255; width as usize * 4],
                )?;
            }
            Ok(())
        }
    }

    #[test]
    fn a_renderer_that_fails_before_it_pulls_answers_with_its_error() {
        let answer = within(10, || {
            let frames = TimelineFrames::start_with(|_times, _sink| {
                Err(Alpha2Error::invalid(
                    "effect resource look has no asset path",
                ))
            })
            .expect("started");
            frames.frame(at(0)).map(|_| ())
        });
        let error = answer.expect_err("fails");
        assert!(
            error.contains("effect resource look has no asset path"),
            "{error}"
        );
    }

    #[test]
    fn a_renderer_that_fails_after_a_frame_answers_the_frame_then_its_error() {
        let answers = within(10, || {
            let frames = TimelineFrames::start_with(frames_until(1)).expect("started");
            [
                frames.frame(at(0)),
                frames.frame(at(1)),
                frames.frame(at(2)),
            ]
            .map(|answer| answer.map(|frame| frame.width))
        });
        assert_eq!(answers[0], Ok(1));
        let failure = answers[1].clone().expect_err("fails");
        assert!(failure.contains("the renderer broke"), "{failure}");
        assert_eq!(
            answers[2],
            Err("the timeline renderer has stopped".to_owned())
        );
    }

    #[test]
    fn frames_come_back_in_order_at_the_times_asked_for() {
        let answers = within(10, || {
            let frames = TimelineFrames::start_with(frames_until(usize::MAX)).expect("started");
            (0..3)
                .map(|index| {
                    let frame = frames.frame(at(index)).expect("frame");
                    (frame.pts, frame.width)
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(answers, [(at(0), 1), (at(1), 2), (at(2), 3)]);
    }

    #[test]
    fn a_panicking_renderer_answers_with_internal() {
        let answer = within(10, || {
            let frames = TimelineFrames::start_with(|_times, _sink| panic!("render broke"))
                .expect("started");
            frames.frame(at(0)).map(|_| ())
        });
        let error = answer.expect_err("fails");
        assert!(error.contains("render broke"), "{error}");
    }

    #[test]
    fn dropping_the_frames_unasked_ends_the_renderer() {
        let (ended, end) = std::sync::mpsc::channel();
        let frames = TimelineFrames::start_with(move |times, _sink| {
            let pulled = times.count();
            let _ = ended.send(pulled);
            Ok(())
        })
        .expect("started");
        drop(frames);
        assert_eq!(end.recv_timeout(Duration::from_secs(10)), Ok(0));
    }

    const RED_PROJECT: &str = r#"{"schema_version":1,"id":"p","name":"P",
        "settings":{"width":320,"height":180,"frame_rate":{"num":30,"den":1},"sample_rate":48000},
        "sources":[{"id":"red","kind":"generated","generator":{"type":"fill",
          "paint":{"type":"solid","color":{"red":1,"green":0,"blue":0,"alpha":1}}}}],
        RESOURCES
        "timeline":{"tracks":[{"id":"t","name":"T","kind":"video","clips":[{"id":"c",
          "source_id":"red","timeline_range":{"start":0,"end":40000},
          "source_range":{"start":0,"end":40000}}]}]}}"#;

    const MEAN_REQUEST: &str = r#"{"extractor":"com.example.mean","version":1,
        "input":{"from":"timeline","range":{"start":0,"end":40000},"size":64,
          "frame_rate":{"num":30,"den":1}},
        "outputs":{"red":"redness"}}"#;

    /// Emits each frame's mean red; cancels `handle` from its first frame when it is set.
    #[derive(Default)]
    struct Mean {
        cancel: OnceLock<Arc<ExtractionHandle>>,
        frames: std::sync::atomic::AtomicUsize,
    }

    impl Extractor for Mean {
        fn describe(&self) -> String {
            r#"{"id":"com.example.mean","version":1,
                "input":{"media":"video","from":["timeline"],"format":"rgba8"},
                "outputs":[{"name":"red","kind":"scalars","channels":["r"]}]}"#
                .into()
        }

        fn begin(&self, _context_json: String) -> Result<(), ExtractorFailure> {
            Ok(())
        }

        fn pass(&self, _pass: u32, _backward: bool) -> Result<(), ExtractorFailure> {
            Ok(())
        }

        fn video(&self, frame: ExtractVideoFrame) -> Result<String, ExtractorFailure> {
            self.frames
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(handle) = self.cancel.get() {
                handle.cancel();
            }
            let red = frame.data.iter().step_by(4).map(|&value| f64::from(value));
            let pixels = f64::from(u32::try_from(frame.data.len() / 4).expect("small"));
            let mean = red.sum::<f64>() / pixels / 255.0;
            Ok(format!(
                r#"[{{"output":"red","time":{},"values":[{mean}]}}]"#,
                frame.time
            ))
        }

        fn audio(&self, _block: ExtractAudioBlock) -> Result<String, ExtractorFailure> {
            Ok("[]".into())
        }

        fn end(&self) -> Result<String, ExtractorFailure> {
            Ok("[]".into())
        }
    }

    fn engine() -> Arc<Alpha2> {
        Alpha2::new(EngineConfig {
            renderer: RendererMode::Cpu,
            ..EngineConfig::default()
        })
    }

    fn request(resources: &str) -> ExtractRequest {
        ExtractRequest {
            project_json: RED_PROJECT.replace("RESOURCES", resources),
            request_json: MEAN_REQUEST.into(),
            assets: HashMap::new(),
            made_from_json: None,
        }
    }

    fn outcome(handle: Arc<ExtractionHandle>) -> Result<String, Alpha2Error> {
        within(60, move || pollster::block_on(handle.outcome()))
    }

    #[test]
    fn a_timeline_extraction_reads_every_frame() {
        let handle = engine()
            .extract(request(""), Arc::new(Mean::default()))
            .expect("started");
        let sets = outcome(handle).expect("data");
        assert!(
            sets.contains(r#""values":[1.0,1.0,1.0,1.0,1.0,1.0,1.0,1.0,1.0,1.0]"#),
            "{sets}"
        );
    }

    #[test]
    fn a_timeline_whose_effect_resource_has_no_asset_fails_with_the_reason() {
        let resources =
            r#""effect_resources":[{"id":"look","kind":"lut3d","locator":"look.cube"}],"#;
        let handle = engine()
            .extract(request(resources), Arc::new(Mean::default()))
            .expect("started");
        let error = outcome(handle).expect_err("fails").to_string();
        assert!(
            error.contains("effect resource look has no asset path"),
            "{error}"
        );
    }

    #[test]
    fn a_cancel_during_a_timeline_extraction_aborts_it() {
        let extractor = Arc::new(Mean::default());
        let handle = engine()
            .extract(request(""), Arc::clone(&extractor) as Arc<dyn Extractor>)
            .expect("started");
        // The run may already be past its first frame; a cancel from there on still aborts it.
        let _ = extractor.cancel.set(Arc::clone(&handle));
        let outcome = outcome(handle);
        assert!(matches!(outcome, Err(Alpha2Error::Aborted)), "{outcome:?}");
        assert!(
            extractor.frames.load(std::sync::atomic::Ordering::SeqCst) < 10,
            "the run stopped early"
        );
    }

    #[test]
    fn the_surface_table_timeline_digest_hashes_as_pinned() {
        let (found, pinned) =
            crate::extract_cases::client_case_hash(&engine(), "digest-timeline-cpu").expect("data");
        assert_eq!(found, pinned);
    }

    /// The plugin entry point over the repository's digest plugin, packed here for this machine.
    #[cfg(all(
        feature = "plugins",
        not(any(target_os = "android", target_os = "ios"))
    ))]
    #[test]
    fn the_surface_table_plugin_timeline_digest_hashes_as_pinned() {
        use crate::extract_cases::{case, request, settled, table, tests_dir};
        let table = table();
        let case = case(&table, "plugin-digest-timeline-cpu");
        let dir = std::env::temp_dir().join(format!("alpha2-mobile-plugin-{}", std::process::id()));
        let (manifest, sha256) = alpha2_test_support::extract::pack_plugin(
            &tests_dir().join(".."),
            &dir,
            case["extractor"]["plugin"].as_str().expect("plugin"),
        );
        let plugin = crate::Plugin::load(manifest.to_string_lossy().into_owned(), sha256, false)
            .expect("the plugin loads");
        let handle = engine()
            .extract_plugin(request(&table, &case), plugin)
            .expect("started");
        let sets: serde_json::Value =
            serde_json::from_str(&settled(handle, 120).expect("data")).expect("json");
        let _ = std::fs::remove_dir_all(dir);
        assert_eq!(
            alpha2_test_support::extract::data_hash(&sets),
            case["sha256"].as_str().expect("sha256")
        );
    }

    #[test]
    fn an_extractor_whose_making_panics_settles_as_internal() {
        let handle = engine()
            .start_extraction(request(""), Box::new(|_| panic!("no extractor")))
            .expect("started");
        match outcome(handle) {
            Err(Alpha2Error::Failed { code, reason }) => {
                assert_eq!(code, "INTERNAL");
                assert!(reason.contains("no extractor"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }
}
