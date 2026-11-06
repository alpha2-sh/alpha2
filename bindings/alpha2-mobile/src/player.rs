//! Real-time preview of a project on a window: a library thread decodes, renders and presents
//! frames at the project's rate while playing, and the frame at the position when paused.
//!
//! Every call returns at once and is carried out on that thread in order. The window is
//! attached and detached with the app's surface lifecycle; nothing is drawn while none is
//! attached or while the session is paused for the background.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use alpha2_engine::{
    EngineProject, FramesWithText, Player as FramePlayer, QualityProfile, TextTextures,
};
use alpha2_host_core::DecodedAudioSources;
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

/// How often a preview held in the background checks whether the app returned.
const PAUSED_POLL: Duration = Duration::from_millis(50);

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
    Seek(i64),
    Play,
    Pause,
}

/// What the preview thread reports back.
#[derive(Default)]
struct Shared {
    position: AtomicI64,
    playing: AtomicBool,
    sounding: AtomicBool,
    presented: AtomicU64,
    dropped: AtomicU64,
    error: Mutex<Option<String>>,
}

/// A project's preview on an app window.
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
            .spawn(move || run(session, &receive, &thread_shared))
            .map_err(|error| Alpha2Error::Failed {
                code: "RESOURCE_LIMIT".into(),
                reason: format!("cannot start the preview thread: {error}"),
            })?;
        Ok(Arc::new(Self {
            commands: Mutex::new(Some(commands)),
            shared,
            thread: Mutex::new(Some(thread)),
        }))
    }

    /// Draws into the window `handle` (an `ANativeWindow` from `Alpha2Android.windowFromSurface`,
    /// whose reference the player takes over, or a `CAMetalLayer` pointer) of `width` x `height`
    /// pixels, replacing any attached one.
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
        self.send(Command::Seek(ticks));
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

    /// Frames playing skipped because they came due before the previous one was drawn, as a
    /// device too slow for the project's rate does; with [`Self::frames_presented`] they account
    /// for every frame played.
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

    /// Ends the preview and waits for its thread; the player does nothing afterwards. Dropping
    /// the player (Kotlin `close()`, Swift release) ends it too.
    pub fn end(&self) {
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

/// The preview thread: carries out commands, and renders while playing.
fn run(session: Session, commands: &Receiver<Command>, shared: &Arc<Shared>) {
    let mut state = State::new(&session, shared);
    let frame_length = frame_length(&session);
    loop {
        let command = if state.playing() && state.output.is_some() {
            match commands.recv_timeout(Duration::ZERO) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match commands.recv() {
                Ok(command) => Some(command),
                Err(_) => return,
            }
        };
        if let Some(command) = command {
            state.apply(command, &session, shared);
            continue;
        }
        // In the background nothing renders, but commands are still carried out: an end or a
        // detach must not wait for the app to return to the foreground.
        if session.paused.is_paused() {
            match commands.recv_timeout(PAUSED_POLL) {
                Ok(command) => state.apply(command, &session, shared),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            continue;
        }
        let time = state.clock_time();
        let end = state.end;
        if time >= end {
            // Playing stops on the last frame, drawn now unless it already was.
            let last = MediaTime::from_ticks(end.ticks() - frame_length.ticks().min(end.ticks()));
            let drawn = state.played_frame == Some(last.ticks() / frame_length.ticks().max(1));
            state.play_frame(last, frame_length, shared);
            state.pause_at(last, shared);
            if !drawn {
                state.show(&session, shared);
            }
            continue;
        }
        state.play_frame(time, frame_length, shared);
        state.position = time;
        state.show(&session, shared);
        let next = MediaTime::from_ticks(
            (time.ticks() / frame_length.ticks().max(1) + 1) * frame_length.ticks().max(1),
        );
        if let Some(wait) = state.until(next) {
            std::thread::sleep(wait);
        }
    }
}

fn frame_length(session: &Session) -> MediaTime {
    let rate = session.project.settings.frame_rate;
    MediaTime::from_ticks(
        alpha2_time::TICKS_PER_SECOND * i64::from(rate.denominator())
            / i64::from(rate.numerator().max(1)),
    )
}

/// A preview's audio sources.
enum Sound {
    /// Not decoded yet: the preview has not played.
    Unread,
    /// The project has none that decode.
    Silent,
    /// Every audible source, decoded whole.
    Decoded(Arc<DecodedAudioSources>),
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
    /// The frame playing last drew, counting the ones it skipped.
    played_frame: Option<i64>,
    index: u64,
    /// The project's audio, decoded when it first plays.
    sound: Sound,
    /// Audio playing with the picture, whose heard position is the clock.
    audio: Option<PreviewAudio>,
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
            sound: Sound::Unread,
            audio: None,
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

    /// How long until the clock reaches `time`.
    fn until(&self, time: MediaTime) -> Option<Duration> {
        let (at, from) = self.started?;
        let ahead = time.ticks() - from.ticks();
        let due = Duration::from_nanos(
            u64::try_from(ahead).ok()?
                * (1_000_000_000 / u64::try_from(alpha2_time::TICKS_PER_SECOND).ok()?),
        );
        due.checked_sub(at.elapsed())
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
        self.started = None;
        self.played_frame = None;
        self.audio = None;
        shared.sounding.store(false, Ordering::Release);
        shared.playing.store(false, Ordering::Release);
    }

    fn apply(&mut self, command: Command, session: &Session, shared: &Arc<Shared>) {
        match command {
            Command::Attach {
                window,
                width,
                height,
            } => {
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
                        // No stream is open before the first window: sources decode for this
                        // device from the start.
                        self.sources =
                            sources(session, Arc::clone(&self.providers), output.frames.as_ref());
                        self.output = Some(output);
                    }),
                };
                match attached {
                    Ok(()) => {
                        // A new window shows the frame at the position at once.
                        self.frames.reset();
                        self.show(session, shared);
                    }
                    Err(error) => record(shared, error),
                }
            }
            Command::Detach => {
                if let Some(output) = self.output.as_mut() {
                    output.surface = None;
                }
                if self.started.is_some() {
                    self.position = self.clock_time();
                    self.started = None;
                }
                self.audio = None;
                shared.sounding.store(false, Ordering::Release);
                shared.playing.store(false, Ordering::Release);
            }
            Command::Resize { width, height } => {
                if let Some(output) = self.output.as_mut() {
                    output.resize(width, height);
                }
                self.frames.reset();
                self.show(session, shared);
            }
            Command::Seek(ticks) => {
                let time = MediaTime::from_ticks(ticks.clamp(0, self.end.ticks()));
                self.position = time;
                if self.started.is_some() {
                    self.start_playing(time, session, shared);
                }
                self.show(session, shared);
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
                self.show(session, shared);
            }
        }
    }

    /// Starts the clock, and the project's audio with it, at `from`.
    fn start_playing(&mut self, from: MediaTime, session: &Session, shared: &Shared) {
        // Playing from a new position skips nothing.
        self.played_frame = None;
        self.audio = None;
        shared.sounding.store(false, Ordering::Release);
        if matches!(self.sound, Sound::Unread) {
            self.sound =
                decode_preview_audio(session, shared).map_or(Sound::Silent, Sound::Decoded);
        }
        if let Sound::Decoded(sources) = &self.sound {
            let sources = Arc::clone(sources);
            let sample = i128::from(from.ticks()) * i128::from(self.rate)
                / i128::from(alpha2_time::TICKS_PER_SECOND);
            match PreviewAudio::start(
                session.snapshot.clone(),
                sources,
                i64::try_from(sample).unwrap_or(0),
            ) {
                Ok(audio) => {
                    self.audio = Some(audio);
                    shared.sounding.store(true, Ordering::Release);
                }
                // Without an output the picture keeps time by the wall clock.
                Err(error) => alpha2_os::diagnostic!("alpha2 preview: no audio: {error}"),
            }
        }
        self.started = Some((Instant::now(), from));
    }

    /// Renders and presents the frame at the position.
    fn show(&mut self, session: &Session, shared: &Shared) {
        if let Err(error) = self.render(session, shared) {
            record(shared, error);
        }
    }

    fn render(&mut self, session: &Session, shared: &Shared) -> Result<(), String> {
        let (Some(output), Some(cache)) = (self.output.as_ref(), self.cache.as_mut()) else {
            return Ok(());
        };
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
        cache.follow(&session.budget);
        cache
            .fill(
                ExportRenderer::Gpu(&output.renderer),
                size,
                color,
                (self.index, &needs),
                &mut self.sources,
                &CancelToken::default(),
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
        if let Some(texture) = frame.texture
            && output.present(texture)?
        {
            shared.presented.fetch_add(1, Ordering::AcqRel);
        }
        cache.unpin();
        shared.position.store(time.ticks(), Ordering::Release);
        Ok(())
    }
}

