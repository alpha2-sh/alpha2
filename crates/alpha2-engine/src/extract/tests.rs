use super::fixtures::{Digest, Scripted, audio_input, digest_values, fnv1a, video_input};
use super::*;
use alpha2_model::{DataInterval, DataKind, DataValues, DataWord, Project};
use alpha2_time::{MediaTime, TimeRange};

fn project() -> Project {
    Project::from_json(
        &serde_json::json!({
            "schema_version": 1, "id": "p", "name": "P",
            "settings": { "width": 640, "height": 360, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48_000 },
            "sources": [
                { "id": "v", "kind": "video", "locator": "v.mp4", "has_audio": true },
                { "id": "a", "kind": "audio", "locator": "a.m4a" }
            ],
            "timeline": { "tracks": [] }
        })
        .to_string(),
    )
    .expect("project")
}

fn range(start: i64, end: i64) -> TimeRange {
    TimeRange::new(MediaTime(start), MediaTime(end)).expect("range")
}

fn request(
    descriptor: &Descriptor,
    from: InputFrom,
    range: TimeRange,
    passes: Vec<Pass>,
) -> ExtractRequest {
    ExtractRequest {
        extractor: descriptor.id.clone(),
        version: descriptor.version,
        params: serde_json::Map::new(),
        input: RequestInput {
            from,
            source_id: (from == InputFrom::Source)
                .then(|| alpha2_media::SourceId::new("v").expect("id")),
            range,
            passes,
            size: None,
            frame_rate: None,
            stream: None,
            sample_rate: None,
            channels: None,
        },
        outputs: descriptor
            .outputs
            .iter()
            .map(|output| {
                (
                    output.name.clone(),
                    alpha2_model::DataSetId::new(format!("set-{}", output.name)).expect("id"),
                )
            })
            .collect(),
    }
}

/// A source with frames at `pts`, each 4x2 pixels filled with its index, and audio whose sample
/// `n` of channel `c` is `(n + 1000 c) / 1e6`.
struct Fake {
    pts: Vec<i64>,
    asked: Vec<Need>,
}

impl Fake {
    fn new(pts: Vec<i64>) -> Self {
        Self {
            pts,
            asked: Vec::new(),
        }
    }

    fn frame_at(&self, index: usize) -> SuppliedFrame {
        let fill = u8::try_from(index % 256).expect("byte");
        SuppliedFrame {
            pts: MediaTime(self.pts[index]),
            width: 4,
            height: 2,
            rgba: vec![fill; 4 * 2 * 4],
        }
    }
}

impl Supplier for Fake {
    fn frame(&mut self, need: &Need) -> Result<Option<SuppliedFrame>, String> {
        self.asked.push(need.clone());
        let index = match need {
            Need::SourceFrame {
                from: FrameFrom::Start(time),
                ..
            } => Some(
                self.pts
                    .iter()
                    .rposition(|pts| *pts <= time.ticks())
                    .unwrap_or(0),
            ),
            Need::SourceFrame {
                from: FrameFrom::After(pts),
                direction: Direction::Forward,
                ..
            } => self
                .pts
                .iter()
                .position(|candidate| *candidate > pts.ticks()),
            Need::SourceFrame {
                from: FrameFrom::After(pts),
                direction: Direction::Backward,
                ..
            } => self
                .pts
                .iter()
                .rposition(|candidate| *candidate < pts.ticks()),
            Need::TimelineFrame { time, .. } => {
                return Ok(Some(SuppliedFrame {
                    pts: *time,
                    width: 2,
                    height: 2,
                    rgba: vec![7; 16],
                }));
            }
            _ => return Err("not a frame".to_owned()),
        };
        Ok(index.map(|index| self.frame_at(index)))
    }

    fn audio(&mut self, need: &Need) -> Result<SuppliedAudio, String> {
        self.asked.push(need.clone());
        let (Need::SourceAudio {
            start_sample,
            frames,
            channels,
            ..
        }
        | Need::TimelineAudio {
            start_sample,
            frames,
            channels,
            ..
        }) = need
        else {
            return Err("not audio".to_owned());
        };
        let count = if *channels == AudioChannels::Mono {
            1
        } else {
            2
        };
        let mut samples = Vec::new();
        for channel in 0..count {
            for frame in 0..i64::from(*frames) {
                #[allow(clippy::cast_precision_loss)]
                samples.push(((start_sample + frame + 1_000 * channel) as f32) / 1e6);
            }
        }
        Ok(SuppliedAudio {
            channels: u32::try_from(count).expect("count"),
            samples,
        })
    }
}

