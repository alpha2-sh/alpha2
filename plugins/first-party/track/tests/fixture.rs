//! The plugin over synthetic scenes: a textured patch moving over a textured background.

#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "frame sizes and pixel positions far inside every float and integer type"
)]

use std::collections::{BTreeMap, HashMap};

use alpha2_plugin_abi::records::Record;
use alpha2_plugin_abi::status;
use alpha2_plugin_sdk::testing::Instance;
use serde_json::{Value, json};

const WIDTH: usize = 320;
const HEIGHT: usize = 180;
const FRAME_TICKS: i64 = 4_000;
const PATCH_SIDE: f64 = 56.0;
const CENTRE_PIXELS: f64 = 0.1;
const TRANSLATION_PIXELS: f64 = 0.25;
const PARTLY_HIDDEN_CENTRE_PIXELS: f64 = 3.0;
const ROTATION_DEGREES: f64 = 0.1;
const SCALE_SHARE: f64 = 0.002;

fn hash(seed: u32, x: i32, y: i32) -> f64 {
    let mut h = seed.wrapping_mul(0x9e37_79b1)
        ^ (x as u32).wrapping_mul(0x85eb_ca77)
        ^ (y as u32).wrapping_mul(0xc2b2_ae3d);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297a_2d39);
    h ^= h >> 15;
    f64::from(h >> 8) / f64::from(1_u32 << 24)
}

fn value_noise(seed: u32, x: f64, y: f64, cell: f64) -> f64 {
    let (gx, gy) = (x / cell, y / cell);
    let (x0, y0) = (gx.floor(), gy.floor());
    let smooth = |t: f64| t * t * (3.0 - 2.0 * t);
    let (fx, fy) = (smooth(gx - x0), smooth(gy - y0));
    let (ix, iy) = (x0 as i32, y0 as i32);
    let top = hash(seed, ix, iy) + (hash(seed, ix + 1, iy) - hash(seed, ix, iy)) * fx;
    let bottom =
        hash(seed, ix, iy + 1) + (hash(seed, ix + 1, iy + 1) - hash(seed, ix, iy + 1)) * fx;
    top + (bottom - top) * fy
}

fn texture(seed: u32, x: f64, y: f64) -> f64 {
    let value = 0.6 * value_noise(seed, x, y, 7.0) + 0.4 * value_noise(seed + 101, x, y, 3.0);
    25.0 + 205.0 * value
}

#[derive(Clone, Copy, Debug)]
struct Pose {
    centre: (f64, f64),
    degrees: f64,
    scale: f64,
}

type OccluderLeftTopRightBottom = (f64, f64, f64, f64);

fn render(pose: Pose, occluder: Option<OccluderLeftTopRightBottom>) -> Vec<u8> {
    let (sin, cos) = pose.degrees.to_radians().sin_cos();
    let half = 0.5 * PATCH_SIDE;
    let mut pixels = Vec::with_capacity(WIDTH * HEIGHT);
    for row in 0..HEIGHT {
        for column in 0..WIDTH {
            let (x, y) = (column as f64, row as f64);
            let covered = occluder.is_some_and(|(left, top, right, bottom)| {
                (left..right).contains(&x) && (top..bottom).contains(&y)
            });
            let (dx, dy) = (x - pose.centre.0, y - pose.centre.1);
            let u = (cos * dx + sin * dy) / pose.scale;
            let v = (-sin * dx + cos * dy) / pose.scale;
            let value = if covered {
                texture(3, x, y)
            } else if u.abs() <= half && v.abs() <= half {
                texture(2, u + 100.0, v + 100.0)
            } else {
                texture(1, x, y)
            };
            pixels.push(value.round().clamp(0.0, 255.0) as u8);
        }
    }
    pixels
}

fn region_inside_patch(pose: Pose) -> Value {
    let side = 0.8 * PATCH_SIDE * pose.scale;
    json!({
        "x": (pose.centre.0 + 0.5 - 0.5 * side) / WIDTH as f64,
        "y": (pose.centre.1 + 0.5 - 0.5 * side) / HEIGHT as f64,
        "width": side / WIDTH as f64,
        "height": side / HEIGHT as f64,
    })
}

fn instance() -> Instance {
    // SAFETY: the plugin's own entry returns its static table.
    unsafe { Instance::new(alpha2_plugin_track::alpha2_plugin_v1(), HashMap::new()) }
}

