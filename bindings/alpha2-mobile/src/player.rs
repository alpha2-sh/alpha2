//! Real-time preview of a project on a window, decoded, rendered and presented on a library thread.
//!
//! Calls return at once and run on that thread in order. A seek or resize that a later one of its
//! kind replaces before the thread reaches it is skipped, and a newer seek stops the frame an older
//! one draws. Nothing is drawn without an attached window or while the session is paused for the
//! background. A window, resize or seek sent while paused takes effect, with one frame drawn, when
//! the session resumes.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use alpha2_engine::{
    EngineProject, FramesWithText, Player as FramePlayer, QualityProfile, TextTextures,
};
use alpha2_host_core::audio_stream::AudioBackends;
use alpha2_host_core::export::{
    ExportRenderer, FrameBudget, SourceFrameCache, SourceStream, StreamSources,
};
use alpha2_host_core::preview_audio::PreviewAudio;
use alpha2_host_core::providers::ProviderMedia;
use alpha2_host_core::video_stream::{DecodePreference, VideoBackends};
use alpha2_media::{CancelToken, SourceId};
use alpha2_model::Project;
use alpha2_render::{RenderedTexture, Renderer, SurfacePresenter};
use alpha2_time::MediaTime;

use crate::error::Alpha2Error;
use crate::session::{Alpha2, LoadedFont, PauseGate};
use crate::surface::NativeWindow;
use crate::zero_copy::GpuFrames;

/// How [`Player::last_error`] begins when the GPU refuses the attached window.
pub(crate) const PRESENT_REFUSED: &str = "the GPU cannot present to this window";

/// How often a preview held in the background with a frame to draw checks for the app's return.
const PAUSED_POLL: Duration = Duration::from_millis(50);

/// How long the heard audio may stand still, with samples waiting for the device, before the
/// preview takes the output for stopped and keeps time by the wall clock.
const AUDIO_STALL: Duration = Duration::from_millis(500);

/// Most bytes of decoded audio a preview's mix holds at once.
const PREVIEW_AUDIO_BYTES: u64 = 64 << 20;

/// Most bytes of decoded source frames a preview keeps.
const PREVIEW_FRAME_BYTES: u64 = 256 << 20;

/// Bytes the open extension sources of a preview may hold.
const PREVIEW_PROVIDER_BYTES: u64 = 256 << 20;

/// What [`Player::new`] previews.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct PlayerRequest {
    /// The project, as project JSON.
    pub project_json: String,
    /// Media file paths in the app's sandbox, by source ID, and effect resource files by
    /// resource ID.
    pub assets: HashMap<String, String>,
    /// Each image sequence's frame files, by source ID; an empty path where it has a gap.
    pub frames: HashMap<String, Vec<String>>,
    /// Files an SVG or Lottie source names outside itself, by source ID and locator.
    pub resources: HashMap<String, HashMap<String, String>>,
}

enum Command {
    Attach {
        window: NativeWindow,
        width: u32,
        height: u32,
    },
    Detach,
    Resize {
        width: u32,
        height: u32,
    },
    /// A position, and the token the next seek cancels.
    Seek(i64, CancelToken),
    Play,
    Pause,
}

/// The commands that leave the same end state as `commands` applied in order: every seek but the
/// last and every resize but the last are dropped, the rest keep their order. A seek only sets the
/// position and draws, and a resize only sets the size and draws.
fn coalesce(commands: Vec<Command>) -> Vec<Command> {
    let last_seek = commands
        .iter()
        .rposition(|command| matches!(command, Command::Seek(..)));
    let last_resize = commands
        .iter()
        .rposition(|command| matches!(command, Command::Resize { .. }));
    commands
        .into_iter()
        .enumerate()
        .filter(|(index, command)| match command {
            Command::Seek(..) => Some(*index) == last_seek,
            Command::Resize { .. } => Some(*index) == last_resize,
            _ => true,
        })
        .map(|(_, command)| command)
        .collect()
}

/// What the preview thread reports back, and the tokens that stop its work.
#[derive(Default)]
struct Shared {
    position: AtomicI64,
    playing: AtomicBool,
    sounding: AtomicBool,
    presented: AtomicU64,
    dropped: AtomicU64,
    error: Mutex<Option<String>>,
    /// Set when the player ends: work stops at its next check and no further command runs.
    stop: CancelToken,
    /// The newest seek's token, cancelled by the seek after it.
    seek: Mutex<CancelToken>,
}

