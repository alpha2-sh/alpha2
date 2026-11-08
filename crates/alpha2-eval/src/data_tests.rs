use alpha2_model::{LoopMode, Project, TimeMap, Vec2};
use alpha2_time::{FrameRate, MediaTime, TimeRange};
use serde_json::{Value, json};

use crate::{
    ActiveIndex, SampleContext, picture_source_time, sample_scalar, sample_visual,
    source_instant_to_timeline, source_range_to_timeline,
};

fn range(start: i64, end: i64) -> TimeRange {
    TimeRange::new(MediaTime(start), MediaTime(end)).expect("ordered range")
}

const MAPS: [TimeMap; 5] = [
    TimeMap::Forward,
    TimeMap::Reverse,
    TimeMap::Freeze {
        source_time: MediaTime(1_350),
    },
    TimeMap::Loop {
        mode: LoopMode::Repeat,
    },
    TimeMap::Loop {
        mode: LoopMode::PingPong,
    },
];

/// The timeline ticks of a clip showing a source time in `from..to`, by trying every tick.
fn brute_force(
    timeline: TimeRange,
    source: TimeRange,
    map: TimeMap,
    rate: FrameRate,
    from: i64,
    to: i64,
) -> Vec<TimeRange> {
    let mut ranges: Vec<TimeRange> = Vec::new();
    for tick in timeline.start().ticks()..timeline.end().ticks() {
        let shown = picture_source_time(timeline, source, map, rate, MediaTime(tick))
            .expect("source time")
            .ticks();
        if !(from..to).contains(&shown) {
            continue;
        }
        match ranges.last_mut() {
            Some(last) if last.end().ticks() == tick => {
                *last = range(last.start().ticks(), tick + 1)
            }
            _ => ranges.push(range(tick, tick + 1)),
        }
    }
    ranges
}

#[test]
fn the_preimage_of_every_time_map_matches_brute_force() {
    // 3000 frames a second: a frame is 40 ticks, so reversed clips mirror by frame.
    let rate = FrameRate::new(3_000, 1).expect("rate");
    let spans = [
        (range(0, 2_400), range(1_000, 3_400)),
        (range(100, 1_300), range(1_000, 3_400)),
        (range(37, 4_837), range(1_000, 2_200)),
        (range(0, 3_000), range(1_200, 1_500)),
    ];
    for map in MAPS {
        for (timeline, source) in spans {
            for (from, to) in [
                (source.start().ticks(), source.end().ticks()),
                (1_100, 1_400),
                (1_350, 1_351),
                (1_499, 1_900),
                (900, 1_001),
                (3_399, 4_000),
            ] {
                let found = source_range_to_timeline(
                    timeline,
                    source,
                    map,
                    rate,
                    MediaTime(from),
                    MediaTime(to),
                )
                .expect("preimage");
                assert_eq!(
                    found,
                    brute_force(timeline, source, map, rate, from, to),
                    "{map:?} {timeline:?} {source:?} {from}..{to}"
                );
            }
        }
    }
}

#[test]
fn an_instant_is_reached_once_per_pass() {
    let rate = FrameRate::new(3_000, 1).expect("rate");
    let (timeline, source) = (range(0, 900), range(1_000, 1_300));
    let at = |map| {
        source_instant_to_timeline(timeline, source, map, rate, MediaTime(1_100)).expect("times")
    };
    assert_eq!(at(TimeMap::Forward), vec![MediaTime(300)]);
    assert_eq!(
        at(TimeMap::Loop {
            mode: LoopMode::Repeat
        }),
        vec![MediaTime(100), MediaTime(400), MediaTime(700)]
    );
    assert_eq!(
        at(TimeMap::Loop {
            mode: LoopMode::PingPong
        })
        .len(),
        3
    );
    assert_eq!(
        at(TimeMap::Freeze {
            source_time: MediaTime(1_100)
        }),
        Vec::new()
    );
    for time in at(TimeMap::Reverse) {
        let shown =
            picture_source_time(timeline, source, TimeMap::Reverse, rate, time).expect("time");
        let before = picture_source_time(
            timeline,
            source,
            TimeMap::Reverse,
            rate,
            MediaTime(time.ticks() - 1),
        )
        .expect("time");
        assert!(shown.ticks() <= 1_100 && before.ticks() > 1_100);
    }
}

