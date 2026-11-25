#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use alpha2_engine::extract::{
    Direction, ExtractSession, FrameFrom, Need, SuppliedAudio, SuppliedFrame, Supplier, drive,
};
use alpha2_model::{DataSet, DataValues, Project};
use serde_json::{Value, json};

use super::tests::{MANIFEST_SHA256, Packed, packed};
use super::*;

const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
const FRAMES: i64 = 21;
const PICKED: i64 = 10;
const FRAME_TICKS: i64 = 4_000;
const PATCH_SIDE: f64 = 56.0;
const CENTRE_PIXELS: f64 = 0.1;
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

struct Pose {
    centre: (f64, f64),
    degrees: f64,
    scale: f64,
}

fn pose(index: i64) -> Pose {
    let t = index as f64;
    Pose {
        centre: (90.0 + 2.5 * t, 70.0 + t),
        degrees: 0.8 * t,
        scale: 1.0 + 0.01 * t,
    }
}

fn grey_rgba_frame(index: i64) -> Vec<u8> {
    let Pose {
        centre: (cx, cy),
        degrees,
        scale,
    } = pose(index);
    let (sin, cos) = degrees.to_radians().sin_cos();
    let half = 0.5 * PATCH_SIDE;
    let mut rgba = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    for row in 0..HEIGHT {
        for column in 0..WIDTH {
            let (x, y) = (f64::from(column), f64::from(row));
            let (dx, dy) = (x - cx, y - cy);
            let u = (cos * dx + sin * dy) / scale;
            let v = (-sin * dx + cos * dy) / scale;
            let value = if u.abs() <= half && v.abs() <= half {
                texture(2, u + 100.0, v + 100.0)
            } else {
                texture(1, x, y)
            };
            let grey = value.round().clamp(0.0, 255.0) as u8;
            rgba.extend_from_slice(&[grey, grey, grey, 255]);
        }
    }
    rgba
}

struct MovingPatch;

impl Supplier for MovingPatch {
    fn frame(&mut self, need: &Need) -> Result<Option<SuppliedFrame>, String> {
        let index = match need {
            Need::SourceFrame {
                from: FrameFrom::Start(time),
                ..
            } => time.ticks() / FRAME_TICKS,
            Need::SourceFrame {
                from: FrameFrom::After(at),
                direction: Direction::Forward,
                ..
            } => at.ticks() / FRAME_TICKS + 1,
            Need::SourceFrame {
                from: FrameFrom::After(at),
                ..
            } => at.ticks() / FRAME_TICKS - 1,
            _ => return Err("not a source frame".to_owned()),
        };
        Ok((0..FRAMES).contains(&index).then(|| SuppliedFrame {
            pts: MediaTime(index * FRAME_TICKS),
            width: WIDTH,
            height: HEIGHT,
            rgba: grey_rgba_frame(index),
        }))
    }

    fn audio(&mut self, _need: &Need) -> Result<SuppliedAudio, String> {
        Err("no audio".to_owned())
    }
}

fn project() -> Project {
    Project::from_json(
        &json!({
            "schema_version": 1, "id": "p", "name": "P",
            "settings": { "width": 640, "height": 360, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48_000 },
            "sources": [{ "id": "v", "kind": "video", "locator": "v.mp4" }],
            "timeline": { "tracks": [] }
        })
        .to_string(),
    )
    .expect("project")
}

fn region_inside_picked_patch() -> Value {
    let Pose {
        centre: (cx, cy),
        scale,
        ..
    } = pose(PICKED);
    let side = 0.8 * PATCH_SIDE * scale;
    json!({
        "x": (cx + 0.5 - 0.5 * side) / f64::from(WIDTH),
        "y": (cy + 0.5 - 0.5 * side) / f64::from(HEIGHT),
        "width": side / f64::from(WIDTH),
        "height": side / f64::from(HEIGHT),
    })
}

