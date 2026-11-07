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
    let joined = |cue: &Value| {
        cue["text"]
            .as_array()
            .expect("runs")
            .iter()
            .map(|run| run["text"].as_str().expect("text"))
            .collect::<String>()
    };
    assert_eq!(joined(&cues[0]), "Hello there.");
    assert_eq!(joined(&cues[1]), "This is a test");
}

#[test]
#[allow(clippy::cast_precision_loss)]
fn words_keep_their_times_as_runs() {
    let words = ["One", "two", "three"]
        .iter()
        .enumerate()
        .map(|(index, word)| json!({"text": word, "start": 1.0 + index as f64 * 0.5, "end": 1.4 + index as f64 * 0.5}))
        .collect::<Vec<_>>();
    let compiled = compile(&json!({"body": {"type": "captions", "words": words}}));
    let cue = &tracks(&compiled)[0]["captions"]["cues"][0];
    let runs = cue["text"].as_array().expect("runs");
    assert_eq!(runs[0]["text"], "One ");
    assert_eq!(runs[2]["text"], "three");
    assert_eq!(runs[0]["time"], json!({"start": 0, "end": 4 * SECOND / 10}));
    assert_eq!(
        runs[1]["time"],
        json!({"start": SECOND / 2, "end": 9 * SECOND / 10})
    );
    assert_eq!(cue["timeline_range"]["start"], SECOND);
}

#[test]
fn karaoke_preset_highlights_the_active_word() {
    let words = json!([
        {"text": "Say", "start": 0.0, "end": 0.5},
        {"text": "it", "start": 0.5, "end": 1.0},
    ]);
    let compiled = compile(&json!({"body": {"type": "captions", "words": words,
        "style": "karaoke", "highlight": "#ffd040"}}));
    let style = &tracks(&compiled)[0]["captions"]["style"];
    assert_eq!(style["strokes"].as_array().expect("strokes").len(), 1);
    assert_eq!(style["shadows"].as_array().expect("shadows").len(), 1);
    let animators = style["animators"].as_array().expect("animators");
    assert_eq!(animators.len(), 2);
    assert_eq!(animators[0]["selector"]["state"], "spoken");
    assert_eq!(animators[1]["selector"]["state"], "active");
    assert_eq!(
        animators[1]["properties"]["fill"]["value"],
        json!({"red": 1.0, "green": 0.815_686_3, "blue": 0.250_980_4, "alpha": 1.0})
    );
    let spec = json!({"body": {"type": "captions", "style": "pop",
        "cues": [{"text": "No times", "start": 0, "end": 1}]}});
    let crate::Compiled {
        project, warnings, ..
    } = crate::compile(
        &serde_json::from_value(spec).expect("spec"),
        &serde_json::from_value(facts()).expect("facts"),
        &[],
    )
    .expect("compiles");
    let style = &project.timeline.tracks[0]
        .captions
        .as_ref()
        .expect("captions")
        .style;
    assert!(style.animators.is_empty() && !style.shadows.is_empty());
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("draw as bold")),
        "{warnings:?}"
    );
}