/// A 1920x1080 project at `rate` frames a second with a video clip `c1` of source `v1` and an
/// audio clip `c3` of `a1`, plus `data` and edits by `edit`.
fn project(rate: (u32, u32), data: &Value, edit: impl FnOnce(&mut Value)) -> Project {
    let mut document = json!({
        "schema_version": 1, "id": "p", "name": "P",
        "settings": { "width": 1920, "height": 1080, "frame_rate": { "num": rate.0, "den": rate.1 }, "sample_rate": 48_000 },
        "sources": [
            { "id": "v1", "kind": "video", "locator": "v1.mp4",
              "fingerprint": { "streams": { "video": { "width": 1920, "height": 1080 } } } },
            { "id": "v2", "kind": "video", "locator": "v2.mp4" },
            { "id": "a1", "kind": "audio", "locator": "a1.m4a" }
        ],
        "timeline": { "tracks": [
            { "id": "t1", "name": "T1", "kind": "video", "clips": [
                { "id": "c1", "source_id": "v1", "timeline_range": { "start": 0, "end": 240_000 },
                  "source_range": { "start": 0, "end": 240_000 } } ] },
            { "id": "t2", "name": "T2", "kind": "video", "clips": [
                { "id": "c2", "source_id": "v2", "timeline_range": { "start": 0, "end": 240_000 },
                  "source_range": { "start": 0, "end": 240_000 } } ] },
            { "id": "t3", "name": "T3", "kind": "audio", "clips": [
                { "id": "c3", "source_id": "a1", "timeline_range": { "start": 0, "end": 240_000 },
                  "source_range": { "start": 0, "end": 240_000 } } ] }
        ] },
        "data": data
    });
    edit(&mut document);
    Project::from_json(&document.to_string()).expect("valid project")
}

/// Scalars on source `v1` at every 24 fps source frame, valued by frame number.
fn frame_numbers(interpolation: &str) -> Value {
    let times = (0..48).map(|frame| frame * 5_000).collect::<Vec<i64>>();
    let values = (0..48_u8).map(f32::from).collect::<Vec<f32>>();
    json!([{ "id": "frames", "anchor": { "type": "source", "source_id": "v1" }, "kind": "scalars",
             "interpolation": interpolation, "channels": ["n"], "times": times, "values": values }])
}

fn opacity_follows(document: &mut Value) {
    document["timeline"]["tracks"][0]["clips"][0]["visual"] = json!({
        "rotation_degrees": { "mode": "data", "data": "frames", "channel": "n", "via": { "clip": "c1" } } });
}

fn rotation(project: &Project, index: &ActiveIndex, tick: i64) -> f32 {
    let clip = &project.timeline.tracks[0].clips[0];
    let context = index.sample_context(project);
    sample_scalar(&clip.visual.rotation_degrees, MediaTime(tick), &context)
}

#[test]
fn a_held_set_stays_locked_to_the_frame_shown_at_any_rate() {
    // A 24 fps source on a 60 fps timeline: every timeline frame reads the source frame it shows.
    let project = project((60, 1), &frame_numbers("hold"), opacity_follows);
    let index = ActiveIndex::build(&project).expect("index");
    for frame in 0..119 {
        let tick = frame * 2_000;
        let shown = picture_source_time(
            range(0, 240_000),
            range(0, 240_000),
            TimeMap::Forward,
            project.settings.frame_rate,
            MediaTime(tick),
        )
        .expect("time");
        #[allow(clippy::cast_precision_loss)]
        let expected = (shown.ticks() / 5_000) as f32;
        assert_eq!(rotation(&project, &index, tick), expected, "frame {frame}");
    }
}

#[test]
fn every_time_map_reads_the_source_time_it_shows() {
    for map in MAPS {
        for interpolation in ["hold", "linear"] {
            let project = project((30, 1), &frame_numbers(interpolation), |document| {
                opacity_follows(document);
                let clip = &mut document["timeline"]["tracks"][0]["clips"][0];
                clip["time_map"] = serde_json::to_value(map).expect("map");
                clip["source_range"] = json!({ "start": 20_000, "end": 120_000 });
                if let TimeMap::Freeze { .. } = map {
                    clip["time_map"] = json!({ "type": "freeze", "source_time": 37_000 });
                }
            });
            let index = ActiveIndex::build(&project).expect("index");
            let clip = &project.timeline.tracks[0].clips[0];
            for tick in (0..240_000).step_by(3_331) {
                let shown = picture_source_time(
                    clip.timeline_range,
                    clip.source_range,
                    clip.time_map,
                    project.settings.frame_rate,
                    MediaTime(tick),
                )
                .expect("time")
                .ticks();
                #[allow(clippy::cast_precision_loss)]
                let expected = if interpolation == "hold" {
                    (shown / 5_000) as f32
                } else {
                    crate::animation::interpolate_f32(
                        (shown / 5_000) as f32,
                        (shown / 5_000 + 1) as f32,
                        crate::animation::interpolation_amount(
                            MediaTime(shown / 5_000 * 5_000),
                            MediaTime((shown / 5_000 + 1) * 5_000),
                            MediaTime(shown),
                        ),
                    )
                };
                assert_eq!(
                    rotation(&project, &index, tick),
                    expected,
                    "{map:?} {interpolation} at {tick}"
                );
            }
        }
    }
}

