use alpha2_engine::extract::{
    Direction, DriveError, ExtractRequest, ExtractSession, FrameFrom, Need, SuppliedAudio,
    SuppliedFrame, Supplier, drive,
};
use alpha2_eval::edit::{DataRef, EditRequest, Target, Times, apply};
use alpha2_model::{DataSet, DataValues, Project};
use serde_json::{Value, json};

use super::tests::{MANIFEST_SHA256, Packed, packed};
use super::*;

const WIDTH: u32 = 160;
const HEIGHT: u32 = 90;
const TICKS_PER_FRAME: i64 = 4_000;
const CUT_FRAMES: [i64; 3] = [111, 141, 166];
const BLACK_FRAMES: (i64, i64) = (141, 166);

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

fn shot(seed: u64) -> Vec<u8> {
    (0..HEIGHT)
        .flat_map(|y| (0..WIDTH).map(move |x| (x, y)))
        .map(|(x, y)| {
            let mut rng = SplitMix64(
                seed.wrapping_mul(0x1_0000_0001) ^ u64::from(((x / 20) << 8) | (y / 18)),
            );
            40 + u8::try_from(rng.next() % 176).expect("small")
        })
        .collect()
}

fn sequence() -> Vec<Vec<u8>> {
    let (first, second) = (shot(1), shot(2));
    let mut frames = Vec::new();
    frames.extend(std::iter::repeat_n(first.clone(), 40));
    frames.push(vec![255; first.len()]);
    frames.extend(std::iter::repeat_n(first.clone(), 20));
    for step in 1..=20_u32 {
        frames.push(
            first
                .iter()
                .zip(&second)
                .map(|(&a, &b)| {
                    let value = (u32::from(a) * (21 - step) + u32::from(b) * step + 10) / 21;
                    u8::try_from(value).expect("a level")
                })
                .collect(),
        );
    }
    frames.extend(std::iter::repeat_n(second, 30));
    frames.extend(std::iter::repeat_n(shot(3), 30));
    frames.extend(std::iter::repeat_n(vec![0; first.len()], 25));
    frames.extend(std::iter::repeat_n(shot(4), 30));
    let mut rng = SplitMix64(7);
    for frame in &mut frames {
        for value in frame.iter_mut() {
            let jitter = i16::try_from(rng.next() % 13).expect("small") - 6;
            *value = u8::try_from((i16::from(*value) + jitter).clamp(0, 255)).expect("clamped");
        }
    }
    frames
}

struct Frames<'a>(&'a [Vec<u8>]);

impl Frames<'_> {
    fn at(&self, index: i64) -> Option<SuppliedFrame> {
        let frame = self.0.get(usize::try_from(index).ok()?)?;
        Some(SuppliedFrame {
            pts: MediaTime(index * TICKS_PER_FRAME),
            width: WIDTH,
            height: HEIGHT,
            rgba: frame
                .iter()
                .flat_map(|&value| [value, value, value, 255])
                .collect(),
        })
    }
}

impl Supplier for Frames<'_> {
    fn frame(&mut self, need: &Need) -> Result<Option<SuppliedFrame>, String> {
        let index = match need {
            Need::SourceFrame {
                from: FrameFrom::Start(time),
                ..
            }
            | Need::TimelineFrame { time, .. } => time.ticks().div_euclid(TICKS_PER_FRAME),
            Need::SourceFrame {
                from: FrameFrom::After(at),
                direction: Direction::Forward,
                ..
            } => at.ticks() / TICKS_PER_FRAME + 1,
            Need::SourceFrame {
                from: FrameFrom::After(at),
                ..
            } => at.ticks() / TICKS_PER_FRAME - 1,
            _ => return Err("not a frame".to_owned()),
        };
        Ok(self.at(index))
    }

    fn audio(&mut self, _need: &Need) -> Result<SuppliedAudio, String> {
        Err("no audio".to_owned())
    }
}

