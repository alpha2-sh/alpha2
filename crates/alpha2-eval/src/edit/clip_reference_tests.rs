use super::tests::{FRAME, clip, corpus, kept_moments_hold, load, remove, timeline_list};
use super::*;
use crate::{ActiveIndex, evaluate};
use alpha2_model::Project;
use alpha2_time::MediaTime;
use serde_json::{Value, json};

fn forward() -> Value {
    json!({ "type": "forward" })
}

fn track_target(id: &str) -> Target {
    Target::Track(alpha2_model::TrackId::new(id).expect("id"))
}

fn timeline_ranges(frames: &[(i64, i64)]) -> Value {
    json!({ "list": frames.iter().map(|(start, end)| json!({ "start": start * FRAME, "end": end * FRAME })).collect::<Vec<_>>(),
            "anchor": { "type": "timeline" } })
}

fn split_frames(frames: &[i64], clips: &[&str]) -> Value {
    json!({ "op": "split_at",
            "times": { "list": frames.iter().map(|frame| frame * FRAME).collect::<Vec<_>>(), "anchor": { "type": "timeline" } },
            "target": { "clips": clips } })
}

fn project_of(tracks: &Value, data: &Value) -> Project {
    load(&json!({
        "schema_version": 1, "id": "p", "name": "P",
        "settings": { "width": 640, "height": 360, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48_000 },
        "sources": [
            { "id": "v", "kind": "video", "locator": "v.mp4" },
            { "id": "w", "kind": "video", "locator": "w.mp4" }
        ],
        "timeline": { "tracks": tracks },
        "data": data
    }))
}

fn edited(project: &Project, request: &Value) -> Project {
    let edited = apply_json(
        &serde_json::to_string(project).expect("json"),
        &request.to_string(),
    )
    .expect("the edit applies");
    Project::from_json(&edited.project).expect("valid result")
}

fn refusal(project: &Project, request: &Value) -> (&'static str, &'static str) {
    let failure = apply_json(
        &serde_json::to_string(project).expect("json"),
        &request.to_string(),
    )
    .expect_err("refused");
    (failure.code, failure.refusal)
}

/// A points set on source `v`: one sample per source frame, x growing with the frame.
fn face() -> Value {
    let times = (0..100).map(|frame| frame * FRAME).collect::<Vec<i64>>();
    let values = (0..100)
        .flat_map(|frame| [0.1 + f64::from(frame) * 0.005, 0.5])
        .collect::<Vec<f64>>();
    json!({ "id": "face", "anchor": { "type": "source", "source_id": "v" }, "kind": "points",
            "interpolation": "hold", "channels": ["nose"], "times": times, "values": values })
}

/// The edit corpus plus a matte pair, or plus a canvas-space follower of `fwd`.
fn corpus_with(matte: bool, follower: bool) -> Value {
    let mut document = corpus();
    let tracks = document["timeline"]["tracks"]
        .as_array_mut()
        .expect("tracks");
    if matte {
        let source = clip("ms", "v", (0, 60), (0, 60), &forward());
        let mut matted = clip("mm", "w", (0, 60), (5, 65), &forward());
        matted["matte"] = json!({ "source": "ms", "mode": "luma" });
        tracks.push(json!({ "id": "t-matte", "name": "MS", "kind": "video", "clips": [source] }));
        tracks.push(json!({ "id": "t-matted", "name": "MM", "kind": "video", "clips": [matted] }));
    }
    if follower {
        let mut label = clip("label", "w", (0, 60), (0, 60), &forward());
        label["visual"] = json!({ "position": { "mode": "data", "data": "face", "channel": "nose",
            "via": { "clip": "fwd" }, "space": "canvas" } });
        tracks.push(json!({ "id": "t-label", "name": "LB", "kind": "video", "clips": [label] }));
        document["data"].as_array_mut().expect("data").push(face());
    }
    document
}

