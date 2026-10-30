use super::*;

use alpha2_media::{AudioSpan, SourceId, sample_ticks_floor};
use alpha2_model::{
    Animated, Clip, Ducking, Interpolation, Keyframe, Limiter, MediaKind, MediaSource, Project,
    ProjectId, ProjectSettings, Timeline, Track, TrackId,
};
use alpha2_time::FrameRate;

mod automation;
mod buses_and_ducking;
mod continuity;
mod edge_cases;
mod effect_budget;
mod exactness;
mod held_renders;
mod lookahead_and_settling;
mod mixing;
mod needs;
mod normalization;
mod retiming;
mod source_blocks;
mod stretched;

/// Returns the sample index containing a tick position.
fn sample_floor(ticks: i64, sample_rate: u32) -> i64 {
    let rate = i128::from(sample_rate.max(1));
    let value = (i128::from(ticks) * rate).div_euclid(i128::from(TICKS_PER_SECOND));
    i64::try_from(value).unwrap_or(i64::MAX)
}

const RATE: u32 = 48_000;

/// Length of [`Fixture::ramp`]: four seconds, as every fixture holds.
const RAMP_SAMPLES: usize = RATE as usize * 4;

/// Supplies constant-valued source blocks covering the whole fixture range.
///
/// Every block starts at the source origin, which exercises the renderer's
/// absolute-position indexing rather than assuming request-aligned blocks.
struct Fixture {
    blocks: Vec<(SourceId, AudioBlock)>,
}

impl Fixture {
    fn new(sources: Vec<(&str, f32, usize)>) -> Self {
        Self {
            blocks: sources
                .into_iter()
                .map(|(id, value, channels)| {
                    (
                        SourceId::new(id.to_owned()).expect("id"),
                        AudioBlock {
                            start_sample: 0,
                            sample_rate: RATE,
                            speakers: SpeakerLayout::default_for(channels).expect("layout"),
                            channels: (0..channels)
                                .map(|_| vec![value; RATE as usize * 4])
                                .collect(),
                        },
                    )
                })
                .collect(),
        }
    }

    /// One source whose planes hold `values`, one constant per plane.
    fn planes(id: &str, values: &[f32]) -> Self {
        Self {
            blocks: vec![(
                SourceId::new(id.to_owned()).expect("id"),
                AudioBlock {
                    start_sample: 0,
                    sample_rate: RATE,
                    speakers: SpeakerLayout::default_for(values.len()).expect("layout"),
                    channels: values
                        .iter()
                        .map(|value| vec![*value; RATE as usize * 4])
                        .collect(),
                },
            )],
        }
    }

    /// One source whose every plane is a ramp: sample `i` holds `i / RAMP_SAMPLES`, so a
    /// value names the source position it came from.
    fn ramp(id: &str, channels: usize) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let plane: Vec<f32> = (0..RAMP_SAMPLES)
            .map(|sample| sample as f32 / RAMP_SAMPLES as f32)
            .collect();
        Self {
            blocks: vec![(
                SourceId::new(id.to_owned()).expect("id"),
                AudioBlock {
                    start_sample: 0,
                    sample_rate: RATE,
                    speakers: SpeakerLayout::default_for(channels).expect("layout"),
                    channels: vec![plane; channels],
                },
            )],
        }
    }

    fn without(mut self, id: &str) -> Self {
        let missing = SourceId::new(id.to_owned()).expect("id");
        self.blocks.retain(|(source, _)| *source != missing);
        self
    }
}

impl AudioBlockProvider for Fixture {
    fn block(
        &self,
        source: &SourceId,
        _start_sample: i64,
        _frames: usize,
    ) -> Option<AudioSpan<'_>> {
        self.blocks
            .iter()
            .find(|(id, _)| id == source)
            .map(|(_, block)| AudioSpan::whole(block))
    }
}

fn audio_clip(id: &str, source: &str, start: i64, end: i64) -> Clip {
    Clip::new(
        alpha2_model::ClipId::new(id.to_owned()).expect("id"),
        SourceId::new(source.to_owned()).expect("id"),
        TimeRange::new(MediaTime(start), MediaTime(end)).expect("range"),
        TimeRange::new(MediaTime(0), MediaTime(end - start)).expect("range"),
    )
}

