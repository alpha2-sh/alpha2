//! Shapes generators and shape masks rendered end to end: project JSON,
//! evaluation, the vector pass, and mask combining.

use super::*;
use crate::EngineProject;

const W: u32 = 64;
const H: u32 = 32;

fn renderer() -> Renderer {
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_builtins()).expect("the built-ins compile");
    renderer
}

fn constant(value: &str) -> String {
    format!(r#"{{ "mode": "constant", "value": {value} }}"#)
}

fn keyed(first: &str, second: &str, end: i64) -> String {
    format!(
        r#"{{ "mode": "keyframes", "keyframes": [
            {{ "time": 0, "value": {first}, "interpolation": {{ "type": "linear" }} }},
            {{ "time": {end}, "value": {second}, "interpolation": {{ "type": "hold" }} }} ] }}"#
    )
}

const WHITE: &str =
    r#"{ "type": "solid", "color": { "red": 1, "green": 1, "blue": 1, "alpha": 1 } }"#;

/// A shapes generator: a square 1/16 of the width wide whose centre moves
/// from x 0.25 to x 0.75 over the first second of its time.
fn moving_square() -> String {
    format!(
        r#"{{ "type": "shapes", "items": [
            {{ "type": "rectangle", "center": {center}, "size": {size} }},
            {{ "type": "fill", "paint": {paint} }} ] }}"#,
        center = keyed(
            r#"{ "x": 0.25, "y": 0.5 }"#,
            r#"{ "x": 0.75, "y": 0.5 }"#,
            120_000
        ),
        size = constant(r#"{ "x": 0.0625, "y": 0.125 }"#),
        paint = constant(WHITE),
    )
}

fn project(sources: &str, compositions: &str, clip: &str) -> EngineProject {
    let json = format!(
        r#"{{
          "schema_version": 1, "id": "p", "name": "P",
          "settings": {{ "width": {W}, "height": {H}, "frame_rate": {{ "num": 30, "den": 1 }}, "sample_rate": 48000 }},
          "sources": [
            {{ "id": "white", "kind": "generated", "generator": {{ "type": "fill", "paint": {WHITE} }} }},
            {{ "id": "square", "kind": "generated", "generator": {square} }} {sources}
          ],
          "compositions": [ {compositions} ],
          "timeline": {{ "tracks": [
            {{ "id": "t1", "name": "V", "kind": "video", "clips": [ {clip} ] }}
          ] }}
        }}"#,
        square = moving_square(),
    );
    let project = alpha2_model::Project::from_json(&json).expect("a valid project");
    let saved = serde_json::to_string(&project).expect("serializes");
    assert_eq!(
        alpha2_model::Project::from_json(&saved).expect("reloads"),
        project
    );
    EngineProject::new(project).expect("indexed")
}

fn clip(source: &str, extra: &str) -> String {
    format!(
        r#"{{ "id": "c", "source_id": "{source}", "timeline_range": {{ "start": 60000, "end": 180000 }},
              "source_range": {{ "start": 0, "end": 120000 }} {extra} }}"#
    )
}

fn pixels(renderer: &Renderer, project: &EngineProject, time: i64) -> Vec<u8> {
    let frame = project.evaluate(MediaTime(time)).expect("evaluates");
    let gpu = gpu();
    let scope = gpu.device().push_error_scope(wgpu::ErrorFilter::Validation);
    let output = renderer
        .render_frame(RenderSize::new(W, H).expect("size"), &frame, &MissingFrames)
        .expect("frame renders");
    let error = pollster::block_on(scope.pop());
    assert!(error.is_none(), "{error:?}");
    assert!(output.report.is_complete(), "{:?}", output.report);
    read_rgba8(renderer, &output.texture.expect("a complete frame"))
}

fn red_at(pixels: &[u8], x: u32, y: u32) -> u8 {
    pixels[((y * W + x) * 4) as usize]
}

/// The columns of row `y` that are lit, as a range of x.
fn lit(pixels: &[u8], y: u32) -> std::ops::Range<u32> {
    let lit: Vec<u32> = (0..W).filter(|&x| red_at(pixels, x, y) > 128).collect();
    lit.first().copied().unwrap_or(0)..lit.last().map_or(0, |x| x + 1)
}

