//! The plugin over synthetic frame sequences with cuts, fades, flashes, noise and black.

use std::collections::HashMap;

use alpha2_plugin_abi::records::Record;
use alpha2_plugin_abi::status;
use alpha2_plugin_sdk::testing::Instance;
use serde_json::{Value, json};

const WIDTH: usize = 160;
const HEIGHT: usize = 90;
const TICKS_PER_FRAME: i64 = 4_000;
const LUMA8: u32 = 1;

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn jitter(&mut self, amplitude: i32) -> i32 {
        let span = u64::try_from(2 * amplitude + 1).expect("positive");
        i32::try_from(self.next() % span).expect("small") - amplitude
    }
}

type Picture = Vec<u8>;

fn shot(seed: u64, offset: i64) -> Picture {
    let mut picture = Vec::with_capacity(WIDTH * HEIGHT);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let column = (i64::try_from(x).expect("small") + offset).div_euclid(20);
            let block = (column.cast_unsigned() << 8) ^ (y as u64 / 18);
            let mut rng = SplitMix64(seed.wrapping_mul(0x1_0000_0001) ^ block);
            picture.push(40 + u8::try_from(rng.next() % 176).expect("small"));
        }
    }
    picture
}

fn flat(level: u8) -> Picture {
    vec![level; WIDTH * HEIGHT]
}

fn noisy(picture: &[u8], rng: &mut SplitMix64, amplitude: i32) -> Picture {
    picture
        .iter()
        .map(|&value| {
            u8::try_from((i32::from(value) + rng.jitter(amplitude)).clamp(0, 255)).expect("clamped")
        })
        .collect()
}

fn mix(a: &[u8], b: &[u8], step: u32, steps: u32) -> Picture {
    a.iter()
        .zip(b)
        .map(|(&a, &b)| {
            let value = (u32::from(a) * (steps - step) + u32::from(b) * step + steps / 2) / steps;
            u8::try_from(value).expect("a level")
        })
        .collect()
}

fn brighter(picture: &[u8], by: u8) -> Picture {
    picture
        .iter()
        .map(|&value| value.saturating_add(by))
        .collect()
}

struct Sequence {
    frames: Vec<Picture>,
    rng: SplitMix64,
    amplitude: i32,
}

impl Sequence {
    fn new(amplitude: i32) -> Self {
        Self {
            frames: Vec::new(),
            rng: SplitMix64(7),
            amplitude,
        }
    }

    fn push(&mut self, picture: &[u8]) {
        let frame = noisy(picture, &mut self.rng, self.amplitude);
        self.frames.push(frame);
    }

    fn shot(&mut self, seed: u64, frames: usize) -> &mut Self {
        for index in 0..frames {
            self.push(&shot(seed, i64::try_from(index / 2).expect("small")));
        }
        self
    }

    fn still(&mut self, picture: &[u8], frames: usize) -> &mut Self {
        for _ in 0..frames {
            self.push(picture);
        }
        self
    }

    fn blend(&mut self, from: &[u8], to: &[u8], frames: u32) -> &mut Self {
        for step in 1..=frames {
            self.push(&mix(from, to, step, frames + 1));
        }
        self
    }

    fn len(&self) -> i64 {
        i64::try_from(self.frames.len()).expect("small")
    }
}

#[derive(Clone, Copy)]
enum Way {
    Forward,
    Backward,
}

#[derive(Debug, Default, PartialEq)]
struct Found {
    cuts: Vec<(i64, f32)>,
    black: Vec<(i64, i64, f32)>,
}

impl Found {
    fn cut_frames(&self) -> Vec<i64> {
        self.cuts
            .iter()
            .map(|(at, _)| at / TICKS_PER_FRAME)
            .collect()
    }

    fn black_frames(&self) -> Vec<(i64, i64)> {
        self.black
            .iter()
            .map(|(start, end, _)| (start / TICKS_PER_FRAME, end / TICKS_PER_FRAME))
            .collect()
    }
}