/// A project's preview on an app window, drawn on a GPU device that presents to it whatever the
/// session's [`crate::RendererMode`].
#[derive(uniffi::Object)]
pub struct Player {
    commands: Mutex<Option<Sender<Command>>>,
    shared: Arc<Shared>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

#[uniffi::export]
impl Player {
    /// A preview of `request`, with `engine`'s fonts, decoders and lifecycle.
    ///
    /// # Errors
    /// [`Alpha2Error::InvalidInput`] for a project that does not load.
    #[uniffi::constructor]
    pub fn new(engine: Arc<Alpha2>, request: PlayerRequest) -> Result<Arc<Self>, Alpha2Error> {
        let project = Project::from_json(&request.project_json)
            .map_err(|error| Alpha2Error::invalid(error.to_string()))?;
        let snapshot = EngineProject::new(project.clone())
            .map_err(|error| Alpha2Error::invalid(error.to_string()))?;
        let session = Session {
            project,
            snapshot,
            request,
            fonts: engine.fonts(),
            video: engine.video_backends(),
            audio: engine.audio_backends(),
            preference: engine.decode_preference(),
            gpu_frames: engine.gpu_frames_allowed(),
            paused: engine.pause_gate(),
            budget: engine.frame_budget(PREVIEW_FRAME_BYTES),
        };
        let (commands, receive) = mpsc::channel();
        let shared = Arc::new(Shared::default());
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("alpha2-preview".into())
            .spawn(move || contained(&thread_shared, || run(session, &receive, &thread_shared)))
            .map_err(|error| Alpha2Error::Failed {
                details: None,
                code: "RESOURCE_LIMIT".into(),
                reason: format!("cannot start the preview thread: {error}"),
            })?;
        Ok(Arc::new(Self {
            commands: Mutex::new(Some(commands)),
            shared,
            thread: Mutex::new(Some(thread)),
        }))
    }

    /// Draws into the window `handle` of `width` x `height` pixels, replacing any attached one.
    /// Apps call `Alpha2Android.attach` (Kotlin) or `Player.attach(layer:)` (Swift) instead. The
    /// handle is an `ANativeWindow` whose reference the player takes over, or a `CAMetalLayer`,
    /// which the player retains before this returns.
    ///
    /// # Errors
    /// [`Alpha2Error::InvalidInput`] for a null handle or one of another platform.
    pub fn attach_window(&self, handle: u64, width: u32, height: u32) -> Result<(), Alpha2Error> {
        let window = NativeWindow::from_handle(handle).ok_or_else(|| {
            Alpha2Error::invalid("the window handle is null or of another platform")
        })?;
        self.send(Command::Attach {
            window,
            width,
            height,
        });
        Ok(())
    }

    /// Stops drawing and lets the window go; call before the app's surface is destroyed.
    pub fn detach_window(&self) {
        self.send(Command::Detach);
    }

    /// The attached window's new size.
    pub fn resize(&self, width: u32, height: u32) {
        self.send(Command::Resize { width, height });
    }

    /// Moves to timeline tick `ticks` and shows the frame there.
    pub fn seek(&self, ticks: i64) {
        let token = CancelToken::default();
        let replaced = std::mem::replace(
            &mut *self
                .shared
                .seek
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            token.clone(),
        );
        replaced.cancel();
        self.send(Command::Seek(ticks, token));
    }

    /// Plays from the position at the project's rate.
    pub fn play(&self) {
        self.send(Command::Play);
    }

    /// Stops at the frame shown.
    pub fn pause(&self) {
        self.send(Command::Pause);
    }

    /// The timeline tick of the frame shown last.
    pub fn position_ticks(&self) -> i64 {
        self.shared.position.load(Ordering::Acquire)
    }

    /// Whether the preview is playing.
    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Acquire)
    }

    /// Whether the project's audio plays with the picture: false for a silent project, when no
    /// audio output opens, and while paused.
    pub fn is_sounding(&self) -> bool {
        self.shared.sounding.load(Ordering::Acquire)
    }

    /// How many frames were presented so far.
    pub fn frames_presented(&self) -> u64 {
        self.shared.presented.load(Ordering::Acquire)
    }

    /// Frames not presented: playing skipped them because they came due before the previous one
    /// was drawn (a device too slow for the project's rate), or the window did not take them.
    /// With [`Self::frames_presented`] they account for every frame shown.
    pub fn frames_dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Acquire)
    }

    /// The last failure, which a later successful frame does not clear.
    pub fn last_error(&self) -> Option<String> {
        self.shared
            .error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Ends the preview and waits for its thread, which stops the frame or audio decode it is
    /// working on and skips the commands still queued; the player does nothing afterwards.
    /// Dropping the player (Kotlin `close()`, Swift release) ends it too.
    pub fn end(&self) {
        self.shared.stop.cancel();
        self.shared
            .seek
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cancel();
        drop(
            self.commands
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        if let Some(thread) = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = thread.join();
        }
    }
}

impl Player {
    fn send(&self, command: Command) {
        if let Some(commands) = self
            .commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            let _ = commands.send(command);
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.end();
    }
}