#[test]
fn a_shapes_clip_animates_in_its_source_time() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let forward = project("", "", &clip("square", ""));
    // Timeline 60000 is source time 0: the square at x 0.25, 4 px wide about pixel 16.
    assert_eq!(lit(&pixels(&renderer, &forward, 60_000), 16), 14..18);
    // Half a second on, halfway: about pixel 32.
    assert_eq!(lit(&pixels(&renderer, &forward, 120_000), 16), 30..34);
    // Reversed, the clip starts where the source ends.
    let reversed = project(
        "",
        "",
        &clip("square", r#", "time_map": { "type": "reverse" }"#),
    );
    let late = lit(&pixels(&renderer, &reversed, 60_000), 16);
    assert!(late.start >= 44, "{late:?}");
}

#[test]
fn a_generator_node_runs_in_composition_time_and_a_media_node_from_its_start() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let node = |op: &str| {
        format!(r#"{{ "id": "g", "nodes": [ {{ "id": "n", "op": {op} }} ], "output": "n" }}"#)
    };
    let generator = node(&format!(
        r#"{{ "type": "generator", "generator": {} }}"#,
        moving_square()
    ));
    let shown = project(
        "",
        &generator,
        &clip("white", r#", "composition": { "id": "g" }"#),
    );
    // Composition time is the clip's: 60000 into the timeline is composition time 0.
    assert_eq!(lit(&pixels(&renderer, &shown, 60_000), 16), 14..18);
    // A media node reading the shapes source half a second in starts there.
    let media = node(r#"{ "type": "media", "source_id": "square", "start": 60000 }"#);
    let started = project(
        "",
        &media,
        &clip("white", r#", "composition": { "id": "g" }"#),
    );
    assert_eq!(lit(&pixels(&renderer, &started, 60_000), 16), 30..34);
}

#[test]
fn masks_cut_by_paths_with_their_rule_combine_and_feather() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let square = |x0: f32, y0: f32, x1: f32, y1: f32| {
        format!(
            r#"{{ "closed": true, "vertices": [
                {{ "point": {{ "x": {x0}, "y": {y0} }} }}, {{ "point": {{ "x": {x1}, "y": {y0} }} }},
                {{ "point": {{ "x": {x1}, "y": {y1} }} }}, {{ "point": {{ "x": {x0}, "y": {y1} }} }} ] }}"#
        )
    };
    let ring = format!(
        r#"{{ "subpaths": [ {}, {} ] }}"#,
        square(0.25, 0.0, 0.75, 1.0),
        square(0.375, 0.25, 0.625, 0.75)
    );
    let masked = |rule: &str, feather: f32| {
        project(
            "",
            "",
            &clip(
                "white",
                &format!(
                    r#", "masks": [ {{ "shape": {{ "type": "path", "path": {} }}, "fill_rule": "{rule}",
                        "feather": {} }} ]"#,
                    constant(&ring),
                    constant(&feather.to_string())
                ),
            ),
        )
    };
    let evenodd = pixels(&renderer, &masked("evenodd", 0.0), 90_000);
    assert_eq!(red_at(&evenodd, 20, 4), 255, "in the ring");
    assert_eq!(red_at(&evenodd, 32, 16), 0, "in its hole");
    assert_eq!(red_at(&evenodd, 4, 16), 0, "outside");
    let nonzero = pixels(&renderer, &masked("nonzero", 0.0), 90_000);
    assert_eq!(red_at(&nonzero, 32, 16), 255, "wound twice, inside");
    // Feathered, the edge at x 16 ramps over 0.8 of the feather (a quarter of 32 px): its 10
    // to 90 percent coverage 6.4 px apart, centred on the edge. White composited over black in
    // linear light stores the coverage sRGB-encoded.
    let soft = pixels(&renderer, &masked("nonzero", 0.25), 90_000);
    let coverage: Vec<f64> = (0..32)
        .map(|x| {
            let encoded = f64::from(red_at(&soft, x, 16)) / 255.0;
            if encoded <= 0.040_45 {
                encoded / 12.92
            } else {
                libm::pow((encoded + 0.055) / 1.055, 2.4)
            }
        })
        .collect();
    assert!(
        coverage.windows(2).all(|pair| pair[0] <= pair[1]),
        "{coverage:?}"
    );
    #[allow(clippy::cast_precision_loss)]
    let crossing = |level: f64| {
        (1..32).find(|x| coverage[*x] >= level).map_or(32.0, |x| {
            let (a, b) = (coverage[x - 1], coverage[x]);
            (x - 1) as f64 + 0.5 + (level - a) / (b - a)
        })
    };
    let (low, middle, high) = (crossing(0.1), crossing(0.5), crossing(0.9));
    assert!((middle - 16.0).abs() < 0.3, "{middle}: {coverage:?}");
    assert!(
        (high - low - 6.4).abs() < 0.8,
        "{low}..{high}: {coverage:?}"
    );

    // Eight masks in every mode: added stripes, one subtracted, one intersected, one inverted.
    let rect = |cx: f32, w: f32, extra: &str| {
        format!(
            r#"{{ "shape": {{ "type": "rectangle", "center": {}, "size": {} }} {extra} }}"#,
            constant(&format!(r#"{{ "x": {cx}, "y": 0.5 }}"#)),
            constant(&format!(r#"{{ "x": {w}, "y": 1 }}"#))
        )
    };
    let masks = [
        rect(0.0625, 0.125, ""),
        rect(0.3125, 0.125, ""),
        rect(0.5625, 0.125, ""),
        rect(0.8125, 0.125, ""),
        rect(0.5, 0.0625, r#", "mode": "add""#),
        rect(0.3125, 0.0625, r#", "mode": "subtract""#),
        rect(0.5, 0.875, r#", "mode": "intersect""#),
        rect(0.5, 0.5, r#", "mode": "subtract", "invert": true"#),
    ]
    .join(", ");
    let stack = project(
        "",
        "",
        &clip("white", &format!(r#", "masks": [ {masks} ]"#)),
    );
    let image = pixels(&renderer, &stack, 90_000);
    let lit_columns: Vec<u32> = (0..W).filter(|&x| red_at(&image, x, 16) > 128).collect();
    // Stripes at x 0..8, 16..24, 32..40 and 48..56, widened by 30..34, holed at 18..22,
    // intersected with 4..60, then everything outside 16..48 subtracted: what is left of the
    // second stripe and the widened third.
    let expected: Vec<u32> = (16..18).chain(22..24).chain(30..40).collect();
    assert_eq!(lit_columns, expected);
}

#[test]
fn a_clip_moves_along_its_spatial_curve() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let position = |tangents: bool| {
        let (out_tangent, in_tangent) = if tangents {
            (
                r#", "out_tangent": { "x": 0, "y": -0.4 }"#,
                r#", "in_tangent": { "x": 0, "y": -0.4 }"#,
            )
        } else {
            ("", "")
        };
        format!(
            r#"{{ "mode": "keyframes", "keyframes": [
                {{ "time": 60000, "value": {{ "x": 0.25, "y": 0.5 }}, "interpolation": {{ "type": "linear" }} {out_tangent} }},
                {{ "time": 180000, "value": {{ "x": 0.75, "y": 0.5 }}, "interpolation": {{ "type": "hold" }} {in_tangent} }} ] }}"#
        )
    };
    let moving = |tangents: bool| {
        project(
            "",
            "",
            &clip(
                "white",
                &format!(
                    r#", "visual": {{ "position": {}, "anchor": {{ "x": 0.5, "y": 0.5 }},
                        "scale": {}, "rotation_degrees": {}, "opacity": {} }}"#,
                    position(tangents),
                    constant(r#"{ "x": 0.125, "y": 0.25 }"#),
                    constant("0"),
                    constant("1")
                ),
            ),
        )
    };
    // Halfway in time, a straight path is at the middle of the frame, a curved one above it.
    let straight = pixels(&renderer, &moving(false), 120_000);
    assert!(red_at(&straight, 32, 16) > 128);
    let curved = pixels(&renderer, &moving(true), 120_000);
    assert_eq!(
        red_at(&curved, 32, 16),
        0,
        "the curve passes above the middle"
    );
    let top: Vec<u32> = (0..H).filter(|&y| red_at(&curved, 32, y) > 128).collect();
    assert!(!top.is_empty() && top[top.len() - 1] < 12, "{top:?}");
}