#[test]
fn trimming_and_moving_a_clip_keeps_the_lock() {
    let original = project((30, 1), &frame_numbers("hold"), opacity_follows);
    let moved = project((30, 1), &frame_numbers("hold"), |document| {
        opacity_follows(document);
        let clip = &mut document["timeline"]["tracks"][0]["clips"][0];
        clip["timeline_range"] = json!({ "start": 50_000, "end": 150_000 });
        clip["source_range"] = json!({ "start": 30_000, "end": 130_000 });
    });
    let (before, after) = (
        ActiveIndex::build(&original).expect("index"),
        ActiveIndex::build(&moved).expect("index"),
    );
    for tick in (50_000..150_000).step_by(4_000) {
        // The moved clip shows at `tick` what the original showed at `tick - 20000`.
        assert_eq!(
            rotation(&moved, &after, tick),
            rotation(&original, &before, tick - 20_000)
        );
    }
}

#[test]
fn lost_samples_hold_and_ends_clamp() {
    let samples = alpha2_model::DataSamples {
        interpolation: alpha2_model::DataInterpolation::Linear,
        channels: vec!["a".into()],
        times: vec![
            MediaTime(100),
            MediaTime(200),
            MediaTime(300),
            MediaTime(400),
        ],
        values: vec![Some(1.0), None, Some(3.0), Some(5.0)],
    };
    let at = |tick| crate::sample_channel(&samples, 0, 1, MediaTime(tick)).map(|value| value[0]);
    assert_eq!(at(0), Some(1.0));
    assert_eq!(at(150), Some(1.0));
    assert_eq!(at(250), Some(1.0));
    assert_eq!(at(350), Some(4.0));
    assert_eq!(at(900), Some(5.0));
    let lost = alpha2_model::DataSamples {
        values: vec![None; 4],
        ..samples
    };
    assert_eq!(crate::sample_channel(&lost, 0, 1, MediaTime(250)), None);
}

fn points_on_v1() -> Value {
    json!([{ "id": "face", "anchor": { "type": "source", "source_id": "v1" }, "kind": "points",
             "interpolation": "hold", "channels": ["nose"], "times": [0], "values": [0.25, 0.75] }])
}

#[test]
fn a_point_in_canvas_space_follows_the_other_clips_transform() {
    let project = project((30, 1), &points_on_v1(), |document| {
        document["timeline"]["tracks"][0]["clips"][0]["visual"] = json!({
            "position": { "mode": "constant", "value": { "x": 0.6, "y": 0.4 } },
            "scale": { "mode": "constant", "value": { "x": 0.5, "y": 0.5 } },
            "rotation_degrees": { "mode": "constant", "value": 90 } });
        document["timeline"]["tracks"][1]["clips"][0]["visual"] = json!({
            "position": { "mode": "data", "data": "face", "channel": "nose", "via": { "clip": "c1" },
                          "space": "canvas", "offset": [0, 0.01] } });
    });
    let index = ActiveIndex::build(&project).expect("index");
    let context = index.sample_context(&project);
    let visual = sample_visual(
        &project.timeline.tracks[1].clips[0].visual,
        MediaTime(1_000),
        16.0 / 9.0,
        &context,
    );
    // The point is (-0.25, 0.25) from the anchor, scaled by half in pixels and turned a quarter.
    let aspect = 16.0_f64 / 9.0;
    let (x, y) = (-0.25 * 0.5 * aspect, 0.25 * 0.5);
    #[allow(clippy::cast_possible_truncation)]
    let expected = Vec2::new((0.6 + -y / aspect) as f32, (0.4 + x + 0.01) as f32);
    assert!(
        (visual.position.x - expected.x).abs() < 1e-6
            && (visual.position.y - expected.y).abs() < 1e-6,
        "{visual:?} {expected:?}"
    );
}

