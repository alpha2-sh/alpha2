//! The engine session apps hold, and its requests and results.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use alpha2_engine::frame_store::DEFAULT_VIDEO_FRAME_BYTES;
use alpha2_host_core::export::FrameBudget;
use alpha2_media::CancelToken;
use serde_json::{Value, json};

use crate::error::Alpha2Error;
use crate::job::Job;
use crate::platform::{self, ExportJob, Platform};

/// Which renderer draws frames.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, uniffi::Enum)]
pub enum RendererMode {
    /// The GPU, else the CPU renderer when no GPU device opens.
    #[default]
    Auto,
    /// The GPU only; [`Alpha2Error::GpuInitFailed`] when none opens.
    Gpu,
    /// The CPU renderer.
    Cpu,
}

/// Which video decoders are used.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, uniffi::Enum)]
pub enum VideoDecodeMode {
    /// Hardware, falling back to the platform's software decoder once per source.
    #[default]
    Auto,
    /// Hardware decoders only.
    Hardware,
    /// Software decoders only.
    Software,
}

/// How decoded video frames reach the renderer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, uniffi::Enum)]
pub enum VideoFrameMode {
    /// Hardware-decoded frames stay on the GPU when the GPU renderer's device imports them;
    /// otherwise frames are copied into memory.
    #[default]
    Auto,
    /// Frames are always copied into memory, as on devices without the import.
    Cpu,
}

/// How a session is set up.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct EngineConfig {
    /// Which renderer draws exports and stills. A [`crate::Player`] always draws on a GPU
    /// device that presents to its window.
    pub renderer: RendererMode,
    /// Which video decoders are used.
    pub video_decode: VideoDecodeMode,
    /// How decoded video frames reach the renderer.
    pub video_frames: VideoFrameMode,
}

/// A font the project's text may use, in fallback order.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FontFile {
    /// The font's bytes (TrueType, OpenType or a collection).
    pub bytes: Vec<u8>,
    /// The face within a collection; 0 for a single font.
    pub face_index: u32,
}

/// One frame [`Alpha2::stills`] renders.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct StillEntry {
    /// The timeline tick it shows; the frame there is rendered.
    pub time_ticks: i64,
    /// Where its PNG file is published, inside the app's sandbox.
    pub output_path: String,
}

/// What [`Alpha2::stills`] renders.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct StillsRequest {
    /// The project, as project JSON.
    pub project_json: String,
    /// Media file paths in the app's sandbox, by source ID, and effect resource files by
    /// resource ID.
    pub assets: HashMap<String, String>,
    /// Each image sequence's frame files, by source ID; an empty path where it has a gap.
    pub frames: HashMap<String, Vec<String>>,
    /// Files an SVG or Lottie source names outside itself, by source ID and locator.
    pub resources: HashMap<String, HashMap<String, String>>,
    /// The frames, in any order.
    pub stills: Vec<StillEntry>,
    /// Keep the project's transparency instead of drawing its background.
    pub alpha: bool,
    /// Replace files already at the outputs instead of refusing.
    pub overwrite: bool,
}

/// A font set with [`Alpha2::set_fonts`], with the family names text finds it by.
#[derive(Clone, Debug)]
pub(crate) struct LoadedFont {
    pub bytes: Arc<[u8]>,
    pub face_index: u32,
    pub families: Vec<String>,
}

/// What [`Alpha2::conform`] reads.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct ConformRequest {
    /// The project, as project JSON.
    pub project_json: String,
    /// Media file paths in the app's sandbox, by source ID.
    pub assets: HashMap<String, String>,
    /// Not read. Every conform reports whether each source's file opens.
    pub purpose: Option<String>,
}

