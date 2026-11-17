//! Project audio for a real-time preview: a thread mixes ahead into a ring that the output
//! callback drains, and the samples the device took drive the preview's clock.
//!
//! The callback only copies from the ring, so it never allocates or waits; when the mix falls
//! behind it plays silence and counts the shortfall. Before rendering a block the mix thread makes
//! ready the audio it reads, so sources are decoded in windows as the mix reaches them.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use alpha2_audio::{ProcessCtx, TransportState};
use alpha2_engine::{AudioRenderer, EngineProject};
use alpha2_media::{AudioBlock, CancelToken};

use crate::audio_host::{CpalHostError, CpalOutputHost, ProjectAudioSources, RealtimeAudioSource};

/// Stereo frames the ring holds: a fifth of a second at 48 kHz, enough to ride out a busy mix
/// thread without delaying a seek much.
const RING_FRAMES: usize = 1 << 13;

/// Frames mixed at a time.
const MIX_BLOCK: usize = 1024;

/// Failures of one block in a row, 5 ms apart, after which the mix stops and the output plays
/// silence.
const MAX_CONSECUTIVE_MIX_FAILURES: u32 = 10;

/// Why the mix failed last.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MixFailure {
    /// The renderer's or the sources' error.
    pub message: String,
    /// Failures in a row up to now; 0 once a later block mixed.
    pub consecutive: u32,
    /// Whether the mix gave up after 10 failures in a row.
    pub stopped: bool,
}

/// The ring between the mix thread and the output callback: interleaved stereo samples as bits,
/// with monotonic counts of frames written and read. A frame is written before the count that
/// covers it is released and read after it is acquired, so the samples need no ordering of their
/// own.
struct Ring {
    samples: Box<[AtomicU32]>,
    written: AtomicU64,
    read: AtomicU64,
    /// Frames below this count are dropped unread: a reposition discarded them.
    discard: AtomicU64,
    /// Frames the callback wanted and the ring did not have.
    short: AtomicU64,
}

impl Ring {
    fn new() -> Self {
        Self {
            samples: (0..RING_FRAMES * 2).map(|_| AtomicU32::new(0)).collect(),
            written: AtomicU64::new(0),
            read: AtomicU64::new(0),
            discard: AtomicU64::new(0),
            short: AtomicU64::new(0),
        }
    }

    fn put(&self, frame: u64, left: f32, right: f32) {
        let at = usize::try_from(frame % RING_FRAMES as u64).unwrap_or(0) * 2;
        self.samples[at].store(left.to_bits(), Ordering::Relaxed);
        self.samples[at + 1].store(right.to_bits(), Ordering::Relaxed);
    }

    fn get(&self, frame: u64) -> [f32; 2] {
        let at = usize::try_from(frame % RING_FRAMES as u64).unwrap_or(0) * 2;
        [
            f32::from_bits(self.samples[at].load(Ordering::Relaxed)),
            f32::from_bits(self.samples[at + 1].load(Ordering::Relaxed)),
        ]
    }
}

/// Plays the ring.
struct RingSource(Arc<Ring>);

impl RealtimeAudioSource for RingSource {
    fn render(&mut self, _ctx: &ProcessCtx, output: &mut [&mut [f32]]) {
        let ring = &self.0;
        let frames = output.first().map_or(0, |plane| plane.len());
        let read = ring
            .read
            .load(Ordering::Acquire)
            .max(ring.discard.load(Ordering::Acquire));
        let available = ring.written.load(Ordering::Acquire) - read;
        let taken = usize::try_from(available).unwrap_or(usize::MAX).min(frames);
        for index in 0..taken {
            let frame = ring.get(read + index as u64);
            for (channel, plane) in output.iter_mut().enumerate() {
                plane[index] = frame[channel.min(1)];
            }
        }
        for plane in output.iter_mut() {
            plane[taken..].fill(0.0);
        }
        ring.read.store(read + taken as u64, Ordering::Release);
        if taken < frames {
            ring.short
                .fetch_add((frames - taken) as u64, Ordering::Relaxed);
        }
    }
}

/// A project's mix ahead of the output: each block makes ready what it reads, then renders.
pub struct Mixer {
    snapshot: EngineProject,
    renderer: AudioRenderer,
    sources: Box<dyn ProjectAudioSources + Send>,
}

impl Mixer {
    /// A stereo mix of `snapshot` reading `sources`.
    ///
    /// # Errors
    /// The renderer's refusal of the project.
    pub fn new(
        snapshot: EngineProject,
        sources: Box<dyn ProjectAudioSources + Send>,
    ) -> Result<Self, String> {
        let renderer =
            AudioRenderer::new(&snapshot, 2, MIX_BLOCK).map_err(|error| error.to_string())?;
        Ok(Self {
            snapshot,
            renderer,
            sources,
        })
    }

