use super::{MeasureError, StringUnit, measure_layout_json, measure_text_json, text_layout_json};
use crate::PictureSize;
use alpha2_text::{TextFont, TextRasterError, TextRasterizer};
use alpha2_time::MediaTime;

fn fixture(bytes: Vec<u8>) -> TextFont {
    TextFont {
        family: "Alpha2 Fixture".into(),
        bytes: bytes.into(),
        face_index: 0,
    }
}

#[test]
fn a_text_generator_measures_as_the_engine_measures_it() {
    let bytes = std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture font");
    let mut rasterizer = TextRasterizer::empty();
    let generator = r#"{ "type": "text", "text": "Title", "style": { "font_size": 0.1 } }"#;
    assert!(
        matches!(
            measure_text_json(&rasterizer, generator, 1920, 1080),
            Err(MeasureError::Text(TextRasterError::NoFonts))
        ),
        "no fonts"
    );
    rasterizer.add_font(fixture(bytes)).expect("valid font");
    let measured: serde_json::Value = serde_json::from_str(
        &measure_text_json(&rasterizer, generator, 1920, 1080).expect("measures"),
    )
    .expect("JSON");
    let bounds = crate::measure_text(
        &rasterizer,
        &crate::TextAt::from("Title"),
        &alpha2_model::TextStyle {
            font_size: 0.1,
            ..alpha2_model::TextStyle::default()
        },
        PictureSize::new(1920, 1080).expect("size"),
    )
    .expect("measures");
    let ink = bounds.ink.expect("ink");
    assert_eq!(measured["ink"]["left"], f64::from(ink.left));
    assert_eq!(
        measured["content"]["bottom"],
        f64::from(bounds.content.bottom)
    );
    assert_eq!(measured["layout"]["top"], f64::from(bounds.layout.top));
    assert_eq!(measured["lineCount"], 1);
    for refused in [
        r#"{ "type": "fill", "paint": { "type": "solid", "color": { "red": 0, "green": 0, "blue": 0, "alpha": 1 } } }"#,
        r#"{ "type": "text", "text": "", "style": {} }"#,
        r#"{ "type": "text", "text": "a", "style": { "font_size": 2 } }"#,
        r#"{ "type": "text", "text": "a", "style": { "size": 0.1 } }"#,
    ] {
        let Err(MeasureError::Input(message)) = measure_text_json(&rasterizer, refused, 1920, 1080)
        else {
            panic!("{refused} is refused as input");
        };
        assert!(message.contains("text generator"), "{message}");
    }
    let Err(MeasureError::Input(message)) = measure_text_json(&rasterizer, generator, 0, 1080)
    else {
        panic!("an empty canvas is refused");
    };
    assert!(message.contains("canvas"), "{message}");
}

