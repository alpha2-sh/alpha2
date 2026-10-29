use serde_json::{Value, json};

use crate::{Facts, compile_json, requirements_json};

const SECOND: i64 = 120_000;

fn facts() -> Value {
    let mut media = serde_json::Map::new();
    for index in 1..=12 {
        media.insert(
            format!("slides/{index:02}.jpg"),
            json!({"width": 2400, "height": 1600}),
        );
    }
    media.insert(
        "music.m4a".into(),
        json!({"duration_ticks": 70 * SECOND, "has_audio": true}),
    );
    media.insert(
        "talk.mp4".into(),
        json!({"width": 1080, "height": 1920, "frame_rate": {"num": 30, "den": 1}, "duration_ticks": 60 * SECOND, "has_audio": true}),
    );
    media.insert("logo.png".into(), json!({"width": 512, "height": 512}));
    for index in 1..=4 {
        media.insert(
            format!("grid{index}.mp4"),
            json!({"width": 1920, "height": 1080, "frame_rate": {"num": 30, "den": 1}, "duration_ticks": (30 + index) * SECOND}),
        );
    }
    media.insert(
        "wide.mp4".into(),
        json!({"width": 1920, "height": 1080, "frame_rate": {"num": 30, "den": 1}, "duration_ticks": 10 * SECOND, "has_audio": true}),
    );
    let srt = (0..30)
        .map(|index| {
            format!(
                "{}\n00:00:{:02},000 --> 00:00:{:02},500\nLine {index}\n",
                index + 1,
                index * 2,
                index * 2 + 1
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    json!({"media": media, "texts": {"talk.srt": srt}})
}

fn compile(spec: &Value) -> Value {
    let text = compile_json(&spec.to_string(), &facts().to_string(), &[])
        .unwrap_or_else(|error| panic!("{error}"));
    serde_json::from_str(&text).expect("JSON")
}

fn compile_error(spec: &Value) -> String {
    compile_json(&spec.to_string(), &facts().to_string(), &[])
        .expect_err("an error")
        .to_string()
}

fn slideshow() -> Value {
    json!({
        "canvas": "youtube-1080p",
        "body": {
            "type": "slideshow",
            "items": (1..=12).map(|index| format!("slides/{index:02}.jpg")).collect::<Vec<_>>(),
            "seconds": 6,
            "transition": {"type": "crossfade", "duration": 1},
            "ken_burns": 1.15,
        },
        "overlays": [{"type": "title", "text": "Summer 2026", "at": 0.5, "duration": 4, "fade": 0.5}],
        "audio": [{"type": "music_bed", "path": "music.m4a", "fade_in": 2, "fade_out": 3}],
    })
}

fn tracks(project: &Value) -> &Vec<Value> {
    project["project"]["timeline"]["tracks"]
        .as_array()
        .expect("tracks")
}

#[test]
fn a_slideshow_overlaps_its_slides_by_the_transition() {
    let compiled = compile(&slideshow());
    let tracks = tracks(&compiled);
    let photos = &tracks[0];
    let clips = photos["clips"].as_array().expect("clips");
    assert_eq!(clips.len(), 12);
    assert_eq!(photos["transitions"].as_array().map(Vec::len), Some(11));
    assert_eq!(
        clips[1]["timeline_range"],
        json!({"start": 5 * SECOND, "end": 11 * SECOND})
    );
    assert_eq!(
        photos["transitions"][0]["effect"]["id"],
        "alpha2.transition.fade"
    );
    // 3:2 photos cover 16:9: the contain-fit size at scale 1 grows by 16/9 / (3/2).
    let scale = &clips[0]["visual"]["scale"]["keyframes"];
    let cover = 1920.0 / (1080.0 * 1.5);
    assert!((scale[0]["value"]["x"].as_f64().unwrap() - cover).abs() < 1e-5);
    assert!((scale[1]["value"]["x"].as_f64().unwrap() - cover * 1.15).abs() < 1e-5);
    // The music lasts as long as the slides: 12 * 5 + 1 seconds.
    let music = tracks.last().expect("music");
    assert_eq!(music["kind"], "audio");
    assert_eq!(music["clips"][0]["timeline_range"]["end"], 61 * SECOND);
    assert_eq!(music["clips"][0]["audio"]["fade_out"], 3 * SECOND);
    assert_eq!(compiled["assets"]["s1"], "slides/01.jpg");
}

#[test]
fn a_title_fades_in_and_out_over_its_time() {
    let compiled = compile(&slideshow());
    let title = &tracks(&compiled)[1];
    let clip = &title["clips"][0];
    assert_eq!(
        clip["timeline_range"],
        json!({"start": SECOND / 2, "end": 9 * SECOND / 2})
    );
    let opacity = clip["visual"]["opacity"]["keyframes"]
        .as_array()
        .expect("keyframes");
    let values = opacity
        .iter()
        .map(|keyframe| keyframe["value"].as_f64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(values, [0.0, 1.0, 1.0, 0.0]);
}

#[test]
fn compiling_twice_gives_the_same_bytes() {
    let spec = slideshow().to_string();
    let facts = facts().to_string();
    let first = compile_json(&spec, &facts, &[]).expect("compiles");
    assert_eq!(first, compile_json(&spec, &facts, &[]).expect("compiles"));
}

#[test]
fn captions_on_a_bar_with_a_logo_and_loudness() {
    let compiled = compile(&json!({
        "canvas": "shorts",
        "body": {"type": "media", "path": "talk.mp4"},
        "overlays": [
            {"type": "captions", "file": "talk.srt", "style": "bar-bottom"},
            {"type": "logo", "path": "logo.png", "corner": "top-right", "size": 180, "opacity": 0.85},
        ],
        "loudness": -14,
    }));
    let tracks = tracks(&compiled);
    let kinds = tracks
        .iter()
        .map(|track| track["kind"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["video", "video", "caption", "video"]);
    assert_eq!(tracks[0]["clips"][0]["timeline_range"]["end"], 60 * SECOND);
    assert_eq!(compiled["project"]["sources"][0]["has_audio"], true);
    // The bar spans the canvas across, behind the cues.
    let bar = &tracks[1]["clips"][0]["visual"];
    assert_eq!(bar["scale"]["value"]["x"], 1.0);
    assert_eq!(
        tracks[2]["captions"]["cues"].as_array().map(Vec::len),
        Some(30)
    );
    // The logo is 180 px at the top-right corner, 40 px (the theme's margin) in.
    let logo = &tracks[3]["clips"][0]["visual"];
    let scale = logo["scale"]["value"]["x"].as_f64().unwrap();
    assert!((scale * 1080.0 - 180.0).abs() < 1e-3, "{scale}");
    // Its top-right corner, in the frame it is placed in: the square fills the frame's width and
    // 1080/1920 of its height, centered.
    assert_eq!(logo["anchor"], json!({"x": 1.0, "y": 0.21875}));
    assert_eq!(
        logo["opacity"]["value"]
            .as_f64()
            .map(|value| (value * 100.0).round()),
        Some(85.0)
    );
    assert_eq!(
        compiled["loudness"],
        json!({"target_lufs": -14.0, "true_peak_ceiling_dbtp": -1.0})
    );
}

#[test]
fn a_comparison_grid_lasts_as_its_shortest_clip_and_labels_each_cell() {
    let labels = ["Ground truth", "Model A", "Model B", "Model C"];
    let compiled = compile(&json!({
        "body": {
            "type": "comparison_grid",
            "items": labels.iter().enumerate().map(|(index, label)| json!({"type": "media", "path": format!("grid{}.mp4", index + 1), "label": label})).collect::<Vec<_>>(),
        },
    }));
    let tracks = tracks(&compiled);
    assert_eq!(tracks.len(), 8);
    let video = &tracks[2]["clips"][0];
    assert_eq!(video["timeline_range"]["end"], 31 * SECOND);
    assert_eq!(
        video["visual"]["position"]["value"],
        json!({"x": 0.75, "y": 0.25})
    );
    assert_eq!(
        video["visual"]["scale"]["value"],
        json!({"x": 0.5, "y": 0.5})
    );
    let label = &compiled["project"]["sources"][3]["generator"];
    assert_eq!(label["text"], "Model A");
    assert_eq!(label["style"]["anchor"], json!({"x": 0.0, "y": 0.0}));
}

#[test]
fn a_longest_grid_freezes_its_shorter_clips() {
    let compiled = compile(&json!({
        "body": {
            "type": "grid",
            "length": "longest_freeze",
            "children": [{"type": "media", "path": "grid1.mp4"}, {"type": "media", "path": "grid4.mp4"}],
        },
    }));
    let first = &tracks(&compiled)[0]["clips"];
    assert_eq!(first[1]["time_map"]["type"], "freeze");
    assert_eq!(
        first[1]["timeline_range"],
        json!({"start": 31 * SECOND, "end": 34 * SECOND})
    );
}

#[test]
fn cover_in_a_cell_is_masked_to_it() {
    let compiled = compile(&json!({
        "body": {"type": "grid", "columns": 2, "children": [
            {"type": "media", "path": "logo.png", "fit": "cover", "duration": 2},
            {"type": "media", "path": "logo.png", "duration": 2},
        ]},
    }));
    let tracks = tracks(&compiled);
    let masks = tracks[0]["clips"][0]["masks"].as_array().expect("masks");
    assert_eq!(masks.len(), 1);
    // A square in a 960x1080 cell: 960/1080 of its width shows, and the square is 1080/1920 of
    // the canvas-sized frame it is placed in.
    let size = &masks[0]["shape"]["size"]["value"];
    assert!((size["x"].as_f64().unwrap() - 0.5).abs() < 1e-5, "{size}");
    assert!((size["y"].as_f64().unwrap() - 1.0).abs() < 1e-5, "{size}");
    assert!(
        tracks[1]["clips"][0]["masks"].is_null()
            || tracks[1]["clips"][0]["masks"]
                .as_array()
                .is_some_and(Vec::is_empty)
    );
}

#[test]
fn reframe_blurs_a_muted_cover_copy_under_the_video() {
    let compiled = compile(&json!({
        "canvas": "shorts",
        "body": {"type": "reframe", "video": "wide.mp4"},
    }));
    let tracks = tracks(&compiled);
    let back = &tracks[0]["clips"][0];
    assert_eq!(back["audio"]["muted"], true);
    assert_eq!(back["composition"]["id"], "blur1");
    assert_eq!(tracks[1]["clips"][0]["audio"]["muted"], false);
    let effect = &compiled["project"]["compositions"][0]["nodes"][1]["op"]["effect"];
    assert_eq!(effect["id"], "alpha2.effect.gaussian-blur");
}

#[test]
fn a_portrait_blur_is_a_fraction_of_the_width() {
    let compiled = compile(&json!({
        "canvas": "shorts",
        "body": {"type": "reframe", "video": "wide.mp4", "blur": 0.05},
    }));
    let effect = &compiled["project"]["compositions"][0]["nodes"][1]["op"]["effect"];
    assert_eq!(effect["id"], "alpha2.effect.gaussian-blur");
    #[allow(clippy::cast_possible_truncation)]
    let radius = effect["params"]["radius"]["value"]["value"]
        .as_f64()
        .expect("a constant radius") as f32;
    let canvas = &compiled["project"]["settings"];
    let size = alpha2_render::RenderSize::new(
        u32::try_from(canvas["width"].as_u64().expect("width")).expect("width"),
        u32::try_from(canvas["height"].as_u64().expect("height")).expect("height"),
    )
    .expect("size");
    assert!(size.height > size.width, "a portrait canvas");
    #[allow(clippy::cast_precision_loss)]
    let expected = 0.025 * size.width as f32;
    assert_eq!(
        alpha2_render::blur_sigma_pixels(radius, size).to_bits(),
        expected.to_bits()
    );
}

#[test]
fn a_music_bed_ducks_under_the_body() {
    let compiled = compile(&json!({
        "body": {"type": "media", "path": "wide.mp4"},
        "audio": [{"type": "music_bed", "path": "music.m4a", "duck_under": "body", "volume": 0.5}],
    }));
    let tracks = tracks(&compiled);
    assert_eq!(
        tracks[1]["audio"]["ducking"]["sidechain_track"],
        tracks[0]["id"]
    );
    // A looping bed lasts as long as the video, not its file.
    assert_eq!(tracks[1]["clips"][0]["timeline_range"]["end"], 10 * SECOND);
}

#[test]
fn a_crossfade_into_a_card_fades_the_card_in_over_the_clip() {
    let compiled = compile(&json!({
        "body": {"type": "sequence", "transition": "theme", "children": [
            {"type": "media", "path": "wide.mp4"},
            {"type": "title_card", "text": "The end", "duration": 3},
        ]},
    }));
    let tracks = tracks(&compiled);
    let card = &tracks[1]["clips"][0];
    assert_eq!(card["timeline_range"]["start"], 19 * SECOND / 2);
    assert_eq!(card["visual"]["opacity"]["keyframes"][0]["value"], 0.0);
}

#[test]
#[allow(clippy::cast_precision_loss)]
fn words_group_into_cues() {
    let words = ["Hello", "there.", "This", "is", "a", "test"]
        .iter()
        .enumerate()
        .map(|(index, word)| json!({"text": word, "start": index as f64 * 0.4, "end": index as f64 * 0.4 + 0.3}))
        .collect::<Vec<_>>();
    let compiled = compile(&json!({"body": {"type": "captions", "words": words}}));
    let cues = &tracks(&compiled)[0]["captions"]["cues"];
    assert_eq!(cues[0]["text"], "Hello there.");
    assert_eq!(cues[1]["text"], "This is a test");
}

#[test]
fn errors_name_where_in_the_spec() {
    let mut spec = slideshow();
    spec["body"]["items"][3] = json!("missing.jpg");
    assert!(
        compile_error(&spec)
            .starts_with("spec.body.items[3].path: no picture size for \"missing.jpg\"")
    );
    let error = compile_error(
        &json!({"body": {"type": "sequence", "transition": "wipe-left", "children": [
            {"type": "media", "path": "wide.mp4"}, {"type": "title_card", "text": "x"},
        ]}}),
    );
    assert!(
        error
            .starts_with("spec.body.children[1]: a wipe-left transition joins two single pictures"),
        "{error}"
    );
    let error =
        compile_error(&json!({"body": {"type": "media", "path": "wide.mp4", "opacity": 2}}));
    assert_eq!(error, "spec.body.opacity: 2 is outside 0..=1");
}

#[test]
fn requirements_list_each_file_once_with_its_kind() {
    let text = requirements_json(&json!({
        "body": {"type": "captioned_video", "video": "talk.mp4", "captions": {"file": "talk.srt"}},
        "overlays": [{"type": "logo", "path": "logo.png"}, {"type": "media", "path": "talk.mp4"}],
        "audio": [{"type": "music_bed", "path": "music.m4a"}],
    }).to_string())
    .expect("parses");
    let found: Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(
        found,
        json!({
            "media": [
                {"path": "talk.mp4", "kind": "video"},
                {"path": "logo.png", "kind": "image"},
                {"path": "music.m4a", "kind": "audio"},
            ],
            "texts": ["talk.srt"],
        })
    );
    assert!(serde_json::from_str::<Facts>("{}").is_ok());
}

#[test]
fn parse_errors_name_where_inside_nodes() {
    let cases = [
        (
            json!({"type": "stack", "children": [{"type": "text", "text": "x", "colour": "#fff"}]}),
            "spec.body.children[0].colour: unknown field `colour`",
        ),
        (
            json!({"type": "stak", "children": []}),
            "spec.body.type: unknown node type \"stak\"",
        ),
        (
            json!({"type": "slideshow", "items": ["a.jpg", {"type": "media", "path": 3}]}),
            "spec.body.items[1].path: invalid type: integer `3`",
        ),
        (json!({"type": "media"}), "spec.body: missing field `path`"),
        (
            json!({"type": "media", "path": "a.mp4", "inset": "x"}),
            "spec.body.inset: an inset is pixels",
        ),
    ];
    for (body, expected) in cases {
        let error = compile_error(&json!({"body": body}));
        assert!(error.starts_with(expected), "{error}");
    }
    let error =
        compile_error(&json!({"canvas": {"width": "x"}, "body": {"type": "media", "path": "a"}}));
    assert!(
        error.starts_with("spec.canvas.width: invalid type"),
        "{error}"
    );
}