/// What [`Alpha2::export`] renders.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct ExportRequest {
    /// The project, as project JSON.
    pub project_json: String,
    /// Media file paths in the app's sandbox, by source ID, and effect resource files by
    /// resource ID.
    pub assets: HashMap<String, String>,
    /// Each image sequence's frame files, by source ID, in the sequence's order; an empty path
    /// where the sequence has a gap.
    pub frames: HashMap<String, Vec<String>>,
    /// Files an SVG or Lottie source names outside itself, by source ID and then by the locator
    /// it names them with.
    pub resources: HashMap<String, HashMap<String, String>>,
    /// Where the export is published, inside the app's sandbox.
    pub output_path: String,
    /// Replace a file already at `output_path` instead of refusing.
    pub overwrite: bool,
    /// `mp4`, `mov`, `mkv` or `webm`; from the output's extension when unset.
    pub container: Option<String>,
    /// `h264`, `hevc`, `vp9`, `av1`, `prores` or `mpeg4-part2`; the container's default when
    /// unset.
    pub video_codec: Option<String>,
    /// `aac` or `opus`; the container's default when unset.
    pub audio_codec: Option<String>,
    /// Mix and encode the project's audio.
    pub include_audio: bool,
    /// The first rendered timeline tick; the timeline's start when unset.
    pub start_ticks: Option<i64>,
    /// The tick the export stops before; the timeline's end when unset.
    pub end_ticks: Option<i64>,
}

/// One progress report of an export.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ExportProgressEvent {
    /// Frames encoded so far.
    pub frames_done: u64,
    /// Frames the export encodes in all.
    pub frames_total: u64,
}

/// Why an app's progress callback failed.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ProgressFailure {
    /// The callback failed for `reason`.
    #[error("{reason}")]
    Failed {
        /// Why.
        reason: String,
    },
}

impl From<uniffi::UnexpectedUniFFICallbackError> for ProgressFailure {
    fn from(error: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Failed {
            reason: error.reason,
        }
    }
}

/// Receives an export's progress on a library thread.
#[uniffi::export(with_foreign)]
pub trait ExportProgress: Send + Sync {
    /// Called after frames are encoded; keep it short.
    ///
    /// # Errors
    /// A failure, or anything the callback throws, stops the export with nothing published, and
    /// [`ExportHandle::outcome`] fails with `PROGRESS_CALLBACK_FAILED`.
    fn on_progress(&self, event: ExportProgressEvent) -> Result<(), ProgressFailure>;
}

/// An export's progress callback and its first failure.
pub(crate) struct Progress {
    callback: Arc<dyn ExportProgress>,
    failure: Mutex<Option<String>>,
}

impl Progress {
    pub(crate) fn new(callback: Arc<dyn ExportProgress>) -> Self {
        Self {
            callback,
            failure: Mutex::new(None),
        }
    }

    /// Hands `event` to the callback; a failure is kept and cancels `cancel`.
    pub(crate) fn report(&self, event: ExportProgressEvent, cancel: &CancelToken) {
        if let Err(failure) = self.callback.on_progress(event) {
            self.failure
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_or_insert_with(|| failure.to_string());
            cancel.cancel();
        }
    }

    /// The error the export fails with when the callback failed.
    pub(crate) fn failure(&self) -> Option<Alpha2Error> {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|reason| Alpha2Error::Failed {
                code: "PROGRESS_CALLBACK_FAILED".into(),
                reason: format!("the progress callback failed: {reason}"),
            })
    }
}

/// What a finished export produced.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ExportOutcome {
    /// Where the output was published.
    pub output_path: String,
    /// The export report, as JSON.
    pub report_json: String,
}

impl Alpha2 {
    /// A session on `platform` instead of this device's backend.
    #[cfg(test)]
    pub(crate) fn with_platform(config: EngineConfig, platform: Arc<dyn Platform>) -> Arc<Self> {
        Arc::new(Self {
            config,
            platform,
            fonts: Mutex::new(Vec::new()),
            paused: Arc::new(PauseGate::default()),
            budgets: Mutex::new(Vec::new()),
        })
    }

    /// The configuration the session was made with.
    pub(crate) const fn engine_config(&self) -> &EngineConfig {
        &self.config
    }

