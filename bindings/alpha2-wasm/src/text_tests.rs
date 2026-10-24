use super::{MeasureError, TextRequestWire, descriptor_json, font, measure_json};
use alpha2_text::TextRasterizer;

#[test]
fn a_json_request_rasterizes_like_the_native_request() {
    let bytes = std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture font");
    let mut rasterizer = TextRasterizer::empty();
    rasterizer
        .add_font(font("Alpha2 Fixture".into(), bytes, 0))
        .expect("valid font");
    let wire: TextRequestWire = serde_json::from_str(
        r#"{"text":"Alpha2","fontSize":24,"align":"center","color":[255,255,255,255]}"#,
    )
    .expect("request JSON");
    let result = rasterizer.rasterize(&wire.into_request(3)).expect("raster");
    let descriptor: serde_json::Value =
        serde_json::from_str(&descriptor_json(&result)).expect("descriptor JSON");
    assert_eq!(descriptor["width"], result.descriptor.texture.width);
    assert_eq!(descriptor["lineCount"], 1);
    assert!(
        serde_json::from_str::<TextRequestWire>(
            r#"{"text":"a","fontSize":1,"color":[0,0,0,0],"font":"x"}"#
        )
        .is_err(),
        "unknown request fields are refused"
    );
}

#[test]
fn a_text_generator_measures_as_the_engine_measures_it() {
    let bytes = std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture font");
    let mut rasterizer = TextRasterizer::empty();
    let generator = r#"{ "type": "text", "text": "Title", "style": { "font_size": 0.1 } }"#;
    assert!(
        matches!(
            measure_json(&rasterizer, generator, 1920, 1080),
            Err(MeasureError::Text(alpha2_engine::TextError::Raster(
                alpha2_text::TextRasterError::NoFonts
            )))
        ),
        "no fonts"
    );
    rasterizer
        .add_font(font("Alpha2 Fixture".into(), bytes, 0))
        .expect("valid font");
    let measured: serde_json::Value =
        serde_json::from_str(&measure_json(&rasterizer, generator, 1920, 1080).expect("measures"))
            .expect("JSON");
    let bounds = alpha2_engine::measure_text(
        &rasterizer,
        "Title",
        &alpha2_model::TextStyle {
            font_size: 0.1,
            ..alpha2_model::TextStyle::default()
        },
        alpha2_render::RenderSize::new(1920, 1080).expect("size"),
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
        let Err(MeasureError::Input(message)) = measure_json(&rasterizer, refused, 1920, 1080)
        else {
            panic!("{refused} is refused as input");
        };
        assert!(message.contains("text generator"), "{message}");
    }
    let Err(MeasureError::Input(message)) = measure_json(&rasterizer, generator, 0, 1080) else {
        panic!("an empty canvas is refused");
    };
    assert!(message.contains("canvas"), "{message}");
}

#[test]
fn a_layout_measures_as_the_engine_measures_it() {
    use super::measure_layout_json;
    let bytes = std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture font");
    let mut rasterizer = TextRasterizer::empty();
    let generator = r#"{ "type": "layout", "root": { "justify_content": "center", "children": [
        { "padding": { "mode": "constant", "value": { "left": 0.02, "right": 0.02 } },
          "children": [ { "text": { "mode": "constant", "value": "Title" } } ] } ] } }"#;
    assert!(
        matches!(
            measure_layout_json(&rasterizer, generator, 1920, 1080, 0.0, None),
            Err(MeasureError::Text(alpha2_engine::TextError::Raster(
                alpha2_text::TextRasterError::NoFonts
            )))
        ),
        "no fonts"
    );
    rasterizer
        .add_font(font("Alpha2 Fixture".into(), bytes, 0))
        .expect("valid font");
    let measured: serde_json::Value = serde_json::from_str(
        &measure_layout_json(&rasterizer, generator, 1920, 1080, 0.0, None).expect("measures"),
    )
    .expect("JSON");
    let alpha2_model::Generator::Layout { root } =
        serde_json::from_str(generator).expect("generator")
    else {
        panic!("a layout");
    };
    let bounds = alpha2_engine::measure_layout(
        &rasterizer,
        &root,
        alpha2_time::MediaTime::ZERO,
        alpha2_render::RenderSize::new(1920, 1080).expect("size"),
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
            measure_layout_json(&rasterizer, refused, 1920, 1080, 0.0, None)
        else {
            panic!("{refused} is refused as input");
        };
        assert!(message.contains(why), "{message}");
    }
    assert!(matches!(
        measure_layout_json(&rasterizer, generator, 1920, 1080, 0.5, None),
        Err(MeasureError::Input(_))
    ));
    // A pixel font size within a larger canvas's shorter side is measured on it.
    let large = r#"{ "type": "layout", "root": { "font_size": { "mode": "constant", "value": "1200px" } } }"#;
    assert!(measure_layout_json(&rasterizer, large, 1920, 1080, 0.0, Some((3840, 2160))).is_ok());
    assert!(matches!(
        measure_layout_json(&rasterizer, generator, 1920, 1080, 0.0, Some((0, 10))),
        Err(MeasureError::Input(message)) if message.contains("project canvas")
    ));
}

/// The pinned layout measurements the browser's `WasmTextMeasurer.measureLayout` must reproduce
/// bit for bit (`npm run test:wasm-parity`). Run with `ALPHA2_UPDATE_GOLDENS=1` to re-pin after an
/// intended change.
#[test]
fn layout_measurements_match_the_pinned_answers() {
    use super::measure_layout_json;
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
    rasterizer
        .add_font(font("Alpha2 Fixture".into(), bytes, 0))
        .expect("valid font");
    let update =
        std::env::var_os("ALPHA2_UPDATE_GOLDENS").as_deref() == Some(std::ffi::OsStr::new("1"));
    for case in golden["cases"].as_array_mut().expect("cases") {
        let number = |key: &str| case[key].as_u64().expect("a size");
        // A case may give the project canvas its pixels are pixels of.
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
            case["time"].as_f64().expect("time"),
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