fn kept_moments_under_timeline_ripples(document: &Value) {
    let project = load(document);
    for ranges in [
        vec![(5, 9)],
        vec![(13, 17), (31, 36), (61, 64)],
        vec![(0, 3), (45, 52)],
    ] {
        let (edited, report) = remove(
            &project,
            timeline_list(&ranges),
            track_target("t-fwd"),
            Ripple::Timeline,
        );
        kept_moments_hold(&project, &edited, &report, true);
    }
}

#[test]
fn kept_moments_with_a_matte_pair_timeline_ripple() {
    kept_moments_under_timeline_ripples(&corpus_with(true, false));
}

#[test]
fn kept_moments_with_a_canvas_follower_timeline_ripple() {
    kept_moments_under_timeline_ripples(&corpus_with(false, true));
}

/// `ms` over `source` frames is the luma matte of `mm` over `matted` frames.
fn matte_pair(source: (i64, i64), matted: (i64, i64)) -> Project {
    let ms = clip("ms", "v", source, (0, source.1 - source.0), &forward());
    let mut mm = clip("mm", "w", matted, (0, matted.1 - matted.0), &forward());
    mm["matte"] = json!({ "source": "ms", "mode": "luma" });
    project_of(
        &json!([
            { "id": "t-matte", "name": "MS", "kind": "video", "clips": [ms] },
            { "id": "t-matted", "name": "MM", "kind": "video", "clips": [mm] }
        ]),
        &json!([]),
    )
}

/// Per visual clip at `frame`: its id, whether it is drawn as a layer, and its matte's state.
fn layers(project: &Project, frame: i64) -> Vec<String> {
    let index = ActiveIndex::build(project).expect("index");
    let desc = evaluate(project, &index, MediaTime(frame * FRAME)).expect("frame");
    desc.visual
        .iter()
        .map(|clip| {
            let matte = clip.matte.map_or_else(String::new, |matte| {
                let active = desc
                    .visual
                    .iter()
                    .any(|other| other.clip_id == &matte.source);
                format!(" matte {} active={active}", matte.source)
            });
            format!("{} layer={}{matte}", clip.clip_id, !clip.is_matte)
        })
        .collect()
}

#[test]
fn a_ripple_through_a_matte_pair_keeps_each_part_matted_by_its_counterpart() {
    let project = matte_pair((0, 60), (0, 60));
    let request = json!({ "op": "remove_ranges", "ranges": timeline_ranges(&[(10, 20)]),
                          "target": { "track": "t-matted" } });
    let edited = edited(&project, &request);
    assert_eq!(
        layers(&edited, 5),
        ["ms layer=false", "mm layer=true matte ms active=true"]
    );
    assert_eq!(
        layers(&edited, 30),
        ["ms.1 layer=false", "mm.1 layer=true matte ms.1 active=true"]
    );
}

#[test]
fn splitting_a_matte_source_alone_is_refused() {
    let project = matte_pair((0, 60), (0, 60));
    assert_eq!(
        refusal(&project, &split_frames(&[20], &["ms"])),
        ("INVALID_INPUT", "matte_source")
    );
}

#[test]
fn removing_from_a_matte_source_alone_without_ripple_is_refused() {
    let project = matte_pair((0, 60), (0, 60));
    let request = json!({ "op": "remove_ranges", "ranges": timeline_ranges(&[(10, 20)]),
                          "target": { "clips": ["ms"] }, "ripple": "none" });
    assert_eq!(
        refusal(&project, &request),
        ("INVALID_INPUT", "matte_source")
    );
}

#[test]
fn removing_a_whole_matte_source_from_under_its_clip_is_refused() {
    let project = matte_pair((10, 20), (0, 60));
    let request = json!({ "op": "remove_ranges", "ranges": timeline_ranges(&[(10, 20)]),
                          "target": { "track": "t-matted" } });
    assert_eq!(
        refusal(&project, &request),
        ("INVALID_INPUT", "matte_source")
    );
}

#[test]
fn cutting_a_matte_source_where_it_overhangs_its_clip_is_refused() {
    let project = matte_pair((0, 80), (0, 60));
    assert_eq!(
        refusal(&project, &split_frames(&[70], &["ms"])),
        ("INVALID_INPUT", "matte_source")
    );
}