    /// The mix of [`MIX_BLOCK`] frames from project sample `at`.
    ///
    /// # Errors
    /// What making the sources ready or rendering met, or a cancel by `cancel`.
    pub fn block(&mut self, at: i64, cancel: &CancelToken) -> Result<AudioBlock, String> {
        let needs = self
            .renderer
            .audio_needs(&self.snapshot, at, MIX_BLOCK)
            .map_err(|error| error.to_string())?;
        self.sources
            .prepare(&needs, cancel)
            .map_err(|error| error.to_string())?;
        let status = self
            .renderer
            .render(
                &self.snapshot,
                at,
                MIX_BLOCK,
                &*self.sources,
                TransportState::Playing,
            )
            .map_err(|error| error.to_string())?;
        Ok(self.renderer.block(&status))
    }
}

/// Where the mix thread mixes, set by the player's thread.
struct Control {
    /// The project sample the next block starts at.
    next: i64,
    /// Bumped by every reposition; a block of an older one is dropped.
    generation: u64,
    running: bool,
    /// Stops the current reposition's source preparation.
    cancel: CancelToken,
    ended: bool,
}

/// A project's audio on the default output, one stream for the preview's life: paused and
/// repositioned in place.
pub struct PreviewAudio {
    host: CpalOutputHost,
    ring: Arc<Ring>,
    control: Arc<(Mutex<Control>, Condvar)>,
    failure: Arc<Mutex<Option<MixFailure>>>,
    mixer: Option<JoinHandle<()>>,
    /// The project sample of the ring's frame `origin`.
    start_sample: i64,
    origin: u64,
}

impl std::fmt::Debug for PreviewAudio {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreviewAudio")
            .field("start_sample", &self.start_sample)
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl PreviewAudio {
    /// Starts `snapshot`'s mix of `sources` at project sample `start_sample` on the default
    /// output. No source is decoded here: the mix thread decodes what each block reads.
    ///
    /// # Errors
    /// No output device, none at the project rate, or a platform without audio output for this
    /// process (Android before the app hands over its context).
    pub fn start(
        snapshot: EngineProject,
        sources: Box<dyn ProjectAudioSources + Send>,
        start_sample: i64,
    ) -> Result<Self, CpalHostError> {
        let rate = snapshot.project().settings.sample_rate;
        let ring = Arc::new(Ring::new());
        let source = RingSource(Arc::clone(&ring));
        let host = open_output(source, rate)?;
        let mixer = Mixer::new(snapshot, sources).map_err(CpalHostError::Device)?;
        let control = Arc::new((
            Mutex::new(Control {
                next: start_sample,
                generation: 0,
                running: true,
                cancel: CancelToken::default(),
                ended: false,
            }),
            Condvar::new(),
        ));
        let failure = Arc::new(Mutex::new(None));
        let (mixer_ring, mixer_control, mixer_failure) = (
            Arc::clone(&ring),
            Arc::clone(&control),
            Arc::clone(&failure),
        );
        let thread = std::thread::Builder::new()
            .name("alpha2-preview-audio".into())
            .spawn(move || {
                let mut mixer = mixer;
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    mix(&mut mixer, &mixer_ring, &mixer_control, &mixer_failure);
                }));
                if let Err(payload) = ran {
                    *lock(&mixer_failure) = Some(MixFailure {
                        message: format!(
                            "the mix thread panicked: {}",
                            crate::panic_text(&*payload)
                        ),
                        consecutive: 0,
                        stopped: true,
                    });
                }
            })
            .map_err(|error| CpalHostError::Device(error.to_string()))?;
        let audio = Self {
            host,
            ring,
            control,
            failure,
            mixer: Some(thread),
            start_sample,
            origin: 0,
        };
        audio.begin()?;
        Ok(audio)
    }

    /// Plays once the first blocks are in, so playback does not open on silence.
    fn begin(&self) -> Result<(), CpalHostError> {
        let until = Instant::now() + Duration::from_millis(200);
        let wanted = self.origin + (MIX_BLOCK * 2) as u64;
        while self.ring.written.load(Ordering::Acquire) < wanted && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.host.play()
    }

    /// Stops the output and the mix where they are; [`Self::reposition`] plays again.
    ///
    /// # Errors
    /// The device error cpal reports.
    pub fn pause(&mut self) -> Result<(), CpalHostError> {
        {
            let mut control = lock(&self.control.0);
            control.running = false;
            control.cancel.cancel();
        }
        self.host.pause()
    }

    /// Plays from project sample `sample`: the mix stops at its block boundary, the ring's
    /// unplayed frames are dropped, and the mix starts again there.
    ///
    /// # Errors
    /// The device error cpal reports.
    pub fn reposition(&mut self, sample: i64) -> Result<(), CpalHostError> {
        {
            let (control, changed) = &*self.control;
            let mut control = lock(control);
            control.cancel.cancel();
            control.cancel = CancelToken::default();
            control.generation += 1;
            control.next = sample;
            control.running = true;
            // The mix writes the ring under this lock, so `written` stands still here.
            let written = self.ring.written.load(Ordering::Acquire);
            self.ring.discard.store(written, Ordering::Release);
            self.origin = written;
            self.start_sample = sample;
            changed.notify_all();
        }
        self.begin()
    }

    /// The project sample the listener hears now: where playing started, plus what the device
    /// took since, less the output's latency.
    #[must_use]
    pub fn heard_sample(&self) -> i64 {
        let read = self.ring.read.load(Ordering::Acquire);
        let taken = i64::try_from(read.saturating_sub(self.origin)).unwrap_or(i64::MAX);
        self.start_sample + taken - i64::from(self.host.latency_samples())
    }

    /// Whether the ring holds mixed samples the device has not taken.
    #[must_use]
    pub fn buffered(&self) -> bool {
        let read = self
            .ring
            .read
            .load(Ordering::Acquire)
            .max(self.ring.discard.load(Ordering::Acquire));
        self.ring.written.load(Ordering::Acquire) > read
    }

    /// Output frames that played silent because the mix fell behind, plus stream errors.
    #[must_use]
    pub fn shortfall(&self) -> u64 {
        self.ring.short.load(Ordering::Relaxed) + self.host.underruns()
    }

    /// The last error the output stream reported, `None` while it reported none.
    #[must_use]
    pub fn stream_error(&self) -> Option<String> {
        self.host.stream_error()
    }

    /// The mix's last failure, `None` while every block mixed.
    #[must_use]
    pub fn last_error(&self) -> Option<MixFailure> {
        lock(&self.failure).clone()
    }
}

