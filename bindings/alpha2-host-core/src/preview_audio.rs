//! Project audio for a real-time preview: a thread mixes ahead into a ring that the output
//! callback drains, and the samples the device took drive the preview's clock.
//!
//! The callback only copies from the ring, so it never allocates or waits; when the mix falls
//! behind it plays silence and counts the shortfall.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use alpha2_audio::{ProcessCtx, TransportState};
use alpha2_engine::{AudioRenderer, EngineProject};

use crate::audio_host::{CpalHostError, CpalOutputHost, DecodedAudioSources, RealtimeAudioSource};

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
    /// The renderer's error.
    pub message: String,
    /// Failures in a row up to now; 0 once a later block mixed.
    pub consecutive: u32,
    /// Whether the mix gave up after [`MAX_CONSECUTIVE_MIX_FAILURES`] failures in a row.
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
    /// Frames the callback wanted and the ring did not have.
    short: AtomicU64,
}

impl Ring {
    fn new() -> Self {
        Self {
            samples: (0..RING_FRAMES * 2).map(|_| AtomicU32::new(0)).collect(),
            written: AtomicU64::new(0),
            read: AtomicU64::new(0),
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
        let read = ring.read.load(Ordering::Acquire);
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

/// A project's audio playing from one sample on the default output.
pub struct PreviewAudio {
    host: CpalOutputHost,
    ring: Arc<Ring>,
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<MixFailure>>>,
    mixer: Option<JoinHandle<()>>,
    start_sample: i64,
}

impl std::fmt::Debug for PreviewAudio {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreviewAudio")
            .field("start_sample", &self.start_sample)
            .finish_non_exhaustive()
    }
}

impl PreviewAudio {
    /// Starts `snapshot`'s mix of `sources` at project sample `start_sample` on the default
    /// output.
    ///
    /// # Errors
    /// No output device, none at the project rate, or a platform without audio output for this
    /// process (Android before the app hands over its context).
    pub fn start(
        snapshot: EngineProject,
        sources: Arc<DecodedAudioSources>,
        start_sample: i64,
    ) -> Result<Self, CpalHostError> {
        let rate = snapshot.project().settings.sample_rate;
        let ring = Arc::new(Ring::new());
        let source = RingSource(Arc::clone(&ring));
        let host = open_output(source, rate)?;
        let mut renderer = AudioRenderer::new(&snapshot, 2, MIX_BLOCK)
            .map_err(|error| CpalHostError::Device(error.to_string()))?;
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let mixer_ring = Arc::clone(&ring);
        let mixer_stop = Arc::clone(&stop);
        let mixer_failure = Arc::clone(&failure);
        let mixer = std::thread::Builder::new()
            .name("alpha2-preview-audio".into())
            .spawn(move || {
                mix(
                    &snapshot,
                    &sources,
                    &mut renderer,
                    &mixer_ring,
                    &mixer_stop,
                    &mixer_failure,
                    start_sample,
                );
            })
            .map_err(|error| CpalHostError::Device(error.to_string()))?;
        // Begin once the first blocks are in, so playback does not open on silence.
        let until = std::time::Instant::now() + Duration::from_millis(200);
        while ring.written.load(Ordering::Acquire) < (MIX_BLOCK * 2) as u64
            && std::time::Instant::now() < until
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        host.play()?;
        Ok(Self {
            host,
            ring,
            stop,
            failure,
            mixer: Some(mixer),
            start_sample,
        })
    }

    /// The project sample the listener hears now: where playing started, plus what the device
    /// took, less the output's latency.
    #[must_use]
    pub fn heard_sample(&self) -> i64 {
        let taken = i64::try_from(self.ring.read.load(Ordering::Acquire)).unwrap_or(i64::MAX);
        self.start_sample + taken - i64::from(self.host.latency_samples())
    }

    /// Output frames that played silent because the mix fell behind, plus stream errors.
    #[must_use]
    pub fn shortfall(&self) -> u64 {
        self.ring.short.load(Ordering::Relaxed) + self.host.underruns()
    }

    /// The mix's last failure, `None` while every block mixed.
    #[must_use]
    pub fn last_error(&self) -> Option<MixFailure> {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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
        let message = payload
            .downcast_ref::<&str>()
            .map(|message| (*message).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic without a message".into());
        Err(CpalHostError::Device(format!(
            "this process has no audio output: {message}"
        )))
    })
}

impl Drop for PreviewAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(mixer) = self.mixer.take() {
            let _ = mixer.join();
        }
    }
}

/// Mixes ahead of the callback until stopped, or until one block fails
/// [`MAX_CONSECUTIVE_MIX_FAILURES`] times in a row.
fn mix(
    snapshot: &EngineProject,
    sources: &DecodedAudioSources,
    renderer: &mut AudioRenderer,
    ring: &Ring,
    stop: &AtomicBool,
    failure: &Mutex<Option<MixFailure>>,
    start_sample: i64,
) {
    let mut next = start_sample;
    let mut consecutive = 0_u32;
    while !stop.load(Ordering::Acquire) {
        let written = ring.written.load(Ordering::Acquire);
        let free = RING_FRAMES as u64 - (written - ring.read.load(Ordering::Acquire));
        if free < MIX_BLOCK as u64 {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        let status =
            match renderer.render(snapshot, next, MIX_BLOCK, sources, TransportState::Playing) {
                Ok(status) => status,
                Err(error) => {
                    consecutive += 1;
                    let stopped = consecutive >= MAX_CONSECUTIVE_MIX_FAILURES;
                    *failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(MixFailure {
                        message: error.to_string(),
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
            && let Some(last) = failure
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_mut()
        {
            last.consecutive = 0;
        }
        let block = renderer.block(&status);
        let frames = block.channels.first().map_or(0, Vec::len);
        for index in 0..frames {
            let left = block.channels[0][index];
            let right = block.channels.get(1).map_or(left, |plane| plane[index]);
            ring.put(written + index as u64, left, right);
        }
        ring.written
            .store(written + frames as u64, Ordering::Release);
        next += i64::try_from(frames).unwrap_or(0);
    }
}
