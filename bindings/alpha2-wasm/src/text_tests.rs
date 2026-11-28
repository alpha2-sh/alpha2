use super::{TextRequestWire, animated_request, descriptor_json, file_faces, font};
use alpha2_text::TextRasterizer;

#[test]
fn a_json_request_rasterizes_like_the_native_request() {
    let bytes = std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture font");
    let mut rasterizer = TextRasterizer::empty();
    rasterizer
        .add_font(font("Alpha2 Fixture".into(), bytes, 0))
        .expect("valid font");
    let wire: TextRequestWire = serde_json::from_str(
        r#"{"text":"Alpha2","fontSize":24,"align":"center","fill":[255,255,255,255]}"#,
    )
    .expect("request JSON");
    let result = rasterizer
        .rasterize(&wire.into_request(3).expect("a request"))
        .expect("raster");
    let descriptor: serde_json::Value =
        serde_json::from_str(&descriptor_json(&result)).expect("descriptor JSON");
    assert_eq!(descriptor["width"], result.descriptor.texture.width);
    assert_eq!(descriptor["lineCount"], 1);
    assert!(
        serde_json::from_str::<TextRequestWire>(
            r#"{"text":"a","fontSize":1,"fill":[0,0,0,0],"font":"x"}"#
        )
        .is_err(),
        "unknown request fields are refused"
    );
    // A gradient fill, strokes and a split background read as the native request's.
    let wire: TextRequestWire = serde_json::from_str(
        r#"{"text":"Alpha2","fontSize":24,"strokes":[{"color":[0,0,0,255],"width":2}],
            "fill":{"type":"linear","start":[0,0],"end":[1,0],
                    "stops":[{"offset":0,"color":[1,0,0,1]},{"offset":1,"color":[0,0,1,1]}]},
            "background":{"color":[0,0,0,128],"padding":4},"splitBackground":true}"#,
    )
    .expect("request JSON");
    let request = wire.into_request(4).expect("a request");
    assert!(matches!(request.fill, alpha2_text::TextFill::Gradient(_)));
    assert_eq!(request.strokes.len(), 1);
    let result = rasterizer.rasterize(&request).expect("raster");
    assert!(result.background.is_some());
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

#[test]
fn an_animated_request_gives_each_cluster_its_state() {
    let bytes = std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture font");
    let mut rasterizer = TextRasterizer::empty();
    rasterizer
        .add_font(font("Alpha2 Fixture".into(), bytes, 0))
        .expect("valid font");
    let wire: TextRequestWire = serde_json::from_str(
        r#"{"text":"Move","fontSize":30,"fill":[255,255,255,255],
            "clusterState":{"scale":[1.2,1.2],"rotation":0.1,"opacity":0.8,
                            "fill":{"keep":0.5,"add":[0.5,0,0,0.5]},"tracking":2}}"#,
    )
    .expect("request JSON");
    assert!(wire.is_animated());
    let (request, states) = animated_request(wire, 5, &rasterizer).expect("states");
    assert_eq!(states.len(), 4);
    assert!(states.iter().all(
        |state| state.scale.map(f32::to_bits) == [1.2_f32.to_bits(); 2] && state.fill.is_some()
    ));
    let animated = rasterizer
        .rasterize_animated(&request, &states)
        .expect("raster");
    // A list of states is taken as is, one per cluster.
    let wire: TextRequestWire = serde_json::from_str(
        r#"{"text":"Mo","fontSize":30,"fill":[255,255,255,255],
            "clusterStates":[{"offset":[0,-4]},{"stroke":{"keep":1,"add":[0,0,0,0]}}]}"#,
    )
    .expect("request JSON");
    let (_, listed) = animated_request(wire, 6, &rasterizer).expect("states");
    assert_eq!(
        listed[0].offset.map(f32::to_bits),
        [0.0, -4.0_f32].map(f32::to_bits)
    );
    assert!(listed[1].stroke.is_some());
    rasterizer.clear();
    let still = rasterizer.rasterize(&request).expect("raster");
    assert_ne!(animated.pixels.rgba8, still.pixels.rgba8);
}

