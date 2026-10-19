//! Fixed mixes with bit-identical native and wasm hashes.
//!
//! Integer-generated input covers the resampler that brings a source to the
//! project rate, effects, fades, pan, retime, ducking, cubic-Bezier
//! automation, the fold of a 5.1 source into stereo and of every source into
//! mono, loudness normalization, the replay a seek runs, and limiting, at the
//! source rate and at 44.1 kHz in uneven blocks. Mixing uses `libm` without
//! fused multiply-add to keep output independent of platform math libraries.
//! `src/main.rs` prints native hashes; `scripts/test-audio-determinism.mjs`
//! compares them with the wasm exports, and `scripts/test-wasm-browser.mjs`
//! (`fixed-audio-mix`) plays the stereo mix through the packaged engine from
//! `tests/golden/audio_mix.json`.

use alpha2_audio::TransportState;
use alpha2_engine::{AudioRenderer, EngineProject};
use alpha2_media::{AudioBlock, AudioBlockProvider, AudioSpan, SourceId};
use alpha2_model::{
    Animated, AudioEffect, Clip, ClipId, CubicBezier, Ducking, EqBand, EqBandKind, FadeShape,
    Interpolation, Keyframe, Limiter, LoopMode, MediaKind, MediaSource, Project, ProjectId,
    ProjectSettings, TimeMap, Timeline, Track, TrackId, TrackKind,
};
use alpha2_time::{FrameRate, MediaTime, TICKS_PER_SECOND, TimeRange};

const RATE: u32 = 48_000;
const BLOCK: usize = 1_024;
const SECONDS: i64 = 2;

/// The static gain the fixed mix plays at, as an export normalizing its
/// loudness sets it.
pub const NORMALIZATION_GAIN_DB: f64 = -4.5;

/// How the fixed mix is played, as `[start, end)` sample runs, each rendered
/// in [`BLOCK`]-sample blocks: from the start, a seek forward over the voice
/// line, then a seek back into it. Each seek replays the history the
/// renderer asks for before its first block.
pub const PLAYBACK: [(i64, i64); 3] = [(0, 38_400), (67_200, 96_000), (19_200, 57_600)];

/// FNV-1a over the bits of every output sample of the fixed project played
/// as [`PLAYBACK`] says at [`NORMALIZATION_GAIN_DB`], left channel then
/// right, block by block.
///
/// # Panics
/// Panics if the engine refuses the fixed project, which is a bug here.
#[must_use]
pub fn render_hash() -> u64 {
    mix_hash(project(), 2, &PLAYBACK, NORMALIZATION_GAIN_DB, &[BLOCK])
}

/// The project rate [`rate_and_blocks_hash`] mixes at.
pub const RATE_44K: u32 = 44_100;

/// [`PLAYBACK`] at [`RATE_44K`]: the same seconds, in project samples.
pub const PLAYBACK_44K: [(i64, i64); 3] = [(0, 35_280), (61_740, 88_200), (17_640, 52_920)];

/// The block sizes [`rate_and_blocks_hash`] renders in, in turn: a full
/// block, one sample, a hundredth of a second, a power of two, a prime and an
/// odd remainder, so block edges fall everywhere against the resampler's
/// phases and the effects' internal blocks.
pub const UNEVEN_BLOCKS: [usize; 7] = [BLOCK, 1, 441, 64, 997, 300, 7];

/// [`render_hash`] for the same project and sources at [`RATE_44K`], where
/// every rate-derived coefficient (filters, reverb, envelopes, fades, the
/// lookahead, retime positions) takes other values, played as
/// [`PLAYBACK_44K`] in [`UNEVEN_BLOCKS`] rather than in whole blocks.
///
/// # Panics
/// Panics if the engine refuses the fixed project, which is a bug here.
#[must_use]
pub fn rate_and_blocks_hash() -> u64 {
    mix_hash(
        project_at(RATE_44K),
        2,
        &PLAYBACK_44K,
        NORMALIZATION_GAIN_DB,
        &UNEVEN_BLOCKS,
    )
}

/// [`render_hash`] for a mono render of the same project: every source,
/// stereo and 5.1 included, folds to one channel.
///
/// # Panics
/// Panics if the engine refuses the fixed project, which is a bug here.
#[must_use]
pub fn mono_hash() -> u64 {
    mix_hash(project(), 1, &PLAYBACK, NORMALIZATION_GAIN_DB, &[BLOCK])
}

