//! The clip effects a renderer holds at once, and what a seek drops and replays.

use super::*;

/// Supplies `seconds` of a constant at any rate, from the source origin.
struct Tone {
    id: SourceId,
    block: AudioBlock,
}

impl Tone {
    fn new(rate: u32, seconds: usize) -> Self {
        Self {
            id: SourceId::new("tone".to_owned()).expect("id"),
            block: AudioBlock {
                start_sample: 0,
                sample_rate: rate,
                speakers: SpeakerLayout::default_for(2).expect("layout"),
                channels: vec![vec![0.25; rate as usize * seconds]; 2],
            },
        }
    }
}

impl AudioBlockProvider for Tone {
    fn block(
        &self,
        source: &SourceId,
        _start_sample: i64,
        _frames: usize,
    ) -> Option<AudioSpan<'_>> {
        (*source == self.id).then(|| AudioSpan::whole(&self.block))
    }
}

/// `count` clips of `length` ticks on one track, `period` apart from `first`, each with
/// `effects`, in a project at `rate`.
fn clips_with_effects(
    rate: u32,
    count: usize,
    first: i64,
    period: i64,
    length: i64,
    effects: &[AudioEffect],
) -> Project {
    let clips = (0..count)
        .map(|index| {
            let start = first + period * i64::try_from(index).expect("index");
            let mut clip = audio_clip(&format!("clip-{index}"), "tone", start, start + length);
            clip.audio.effects = effects.to_vec();
            clip
        })
        .collect();
    let mut project = project(vec![track("a1", clips)], vec!["tone"]);
    project.settings.sample_rate = rate;
    project
}

/// Renders `blocks` of `frames` at each start, and returns the most clip effects, and their
/// delay, the renderer held at once.
fn most_held(project: Project, max_block: usize, blocks: &[(i64, usize)]) -> (usize, u64) {
    let longest = project
        .timeline
        .tracks
        .iter()
        .flat_map(|track| &track.clips)
        .filter_map(|clip| {
            clip.source_range
                .end()
                .ticks()
                .checked_div(TICKS_PER_SECOND)
        })
        .max()
        .unwrap_or(0);
    let seconds = usize::try_from(longest).expect("seconds") + 1;
    let tone = Tone::new(project.settings.sample_rate, seconds);
    let snapshot = EngineProject::new(project).expect("valid project");
    let mut renderer = AudioRenderer::new(&snapshot, 2, max_block).expect("renderer");
    for &(start, frames) in blocks {
        renderer
            .render(&snapshot, start, frames, &tone, TransportState::Playing)
            .expect("render");
    }
    renderer.most_held
}

#[test]
fn a_seek_drops_the_clip_effects_it_leaves_before_building_others() {
    // One reverb at a time: from one second to two, and from 100 s to 140 s. A seek to 135 s
    // replays its history from inside the second clip, whose first block must not find the
    // first clip's reverb still built.
    let reverb = AudioEffect::Reverb {
        room_size: 0.5,
        damping: 0.5,
        mix: 0.25,
    };
    let mut project = clips_with_effects(RATE, 2, TICKS_PER_SECOND, 0, TICKS_PER_SECOND, &[reverb]);
    let later = &mut project.timeline.tracks[0].clips[1];
    later.timeline_range = TimeRange::new(
        MediaTime(100 * TICKS_PER_SECOND),
        MediaTime(140 * TICKS_PER_SECOND),
    )
    .expect("range");
    later.source_range =
        TimeRange::new(MediaTime(0), MediaTime(40 * TICKS_PER_SECOND)).expect("range");
    let load = project.audio_effect_load();
    assert_eq!(load.effects, 1);
    let rate = i64::from(RATE);
    let held = most_held(
        project,
        512,
        &[(rate * 3 / 2, 512), (rate * 135, 512), (rate * 3 / 2, 512)],
    );
    assert_eq!(
        held,
        (load.effects, load.delay_ticks),
        "one reverb at a time"
    );
}

#[test]
fn clips_one_after_another_hold_one_clip_of_effects_at_96_khz() {
    // 2,000 reverbs of 10 ms, 100 ms apart: an export holds one at a time, as the model
    // counts no more than its margin around each.
    let reverb = AudioEffect::Reverb {
        room_size: 0.9,
        damping: 0.2,
        mix: 0.5,
    };
    let rate = 96_000;
    let period = TICKS_PER_SECOND / 10;
    let project = clips_with_effects(rate, 2_000, 0, period, TICKS_PER_SECOND / 100, &[reverb]);
    project
        .validate()
        .expect("2,000 reverbs one after another are valid");
    let load = project.audio_effect_load();
    let end = 2_000 * i64::from(rate) / 10;
    let blocks: Vec<_> = (0..end)
        .step_by(4_096)
        .map(|start| (start, 4_096))
        .collect();
    let held = most_held(project, 4_096, &blocks);
    assert_eq!(held, (1, u64::from(alpha2_model::REVERB_DELAY_MS) * 120));
    assert!(held.0 <= load.effects && held.1 <= load.delay_ticks);
}