    /// The fonts set last.
    pub(crate) fn fonts(&self) -> Vec<LoadedFont> {
        self.fonts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The platform's video decoders, when it has any.
    pub(crate) fn video_backends(
        &self,
    ) -> Option<Arc<dyn alpha2_host_core::video_stream::VideoBackends>> {
        self.platform.video_decoders()
    }

    /// The platform's audio decoders, when it has any.
    pub(crate) fn audio_backends(
        &self,
    ) -> Option<Arc<dyn alpha2_host_core::audio_stream::AudioBackends>> {
        self.platform.audio_decoders()
    }

    /// Which decoders the session's configuration allows.
    pub(crate) const fn decode_preference(
        &self,
    ) -> alpha2_host_core::video_stream::DecodePreference {
        decode_preference(self.config.video_decode)
    }

    /// Whether decoded frames may stay on the GPU.
    pub(crate) fn gpu_frames_allowed(&self) -> bool {
        self.config.video_frames == VideoFrameMode::Auto
    }

    /// The gate [`Alpha2::pause`] closes.
    pub(crate) fn pause_gate(&self) -> Arc<PauseGate> {
        Arc::clone(&self.paused)
    }

    /// A frame budget for new work, a quarter of the memory free now and at most `ceiling`, which
    /// [`Alpha2::memory_warning`] lowers while the work holds it.
    pub(crate) fn frame_budget(&self, ceiling: u64) -> FrameBudget {
        let budget = FrameBudget::of_memory(&alpha2_os::memory_capacity(), ceiling);
        let mut budgets = self
            .budgets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        budgets.retain(FrameBudget::is_shared);
        budgets.push(budget.clone());
        budget
    }
}

/// An app's alpha2 session: its settings, fonts and media backend.
#[derive(uniffi::Object)]
pub struct Alpha2 {
    config: EngineConfig,
    platform: Arc<dyn Platform>,
    fonts: Mutex<Vec<LoadedFont>>,
    paused: Arc<PauseGate>,
    /// The frame budgets of running exports, still runs and previews.
    budgets: Mutex<Vec<FrameBudget>>,
}

#[uniffi::export]
impl Alpha2 {
    /// A session configured by `config`.
    #[uniffi::constructor]
    #[must_use]
    pub fn new(config: EngineConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            platform: platform::current(),
            fonts: Mutex::new(Vec::new()),
            paused: Arc::new(PauseGate::default()),
            budgets: Mutex::new(Vec::new()),
        })
    }

    /// What this device can decode, render and encode, as JSON.
    #[must_use]
    pub fn capabilities(&self) -> String {
        capabilities_json(&self.config, &*self.platform).to_string()
    }

    /// Sets the fonts text may use, in fallback order, and returns each one's family names.
    ///
    /// # Errors
    /// [`Alpha2Error::InvalidInput`] naming a font that does not parse.
    pub fn set_fonts(&self, fonts: Vec<FontFile>) -> Result<Vec<Vec<String>>, Alpha2Error> {
        let mut families = Vec::with_capacity(fonts.len());
        let mut kept = Vec::with_capacity(fonts.len());
        for (index, font) in fonts.into_iter().enumerate() {
            let names = alpha2_text::font_family_names(&font.bytes, font.face_index);
            if names.is_empty() {
                return Err(Alpha2Error::invalid(format!(
                    "font {index} (face {}) has no readable family name",
                    font.face_index
                )));
            }
            kept.push(LoadedFont {
                bytes: Arc::from(font.bytes),
                face_index: font.face_index,
                families: names.clone(),
            });
            families.push(names);
        }
        *self
            .fonts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = kept;
        Ok(families)
    }

    /// Whether each source's file opens, as `{"sources": [{"id", "path", "readable", "reason"}]}`.
    ///
    /// # Errors
    /// [`Alpha2Error::InvalidInput`] for an invalid project or request.
    pub fn conform(&self, request: ConformRequest) -> Result<String, Alpha2Error> {
        let problems = project_problems(&request.project_json);
        if !problems.is_empty() {
            return Err(Alpha2Error::InvalidInput { problems });
        }
        self.platform
            .conform(&request)
            .map(|result| result.to_string())
    }

    /// Renders frames to 8-bit sRGB RGBA PNG files, as an export draws them before encoding,
    /// and returns the stills result JSON. Runs on the calling thread: call it off the main one.
    ///
    /// # Errors
    /// [`Alpha2Error::InvalidInput`] for an invalid project or request, else what rendering or
    /// writing a still met.
    pub fn stills(&self, request: StillsRequest) -> Result<String, Alpha2Error> {
        let problems = project_problems(&request.project_json);
        if !problems.is_empty() {
            return Err(Alpha2Error::InvalidInput { problems });
        }
        if request.stills.is_empty() {
            return Err(Alpha2Error::invalid("stills names no frame"));
        }
        crate::stills::render(
            &request,
            &self.config,
            &self.fonts(),
            self.video_backends(),
            self.decode_preference(),
            &self.frame_budget(DEFAULT_VIDEO_FRAME_BYTES),
        )
        .map(|result| result.to_string())
    }

    /// Starts rendering and encoding the request on a library thread.
    ///
    /// # Errors
    /// [`Alpha2Error::InvalidInput`] for an invalid project or request; everything later is the
    /// handle's [`ExportHandle::outcome`] result.
    pub fn export(
        &self,
        request: ExportRequest,
        progress: Arc<dyn ExportProgress>,
    ) -> Result<Arc<ExportHandle>, Alpha2Error> {
        let problems = project_problems(&request.project_json);
        if !problems.is_empty() {
            return Err(Alpha2Error::InvalidInput { problems });
        }
        if request.output_path.is_empty() {
            return Err(Alpha2Error::invalid("the output path is empty"));
        }
        let handle = Arc::new(ExportHandle {
            job: Job::new("export"),
            held: Arc::new(AtomicBool::new(false)),
        });
        let progress = Arc::new(Progress::new(progress));
        let job = ExportJob {
            request,
            config: self.config.clone(),
            fonts: self.fonts(),
            cancel: handle.job.cancel_token().clone(),
            paused: Arc::clone(&self.paused),
            held: Arc::clone(&handle.held),
            budget: self.frame_budget(DEFAULT_VIDEO_FRAME_BYTES),
            progress: Arc::clone(&progress),
        };
        let platform = Arc::clone(&self.platform);
        handle.job.spawn("alpha2-export", move || {
            let outcome = platform.export(job);
            progress.failure().map_or(outcome, Err)
        })?;
        Ok(handle)
    }

    /// Holds exports at their next frame boundary until [`Self::resume`]; call when the app leaves
    /// the foreground, since iOS does not run GPU work in the background.
    pub fn pause(&self) {
        self.paused.set(true);
    }

    /// Lets paused work continue; call when the app returns to the foreground.
    pub fn resume(&self) {
        self.paused.set(false);
    }

    /// Halves the decoded frames running work keeps, evicting at its next frame; call on a
    /// system memory warning (`didReceiveMemoryWarning` on iOS, `onTrimMemory` on Android).
    /// Work started later sizes itself from the memory free then.
    pub fn memory_warning(&self) {
        let mut budgets = self
            .budgets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        budgets.retain(FrameBudget::is_shared);
        for budget in budgets.iter() {
            budget.halve();
        }
    }
}

