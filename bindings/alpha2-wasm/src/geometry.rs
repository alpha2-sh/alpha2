//! The geometry an editor asks of shapes, answered by the code the renderer
//! draws them with (`alpha2-geometry`): bounds, hit tests, nearest points,
//! lengths, trims, resampling, conversion to paths, and motion paths.
//!
//! Inputs and outputs are JSON in the project document's form: a content
//! tree is a shapes generator's `items`, a path a `Path`, a geometry a
//! `ShapeGeometry`, a position an `AnimatedPoint`. Positions are normalized;
//! lengths and distances are in the pixels of a `width` by `height` picture.
//! Times are ticks, as keyframe times are.

use alpha2_geometry::kurbo::Point;
use alpha2_geometry::{CanvasSize, EvaluatedPath, query};
use alpha2_model::{AnimatedPoint, Path, PathVertex, ShapeGeometry, ShapeItem, Subpath, Vec2};
use alpha2_time::MediaTime;
use serde_json::{Value, json};

/// Points sampled per segment of a drawn motion path.
const MOTION_SAMPLES: u32 = 32;

fn canvas(width: u32, height: u32) -> Result<CanvasSize, String> {
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return Err(format!(
            "a picture is 1 to 16384 pixels on each side, not {width}x{height}"
        ));
    }
    Ok(CanvasSize::new(width, height))
}

fn aspect(size: CanvasSize) -> f64 {
    f64::from(size.width) / f64::from(size.height)
}

/// Ticks as a time: a JavaScript number that is whole and within the
/// safe-integer range (magnitude at most 2^53 - 1).
#[allow(clippy::cast_possible_truncation)]
fn time(ticks: f64) -> Result<MediaTime, String> {
    if !ticks.is_finite() || ticks.fract() != 0.0 || ticks.abs() > 9.007_199_254_740_991e15 {
        return Err(format!("a time is a whole number of ticks, not {ticks}"));
    }
    Ok(MediaTime(ticks as i64))
}

fn items(json: &str) -> Result<Vec<ShapeItem>, String> {
    let items: Vec<ShapeItem> =
        serde_json::from_str(json).map_err(|error| format!("invalid shape items: {error}"))?;
    alpha2_model::validate_shape_items(&items)
        .map_err(|issue| format!("invalid shape items: {} {}", issue.at, issue.problem))?;
    Ok(items)
}

fn path(json: &str) -> Result<EvaluatedPath, String> {
    let path: Path =
        serde_json::from_str(json).map_err(|error| format!("invalid path: {error}"))?;
    let finite = path.subpaths.iter().all(|subpath| {
        subpath.vertices.iter().all(|vertex| {
            [vertex.point, vertex.in_tangent, vertex.out_tangent]
                .iter()
                .all(|value| value.x.is_finite() && value.y.is_finite())
        })
    });
    if !finite {
        return Err("invalid path: a point or tangent is not finite".into());
    }
    Ok(EvaluatedPath::from(&path))
}

