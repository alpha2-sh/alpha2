//! The media a mix needs: what it names, what it leaves out, and how it follows the time map.

use super::*;

/// Source windows of one long ramp at `rate`, each exactly a need's sample range widened
/// by `margin`, as a host decodes them: no readahead, nothing else.
struct Needed {
    windows: Vec<(SourceId, AudioBlock)>,
    length: i64,
}

impl Needed {
    fn new(needs: &[(SourceId, TimeRange)], rate: u32, length: i64, margin: i64) -> Self {
        let windows = needs
            .iter()
            .map(|(source, range)| {
                let start = (sample_floor(range.start().ticks(), rate) - margin).max(0);
                let end = (sample_at_or_after(range.end().ticks(), rate) + margin).min(length);
                #[allow(clippy::cast_precision_loss)]
                let plane: Vec<f32> = (start..end)
                    .map(|sample| sample as f32 / length as f32)
                    .collect();
                (
                    source.clone(),
                    AudioBlock {
                        start_sample: start,
                        sample_rate: rate,
                        speakers: alpha2_media::SpeakerLayout::MONO,
                        channels: vec![plane],
                    },
                )
            })
            .collect();
        Self { windows, length }
    }
}

impl AudioBlockProvider for Needed {
    fn block(&self, source: &SourceId, start: i64, _: usize) -> Option<AudioSpan<'_>> {
        self.windows.iter().find_map(|(id, block)| {
            (id == source && block.start_sample <= start && start < block.end_sample()).then(|| {
                AudioSpan {
                    block,
                    starts_source: block.start_sample <= 0,
                    ends_source: block.end_sample() >= self.length,
                }
            })
        })
    }
}

#[test]
fn a_retimed_clip_mixes_from_exactly_the_media_its_needs_name() {
    let second = MediaTime::from_seconds(1).ticks();
    for rate in [44_100, 48_000, 96_000] {
        let length = i64::from(rate) * 10;
        let whole = Needed {
            windows: vec![(
                SourceId::new("tone".to_owned()).expect("id"),
                AudioBlock {
                    start_sample: 0,
                    sample_rate: rate,
                    speakers: SpeakerLayout::MONO,
                    #[allow(clippy::cast_precision_loss)]
                    channels: vec![
                        (0..length)
                            .map(|sample| sample as f32 / length as f32)
                            .collect(),
                    ],
                },
            )],
            length,
        };
        for (numerator, denominator) in [(5, 2), (4, 1), (8, 1)] {
            for time_map in [TimeMap::Forward, TimeMap::Reverse] {
                let snapshot = rate_project(
                    rate,
                    vec![mapped_clip(
                        time_map,
                        second,
                        second * numerator / denominator,
                    )],
                );
                for start in [0, i64::from(rate) / 3 + 17] {
                    let mut renderer = AudioRenderer::new(&snapshot, 2, 1024).expect("renderer");
                    let mut reference = AudioRenderer::new(&snapshot, 2, 1024).expect("renderer");
                    for block in 0..4 {
                        let first = start + block * 1024;
                        let needs: Vec<(SourceId, TimeRange)> = renderer
                            .audio_needs(&snapshot, first, 1024)
                            .expect("needs")
                            .into_iter()
                            .map(|need| (need.source_id, need.source_range))
                            .collect();
                        let supplied = Needed::new(&needs, rate, length, 0);
                        let status = renderer
                            .render(&snapshot, first, 1024, &supplied, TransportState::Playing)
                            .expect("render");
                        assert!(
                            status.is_complete(),
                            "{rate} Hz, {numerator}/{denominator}x {time_map:?}, block at {first}: missing"
                        );
                        reference
                            .render(&snapshot, first, 1024, &whole, TransportState::Playing)
                            .expect("render");
                        assert_eq!(renderer.channel(0)[..1024], reference.channel(0)[..1024]);
                    }
                }
            }
        }
    }
}

#[test]
fn needs_leave_out_tracks_the_mix_does_not_hear() {
    let second = MediaTime::from_seconds(1).ticks();
    let mut muted = track("muted", vec![audio_clip("c1", "music", 0, second)]);
    muted.audio.muted = true;
    let mut soloed = track("soloed", vec![audio_clip("c2", "voice", 0, second)]);
    let unsoloed = track("unsoloed", vec![audio_clip("c3", "room", 0, second)]);
    soloed.audio.solo = true;
    let snapshot = EngineProject::new(project(
        vec![muted, soloed, unsoloed],
        vec!["music", "voice", "room"],
    ))
    .expect("project");
    let renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    let needs = renderer.audio_needs(&snapshot, 4_800, 512).expect("needs");
    let sources: Vec<&str> = needs.iter().map(|need| need.source_id.as_str()).collect();
    assert_eq!(sources, ["voice"]);
}