fn digest_times(sets: &[alpha2_model::DataSet]) -> Vec<i64> {
    let DataValues::Scalars(samples) = &sets[0].values else {
        panic!("scalars")
    };
    samples.times.iter().map(|time| time.ticks()).collect()
}

const PTS: [i64; 6] = [0, 4_000, 8_000, 12_000, 16_000, 20_000];

#[test]
fn a_forward_pass_delivers_the_frames_shown_inside_the_range() {
    let digest = Digest::new(video_input(VideoFormat::Rgba8, 960));
    let descriptor = digest.describe().clone();
    let session = ExtractSession::new(
        &project(),
        &descriptor,
        request(&descriptor, InputFrom::Source, range(5_000, 16_000), vec![]),
    )
    .expect("session");
    let mut fake = Fake::new(PTS.to_vec());
    let sets = drive(session, &mut digest.clone(), &mut fake).expect("run");
    // 4000 is shown at the range start; 16000 is past the end and not delivered.
    assert_eq!(digest_times(&sets), vec![4_000, 8_000, 12_000]);
    assert_eq!(fake.asked.len(), 4);
    let DataValues::Scalars(samples) = &sets[0].values else {
        panic!("scalars")
    };
    let expected = digest_values(fnv1a(vec![1_u8; 32]));
    assert_eq!(samples.values[0..2], [Some(expected[0]), Some(expected[1])]);
    assert_eq!(
        sets[0].anchor,
        alpha2_model::DataAnchor::Source {
            source_id: alpha2_media::SourceId::new("v").expect("id")
        }
    );
    assert_eq!(
        sets[0].made_by.as_ref().map(|made| made.extractor.as_str()),
        Some(fixtures::DIGEST_ID)
    );
}

