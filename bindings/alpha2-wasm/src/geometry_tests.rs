use super::*;

const SQUARE: &str = r#"{ "subpaths": [ { "closed": true, "vertices": [
    { "point": { "x": 0.25, "y": 0.25 } }, { "point": { "x": 0.75, "y": 0.25 } },
    { "point": { "x": 0.75, "y": 0.75 } }, { "point": { "x": 0.25, "y": 0.75 } } ] } ] }"#;

fn items() -> String {
    format!(
        r#"[ {{ "type": "path", "path": {{ "mode": "constant", "value": {SQUARE} }} }},
             {{ "type": "fill", "paint": {{ "mode": "constant", "value": {{ "type": "solid",
                "color": {{ "red": 1, "green": 0, "blue": 0, "alpha": 1 }} }} }} }} ]"#
    )
}

#[test]
fn bounds_and_hits_answer_in_normalized_coordinates() {
    let bounds: Value =
        serde_json::from_str(&shape_bounds(&items(), 0.0, 200, 100).expect("bounds"))
            .expect("JSON");
    assert_eq!(bounds[0]["at"], "items[0]");
    assert_eq!(bounds[0]["bounds"]["x0"], 0.25);
    assert_eq!(bounds[1]["at"], "items[1]");
    assert_eq!(
        hit_test(&items(), 0.0, (200, 100), (0.5, 0.5)).expect("hit"),
        Some("items[1]".into())
    );
    assert_eq!(
        hit_test(&items(), 0.0, (200, 100), (0.1, 0.5)).expect("miss"),
        None
    );
}

#[test]
fn lengths_trims_and_nearest_points_are_in_pixels() {
    // 100 x 50 pixels a side on a 200 x 100 picture.
    let lengths: Vec<f64> =
        serde_json::from_str(&path_length(SQUARE, 200, 100).expect("lengths")).expect("JSON");
    assert_eq!(lengths, [300.0]);
    let nearest: Value =
        serde_json::from_str(&nearest_point(SQUARE, (200, 100), (0.5, 0.1)).expect("nearest"))
            .expect("JSON");
    assert_eq!(nearest["segment"], 0);
    assert!((nearest["distance"].as_f64().expect("distance") - 15.0).abs() < 1e-6);
    let trimmed: Path =
        serde_json::from_str(&trim_path(SQUARE, (200, 100), (0.0, 0.5, 0.0)).expect("trimmed"))
            .expect("a path");
    let last = trimmed.subpaths[0].vertices.last().expect("vertices").point;
    // Half of 300 px from the top-left corner: along the top (100) and down the right (50).
    assert_eq!((last.x, last.y), (0.75, 0.75));
    let resampled: Path =
        serde_json::from_str(&resample_path(SQUARE, (200, 100), &[6]).expect("resampled"))
            .expect("a path");
    assert_eq!(resampled.subpaths[0].vertices.len(), 6);
}

#[test]
fn a_rectangle_converts_to_a_path_of_its_outline() {
    let rectangle = r#"{ "type": "rectangle",
        "center": { "mode": "constant", "value": { "x": 0.5, "y": 0.5 } },
        "size": { "mode": "constant", "value": { "x": 0.5, "y": 0.5 } } }"#;
    let converted: Path =
        serde_json::from_str(&to_path(rectangle, 0.0, 200, 100).expect("a path")).expect("JSON");
    let points: Vec<(f32, f32)> = converted.subpaths[0]
        .vertices
        .iter()
        .map(|vertex| (vertex.point.x, vertex.point.y))
        .collect();
    assert_eq!(
        points,
        [(0.75, 0.25), (0.75, 0.75), (0.25, 0.75), (0.25, 0.25)]
    );
    assert!(converted.subpaths[0].closed);
}

#[test]
fn a_motion_path_follows_the_tangents() {
    let point = r#"{ "mode": "keyframes", "keyframes": [
        { "time": 0, "value": { "x": 0.25, "y": 0.5 }, "interpolation": { "type": "linear" },
          "out_tangent": { "x": 0, "y": -0.4 } },
        { "time": 1000, "value": { "x": 0.75, "y": 0.5 }, "interpolation": { "type": "hold" },
          "in_tangent": { "x": 0, "y": -0.4 } } ] }"#;
    let polyline: Vec<Value> =
        serde_json::from_str(&motion_path(point, 200, 100).expect("a polyline")).expect("JSON");
    assert_eq!(polyline.len(), 33);
    let middle = &polyline[16];
    assert!((middle["x"].as_f64().expect("x") - 0.5).abs() < 1e-9);
    assert!(middle["y"].as_f64().expect("y") < 0.3);
}

#[test]
fn invalid_input_is_refused_with_its_reason() {
    assert!(shape_bounds("[{\"type\":\"repeater\"}]", 0.0, 10, 10).is_err());
    assert!(path_length(SQUARE, 0, 10).is_err());
    assert!(shape_bounds(&items(), 0.5, 10, 10).is_err());
    assert!(trim_path(SQUARE, (10, 10), (f64::NAN, 1.0, 0.0)).is_err());
}

/// The pinned answers of [`parity_cases`]. Run with `ALPHA2_UPDATE_GOLDENS=1`
/// to re-pin after an intended change.
#[test]
fn geometry_queries_match_the_pinned_answers() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/geometry_queries.json"
    );
    let actual: serde_json::Map<String, Value> = parity_cases()
        .into_iter()
        .map(|(name, answer)| (name.to_owned(), Value::String(answer)))
        .collect();
    let actual = Value::Object(actual);
    if std::env::var_os("ALPHA2_UPDATE_GOLDENS").as_deref() == Some(std::ffi::OsStr::new("1")) {
        let text = serde_json::to_string_pretty(&actual).expect("JSON") + "\n";
        std::fs::write(path, text).expect("write the golden");
        return;
    }
    let pinned: Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("golden")).expect("JSON");
    assert_eq!(actual, pinned);
}
