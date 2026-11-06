//! The engine session apps hold, and its requests and results.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use alpha2_media::CancelToken;
use serde_json::{Value, json};

use crate::error::Alpha2Error;
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
    /// Which renderer draws frames.
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
    /// `export_original` (the default) or `preview`.
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

/// Receives an export's progress on a library thread.
#[uniffi::export(with_foreign)]
pub trait ExportProgress: Send + Sync {
    /// Called after frames are encoded; keep it short.
    fn on_progress(&self, event: ExportProgressEvent);
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
        match self.config.video_decode {
            VideoDecodeMode::Auto => alpha2_host_core::video_stream::DecodePreference::Auto,
            VideoDecodeMode::Hardware => alpha2_host_core::video_stream::DecodePreference::Hardware,
            VideoDecodeMode::Software => alpha2_host_core::video_stream::DecodePreference::Software,
        }
    }

    /// Whether decoded frames may stay on the GPU.
    pub(crate) fn gpu_frames_allowed(&self) -> bool {
        self.config.video_frames == VideoFrameMode::Auto
    }

    /// The gate [`Alpha2::pause`] closes.
    pub(crate) fn pause_gate(&self) -> Arc<PauseGate> {
        Arc::clone(&self.paused)
    }
}

/// An app's alpha2 session: its renderer, fonts and media backend.
#[derive(uniffi::Object)]
pub struct Alpha2 {
    config: EngineConfig,
    platform: Arc<dyn Platform>,
    fonts: Mutex<Vec<LoadedFont>>,
    paused: Arc<PauseGate>,
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

    /// Which media each source would read for the request's purpose, and what blocks it, as the
    /// conform result JSON.
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
        let handle = Arc::new(ExportHandle::default());
        let job = ExportJob {
            request,
            config: self.config.clone(),
            fonts: self
                .fonts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            cancel: handle.cancel.clone(),
            paused: Arc::clone(&self.paused),
            progress,
        };
        let platform = Arc::clone(&self.platform);
        let finished = Arc::clone(&handle);
        std::thread::Builder::new()
            .name("alpha2-export".into())
            .spawn(move || {
                let outcome = platform.export(job);
                finished.finish(outcome);
            })
            .map_err(|error| Alpha2Error::Failed {
                code: "RESOURCE_LIMIT".into(),
                reason: format!("cannot start the export thread: {error}"),
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
}

/// A running export.
#[derive(uniffi::Object, Default)]
pub struct ExportHandle {
    cancel: CancelToken,
    outcome: Mutex<Option<Result<ExportOutcome, Alpha2Error>>>,
    done: Condvar,
    taken: AtomicBool,
}

#[uniffi::export]
impl ExportHandle {
    /// Stops the export; [`Self::outcome`] then fails with [`Alpha2Error::Aborted`] and no output is
    /// left behind.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Whether the export has finished, succeeded or not.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
            || self.taken.load(Ordering::Acquire)
    }

    /// The export's outcome once it finishes.
    ///
    /// # Errors
    /// The export's failure; [`Alpha2Error::Aborted`] after [`Self::cancel`];
    /// [`Alpha2Error::InvalidInput`] when the outcome was already taken.
    pub async fn outcome(&self) -> Result<ExportOutcome, Alpha2Error> {
        let outcome = WaitOutcome { handle: self }.await;
        self.taken.store(true, Ordering::Release);
        outcome
    }
}

impl ExportHandle {
    fn finish(&self, outcome: Result<ExportOutcome, Alpha2Error>) {
        *self
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(outcome);
        self.done.notify_all();
        for waker in WAKERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            waker.wake();
        }
    }
}

/// Wakers of pending [`ExportHandle::outcome`] futures, woken when any export finishes.
static WAKERS: Mutex<Vec<std::task::Waker>> = Mutex::new(Vec::new());

struct WaitOutcome<'a> {
    handle: &'a ExportHandle,
}

impl std::future::Future for WaitOutcome<'_> {
    type Output = Result<ExportOutcome, Alpha2Error>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut wakers = WAKERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut outcome = self
            .handle
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(outcome) = outcome.take() {
            return std::task::Poll::Ready(outcome);
        }
        if self.handle.taken.load(Ordering::Acquire) {
            return std::task::Poll::Ready(Err(Alpha2Error::invalid(
                "this export's outcome was already taken",
            )));
        }
        wakers.push(context.waker().clone());
        std::task::Poll::Pending
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

    /// Blocks while paused, returning early on cancellation.
    pub(crate) fn wait_while_paused(&self, cancel: &CancelToken) {
        let mut paused = self
            .paused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *paused && !cancel.is_cancelled() {
            paused = self
                .changed
                .wait_timeout(paused, std::time::Duration::from_millis(100))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
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
    use super::*;

    struct Quiet;

    impl ExportProgress for Quiet {
        fn on_progress(&self, _event: ExportProgressEvent) {}
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
    fn capabilities_name_the_renderer_and_extensions() {
        let report: Value = serde_json::from_str(&cpu().capabilities()).expect("JSON");
        assert_eq!(report["renderer"], "cpu");
        assert_eq!(report["features"]["extensions.latex"]["available"], true);
        assert!(report["media"].is_object());
    }
}