#[test]
fn a_backward_pass_delivers_the_same_frames_reversed() {
    let digest = Digest::new(video_input(VideoFormat::Luma8, 960));
    let descriptor = digest.describe().clone();
    let passes = vec![Pass {
        start: MediaTime(15_999),
        direction: Direction::Backward,
    }];
    let session = ExtractSession::new(
        &project(),
        &descriptor,
        request(&descriptor, InputFrom::Source, range(5_000, 16_000), passes),
    )
    .expect("session");
    let mut fake = Fake::new(PTS.to_vec());
    let sets = drive(session, &mut digest.clone(), &mut fake).expect("run");
    assert_eq!(digest_times(&sets), vec![4_000, 8_000, 12_000]);
    let order = fake
        .asked
        .iter()
        .map(|need| match need {
            Need::SourceFrame { from, .. } => *from,
            _ => panic!("frames"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        order,
        vec![
            FrameFrom::Start(MediaTime(15_999)),
            FrameFrom::After(MediaTime(12_000)),
            FrameFrom::After(MediaTime(8_000))
        ]
    );
}

#[test]
fn luma8_is_bt709_of_the_rgb() {
    assert_eq!(
        luma8_from_rgba8(&[255, 255, 255, 0, 255, 0, 0, 255, 0, 0, 255, 255]),
        vec![255, 54, 18]
    );
}

#[test]
fn the_delivered_size_rounds_half_up_and_never_upscales() {
    assert_eq!(delivered_size((1920, 1080), 960), (960, 540));
    assert_eq!(delivered_size((1080, 1920), 960), (540, 960));
    assert_eq!(delivered_size((1001, 3), 100), (100, 1));
    assert_eq!(delivered_size((640, 360), 960), (640, 360));
    assert_eq!(delivered_size((3, 1001), 2), (1, 2));
}

#[test]
fn two_passes_from_a_seed_meet_without_repeating_it() {
    let outputs = vec![OutputSpec {
        name: "track".into(),
        kind: DataKind::Points,
        channels: vec!["a".into()],
        interpolation: None,
        suggest: None,
    }];
    let sample = |time: i64| Emit::Sample {
        output: "track".into(),
        time: MediaTime(time),
        values: vec![Some(0.5), Some(0.5)],
    };
    // Forward from 8000 delivers 8000, 12000, 16000; backward delivers 8000 again, then 4000, 0.
    let per_delivery = vec![
        vec![sample(8_000)],
        vec![sample(12_000)],
        vec![sample(16_000)],
        vec![],
        vec![sample(4_000)],
        vec![sample(0)],
    ];
    let mut scripted = Scripted::new(
        video_input(VideoFormat::Rgba8, 960),
        outputs,
        per_delivery,
        vec![],
    );
    let descriptor = scripted.describe().clone();
    let passes = vec![
        Pass {
            start: MediaTime(8_000),
            direction: Direction::Forward,
        },
        Pass {
            start: MediaTime(8_000),
            direction: Direction::Backward,
        },
    ];
    let session = ExtractSession::new(
        &project(),
        &descriptor,
        request(&descriptor, InputFrom::Source, range(0, 20_000), passes),
    )
    .expect("session");
    let sets = drive(session, &mut scripted, &mut Fake::new(PTS.to_vec())).expect("run");
    let DataValues::Points(samples) = &sets[0].values else {
        panic!("points")
    };
    assert_eq!(
        samples
            .times
            .iter()
            .map(|time| time.ticks())
            .collect::<Vec<_>>(),
        vec![0, 4_000, 8_000, 12_000, 16_000]
    );
}

#[test]
fn a_time_emitted_twice_is_refused_at_finish() {
    let outputs = vec![OutputSpec {
        name: "track".into(),
        kind: DataKind::Scalars,
        channels: vec!["a".into()],
        interpolation: None,
        suggest: None,
    }];
    let sample = |time: i64| Emit::Sample {
        output: "track".into(),
        time: MediaTime(time),
        values: vec![Some(0.5)],
    };
    let mut scripted = Scripted::new(
        video_input(VideoFormat::Rgba8, 960),
        outputs,
        vec![vec![sample(8_000)], vec![], vec![], vec![sample(8_000)]],
        vec![],
    );
    let descriptor = scripted.describe().clone();
    let passes = vec![
        Pass {
            start: MediaTime(8_000),
            direction: Direction::Forward,
        },
        Pass {
            start: MediaTime(8_000),
            direction: Direction::Backward,
        },
    ];
    let session = ExtractSession::new(
        &project(),
        &descriptor,
        request(&descriptor, InputFrom::Source, range(0, 16_000), passes),
    )
    .expect("session");
    let result = drive(session, &mut scripted, &mut Fake::new(PTS.to_vec()));
    assert!(
        matches!(
            result,
            Err(DriveError::Session(SessionError::InvalidResult(_)))
        ),
        "{result:?}"
    );
}

#[test]
fn supply_end_mid_range_ends_the_pass() {
    let digest = Digest::new(video_input(VideoFormat::Rgba8, 960));
    let descriptor = digest.describe().clone();
    let session = ExtractSession::new(
        &project(),
        &descriptor,
        request(&descriptor, InputFrom::Source, range(0, 100_000), vec![]),
    )
    .expect("session");
    let sets = drive(session, &mut digest.clone(), &mut Fake::new(vec![0, 4_000])).expect("run");
    assert_eq!(digest_times(&sets), vec![0, 4_000]);
}

fn emit_error(emits: Vec<Emit>, outputs: Vec<OutputSpec>) -> SessionError {
    let mut scripted = Scripted::new(
        video_input(VideoFormat::Rgba8, 960),
        outputs,
        vec![emits],
        vec![],
    );
    let descriptor = scripted.describe().clone();
    let session = ExtractSession::new(
        &project(),
        &descriptor,
        request(&descriptor, InputFrom::Source, range(4_000, 16_000), vec![]),
    )
    .expect("session");
    match drive(session, &mut scripted, &mut Fake::new(PTS.to_vec())) {
        Err(DriveError::Session(error)) => error,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn scalar_output() -> Vec<OutputSpec> {
    vec![OutputSpec {
        name: "s".into(),
        kind: DataKind::Scalars,
        channels: vec!["a".into()],
        interpolation: None,
        suggest: None,
    }]
}

#[test]
fn emits_that_break_a_rule_are_refused() {
    let sample = |output: &str, time: i64, values: Vec<Option<f32>>| Emit::Sample {
        output: output.into(),
        time: MediaTime(time),
        values,
    };
    let cases = [
        vec![sample("nope", 4_000, vec![Some(1.0)])],
        vec![sample("s", 4_000, vec![Some(1.0), Some(2.0)])],
        vec![sample("s", 4_000, vec![Some(f32::NAN)])],
        vec![sample("s", 4_000, vec![Some(2e9)])],
        vec![sample("s", 2_000, vec![Some(1.0)])],
        vec![sample("s", 8_000, vec![Some(1.0)])],
        vec![
            sample("s", 4_000, vec![Some(1.0)]),
            sample("s", 4_000, vec![Some(1.0)]),
        ],
        vec![Emit::Word {
            output: "s".into(),
            word: DataWord {
                start: MediaTime(4_000),
                end: MediaTime(4_001),
                text: "a".into(),
                speaker: None,
            },
        }],
    ];
    for emits in cases {
        let error = emit_error(emits.clone(), scalar_output());
        assert!(
            matches!(error, SessionError::InvalidEmit { .. }),
            "{emits:?}: {error:?}"
        );
    }
}

#[test]
fn words_and_intervals_follow_their_rules() {
    let outputs = vec![
        OutputSpec {
            name: "w".into(),
            kind: DataKind::Words,
            channels: vec![],
            interpolation: None,
            suggest: None,
        },
        OutputSpec {
            name: "i".into(),
            kind: DataKind::Intervals,
            channels: vec![],
            interpolation: None,
            suggest: None,
        },
    ];
    let word = |start, end, text: &str| Emit::Word {
        output: "w".into(),
        word: DataWord {
            start: MediaTime(start),
            end: MediaTime(end),
            text: text.into(),
            speaker: None,
        },
    };
    let interval = |start, end| Emit::Interval {
        output: "i".into(),
        interval: DataInterval {
            start: MediaTime(start),
            end: MediaTime(end),
            label: None,
            score: Some(0.5),
        },
    };
    for bad in [
        vec![word(4_000, 4_000, "a")],
        vec![word(4_000, 4_001, "")],
        vec![interval(4_001, 4_000)],
        vec![interval(3_000, 4_000)],
    ] {
        assert!(matches!(
            emit_error(bad, outputs.clone()),
            SessionError::InvalidEmit { .. }
        ));
    }
    // From the end, whole-range work may place items anywhere in the range.
    let mut scripted = Scripted::new(
        video_input(VideoFormat::Rgba8, 960),
        outputs,
        vec![],
        vec![
            interval(5_000, 15_000),
            word(4_000, 4_500, "hi"),
            interval(4_000, 4_000),
        ],
    );
    let descriptor = scripted.describe().clone();
    let session = ExtractSession::new(
        &project(),
        &descriptor,
        request(&descriptor, InputFrom::Source, range(4_000, 16_000), vec![]),
    )
    .expect("session");
    let sets = drive(session, &mut scripted, &mut Fake::new(PTS.to_vec())).expect("run");
    let intervals = sets.iter().find_map(|set| match &set.values {
        DataValues::Intervals(items) => Some(items.clone()),
        _ => None,
    });
    assert_eq!(
        intervals
            .expect("intervals")
            .iter()
            .map(|item| item.start.ticks())
            .collect::<Vec<_>>(),
        vec![4_000, 5_000]
    );
}

#[test]
fn audio_blocks_are_a_function_of_range_passes_and_block_size() {
    let digest = Digest::new(audio_input(64));
    let descriptor = digest.describe().clone();
    // 48 kHz: 2.5 ticks a sample. 1001..11001 ticks hold samples 401..4401 (4000 samples).
    let passes = vec![
        Pass {
            start: MediaTime(6_001),
            direction: Direction::Forward,
        },
        Pass {
            start: MediaTime(6_001),
            direction: Direction::Backward,
        },
    ];
    let session = ExtractSession::new(
        &project(),
        &descriptor,
        request(&descriptor, InputFrom::Source, range(1_001, 11_001), passes),
    )
    .expect("session");
    let mut fake = Fake::new(vec![]);
    drive(session, &mut digest.clone(), &mut fake).expect("run");
    let blocks = fake
        .asked
        .iter()
        .map(|need| match need {
            Need::SourceAudio {
                start_sample,
                frames,
                ..
            } => (*start_sample, *frames),
            _ => panic!("audio"),
        })
        .collect::<Vec<_>>();
    // Forward from sample 2401 to 4401 in 64s, the last shortened; backward from 2401 to 401.
    let forward = blocks
        .iter()
        .take_while(|(start, _)| *start >= 2_401)
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(forward.first(), Some(&(2_401, 64)));
    assert_eq!(forward.last(), Some(&(4_385, 16)));
    assert_eq!(
        forward
            .iter()
            .map(|(_, frames)| u64::from(*frames))
            .sum::<u64>(),
        2_000
    );
    let backward = &blocks[forward.len()..];
    assert_eq!(backward.first(), Some(&(2_337, 64)));
    assert_eq!(backward.last(), Some(&(401, 16)));
    assert_eq!(
        backward
            .iter()
            .map(|(_, frames)| u64::from(*frames))
            .sum::<u64>(),
        2_000
    );
}

#[test]
fn progress_counts_deliveries_and_cancel_is_not_calling_next() {
    let digest = Digest::new(audio_input(1024));
    let descriptor = digest.describe().clone();
    let mut session = ExtractSession::new(
        &project(),
        &descriptor,
        request(&descriptor, InputFrom::Source, range(0, 120_000), vec![]),
    )
    .expect("session");
    assert_eq!(
        session.progress(),
        &[PassProgress {
            completed: 0,
            total: Some(47)
        }]
    );
    let mut fake = Fake::new(vec![]);
    let mut delivered = 0;
    while delivered < 3 {
        match session.next() {
            Step::Need(need) => session
                .supply_audio(fake.audio(&need).expect("audio"))
                .expect("supply"),
            Step::Deliver(_) => delivered += 1,
            Step::PassBoundary { .. } => {}
            Step::Done => panic!("not done yet"),
        }
    }
    assert_eq!(session.progress()[0].completed, 3);
    // Cancelling: no more `next`; finishing early is refused.
    assert!(matches!(
        session.finish(),
        Err(SessionError::InvalidResult(_))
    ));
}

#[test]
fn timeline_frames_are_every_frame_time_inside_the_range() {
    let digest = Digest::new(video_input(VideoFormat::Rgba8, 960));
    let descriptor = digest.describe().clone();
    let session = ExtractSession::new(
        &project(),
        &descriptor,
        request(&descriptor, InputFrom::Timeline, range(1, 20_000), vec![]),
    )
    .expect("session");
    let sets = drive(session, &mut digest.clone(), &mut Fake::new(vec![])).expect("run");
    assert_eq!(digest_times(&sets), vec![4_000, 8_000, 12_000, 16_000]);
    assert_eq!(sets[0].anchor, alpha2_model::DataAnchor::Timeline);
}

#[test]
fn requests_that_break_a_rule_are_refused() {
    let digest = Digest::new(video_input(VideoFormat::Rgba8, 960));
    let descriptor = digest.describe().clone();
    let base = request(&descriptor, InputFrom::Source, range(0, 10_000), vec![]);
    let mut cases = Vec::new();
    let mut wrong = base.clone();
    wrong.version = 2;
    cases.push(wrong);
    let mut too_big = base.clone();
    too_big.input.size = Some(961);
    cases.push(too_big);
    let mut outside = base.clone();
    outside.input.passes = vec![Pass {
        start: MediaTime(10_000),
        direction: Direction::Backward,
    }];
    cases.push(outside);
    let mut undeclared = base.clone();
    undeclared.outputs.insert(
        "other".into(),
        alpha2_model::DataSetId::new("x").expect("id"),
    );
    cases.push(undeclared);
    let mut audio = base.clone();
    audio.input.source_id = Some(alpha2_media::SourceId::new("a").expect("id"));
    cases.push(audio);
    for case in cases {
        assert!(
            ExtractSession::new(&project(), &descriptor, case.clone()).is_err(),
            "{case:?}"
        );
    }
    let mut supply = ExtractSession::new(&project(), &descriptor, base).expect("session");
    assert!(supply.supply_end().is_err());
}