#[test]
fn speakers_compile_to_styles() {
    let compiled = compile(&json!({"body": {"type": "captions",
        "cues": [{"text": "Hi", "start": 0, "end": 1, "speaker": "Ann"},
                 {"text": "Hey", "start": 1, "end": 2, "speaker": "Bob"}],
        "speakers": {"Ann": {"color": "#ff8080", "position": {"x": 0.25, "y": 0.8}}}}}));
    let captions = &tracks(&compiled)[0]["captions"];
    assert_eq!(
        captions["styles"]["Ann"]["position"],
        json!({"x": 0.25, "y": 0.8})
    );
    assert_eq!(captions["styles"]["Ann"]["fill"]["type"], "solid");
    assert_eq!(captions["cues"][0]["speaker"], "Ann");
    assert_eq!(captions["cues"][1]["speaker"], "Bob");
    assert!(captions["styles"].get("Bob").is_none());
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

#[test]
fn a_faster_video_keeps_its_pitch_unless_told_not_to() {
    let audio = |media: Value| {
        let project = compile(&json!({ "body": media }));
        tracks(&project)
            .iter()
            .flat_map(|track| track["clips"].as_array().into_iter().flatten())
            // A held last frame is a freeze, silent at any speed.
            .filter(|clip| clip["time_map"]["type"] != "freeze")
            .map(|clip| clip["audio"]["preserve_pitch"].as_bool().unwrap_or(false))
            .collect::<Vec<_>>()
    };
    let path = "talk.mp4";
    assert!(
        audio(json!({"type": "media", "path": path, "speed": 1.5}))
            .iter()
            .all(|kept| *kept)
    );
    // At speed 1 the clip reads as before, so the document says nothing.
    assert!(
        !audio(json!({"type": "media", "path": path}))
            .iter()
            .any(|kept| *kept)
    );
    assert!(
        !audio(json!({"type": "media", "path": path, "speed": 1.5, "preserve_pitch": false}))
            .iter()
            .any(|kept| *kept)
    );
}

/// Specs that use no motion field compile to pinned bytes. Set `ALPHA2_UPDATE_GOLDENS=1` to
/// re-pin after an intended change.
#[test]
fn old_specs_compile_to_the_same_bytes() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/old-specs.golden.json");
    let mut cases: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(path).expect("golden")).expect("JSON");
    let update =
        std::env::var_os("ALPHA2_UPDATE_GOLDENS").as_deref() == Some(std::ffi::OsStr::new("1"));
    let facts = facts().to_string();
    for case in &mut cases {
        let compiled = compile_json(&case["spec"].to_string(), &facts, &[]).expect("compiles");
        if update {
            case["compiled"] = Value::String(compiled);
            continue;
        }
        assert_eq!(
            compiled,
            case["compiled"].as_str().expect("a string"),
            "{}",
            case["spec"]
        );
    }
    if update {
        let mut text = serde_json::to_string_pretty(&cases).expect("serializes");
        text.push('\n');
        std::fs::write(path, text).expect("writes the golden");
    }
}

fn keyframe_values(animated: &Value) -> Vec<Value> {
    animated["keyframes"]
        .as_array()
        .expect("keyframes")
        .iter()
        .map(|keyframe| keyframe["value"].clone())
        .collect()
}

fn keyframe_times(animated: &Value) -> Vec<i64> {
    animated["keyframes"]
        .as_array()
        .expect("keyframes")
        .iter()
        .map(|keyframe| keyframe["time"].as_i64().expect("ticks"))
        .collect()
}

#[test]
fn a_rise_enters_and_a_fade_exits_back_from_the_end() {
    let compiled = compile(&json!({
        "body": {"type": "stack", "children": [
            {"type": "color", "duration": 4,
             "enter": {"type": "rise", "duration": 0.5, "distance": 54, "easing": "ease-out"},
             "exit": {"type": "fade", "duration": 1}},
        ]},
    }));
    let visual = &tracks(&compiled)[0]["clips"][0]["visual"];
    assert_eq!(
        keyframe_times(&visual["opacity"]),
        [0, SECOND / 2, 3 * SECOND, 4 * SECOND]
    );
    assert_eq!(
        keyframe_values(&visual["opacity"]),
        [json!(0.0), json!(1.0), json!(1.0), json!(0.0)]
    );
    assert_eq!(
        visual["opacity"]["keyframes"][0]["interpolation"],
        json!({"type": "cubic_bezier", "x1": 0.0, "y1": 0.0, "x2": 0.58, "y2": 1.0})
    );
    // 54 px below on a 1080 px canvas, rising into place.
    let position = keyframe_values(&visual["position"]);
    assert_eq!(position[0], json!({"x": 0.5, "y": 0.55}));
    assert_eq!(position[1], json!({"x": 0.5, "y": 0.5}));
}

#[test]
fn a_staggered_stack_enters_child_by_child() {
    let compiled = compile(&json!({
        "body": {"type": "stack", "duration": 3, "stagger": 0.25,
                 "enter": {"type": "fade", "duration": 0.5},
                 "children": [{"type": "color"}, {"type": "color"}, {"type": "color"}]},
    }));
    let tracks = tracks(&compiled);
    for (index, track) in tracks.iter().enumerate() {
        let start = i64::try_from(index).expect("index") * SECOND / 4;
        let times = keyframe_times(&track["clips"][0]["visual"]["opacity"]);
        let expected = if start == 0 {
            vec![0, SECOND / 2, 3 * SECOND]
        } else {
            vec![0, start, start + SECOND / 2, 3 * SECOND]
        };
        assert_eq!(times, expected, "child {index}");
    }
}