#[test]
fn a_request_over_the_frame_area_is_refused_and_a_wide_paragraph_draws() {
    let bytes = std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture font");
    let mut rasterizer = TextRasterizer::empty();
    rasterizer
        .add_font(font("Alpha2 Fixture".into(), bytes, 0))
        .expect("valid font");
    let huge = format!(
        r#"{{"text":"{}","fontSize":2048,"fill":[255,255,255,255]}}"#,
        "WWWWWWW\\n".repeat(6)
    );
    let wire: TextRequestWire = serde_json::from_str(&huge).expect("request JSON");
    assert_eq!(
        rasterizer
            .rasterize(&wire.into_request(6).expect("a request"))
            .map(|_| ()),
        Err(alpha2_text::TextRasterError::TooLarge)
    );
    let animated = huge.replacen('{', r#"{"clusterState":{"opacity":0.5},"#, 1);
    let wire: TextRequestWire = serde_json::from_str(&animated).expect("request JSON");
    let (request, states) = animated_request(wire, 7, &rasterizer).expect("states");
    assert_eq!(
        rasterizer.rasterize_animated(&request, &states).map(|_| ()),
        Err(alpha2_text::TextRasterError::TooLarge)
    );
    let paragraph = format!(
        r#"{{"text":"{}","fontSize":96,"maxWidth":3800,"fill":[255,255,255,255],
            "strokes":[{{"color":[0,0,0,255],"width":3}}]}}"#,
        "The quick brown fox jumps over the lazy dog. ".repeat(24)
    );
    let wire: TextRequestWire = serde_json::from_str(&paragraph).expect("request JSON");
    let result = rasterizer
        .rasterize(&wire.into_request(8).expect("a request"))
        .expect("raster");
    assert!(result.descriptor.texture.width >= 3_700);
}

#[test]
fn wire_runs_become_spans_over_the_request() {
    let wire: TextRequestWire = serde_json::from_str(
        r#"{"runs":[{"text":"Big "},{"text":"word","fontSize":40,"fontStyle":"italic","underline":true,"fill":[255,0,0,255]},{"text":"é"}],
            "fontSize":20,"fill":[255,255,255,255],"fontFamily":"Alpha2 Fixture","letterSpacing":1,
            "clamp":{"maxLines":2}}"#,
    )
    .expect("request JSON");
    let request = wire.into_request(9).expect("a request");
    assert_eq!(request.text, "Big wordé");
    assert_eq!(
        request.spans.len(),
        1,
        "plain runs take the request's values"
    );
    let span = &request.spans[0];
    assert_eq!(span.bytes, 4..8);
    assert_eq!(
        (span.font_size, span.letter_spacing, span.font_weight),
        (40.0, 1.0, 400)
    );
    assert_eq!(span.font_family.as_deref(), Some("Alpha2 Fixture"));
    assert_eq!(span.font_style, alpha2_text::FontStyle::Italic);
    assert!(span.underline && !span.line_through);
    assert_eq!(span.fill, alpha2_text::TextFill::Solid([255, 0, 0, 255]));
    assert_eq!(
        request.clamp,
        Some(alpha2_text::TextClamp {
            max_lines: Some(2),
            max_height: 0.0,
            ellipsis: Some("\u{2026}".into()),
        })
    );
    let plain: TextRequestWire = serde_json::from_str(
        r#"{"text":"x","fontSize":20,"fill":[0,0,0,255],"clamp":{"maxHeight":30,"ellipsis":null}}"#,
    )
    .expect("request JSON");
    assert_eq!(
        plain
            .into_request(10)
            .expect("a request")
            .clamp
            .and_then(|clamp| clamp.ellipsis),
        None
    );
    for both_or_neither in [
        r#"{"text":"x","runs":[{"text":"x"}],"fontSize":20,"fill":[0,0,0,255]}"#,
        r#"{"fontSize":20,"fill":[0,0,0,255]}"#,
    ] {
        let wire: TextRequestWire = serde_json::from_str(both_or_neither).expect("request JSON");
        assert_eq!(
            wire.into_request(11).err(),
            Some(alpha2_text::TextRasterError::InvalidRequest)
        );
    }
}