/// What the preview thread renders.
struct Session {
    project: Project,
    snapshot: EngineProject,
    request: PlayerRequest,
    fonts: Vec<LoadedFont>,
    video: Option<Arc<dyn VideoBackends>>,
    audio: Option<Arc<dyn AudioBackends>>,
    preference: DecodePreference,
    /// Whether decoded frames may stay on the GPU.
    gpu_frames: bool,
    paused: Arc<PauseGate>,
    budget: FrameBudget,
}

/// The GPU and the window it presents to.
struct Output {
    renderer: Renderer,
    gpu: alpha2_gpu::GpuContext,
    /// Decoders and import for frames that stay on the GPU, when the device imports them.
    frames: Option<GpuFrames>,
    instance: wgpu::Instance,
    presenter: SurfacePresenter,
    surface: Option<(
        wgpu::Surface<'static>,
        wgpu::SurfaceConfiguration,
        NativeWindow,
    )>,
}

/// Configures `surface`, or says why the device cannot present to it.
fn configure(
    gpu: &alpha2_gpu::GpuContext,
    surface: &wgpu::Surface<'_>,
    config: &wgpu::SurfaceConfiguration,
) -> Result<(), String> {
    let validation = gpu.device().push_error_scope(wgpu::ErrorFilter::Validation);
    surface.configure(gpu.device(), config);
    match pollster::block_on(validation.pop()) {
        Some(error) => Err(format!("{PRESENT_REFUSED}: {error}")),
        None => Ok(()),
    }
}

impl Output {
    /// Opens a GPU device that presents to `window`; GPU errors no call returns go to `errors`.
    fn open(
        window: NativeWindow,
        (width, height): (u32, u32),
        gpu_frames: bool,
        errors: Arc<Shared>,
    ) -> Result<Self, String> {
        let backends = if cfg!(target_vendor = "apple") {
            wgpu::Backends::METAL
        } else {
            wgpu::Backends::VULKAN | wgpu::Backends::GL
        };
        let instance = crate::surface::window_instance(backends);
        let surface = window.surface(&instance)?;
        let gpu = pollster::block_on(alpha2_gpu::GpuContext::request_compatible(
            &instance,
            Some(&surface),
        ))
        .map_err(|error| error.to_string())?;
        let (gpu, frames) = crate::zero_copy::open(gpu, gpu_frames);
        // A window that goes away or a driver that refuses it reports here, not as a crash.
        gpu.set_diagnostic_handler(move |diagnostic| record(&errors, diagnostic.to_string()));
        let config = gpu
            .surface_config(&surface, width.max(1), height.max(1))
            .ok_or_else(|| PRESENT_REFUSED.to_owned())?;
        configure(&gpu, &surface, &config)?;
        let presenter = SurfacePresenter::new(gpu.device(), config.format);
        let mut renderer = Renderer::new(gpu.clone());
        renderer.register_builtins().map_err(|diagnostics| {
            format!("built-in effects failed to compile: {diagnostics:?}")
        })?;
        Ok(Self {
            renderer,
            gpu,
            frames,
            instance,
            presenter,
            surface: Some((surface, config, window)),
        })
    }

    /// Presents to `window` from now on, on the same device.
    fn attach(&mut self, window: NativeWindow, width: u32, height: u32) -> Result<(), String> {
        self.surface = None;
        let surface = window.surface(&self.instance)?;
        let config = self
            .gpu
            .surface_config(&surface, width.max(1), height.max(1))
            .ok_or_else(|| PRESENT_REFUSED.to_owned())?;
        if config.format != self.presenter_format() {
            self.presenter = SurfacePresenter::new(self.gpu.device(), config.format);
        }
        configure(&self.gpu, &surface, &config)?;
        self.surface = Some((surface, config, window));
        Ok(())
    }

    fn presenter_format(&self) -> wgpu::TextureFormat {
        self.surface
            .as_ref()
            .map_or(wgpu::TextureFormat::Bgra8Unorm, |(_, config, _)| {
                config.format
            })
    }

    fn resize(&mut self, width: u32, height: u32) {
        if let Some((surface, config, _)) = self.surface.as_mut() {
            config.width = width.max(1);
            config.height = height.max(1);
            surface.configure(self.gpu.device(), config);
        }
    }

    /// Draws `texture` on the window; `false` when it cannot take a frame now.
    fn present(&self, texture: &RenderedTexture) -> Result<bool, String> {
        let Some((surface, config, _)) = self.surface.as_ref() else {
            return Ok(false);
        };
        let current = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture)
            | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                surface.configure(self.gpu.device(), config);
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err("the window surface is invalid".into());
            }
        };
        let target = current
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.presenter
            .draw(self.gpu.device(), &self.gpu, texture.view(), &target);
        drop(target);
        self.gpu.present(current);
        Ok(true)
    }
}