#[test]
fn a_scale_grows_text_about_its_own_point() {
    let compiled = compile(&json!({
        "body": {"type": "text", "text": "Hi", "align": "top-left", "inset": 100, "duration": 2,
                 "enter": {"type": "scale", "size": 0.5, "fade": false}},
    }));
    let visual = &tracks(&compiled)[0]["clips"][0]["visual"];
    assert_eq!(visual["anchor"], visual["position"]["value"]);
    assert_eq!(visual["opacity"], json!({"mode": "constant", "value": 1.0}));
    let scale = keyframe_values(&visual["scale"]);
    assert_eq!(scale[0], json!({"x": 0.5, "y": 0.5}));
    assert_eq!(scale[1], json!({"x": 1.0, "y": 1.0}));
}

#[test]
fn a_word_reveal_sweeps_a_range_selector() {
    let compiled = compile(&json!({
        "body": {"type": "text", "text": "One two three", "duration": 3,
                 "reveal": {"by": "word", "type": "rise", "stagger": 0.1, "duration": 0.4, "distance": 27}},
    }));
    let style = &compiled["project"]["sources"][0]["generator"]["style"];
    let animator = &style["animators"][0];
    assert_eq!(animator["selector"]["based_on"], "words");
    assert_eq!(animator["selector"]["units"], "index");
    assert_eq!(animator["selector"]["shape"], "ramp_up");
    // A unit spans 4 staggers; word 0 is fully hidden at time 0.
    assert_eq!(animator["selector"]["end"]["value"], 4.0);
    assert_eq!(
        animator["selector"]["offset"]["keyframes"][0]["value"],
        -3.5
    );
    assert_eq!(animator["properties"]["opacity"]["value"], 0.0);
    let rise = animator["properties"]["offset"]["value"]["y"]
        .as_f64()
        .expect("a rise");
    let font = style["font_size"].as_f64().expect("size") * 1080.0;
    assert!((rise * font - 27.0).abs() < 1e-3, "{rise}");
}

#[test]
fn a_counter_holds_each_value_on_the_frame_grid() {
    let compiled = compile(&json!({
        "canvas": {"frame_rate": 10},
        "body": {"type": "counter", "start": 0, "end": 1500, "duration": 2, "count_duration": 1,
                 "format": {"thousands": ",", "prefix": "$"}},
    }));
    let root = &compiled["project"]["sources"][0]["generator"]["root"];
    let text = &root["children"][0]["text"];
    let values = keyframe_values(text);
    assert_eq!(values.first(), Some(&json!("$0")));
    assert_eq!(values.last(), Some(&json!("$1,500")));
    assert_eq!(values[5], json!("$750"));
    assert_eq!(keyframe_times(text)[1], SECOND / 10);
    assert!(
        text["keyframes"]
            .as_array()
            .expect("keys")
            .iter()
            .all(|key| key["interpolation"]["type"] == "hold")
    );
}

#[test]
fn a_counter_formats_decimals_and_negatives() {
    let compiled = compile(&json!({
        "body": {"type": "counter", "start": -2.5, "end": -2.5, "duration": 1,
                 "format": {"decimals": 2, "point": ",", "suffix": " %"}},
    }));
    let text = &compiled["project"]["sources"][0]["generator"]["root"]["children"][0]["text"];
    assert_eq!(text, &json!({"mode": "constant", "value": "-2,50 %"}));
}

#[test]
fn motion_fields_are_checked() {
    let error = compile_error(&json!({
        "body": {"type": "color", "enter": {"type": "fade", "direction": "up"}},
    }));
    assert!(error.contains("spec.body.enter.direction"), "{error}");
    let error = compile_error(&json!({
        "body": {"type": "color", "enter": {"type": "fade", "easing": "bouncy"}},
    }));
    assert!(error.contains("unknown easing"), "{error}");
}

fn text_style(spec: &Value) -> Value {
    let compiled = compile(spec);
    compiled["project"]["sources"]
        .as_array()
        .expect("sources")
        .iter()
        .find_map(|source| source["generator"].get("style").cloned())
        .expect("a text source")
}

