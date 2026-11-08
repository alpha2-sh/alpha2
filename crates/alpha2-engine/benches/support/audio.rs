//! Audio projects shared by the mixer benchmark and the allocation budget tests.

use alpha2_media::{AudioBlock, AudioBlockProvider, AudioSpan, SourceId};
use alpha2_model::{
    Animated, AudioEffect, Clip, ClipId, EqBand, EqBandKind, Interpolation, Keyframe, MasterAudio,
    MediaKind, MediaSource, Project, ProjectId, ProjectSettings, Timeline, Track, TrackId,
    TrackKind,
};
use alpha2_time::{FrameRate, MediaTime, TICKS_PER_SECOND, TimeRange};

/// Seconds of timeline every clip spans.
pub const SECONDS: i64 = 4;

/// A 440 Hz sine source.
pub struct Sine {
    block: AudioBlock,
}

impl Sine {
    /// A sine one second longer than every clip.
    pub fn new(rate: u32) -> Self {
        Self::lasting(rate, SECONDS + 1)
    }

    /// A sine lasting `seconds`.
    pub fn lasting(rate: u32, seconds: i64) -> Self {
        let frames = rate as usize * usize::try_from(seconds).expect("small");
        let plane = (0..frames)
            .map(|index| {
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let phase = std::f64::consts::TAU * 440.0 * index as f64 / f64::from(rate);
                #[allow(clippy::cast_possible_truncation)]
                let sample = (libm::sin(phase) * 0.1) as f32;
                sample
            })
            .collect::<Vec<_>>();
        Self {
            block: AudioBlock {
                start_sample: 0,
                sample_rate: rate,
                speakers: alpha2_media::SpeakerLayout::STEREO,
                channels: vec![plane.clone(), plane],
            },
        }
    }
}

impl AudioBlockProvider for Sine {
    fn block(
        &self,
        _source: &SourceId,
        _stream: Option<u32>,
        _start_sample: i64,
        _frames: usize,
    ) -> Option<AudioSpan<'_>> {
        Some(AudioSpan::whole(&self.block))
    }
}

/// Plays every clip at 1.25x plus `extra_ticks` of source. With zero extra ticks the fractional
/// positions repeat every four samples; any extra makes every fraction new.
pub fn retimed(mut project: Project, extra_ticks: i64) -> Project {
    let source = TimeRange::new(
        MediaTime::ZERO,
        MediaTime::from_ticks((SECONDS + 1) * TICKS_PER_SECOND + extra_ticks),
    )
    .expect("range");
    for track in &mut project.timeline.tracks {
        for clip in &mut track.clips {
            clip.source_range = source;
        }
    }
    project
}

/// Sweeps every clip's gain and pan with `interpolation`.
pub fn automated(mut project: Project, interpolation: Interpolation) -> Project {
    let sweep = |from: f32, to: f32| Animated::Keyframes {
        keyframes: vec![
            Keyframe {
                time: MediaTime::ZERO,
                value: from,
                interpolation,
            },
            Keyframe {
                time: MediaTime::from_ticks(SECONDS * TICKS_PER_SECOND),
                value: to,
                interpolation: Interpolation::Hold,
            },
        ],
    };
    for track in &mut project.timeline.tracks {
        for clip in &mut track.clips {
            clip.audio.gain = sweep(0.25, 1.0);
            clip.audio.pan = sweep(-0.5, 0.5);
        }
    }
    project
}

/// Staggers the clips so only `active` sound at a time and the other buses stay idle.
pub fn staggered(mut project: Project, active: usize) -> Project {
    let tracks = project.timeline.tracks.len();
    let slots = i64::try_from(tracks.div_ceil(active)).expect("small");
    let slot = SECONDS * TICKS_PER_SECOND / slots;
    for (index, track) in project.timeline.tracks.iter_mut().enumerate() {
        let start = i64::try_from(index / active).expect("small") * slot;
        let span = TimeRange::new(
            MediaTime::from_ticks(start),
            MediaTime::from_ticks(start + slot),
        )
        .expect("range");
        for clip in &mut track.clips {
            clip.timeline_range = span;
            clip.source_range = span;
        }
    }
    project
}

/// Sets every clip to `seconds` of timeline and source.
pub fn lengthened(mut project: Project, seconds: i64) -> Project {
    let span = TimeRange::new(
        MediaTime::ZERO,
        MediaTime::from_ticks(seconds * TICKS_PER_SECOND),
    )
    .expect("range");
    for track in &mut project.timeline.tracks {
        for clip in &mut track.clips {
            clip.timeline_range = span;
            clip.source_range = span;
        }
    }
    project
}

/// `clips` unity clips, one per track; `effects` adds a clip EQ, a track compressor and a master
/// reverb.
pub fn project(clips: usize, rate: u32, effects: bool) -> Project {
    let span = TimeRange::new(
        MediaTime::ZERO,
        MediaTime::from_ticks(SECONDS * TICKS_PER_SECOND),
    )
    .expect("range");
    let source = SourceId::new("tone").expect("ID");
    let tracks = (0..clips)
        .map(|index| {
            let mut track = Track::new(
                TrackId::new(format!("track-{index}")).expect("ID"),
                format!("Track {index}"),
                TrackKind::Audio,
            );
            let mut clip = Clip::new(
                ClipId::new(format!("clip-{index}")).expect("ID"),
                source.clone(),
                span,
                span,
            );
            if effects {
                clip.audio.effects.push(AudioEffect::Eq {
                    bands: vec![EqBand {
                        kind: EqBandKind::Peaking,
                        frequency_hz: 1_000.0,
                        q: 0.7,
                        gain_db: 3.0,
                    }],
                });
                // Below the -20 dBFS sine, so the compressor works on every sample.
                track.audio.effects.push(AudioEffect::Compressor {
                    threshold_db: -30.0,
                    ratio: 4.0,
                    attack_ms: 5.0,
                    release_ms: 80.0,
                    lookahead_ms: 0.0,
                });
            }
            track.clips.push(clip);
            track
        })
        .collect();
    let mut audio = MasterAudio::default();
    if effects {
        audio.effects.push(AudioEffect::Reverb {
            room_size: 0.6,
            damping: 0.4,
            mix: 0.2,
        });
    }
    Project {
        data: Vec::new(),
        schema_version: alpha2_model::CURRENT_SCHEMA_VERSION,
        id: ProjectId::new("audio-mix").expect("ID"),
        name: "audio mix benchmark".into(),
        settings: ProjectSettings::new(16, 16, FrameRate::new(30, 1).expect("rate"), rate)
            .expect("settings"),
        sources: vec![MediaSource::new(
            source,
            "benchmark://tone",
            MediaKind::Audio,
        )],
        effect_resources: Vec::new(),
        compositions: Vec::new(),
        timeline: Timeline { tracks },
        audio,
    }
}