fn instance() -> Instance {
    // SAFETY: the plugin's own entry returns its static table.
    unsafe { Instance::new(alpha2_plugin_scenes::alpha2_plugin_v1(), HashMap::new()) }
}

fn context(range: (i64, i64), ways: &[Way], params: &Value) -> String {
    let passes = ways
        .iter()
        .map(|way| match way {
            Way::Forward => json!({ "start": range.0, "direction": "forward" }),
            Way::Backward => json!({ "start": range.1 - 1, "direction": "backward" }),
        })
        .collect::<Vec<_>>();
    json!({
        "range": { "start": range.0, "end": range.1 }, "passes": passes,
        "from": "source", "video": { "size": 160, "format": "luma8" }, "audio": null,
        "frame_rate": null, "params": params
    })
    .to_string()
}

fn begin_failure(params: &Value) -> Option<String> {
    let mut plugin = instance();
    let range = (0, 10 * TICKS_PER_FRAME);
    (plugin.begin(&context(range, &[Way::Forward], params)) != status::OK)
        .then(|| plugin.take().failure.expect("a message"))
}

fn run_over(frames: &[Picture], range: (i64, i64), ways: &[Way], params: &Value) -> Found {
    let mut plugin = instance();
    assert_eq!(
        plugin.begin(&context(range, ways, params)),
        status::OK,
        "{:?}",
        plugin.take().failure
    );
    let mut found = Found::default();
    let mut keep = |records: Vec<Record>, delivered: Option<(i64, Way)>, latest: &mut [i64; 2]| {
        for record in records {
            let Record::Interval {
                output,
                start,
                end,
                label,
                score,
            } = record
            else {
                panic!("an interval, not {record:?}");
            };
            let score = score.expect("a score");
            assert!(label.is_none());
            assert!(
                range.0 <= start && start <= end && end <= range.1,
                "{start}..{end}"
            );
            assert!(score.is_finite() && (0.0..=1.0).contains(&score), "{score}");
            if let Some((time, way)) = delivered {
                let slot = &mut latest[usize::from(output)];
                match way {
                    Way::Forward => {
                        assert!(end <= time, "{start}..{end} emitted at {time}");
                        assert!(start >= *slot, "{start} after {slot}");
                    }
                    Way::Backward => {
                        assert!(start >= time, "{start}..{end} emitted at {time}");
                        assert!(start <= *slot, "{start} after {slot}");
                    }
                }
                *slot = start;
            }
            match output {
                0 => {
                    assert_eq!(start, end, "a cut is an instant");
                    found.cuts.push((start, score));
                }
                1 => found.black.push((start, end, score)),
                other => panic!("output {other}"),
            }
        }
    };
    let last = frames.len() - 1;
    let index_at = |time: i64| {
        usize::try_from(time.div_euclid(TICKS_PER_FRAME))
            .expect("inside")
            .min(last)
    };
    for (pass, &way) in ways.iter().enumerate() {
        let direction = match way {
            Way::Forward => 0,
            Way::Backward => 1,
        };
        assert_eq!(
            plugin.pass(u32::try_from(pass).expect("few"), direction),
            status::OK
        );
        let (first, last) = (index_at(range.0), index_at(range.1 - 1));
        let order: Vec<usize> = match way {
            Way::Forward => (first..=last).collect(),
            Way::Backward => (first..=last).rev().collect(),
        };
        let mut latest = match way {
            Way::Forward => [i64::MIN; 2],
            Way::Backward => [i64::MAX; 2],
        };
        for index in order {
            let time = i64::try_from(index).expect("small") * TICKS_PER_FRAME;
            let frame = &frames[index];
            let result = plugin.video(time, 160, 90, LUMA8, frame);
            let received = plugin.take();
            assert_eq!(result, status::OK, "{:?}", received.failure);
            keep(received.records, Some((time, way)), &mut latest);
        }
    }
    assert_eq!(plugin.end(), status::OK, "{:?}", plugin.take().failure);
    keep(plugin.take().records, None, &mut [0; 2]);
    found.cuts.sort_by_key(|(at, _)| *at);
    found.black.sort_by_key(|(start, end, _)| (*start, *end));
    found
}