/// Carries out `first` and the commands queued behind it, coalesced. Returns `false` once ended.
fn apply_queued(
    first: Command,
    commands: &Receiver<Command>,
    state: &mut State,
    session: &Session,
    shared: &Arc<Shared>,
) -> bool {
    let mut queued = vec![first];
    queued.extend(commands.try_iter());
    for command in coalesce(queued) {
        if shared.stop.is_cancelled() {
            return false;
        }
        state.apply(command, session, shared);
    }
    !shared.stop.is_cancelled()
}

/// The preview thread: carries out commands, and renders while playing.
fn run(session: Session, commands: &Receiver<Command>, shared: &Arc<Shared>) {
    let mut state = State::new(&session, shared);
    let frame_length = frame_length(&session);
    loop {
        if shared.stop.is_cancelled() {
            return;
        }
        let paused = session.paused.is_paused();
        if paused {
            state.hold(shared);
        } else {
            state.resume(&session, shared);
        }
        // In the background nothing touches the GPU, but commands are still carried out: an end
        // or a detach must not wait for the app to return to the foreground.
        let wait = if paused {
            (state.deferred.is_some() || state.held).then_some(PAUSED_POLL)
        } else {
            (state.playing() && state.output.is_some()).then_some(Duration::ZERO)
        };
        let command = if let Some(wait) = wait {
            match commands.recv_timeout(wait) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            // Idle until a command: decoder surfaces whose frames are done go back now.
            if let Some(frames) = state
                .output
                .as_ref()
                .and_then(|output| output.frames.as_ref())
            {
                frames.import.collect();
            }
            let Ok(command) = commands.recv() else {
                return;
            };
            Some(command)
        };
        if let Some(command) = command {
            if !apply_queued(command, commands, &mut state, &session, shared) {
                return;
            }
            continue;
        }
        if paused {
            continue;
        }
        state.watch_audio(shared);
        let time = state.clock_time();
        let end = state.end;
        if time >= end {
            // Playing stops on the last frame, drawn now unless it already was. It is drawn before
            // playing reads as stopped, so whoever sees it stop sees every frame accounted for.
            let last = MediaTime::from_ticks(end.ticks() - frame_length.ticks().min(end.ticks()));
            let drawn = state.played_frame == Some(last.ticks() / frame_length.ticks().max(1));
            state.play_frame(last, frame_length, shared);
            if !drawn {
                state.position = last;
                state.show(&session, shared, &shared.stop);
            }
            state.pause_at(last, shared);
            continue;
        }
        // A frame already drawn is not drawn again: the loop waits for the next one by the clock
        // that drives playing (the heard audio, which can lag the wall clock, or the wall clock).
        let frame = time.ticks() / frame_length.ticks().max(1);
        if state.played_frame != Some(frame) {
            state.play_frame(time, frame_length, shared);
            state.position = time;
            state.show(&session, shared, &shared.stop);
        }
        let next = (frame + 1) * frame_length.ticks().max(1);
        let ahead = u64::try_from(next - state.clock_time().ticks()).unwrap_or(0);
        std::thread::sleep(Duration::from_nanos(ahead.saturating_mul(
            1_000_000_000 / alpha2_time::TICKS_PER_SECOND.unsigned_abs(),
        )));
    }
}

/// Runs the preview thread's `body`; a panic in it is recorded and leaves the preview stopped.
fn contained(shared: &Shared, body: impl FnOnce()) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        record(
            shared,
            format!(
                "the preview thread stopped: {}",
                alpha2_host_core::panic_text(&*payload)
            ),
        );
        shared.playing.store(false, Ordering::Release);
        shared.sounding.store(false, Ordering::Release);
    }
}

fn frame_length(session: &Session) -> MediaTime {
    let rate = session.project.settings.frame_rate;
    MediaTime::from_ticks(
        alpha2_time::TICKS_PER_SECOND * i64::from(rate.denominator())
            / i64::from(rate.numerator().max(1)),
    )
}

/// Opens a source's stream for the preview.
type OpenSource = Box<dyn FnMut(&SourceId) -> Result<SourceStream, String> + Send>;

/// The preview thread's state.
struct State {
    output: Option<Output>,
    frames: FramePlayer<RenderedTexture>,
    cache: Option<SourceFrameCache>,
    sources: StreamSources<OpenSource>,
    providers: Arc<ProviderMedia>,
    texts: TextTextures,
    position: MediaTime,
    end: MediaTime,
    /// The project's sample rate.
    rate: u32,
    /// The wall clock and position playing started from.
    started: Option<(Instant, MediaTime)>,
    /// The frame playing last drew, or the one before its first while it starts, counting the
    /// ones it skipped.
    played_frame: Option<i64>,
    index: u64,
    /// The project's audio on the output, opened on the first play and kept while it plays: its
    /// heard position is the clock while playing.
    audio: Option<PreviewAudio>,
    /// What the session's pause held back, done when it resumes.
    deferred: Option<Deferred>,
    /// Whether the session's pause stopped playing, which goes on when the session resumes.
    held: bool,
    /// When the heard audio last moved.
    heard: HeardClock,
    /// Sources whose dropped layer a diagnostic line already named.
    dropped_named: HashSet<SourceId>,
}