#[test]
fn a_seek_needs_each_clip_only_from_where_its_own_stages_settle() {
    let seconds = MediaTime::from_seconds(30).ticks();
    let mut verb = track("verb", vec![audio_clip("c1", "wet", 0, seconds)]);
    verb.audio.effects = vec![alpha2_model::AudioEffect::Reverb {
        room_size: 0.5,
        damping: 0.5,
        mix: 0.5,
    }];
    let dry = track("dry", vec![audio_clip("c2", "dry", 0, seconds)]);
    let mut project = project(vec![verb, dry], vec!["wet", "dry"]);
    project.settings = ProjectSettings::new(16, 16, FrameRate::new(30, 1).expect("rate"), RATE_LOW)
        .expect("settings");
    let snapshot = EngineProject::new(project).expect("project");
    let renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    let target = 20 * i64::from(RATE_LOW);
    let history = renderer.required_history_samples();
    assert!(
        history > i64::from(RATE_LOW),
        "the reverb must need a long history"
    );
    let needs = renderer.audio_needs(&snapshot, target, 512).expect("needs");
    let start = |source: &str| {
        needs
            .iter()
            .filter(|need| need.source_id.as_str() == source)
            .map(|need| sample_floor(need.source_range.start().ticks(), RATE_LOW))
            .min()
            .expect("need")
    };
    // The reverb settles from `history` before the target; the dry track has no state to
    // settle, so its clip is read from the target on.
    assert!(start("wet") <= target - history + 1);
    assert!(
        start("dry") >= target - 512,
        "the dry clip is needed from {} though nothing it feeds needs history",
        start("dry")
    );
}

/// The needs of one 480-sample block at sample 4 800 (100 ms), as
/// sample ranges per source.
fn block_needs(project: Project) -> Vec<(String, i64, i64)> {
    let snapshot = EngineProject::new(project).expect("project");
    let renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    let mut needs: Vec<_> = renderer
        .audio_needs(&snapshot, 4_800, 480)
        .expect("needs")
        .into_iter()
        .map(|need| {
            (
                need.source_id.as_str().to_owned(),
                sample_floor(need.source_range.start().ticks(), RATE),
                sample_at_or_after(need.source_range.end().ticks(), RATE),
            )
        })
        .collect();
    needs.sort();
    needs
}

#[test]
fn needs_name_the_samples_read_and_skip_clips_not_heard() {
    let second = MediaTime::from_seconds(1).ticks();
    let one = |edit: &dyn Fn(&mut Clip)| {
        let mut clip = audio_clip("c1", "tone", 0, second);
        edit(&mut clip);
        project(vec![track("a1", vec![clip])], vec!["tone"])
    };
    // A unity clip copies the block's samples, 4 800 to 5 279.
    assert_eq!(block_needs(one(&|_| {})), [("tone".into(), 4_800, 5_280)]);
    assert_eq!(block_needs(one(&|clip| clip.audio.muted = true)), []);
    assert_eq!(
        block_needs(one(
            &|clip| clip.audio.gain = Animated::Constant { value: 0.0 }
        )),
        []
    );
}

#[test]
fn needs_follow_the_time_map() {
    let second = MediaTime::from_seconds(1).ticks();
    let mapped = |time_map: TimeMap, source_end: i64| {
        project(
            vec![track("a1", vec![mapped_clip(time_map, second, source_end)])],
            vec!["tone"],
        )
    };
    // Reversed, sample n reads sample 47 999 - n.
    assert_eq!(
        block_needs(mapped(TimeMap::Reverse, second)),
        [("tone".into(), 42_720, 43_200)]
    );
    assert_eq!(
        block_needs(mapped(
            TimeMap::Freeze {
                source_time: MediaTime(1_000)
            },
            second
        )),
        []
    );
    // A 105 ms loop wraps inside the block: its end, then its start.
    let period = second * 105 / 1_000;
    assert_eq!(
        block_needs(mapped(
            TimeMap::Loop {
                mode: LoopMode::Repeat
            },
            period
        )),
        [("tone".into(), 0, 240), ("tone".into(), 4_800, 5_040)]
    );
    // Ping-pong turns back at the same place, so it reads one interval.
    assert_eq!(
        block_needs(mapped(
            TimeMap::Loop {
                mode: LoopMode::PingPong
            },
            period
        )),
        [("tone".into(), 4_800, 5_040)]
    );
}

#[test]
fn disjoint_intervals_of_one_source_stay_separate_needs() {
    let second = MediaTime::from_seconds(1).ticks();
    let cut = |id: &str, start: i64, end: i64, source: i64, length: i64| {
        let mut clip = audio_clip(id, "tone", start, end);
        clip.source_range =
            TimeRange::new(MediaTime(source), MediaTime(source + length)).expect("range");
        clip
    };
    // Two cuts from far apart in the source meet at sample 5 000, inside the block.
    let join = 12_500;
    let tracks = || {
        vec![
            track("a1", vec![cut("c1", 0, join, 0, join)]),
            track(
                "a2",
                vec![cut("c2", join, second, 2 * second, second - join)],
            ),
        ]
    };
    assert_eq!(
        block_needs(project(tracks(), vec!["tone"])),
        [
            ("tone".into(), 4_800, 5_000),
            ("tone".into(), 96_000, 96_280)
        ]
    );
    // A clip reading across both, 191 times faster than its source over the block, folds them
    // into one.
    let mut bridged = tracks();
    bridged.push(track(
        "a3",
        vec![cut("c3", 12_000, 13_200, 12_000, 229_200)],
    ));
    let needs = block_needs(project(bridged, vec!["tone"]));
    assert_eq!(needs.len(), 1, "{needs:?}");
    assert!(needs[0].1 <= 4_800 && needs[0].2 >= 96_280, "{needs:?}");
}