#[test]
fn a_layout_measures_as_the_engine_measures_it() {
    let bytes = std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture font");
    let mut rasterizer = TextRasterizer::empty();
    let generator = r#"{ "type": "layout", "root": { "justify_content": "center", "children": [
        { "padding": { "mode": "constant", "value": { "left": 0.02, "right": 0.02 } },
          "children": [ { "text": { "mode": "constant", "value": "Title" } } ] } ] } }"#;
    assert!(
        matches!(
            measure_layout_json(&rasterizer, generator, 1920, 1080, MediaTime::ZERO, None),
            Err(MeasureError::Text(TextRasterError::NoFonts))
        ),
        "no fonts"
    );
    rasterizer.add_font(fixture(bytes)).expect("valid font");
    let measured: serde_json::Value = serde_json::from_str(
        &measure_layout_json(&rasterizer, generator, 1920, 1080, MediaTime::ZERO, None)
            .expect("measures"),
    )
    .expect("JSON");
    let alpha2_model::Generator::Layout { root } =
        serde_json::from_str(generator).expect("generator")
    else {
        panic!("a layout");
    };
    let bounds = crate::measure_layout(
        &rasterizer,
        &root,
        MediaTime::ZERO,
        PictureSize::new(1920, 1080).expect("size"),
        (1920, 1080),
    )
    .expect("measures");
    let nodes = measured["nodes"].as_array().expect("nodes");
    assert_eq!(nodes.len(), bounds.nodes.len());
    for (json, node) in nodes.iter().zip(&bounds.nodes) {
        assert_eq!(json["at"], node.at);
        assert_eq!(json["border"]["left"], f64::from(node.border.left));
        assert_eq!(json["content"]["bottom"], f64::from(node.content.bottom));
        assert_eq!(json["overflow"], node.overflow);
    }
    assert_eq!(nodes[2]["lineCount"], 1);
    assert_eq!(nodes[0]["lineCount"], serde_json::Value::Null);
    for (refused, why) in [
        (r#"{ "type": "text", "text": "a" }"#, "only a layout"),
        (
            r#"{ "type": "layout", "root": { "width": { "mode": "constant", "value": 1 } } }"#,
            "root sets a size",
        ),
        (
            r#"{ "type": "layout", "root": { "float": "left" } }"#,
            "unknown field",
        ),
        (
            r#"{ "type": "layout", "root": { "display": "grid", "grid_template_columns": "1 fr" } }"#,
            r#"unknown track "1 fr" (write 1fr)"#,
        ),
        (
            r#"{ "type": "layout", "root": { "font_size": { "mode": "constant", "value": "1200px" } } }"#,
            "root.font_size has a value out of range",
        ),
    ] {
        let Err(MeasureError::Input(message)) =
            measure_layout_json(&rasterizer, refused, 1920, 1080, MediaTime::ZERO, None)
        else {
            panic!("{refused} is refused as input");
        };
        assert!(message.contains(why), "{message}");
    }
    // A pixel font size within a larger canvas's shorter side is measured on it.
    let large = r#"{ "type": "layout", "root": { "font_size": { "mode": "constant", "value": "1200px" } } }"#;
    assert!(
        measure_layout_json(
            &rasterizer,
            large,
            1920,
            1080,
            MediaTime::ZERO,
            Some((3840, 2160))
        )
        .is_ok()
    );
    assert!(matches!(
        measure_layout_json(&rasterizer, generator, 1920, 1080, MediaTime::ZERO, Some((0, 10))),
        Err(MeasureError::Input(message)) if message.contains("project canvas")
    ));
}

/// Pinned layout measurements that the wasm and Python measurers must reproduce bit for bit.
/// Set `ALPHA2_UPDATE_GOLDENS=1` to re-pin after an intended change.
#[test]
fn layout_measurements_match_the_pinned_answers() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/layout_measure.json"
    );
    let mut golden: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("golden")).expect("JSON");
    let bytes = std::fs::read(format!(
        "{}/../../{}",
        env!("CARGO_MANIFEST_DIR"),
        golden["font"].as_str().expect("font")
    ))
    .expect("font");
    let mut rasterizer = TextRasterizer::empty();
    rasterizer.add_font(fixture(bytes)).expect("valid font");
    let update =
        std::env::var_os("ALPHA2_UPDATE_GOLDENS").as_deref() == Some(std::ffi::OsStr::new("1"));
    for case in golden["cases"].as_array_mut().expect("cases") {
        let number = |key: &str| case[key].as_u64().expect("a size");
        let canvas = case.get("canvas").map(|canvas| {
            let side = |index: usize| {
                u32::try_from(canvas[index].as_u64().expect("a canvas side")).expect("a side")
            };
            (side(0), side(1))
        });
        let measured = measure_layout_json(
            &rasterizer,
            &case["generator"].to_string(),
            u32::try_from(number("width")).expect("width"),
            u32::try_from(number("height")).expect("height"),
            MediaTime(case["time"].as_i64().expect("time")),
            canvas,
        )
        .expect("measures");
        if update {
            case["expected"] = serde_json::Value::String(measured);
        } else {
            assert_eq!(
                case["expected"].as_str(),
                Some(measured.as_str()),
                "{}",
                case["name"]
            );
        }
    }
    if update {
        let mut text = serde_json::to_string_pretty(&golden).expect("JSON");
        text.push('\n');
        std::fs::write(path, text).expect("golden written");
    }
}