/// Tells an audio output that stopped taking samples from one that is playing.
#[derive(Clone, Copy, Debug, Default)]
struct HeardClock {
    sample: i64,
    since: Duration,
}

impl HeardClock {
    /// Whether the output stopped: the heard `sample` has not moved for [`AUDIO_STALL`] up to
    /// `now` while the ring held samples (`buffered`).
    fn stalled(&mut self, sample: i64, now: Duration, buffered: bool) -> bool {
        if sample != self.sample || !buffered {
            *self = Self { sample, since: now };
            return false;
        }
        now.saturating_sub(self.since) >= AUDIO_STALL
    }
}

/// The window and size a paused session held back, applied with one frame drawn on resume.
#[derive(Default)]
struct Deferred {
    window: Option<(NativeWindow, u32, u32)>,
    size: Option<(u32, u32)>,
}

impl State {
    fn new(session: &Session, shared: &Shared) -> Self {
        // Text draws as an export draws it; a font the export would refuse is reported here too.
        let texts = crate::export::text_textures(&session.fonts).unwrap_or_else(|error| {
            record(shared, error.to_string());
            TextTextures::new()
        });
        let end = session
            .snapshot
            .timeline_range()
            .map_or(MediaTime::ZERO, alpha2_time::TimeRange::end);
        let files = crate::export::files_of(
            &session.request.assets,
            &session.request.frames,
            &session.request.resources,
            &session.fonts,
        );
        let providers = Arc::new(ProviderMedia::new(
            &session.project,
            files,
            (
                session.project.settings.width,
                session.project.settings.height,
            ),
            PREVIEW_PROVIDER_BYTES,
        ));
        Self {
            output: None,
            frames: FramePlayer::new(QualityProfile::preview()).unwrap_or_default(),
            cache: None,
            sources: sources(session, Arc::clone(&providers), None),
            providers,
            texts,
            position: MediaTime::ZERO,
            end,
            rate: session.project.settings.sample_rate,
            started: None,
            played_frame: None,
            index: 0,
            audio: None,
            deferred: None,
            held: false,
            heard: HeardClock::default(),
            dropped_named: HashSet::new(),
        }
    }

    /// Stops playing at the position when the session pauses; it goes on in [`Self::resume`].
    fn hold(&mut self, shared: &Shared) {
        if self.started.is_none() {
            return;
        }
        self.position = self.clock_time().min(self.end);
        self.started = None;
        self.played_frame = None;
        self.silence(shared);
        self.held = true;
    }

    /// Pauses the audio output, keeping its stream for the next play.
    fn silence(&mut self, shared: &Shared) {
        if let Some(audio) = self.audio.as_mut()
            && let Err(error) = audio.pause()
        {
            alpha2_os::diagnostic!("alpha2 preview: the audio output did not pause: {error}");
            self.audio = None;
        }
        shared.sounding.store(false, Ordering::Release);
    }

    /// Lets the picture keep time by the wall clock from the heard position when the audio output
    /// stopped taking samples or reported an error.
    fn watch_audio(&mut self, shared: &Shared) {
        let (Some(audio), Some((at, _))) = (self.audio.as_ref(), self.started) else {
            return;
        };
        let heard = audio.heard_sample();
        if let Some(failure) = audio.last_error().filter(|failure| failure.stopped) {
            record(
                shared,
                format!("the project's audio stopped mixing: {}", failure.message),
            );
        } else {
            let reason = audio.stream_error().or_else(|| {
                self.heard
                    .stalled(heard, at.elapsed(), audio.buffered())
                    .then(|| format!("it took no sample for {} ms", AUDIO_STALL.as_millis()))
            });
            let Some(reason) = reason else {
                return;
            };
            alpha2_os::diagnostic!(
                "alpha2 preview: the audio output stopped ({reason}); playing on silent"
            );
        }
        let from = self.clock_time();
        self.audio = None;
        shared.sounding.store(false, Ordering::Release);
        self.started = Some((Instant::now(), from));
    }

    /// Restarts playing the pause held, then applies the deferred window or size and draws a frame.
    fn resume(&mut self, session: &Session, shared: &Arc<Shared>) {
        if std::mem::take(&mut self.held) {
            self.start_playing(self.position, session, shared);
        }
        let Some(deferred) = self.deferred.take() else {
            return;
        };
        if let Some((window, width, height)) = deferred.window {
            self.attach(window, (width, height), session, shared);
            return;
        }
        if let Some((width, height)) = deferred.size
            && let Some(output) = self.output.as_mut()
        {
            output.resize(width, height);
        }
        self.frames.reset();
        self.show(session, shared, &shared.stop);
    }