/// A running export.
#[derive(uniffi::Object)]
pub struct ExportHandle {
    job: Arc<Job<ExportOutcome>>,
    held: Arc<AtomicBool>,
}

#[uniffi::export]
impl ExportHandle {
    /// Stops the export; [`Self::outcome`] then fails with [`Alpha2Error::Aborted`] and no output is
    /// left behind.
    pub fn cancel(&self) {
        self.job.cancel_token().cancel();
    }

    /// Whether the export is held at a frame boundary by [`Alpha2::pause`].
    #[must_use]
    pub fn is_held(&self) -> bool {
        self.held.load(Ordering::Acquire)
    }

    /// Whether the export has finished, succeeded or not.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.job.is_finished()
    }

    /// The export's outcome once it finishes.
    ///
    /// # Errors
    /// The export's failure; [`Alpha2Error::Aborted`] after [`Self::cancel`];
    /// `PROGRESS_CALLBACK_FAILED` when the progress callback failed; `INTERNAL` when the export
    /// thread panicked; [`Alpha2Error::InvalidInput`] when the outcome was already taken.
    pub async fn outcome(&self) -> Result<ExportOutcome, Alpha2Error> {
        self.job.outcome().await
    }
}

/// Whether the app is in the background; work waits at frame boundaries while set.
#[derive(Default)]
pub(crate) struct PauseGate {
    paused: Mutex<bool>,
    changed: Condvar,
}

