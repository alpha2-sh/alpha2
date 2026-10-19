//! The engine's time rules as a table: `tests/golden/time-rules-corpus.json`.
//!
//! TypeScript keeps ports of these rules where it runs without the engine
//! (the SDK exporter's schedule, the editor's frame grid, a curve's display
//! value, where a clip reads its source, tick conversions). Each port runs
//! this table, so a port that drifts from the engine fails its own tests.
//! This test computes every expected value through the engine and fails
//! when the checked-in table differs; `ALPHA2_UPDATE_GOLDENS=1` rewrites it.

use alpha2_engine::{EngineProject, Exporter, QualityProfile};
use alpha2_eval::picture_source_time;
use alpha2_media::VideoCodec;
use alpha2_media::sample_ticks_floor;
use alpha2_model::{CubicBezier, LoopMode, Project, ProjectId, ProjectSettings, TimeMap};
use alpha2_time::{FrameRate, MediaTime, TICKS_PER_SECOND, TimeRange};
use serde_json::{Value, json};

const RATES: [(u32, u32); 13] = [
    (30, 1),
    (24, 1),
    (25, 1),
    (30_000, 1_001),
    (24_000, 1_001),
    (60_000, 1_001),
    (48_000, 1_001),
    (90, 1),
    (72, 1),
    (144, 1),
    (165, 1),
    (18, 1),
    (7, 1),
];

fn rate((num, den): (u32, u32)) -> FrameRate {
    FrameRate::new(num, den).expect("rate")
}

fn rate_json(rate: FrameRate) -> Value {
    json!({ "num": rate.numerator(), "den": rate.denominator() })
}

fn start(rate: FrameRate, frame: i64) -> i64 {
    rate.frame_start(frame).expect("frame start").ticks()
}

fn range(start: i64, end: i64) -> TimeRange {
    TimeRange::new(MediaTime::from_ticks(start), MediaTime::from_ticks(end)).expect("range")
}

/// Ticks on and beside the first frame starts and one far out, with the
/// frame each falls in and that frame's start.
fn frames() -> Value {
    RATES
        .into_iter()
        .map(|pair| {
            let rate = rate(pair);
            let mut ticks: Vec<i64> = (0..=12)
                .chain([1_000, 100_000])
                .flat_map(|frame| {
                    let at = start(rate, frame);
                    [at - 1, at, at + 1]
                })
                .filter(|tick| *tick >= 0)
                .collect();
            ticks.sort_unstable();
            ticks.dedup();
            let cases = ticks
                .into_iter()
                .map(|tick| {
                    let frame = rate
                        .frame_at_or_before(MediaTime::from_ticks(tick))
                        .expect("frame");
                    json!({ "tick": tick, "frame": frame, "frameStart": start(rate, frame) })
                })
                .collect::<Vec<_>>();
            json!({ "rate": rate_json(rate), "cases": cases })
        })
        .collect()
}

/// Export requests: the range the engine widens each to, its frame count,
/// and the samples its audio covers.
fn exports() -> Value {
    let project = |rate: FrameRate, sample_rate: u32| {
        EngineProject::new(Project::new(
            ProjectId::new("time-rules".to_owned()).expect("id"),
            "time rules",
            ProjectSettings::new(64, 36, rate, sample_rate).expect("settings"),
        ))
        .expect("project")
    };
    let mut cases = Vec::new();
    for pair in RATES {
        let rate = rate(pair);
        let requests = [
            (start(rate, 1), start(rate, 5)),
            (start(rate, 1) + 1, start(rate, 5) - 1),
            (start(rate, 7), start(rate, 8)),
            (0, 1),
            (2_000, 4_000),
            (1_333, 6_666),
            (2_502, 12_512),
            (833, 4_166),
            (120_000, 240_000),
        ];
        for sample_rate in [44_100, 48_000] {
            let snapshot = project(rate, sample_rate);
            for (start, end) in requests {
                let exporter = Exporter::new(
                    &snapshot,
                    range(start, end),
                    rate,
                    QualityProfile::export(),
                    VideoCodec::H264,
                )
                .expect("export");
                let effective = exporter.effective_range();
                let audio = exporter.audio_window(sample_rate).expect("audio window");
                cases.push(json!({
                    "rate": rate_json(rate),
                    "sampleRate": sample_rate,
                    "start": start,
                    "end": end,
                    "effectiveStart": effective.start().ticks(),
                    "effectiveEnd": effective.end().ticks(),
                    "total": exporter.progress().total,
                    "audioStartSample": audio.start_sample,
                    "audioFrames": audio.frames,
                }));
            }
        }
    }
    Value::Array(cases)
}