    /// The work held back for the resume while the session is paused, `None` when it is not.
    fn defer(&mut self, session: &Session) -> Option<&mut Deferred> {
        session
            .paused
            .is_paused()
            .then(|| self.deferred.get_or_insert_with(Deferred::default))
    }

    fn attach(
        &mut self,
        window: NativeWindow,
        (width, height): (u32, u32),
        session: &Session,
        shared: &Arc<Shared>,
    ) {
        let attached = match self.output.as_mut() {
            Some(output) => output.attach(window, width, height),
            None => Output::open(
                window,
                (width, height),
                session.gpu_frames,
                Arc::clone(shared),
            )
            .map(|output| {
                self.cache = Some(SourceFrameCache::for_renderer(
                    ExportRenderer::Gpu(&output.renderer),
                    session.budget.bytes(),
                ));
                // No stream opens before the first window, so sources decode for this device.
                self.sources =
                    sources(session, Arc::clone(&self.providers), output.frames.as_ref());
                self.output = Some(output);
            }),
        };
        match attached {
            Ok(()) => {
                // A new window shows the frame at the position at once.
                self.frames.reset();
                self.show(session, shared, &shared.stop);
            }
            Err(error) => record(shared, error),
        }
    }

    const fn playing(&self) -> bool {
        self.started.is_some()
    }

    fn clock_time(&self) -> MediaTime {
        if let (Some(audio), Some((_, from))) = (self.audio.as_ref(), self.started) {
            let heard = audio.heard_sample();
            let ticks = i128::from(heard) * i128::from(alpha2_time::TICKS_PER_SECOND)
                / i128::from(self.rate.max(1));
            return MediaTime::from_ticks(i64::try_from(ticks).unwrap_or(0)).max(from);
        }
        self.started.map_or(self.position, |(at, from)| {
            let elapsed = i64::try_from(at.elapsed().as_nanos()).unwrap_or(i64::MAX);
            MediaTime::from_ticks(
                from.ticks()
                    .saturating_add(elapsed / (1_000_000_000 / alpha2_time::TICKS_PER_SECOND)),
            )
        })
    }

    /// Counts the frames between the last one drawn while playing and `time`'s as dropped.
    fn play_frame(&mut self, time: MediaTime, frame_length: MediaTime, shared: &Shared) {
        let frame = time.ticks() / frame_length.ticks().max(1);
        if let Some(last) = self.played_frame
            && frame > last + 1
        {
            shared.dropped.fetch_add(
                u64::try_from(frame - last - 1).unwrap_or(0),
                Ordering::AcqRel,
            );
        }
        self.played_frame = Some(frame);
    }

    fn pause_at(&mut self, time: MediaTime, shared: &Shared) {
        self.position = time;
        self.held = false;
        self.started = None;
        self.played_frame = None;
        self.silence(shared);
        shared.playing.store(false, Ordering::Release);
    }

    fn apply(&mut self, command: Command, session: &Session, shared: &Arc<Shared>) {
        match command {
            Command::Attach {
                window,
                width,
                height,
            } => match self.defer(session) {
                Some(deferred) => {
                    deferred.window = Some((window, width, height));
                    deferred.size = None;
                }
                None => self.attach(window, (width, height), session, shared),
            },
            Command::Detach => {
                if let Some(deferred) = self.deferred.as_mut() {
                    deferred.window = None;
                }
                if let Some(output) = self.output.as_mut() {
                    output.surface = None;
                }
                if self.started.is_some() {
                    self.position = self.clock_time();
                    self.started = None;
                }
                self.held = false;
                self.silence(shared);
                shared.playing.store(false, Ordering::Release);
            }
            Command::Resize { width, height } => match self.defer(session) {
                Some(Deferred {
                    window: Some((_, window_width, window_height)),
                    ..
                }) => (*window_width, *window_height) = (width, height),
                Some(deferred) => deferred.size = Some((width, height)),
                None => {
                    if let Some(output) = self.output.as_mut() {
                        output.resize(width, height);
                    }
                    self.frames.reset();
                    self.show(session, shared, &shared.stop);
                }
            },
            Command::Seek(ticks, token) => {
                let time = MediaTime::from_ticks(ticks.clamp(0, self.end.ticks()));
                self.position = time;
                if self.started.is_some() || self.held {
                    self.start_playing(time, session, shared);
                }
                self.show(session, shared, &token);
            }
            Command::Play => {
                let from = if self.position >= self.end {
                    MediaTime::ZERO
                } else {
                    self.position
                };
                self.start_playing(from, session, shared);
                shared.playing.store(true, Ordering::Release);
            }
            Command::Pause => {
                let time = self.clock_time().min(self.end);
                self.pause_at(time, shared);
                self.show(session, shared, &shared.stop);
            }
        }
    }