fn context(frames: usize, passes: &[(usize, u32)], params: &Value) -> String {
    let passes = passes
        .iter()
        .map(|(start, direction)| {
            json!({
                "start": *start as i64 * FRAME_TICKS,
                "direction": if *direction == 0 { "forward" } else { "backward" }
            })
        })
        .collect::<Vec<_>>();
    json!({
        "range": { "start": 0, "end": frames as i64 * FRAME_TICKS }, "passes": passes,
        "from": "source", "video": { "size": WIDTH, "format": "luma8" }, "audio": null,
        "frame_rate": null, "params": params
    })
    .to_string()
}

type Sample = (Option<[f64; 2]>, Option<[f64; 4]>);

fn values(values: &[Option<f32>]) -> Option<Vec<f64>> {
    let first = values[0];
    assert!(
        values
            .iter()
            .all(|value| value.is_some() == first.is_some()),
        "a sample is lost whole or not at all: {values:?}"
    );
    first?;
    Some(
        values
            .iter()
            .map(|value| f64::from(value.expect("kept")))
            .collect(),
    )
}

fn track(frames: &[Vec<u8>], passes: &[(usize, u32)], params: &Value) -> BTreeMap<usize, Sample> {
    let mut plugin = instance();
    assert_eq!(
        plugin.begin(&context(frames.len(), passes, params)),
        status::OK,
        "{:?}",
        plugin.take().failure
    );
    let mut found = BTreeMap::new();
    for (pass, (start, direction)) in passes.iter().enumerate() {
        assert_eq!(plugin.pass(pass as u32, *direction), status::OK);
        let order: Vec<usize> = if *direction == 0 {
            (*start..frames.len()).collect()
        } else {
            (0..=*start).rev().collect()
        };
        for index in order {
            let time = index as i64 * FRAME_TICKS;
            let status = plugin.video(time, WIDTH as u32, HEIGHT as u32, 1, &frames[index]);
            assert_eq!(status, status::OK, "{:?}", plugin.take().failure);
            let mut center = None;
            let mut motion = None;
            for record in plugin.take().records {
                let Record::Sample {
                    output,
                    time: at,
                    values: sample,
                } = record
                else {
                    panic!("samples only");
                };
                assert_eq!(at, time, "the sample is the delivered frame's");
                match output {
                    0 => center = Some(values(&sample).map(|v| [v[0], v[1]])),
                    1 => motion = Some(values(&sample).map(|v| [v[0], v[1], v[2], v[3]])),
                    other => panic!("output {other}"),
                }
            }
            match (center, motion) {
                (Some(center), Some(motion)) => {
                    assert_eq!(center.is_some(), motion.is_some(), "frame {index}");
                    assert!(
                        found.insert(index, (center, motion)).is_none(),
                        "frame {index} emitted twice"
                    );
                }
                (None, None) => assert!(found.contains_key(&index), "frame {index} is emitted"),
                _ => panic!("frame {index} has one output"),
            }
        }
    }
    assert_eq!(plugin.end(), status::OK);
    found
}

fn assert_tracks(index: usize, sample: &Sample, picked: Pose, pose: Pose) {
    let (Some(center), Some(motion)) = sample else {
        panic!("frame {index} is lost");
    };
    let (width, height) = (WIDTH as f64, HEIGHT as f64);
    let (cx, cy) = (center[0] * width - 0.5, center[1] * height - 0.5);
    let centre_error = (cx - pose.centre.0).hypot(cy - pose.centre.1);
    assert!(
        centre_error <= CENTRE_PIXELS,
        "frame {index}: centre ({cx:.3}, {cy:.3}) against {:?}, {centre_error:.3} px off",
        pose.centre
    );
    let degrees = pose.degrees - picked.degrees;
    let scale = pose.scale / picked.scale;
    let (sin, cos) = degrees.to_radians().sin_cos();
    let picture = (0.5 * width - 0.5, 0.5 * height - 0.5);
    let (dx, dy) = (picture.0 - picked.centre.0, picture.1 - picked.centre.1);
    let moved = (
        pose.centre.0 + scale * (cos * dx - sin * dy),
        pose.centre.1 + scale * (sin * dx + cos * dy),
    );
    let (tx, ty) = (moved.0 - picture.0, moved.1 - picture.1);
    let translation_error = (motion[0] * width - tx).hypot(motion[1] * height - ty);
    assert!(
        translation_error <= TRANSLATION_PIXELS,
        "frame {index}: translation {:?} against ({tx:.3}, {ty:.3}), {translation_error:.3} px off",
        (motion[0] * width, motion[1] * height)
    );
    assert!(
        (motion[2] - degrees).abs() <= ROTATION_DEGREES,
        "frame {index}: {} degrees against {degrees}",
        motion[2]
    );
    assert!(
        (motion[3] / scale - 1.0).abs() <= SCALE_SHARE,
        "frame {index}: scale {} against {scale}",
        motion[3]
    );
}

