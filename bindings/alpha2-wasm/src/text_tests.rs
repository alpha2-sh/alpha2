use super::{TextRequestWire, descriptor_json, file_faces, font};
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
fn a_font_file_gives_every_face_it_holds() {
    let pair = std::fs::read("../../tests/fonts/Alpha2FixturePair.ttc").expect("pair");
    let faces = file_faces("Pair", pair).expect("faces");
    assert_eq!(
        faces.iter().map(|face| face.face_index).collect::<Vec<_>>(),
        [0, 1]
    );
    assert!(std::sync::Arc::ptr_eq(&faces[0].bytes, &faces[1].bytes));
    assert!(file_faces("Not a font", b"wOF2....".to_vec()).is_err());
}