/// An evaluated path in the document's form.
#[allow(clippy::cast_possible_truncation)]
fn document_path(path: &EvaluatedPath) -> Path {
    let vector = |x: f64, y: f64| Vec2::new(x as f32, y as f32);
    Path {
        subpaths: path
            .subpaths
            .iter()
            .map(|subpath| Subpath {
                closed: subpath.closed,
                vertices: subpath
                    .vertices
                    .iter()
                    .map(|vertex| PathVertex {
                        point: vector(vertex.point.x, vertex.point.y),
                        in_tangent: vector(vertex.in_tangent.x, vertex.in_tangent.y),
                        out_tangent: vector(vertex.out_tangent.x, vertex.out_tangent.y),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn to_text(value: &impl serde::Serialize) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| error.to_string())
}

/// Each item's normalized bounds, in document order: `[{ at, bounds }]`,
/// `bounds` `{ x0, y0, x1, y1 }` or `null` for an item that shows nothing.
pub(crate) fn shape_bounds(
    items_json: &str,
    ticks: f64,
    width: u32,
    height: u32,
) -> Result<String, String> {
    let size = canvas(width, height)?;
    let evaluated = alpha2_eval::evaluate_items(&items(items_json)?, time(ticks)?, aspect(size));
    let bounds: Vec<Value> = query::shape_bounds(&evaluated, size)
        .into_iter()
        .map(|(at, bounds)| {
            json!({
                "at": at,
                "bounds": bounds.map(|rect| json!({ "x0": rect.x0, "y0": rect.y0, "x1": rect.x1, "y1": rect.y1 })),
            })
        })
        .collect();
    to_text(&bounds)
}

/// The front-most fill or stroke painting the normalized point `(x, y)`:
/// its place in the tree, like `items[0].items[2]`, or `None`.
pub(crate) fn hit_test(
    items_json: &str,
    ticks: f64,
    (width, height): (u32, u32),
    (x, y): (f64, f64),
) -> Result<Option<String>, String> {
    let size = canvas(width, height)?;
    let evaluated = alpha2_eval::evaluate_items(&items(items_json)?, time(ticks)?, aspect(size));
    Ok(query::hit_test(&evaluated, size, Point::new(x, y)))
}

/// The point of a path nearest `(x, y)`: `{ subpath, segment, t, point,
/// distance }` (`distance` in pixels), or `null` for an empty path.
pub(crate) fn nearest_point(
    path_json: &str,
    (width, height): (u32, u32),
    (x, y): (f64, f64),
) -> Result<String, String> {
    let size = canvas(width, height)?;
    let nearest = query::nearest_point(&path(path_json)?, size, Point::new(x, y)).map(|found| {
        json!({
            "subpath": found.subpath,
            "segment": found.segment,
            "t": found.t,
            "point": { "x": found.point.x, "y": found.point.y },
            "distance": found.distance,
        })
    });
    to_text(&nearest)
}

/// Each subpath's length, in pixels.
pub(crate) fn path_length(path_json: &str, width: u32, height: u32) -> Result<String, String> {
    to_text(&query::path_lengths(
        &path(path_json)?,
        canvas(width, height)?,
    ))
}

/// The path a trim item keeps of `path`: `start` to `end` (fractions of its
/// pixel length, together), shifted by `offset` turns.
pub(crate) fn trim_path(
    path_json: &str,
    (width, height): (u32, u32),
    (start, end, offset): (f64, f64, f64),
) -> Result<String, String> {
    if ![start, end, offset].iter().all(|value| value.is_finite()) {
        return Err("a trim's start, end, and offset are finite".into());
    }
    let trimmed = query::trim_path(
        &path(path_json)?,
        canvas(width, height)?,
        start.clamp(0.0, 1.0),
        end.clamp(0.0, 1.0),
        offset,
    );
    to_text(&document_path(&trimmed))
}

/// `path` with `counts[i]` vertices on subpath `i`, evenly along its length.
pub(crate) fn resample_path(
    path_json: &str,
    (width, height): (u32, u32),
    counts: &[u32],
) -> Result<String, String> {
    if counts
        .iter()
        .any(|count| *count as usize > alpha2_model::MAX_PATH_VERTICES)
    {
        return Err(format!(
            "a subpath has at most {} vertices",
            alpha2_model::MAX_PATH_VERTICES
        ));
    }
    let counts: Vec<usize> = counts.iter().map(|count| *count as usize).collect();
    let resampled = query::resample_path(&path(path_json)?, canvas(width, height)?, &counts);
    to_text(&document_path(&resampled))
}

/// The path of a rectangle, ellipse, or polystar at `ticks`, the same
/// outline as a path: the editor's "convert to path".
pub(crate) fn to_path(
    geometry_json: &str,
    ticks: f64,
    width: u32,
    height: u32,
) -> Result<String, String> {
    let geometry: ShapeGeometry = serde_json::from_str(geometry_json)
        .map_err(|error| format!("invalid geometry: {error}"))?;
    geometry
        .validate("geometry")
        .map_err(|issue| format!("invalid geometry: {} {}", issue.at, issue.problem))?;
    let size = canvas(width, height)?;
    let evaluated = alpha2_eval::evaluate_geometry(&geometry, time(ticks)?, aspect(size));
    to_text(&document_path(&query::to_path(&evaluated, size)))
}

/// The polyline a position's spatial curves draw between its keyframes,
/// normalized: `[{ x, y }]`.
pub(crate) fn motion_path(point_json: &str, width: u32, height: u32) -> Result<String, String> {
    let point: AnimatedPoint =
        serde_json::from_str(point_json).map_err(|error| format!("invalid position: {error}"))?;
    let size = canvas(width, height)?;
    let scale = size.scale();
    let pixel = |value: Vec2| {
        alpha2_geometry::kurbo::Vec2::new(
            f64::from(value.x) * scale.x,
            f64::from(value.y) * scale.y,
        )
    };
    let keys: Vec<_> = match &point {
        AnimatedPoint::Constant { value } => vec![(
            pixel(*value).to_point(),
            alpha2_geometry::kurbo::Vec2::ZERO,
            alpha2_geometry::kurbo::Vec2::ZERO,
        )],
        AnimatedPoint::Keyframes { keyframes } => keyframes
            .iter()
            .map(|keyframe| {
                (
                    pixel(keyframe.value).to_point(),
                    pixel(keyframe.out_tangent),
                    pixel(keyframe.in_tangent),
                )
            })
            .collect(),
    };
    let polyline: Vec<Value> = alpha2_geometry::spatial::motion_polyline(&keys, MOTION_SAMPLES)
        .into_iter()
        .map(|at| json!({ "x": at.x / scale.x, "y": at.y / scale.y }))
        .collect();
    to_text(&polyline)
}

/// Geometry queries on fixed inputs, as text: what the browser module must
/// answer too (`npm run test:wasm-parity`).
pub(crate) fn parity_cases() -> Vec<(&'static str, String)> {
    let star = r#"{ "type": "polystar", "kind": "star",
        "center": { "mode": "constant", "value": { "x": 0.5, "y": 0.5 } },
        "points": { "mode": "constant", "value": 5 },
        "outer_radius": { "mode": "constant", "value": 0.4 },
        "inner_radius": { "mode": "constant", "value": 0.2 },
        "outer_roundness": { "mode": "constant", "value": 0.3 },
        "rotation_degrees": { "mode": "constant", "value": 12 } }"#;
    let curve = r#"{ "subpaths": [ { "closed": false, "vertices": [
        { "point": { "x": 0.1, "y": 0.8 }, "out_tangent": { "x": 0.2, "y": -0.6 } },
        { "point": { "x": 0.9, "y": 0.2 }, "in_tangent": { "x": -0.3, "y": 0.1 } } ] } ] }"#;
    let tree = format!(
        r#"[ {{ "type": "group", "transform": {{
                "position": {{ "mode": "constant", "value": {{ "x": 0.5, "y": 0.5 }} }},
                "rotation_degrees": {{ "mode": "keyframes", "keyframes": [
                  {{ "time": 0, "value": 0, "interpolation": {{ "type": "linear" }} }},
                  {{ "time": 120000, "value": 90, "interpolation": {{ "type": "hold" }} }} ] }} }},
              "items": [ {star_at_origin}, {{ "type": "stroke", "width": {{ "mode": "constant", "value": 0.02 }},
                "paint": {{ "mode": "constant", "value": {{ "type": "solid",
                  "color": {{ "red": 1, "green": 1, "blue": 1, "alpha": 1 }} }} }} }} ] }},
             {{ "type": "path", "path": {{ "mode": "constant", "value": {curve} }} }},
             {{ "type": "trim", "start": {{ "mode": "constant", "value": 0.2 }},
               "end": {{ "mode": "constant", "value": 0.7 }} }},
             {{ "type": "fill", "rule": "evenodd", "paint": {{ "mode": "constant", "value": {{ "type": "solid",
                "color": {{ "red": 1, "green": 0, "blue": 0, "alpha": 1 }} }} }} }} ]"#,
        star_at_origin = star.replace(r#"{ "x": 0.5, "y": 0.5 }"#, r#"{ "x": 0, "y": 0 }"#),
    );
    let size = (320, 180);
    vec![
        ("shapeBounds", shape_bounds(&tree, 60_000.0, size.0, size.1).expect("bounds")),
        (
            "hitTest",
            format!(
                "{:?}",
                [(0.5, 0.5), (0.3, 0.45), (0.9, 0.9)]
                    .map(|point| hit_test(&tree, 60_000.0, size, point).expect("hit"))
            ),
        ),
        ("nearestPoint", nearest_point(curve, size, (0.4, 0.3)).expect("nearest")),
        ("pathLength", path_length(curve, size.0, size.1).expect("length")),
        ("trimPath", trim_path(curve, size, (0.25, 0.75, 0.1)).expect("trim")),
        ("resamplePath", resample_path(curve, size, &[5]).expect("resample")),
        ("toPath", to_path(star, 0.0, size.0, size.1).expect("path")),
        (
            "motionPath",
            motion_path(
                r#"{ "mode": "keyframes", "keyframes": [
                    { "time": 0, "value": { "x": 0.2, "y": 0.5 }, "interpolation": { "type": "linear" },
                      "out_tangent": { "x": 0.1, "y": -0.3 } },
                    { "time": 1000, "value": { "x": 0.8, "y": 0.5 }, "interpolation": { "type": "hold" },
                      "in_tangent": { "x": -0.1, "y": -0.3 } } ] }"#,
                size.0,
                size.1,
            )
            .expect("motion"),
        ),
    ]
}