fn moving(index: usize) -> Pose {
    let t = index as f64;
    Pose {
        centre: (90.0 + 2.5 * t, 70.0 + 1.0 * t),
        degrees: 0.8 * t,
        scale: 1.0 + 0.01 * t,
    }
}

#[test]
fn a_patch_translating_rotating_and_scaling_is_tracked_from_the_first_frame() {
    let poses = (0..30).map(moving).collect::<Vec<_>>();
    let frames = poses
        .iter()
        .map(|pose| render(*pose, None))
        .collect::<Vec<_>>();
    let found = track(
        &frames,
        &[(0, 0)],
        &json!({ "region": region_inside_patch(poses[0]) }),
    );
    assert_eq!(found.len(), frames.len());
    for (index, sample) in &found {
        assert_tracks(*index, sample, poses[0], poses[*index]);
    }
}

#[test]
fn passes_forward_and_backward_from_a_middle_frame_track_both_ways_from_it() {
    let poses = (0..31).map(moving).collect::<Vec<_>>();
    let frames = poses
        .iter()
        .map(|pose| render(*pose, None))
        .collect::<Vec<_>>();
    let picked = poses[15];
    let found = track(
        &frames,
        &[(15, 0), (15, 1)],
        &json!({ "region": region_inside_patch(picked), "confidence": 0.6 }),
    );
    assert_eq!(
        found.keys().copied().collect::<Vec<_>>(),
        (0..31).collect::<Vec<_>>()
    );
    let (_, motion) = &found[&15];
    assert_eq!(
        *motion,
        Some([0.0, 0.0, 0.0, 1.0]),
        "the picked frame is the identity"
    );
    for (index, sample) in &found {
        assert_tracks(*index, sample, picked, poses[*index]);
    }
}

#[test]
fn a_patch_hidden_behind_an_occluder_is_marked_lost_then_found_again() {
    let pose = |index: usize| Pose {
        centre: (50.0 + 3.0 * index as f64, 90.0),
        degrees: 0.0,
        scale: 1.0,
    };
    let occluder = (120.0, 40.0, 190.0, 140.0);
    let frames = (0..70)
        .map(|index| render(pose(index), Some(occluder)))
        .collect::<Vec<_>>();
    let found = track(
        &frames,
        &[(0, 0)],
        &json!({ "region": region_inside_patch(pose(0)) }),
    );
    let half = 0.5 * PATCH_SIDE;
    let visible = |index: usize| {
        let centre = pose(index).centre.0;
        let (left, right) = (centre - half, centre + half);
        let hidden = (right.min(occluder.2) - left.max(occluder.0)).max(0.0);
        1.0 - hidden / PATCH_SIDE
    };
    let lost = found
        .iter()
        .filter(|(_, (center, _))| center.is_none())
        .map(|(index, _)| *index)
        .collect::<Vec<_>>();
    for index in 0..70 {
        let share = visible(index);
        if share <= 0.0 {
            assert!(
                lost.contains(&index),
                "frame {index} is hidden but not lost"
            );
        }
        let before = pose(index).centre.0 < occluder.0;
        let entered = before && share >= 1.0;
        let emerged = !before && (0..=2).all(|back| visible(index - back) >= 1.0);
        if entered || emerged {
            assert_tracks(index, &found[&index], pose(0), pose(index));
        } else if let (Some(center), _) = found[&index] {
            let (x, y) = (
                center[0] * WIDTH as f64 - 0.5,
                center[1] * HEIGHT as f64 - 0.5,
            );
            let off = (x - pose(index).centre.0).hypot(y - pose(index).centre.1);
            assert!(
                off <= PARTLY_HIDDEN_CENTRE_PIXELS,
                "frame {index}, {share:.2} visible, is found {off:.3} px off"
            );
        }
    }
    assert!(
        (0..70)
            .filter(|index| visible(*index) >= 1.0 && pose(*index).centre.0 > occluder.2)
            .count()
            >= 5,
        "the sequence shows the patch again for a while"
    );
}

#[test]
fn a_static_scene_gives_the_identity_on_every_frame() {
    let still = Pose {
        centre: (160.0, 90.0),
        degrees: 0.0,
        scale: 1.0,
    };
    let frame = render(still, None);
    let frames = vec![frame; 12];
    let picked = region_inside_patch(still);
    let found = track(&frames, &[(0, 0)], &json!({ "region": picked }));
    let expected_centre = [
        picked["x"].as_f64().expect("x") + 0.5 * picked["width"].as_f64().expect("width"),
        picked["y"].as_f64().expect("y") + 0.5 * picked["height"].as_f64().expect("height"),
    ];
    for (index, (center, motion)) in &found {
        assert_eq!(*motion, Some([0.0, 0.0, 0.0, 1.0]), "frame {index}");
        let center = center.expect("found");
        for (found, expected) in center.iter().zip(expected_centre) {
            assert!((found - expected).abs() < 1e-6, "frame {index}: {center:?}");
        }
    }
}