fn track_from_picked_both_ways(policy: &Policy) -> (DataSet, DataSet) {
    let manifest = parse_manifest(
        &std::fs::read_to_string(packed("track").join("manifest.json")).expect("manifest"),
    )
    .expect("manifest");
    let params = json!({ "region": region_inside_picked_patch() });
    let mut plugin = load(
        LoadRequest {
            manifest: &manifest,
            manifest_sha256: MANIFEST_SHA256,
            params: params.as_object().expect("params"),
            supplied: &BTreeMap::new(),
        },
        policy,
        &mut Packed("track"),
        &CancelToken::default(),
    )
    .expect("plugin loads");
    let start = PICKED * FRAME_TICKS;
    let request = serde_json::from_value(json!({
        "extractor": "alpha2.track", "version": 1, "params": params,
        "input": {
            "from": "source", "source_id": "v", "size": WIDTH,
            "range": { "start": 0, "end": FRAMES * FRAME_TICKS },
            "passes": [{ "start": start, "direction": "forward" }, { "start": start, "direction": "backward" }]
        },
        "outputs": { "center": "box-centre", "motion": "box-motion" }
    }))
    .expect("request");
    let session = ExtractSession::new(&project(), &manifest.descriptor, request).expect("session");
    let mut sets = drive(session, &mut plugin, &mut MovingPatch).expect("run");
    let motion = sets.pop().expect("motion");
    let center = sets.pop().expect("center");
    (center, motion)
}

fn times_and_values(set: &DataSet) -> Vec<(i64, Vec<Option<f32>>)> {
    let (DataValues::Points(samples) | DataValues::Similarity(samples)) = &set.values else {
        panic!("sampled data");
    };
    let width = samples.values.len() / samples.times.len();
    samples
        .times
        .iter()
        .zip(samples.values.chunks(width))
        .map(|(time, values)| (time.ticks(), values.to_vec()))
        .collect()
}

#[test]
fn the_track_plugin_follows_the_patch_both_ways_alike_as_wasm_and_native() {
    let [wasm, native] = [
        Policy::default(),
        Policy {
            allow_native: true,
            ..Policy::default()
        },
    ]
    .map(|policy| track_from_picked_both_ways(&policy));
    assert_eq!(
        times_and_values(&wasm.0),
        times_and_values(&native.0),
        "the centres of wasm and native"
    );
    assert_eq!(
        times_and_values(&wasm.1),
        times_and_values(&native.1),
        "the motions of wasm and native"
    );
    let centres = times_and_values(&wasm.0);
    let motions = times_and_values(&wasm.1);
    assert_eq!(
        centres.iter().map(|(time, _)| *time).collect::<Vec<_>>(),
        (0..FRAMES)
            .map(|index| index * FRAME_TICKS)
            .collect::<Vec<_>>()
    );
    let picked = pose(PICKED);
    for ((time, centre), (_, motion)) in centres.iter().zip(&motions) {
        let index = time / FRAME_TICKS;
        let Pose {
            centre: (cx, cy),
            degrees,
            scale,
        } = pose(index);
        let [Some(x), Some(y)] = centre[..] else {
            panic!("frame {index} is lost");
        };
        let found = (
            f64::from(x) * f64::from(WIDTH) - 0.5,
            f64::from(y) * f64::from(HEIGHT) - 0.5,
        );
        assert!(
            (found.0 - cx).hypot(found.1 - cy) <= CENTRE_PIXELS,
            "frame {index}: centre {found:?} against ({cx}, {cy})"
        );
        let [_, _, Some(rotation), Some(found_scale)] = motion[..] else {
            panic!("frame {index}'s motion is lost");
        };
        assert!(
            (f64::from(rotation) - (degrees - picked.degrees)).abs() <= ROTATION_DEGREES,
            "frame {index}: {rotation} degrees"
        );
        assert!(
            (f64::from(found_scale) / (scale / picked.scale) - 1.0).abs() <= SCALE_SHARE,
            "frame {index}: scale {found_scale}"
        );
    }
    assert_eq!(
        motions[PICKED as usize].1,
        vec![Some(0.0), Some(0.0), Some(0.0), Some(1.0)]
    );
    let descriptor = &packed_descriptor();
    assert_eq!(
        descriptor.params_schema["properties"]["region"]["x-alpha2-display"],
        "box"
    );
}

fn packed_descriptor() -> Descriptor {
    parse_manifest(
        &std::fs::read_to_string(packed("track").join("manifest.json")).expect("manifest"),
    )
    .expect("manifest")
    .descriptor
}