#[test]
fn a_mask_follows_a_point_on_its_own_clip() {
    let project = project((30, 1), &points_on_v1(), |document| {
        document["timeline"]["tracks"][0]["clips"][0]["masks"] = json!([{ "shape": { "type": "ellipse",
            "center": { "mode": "data", "data": "face", "channel": "nose", "via": { "clip": "c1" }, "space": "picture" },
            "size": { "mode": "constant", "value": { "x": 0.2, "y": 0.2 } } } }]);
    });
    let index = ActiveIndex::build(&project).expect("index");
    let context = index.sample_context(&project);
    let mask = &project.timeline.tracks[0].clips[0].masks[0];
    let alpha2_geometry::EvaluatedGeometry::Ellipse { center, .. } =
        crate::evaluate_geometry(&mask.shape, MediaTime(0), 16.0 / 9.0, &context)
    else {
        panic!("an ellipse");
    };
    assert_eq!((center.x, center.y), (0.25, 0.75));
}

#[test]
fn a_narrower_picture_maps_into_its_contained_rectangle() {
    let project = project((30, 1), &points_on_v1(), |document| {
        document["sources"][0]["fingerprint"] =
            json!({ "streams": { "video": { "width": 1080, "height": 1080 } } });
        document["timeline"]["tracks"][0]["clips"][0]["visual"] = json!({
            "position": { "mode": "data", "data": "face", "channel": "nose", "via": { "clip": "c1" }, "space": "picture" } });
    });
    let index = ActiveIndex::build(&project).expect("index");
    let context = index.sample_context(&project);
    let position = sample_visual(
        &project.timeline.tracks[0].clips[0].visual,
        MediaTime(0),
        16.0 / 9.0,
        &context,
    )
    .position;
    // A square picture fills 9/16 of the layer's width, centered.
    assert!(
        (position.x - (0.5 - 0.25 * 9.0 / 16.0)).abs() < 1e-6 && (position.y - 0.75).abs() < 1e-6
    );
}

#[test]
fn a_canvas_cycle_fails_to_index() {
    let document = json!({ "data": points_on_v1() });
    let mut both = project((30, 1), &json!([]), |_| {});
    both.data = serde_json::from_value(document["data"].clone()).expect("data");
    both.data.push(serde_json::from_value(json!({ "id": "back", "anchor": { "type": "source", "source_id": "v2" },
        "kind": "points", "interpolation": "hold", "channels": ["nose"], "times": [0], "values": [0.5, 0.5] })).expect("set"));
    let follow = |data: &str, clip: &str| -> alpha2_model::AnimatedPoint {
        serde_json::from_value(json!({ "mode": "data", "data": data, "channel": "nose", "via": { "clip": clip }, "space": "canvas" }))
            .expect("binding")
    };
    both.timeline.tracks[0].clips[0].visual.position = follow("back", "c2");
    both.timeline.tracks[1].clips[0].visual.position = follow("face", "c1");
    assert!(matches!(
        ActiveIndex::build(&both),
        Err(crate::EvalError::InvalidProject(
            alpha2_model::ModelError::InvalidDataBinding { .. }
        ))
    ));
}

#[test]
fn a_gain_bound_to_data_changes_only_at_sample_times() {
    let project = project((30, 1), &frame_numbers("hold"), |document| {
        document["sources"][2]["id"] = json!("a1");
        document["data"][0]["anchor"]["source_id"] = json!("a1");
        document["timeline"]["tracks"][2]["clips"][0]["audio"] = json!({
            "gain": { "mode": "data", "data": "frames", "channel": "n", "via": { "clip": "c3" },
                      "remap": { "input": [0, 48], "output": [0, 1] } } });
    });
    let index = ActiveIndex::build(&project).expect("index");
    let context: SampleContext<'_> = index.sample_context(&project);
    let alpha2_model::Animated::Data(binding) = &project.timeline.tracks[2].clips[0].audio.gain
    else {
        panic!("a binding");
    };
    let mut tick = 0;
    let mut knots = Vec::new();
    while let Some(next) = crate::next_data_change(binding, MediaTime(tick), &context) {
        knots.push(next.ticks());
        tick = next.ticks();
    }
    assert_eq!(
        knots,
        (1..48).map(|frame| frame * 5_000).collect::<Vec<i64>>()
    );
}