#[test]
fn the_same_frames_give_the_same_samples() {
    let poses = (0..12).map(moving).collect::<Vec<_>>();
    let frames = poses
        .iter()
        .map(|pose| render(*pose, None))
        .collect::<Vec<_>>();
    let params = json!({ "region": region_inside_patch(poses[6]) });
    let first = track(&frames, &[(6, 0), (6, 1)], &params);
    assert_eq!(first, track(&frames, &[(6, 0), (6, 1)], &params));
}

#[test]
fn params_outside_the_schema_fail_begin_naming_the_param() {
    let fits = json!({ "x": 0.25, "y": 0.25, "width": 0.5, "height": 0.5 });
    let with = |key: &str, value: Value| {
        let mut region = fits.clone();
        region[key] = value;
        json!({ "region": region })
    };
    for (params, named) in [
        (json!({}), "params.region"),
        (json!({ "region": [0.1, 0.1, 0.5, 0.5] }), "params.region"),
        (json!({ "region": "0.1 0.1 0.5 0.5" }), "params.region"),
        (with("x", json!("0.25")), "params.region.x"),
        (with("x", json!(-0.1)), "params.region.x"),
        (with("width", json!(0.01)), "params.region.width"),
        (with("height", json!(1.5)), "params.region.height"),
        (
            with("x", json!(0.75)),
            "params.region must lie inside the frame",
        ),
        (
            with("y", json!(0.6)),
            "params.region must lie inside the frame",
        ),
        (with("depth", json!(1)), "params.region.depth"),
        (
            json!({ "region": { "x": 0.1, "y": 0.1, "width": 0.5 } }),
            "params.region.height",
        ),
        (
            json!({ "region": fits, "confidence": 1.5 }),
            "params.confidence",
        ),
        (
            json!({ "region": fits, "confidence": "high" }),
            "params.confidence",
        ),
        (json!({ "region": fits, "speed": 1 }), "params.speed"),
    ] {
        let mut plugin = instance();
        assert_eq!(
            plugin.begin(&context(2, &[(0, 0)], &params)),
            status::FAILED,
            "{params}"
        );
        let failure = plugin.take().failure.expect("a reason");
        assert!(failure.contains(named), "{params}: {failure}");
    }
    for params in [
        json!({ "region": fits }),
        json!({ "region": { "x": 0.3, "y": 0.0, "width": 0.7, "height": 1.0 }, "confidence": 0 }),
        json!({ "region": { "x": 0.0, "y": 0.96, "width": 0.04, "height": 0.04 } }),
    ] {
        let mut plugin = instance();
        assert_eq!(
            plugin.begin(&context(2, &[(0, 0)], &params)),
            status::OK,
            "{params}"
        );
    }
}

#[test]
fn an_rgba_frame_fails_the_call() {
    let mut plugin = instance();
    let params = json!({ "region": { "x": 0.25, "y": 0.25, "width": 0.5, "height": 0.5 } });
    assert_eq!(plugin.begin(&context(1, &[(0, 0)], &params)), status::OK);
    assert_eq!(plugin.pass(0, 0), status::OK);
    let rgba = vec![0_u8; WIDTH * HEIGHT * 4];
    assert_eq!(
        plugin.video(0, WIDTH as u32, HEIGHT as u32, 0, &rgba),
        status::FAILED
    );
}

#[test]
fn frames_of_a_few_pixels_are_tracked_or_lost_without_failing() {
    let params = json!({ "region": { "x": 0.0, "y": 0.0, "width": 1.0, "height": 1.0 } });
    for (width, height) in [(1_u32, 1_u32), (2, 3), (7, 5), (40, 2)] {
        let mut plugin = instance();
        assert_eq!(plugin.begin(&context(3, &[(0, 0)], &params)), status::OK);
        assert_eq!(plugin.pass(0, 0), status::OK);
        for index in 0..3_u8 {
            let pixels = (0..width * height)
                .map(|at| (at as u8).wrapping_mul(37).wrapping_add(index))
                .collect::<Vec<_>>();
            let time = i64::from(index) * FRAME_TICKS;
            assert_eq!(
                plugin.video(time, width, height, 1, &pixels),
                status::OK,
                "{width}x{height}: {:?}",
                plugin.take().failure
            );
        }
        assert_eq!(plugin.take().records.len(), 6, "{width}x{height}");
    }
}