#[test]
fn a_text_fits_and_clamps_in_its_area_as_the_engine_draws_it() {
    let text = |fields: Value| {
        let mut node =
            json!({"type": "text", "text": "A headline long enough to wrap", "font_size": 100});
        node.as_object_mut()
            .expect("object")
            .extend(fields.as_object().expect("object").clone());
        text_style(&json!({"canvas": "youtube-1080p", "duration": 2, "body": node}))
    };
    // 1080 px is the shorter side.
    let style = text(json!({"max_lines": 2}));
    assert_eq!(
        style["line_clamp"],
        json!({"max_lines": 2, "block_ellipsis": "ellipsis"})
    );
    let fit = &style["font_size_fit"];
    assert!(
        (fit["min"].as_f64().unwrap() - 30.0 / 1080.0).abs() < 1e-6,
        "{fit}"
    );
    assert!(
        (fit["max"].as_f64().unwrap() - 100.0 / 1080.0).abs() < 1e-6,
        "{fit}"
    );
    assert!(style["max_height"].as_f64().is_some());

    let style = text(json!({"fit": {"min": 40, "max": 120}, "ellipsis": "..."}));
    assert_eq!(
        style["line_clamp"],
        json!({"block_ellipsis": {"string": "..."}})
    );
    assert!((style["font_size_fit"]["max"].as_f64().unwrap() - 120.0 / 1080.0).abs() < 1e-6);

    let style = text(json!({"max_lines": 1, "ellipsis": false}));
    assert_eq!(style["line_clamp"]["block_ellipsis"], json!("no_ellipsis"));

    let style = text(json!({}));
    assert!(style.get("font_size_fit").is_none_or(Value::is_null));
    assert!(style.get("line_clamp").is_none_or(Value::is_null));
}

#[test]
fn a_text_fit_and_line_count_are_checked() {
    let error = |fields: Value| {
        let mut node = json!({"type": "text", "text": "Hi", "font_size": 100});
        node.as_object_mut()
            .expect("object")
            .extend(fields.as_object().expect("object").clone());
        compile_error(&json!({"duration": 1, "body": node}))
    };
    assert!(error(json!({"max_lines": 0})).contains("at least one line"));
    assert!(error(json!({"fit": {"min": 0}})).contains("fit"));
    assert!(error(json!({"fit": {"min": 120}})).contains("fit"));
}

fn media_scale(zoom: &Value) -> Value {
    let compiled = compile(&json!({
        "canvas": "youtube-1080p",
        "body": {"type": "media", "path": "logo.png", "duration": 4, "zoom": zoom},
    }));
    tracks(&compiled)[0]["clips"][0]["visual"]["scale"].clone()
}

#[test]
fn an_animated_value_is_a_constant_a_ramp_or_keys_over_the_node() {
    assert_eq!(media_scale(&json!(1.5))["mode"], "constant");

    let ramp = media_scale(&json!({"from": 1, "to": 2, "ease": "ease-in", "at": 1, "duration": 2}));
    let keys = ramp["keyframes"].as_array().expect("keyframes");
    let times = keys
        .iter()
        .map(|key| key["time"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(times, [SECOND, 3 * SECOND]);
    assert_eq!(keys[0]["interpolation"]["type"], "cubic_bezier");
    let ratio = keys[1]["value"]["x"].as_f64().unwrap() / keys[0]["value"]["x"].as_f64().unwrap();
    assert!((ratio - 2.0).abs() < 1e-6, "{ratio}");

    // A ramp without a duration runs to the node's end.
    let to_end = media_scale(&json!({"from": 1, "to": 2}));
    assert_eq!(to_end["keyframes"][1]["time"].as_i64(), Some(4 * SECOND));

    let keyed = media_scale(
        &json!({"keys": [{"t": 0, "v": 1}, {"t": 1, "v": 1.2, "ease": "ease-out"}, {"t": 4, "v": 1}]}),
    );
    let keys = keyed["keyframes"].as_array().expect("keyframes");
    assert_eq!(keys.len(), 3);
    assert_eq!(keys[0]["interpolation"]["type"], "linear");
    assert_eq!(keys[1]["interpolation"]["type"], "cubic_bezier");
}

#[test]
fn animated_values_are_checked() {
    let error = |zoom: Value| {
        compile_error(
            &json!({"body": {"type": "media", "path": "logo.png", "duration": 1, "zoom": zoom}}),
        )
    };
    assert!(error(json!(0)).contains("zoom"));
    assert!(error(json!({"from": 1, "to": -1})).contains("zoom"));
    assert!(error(json!({"keys": []})).contains("at least one key"));
    assert!(error(json!({"keys": [{"t": 1, "v": 1}, {"t": 1, "v": 2}]})).contains("increasing"));
    assert!(error(json!({"from": 1, "to": 2, "ease": "bounce"})).contains("ease"));
    assert!(error(json!([1, 2])).contains("zoom"));
}