#[cfg(target_arch = "wasm32")]
mod exports {
    use crate::web::invalid_input;
    use wasm_bindgen::prelude::*;

    /// Each item's normalized bounds; see the geometry module.
    ///
    /// # Errors
    /// Rejects items that are not a valid content tree, or a bad size or time.
    #[wasm_bindgen(js_name = shapeBounds)]
    pub fn shape_bounds(
        items_json: &str,
        time: f64,
        width: u32,
        height: u32,
    ) -> Result<String, JsValue> {
        super::shape_bounds(items_json, time, width, height).map_err(invalid_input)
    }

    /// The front-most paint under a normalized point, or `undefined`.
    ///
    /// # Errors
    /// Rejects items that are not a valid content tree, or a bad size or time.
    #[wasm_bindgen(js_name = hitTest)]
    pub fn hit_test(
        items_json: &str,
        time: f64,
        width: u32,
        height: u32,
        x: f64,
        y: f64,
    ) -> Result<Option<String>, JsValue> {
        super::hit_test(items_json, time, (width, height), (x, y)).map_err(invalid_input)
    }

    /// The point of a path nearest a normalized point, as JSON or `null`.
    ///
    /// # Errors
    /// Rejects an invalid path or size.
    #[wasm_bindgen(js_name = nearestPoint)]
    pub fn nearest_point(
        path_json: &str,
        width: u32,
        height: u32,
        x: f64,
        y: f64,
    ) -> Result<String, JsValue> {
        super::nearest_point(path_json, (width, height), (x, y)).map_err(invalid_input)
    }