/// The same hash, played straight through without normalization, for clips
/// that play their sources reversed at three quarters speed, looped with a
/// fade at each wrap, and looped back and forth: every mixer piece but plain
/// forward play.
///
/// # Panics
/// Panics if the engine refuses the fixed project, which is a bug here.
#[must_use]
pub fn time_map_hash() -> u64 {
    mix_hash(
        time_map_project(),
        2,
        &[(0, SECONDS * i64::from(RATE))],
        0.0,
        &[BLOCK],
    )
}

/// Renders `playback` taking each block's size from `blocks` in turn (each
/// at most [`BLOCK`]), cut short at the end of a run, and hashes each run in
/// [`BLOCK`]-sample pieces from its start, left channel then right: the hash
/// is of the samples, the same whichever blocks rendered them.
fn mix_hash(
    project: Project,
    channels: usize,
    playback: &[(i64, i64)],
    gain_db: f64,
    blocks: &[usize],
) -> u64 {
    let sources = Sources::at(project.settings.sample_rate);
    let snapshot = EngineProject::new(project).expect("valid project");
    let mut renderer = AudioRenderer::new(&snapshot, channels, BLOCK).expect("renderer");
    renderer
        .set_normalization_gain_db(gain_db)
        .expect("normalization gain");
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut sizes = blocks.iter().cycle();
    for &(mut start, end) in playback {
        let mut run = vec![Vec::new(); channels];
        while start < end {
            let size = *sizes.next().expect("at least one block size");
            assert!(size <= BLOCK, "the renderer takes blocks up to {BLOCK}");
            let frames = size.min(usize::try_from(end - start).expect("small"));
            let status = renderer
                .render(&snapshot, start, frames, &sources, TransportState::Playing)
                .expect("render");
            assert!(status.is_complete(), "every source is supplied");
            for (channel, plane) in run.iter_mut().enumerate() {
                plane.extend_from_slice(&renderer.channel(channel)[..frames]);
            }
            start += i64::try_from(frames).expect("small");
        }
        for piece in 0..run[0].len().div_ceil(BLOCK) {
            for plane in &run {
                let samples = &plane[piece * BLOCK..plane.len().min((piece + 1) * BLOCK)];
                for sample in samples {
                    for byte in sample.to_bits().to_le_bytes() {
                        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
                    }
                }
            }
        }
    }
    hash
}

/// What `scripts/test-wasm-browser.mjs` needs to play [`render_hash`]'s mix
/// through the packaged engine: the project document, how each source is
/// generated, the playback, and the hash. `tests/golden/audio_mix.json`
/// holds it; `cargo run -p alpha2-audio-determinism -- fixture` prints it.
///
/// # Panics
/// Panics if the fixed project does not serialize, which is a bug here.
#[must_use]
pub fn browser_fixture() -> serde_json::Value {
    serde_json::json!({
        "description": "The fixed stereo mix of tests/audio_determinism, for the packaged engine. Generated by `cargo run -p alpha2-audio-determinism -- fixture`; a test there keeps it equal to the native fixture.",
        "sampleRate": RATE,
        "block": BLOCK,
        "sourceFrames": Sources::FRAMES,
        "sources": [
            { "id": "noise", "generator": "noise", "channels": 2 },
            { "id": "triangle", "generator": "triangle", "channels": 1 },
            { "id": "surround", "generator": "saw", "channels": 6 },
        ],
        "normalizationGainDb": NORMALIZATION_GAIN_DB,
        "playback": PLAYBACK,
        "project": serde_json::to_value(project()).expect("project serializes"),
        "expected": format!("{:016x}", render_hash()),
    })
}

/// FNV-1a over every bucket of a multi-resolution waveform summary of the
/// noise source, then of [`signed_zeros`], each fed in mix-sized blocks.
///
/// # Panics
/// Panics if the fixed summary shape is refused, which is a bug here.
#[must_use]
pub fn waveform_hash() -> u64 {
    let sources = Sources::new();
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for planes in [sources.noise.channels.clone(), signed_zeros()] {
        let mut summary = alpha2_audio::WaveformSummary::new(planes.len(), 100, 5).expect("shape");
        let frames = planes[0].len();
        let mut start = 0;
        while start < frames {
            let end = (start + BLOCK).min(frames);
            let block = planes
                .iter()
                .map(|plane| &plane[start..end])
                .collect::<Vec<_>>();
            summary.push(&block).expect("block");
            start = end;
        }
        for level in summary.finish().levels {
            for channel in level.channels {
                for bucket in channel {
                    for value in [bucket.min, bucket.max, bucket.rms] {
                        for byte in value.to_bits().to_le_bytes() {
                            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
                        }
                    }
                }
            }
        }
    }
    hash
}