#[test]
fn the_effects_a_renderer_holds_at_once_are_within_the_models_count() {
    // Lookahead on the track and the clip, the lowest and a common rate, large and small blocks.
    let reverb = AudioEffect::Reverb {
        room_size: 0.5,
        damping: 0.5,
        mix: 0.25,
    };
    let compressor = |lookahead_ms| AudioEffect::Compressor {
        threshold_db: -12.0,
        ratio: 4.0,
        attack_ms: 5.0,
        release_ms: 80.0,
        lookahead_ms,
    };
    let margin = i64::from(alpha2_model::AUDIO_EFFECT_STATE_MARGIN_MS) * 120;
    for rate in [8_000, 44_100] {
        for gap in [0, margin / 2, margin - 1, margin, margin + 1, 3 * margin] {
            for (bus_lookahead, clip_lookahead) in [(0.0, 0.0), (100.0, 0.3), (0.0, 100.0)] {
                let length = TICKS_PER_SECOND / 4;
                let mut project = clips_with_effects(
                    rate,
                    6,
                    TICKS_PER_SECOND / 3,
                    length + gap,
                    length,
                    &[compressor(clip_lookahead), reverb.clone()],
                );
                let bus = compressor(bus_lookahead);
                let bus_delay = bus.delay_ticks();
                project.timeline.tracks[0].audio.effects = vec![bus];
                let load = project.audio_effect_load();
                let end = sample_floor(
                    project.timeline.tracks[0].clips[5]
                        .timeline_range
                        .end()
                        .ticks(),
                    rate,
                ) + 1;
                for max_block in [MAX_AUDIO_BLOCK, 333] {
                    let frames = i64::try_from(max_block).expect("frames");
                    let mut blocks: Vec<_> = (0..end)
                        .step_by(max_block)
                        .map(|start| (start, max_block))
                        .collect();
                    for step in 0..12_i64 {
                        blocks.push(((step * 7_919 * frames / 5) % end, max_block));
                    }
                    let held = most_held(project.clone(), max_block, &blocks);
                    assert!(
                        held.0 < load.effects && held.1 + bus_delay <= load.delay_ticks,
                        "rate {rate}, gap {gap}, lookahead {bus_lookahead}/{clip_lookahead}, block {max_block}: held {held:?}, counted {load:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn a_seek_replays_at_most_the_settling_cap_whatever_the_effects() {
    // Each of these is valid, and each one's own horizon is minutes to a day.
    let reverb = AudioEffect::Reverb {
        room_size: 1.0,
        damping: 0.999_999_94,
        mix: 0.5,
    };
    let compressor = AudioEffect::Compressor {
        threshold_db: -12.0,
        ratio: 4.0,
        attack_ms: 1.0,
        release_ms: 10_000.0,
        lookahead_ms: 5.0,
    };
    let hour = TICKS_PER_SECOND * 3_600;
    let mut clip = audio_clip("clip", "tone", 0, hour);
    clip.audio.effects = vec![reverb.clone(), compressor.clone()];
    let mut bus = track("a1", vec![clip]);
    bus.audio.effects = vec![compressor.clone(), reverb.clone()];
    let mut project = project(vec![bus], vec!["tone"]);
    project.audio.effects = vec![reverb, compressor];
    let snapshot = EngineProject::new(project).expect("extreme but valid effects");
    let renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");

    let cap = i64::from(RATE) * i64::from(MAX_SETTLING_SECONDS);
    assert!(
        renderer.required_history_samples() <= cap,
        "history {} samples, cap {cap}",
        renderer.required_history_samples()
    );
    // A seek half an hour in reads the source from no earlier than the cap and the graph's
    // delay before it, and one interpolation neighbour.
    let start = i64::from(RATE) * 1_800;
    let needs = renderer.audio_needs(&snapshot, start, 512).expect("needs");
    let lookahead = i64::from(renderer.required_lookahead_samples());
    let earliest = needs
        .iter()
        .map(|need| need.source_range.start().ticks())
        .min()
        .expect("the clip is heard");
    assert!(
        earliest >= ticks_at(start - cap - lookahead - 1, RATE),
        "a seek to {start} reads from tick {earliest}"
    );
}