fn run(sequence: &Sequence, params: &Value) -> Found {
    let range = (0, sequence.len() * TICKS_PER_FRAME);
    run_over(&sequence.frames, range, &[Way::Forward], params)
}

fn hard_cuts() -> Sequence {
    let mut sequence = Sequence::new(6);
    for (seed, frames) in [(1, 40), (2, 25), (3, 60), (4, 18), (5, 45)] {
        sequence.shot(seed, frames);
    }
    sequence
}

#[test]
fn hard_cuts_are_found_at_the_first_frame_of_each_new_shot() {
    let found = run(&hard_cuts(), &json!({}));
    assert_eq!(found.cut_frames(), [40, 65, 125, 143]);
    assert!(
        found.cuts.iter().all(|(_, score)| *score > 0.5),
        "{found:?}"
    );
    assert_eq!(found.black, []);
}

#[test]
fn every_sensitivity_finds_the_hard_cuts() {
    for sensitivity in [0.0, 0.25, 0.75, 1.0] {
        let found = run(&hard_cuts(), &json!({ "sensitivity": sensitivity }));
        assert_eq!(found.cut_frames(), [40, 65, 125, 143], "{sensitivity}");
    }
}

#[test]
fn a_backward_pass_finds_what_a_forward_pass_finds() {
    let mut sequence = hard_cuts();
    sequence.still(&flat(0), 30).shot(6, 30);
    let range = (0, sequence.len() * TICKS_PER_FRAME);
    let forward = run_over(&sequence.frames, range, &[Way::Forward], &json!({}));
    let backward = run_over(&sequence.frames, range, &[Way::Backward], &json!({}));
    assert_eq!(forward.cut_frames(), [40, 65, 125, 143, 188, 218]);
    assert_eq!(backward.cut_frames(), forward.cut_frames());
    assert_eq!(forward.black_frames(), [(188, 218)]);
    assert_eq!(backward.black_frames(), forward.black_frames());
    let both = run_over(
        &sequence.frames,
        range,
        &[Way::Forward, Way::Backward],
        &json!({}),
    );
    assert_eq!(both.cut_frames(), forward.cut_frames(), "each cut once");
    assert_eq!(
        both.black_frames(),
        forward.black_frames(),
        "each stretch once"
    );
}

#[test]
fn fades_through_black_of_every_length_make_no_cut() {
    for frames in [6, 12, 45] {
        let mut sequence = Sequence::new(6);
        let (from, to) = (shot(1, 0), shot(2, 0));
        sequence
            .still(&from, 30)
            .blend(&from, &flat(0), frames)
            .still(&flat(0), 8)
            .blend(&flat(0), &to, frames)
            .still(&to, 30);
        let found = run(&sequence, &json!({}));
        assert_eq!(found.cuts, [], "a fade of {frames} frames");
    }
}

#[test]
fn dissolves_of_every_length_make_no_cut() {
    for frames in [6, 12, 30, 60] {
        let mut sequence = Sequence::new(6);
        let (from, to) = (shot(1, 0), shot(2, 0));
        sequence
            .still(&from, 30)
            .blend(&from, &to, frames)
            .still(&to, 30);
        let found = run(&sequence, &json!({}));
        assert_eq!(found.cuts, [], "a dissolve of {frames} frames");
    }
}