/// Two channels of silence written with both zeros: stretches of +0 and -0
/// in either order, a +0/-0 alternation, and buckets whose only nonzero
/// sample is one extreme, so a zero is the other. A target whose min and max
/// pick another zero when the two compare equal hashes differently.
fn signed_zeros() -> Vec<Vec<f32>> {
    let frames = 100 * 64;
    let left = (0..frames)
        .map(|index| match (index / 100) % 4 {
            0 => {
                if index % 2 == 0 {
                    0.0
                } else {
                    -0.0
                }
            }
            1 => {
                if index % 100 < 50 {
                    -0.0
                } else {
                    0.0
                }
            }
            2 if index % 100 == 37 => -0.25,
            3 if index % 100 == 61 => 0.25,
            2 => 0.0,
            _ => -0.0,
        })
        .collect::<Vec<_>>();
    let right = left.iter().map(|sample| -sample).collect();
    vec![left, right]
}

/// FNV-1a over two loudness reports (integrated, range, true and sample
/// peak, momentary and short-term maxima): the fixed mix's, measured by the
/// engine, and [`stepped_program`]'s, measured by a meter alone. The mix is
/// two seconds, too short for a short-term window (three) and so for a
/// loudness range; the stepped program measures both, through the
/// short-term windows, the range's gating and its percentiles.
///
/// # Panics
/// Panics if the fixed project is refused, or a report leaves a field out,
/// which is a bug here.
#[must_use]
pub fn loudness_hash() -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for (index, report) in loudness_reports().iter().enumerate() {
        for value in [
            report.integrated_lufs,
            report.loudness_range_lu,
            report.true_peak_dbtp,
            report.sample_peak_dbfs,
            report.momentary_max_lufs,
            report.short_term_max_lufs,
        ] {
            // The mix measures no short-term window and so no range; every other value must be
            // there to be compared.
            let value = value.or_else(|| {
                assert_eq!(
                    index, 0,
                    "the stepped program measures every value: {report:?}"
                );
                None
            });
            for byte in value.unwrap_or(f64::NAN).to_bits().to_le_bytes() {
                hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    hash
}

/// The loudness reports [`loudness_hash`] covers: the fixed mix, then
/// [`stepped_program`].
///
/// # Panics
/// Panics if the fixed project is refused, which is a bug here.
#[must_use]
pub fn loudness_reports() -> [alpha2_audio::LoudnessReport; 2] {
    let snapshot = EngineProject::new(project()).expect("valid project");
    let sources = Sources::new();
    let mut renderer = AudioRenderer::new(&snapshot, 2, BLOCK).expect("renderer");
    renderer.set_loudness_metering(true).expect("meter");
    let total = usize::try_from(SECONDS).expect("small") * RATE as usize;
    let mut start = 0;
    while start < total {
        let frames = BLOCK.min(total - start);
        renderer
            .render(
                &snapshot,
                i64::try_from(start).expect("small"),
                frames,
                &sources,
                TransportState::Playing,
            )
            .expect("render");
        start += frames;
    }
    let mix = renderer.loudness_report().expect("metering");
    let program = stepped_program(&sources);
    let mut meter = alpha2_audio::LoudnessMeter::new(RATE, 2).expect("stereo meter");
    for start in (0..program[0].len()).step_by(BLOCK) {
        let end = (start + BLOCK).min(program[0].len());
        meter
            .push(&[&program[0][start..end], &program[1][start..end]])
            .expect("block");
    }
    [mix, meter.report()]
}

/// Twelve seconds of the noise source, looped, at a level that steps every
/// two seconds over 34 dB: long enough for short-term windows, and spread
/// enough that the loudness range gates some away and takes percentiles of
/// the rest. Each sample is one correctly rounded product.
fn stepped_program(sources: &Sources) -> [Vec<f32>; 2] {
    const LEVELS: [f32; 6] = [0.5, 0.05, 0.2, 0.01, 0.35, 0.1];
    let frames = RATE as usize * 12;
    let plane = |channel: usize| {
        let noise = &sources.noise.channels[channel];
        (0..frames)
            .map(|index| noise[index % noise.len()] * LEVELS[index / (RATE as usize * 2)])
            .collect::<Vec<_>>()
    };
    [plane(0), plane(1)]
}

/// FNV-1a over the noise source's left channel converted by
/// [`alpha2_audio::Resampler`] from 48 kHz to each of a set of rates, rising,
/// falling and sharing too many phases to keep, in uneven pieces.
///
/// # Panics
/// Panics if a fixed rate is refused, which is a bug here.
#[must_use]
pub fn resample_hash() -> u64 {
    let sources = Sources::new();
    let source = &sources.noise.channels[0];
    let length = i64::try_from(source.len()).expect("small");
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for target in [44_100, 96_000, 8_000, 47_999] {
        let resampler = alpha2_audio::Resampler::new(RATE, target).expect("rates");
        let end = resampler.target_at_or_after(length);
        let mut start = 0;
        let mut piece = 0;
        while start < end {
            let stop = (start + 113 + piece % 7 * 300).min(end);
            let span = resampler.source_span(start..stop);
            let first = span.start.clamp(0, length);
            let last = span.end.clamp(0, length);
            let mut output = vec![0.0; usize::try_from(stop - start).expect("small")];
            resampler.resample(
                &source
                    [usize::try_from(first).expect("index")..usize::try_from(last).expect("index")],
                first,
                start,
                &mut output,
            );
            for sample in output {
                for byte in sample.to_bits().to_le_bytes() {
                    hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
                }
            }
            start = stop;
            piece += 1;
        }
    }
    hash
}

/// The wasm entry point for [`resample_hash`].
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_resample_hash() -> u64 {
    resample_hash()
}

/// The wasm entry point for [`rate_and_blocks_hash`].
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_rate_and_blocks_hash() -> u64 {
    rate_and_blocks_hash()
}

/// The wasm entry point for [`mono_hash`].
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_mono_hash() -> u64 {
    mono_hash()
}

/// The wasm entry point for [`time_map_hash`].
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_time_map_hash() -> u64 {
    time_map_hash()
}

/// The wasm entry point for [`loudness_hash`].
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_loudness_hash() -> u64 {
    loudness_hash()
}

/// The wasm entry point for [`waveform_hash`].
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_waveform_hash() -> u64 {
    waveform_hash()
}

/// The wasm entry point for [`render_hash`], callable by a host without bindings.
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_audio_determinism_hash() -> u64 {
    render_hash()
}

/// Three sources generated with integer arithmetic and single correctly
/// rounded operations only, so the input is the same everywhere before the
/// engine touches it (`scripts/test-wasm-browser.mjs` generates them again).
struct Sources {
    noise: AudioBlock,
    triangle: AudioBlock,
    /// 5.1 in the default order (front left and right, center, LFE, back
    /// left and right), a sawtooth of its own period on each plane.
    surround: AudioBlock,
}

impl Sources {
    const FRAMES: usize = RATE as usize * 3;

    fn new() -> Self {
        Self::at(RATE)
    }

    /// The same generators at `rate`, three seconds long: the engine mixes
    /// sources at the project rate, which a host resamples them to first.
    fn at(rate: u32) -> Self {
        let frames = rate as usize * 3;
        let mut state = 0x1234_5678_u32;
        let mut noise = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            #[allow(clippy::cast_precision_loss)]
            let unit = f32::from(u16::try_from(state >> 16).expect("16 bits")) / 65_535.0;
            unit - 0.5
        };
        let left = (0..frames).map(|_| noise()).collect::<Vec<_>>();
        let right = (0..frames).map(|_| noise()).collect::<Vec<_>>();
        let triangle = (0..frames)
            .map(|index| {
                let phase = u16::try_from(index % 200).expect("small");
                let rising = if phase < 100 { phase } else { 200 - phase };
                f32::from(rising) / 100.0 - 0.5
            })
            .collect::<Vec<_>>();
        let block = |channels: Vec<Vec<f32>>| AudioBlock {
            start_sample: 0,
            sample_rate: rate,
            speakers: alpha2_media::SpeakerLayout::default_for(channels.len()).expect("layout"),
            channels,
        };
        let surround = (0..6_usize)
            .map(|plane| {
                (0..frames)
                    .map(|index| {
                        let phase = u16::try_from(index * (2 * plane + 3) % 400).expect("small");
                        f32::from(phase) / 400.0 - 0.5
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        Self {
            noise: block(vec![left, right]),
            triangle: block(vec![triangle]),
            surround: block(surround),
        }
    }
}

impl AudioBlockProvider for Sources {
    fn block(
        &self,
        source: &SourceId,
        _start_sample: i64,
        _frames: usize,
    ) -> Option<AudioSpan<'_>> {
        match source.as_str() {
            "noise" => Some(AudioSpan::whole(&self.noise)),
            "triangle" => Some(AudioSpan::whole(&self.triangle)),
            "surround" => Some(AudioSpan::whole(&self.surround)),
            _ => None,
        }
    }
}

fn ticks(seconds_times_ten: i64) -> MediaTime {
    MediaTime::from_ticks(seconds_times_ten * TICKS_PER_SECOND / 10)
}

fn range(from: i64, to: i64) -> TimeRange {
    TimeRange::new(ticks(from), ticks(to)).expect("range")
}

fn id<T>(make: fn(String) -> Result<T, alpha2_media::IdError>, value: &str) -> T {
    make(value.to_owned()).expect("ID")
}

fn project() -> Project {
    project_at(RATE)
}

/// The fixed project mixed at `rate`.
fn project_at(rate: u32) -> Project {
    let mut project = Project::new(
        id(ProjectId::new, "determinism"),
        "determinism",
        ProjectSettings::new(16, 16, FrameRate::new(30, 1).expect("rate"), rate).expect("settings"),
    );
    project.sources = vec![
        MediaSource::new(
            id(SourceId::new, "noise"),
            "synthetic://noise",
            MediaKind::Audio,
        ),
        MediaSource::new(
            id(SourceId::new, "triangle"),
            "synthetic://triangle",
            MediaKind::Audio,
        ),
        MediaSource::new(
            id(SourceId::new, "surround"),
            "synthetic://surround",
            MediaKind::Audio,
        ),
    ];
    project.timeline = Timeline {
        tracks: vec![music(), voice(), surround()],
    };
    project.audio.gain = Animated::Constant { value: 0.9 };
    project.audio.effects = vec![
        AudioEffect::Reverb {
            room_size: 0.4,
            damping: 0.5,
            mix: 0.1,
        },
        AudioEffect::Compressor {
            threshold_db: -12.0,
            ratio: 2.0,
            attack_ms: 10.0,
            release_ms: 120.0,
            lookahead_ms: 0.0,
        },
    ];
    project.audio.limiter = Some(Limiter {
        threshold: 0.8,
        release_ms: 50.0,
    });
    project
}

/// The triangle reversed at three quarters speed, the noise repeated every
/// 0.3 s, and the triangle ping-ponging every quarter second, on one track.
fn time_map_project() -> Project {
    let mut project = project();
    let mut track = Track::new(id(TrackId::new, "mapped"), "Mapped", TrackKind::Audio);
    let clip = |name: &str, source: &str, timeline: TimeRange, source_range, time_map| {
        let mut clip = Clip::new(
            id(ClipId::new, name),
            id(SourceId::new, source),
            timeline,
            source_range,
        );
        clip.time_map = time_map;
        clip
    };
    track.clips = vec![
        clip(
            "reversed",
            "triangle",
            range(0, 8),
            TimeRange::new(
                MediaTime::ZERO,
                MediaTime::from_ticks(TICKS_PER_SECOND * 6 / 10),
            )
            .expect("range"),
            TimeMap::Reverse,
        ),
        clip(
            "repeated",
            "noise",
            range(8, 20),
            range(0, 3),
            TimeMap::Loop {
                mode: LoopMode::Repeat,
            },
        ),
        clip(
            "ping-pong",
            "triangle",
            range(12, 20),
            TimeRange::new(MediaTime::ZERO, MediaTime::from_ticks(TICKS_PER_SECOND / 4))
                .expect("range"),
            TimeMap::Loop {
                mode: LoopMode::PingPong,
            },
        ),
    ];
    project.timeline = Timeline {
        tracks: vec![track],
    };
    project
}

/// Noise with fades, a pan sweep, an EQ, a lookahead compressor, and ducking.
fn music() -> Track {
    let mut music = Track::new(id(TrackId::new, "music"), "Music", TrackKind::Audio);
    let mut bed = Clip::new(
        id(ClipId::new, "bed"),
        id(SourceId::new, "noise"),
        range(0, 20),
        range(0, 20),
    );
    bed.audio.fade_in = ticks(2);
    bed.audio.fade_out = ticks(3);
    bed.audio.fade_shape = FadeShape::EqualPower;
    bed.audio.pan = Animated::Keyframes {
        keyframes: vec![
            // An eased sweep: the cubic-Bezier solver runs at each control point, and at every
            // sample in the intervals that hold a keyframe.
            Keyframe {
                time: ticks(0),
                value: -1.0,
                interpolation: Interpolation::CubicBezier(CubicBezier::new(0.42, 0.0, 0.58, 1.0)),
            },
            Keyframe {
                time: ticks(20),
                value: 1.0,
                interpolation: Interpolation::Linear,
            },
        ],
    };
    bed.audio.effects = vec![AudioEffect::Eq {
        bands: [
            (EqBandKind::Peaking, 1_000.0, 3.0),
            (EqBandKind::LowPass, 8_000.0, 0.0),
            (EqBandKind::HighPass, 60.0, 0.0),
        ]
        .into_iter()
        .map(|(kind, frequency_hz, gain_db)| EqBand {
            kind,
            frequency_hz,
            q: 0.7,
            gain_db,
        })
        .collect(),
    }];
    music.clips.push(bed);
    music.audio.effects = vec![AudioEffect::Compressor {
        threshold_db: -20.0,
        ratio: 4.0,
        attack_ms: 5.0,
        release_ms: 80.0,
        lookahead_ms: 5.0,
    }];
    music.audio.ducking = Some(Ducking {
        sidechain_track: id(TrackId::new, "voice"),
        threshold: 0.1,
        depth_db: -12.0,
        attack_ms: 10.0,
        release_ms: 200.0,
    });

    music
}

/// The 5.1 source over the middle of the mix, quieter, faded in: every
/// surround plane but the LFE folds into each output channel.
fn surround() -> Track {
    let mut track = Track::new(id(TrackId::new, "surround"), "Surround", TrackKind::Audio);
    let mut bed = Clip::new(
        id(ClipId::new, "room"),
        id(SourceId::new, "surround"),
        range(3, 17),
        range(0, 14),
    );
    bed.audio.fade_in = ticks(2);
    track.clips.push(bed);
    track.audio.gain = Animated::Constant { value: 0.5 };
    track
}

/// A triangle at half speed with a reverb, the ducking sidechain.
fn voice() -> Track {
    let mut voice = Track::new(id(TrackId::new, "voice"), "Voice", TrackKind::Audio);
    // Half the source over the clip's second: a retime at half speed.
    let mut line = Clip::new(
        id(ClipId::new, "line"),
        id(SourceId::new, "triangle"),
        range(5, 15),
        range(0, 5),
    );
    line.audio.fade_in = ticks(1);
    line.audio.fade_out = ticks(1);
    voice.clips.push(line);
    voice.audio.effects = vec![AudioEffect::Reverb {
        room_size: 0.7,
        damping: 0.3,
        mix: 0.3,
    }];
    voice.audio.pan = Animated::Constant { value: 0.25 };
    // An overshooting curve on the bus gain: past its keyframes, then back.
    voice.audio.gain = Animated::Keyframes {
        keyframes: vec![
            Keyframe {
                time: ticks(5),
                value: 0.2,
                interpolation: Interpolation::CubicBezier(CubicBezier::new(0.34, 1.56, 0.64, 1.0)),
            },
            Keyframe {
                time: ticks(12),
                value: 1.0,
                interpolation: Interpolation::Hold,
            },
        ],
    };
    voice
}

#[cfg(test)]
mod tests {
    /// The hash every platform must produce. A change to the engine's audio
    /// output changes it on purpose; update it together with that change,
    /// never to make one platform pass.
    const EXPECTED: u64 = 0x1550_6d42_6e7e_36e2;

    /// The loudness report of the fixed mix, likewise.
    const EXPECTED_LOUDNESS: u64 = 0x078b_c676_bc76_f77c;

    /// The hash of the time-mapped mix.
    const EXPECTED_TIME_MAPS: u64 = 0x0be4_6a89_9877_69ef;

    #[test]
    fn the_time_mapped_mix_hashes_to_the_same_value_everywhere() {
        assert_eq!(
            super::time_map_hash(),
            EXPECTED_TIME_MAPS,
            "{:016x}",
            super::time_map_hash()
        );
    }

    #[test]
    fn the_loudness_pin_compares_a_loudness_range_and_a_short_term_maximum() {
        let [_, program] = super::loudness_reports();
        assert!(
            program.loudness_range_lu.is_some_and(|range| range > 1.0)
                && program.short_term_max_lufs.is_some(),
            "{program:?}"
        );
    }

    #[test]
    fn the_loudness_report_hashes_to_the_same_value_everywhere() {
        assert_eq!(
            super::loudness_hash(),
            EXPECTED_LOUDNESS,
            "{:016x}",
            super::loudness_hash()
        );
    }

    /// The waveform summary of the fixed noise and the signed zeros, likewise.
    const EXPECTED_WAVEFORM: u64 = 0x8213_35d4_9cce_921c;

    #[test]
    fn the_waveform_summary_hashes_to_the_same_value_everywhere() {
        assert_eq!(
            super::waveform_hash(),
            EXPECTED_WAVEFORM,
            "{:016x}",
            super::waveform_hash()
        );
    }

    /// The resampled noise, likewise.
    const EXPECTED_RESAMPLE: u64 = 0x7364_749d_ade4_cb96;

    #[test]
    fn the_resampled_source_hashes_to_the_same_value_everywhere() {
        assert_eq!(
            super::resample_hash(),
            EXPECTED_RESAMPLE,
            "{:016x}",
            super::resample_hash()
        );
    }

    /// The mono render of the fixed project, likewise.
    const EXPECTED_MONO: u64 = 0xdafb_39b8_0e1b_d675;

    #[test]
    fn the_mono_mix_hashes_to_the_same_value_everywhere() {
        assert_eq!(
            super::mono_hash(),
            EXPECTED_MONO,
            "{:016x}",
            super::mono_hash()
        );
    }

    /// The packaged engine is checked against the native mix from this file,
    /// so it must describe exactly the native fixture and its pinned hash.
    #[test]
    fn the_browser_fixture_is_the_native_fixed_mix() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../golden/audio_mix.json");
        let golden: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(path).expect("tests/golden/audio_mix.json"),
        )
        .expect("JSON");
        assert_eq!(
            golden,
            super::browser_fixture(),
            "regenerate with `cargo run -p alpha2-audio-determinism -- fixture > tests/golden/audio_mix.json`"
        );
        assert_eq!(golden["expected"], format!("{EXPECTED:016x}"));
        // The document the browser loads is the project the native mix renders.
        let project = alpha2_model::Project::from_json(&golden["project"].to_string())
            .expect("the fixture project loads");
        assert_eq!(project, super::project());
    }

    /// The fixed project at 44.1 kHz in uneven blocks, likewise.
    const EXPECTED_RATE_AND_BLOCKS: u64 = 0x3aec_1fd8_9672_9084;

    #[test]
    fn the_44_1_khz_mix_in_uneven_blocks_hashes_to_the_same_value_everywhere() {
        assert_eq!(
            super::rate_and_blocks_hash(),
            EXPECTED_RATE_AND_BLOCKS,
            "{:016x}",
            super::rate_and_blocks_hash()
        );
    }

    /// The mix, seeks and their replays included, is the same whichever
    /// blocks render it, so the uneven-block pin covers the rate and not a
    /// second copy of the mix.
    #[test]
    fn uneven_blocks_mix_what_whole_blocks_do_at_either_rate() {
        use super::{BLOCK, NORMALIZATION_GAIN_DB, UNEVEN_BLOCKS, mix_hash, project_at};
        assert_eq!(
            super::rate_and_blocks_hash(),
            mix_hash(
                project_at(super::RATE_44K),
                2,
                &super::PLAYBACK_44K,
                NORMALIZATION_GAIN_DB,
                &[BLOCK],
            )
        );
        assert_eq!(
            super::render_hash(),
            mix_hash(
                project_at(super::RATE),
                2,
                &super::PLAYBACK,
                NORMALIZATION_GAIN_DB,
                &UNEVEN_BLOCKS,
            )
        );
    }

    #[test]
    fn the_fixed_mix_hashes_to_the_same_value_everywhere() {
        assert_eq!(
            super::render_hash(),
            EXPECTED,
            "{:016x}",
            super::render_hash()
        );
    }
}