/// Pinned text measurements that the wasm and Python measurers must reproduce bit for bit.
/// Set `ALPHA2_UPDATE_GOLDENS=1` to re-pin after an intended change.
#[test]
fn text_measurements_match_the_pinned_answers() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/text_measure.json"
    );
    let mut golden: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("golden")).expect("JSON");
    let bytes = std::fs::read(format!(
        "{}/../../{}",
        env!("CARGO_MANIFEST_DIR"),
        golden["font"].as_str().expect("font")
    ))
    .expect("font");
    let mut rasterizer = TextRasterizer::empty();
    rasterizer.add_font(fixture(bytes)).expect("valid font");
    let update =
        std::env::var_os("ALPHA2_UPDATE_GOLDENS").as_deref() == Some(std::ffi::OsStr::new("1"));
    for case in golden["cases"].as_array_mut().expect("cases") {
        let number =
            |key: &str| u32::try_from(case[key].as_u64().expect("a size")).expect("a side");
        let measured = measure_text_json(
            &rasterizer,
            &case["generator"].to_string(),
            number("width"),
            number("height"),
        )
        .expect("measures");
        if update {
            case["expected"] = serde_json::Value::String(measured);
        } else {
            assert_eq!(
                case["expected"].as_str(),
                Some(measured.as_str()),
                "{}",
                case["name"]
            );
        }
    }
    if update {
        let mut text = serde_json::to_string_pretty(&golden).expect("JSON");
        text.push('\n');
        std::fs::write(path, text).expect("golden written");
    }
}

/// A layout child with every bounded value at its bound.
const AT_BOUNDS: &str = r#"{ "type": "layout", "root": { "children": [{
    "width": { "mode": "constant", "value": "16777216px" },
    "height": { "mode": "constant", "value": 1024 },
    "min_width": { "mode": "constant", "value": "100000%" },
    "margin": { "mode": "constant", "value": { "top": -1024, "right": 1024, "bottom": "-16777216px", "left": 0 } },
    "padding": { "mode": "constant", "value": { "top": 1024, "right": 0, "bottom": 0, "left": 0 } },
    "flex_grow": { "mode": "constant", "value": 1000000 },
    "flex_shrink": { "mode": "constant", "value": 1000000 },
    "aspect_ratio": 10000,
    "translate": { "mode": "constant", "value": { "x": -1024, "y": "16777216px" } },
    "text": { "mode": "constant", "value": "Far" }
}] } }"#;

/// Whether every number in `value` is finite; a non-finite one is written as `null`, so every
/// rectangle edge must be a number.
fn numbers_only(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(rect) if rect.contains_key("left") => {
            ["left", "top", "right", "bottom"]
                .iter()
                .all(|edge| rect[*edge].as_f64().is_some_and(f64::is_finite))
        }
        serde_json::Value::Number(number) => number.as_f64().is_some_and(f64::is_finite),
        serde_json::Value::Array(items) => items.iter().all(numbers_only),
        serde_json::Value::Object(fields) => fields.values().all(numbers_only),
        _ => true,
    }
}