#[test]
fn flashes_of_up_to_three_frames_make_no_cut() {
    let mut sequence = Sequence::new(6);
    let (scene, other) = (shot(1, 0), shot(2, 0));
    sequence
        .still(&scene, 20)
        .still(&flat(255), 1)
        .still(&scene, 20)
        .still(&other, 1)
        .still(&scene, 20)
        .still(&flat(0), 1)
        .still(&scene, 20)
        .still(&flat(255), 2)
        .still(&scene, 20)
        .still(&other, 3)
        .still(&scene, 20);
    let found = run(&sequence, &json!({}));
    assert_eq!(found, Found::default());
}

#[test]
fn camera_noise_and_shake_make_no_cut_even_at_the_highest_sensitivity() {
    let mut sequence = Sequence::new(24);
    let mut shake = SplitMix64(11);
    for _ in 0..300 {
        sequence.still(&shot(3, i64::from(shake.jitter(2))), 1);
    }
    for sensitivity in [0.5, 1.0] {
        let found = run(&sequence, &json!({ "sensitivity": sensitivity }));
        assert_eq!(found.cuts, [], "sensitivity {sensitivity}");
    }
}

#[test]
fn a_subtle_cut_is_found_only_at_a_high_sensitivity() {
    let mut sequence = Sequence::new(6);
    let scene = shot(1, 0);
    sequence.still(&scene, 40).still(&brighter(&scene, 5), 40);
    assert_eq!(run(&sequence, &json!({})).cuts, []);
    assert_eq!(
        run(&sequence, &json!({ "sensitivity": 1 })).cut_frames(),
        [40]
    );
}

#[test]
fn a_cut_far_past_the_threshold_scores_higher_than_one_just_past_it() {
    let mut sequence = Sequence::new(2);
    let scene = shot(1, 0);
    sequence
        .still(&scene, 40)
        .still(&brighter(&scene, 12), 40)
        .still(&flat(250), 40);
    let found = run(&sequence, &json!({}));
    assert_eq!(found.cut_frames(), [40, 80]);
    let (near, far) = (found.cuts[0].1, found.cuts[1].1);
    assert!(
        0.0 < near && near < 0.5 && far > 0.9 && far < 1.0,
        "{near} {far}"
    );
}

#[test]
fn a_cut_closer_than_the_shortest_shot_to_the_previous_is_dropped() {
    let mut sequence = Sequence::new(6);
    sequence.shot(1, 30).shot(2, 6).shot(3, 30);
    assert_eq!(run(&sequence, &json!({})).cut_frames(), [30]);
    assert_eq!(
        run(&sequence, &json!({ "min_scene_ms": 100 })).cut_frames(),
        [30, 36]
    );
    assert_eq!(
        run(&sequence, &json!({ "min_scene_ms": 2_000 })).cut_frames(),
        [30]
    );
    let range = (0, sequence.len() * TICKS_PER_FRAME);
    let backward = run_over(&sequence.frames, range, &[Way::Backward], &json!({}));
    assert_eq!(backward.cut_frames(), [36]);
    for (ways, kept) in [
        ([Way::Forward, Way::Backward], 30),
        ([Way::Backward, Way::Forward], 36),
    ] {
        let both = run_over(&sequence.frames, range, &ways, &json!({}));
        assert_eq!(both.cut_frames(), [kept]);
    }
}

fn black_runs() -> Sequence {
    let mut sequence = Sequence::new(3);
    sequence
        .shot(1, 30)
        .still(&flat(0), 20)
        .shot(2, 30)
        .still(&flat(0), 6)
        .shot(2, 30)
        .shot(3, 20)
        .still(&flat(0), 25);
    sequence
}

#[test]
fn black_stretches_are_found_with_their_bounds_and_short_ones_ignored() {
    let sequence = black_runs();
    let found = run(&sequence, &json!({}));
    assert_eq!(found.black_frames(), [(30, 50), (136, 161)]);
    assert_eq!(
        found.black[1].1,
        sequence.len() * TICKS_PER_FRAME,
        "closes at the end"
    );
    assert!(
        found.black.iter().all(|(.., score)| *score > 0.9),
        "{found:?}"
    );
    let all = run(&sequence, &json!({ "min_black_ms": 0 }));
    assert_eq!(all.black_frames(), [(30, 50), (80, 86), (136, 161)]);
}