/// Curves, including the steep ones whose Newton step leaves the curve,
/// each at progress across and beyond `0..=1`.
fn curves() -> Value {
    let curves = [
        CubicBezier::new(0.42, 0.0, 1.0, 1.0),
        CubicBezier::new(0.0, 0.0, 0.58, 1.0),
        CubicBezier::new(0.42, 0.0, 0.58, 1.0),
        CubicBezier::new(0.25, 0.1, 0.25, 1.0),
        CubicBezier::new(0.0, 0.0, 0.0, 1.0),
        CubicBezier::new(0.0, 1.0, 0.05, 1.0),
        CubicBezier::new(0.0, 0.0, 0.05, 1.0),
        CubicBezier::new(0.001, 0.0, 0.0, 1.0),
        CubicBezier::new(1.0, 0.0, 1.0, 1.0),
        CubicBezier::new(0.95, 0.0, 1.0, 1.0),
        CubicBezier::new(0.68, -0.6, 0.32, 1.6),
        CubicBezier::new(0.0, 0.0, 1.0, 1.0),
        CubicBezier::new(1.0, 1.0, 0.0, 0.0),
    ];
    let progress: Vec<f64> = [-0.5, 0.0, 0.001, 0.005, 0.01, 0.05]
        .into_iter()
        .chain((1..64).map(|step| f64::from(step) / 64.0))
        .chain([0.95, 0.99, 0.995, 0.999, 1.0, 1.5])
        .collect();
    curves
        .into_iter()
        .map(|curve| {
            let cases = progress
                .iter()
                .map(|&progress| json!([progress, curve.apply(progress)]))
                .collect::<Vec<_>>();
            json!({ "curve": curve, "cases": cases })
        })
        .collect()
}

/// Clips of every time map and speed: the source instant their picture
/// shows at timeline ticks on and beside each frame start.
fn source_times() -> Value {
    let maps = [
        TimeMap::Forward,
        TimeMap::Reverse,
        TimeMap::Freeze {
            source_time: MediaTime::from_ticks(50_000),
        },
        TimeMap::Loop {
            mode: LoopMode::Repeat,
        },
        TimeMap::Loop {
            mode: LoopMode::PingPong,
        },
    ];
    let spans = [
        ((0, 120_000), (0, 120_000)),
        ((60_000, 180_000), (12_000, 72_000)),
        ((0, 60_000), (10_000, 250_000)),
        ((40_000, 300_000), (10_000, 130_000)),
        // Starting and ending inside frames, twice as fast.
        ((0, 58_000), (0, 116_000)),
        ((40_001, 58_000), (10_000, 46_000)),
    ];
    let mut cases = Vec::new();
    for pair in [(30, 1), (90, 1), (30_000, 1_001), (24, 1)] {
        let rate = rate(pair);
        for map in maps {
            for (timeline, source) in spans {
                let (timeline, source) = (range(timeline.0, timeline.1), range(source.0, source.1));
                let first = rate
                    .frame_at_or_before(timeline.start())
                    .expect("first frame");
                let last = rate
                    .frame_at_or_before(MediaTime::from_ticks(timeline.end().ticks() - 1))
                    .expect("last frame");
                // The first and last frames, every seventh between, and both
                // sides of each loop pass boundary.
                let passes = (timeline.start().ticks()..timeline.end().ticks())
                    .step_by(
                        usize::try_from(source.duration().expect("span").ticks()).expect("span"),
                    )
                    .flat_map(|boundary| {
                        let frame = rate
                            .frame_at_or_before(MediaTime::from_ticks(boundary))
                            .expect("frame");
                        [boundary - 1, boundary, start(rate, frame + 1)]
                    });
                let mut ticks: Vec<i64> = (first..=last)
                    .filter(|frame| frame - first < 4 || last - frame < 4 || frame % 7 == 0)
                    .map(|frame| start(rate, frame))
                    .flat_map(|tick| [tick, tick + 1])
                    .chain(passes)
                    .chain([timeline.end().ticks() - 1])
                    .filter(|tick| timeline.contains(MediaTime::from_ticks(*tick)))
                    .collect();
                ticks.sort_unstable();
                ticks.dedup();
                let times = ticks
                    .into_iter()
                    .map(|tick| {
                        let shown = picture_source_time(
                            timeline,
                            source,
                            map,
                            rate,
                            MediaTime::from_ticks(tick),
                        )
                        .expect("source time");
                        json!([tick, shown.ticks()])
                    })
                    .collect::<Vec<_>>();
                cases.push(json!({
                    "rate": rate_json(rate),
                    "timeline": { "start": timeline.start().ticks(), "end": timeline.end().ticks() },
                    "source": { "start": source.start().ticks(), "end": source.end().ticks() },
                    "timeMap": map,
                    "cases": times,
                }));
            }
        }
    }
    Value::Array(cases)
}