fn project(tracks: Vec<Track>, sources: Vec<&str>) -> Project {
    Project {
        schema_version: alpha2_model::CURRENT_SCHEMA_VERSION,
        id: ProjectId::new("audio-test".to_owned()).expect("id"),
        name: "audio".into(),
        settings: ProjectSettings::new(16, 16, FrameRate::new(30, 1).expect("rate"), RATE)
            .expect("settings"),
        sources: sources
            .into_iter()
            .map(|id| {
                MediaSource::new(
                    SourceId::new(id.to_owned()).expect("id"),
                    format!("asset://{id}"),
                    MediaKind::Audio,
                )
            })
            .collect(),
        effect_resources: Vec::new(),
        compositions: Vec::new(),
        timeline: Timeline { tracks },
        audio: alpha2_model::MasterAudio {
            limiter: None,
            ..alpha2_model::MasterAudio::default()
        },
    }
}

fn track(id: &str, clips: Vec<Clip>) -> Track {
    let mut track = Track::new(
        TrackId::new(id.to_owned()).expect("id"),
        id,
        alpha2_model::TrackKind::Audio,
    );
    track.clips = clips;
    track
}

fn render(
    project: Project,
    frames: usize,
    fixture: &Fixture,
) -> (AudioRenderer, AudioRenderStatus) {
    let snapshot = EngineProject::new(project).expect("valid project");
    let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    let status = renderer
        .render(&snapshot, 0, frames, fixture, TransportState::Playing)
        .expect("render");
    (renderer, status)
}

/// Ducks `track` from `key`, hard and fast.
fn duck(project: &mut Project, track: usize, key: &str) {
    project.timeline.tracks[track].audio.ducking = Some(Ducking {
        sidechain_track: TrackId::new(key.to_owned()).expect("id"),
        threshold: 0.05,
        depth_db: -24.0,
        attack_ms: 1.0,
        release_ms: 50.0,
    });
}

/// A 1 kHz stereo sine at `amplitude` for `seconds`.
struct Sine(Vec<(SourceId, AudioBlock)>);

impl Sine {
    fn new(amplitude: f32, seconds: usize) -> Self {
        let plane = (0..RATE as usize * seconds)
            .map(|index| {
                #[allow(clippy::cast_precision_loss)]
                let phase = index as f32 * 1_000.0 / RATE as f32 * core::f32::consts::TAU;
                amplitude * libm::sinf(phase)
            })
            .collect::<Vec<_>>();
        Self(vec![(
            SourceId::new("tone").expect("id"),
            AudioBlock {
                start_sample: 0,
                sample_rate: RATE,
                speakers: alpha2_media::SpeakerLayout::STEREO,
                channels: vec![plane.clone(), plane],
            },
        )])
    }
}

impl AudioBlockProvider for Sine {
    fn block(&self, source: &SourceId, _: i64, _: usize) -> Option<AudioSpan<'_>> {
        self.0
            .iter()
            .find(|(id, _)| id == source)
            .map(|(_, block)| AudioSpan::whole(block))
    }
}

/// Renders `[start, end)` in 4096-sample blocks with the meter on, and
/// the normalization gain `gain_db`, if any, set once it is.
fn meter(
    renderer: &mut AudioRenderer,
    snapshot: &EngineProject,
    provider: &Sine,
    (start, end): (i64, i64),
    gain_db: Option<f64>,
) -> LoudnessReport {
    renderer.set_loudness_metering(true).expect("meter");
    if let Some(gain_db) = gain_db {
        renderer.set_normalization_gain_db(gain_db).expect("gain");
    }
    let mut position = start;
    while position < end {
        let frames = usize::try_from((end - position).min(4_096)).expect("small");
        renderer
            .render(
                snapshot,
                position,
                frames,
                provider,
                TransportState::Playing,
            )
            .expect("render");
        position += i64::try_from(frames).expect("small");
    }
    renderer.loudness_report().expect("metering is on")
}

/// One source block holding `samples`, at `rate`, from sample zero.
struct Plane(AudioBlock);

impl AudioBlockProvider for Plane {
    fn block(
        &self,
        _source: &SourceId,
        _start_sample: i64,
        _frames: usize,
    ) -> Option<AudioSpan<'_>> {
        Some(AudioSpan::whole(&self.0))
    }
}