impl PauseGate {
    fn set(&self, paused: bool) {
        *self
            .paused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = paused;
        self.changed.notify_all();
    }

    /// Whether work is held now.
    pub(crate) fn is_paused(&self) -> bool {
        *self
            .paused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Blocks while paused, returning early on cancellation; `held` is set while it blocks.
    pub(crate) fn wait_while_paused(&self, cancel: &CancelToken, held: &AtomicBool) {
        let mut paused = self
            .paused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held.store(*paused && !cancel.is_cancelled(), Ordering::Release);
        while *paused && !cancel.is_cancelled() {
            paused = self
                .changed
                .wait_timeout(paused, std::time::Duration::from_millis(100))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        held.store(false, Ordering::Release);
    }
}

/// The decode preference `mode` asks for.
pub(crate) const fn decode_preference(
    mode: VideoDecodeMode,
) -> alpha2_host_core::video_stream::DecodePreference {
    use alpha2_host_core::video_stream::DecodePreference;
    match mode {
        VideoDecodeMode::Auto => DecodePreference::Auto,
        VideoDecodeMode::Hardware => DecodePreference::Hardware,
        VideoDecodeMode::Software => DecodePreference::Software,
    }
}

/// Why `project_json` does not load as a project; empty when it does.
pub(crate) fn project_problems(project_json: &str) -> Vec<String> {
    match alpha2_model::Project::from_json(project_json) {
        Ok(_) => Vec::new(),
        Err(error) => vec![error.to_string()],
    }
}

fn capabilities_json(config: &EngineConfig, platform: &dyn Platform) -> Value {
    let gpu = match config.renderer {
        RendererMode::Cpu => Value::Null,
        RendererMode::Auto | RendererMode::Gpu => {
            match pollster::block_on(alpha2_gpu::GpuContext::request_product()) {
                Ok(gpu) => {
                    let capabilities = gpu.capabilities();
                    json!({
                        "adapter": capabilities.adapter_name,
                        "backend": capabilities.backend,
                        "driver": capabilities.driver,
                        "softwareRasterizer": capabilities.software_rasterizer,
                    })
                }
                Err(error) => json!({ "unavailable": error.to_string() }),
            }
        }
    };
    let renderer = match (config.renderer, gpu.get("adapter").is_some()) {
        (RendererMode::Cpu, _) | (RendererMode::Auto, false) => "cpu",
        _ => "gpu",
    };
    let extensions = alpha2_extensions::Registry::with_defaults().names();
    let mut features = serde_json::Map::new();
    for format in ["svg", "animated-image", "lottie", "exr", "pdf"] {
        features.insert(
            format!("extensions.{format}"),
            json!({ "available": extensions.contains(&format) }),
        );
    }
    features.insert(
        "extensions.image-sequence".into(),
        json!({ "available": alpha2_extensions::Registry::reads_image_sequences() }),
    );
    features.insert(
        "extensions.latex".into(),
        json!({ "available": alpha2_extensions::Registry::reads_latex() }),
    );
    json!({
        "hostVersion": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "osVersion": alpha2_os::os_version(),
        "arch": std::env::consts::ARCH,
        "renderer": renderer,
        "gpu": gpu,
        "features": features,
        "media": platform.media_capabilities(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    struct Quiet;

    impl ExportProgress for Quiet {
        fn on_progress(&self, _event: ExportProgressEvent) -> Result<(), ProgressFailure> {
            Ok(())
        }
    }

    /// A file of the repository's `tests/`, or of `ALPHA2_TESTS_DIR` where a device runs the
    /// tests from a copy.
    fn tests_file(name: &str) -> std::path::PathBuf {
        std::env::var_os("ALPHA2_TESTS_DIR")
            .map_or_else(
                || std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests"),
                std::path::PathBuf::from,
            )
            .join(name)
    }

    fn project() -> String {
        std::fs::read_to_string(tests_file("cases/host-parity-layout-project.json"))
            .expect("project")
    }

    fn cpu() -> Arc<Alpha2> {
        Alpha2::new(EngineConfig {
            renderer: RendererMode::Cpu,
            video_decode: VideoDecodeMode::Auto,
            video_frames: VideoFrameMode::Auto,
        })
    }

    #[test]
    fn a_project_that_does_not_load_is_refused_with_its_problems() {
        assert_eq!(project_problems(&project()), Vec::<String>::new());
        let request = ExportRequest {
            project_json: "{}".into(),
            output_path: "out.mp4".into(),
            ..ExportRequest::default()
        };
        let refused = cpu().export(request, Arc::new(Quiet)).err();
        assert!(
            matches!(&refused, Some(Alpha2Error::InvalidInput { problems }) if !problems.is_empty()),
            "{refused:?}"
        );
    }

    #[test]
    fn fonts_answer_with_their_families_and_unreadable_ones_are_refused() {
        let bytes = std::fs::read(tests_file("fonts/Alpha2Fixture.ttf")).expect("font");
        let engine = cpu();
        let families = engine
            .set_fonts(vec![FontFile {
                bytes,
                face_index: 0,
            }])
            .expect("fonts");
        assert_ne!(families[0], Vec::<String>::new());
        let refused = engine.set_fonts(vec![FontFile {
            bytes: vec![1, 2, 3],
            face_index: 0,
        }]);
        assert_eq!(
            refused.map_err(|error| error.code().to_owned()),
            Err("INVALID_INPUT".into())
        );
    }

    #[test]
    fn a_cancelled_export_is_aborted_and_its_outcome_is_taken_once() {
        let engine = cpu();
        engine.pause();
        let request = ExportRequest {
            project_json: project(),
            output_path: std::env::temp_dir()
                .join("alpha2-mobile-cancelled.mp4")
                .to_string_lossy()
                .into_owned(),
            ..ExportRequest::default()
        };
        let handle = engine.export(request, Arc::new(Quiet)).expect("started");
        handle.cancel();
        let outcome = pollster::block_on(handle.outcome());
        assert!(matches!(outcome, Err(Alpha2Error::Aborted)), "{outcome:?}");
        assert!(handle.is_finished());
        let again = pollster::block_on(handle.outcome());
        assert_eq!(
            again.map_err(|error| error.code().to_owned()),
            Err("INVALID_INPUT".into())
        );
        engine.resume();
    }

    #[test]
    fn stills_are_png_files_of_the_frames_asked_for() {
        let directory =
            std::env::temp_dir().join(format!("alpha2-mobile-stills-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("directory");
        let output = |name: &str| directory.join(name).to_string_lossy().into_owned();
        let request = StillsRequest {
            project_json: project(),
            assets: HashMap::from([(
                "still".to_owned(),
                tests_file("corpus/parity-image.png")
                    .to_string_lossy()
                    .into_owned(),
            )]),
            stills: vec![
                StillEntry {
                    time_ticks: 60_000,
                    output_path: output("late.png"),
                },
                StillEntry {
                    time_ticks: 0,
                    output_path: output("first.png"),
                },
            ],
            overwrite: true,
            ..StillsRequest::default()
        };
        let engine = cpu();
        engine
            .set_fonts(vec![FontFile {
                bytes: std::fs::read(tests_file("fonts/Alpha2Fixture.ttf")).expect("font"),
                face_index: 0,
            }])
            .expect("fonts");
        let result: Value =
            serde_json::from_str(&engine.stills(request).expect("stills")).expect("JSON");
        let stills = result["stills"].as_array().expect("stills");
        assert_eq!(stills[0]["frameTime"], 60_000);
        assert_eq!(stills[1]["frameTime"], 0);
        for name in ["late.png", "first.png"] {
            let decoded = alpha2_host_core::stills::decode_still(
                &std::fs::read(directory.join(name)).expect("png"),
            )
            .expect("decodes");
            assert_eq!(
                (decoded.info.width, decoded.info.height),
                (192, 108),
                "{name}"
            );
        }
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn a_memory_warning_halves_the_budgets_of_running_work_only() {
        let engine = cpu();
        let running = engine.frame_budget(1 << 30);
        let before = running.bytes();
        drop(engine.frame_budget(1 << 30));
        engine.memory_warning();
        assert_eq!(
            running.bytes(),
            (before / 2).max(alpha2_host_core::export::MIN_FRAME_BYTES)
        );
        let held = engine
            .budgets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        assert_eq!(held, 1, "ended work's budget is let go");
    }

    /// A backend whose export panics.
    struct Panics;

    impl Platform for Panics {
        fn media_capabilities(&self) -> Value {
            Value::Null
        }

        fn conform(&self, _request: &ConformRequest) -> Result<Value, Alpha2Error> {
            Ok(Value::Null)
        }

        fn export(&self, _job: ExportJob) -> Result<ExportOutcome, Alpha2Error> {
            panic!("the backend broke");
        }
    }

    #[test]
    fn a_panicking_export_settles_its_outcome_as_internal() {
        let engine = Alpha2::with_platform(EngineConfig::default(), Arc::new(Panics));
        let request = ExportRequest {
            project_json: project(),
            output_path: "unused.mp4".into(),
            ..ExportRequest::default()
        };
        let handle = engine.export(request, Arc::new(Quiet)).expect("started");
        let (done, wait) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(pollster::block_on(handle.outcome()));
        });
        let outcome = wait
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the outcome settles");
        match outcome {
            Err(Alpha2Error::Failed { code, reason }) => {
                assert_eq!(code, "INTERNAL");
                assert!(reason.contains("the backend broke"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// Encodes each frame into a one-byte packet at once and decodes nothing.
    mod fake_media {
        use alpha2_container::TrackInfo;
        use alpha2_host_core::encode::{
            AudioEncodeBackend, AudioEncodeConfig, AudioEncoders, EncodePoll, EncodedPacket,
            PlaneBuffer, VideoEncodeBackend, VideoEncodeConfig, VideoEncoders,
        };
        use alpha2_host_core::rate_control::AppliedRateControl;
        use alpha2_host_core::video_stream::{
            BackendError, BackendKind, Submit, VideoBackends, VideoDecodeBackend,
        };
        use alpha2_media::EncodeError;
        use bytes::Bytes;

        pub struct Media;

        impl VideoBackends for Media {
            fn open(
                &self,
                _track: &TrackInfo,
                _kind: BackendKind,
            ) -> Result<Box<dyn VideoDecodeBackend>, BackendError> {
                Err(BackendError::Unsupported("no decoders".into()))
            }
        }

        impl alpha2_host_core::audio_stream::AudioBackends for Media {
            fn open(
                &self,
                _track: &TrackInfo,
            ) -> Result<Box<dyn alpha2_host_core::audio_stream::AudioDecodeBackend>, BackendError>
            {
                Err(BackendError::Unsupported("no decoders".into()))
            }
        }

        impl AudioEncoders for Media {
            fn open(
                &self,
                _config: &AudioEncodeConfig,
            ) -> Result<Box<dyn AudioEncodeBackend>, EncodeError> {
                Err(EncodeError::Adapter("no audio encoder".into()))
            }
        }

        impl VideoEncoders for Media {
            fn open(
                &self,
                _config: &VideoEncodeConfig,
            ) -> Result<Box<dyn VideoEncodeBackend>, EncodeError> {
                Ok(Box::new(Video::default()))
            }
        }

        #[derive(Default)]
        struct Video {
            queued: std::collections::VecDeque<i64>,
            ended: bool,
            given: bool,
        }

        impl VideoEncodeBackend for Video {
            fn name(&self) -> &'static str {
                "fake"
            }

            fn hardware(&self) -> bool {
                false
            }

            fn timescale(&self) -> u32 {
                90_000
            }

            fn submit(
                &mut self,
                pts: i64,
                _planes: &[PlaneBuffer],
                _keyframe: bool,
            ) -> Result<Submit, EncodeError> {
                self.queued.push_back(pts);
                Ok(Submit::Accepted)
            }

            fn receive(&mut self, _wait: bool) -> Result<EncodePoll, EncodeError> {
                Ok(match self.queued.pop_front() {
                    Some(pts) => {
                        self.given = true;
                        EncodePoll::Packet(EncodedPacket {
                            pts,
                            dts: pts,
                            keyframe: true,
                            data: Bytes::from_static(&[1]),
                        })
                    }
                    None if self.ended => EncodePoll::Drained,
                    None => EncodePoll::Pending,
                })
            }

            fn finish_input(&mut self) -> Result<(), EncodeError> {
                self.ended = true;
                Ok(())
            }

            fn codec_private(&self) -> Option<Bytes> {
                self.given.then(|| Bytes::from_static(&[1, 0, 0, 0]))
            }

            fn applied_rate_control(&self) -> AppliedRateControl {
                AppliedRateControl::Bitrate
            }
        }
    }

    fn fake_media() -> Arc<Alpha2> {
        let media = Arc::new(fake_media::Media);
        let platform = crate::platform::MediaPlatform {
            name: "test",
            media: crate::export::MediaBackends {
                video_decoders: media.clone(),
                audio_decoders: media.clone(),
                video_encoders: media.clone(),
                audio_encoders: media,
            },
            video_codecs: Vec::new,
            audio_codecs: Vec::new,
        };
        let engine = Alpha2::with_platform(
            EngineConfig {
                renderer: RendererMode::Cpu,
                ..EngineConfig::default()
            },
            Arc::new(platform),
        );
        engine
            .set_fonts(vec![FontFile {
                bytes: std::fs::read(tests_file("fonts/Alpha2Fixture.ttf")).expect("font"),
                face_index: 0,
            }])
            .expect("fonts");
        engine
    }

    /// Fails the report of frame `at`, counting from 1.
    struct FailsAt(u64);

    impl ExportProgress for FailsAt {
        fn on_progress(&self, event: ExportProgressEvent) -> Result<(), ProgressFailure> {
            if event.frames_done == self.0.min(event.frames_total) {
                return Err(ProgressFailure::Failed {
                    reason: "the app broke".into(),
                });
            }
            Ok(())
        }
    }

    fn export_with(
        progress: Arc<dyn ExportProgress>,
        name: &str,
    ) -> (PathBuf, Result<ExportOutcome, Alpha2Error>) {
        let output =
            std::env::temp_dir().join(format!("alpha2-mobile-{}-{name}.mkv", std::process::id()));
        let _ = std::fs::remove_file(&output);
        let request = ExportRequest {
            project_json: project(),
            assets: HashMap::from([(
                "still".to_owned(),
                tests_file("corpus/parity-image.png")
                    .to_string_lossy()
                    .into_owned(),
            )]),
            output_path: output.to_string_lossy().into_owned(),
            video_codec: Some("vp9".into()),
            ..ExportRequest::default()
        };
        let handle = fake_media().export(request, progress).expect("started");
        let (done, wait) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(pollster::block_on(handle.outcome()));
        });
        let outcome = wait
            .recv_timeout(std::time::Duration::from_secs(120))
            .expect("the outcome settles");
        (output, outcome)
    }

    #[test]
    fn an_export_with_a_quiet_callback_publishes_its_file() {
        let (output, outcome) = export_with(Arc::new(Quiet), "quiet");
        outcome.expect("exported");
        assert!(output.exists());
        let _ = std::fs::remove_file(output);
    }

    #[test]
    fn a_failing_progress_callback_fails_the_export_and_publishes_nothing() {
        for (at, name) in [(1, "first"), (u64::MAX, "last")] {
            let (output, outcome) = export_with(Arc::new(FailsAt(at)), name);
            match outcome {
                Err(Alpha2Error::Failed { code, reason }) => {
                    assert_eq!(code, "PROGRESS_CALLBACK_FAILED", "{name}");
                    assert!(reason.contains("progress callback"), "{reason}");
                    assert!(reason.contains("the app broke"), "{reason}");
                }
                other => panic!("{name}: {other:?}"),
            }
            assert!(!output.exists(), "{name}: no file is published");
        }
    }

    #[test]
    fn capabilities_name_the_renderer_and_extensions() {
        let report: Value = serde_json::from_str(&cpu().capabilities()).expect("JSON");
        assert_eq!(report["renderer"], "cpu");
        assert_eq!(report["features"]["extensions.latex"]["available"], true);
        assert!(report["media"].is_object());
    }
}