fn project(duration: i64, data: &[DataSet]) -> Project {
    Project::from_json(
        &json!({
            "schema_version": 1, "id": "p", "name": "P",
            "settings": { "width": 160, "height": 90, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48_000 },
            "sources": [{ "id": "v", "kind": "video", "locator": "v.mp4" }],
            "timeline": { "tracks": [{ "id": "t", "name": "T", "kind": "video", "clips": [{
                "id": "c", "source_id": "v",
                "timeline_range": { "start": 0, "end": duration }, "source_range": { "start": 0, "end": duration } }] }] },
            "data": data
        })
        .to_string(),
    )
    .expect("project")
}

fn packed_manifest() -> PluginManifest {
    parse_manifest(
        &std::fs::read_to_string(packed("scenes").join("manifest.json")).expect("manifest"),
    )
    .expect("manifest")
}

fn extract(
    frames: &[Vec<u8>],
    input: &Value,
    params: &Value,
    policy: &Policy,
) -> Result<Vec<DataSet>, DriveError> {
    let manifest = packed_manifest();
    let mut plugin = load(
        LoadRequest {
            manifest: &manifest,
            manifest_sha256: MANIFEST_SHA256,
            params: &params.as_object().cloned().unwrap_or_default(),
            supplied: &BTreeMap::new(),
        },
        policy,
        &mut Packed("scenes"),
        &CancelToken::default(),
    )
    .expect("plugin loads");
    let request: ExtractRequest = serde_json::from_value(json!({
        "extractor": "alpha2.scenes", "version": 1, "params": params, "input": input,
        "outputs": { "cuts": "cuts", "black": "black" }
    }))
    .expect("request");
    let duration = i64::try_from(frames.len()).expect("small") * TICKS_PER_FRAME;
    let session = ExtractSession::new(&project(duration, &[]), &manifest.descriptor, request)
        .expect("session");
    drive(session, &mut plugin, &mut Frames(frames))
}

fn source_input(frames: &[Vec<u8>]) -> Value {
    let end = i64::try_from(frames.len()).expect("small") * TICKS_PER_FRAME;
    json!({ "from": "source", "source_id": "v", "range": { "start": 0, "end": end } })
}

fn named<'a>(sets: &'a [DataSet], id: &str) -> &'a DataSet {
    sets.iter().find(|set| set.id.as_str() == id).expect(id)
}

fn intervals(set: &DataSet) -> Vec<(i64, i64, f32)> {
    let DataValues::Intervals(intervals) = &set.values else {
        panic!("intervals");
    };
    intervals
        .iter()
        .map(|interval| {
            (
                interval.start.ticks(),
                interval.end.ticks(),
                interval.score.expect("a score"),
            )
        })
        .collect()
}

fn frames_of(found: &[(i64, i64, f32)]) -> Vec<(i64, i64)> {
    found
        .iter()
        .map(|(start, end, _)| (start / TICKS_PER_FRAME, end / TICKS_PER_FRAME))
        .collect()
}

fn wasm_and_native() -> [Policy; 2] {
    [
        Policy::default(),
        Policy {
            allow_native: true,
            ..Policy::default()
        },
    ]
}