/// Samples to the tick at or before them, negative ones included.
fn samples() -> Value {
    let mut cases = Vec::new();
    for sample_rate in [8_000, 22_050, 44_100, 48_000, 96_000] {
        for sample in [
            -48_001, -3, -2, -1, 0, 1, 2, 3, 44_099, 44_100, 48_000, 123_457,
        ] {
            cases.push(json!([
                sample,
                sample_rate,
                sample_ticks_floor(sample, sample_rate)
            ]));
        }
    }
    Value::Array(cases)
}

/// Exact times written as seconds, and the tick each is: seconds round to
/// the nearest tick, as the native host reads a time in seconds.
fn seconds() -> Value {
    let ntsc = rate((30_000, 1_001));
    let mut ticks: Vec<i64> = (0..=200)
        .map(|frame| start(ntsc, frame))
        .chain((0..200).map(|step| step * 7_919))
        .chain([-4_004, -1, 1, 58, 59, 120_119, 120_120, 9_007_199_254])
        .collect();
    ticks.sort_unstable();
    ticks.dedup();
    let scale = f64::from(u32::try_from(TICKS_PER_SECOND).expect("ticks per second"));
    ticks
        .into_iter()
        .map(|tick| {
            #[allow(clippy::cast_precision_loss)]
            let seconds = tick as f64 / scale;
            #[allow(clippy::cast_possible_truncation)]
            let read = (seconds * scale).round() as i64;
            assert_eq!(read, tick, "{seconds} s reads as its own tick");
            json!([seconds, tick])
        })
        .collect()
}

/// The presentation slack of streams whose first timestamps (in units of
/// their time base) round their frames or hold them exactly: the rounding
/// quantum they show and the slack it gives.
fn presentation_slacks() -> Value {
    let rounded = |num: i64, den: i64, base: i64, scale: i64| {
        (0..i64::try_from(alpha2_time::ROUNDING_SCAN_FRAMES).expect("frames"))
            .map(|frame| (2 * frame * den * base + num) / (2 * num) * scale)
            .collect::<Vec<_>>()
    };
    let streams: [((i64, i64), Vec<i64>); 14] = [
        ((1, 1_000), rounded(30_000, 1_001, 1_000, 1)),
        ((1, 16_000), rounded(30_000, 1_001, 1_000, 16)),
        ((1, 600), rounded(30_000, 1_001, 600, 1)),
        ((1, 100), rounded(30_000, 1_001, 100, 1)),
        ((1, 600), rounded(30, 1, 600, 1)),
        ((1, 1_000), rounded(25, 1, 1_000, 1)),
        ((1, 90_000), rounded(30_000, 1_001, 90_000, 1)),
        ((1_001, 30_000), (0..64).collect()),
        ((1, 12), (0..6).collect()),
        ((1, 90_000), vec![0, 1_500, 3_000, 6_000, 7_500, 10_500]),
        ((1, 1_000), vec![0, 17, 33, 67, 83, 100, 133, 150]),
        ((1, 15_360), vec![67, 0, 33, 33, 100, 133]),
        ((1, 1_000), vec![0, 33]),
        ((0, 0), vec![0, 33, 67, 100]),
    ];
    streams
        .into_iter()
        .map(|((num, den), timestamps)| {
            let quantum = alpha2_time::rounding_quantum(&timestamps);
            json!({
                "timeBase": { "num": num, "den": den },
                "timestamps": timestamps,
                "quantum": quantum,
                "slackTicks": alpha2_time::presentation_slack(num, den, quantum).ticks(),
            })
        })
        .collect()
}