/// `a` shows source `v`, which `face` is on; `b` follows the nose through `a` in canvas space.
fn follower(a_map: &Value, a_frames: (i64, i64), b_frames: (i64, i64)) -> Project {
    let a = clip(
        "a",
        "v",
        a_frames,
        (10, 10 + a_frames.1 - a_frames.0),
        a_map,
    );
    let mut b = clip("b", "w", b_frames, (0, b_frames.1 - b_frames.0), &forward());
    b["visual"] = json!({ "position": { "mode": "data", "data": "face", "channel": "nose",
        "via": { "clip": "a" }, "space": "canvas" } });
    project_of(
        &json!([
            { "id": "t-a", "name": "A", "kind": "video", "clips": [a] },
            { "id": "t-b", "name": "B", "kind": "video", "clips": [b] }
        ]),
        &json!([face()]),
    )
}

/// Per visual clip at `frame`: the source frame it shows and its position's x.
fn positions(project: &Project, frame: i64) -> Vec<(i64, f32)> {
    let index = ActiveIndex::build(project).expect("index");
    let desc = evaluate(project, &index, MediaTime(frame * FRAME)).expect("frame");
    desc.visual
        .iter()
        .map(|clip| {
            (
                clip.source_time.ticks() / FRAME,
                clip.visual.expect("visual").position.x,
            )
        })
        .collect()
}

#[test]
fn a_follower_reads_the_part_it_follows_after_a_ripple() {
    let project = follower(&forward(), (0, 60), (0, 60));
    let request = json!({ "op": "remove_ranges", "ranges": timeline_ranges(&[(10, 20)]),
                          "target": { "track": "t-a" } });
    let edited = edited(&project, &request);
    let via = |clip: &alpha2_model::Clip| clip.visual.position.via_clip().cloned();
    let followers = &edited.timeline.tracks[1].clips;
    assert_eq!(via(&followers[0]), Some(ClipId::new("a").expect("id")));
    assert_eq!(via(&followers[1]), Some(ClipId::new("a.1").expect("id")));
    for frame in (0..10).chain(20..60) {
        let at = if frame < 10 { frame } else { frame - 10 };
        assert_eq!(
            positions(&project, frame),
            positions(&edited, at),
            "{frame}"
        );
    }
}

#[test]
fn removing_the_clip_a_follower_reads_through_is_refused() {
    let project = follower(&forward(), (10, 20), (0, 60));
    let request = json!({ "op": "remove_ranges", "ranges": timeline_ranges(&[(10, 20)]),
                          "target": { "track": "t-a" } });
    assert_eq!(
        refusal(&project, &request),
        ("INVALID_INPUT", "read_through")
    );
}

#[test]
fn splitting_a_reversed_reader_alone_is_refused() {
    let project = follower(&json!({ "type": "reverse" }), (0, 50), (0, 50));
    assert_eq!(
        refusal(&project, &split_frames(&[20], &["a"])),
        ("INVALID_INPUT", "read_through")
    );
}

#[test]
fn a_track_gain_read_through_a_clip_the_ripple_cuts_is_refused() {
    let mut document = corpus();
    document["data"].as_array_mut().expect("data").push(json!({
        "id": "level", "anchor": { "type": "source", "source_id": "a" }, "kind": "scalars",
        "interpolation": "hold", "channels": ["l"],
        "times": (0..100).map(|frame| frame * FRAME).collect::<Vec<i64>>(),
        "values": (0..100).map(|frame| f64::from(frame) * 0.01).collect::<Vec<f64>>() }));
    document["timeline"]["tracks"][7]["audio"] = json!({ "gain": { "mode": "data",
        "data": "level", "channel": "l", "via": { "clip": "au" } } });
    let project = load(&document);
    let request = json!({ "op": "remove_ranges", "ranges": timeline_ranges(&[(10, 20)]),
                          "target": { "track": "t-audio" } });
    assert_eq!(
        refusal(&project, &request),
        ("INVALID_INPUT", "read_through")
    );
}