#[test]
fn values_at_their_bounds_measure_to_finite_numbers() {
    let bytes = std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture font");
    let mut rasterizer = TextRasterizer::empty();
    rasterizer.add_font(fixture(bytes)).expect("valid font");
    // The widest picture, as tall as the pixel limit lets it be.
    let (width, height) = (alpha2_model::MAX_CANVAS_DIMENSION, 4_096);
    let layout = measure_layout_json(&rasterizer, AT_BOUNDS, width, height, MediaTime(0), None)
        .expect("valid");
    let layout: serde_json::Value = serde_json::from_str(&layout).expect("JSON");
    assert!(numbers_only(&layout), "{layout}");
    let past = AT_BOUNDS.replace("\"aspect_ratio\": 10000", "\"aspect_ratio\": 10001");
    assert!(matches!(
        measure_layout_json(&rasterizer, &past, width, height, MediaTime(0), None),
        Err(MeasureError::Input(_))
    ));
    // An eased keyframe overshoots its values far past the bound; sampling clamps it.
    let overshoot = r#"{ "type": "layout", "root": { "children": [{ "text": { "mode": "constant", "value": "a" },
        "width": { "mode": "keyframes", "keyframes": [
            { "time": 0, "value": 0, "interpolation": { "type": "cubic_bezier", "x1": 0.5, "y1": 1e35, "x2": 0.5, "y2": 1e35 } },
            { "time": 1000, "value": 1024, "interpolation": { "type": "hold" } }] } }] } }"#;
    let eased = measure_layout_json(&rasterizer, overshoot, width, height, MediaTime(500), None)
        .expect("valid");
    let eased: serde_json::Value = serde_json::from_str(&eased).expect("JSON");
    assert!(numbers_only(&eased), "{eased}");
    let text = r#"{ "type": "text", "text": "Far", "style": {
        "position": { "x": 1024, "y": -1024 }, "anchor": { "x": -1024, "y": 1024 } } }"#;
    let text: serde_json::Value =
        serde_json::from_str(&measure_text_json(&rasterizer, text, width, height).expect("valid"))
            .expect("JSON");
    assert!(numbers_only(&text), "{text}");
}

fn reporter() -> TextRasterizer {
    let bytes = std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture font");
    TextRasterizer::new(vec![fixture(bytes)]).expect("font")
}

fn report(generator: &serde_json::Value, unit: StringUnit) -> serde_json::Value {
    serde_json::from_str(
        &text_layout_json(&reporter(), &generator.to_string(), 960, 540, unit).expect("reports"),
    )
    .expect("JSON")
}

#[test]
fn text_layout_reports_lines_words_and_clusters() {
    let generator = serde_json::json!({ "type": "text", "text": "Hello, brave\nworld",
        "style": { "font_size": 0.1 } });
    let layout = report(&generator, StringUnit::Utf8);
    let lines = layout["lines"].as_array().expect("lines");
    let words = layout["words"].as_array().expect("words");
    let clusters = layout["clusters"].as_array().expect("clusters");
    assert_eq!((lines.len(), words.len(), clusters.len()), (2, 3, 17));
    assert_eq!(lines[0]["clusters"], serde_json::json!([0, 12]));
    assert_eq!(words[1]["clusters"], serde_json::json!([7, 12]));
    assert_eq!(clusters[5]["range"], serde_json::json!([5, 6]));
    assert_eq!(clusters[6]["space"], true);
    assert_eq!(clusters[6]["ink"], serde_json::Value::Null);
    assert_eq!(
        clusters[5]["word"],
        serde_json::Value::Null,
        "the comma is in no word"
    );
    assert_eq!(
        clusters[0]["run"],
        serde_json::Value::Null,
        "a plain string has no runs"
    );
    let measured: serde_json::Value = serde_json::from_str(
        &measure_text_json(&reporter(), &generator.to_string(), 960, 540).expect("measures"),
    )
    .expect("JSON");
    assert_eq!(layout["layout"], measured["layout"]);
    assert_eq!(lines[0]["baseline"], measured["baselines"]["first"]);
    assert_eq!(lines[1]["baseline"], measured["baselines"]["last"]);
    let number = |value: &serde_json::Value| value.as_f64().expect("a number");
    let em = &lines[0]["em"];
    let height = number(&layout["fontSize"]) * 540.0 / 540.0;
    assert!((number(&em["bottom"]) - number(&em["top"]) - height).abs() < 1e-6);
    let word = &words[0]["boxes"][0];
    assert_eq!(word["left"], clusters[0]["box"]["left"]);
    assert_eq!(word["right"], clusters[4]["box"]["right"]);
}