fn rate_project(rate: u32, clips: Vec<Clip>) -> EngineProject {
    let mut project = project(vec![track("a1", clips)], vec!["tone"]);
    project.settings =
        ProjectSettings::new(16, 16, FrameRate::new(30, 1).expect("rate"), rate).expect("settings");
    EngineProject::new(project).expect("valid project")
}

/// Renders `total` samples in blocks of `block`, left channel only.
fn render_all(
    snapshot: &EngineProject,
    provider: &dyn AudioBlockProvider,
    total: usize,
    block: usize,
) -> Vec<f32> {
    let mut renderer = AudioRenderer::new(snapshot, 2, block).expect("renderer");
    let mut output = Vec::with_capacity(total);
    while output.len() < total {
        let frames = block.min(total - output.len());
        renderer
            .render(
                snapshot,
                i64::try_from(output.len()).expect("small"),
                frames,
                provider,
                TransportState::Playing,
            )
            .expect("render");
        output.extend_from_slice(&renderer.channel(0)[..frames]);
    }
    output
}

/// A 10 ms lookahead compressor that never compresses: all latency.
fn lookahead() -> alpha2_model::AudioEffect {
    alpha2_model::AudioEffect::Compressor {
        threshold_db: 0.0,
        ratio: 1.0,
        attack_ms: 5.0,
        release_ms: 50.0,
        lookahead_ms: 10.0,
    }
}

fn impulse_at(index: usize, frames: usize) -> Plane {
    let mut plane = vec![0.0; frames];
    plane[index] = 0.5;
    Plane(AudioBlock {
        start_sample: 0,
        sample_rate: RATE,
        speakers: alpha2_media::SpeakerLayout::STEREO,
        channels: vec![plane.clone(), plane],
    })
}

const RATE_LOW: u32 = 8_000;

/// The project samples `[first, end)` every need lies within.
fn needed_samples(needs: &[AudioNeed], rate: u32) -> (usize, usize) {
    let first = needs
        .iter()
        .map(|need| sample_floor(need.source_range.start().ticks(), rate))
        .min()
        .expect("needs");
    let end = needs
        .iter()
        .map(|need| sample_at_or_after(need.source_range.end().ticks(), rate))
        .max()
        .expect("needs");
    (
        usize::try_from(first).expect("start"),
        usize::try_from(end).expect("end"),
    )
}

/// A long reverb on one track, a slow compressor after a clip EQ on
/// another, a ducker on a third sidechained to it, and a master
/// compressor with lookahead and the limiter: stages whose leads differ.
fn settling_project() -> EngineProject {
    let seconds = MediaTime::from_seconds(26).ticks();
    let mut verb = track("verb", vec![audio_clip("c1", "bursts", 0, seconds)]);
    verb.audio.effects = vec![alpha2_model::AudioEffect::Reverb {
        room_size: 0.9,
        damping: 0.1,
        mix: 0.5,
    }];
    let mut comp = track("comp", vec![audio_clip("c2", "steady", 0, seconds)]);
    comp.audio.effects = vec![alpha2_model::AudioEffect::Compressor {
        threshold_db: -20.0,
        ratio: 4.0,
        attack_ms: 10.0,
        release_ms: 300.0,
        lookahead_ms: 0.0,
    }];
    comp.clips[0].audio.effects = vec![alpha2_model::AudioEffect::Eq {
        bands: vec![alpha2_model::EqBand {
            kind: alpha2_model::EqBandKind::Peaking,
            frequency_hz: 700.0,
            q: 1.0,
            gain_db: 6.0,
        }],
    }];
    let mut duck = track("duck", vec![audio_clip("c3", "bursts", 0, seconds)]);
    duck.audio.ducking = Some(Ducking {
        sidechain_track: TrackId::new("comp").expect("id"),
        threshold: 0.05,
        depth_db: -12.0,
        attack_ms: 10.0,
        release_ms: 400.0,
    });
    let mut project = project(vec![verb, comp, duck], vec!["bursts", "steady"]);
    project.settings = ProjectSettings::new(16, 16, FrameRate::new(30, 1).expect("rate"), RATE_LOW)
        .expect("settings");
    project.audio.effects = vec![alpha2_model::AudioEffect::Compressor {
        threshold_db: -12.0,
        ratio: 2.0,
        attack_ms: 5.0,
        release_ms: 100.0,
        lookahead_ms: 5.0,
    }];
    project.audio.limiter = Some(Limiter {
        threshold: 0.5,
        release_ms: 50.0,
    });
    EngineProject::new(project).expect("project")
}