/// The decode side of a preview: one stream per source and cursor, opened on first use and kept
/// while the preview lives. With `frames`, hardware-decoded frames stay on the GPU.
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
    let sources = StreamSources::new(open).with_providers(providers);
    match frames {
        Some(frames) => sources.with_surface_import(Arc::clone(&frames.import)),
        None => sources,
    }
}

/// The project's audible sources, decoded for the preview; `None` for a silent project or one
/// whose audio does not decode (recorded).
fn decode_preview_audio(session: &Session, shared: &Shared) -> Option<Arc<DecodedAudioSources>> {
    let decoders = session.audio.as_ref()?;
    if !session
        .project
        .sources
        .iter()
        .any(alpha2_model::MediaSource::is_audible)
    {
        return None;
    }
    let assets = session
        .request
        .assets
        .iter()
        .map(|(id, path)| (id.clone(), PathBuf::from(path)))
        .collect();
    match crate::export::decode_audio(
        &session.project,
        &assets,
        decoders.as_ref(),
        session.project.settings.sample_rate,
        &CancelToken::default(),
    ) {
        Ok(sources) => Some(Arc::new(sources)),
        Err(error) => {
            record(
                shared,
                format!("the project's audio does not decode: {error:?}"),
            );
            None
        }
    }
}

fn record(shared: &Shared, error: String) {
    alpha2_os::diagnostic!("alpha2 preview: {error}");
    *shared.error.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
}