#[test]
fn text_layout_ranges_follow_the_binding_unit() {
    let generator = serde_json::json!({ "type": "text",
        "text": [{ "text": "\u{e9}t\u{e9} " }, { "text": "\u{1d11e}a" }] });
    let ranges = |unit| {
        report(&generator, unit)["clusters"]
            .as_array()
            .expect("clusters")
            .iter()
            .map(|cluster| (cluster["range"].clone(), cluster["run"].clone()))
            .collect::<Vec<_>>()
    };
    let pair =
        |start: u32, end: u32, run: u32| (serde_json::json!([start, end]), serde_json::json!(run));
    assert_eq!(
        ranges(StringUnit::Utf8),
        [
            pair(0, 2, 0),
            pair(2, 3, 0),
            pair(3, 5, 0),
            pair(5, 6, 0),
            pair(6, 10, 1),
            pair(10, 11, 1)
        ]
    );
    assert_eq!(
        ranges(StringUnit::Utf16),
        [
            pair(0, 1, 0),
            pair(1, 2, 0),
            pair(2, 3, 0),
            pair(3, 4, 0),
            pair(4, 6, 1),
            pair(6, 7, 1)
        ]
    );
    assert_eq!(
        ranges(StringUnit::CodePoints),
        [
            pair(0, 1, 0),
            pair(1, 2, 0),
            pair(2, 3, 0),
            pair(3, 4, 0),
            pair(4, 5, 1),
            pair(5, 6, 1)
        ]
    );
}

#[test]
fn text_layout_is_the_static_layout() {
    let plain = serde_json::json!({ "type": "text", "text": "Static words",
        "style": { "font_size": 0.1 } });
    let animated = serde_json::json!({ "type": "text", "text": "Static words",
        "style": { "font_size": 0.1, "animators": [{ "selector": { "type": "range" },
            "properties": { "tracking": { "mode": "constant", "value": 0.5 },
                            "opacity": { "mode": "constant", "value": 0 } } }] } });
    assert_eq!(
        report(&plain, StringUnit::Utf8),
        report(&animated, StringUnit::Utf8)
    );
}

#[test]
fn text_layout_reports_the_ellipsis_and_not_the_cut_text() {
    let generator = serde_json::json!({ "type": "text", "text": "one two three four five six",
        "style": { "font_size": 0.1, "max_width": 0.3, "line_clamp": { "max_lines": 1 } } });
    let layout = report(&generator, StringUnit::Utf8);
    let clusters = layout["clusters"].as_array().expect("clusters");
    let last = clusters.last().expect("clusters");
    assert_eq!(last["ellipsis"], true);
    assert_eq!(last["range"][0], last["range"][1]);
    assert_eq!(layout["lines"].as_array().expect("lines").len(), 1);
    assert!(
        clusters
            .iter()
            .all(|cluster| cluster["range"][1].as_u64() <= Some(8)),
        "the cut words have no clusters"
    );
}

/// Pinned text layouts that the wasm and Python measurers must reproduce bit for bit; their
/// texts are ASCII, so every binding's ranges agree. Set `ALPHA2_UPDATE_GOLDENS=1` to re-pin
/// after an intended change.
#[test]
fn text_layouts_match_the_pinned_answers() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/text_layout.json"
    );
    let mut golden: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("golden")).expect("JSON");
    let bytes = std::fs::read(format!(
        "{}/../../{}",
        env!("CARGO_MANIFEST_DIR"),
        golden["font"].as_str().expect("font")
    ))
    .expect("font");
    let rasterizer = TextRasterizer::new(vec![fixture(bytes)]).expect("font");
    let update =
        std::env::var_os("ALPHA2_UPDATE_GOLDENS").as_deref() == Some(std::ffi::OsStr::new("1"));
    for case in golden["cases"].as_array_mut().expect("cases") {
        let number =
            |key: &str| u32::try_from(case[key].as_u64().expect("a size")).expect("a side");
        let reported = text_layout_json(
            &rasterizer,
            &case["generator"].to_string(),
            number("width"),
            number("height"),
            StringUnit::Utf8,
        )
        .expect("reports");
        if update {
            case["expected"] = serde_json::Value::String(reported);
        } else {
            assert_eq!(
                case["expected"].as_str(),
                Some(reported.as_str()),
                "{}",
                case["name"]
            );
        }
    }
    if update {
        let mut text = serde_json::to_string_pretty(&golden).expect("JSON");
        text.push('\n');
        std::fs::write(path, text).expect("golden written");
    }
}