#[test]
fn the_scenes_plugin_finds_the_cuts_and_black_and_its_cuts_split_the_clip() {
    let frames = sequence();
    let [wasm, native] = wasm_and_native()
        .map(|policy| extract(&frames, &source_input(&frames), &json!({}), &policy).expect("run"));
    assert_eq!(
        wasm.iter().map(|set| &set.values).collect::<Vec<_>>(),
        native.iter().map(|set| &set.values).collect::<Vec<_>>(),
        "the same integer histograms and float steps give the same values"
    );
    let (cuts, black) = (named(&wasm, "cuts"), named(&wasm, "black"));
    let found_cuts = intervals(cuts);
    assert_eq!(
        frames_of(&found_cuts),
        CUT_FRAMES.map(|cut| (cut, cut)),
        "no cut at the flash or the dissolve"
    );
    assert!(
        found_cuts
            .iter()
            .all(|(.., score)| (0.5..1.0).contains(score))
    );
    let found_black = intervals(black);
    assert_eq!(frames_of(&found_black), [BLACK_FRAMES]);
    assert!(found_black[0].2 > 0.85);

    let duration = i64::try_from(frames.len()).expect("small") * TICKS_PER_FRAME;
    let original = project(duration, std::slice::from_ref(cuts));
    let (edited, report) = apply(
        &original,
        &EditRequest::SplitAt {
            times: Times::Data(DataRef {
                data: cuts.id.clone(),
            }),
            target: Target::Clips(vec![alpha2_model::ClipId::new("c").expect("id")]),
        },
    )
    .expect("the edit applies");
    edited.validate().expect("the edited project is valid");
    assert_eq!(report.created.len(), CUT_FRAMES.len());
    let starts = edited.timeline.tracks[0]
        .clips
        .iter()
        .map(|clip| clip.timeline_range.start().ticks() / TICKS_PER_FRAME)
        .collect::<Vec<_>>();
    assert_eq!(starts, [0, CUT_FRAMES[0], CUT_FRAMES[1], CUT_FRAMES[2]]);
}

#[test]
fn timeline_video_gives_the_cuts_on_the_timeline_clock() {
    let frames = sequence();
    let end = i64::try_from(frames.len()).expect("small") * TICKS_PER_FRAME;
    let sets = extract(
        &frames,
        &json!({ "from": "timeline", "range": { "start": 0, "end": end } }),
        &json!({}),
        &Policy::default(),
    )
    .expect("run");
    assert_eq!(
        frames_of(&intervals(named(&sets, "cuts"))),
        CUT_FRAMES.map(|cut| (cut, cut))
    );
    assert_eq!(frames_of(&intervals(named(&sets, "black"))), [BLACK_FRAMES]);
}

#[test]
fn a_backward_pass_through_the_loader_finds_the_same() {
    let frames = sequence();
    let end = i64::try_from(frames.len()).expect("small") * TICKS_PER_FRAME;
    for policy in wasm_and_native() {
        let sets = extract(
            &frames,
            &json!({ "from": "source", "source_id": "v", "range": { "start": 0, "end": end },
                "passes": [{ "start": end - 1, "direction": "backward" }] }),
            &json!({}),
            &policy,
        )
        .expect("run");
        assert_eq!(
            frames_of(&intervals(named(&sets, "cuts"))),
            CUT_FRAMES.map(|cut| (cut, cut))
        );
        assert_eq!(frames_of(&intervals(named(&sets, "black"))), [BLACK_FRAMES]);
    }
}

#[test]
fn scenes_params_outside_the_schema_fail_the_run_naming_the_key() {
    let frames = &sequence()[..4];
    for (params, key) in [
        (json!({ "sensitivity": "0.7" }), "sensitivity"),
        (json!({ "min_black_ms": -5 }), "min_black_ms"),
        (json!({ "x": 1 }), "x"),
    ] {
        let failed = extract(frames, &source_input(frames), &params, &Policy::default())
            .expect_err("refused");
        let DriveError::Extractor(error) = failed else {
            panic!("an extractor failure, not {failed:?}");
        };
        assert!(
            error.message.contains(&format!("params.{key}")),
            "{}",
            error.message
        );
    }
}

#[test]
fn the_packed_scenes_manifest_names_the_plugin_and_its_outputs_for_interfaces() {
    let manifest = packed_manifest();
    let about = manifest.about.expect("about");
    assert_eq!(about.name, "Scenes");
    let outputs = &manifest.descriptor.outputs;
    assert_eq!(outputs[0].label.as_deref(), Some("Cuts"));
    assert_eq!(outputs[1].label.as_deref(), Some("Black"));
}