/// Opens the default output playing `source` at `rate`.
#[cfg(not(target_os = "android"))]
fn open_output(source: RingSource, rate: u32) -> Result<CpalOutputHost, CpalHostError> {
    CpalOutputHost::open(source, rate, MIX_BLOCK)
}

/// Opens the default output playing `source` at `rate`.
///
/// cpal panics on Android before the app hands its context to `ndk_context`, which offers no way
/// to ask first; that panic is an output that cannot open, reported with its message.
#[cfg(target_os = "android")]
fn open_output(source: RingSource, rate: u32) -> Result<CpalOutputHost, CpalHostError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        CpalOutputHost::open(source, rate, MIX_BLOCK)
    }))
    .unwrap_or_else(|payload| {
        Err(CpalHostError::Device(format!(
            "this process has no audio output: {}",
            crate::panic_text(&*payload)
        )))
    })
}

impl Drop for PreviewAudio {
    fn drop(&mut self) {
        {
            let (control, changed) = &*self.control;
            let mut control = lock(control);
            control.ended = true;
            control.cancel.cancel();
            changed.notify_all();
        }
        if let Some(mixer) = self.mixer.take() {
            let _ = mixer.join();
        }
    }
}

/// Mixes ahead of the callback until the audio drops, or until one block fails
/// [`MAX_CONSECUTIVE_MIX_FAILURES`] times in a row.
fn mix(
    mixer: &mut Mixer,
    ring: &Ring,
    control: &(Mutex<Control>, Condvar),
    failure: &Mutex<Option<MixFailure>>,
) {
    let (control, changed) = control;
    let mut consecutive = 0_u32;
    loop {
        let (at, generation, cancel) = {
            let mut state = lock(control);
            while !state.running && !state.ended {
                state = changed.wait(state).unwrap_or_else(PoisonError::into_inner);
            }
            if state.ended {
                return;
            }
            (state.next, state.generation, state.cancel.clone())
        };
        let free = RING_FRAMES as u64
            - (ring.written.load(Ordering::Acquire)
                - ring
                    .read
                    .load(Ordering::Acquire)
                    .max(ring.discard.load(Ordering::Acquire)));
        if free < MIX_BLOCK as u64 {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        let block = mixer.block(at, &cancel);
        let state = lock(control);
        if state.generation != generation || !state.running || state.ended {
            continue;
        }
        let block = match block {
            Ok(block) => block,
            Err(message) => {
                drop(state);
                consecutive += 1;
                let stopped = consecutive >= MAX_CONSECUTIVE_MIX_FAILURES;
                *lock(failure) = Some(MixFailure {
                    message,
                    consecutive,
                    stopped,
                });
                if stopped {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
        };
        if std::mem::take(&mut consecutive) > 0
            && let Some(last) = lock(failure).as_mut()
        {
            last.consecutive = 0;
        }
        let written = ring.written.load(Ordering::Acquire);
        let frames = block.channels.first().map_or(0, Vec::len);
        for index in 0..frames {
            let left = block.channels[0][index];
            let right = block.channels.get(1).map_or(left, |plane| plane[index]);
            ring.put(written + index as u64, left, right);
        }
        ring.written
            .store(written + frames as u64, Ordering::Release);
        let mut state = state;
        state.next = at + i64::try_from(frames).unwrap_or(0);
    }
}

#[cfg(test)]
#[path = "preview_audio_tests.rs"]
mod tests;