    /// Starts the clock, and the project's audio with it, at `from`.
    fn start_playing(&mut self, from: MediaTime, session: &Session, shared: &Shared) {
        if session.paused.is_paused() {
            self.position = from;
            self.held = true;
            return;
        }
        // Frames from `from` on count: one the clock passes before the first draw (audio that
        // starts a little in) is dropped like any other.
        self.played_frame = Some(from.ticks() / frame_length(session).ticks().max(1) - 1);
        self.heard = HeardClock::default();
        shared.sounding.store(false, Ordering::Release);
        let sample = i128::from(from.ticks()) * i128::from(self.rate)
            / i128::from(alpha2_time::TICKS_PER_SECOND);
        let sample = i64::try_from(sample).unwrap_or(0);
        let sounding = match self.audio.as_mut() {
            Some(audio) => audio.reposition(sample).map_err(|error| error.to_string()),
            None => open_audio(session, sample).map(|audio| {
                self.audio = audio;
            }),
        };
        match sounding {
            Ok(()) => shared
                .sounding
                .store(self.audio.is_some(), Ordering::Release),
            // Without an output the picture keeps time by the wall clock.
            Err(error) => {
                alpha2_os::diagnostic!("alpha2 preview: no audio: {error}");
                self.audio = None;
            }
        }
        self.started = Some((Instant::now(), from));
    }

    /// Renders and presents the frame at the position unless `cancel` stops it first, which is
    /// neither counted nor an error. While the session is paused, the frame waits for the resume.
    fn show(&mut self, session: &Session, shared: &Shared, cancel: &CancelToken) {
        if self.defer(session).is_some() {
            return;
        }
        match self.render(session, shared, cancel) {
            Ok(()) => {}
            Err(_) if cancel.is_cancelled() => {
                if let Some(cache) = self.cache.as_mut() {
                    cache.unpin();
                }
            }
            Err(error) => record(shared, error),
        }
        for (source, dropped) in self.sources.dropped_layers() {
            if self.dropped_named.insert(source.clone()) {
                alpha2_os::diagnostic!(
                    "alpha2 preview: source {source} draws opaque ({}): {}",
                    dropped.code,
                    dropped.reason
                );
            }
        }
    }

    fn render(
        &mut self,
        session: &Session,
        shared: &Shared,
        cancel: &CancelToken,
    ) -> Result<(), String> {
        let (Some(output), Some(cache)) = (self.output.as_ref(), self.cache.as_mut()) else {
            return Ok(());
        };
        if cancel.is_cancelled() {
            return Ok(());
        }
        let time = self.position;
        let snapshot = &session.snapshot;
        let size = QualityProfile::preview()
            .render_size(&session.project.settings)
            .map_err(|error| error.to_string())?;
        let color = alpha2_engine::color_policy(&session.project.settings.color);
        let needs = snapshot
            .frame_needs(time)
            .map_err(|error| error.to_string())?;
        self.index += 1;
        cache.follow(
            &session.budget,
            alpha2_host_core::export::SourceFrames::held_bytes(&self.sources),
        );
        cache
            .fill(
                ExportRenderer::Gpu(&output.renderer),
                size,
                color,
                (self.index, &needs),
                &mut self.sources,
                cancel,
            )
            .map_err(|error| error.to_string())?;
        self.texts
            .prepare_at(&output.renderer, snapshot, time, size)
            .map_err(|error| error.to_string())?;
        let provider = FramesWithText::new(cache.provider(), &self.texts);
        let frame = self
            .frames
            .render_at(snapshot, &output.renderer, &provider, time)
            .map_err(|error| error.to_string())?;
        // A frame the window did not take (not ready, occluded, being rebuilt) is dropped.
        let presented = match frame.texture {
            Some(texture) => output.present(texture)?,
            None => false,
        };
        let count = if presented {
            &shared.presented
        } else {
            &shared.dropped
        };
        count.fetch_add(1, Ordering::AcqRel);
        cache.unpin();
        shared.position.store(time.ticks(), Ordering::Release);
        Ok(())
    }
}

/// The decode side of a preview: one stream per source and cursor, opened on first use and kept
/// while the preview lives, the least recently read closed past the timeline's peak of cursors
/// reading at once plus a spare. With `frames`, hardware-decoded frames stay on the GPU.
fn sources(
    session: &Session,
    providers: Arc<ProviderMedia>,
    frames: Option<&GpuFrames>,
) -> StreamSources<OpenSource> {
    let assets = session
        .request
        .assets
        .iter()
        .map(|(id, path)| (id.clone(), PathBuf::from(path)))
        .collect();
    let video = frames
        .map(|frames| Arc::clone(&frames.decoders))
        .or_else(|| session.video.clone());
    let open: OpenSource = Box::new(crate::export::source_opener(
        &session.project,
        assets,
        video,
        session.preference,
    ));
    let peak = session.snapshot.timeline_range().map_or(1, |range| {
        alpha2_host_core::cursor::visual_decode_reads(&session.project, range).0
    });
    let sources = StreamSources::new(open)
        .with_providers(providers)
        .with_session_limit(peak + crate::export::SPARE_SESSIONS);
    match frames {
        Some(frames) => sources.with_surface_import(Arc::clone(&frames.import)),
        None => sources,
    }
}