    /// Each subpath's length in pixels, as a JSON array.
    ///
    /// # Errors
    /// Rejects an invalid path or size.
    #[wasm_bindgen(js_name = pathLength)]
    pub fn path_length(path_json: &str, width: u32, height: u32) -> Result<String, JsValue> {
        super::path_length(path_json, width, height).map_err(invalid_input)
    }

    /// The part of a path a trim keeps, as a path.
    ///
    /// # Errors
    /// Rejects an invalid path, size, or span.
    #[wasm_bindgen(js_name = trimPath)]
    pub fn trim_path(
        path_json: &str,
        width: u32,
        height: u32,
        start: f64,
        end: f64,
        offset: f64,
    ) -> Result<String, JsValue> {
        super::trim_path(path_json, (width, height), (start, end, offset)).map_err(invalid_input)
    }

    /// A path with the given vertex count per subpath, as a path.
    ///
    /// # Errors
    /// Rejects an invalid path or size, or a count past the vertex limit.
    #[wasm_bindgen(js_name = resamplePath)]
    pub fn resample_path(
        path_json: &str,
        width: u32,
        height: u32,
        counts: &[u32],
    ) -> Result<String, JsValue> {
        super::resample_path(path_json, (width, height), counts).map_err(invalid_input)
    }

    /// A rectangle, ellipse, or polystar as a path of the same outline.
    ///
    /// # Errors
    /// Rejects an invalid geometry, size, or time.
    #[wasm_bindgen(js_name = toPath)]
    pub fn to_path(
        geometry_json: &str,
        time: f64,
        width: u32,
        height: u32,
    ) -> Result<String, JsValue> {
        super::to_path(geometry_json, time, width, height).map_err(invalid_input)
    }

    /// The polyline a position travels between its keyframes.
    ///
    /// # Errors
    /// Rejects an invalid position or size.
    #[wasm_bindgen(js_name = motionPath)]
    pub fn motion_path(point_json: &str, width: u32, height: u32) -> Result<String, JsValue> {
        super::motion_path(point_json, width, height).map_err(invalid_input)
    }

    /// The answers of the geometry queries the native tests pin
    /// (`tests/golden/geometry_queries.json`), computed by this module.
    #[wasm_bindgen(js_name = geometryParityAnswers)]
    #[must_use]
    pub fn geometry_parity_answers() -> String {
        let answers: serde_json::Map<String, serde_json::Value> = super::parity_cases()
            .into_iter()
            .map(|(name, answer)| (name.to_owned(), serde_json::Value::String(answer)))
            .collect();
        serde_json::Value::Object(answers).to_string()
    }

    /// The vector coverage and scene hashes the native tests pin
    /// (`tests/golden/vector_coverage.json`), computed by this module.
    #[wasm_bindgen(js_name = vectorParityHashes)]
    #[must_use]
    pub fn vector_parity_hashes() -> String {
        alpha2_geometry::parity::golden_json()
    }
}

#[cfg(test)]
#[path = "geometry_tests.rs"]
mod tests;
