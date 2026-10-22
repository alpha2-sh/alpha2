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