/// The frame grids of audio streams whose first timestamps (in units of
/// their time base) round their frames or hold them exactly, and where a
/// first frame lands on each.
fn audio_frame_grids() -> Value {
    // `count` frames of `period` samples at `rate` from sample `start`, at the nearest `1 / den` s,
    // a half away from zero (as `FFmpeg`'s `av_rescale_q` rounds).
    let rounded = |start: i64, period: i64, rate: i64, den: i64, count: i64| {
        (0..count)
            .map(|k| {
                let scaled = 2 * (start + k * period) * den;
                scaled.signum() * ((scaled.abs() + rate) / (2 * rate))
            })
            .collect::<Vec<_>>()
    };
    let frames = i64::try_from(alpha2_time::ROUNDING_SCAN_FRAMES).expect("frames");
    // Matroska's milliseconds copied into MP4 (`audio-seek-aac-remux.mp4`): 0, 1024, then 8 short.
    let remux = [0, 1_024]
        .into_iter()
        .chain((2..frames).map(|k| k * 1_024 - 8))
        .collect::<Vec<_>>();
    let mut missing = rounded(0, 1_024, 48_000, 1_000, frames);
    missing.remove(10);
    let streams: [(i64, u32, Vec<i64>); 12] = [
        // AAC in Matroska, 48 and 44.1 kHz, from its priming frame.
        (1_000, 48_000, rounded(-1_024, 1_024, 48_000, 1_000, frames)),
        (1_000, 44_100, rounded(-1_024, 1_024, 44_100, 1_000, frames)),
        // Opus in WebM, libopus's 312-sample pre-skip and FFmpeg's 120.
        (1_000, 48_000, rounded(-312, 960, 48_000, 1_000, frames)),
        (1_000, 48_000, rounded(-120, 960, 48_000, 1_000, frames)),
        (48_000, 48_000, remux),
        // A time base of the sample rate, and MPEG-TS's 1/90000.
        (48_000, 48_000, (0..frames).map(|k| k * 1_024).collect()),
        (
            90_000,
            48_000,
            (0..frames).map(|k| 126_000 + k * 1_920).collect(),
        ),
        // Vorbis's long and short blocks; a missing packet; too coarse a unit; too few.
        (1_000, 48_000, vec![0, 3, 15, 36, 58, 79, 100]),
        (1_000, 48_000, missing),
        (50, 48_000, rounded(0, 1_024, 48_000, 50, frames)),
        (1_000, 48_000, vec![0, 21]),
        // Opus's 20 ms frames, which milliseconds hold exactly.
        (1_000, 48_000, (0..frames).map(|k| k * 20).collect()),
    ];
    streams
        .into_iter()
        .map(|(den, rate, timestamps)| {
            let grid = alpha2_time::audio_frame_grid(&timestamps, 1, den, rate);
            let placements = grid.map_or_else(Vec::new, |grid| {
                // A whole frame near each of the first timestamps, a trimmed one ending near one,
                // and one too far from the grid either way.
                let unit = i64::from(rate) / den;
                timestamps
                    .iter()
                    .take(4)
                    .flat_map(|&timestamp| {
                        let at = timestamp * i64::from(rate) / den;
                        [
                            (at, grid.period),
                            (at + unit, grid.period),
                            (at + grid.period / 3, grid.period - grid.period / 3),
                            (at + 3 * unit, grid.period),
                        ]
                    })
                    .map(|(at, frames)| json!([at, frames, grid.place(at, frames)]))
                    .collect()
            });
            json!({
                "timeResolution": den,
                "sampleRate": rate,
                "timestamps": timestamps,
                "grid": grid.map(|grid| json!({
                    "anchor": grid.anchor,
                    "period": grid.period,
                    "startTolerance": grid.start_tolerance,
                    "trimmedTolerance": grid.trimmed_tolerance,
                })),
                "placements": placements,
            })
        })
        .collect()
}

#[test]
fn the_time_rules_table_is_the_engines() {
    let table = json!({
        "description": "The engine's time rules, generated by crates/alpha2-engine/tests/time_rules.rs (ALPHA2_UPDATE_GOLDENS=1 rewrites it). TypeScript ports run it: alpha2 (Alpha2Exporter, secondsToTicks, samplesToTicks, roundingQuantum, presentationSlackTicks, audioFrameGrid, placeOnAudioFrameGrid) and @alpha2-sh/editor (frameAt, frameStart, cubicBezierAt, clipSourceTime, clipTimelineTime).",
        "frames": frames(),
        "exports": exports(),
        "curves": curves(),
        "sourceTimes": source_times(),
        "samplesToTicks": samples(),
        "secondsToTicks": seconds(),
        "presentationSlack": presentation_slacks(),
        "audioFrameGrid": audio_frame_grids(),
    });
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(&table).expect("serializes")
    );
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/time-rules-corpus.json");
    if std::env::var_os("ALPHA2_UPDATE_GOLDENS").as_deref() == Some(std::ffi::OsStr::new("1")) {
        std::fs::write(&path, &text).expect("writes the table");
        return;
    }
    let pinned = std::fs::read_to_string(&path).expect("tests/golden/time-rules-corpus.json");
    assert!(
        pinned == text,
        "tests/golden/time-rules-corpus.json differs from the engine's rules; rerun with ALPHA2_UPDATE_GOLDENS=1 after an intended change"
    );
}
