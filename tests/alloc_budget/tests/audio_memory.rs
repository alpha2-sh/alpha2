//! What an [`AudioRenderer`] holds for a project's clip effects.
//!
//! The renderer keeps a clip's effect state only while the clip sounds, so a
//! long timeline of clips with effects costs what the clips heard at once
//! cost. This binary counts every allocation to see it.

// The counting allocator is the only unsafe code, and it only forwards.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use alpha2_audio::TransportState;
use alpha2_engine::{AudioRenderer, EngineProject};
use alpha2_media::{AudioBlock, AudioBlockProvider, AudioSpan, SourceId};
use alpha2_model::{
    AudioEffect, Clip, ClipId, MAX_AUDIO_EFFECTS, MAX_CLIPS_PER_TRACK, MasterAudio, MediaKind,
    MediaSource, Project, ProjectId, ProjectSettings, Timeline, Track, TrackId, TrackKind,
};
use alpha2_time::{FrameRate, MediaTime, TICKS_PER_SECOND, TimeRange};

/// Counts the bytes allocated and not yet freed, and the most there were.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call forwards to `System` with the caller's layout; the counters only observe. The
// default `alloc_zeroed` and `realloc` go through these two.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(live, Ordering::Relaxed);
        // SAFETY: the caller's contract for `GlobalAlloc::alloc` holds for `System`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `ptr` came from `System` with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const RATE: u32 = 96_000;
const CLIPS: usize = MAX_AUDIO_EFFECTS;

/// One second of constant signal for every source.
struct Tone(AudioBlock);

impl AudioBlockProvider for Tone {
    fn block(&self, _: &SourceId, _: i64, _: usize) -> Option<AudioSpan<'_>> {
        Some(AudioSpan::whole(&self.0))
    }
}

/// One-second clips, one after another, each through a compressor with 14 ms
/// of lookahead: as many effects as the model allows, and nearly as much
/// delay.
fn long_timeline() -> Project {
    let source = SourceId::new("tone".to_owned()).expect("id");
    let compressor = AudioEffect::Compressor {
        threshold_db: -12.0,
        ratio: 4.0,
        attack_ms: 5.0,
        release_ms: 80.0,
        lookahead_ms: 14.0,
    };
    let tracks = (0..CLIPS.div_ceil(MAX_CLIPS_PER_TRACK))
        .map(|index| {
            let mut track = Track::new(
                TrackId::new(format!("a{index}")).expect("id"),
                format!("A{index}"),
                TrackKind::Audio,
            );
            let first = index * MAX_CLIPS_PER_TRACK;
            track.clips = (first..CLIPS.min(first + MAX_CLIPS_PER_TRACK))
                .map(|clip| {
                    let start = TICKS_PER_SECOND * i64::try_from(clip).expect("index");
                    let mut clip = Clip::new(
                        ClipId::new(format!("c{clip}")).expect("id"),
                        source.clone(),
                        TimeRange::new(MediaTime(start), MediaTime(start + TICKS_PER_SECOND))
                            .expect("range"),
                        TimeRange::new(MediaTime(0), MediaTime(TICKS_PER_SECOND)).expect("range"),
                    );
                    clip.audio.effects = vec![compressor.clone()];
                    clip
                })
                .collect();
            track
        })
        .collect();
    Project {
        schema_version: alpha2_model::CURRENT_SCHEMA_VERSION,
        id: ProjectId::new("memory".to_owned()).expect("id"),
        name: "memory".into(),
        settings: ProjectSettings::new(16, 16, FrameRate::new(30, 1).expect("rate"), RATE)
            .expect("settings"),
        sources: vec![MediaSource::new(
            source,
            "asset://tone".to_owned(),
            MediaKind::Audio,
        )],
        effect_resources: Vec::new(),
        compositions: Vec::new(),
        timeline: Timeline { tracks },
        audio: MasterAudio::default(),
    }
}

#[test]
fn clip_effects_hold_state_only_while_their_clips_sound() {
    let snapshot = EngineProject::new(long_timeline()).expect("a valid project");
    let tone = Tone(AudioBlock {
        start_sample: 0,
        sample_rate: RATE,
        speakers: alpha2_media::SpeakerLayout::STEREO,
        channels: vec![vec![0.25; RATE as usize]; 2],
    });
    let budget = 32 << 20;

    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let mut renderer = AudioRenderer::new(&snapshot, 2, 4_096).expect("renderer");
    let built = LIVE.load(Ordering::Relaxed) - before;
    let peak = PEAK.load(Ordering::Relaxed) - before;
    assert!(
        built < budget && peak < budget,
        "a renderer for {CLIPS} clips holds {built} bytes (peak {peak})"
    );

    // A seek into the middle replays the settling history and mixes the clips under it, one after
    // another.
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    let middle = i64::from(RATE) * 4_000;
    for block in 0..8 {
        renderer
            .render(
                &snapshot,
                middle + block * 4_096,
                4_096,
                &tone,
                TransportState::Playing,
            )
            .expect("render");
    }
    let mixing = PEAK.load(Ordering::Relaxed) - before;
    println!("audio_memory clips={CLIPS} built={built} peak={peak} mixing_peak={mixing}");
    assert!(mixing < budget, "mixing held {mixing} bytes at its peak");
    assert!(
        renderer.channel(0).iter().any(|sample| *sample != 0.0),
        "the clips are heard"
    );
}
