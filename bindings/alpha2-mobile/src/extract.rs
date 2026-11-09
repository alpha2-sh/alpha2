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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};

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
use alpha2_render::CpuRenderer;
use alpha2_time::MediaTime;

use crate::error::Alpha2Error;
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
    ) -> Result<Self, Alpha2Error> {
        let mut resized = project.clone();
        resized.settings.width = width;
        resized.settings.height = height;
        let request = StillsRequest {
            project_json: serde_json::to_string(&resized)
                .map_err(|error| Alpha2Error::invalid(error.to_string()))?,
            assets,
            frames: HashMap::new(),
            resources: HashMap::new(),
            stills: Vec::new(),
            alpha: false,
            overwrite: false,
        };
        let (times, wanted) = std::sync::mpsc::sync_channel::<(usize, i64)>(0);
        let (deliver, frames) = std::sync::mpsc::sync_channel(0);
        let config = session.engine_config().clone();
        let fonts = session.fonts();
        let video = session.video_backends();
        let preference = session.decode_preference();
        let budget = session.frame_budget(alpha2_engine::frame_store::DEFAULT_VIDEO_FRAME_BYTES);
        std::thread::Builder::new()
            .name("alpha2-extract-timeline".into())
            .spawn(move || {
                let failed = deliver.clone();
                let rendered = crate::stills::render_each(
                    &request,
                    &config,
                    &fonts,
                    (video, preference),
                    &budget,
                    wanted.iter(),
                    |_, time, size, pixels| {
                        let frame = SuppliedFrame {
                            pts: time,
                            width: size.width,
                            height: size.height,
                            rgba: alpha2_host_core::tools::unpremultiplied(pixels),
                        };
                        deliver.send(Ok(frame)).map_err(|_| Alpha2Error::Aborted)
                    },
                );
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
        self.times
            .as_ref()
            .ok_or("the timeline renderer has stopped")?
            .send((0, time.ticks()))
            .map_err(|_| "the timeline renderer has stopped".to_owned())?;
        match self.frames.recv() {
            Ok(Ok(mut frame)) => {
                frame.pts = time;
                Ok(frame)
            }
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err("the timeline renderer has stopped".to_owned()),
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
}

impl Supplier for MobileSupplier<'_> {
    fn frame(&mut self, need: &Need) -> Result<Option<SuppliedFrame>, String> {
        match need {
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
            )?)
        }
        _ => None,
    };
    let renderer = CpuRenderer::new(1);
    let mut supplier = MobileSupplier {
        reader: ReaderSupplier::new(&renderer, video, audio),
        timeline,
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
type MakeExtractor = Box<dyn FnOnce() -> Result<RunExtractor, Alpha2Error> + Send>;

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
        let handle = Arc::new(ExtractionHandle::default());
        let finished = Arc::clone(&handle);
        std::thread::Builder::new()
            .name("alpha2-extract".into())
            .spawn(move || {
                let outcome = make().and_then(|mut made| {
                    let mut sets =
                        extract(&self, &request, &mut *made.extractor, &finished.cancel)?;
                    for set in &mut sets {
                        if let (Some(plugin), Some(made_by)) = (&made.plugin, &mut set.made_by) {
                            made_by.plugin = Some(plugin.clone());
                        }
                    }
                    serde_json::to_string(&sets).map_err(|error| Alpha2Error::Failed {
                        code: "INTERNAL".into(),
                        reason: error.to_string(),
                    })
                });
                finished.finish(outcome);
            })
            .map_err(|error| Alpha2Error::Failed {
                code: "RESOURCE_LIMIT".into(),
                reason: format!("cannot start the extraction thread: {error}"),
            })?;
        Ok(handle)
    }
}

/// A running extraction.
#[derive(uniffi::Object, Default)]
pub struct ExtractionHandle {
    cancel: CancelToken,
    outcome: Mutex<Option<Result<String, Alpha2Error>>>,
    taken: AtomicBool,
}

#[uniffi::export]
impl ExtractionHandle {
    /// Stops the extraction between steps, after which [`Self::outcome`] fails with
    /// [`Alpha2Error::Aborted`] for the caller.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Whether the extraction has finished, succeeded or not.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.lock().is_some() || self.taken.load(Ordering::Acquire)
    }

    /// The data sets, as JSON, once the extraction finishes.
    ///
    /// # Errors
    /// The extraction's failure (`EXTRACTOR_FAILED`, `INVALID_EMIT`, ...);
    /// [`Alpha2Error::Aborted`] after [`Self::cancel`]; [`Alpha2Error::InvalidInput`] when the
    /// outcome was already taken.
    pub async fn outcome(&self) -> Result<String, Alpha2Error> {
        let outcome = Waiting { handle: self }.await;
        self.taken.store(true, Ordering::Release);
        outcome
    }
}

impl ExtractionHandle {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Result<String, Alpha2Error>>> {
        self.outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn finish(&self, outcome: Result<String, Alpha2Error>) {
        *self.lock() = Some(outcome);
        for waker in WAKERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            waker.wake();
        }
    }
}

/// Wakers of pending [`ExtractionHandle::outcome`] futures, woken when any extraction finishes.
static WAKERS: Mutex<Vec<std::task::Waker>> = Mutex::new(Vec::new());

struct Waiting<'a> {
    handle: &'a ExtractionHandle,
}

impl std::future::Future for Waiting<'_> {
    type Output = Result<String, Alpha2Error>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut wakers = WAKERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(outcome) = self.handle.lock().take() {
            return std::task::Poll::Ready(outcome);
        }
        if self.handle.taken.load(Ordering::Acquire) {
            return std::task::Poll::Ready(Err(Alpha2Error::invalid(
                "this extraction's outcome was already taken",
            )));
        }
        wakers.push(context.waker().clone());
        std::task::Poll::Pending
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
            Box::new(move || {
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