/// The project's audio on the default output from project sample `sample`, its sources read in
/// windows of at most [`PREVIEW_AUDIO_BYTES`] as the mix reaches them; `None` for a project
/// without audible sources or a build without audio decoders.
///
/// # Errors
/// A source without an asset, or no output to open.
fn open_audio(session: &Session, sample: i64) -> Result<Option<PreviewAudio>, String> {
    let Some(decoders) = session.audio.as_ref() else {
        return Ok(None);
    };
    if !session
        .project
        .sources
        .iter()
        .any(alpha2_model::MediaSource::is_audible)
    {
        return Ok(None);
    }
    let assets = session
        .request
        .assets
        .iter()
        .map(|(id, path)| (id.clone(), PathBuf::from(path)))
        .collect();
    let sources = crate::export::audio_windows(
        &session.project,
        &assets,
        Arc::clone(decoders),
        (session.project.settings.sample_rate, PREVIEW_AUDIO_BYTES),
    )
    .map_err(|error| error.to_string())?;
    PreviewAudio::start(session.snapshot.clone(), Box::new(sources), sample)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn record(shared: &Shared, error: String) {
    alpha2_os::diagnostic!("alpha2 preview: {error}");
    *shared.error.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(commands: &[Command]) -> Vec<String> {
        commands
            .iter()
            .map(|command| match command {
                Command::Attach { width, .. } => format!("attach {width}"),
                Command::Detach => "detach".into(),
                Command::Resize { width, .. } => format!("resize {width}"),
                Command::Seek(ticks, _) => format!("seek {ticks}"),
                Command::Play => "play".into(),
                Command::Pause => "pause".into(),
            })
            .collect()
    }

    fn seek(ticks: i64) -> Command {
        Command::Seek(ticks, CancelToken::default())
    }

    const fn resize(width: u32) -> Command {
        Command::Resize { width, height: 1 }
    }

    #[test]
    fn coalescing_keeps_the_last_seek_and_resize_and_the_order_of_the_rest() {
        let commands = vec![
            seek(1),
            resize(10),
            Command::Play,
            seek(2),
            resize(20),
            Command::Detach,
            seek(3),
            Command::Pause,
        ];
        assert_eq!(
            names(&coalesce(commands)),
            ["play", "resize 20", "detach", "seek 3", "pause"]
        );
        assert_eq!(
            names(&coalesce(vec![
                Command::Play,
                Command::Pause,
                Command::Detach
            ])),
            ["play", "pause", "detach"]
        );
        assert_eq!(names(&coalesce(vec![seek(4)])), ["seek 4"]);
    }

    #[test]
    fn the_output_counts_as_stopped_when_the_heard_sample_stands_still_with_samples_waiting() {
        let ms = Duration::from_millis;
        let mut clock = HeardClock::default();
        let moving = [(100, 0), (900, 20), (1_700, 40), (2_500, 60)];
        assert!(
            moving
                .into_iter()
                .all(|(sample, at)| !clock.stalled(sample, ms(at), true))
        );
        assert!(!clock.stalled(2_500, ms(300), true), "200 ms still");
        assert!(!clock.stalled(2_500, ms(559), true), "499 ms still");
        assert!(clock.stalled(2_500, ms(560), true), "500 ms still");
        assert!(!clock.stalled(3_300, ms(580), true), "it moved again");

        let mut starved = HeardClock::default();
        assert!(!starved.stalled(0, ms(0), false));
        assert!(
            !starved.stalled(0, ms(2_000), false),
            "an empty ring is a mix that fell behind, not a stopped output"
        );
        assert!(!starved.stalled(0, ms(2_100), true));
        assert!(starved.stalled(0, ms(2_600), true));
    }

    #[test]
    fn a_panic_on_the_preview_thread_is_recorded_and_stops_the_preview() {
        let shared = Shared::default();
        shared.playing.store(true, Ordering::Release);
        shared.sounding.store(true, Ordering::Release);
        contained(&shared, || panic!("the frame broke"));
        let error = shared.error.lock().expect("lock").clone();
        assert_eq!(
            error.as_deref(),
            Some("the preview thread stopped: the frame broke")
        );
        assert!(!shared.playing.load(Ordering::Acquire));
        assert!(!shared.sounding.load(Ordering::Acquire));
    }
}