/// Mono sources at [`RATE_LOW`], holding only `first..end` of each plane.
fn low_rate_window(sources: &[(&str, Vec<f32>)], first: usize, end: usize) -> Fixture {
    Fixture {
        blocks: sources
            .iter()
            .map(|(id, plane)| {
                (
                    SourceId::new((*id).to_owned()).expect("id"),
                    AudioBlock {
                        start_sample: i64::try_from(first).expect("start"),
                        sample_rate: RATE_LOW,
                        speakers: alpha2_media::SpeakerLayout::MONO,
                        channels: vec![plane[first..end].to_vec()],
                    },
                )
            })
            .collect(),
    }
}

/// The sources of [`settling_project`], `total` samples at [`RATE_LOW`]:
/// noise in bursts of a second on and a second off, so envelopes attack
/// and release, and steady noise.
fn settling_sources(total: usize) -> [(&'static str, Vec<f32>); 2] {
    let mut state = 0x9e37_79b9_u32;
    #[allow(clippy::cast_precision_loss)]
    let mut noise = |gate: bool| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        if gate {
            (state as f32 / u32::MAX as f32 - 0.5) * 0.8
        } else {
            0.0
        }
    };
    let bursts: Vec<f32> = (0..total)
        .map(|index| noise((index / RATE_LOW as usize).is_multiple_of(2)))
        .collect();
    let steady: Vec<f32> = (0..total).map(|_| noise(true)).collect();
    [("bursts", bursts), ("steady", steady)]
}

/// Renders `frames` at each start in turn with a renderer under
/// `continuity`, returning the left channel of each block and how many
/// blocks the renderer mixed for it, replayed ones included.
fn render_steps(
    snapshot: &EngineProject,
    provider: &dyn AudioBlockProvider,
    continuity: AudioContinuity,
    starts: &[i64],
    frames: usize,
) -> Vec<(Vec<f32>, u64)> {
    let mut renderer = AudioRenderer::new(snapshot, 2, 512).expect("renderer");
    renderer.set_continuity(continuity);
    starts
        .iter()
        .map(|&start| {
            let before = renderer.blocks_mixed;
            let status = renderer
                .render(snapshot, start, frames, provider, TransportState::Playing)
                .expect("render");
            assert!(status.is_complete());
            (
                renderer.channel(0)[..frames].to_vec(),
                renderer.blocks_mixed - before,
            )
        })
        .collect()
}

/// Keyframes at `ticks` holding `values` in turn, each moving to the next
/// with `interpolation`.
fn keyframed(ticks: &[i64], values: &[f32], interpolation: Interpolation) -> Animated<f32> {
    Animated::Keyframes {
        keyframes: ticks
            .iter()
            .zip(values.iter().cycle())
            .map(|(&time, &value)| Keyframe {
                time: MediaTime(time),
                value,
                interpolation,
            })
            .collect(),
    }
}

/// A mono source of distinct values, two seconds long at [`RATE`].
fn distinct_plane() -> Plane {
    Plane(AudioBlock {
        start_sample: 0,
        sample_rate: RATE,
        speakers: SpeakerLayout::MONO,
        channels: vec![
            (0..RATE as usize * 2)
                .map(|index| {
                    #[allow(clippy::cast_precision_loss)]
                    let value = ((index * 7_919) % 1_000) as f32 / 1_000.0 - 0.5;
                    value
                })
                .collect(),
        ],
    })
}

fn mapped_clip(time_map: TimeMap, timeline_end: i64, source_end: i64) -> Clip {
    let mut clip = audio_clip("c1", "tone", 0, timeline_end);
    clip.source_range = TimeRange::new(MediaTime(0), MediaTime(source_end)).expect("range");
    clip.time_map = time_map;
    clip
}