#[test]
fn the_black_level_decides_whether_a_dark_frame_is_black() {
    let mut sequence = Sequence::new(3);
    sequence.shot(1, 30).still(&flat(25), 30).shot(2, 30);
    assert_eq!(run(&sequence, &json!({})).black, []);
    let found = run(&sequence, &json!({ "black_level": 0.15 }));
    assert_eq!(found.black_frames(), [(30, 60)]);
    let score = found.black[0].2;
    assert!(
        (score - (1.0 - 25.0 / 255.0 / 0.15)).abs() < 0.02,
        "{score}"
    );
}

#[test]
fn a_range_starting_inside_a_black_frame_keeps_the_stretch_inside_it() {
    let sequence = black_runs();
    let range = (
        33 * TICKS_PER_FRAME + 1_500,
        sequence.len() * TICKS_PER_FRAME - 2_000,
    );
    let found = run_over(&sequence.frames, range, &[Way::Forward], &json!({}));
    assert_eq!(found.black[0].0, range.0);
    assert_eq!(found.black[0].1, 50 * TICKS_PER_FRAME);
    assert_eq!(found.black.last().map(|black| black.1), Some(range.1));
    let backward = run_over(&sequence.frames, range, &[Way::Backward], &json!({}));
    assert_eq!(backward.black, found.black);
}

#[test]
fn the_same_frames_give_the_same_output() {
    let sequence = black_runs();
    assert_eq!(run(&sequence, &json!({})), run(&sequence, &json!({})));
}

#[test]
fn params_outside_the_schema_fail_begin_naming_the_key() {
    for (params, key) in [
        (json!({ "sensitivity": "high" }), "sensitivity"),
        (json!({ "sensitivity": 1.5 }), "sensitivity"),
        (json!({ "min_scene_ms": -1 }), "min_scene_ms"),
        (json!({ "black_level": 0 }), "black_level"),
        (json!({ "black_level": null }), "black_level"),
        (json!({ "min_black_ms": 60_001 }), "min_black_ms"),
        (json!({ "threshold": 0.5 }), "threshold"),
    ] {
        let failure = begin_failure(&params).unwrap_or_else(|| panic!("{params} is refused"));
        assert!(failure.contains(&format!("params.{key}")), "{failure}");
    }
    for params in [
        json!({}),
        json!({ "sensitivity": 0, "min_scene_ms": 0, "black_level": 0.01, "min_black_ms": 0 }),
        json!({ "sensitivity": 1, "min_scene_ms": 60_000, "black_level": 0.5, "min_black_ms": 60_000 }),
    ] {
        assert_eq!(begin_failure(&params), None, "{params}");
    }
}

#[test]
fn a_frame_that_is_not_luma8_of_its_size_fails_the_call() {
    let mut plugin = instance();
    let range = (0, 10 * TICKS_PER_FRAME);
    assert_eq!(
        plugin.begin(&context(range, &[Way::Forward], &json!({}))),
        status::OK
    );
    assert_eq!(plugin.pass(0, 0), status::OK);
    assert_eq!(plugin.video(0, 160, 90, LUMA8, &[0; 100]), status::FAILED);
    assert_eq!(plugin.video(0, 4, 2, 0, &[0; 32]), status::FAILED);
}

#[test]
fn dropped_frames_in_a_fast_pan_make_no_cut() {
    for speed in [2, 3, 4, 6] {
        let mut sequence = Sequence::new(6);
        let mut offset = 0;
        for index in 0..90 {
            offset += if index == 45 { 3 * speed } else { speed };
            sequence.still(&shot(1, offset), 1);
        }
        assert_eq!(
            run(&sequence, &json!({})).cuts,
            [],
            "{speed} pixels a frame"
        );
    }
}
