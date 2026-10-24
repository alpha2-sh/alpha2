//! Layout generators rendered end to end: project JSON, evaluation, the
//! text host's resolve, and the graph built from the paint list.

use super::*;
use crate::{EngineProject, FramesWithText, Player, QualityProfile, TextTextures};

const W: u32 = 320;
const H: u32 = 180;

fn renderer() -> Renderer {
    let mut renderer = Renderer::new(gpu());
    pollster::block_on(renderer.register_builtins()).expect("the built-ins compile");
    renderer
}

fn texts() -> TextTextures {
    let mut texts = TextTextures::new();
    texts
        .add_font(
            "Alpha2 Fixture",
            std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture font"),
            0,
        )
        .expect("font");
    texts
}

fn c(value: &str) -> String {
    format!(r#"{{ "mode": "constant", "value": {value} }}"#)
}

fn len(value: f32) -> String {
    c(&value.to_string())
}

fn solid(red: f32, green: f32, blue: f32, alpha: f32) -> String {
    c(&format!(
        r#"{{ "type": "solid", "color": {{ "red": {red}, "green": {green}, "blue": {blue}, "alpha": {alpha} }} }}"#
    ))
}

fn text(content: &str, size: f32, extra: &str) -> String {
    format!(
        r#"{{ "text": {content}, "font_size": {size} {extra} }}"#,
        content = c(&format!("{content:?}")),
        size = c(&size.to_string()),
    )
}

/// A project of `W` by `H` whose `layout` source is `generator`, shown by one clip over two
/// seconds on a mid-grey backdrop.
fn project(generator: &str, compositions: &str, clip_extra: &str) -> EngineProject {
    let json = format!(
        r#"{{
          "schema_version": 1, "id": "p", "name": "P",
          "settings": {{ "width": {W}, "height": {H}, "frame_rate": {{ "num": 30, "den": 1 }}, "sample_rate": 48000 }},
          "sources": [
            {{ "id": "grey", "kind": "generated", "generator": {{ "type": "fill", "paint": {{ "type": "solid",
               "color": {{ "red": 0.4, "green": 0.4, "blue": 0.4, "alpha": 1 }} }} }} }},
            {{ "id": "layout", "kind": "generated", "generator": {generator} }}
          ],
          "compositions": [ {compositions} ],
          "timeline": {{ "tracks": [
            {{ "id": "t0", "name": "Back", "kind": "video", "clips": [
              {{ "id": "back", "source_id": "grey", "timeline_range": {{ "start": 0, "end": 240000 }},
                 "source_range": {{ "start": 0, "end": 240000 }} }} ] }},
            {{ "id": "t1", "name": "Front", "kind": "video", "clips": [
              {{ "id": "front", "source_id": "layout", "timeline_range": {{ "start": 0, "end": 240000 }},
                 "source_range": {{ "start": 0, "end": 240000 }} {clip_extra} }} ] }}
          ] }}
        }}"#
    );
    let project = alpha2_model::Project::from_json(&json).expect("a valid project");
    assert!(project.draws_text());
    let saved = serde_json::to_string(&project).expect("serializes");
    assert_eq!(
        alpha2_model::Project::from_json(&saved).expect("reloads"),
        project
    );
    EngineProject::new(project).expect("indexed")
}

fn layout(root: &str) -> String {
    format!(r#"{{ "type": "layout", "root": {root} }}"#)
}

/// The frame at `ticks`, its texts and layouts prepared first.
fn render(
    renderer: &Renderer,
    texts: &mut TextTextures,
    project: &EngineProject,
    ticks: i64,
) -> Vec<u8> {
    let size = RenderSize::new(W, H).expect("size");
    let frame = project.evaluate(MediaTime(ticks)).expect("evaluates");
    texts.prepare(renderer, &frame, size).expect("prepared");
    let gpu = gpu();
    let scope = gpu.device().push_error_scope(wgpu::ErrorFilter::Validation);
    let output = renderer
        .render_frame(size, &frame, &FramesWithText::new(&MissingFrames, texts))
        .expect("frame renders");
    let error = pollster::block_on(scope.pop());
    assert!(error.is_none(), "{error:?}");
    read_rgba8(renderer, &output.texture.expect("a complete frame"))
}

fn at(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
    let index = ((y * W + x) * 4) as usize;
    pixels[index..index + 4].try_into().expect("a pixel")
}

/// Images side by side, each `W` by `H`.
fn side_by_side(images: &[&[u8]]) -> Vec<u8> {
    let row = (W * 4) as usize;
    (0..H as usize)
        .flat_map(|y| {
            images
                .iter()
                .flat_map(move |image| image[y * row..(y + 1) * row].iter().copied())
        })
        .collect()
}

fn title_card() -> String {
    layout(&format!(
        r#"{{
          "justify_content": "center", "align_items": "center",
          "background": {{ "mode": "constant", "value": {{ "type": "linear_gradient",
            "start": {{ "x": 0, "y": 0 }}, "end": {{ "x": 1, "y": 1 }},
            "stops": [ {{ "offset": 0, "color": {{ "red": 0.05, "green": 0.1, "blue": 0.3, "alpha": 1 }} }},
                       {{ "offset": 1, "color": {{ "red": 0.3, "green": 0.05, "blue": 0.2, "alpha": 1 }} }} ] }} }},
          "children": [ {{
            "flex_direction": "column", "align_items": "center", "gap": {gap},
            "padding": {padding},
            "background": {card},
            "border": {{ "width": {border_width}, "paint": {border} }},
            "border_radius": {radius},
            "children": [
              {{ "flex_direction": "row", "align_items": "center", "gap": {gap}, "children": [
                {{ "width": {icon}, "height": {icon}, "shape": {{ "items": [
                  {{ "type": "ellipse", "center": {centre}, "size": {whole} }},
                  {{ "type": "fill", "paint": {{ "mode": "constant", "value": {{ "type": "radial_gradient",
                    "center": {{ "x": 0.5, "y": 0.5 }}, "radius": 0.5,
                    "stops": [ {{ "offset": 0, "color": {{ "red": 1, "green": 0.9, "blue": 0.2, "alpha": 1 }} }},
                               {{ "offset": 1, "color": {{ "red": 1, "green": 0.4, "blue": 0, "alpha": 1 }} }} ] }} }} }} ] }} }},
                {title}
              ] }},
              {subtitle}
            ]
          }} ]
        }}"#,
        gap = c("0.03"),
        padding = c(r#"{ "top": 0.05, "right": 0.08, "bottom": 0.05, "left": 0.08 }"#),
        card = solid(0.95, 0.95, 0.92, 1.0),
        border_width = c("0.012"),
        border = solid(0.9, 0.5, 0.1, 1.0),
        radius = c("0.06"),
        icon = len(0.12),
        centre = c(r#"{ "x": 0.5, "y": 0.5 }"#),
        whole = c(r#"{ "x": 1, "y": 1 }"#),
        title = text(
            "Title",
            0.14,
            r#", "color": { "red": 0.1, "green": 0.1, "blue": 0.15, "alpha": 1 }"#
        ),
        subtitle = text(
            "a centred subtitle",
            0.07,
            r#", "color": { "red": 0.3, "green": 0.3, "blue": 0.35, "alpha": 1 }"#
        ),
    ))
}

/// The lower third of the plan's example: a card that fades and rises in over 0.3 s.
fn lower_third() -> String {
    let ease = r#"{ "type": "cubic_bezier", "x1": 0.2, "y1": 0.0, "x2": 0.0, "y2": 1.0 }"#;
    layout(&format!(
        r#"{{
          "flex_direction": "column", "justify_content": "end", "align_items": "start",
          "padding": {padding},
          "children": [ {{
            "flex_direction": "row", "gap": {gap},
            "padding": {card_padding},
            "background": {card},
            "border_radius": {radius},
            "max_width": {{ "mode": "constant", "value": "60%" }},
            "opacity": {{ "mode": "keyframes", "keyframes": [
              {{ "time": 0, "value": 0, "interpolation": {ease} }},
              {{ "time": 36000, "value": 1, "interpolation": {{ "type": "hold" }} }} ] }},
            "translate": {{ "mode": "keyframes", "keyframes": [
              {{ "time": 0, "value": {{ "x": 0, "y": 0.03 }}, "interpolation": {ease} }},
              {{ "time": 36000, "value": {{ "x": 0, "y": 0 }}, "interpolation": {{ "type": "hold" }} }} ] }},
            "children": [
              {{ "width": {bar}, "flex_shrink": {{ "mode": "constant", "value": 0 }}, "background": {accent},
                 "border_radius": {bar_radius} }},
              {{ "flex_direction": "column", "gap": {inner_gap}, "children": [
                {name},
                {role}
              ] }}
            ]
          }} ]
        }}"#,
        padding = c(r#"{ "top": 0.06, "right": 0.06, "bottom": 0.08, "left": 0.06 }"#),
        gap = c("0.016"),
        card_padding = c(r#"{ "top": 0.022, "right": 0.03, "bottom": 0.022, "left": 0.022 }"#),
        card = solid(0.06, 0.07, 0.09, 0.92),
        radius = c("0.012"),
        bar = len(0.005),
        accent = solid(1.0, 0.62, 0.1, 1.0),
        bar_radius = c("0.0025"),
        inner_gap = c("0.006"),
        name = text("Ada Lovelace", 0.07, ""),
        role = text(
            "Wrote the first published algorithm for a machine, 1843",
            0.045,
            r#", "color": { "red": 0.8, "green": 0.82, "blue": 0.86, "alpha": 1 }"#
        ),
    ))
}

/// A panel: a wrapped paragraph aligned to its end, a box that clips a child too big for it, and
/// a rotated absolute badge.
fn text_panel() -> String {
    layout(&format!(
        r#"{{
          "flex_direction": "row", "gap": {gap}, "padding": {padding}, "align_items": "start",
          "children": [
            {{ "width": {column}, "padding": {inner}, "background": {paper}, "border_radius": {radius},
               "children": [ {paragraph} ] }},
            {{ "width": {clip}, "height": {clip}, "overflow": "hidden", "border_radius": {radius},
               "background": {blue}, "border": {{ "width": {thin}, "paint": {white} }},
               "children": [ {{ "width": {big}, "height": {big}, "flex_shrink": {zero}, "background": {red},
                                "margin": {offset} }} ] }},
            {{ "position": "absolute",
               "inset": {{ "right": {edge}, "bottom": {edge} }},
               "padding": {badge_padding}, "background": {green}, "border_radius": {pill},
               "rotation_degrees": {tilt},
               "children": [ {badge} ] }}
          ]
        }}"#,
        gap = c("0.04"),
        padding = c(r#"{ "top": 0.05, "right": 0.05, "bottom": 0.05, "left": 0.05 }"#),
        column = len(0.9),
        inner = c(r#"{ "top": 0.03, "right": 0.03, "bottom": 0.03, "left": 0.03 }"#),
        paper = solid(1.0, 1.0, 1.0, 0.9),
        radius = c("0.03"),
        paragraph = text(
            "Layout resolves where text is drawn, so a paragraph wraps the same in preview and export.",
            0.06,
            r#", "color": { "red": 0, "green": 0, "blue": 0, "alpha": 1 }, "text_align": "end", "line_height": 1.1"#
        ),
        clip = len(0.4),
        blue = solid(0.1, 0.3, 0.8, 1.0),
        thin = c("0.01"),
        white = solid(1.0, 1.0, 1.0, 1.0),
        big = len(0.6),
        zero = c("0"),
        red = solid(0.9, 0.1, 0.1, 1.0),
        offset = c(r#"{ "left": 0.1, "top": 0.1 }"#),
        edge = len(0.05),
        badge_padding = c(r#"{ "top": 0.01, "right": 0.03, "bottom": 0.01, "left": 0.03 }"#),
        green = solid(0.1, 0.6, 0.3, 1.0),
        pill = c("0.5"),
        tilt = c("-8"),
        badge = text("NEW", 0.06, ""),
    ))
}

#[test]
fn layout_goldens() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let mut texts = texts();
    let title = render(&renderer, &mut texts, &project(&title_card(), "", ""), 0);
    alpha2_golden_support::assert_rgba8_golden("layout_title_card", (W, H), &title, 2);
    let third = project(&lower_third(), "", "");
    let mid = render(&renderer, &mut texts, &third, 15_000);
    let end = render(&renderer, &mut texts, &third, 60_000);
    alpha2_golden_support::assert_rgba8_golden(
        "layout_lower_third",
        (W * 2, H),
        &side_by_side(&[&mid, &end]),
        2,
    );
    let panel = render(&renderer, &mut texts, &project(&text_panel(), "", ""), 0);
    alpha2_golden_support::assert_rgba8_golden("layout_text_panel", (W, H), &panel, 2);
}

/// A grid dashboard in the authoring forms: pixel lengths on the 320x180 canvas, hex colors, the
/// padding shorthand, text style inherited from the grid, `fr` and `minmax` tracks, a spanning
/// item and an item aligned in its area.
fn grid_dashboard() -> String {
    layout(&format!(
        r##"{{
          "display": "grid", "grid_template_columns": "1fr 2fr 60px",
          "grid_template_rows": "minmax(40px, auto) 1fr", "gap": {gap}, "padding": {padding},
          "background": {back}, "font_size": {font}, "color": "#f5f5f0", "line_height": 1.1,
          "children": [
            {{ "text": {revenue}, "background": {card}, "padding": {inner}, "border_radius": {radius} }},
            {{ "text": {margin}, "background": {card}, "padding": {inner}, "border_radius": {radius},
               "color": "#ffb020", "text_align": "end" }},
            {{ "grid_row": "1 / 3", "grid_column": 3, "background": {bar}, "border_radius": {radius} }},
            {{ "grid_column": "1 / span 2", "background": {card}, "border_radius": {radius},
               "display": "grid", "grid_template_columns": "repeat(4, 1fr)", "align_items": "end",
               "padding": {inner}, "column_gap": {inner_gap},
               "children": [
                 {{ "height": {h1}, "background": {blue} }},
                 {{ "height": {h2}, "background": {blue} }},
                 {{ "height": {h3}, "background": {blue} }},
                 {{ "height": {h4}, "justify_self": "center", "width": {half}, "background": {pink} }}
               ] }}
          ]
        }}"##,
        gap = c(r#""8px""#),
        padding = c(r#""12px 16px""#),
        back = c(r##""#101418""##),
        font = c(r#""14px""#),
        revenue = c(r#""Revenue 4.2M""#),
        card = c(r##""#1e2230""##),
        inner = c(r#""6px 8px""#),
        radius = c(r#""6px""#),
        margin = c(r#""Margin up four points""#),
        bar = c(r##""#2b5c3a""##),
        inner_gap = c(r#""6px""#),
        h1 = c(r#""30%""#),
        h2 = c(r#""60%""#),
        h3 = c(r#""45%""#),
        h4 = c(r#""80%""#),
        half = c(r#""50%""#),
        blue = c(r##""#4c8dff""##),
        pink = c(r##""#ff5c7a""##),
    ))
}

#[test]
fn layout_grid_golden() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let mut texts = texts();
    let grid = render(
        &renderer,
        &mut texts,
        &project(&grid_dashboard(), "", ""),
        0,
    );
    alpha2_golden_support::assert_rgba8_golden("layout_grid", (W, H), &grid, 2);
}

/// Two overlapping half-white squares in a box: `group` puts the opacity on the box, else on
/// each square.
fn overlapping(group: bool) -> String {
    let half = c("0.5");
    let square = |left: f32| {
        format!(
            r#"{{ "position": "absolute", "inset": {{ "left": {left}, "top": {top} }}, "width": {side},
                 "height": {side}, "background": {white} {opacity} }}"#,
            left = len(left),
            top = len(0.2),
            side = len(0.4),
            white = solid(1.0, 1.0, 1.0, 1.0),
            opacity = if group {
                String::new()
            } else {
                format!(r#", "opacity": {half}"#)
            },
        )
    };
    layout(&format!(
        r#"{{ "children": [ {{ "width": {all}, "height": {all} {opacity},
              "children": [ {a}, {b} ] }} ] }}"#,
        all = c(r#""100%""#),
        opacity = if group {
            format!(r#", "opacity": {half}"#)
        } else {
            String::new()
        },
        a = square(0.2),
        b = square(0.4),
    ))
}

#[test]
fn group_opacity_differs_from_fading_each_layer_where_they_overlap() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let mut texts = texts();
    let group = render(
        &renderer,
        &mut texts,
        &project(&overlapping(true), "", ""),
        0,
    );
    let each = render(
        &renderer,
        &mut texts,
        &project(&overlapping(false), "", ""),
        0,
    );
    // Where only one square lies (x 40..72 at y 90), both are the same half-white over grey.
    assert_rgba8_near(&at(&group, 50, 90), &at(&each, 50, 90), 1);
    // Where both lie (x 72..112), the group is still half white; the layers fade one over another.
    let (one, two) = (at(&group, 90, 90), at(&each, 90, 90));
    assert_rgba8_near(&one, &at(&group, 50, 90), 1);
    assert!(two[0] > one[0] + 10, "{one:?} {two:?}");
}

#[test]
fn a_collapsed_group_matches_one_that_is_not() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let mut texts = texts();
    // A moved, scaled, half-opaque card with one inner box. Without a clip it collapses into one
    // vector layer; with a clip that keeps everything, it is a group of two.
    let card = |overflow: &str| {
        layout(&format!(
            r#"{{ "justify_content": "center", "align_items": "center", "children": [ {{
                 "width": {w}, "height": {h}, "padding": {pad}, "background": {blue},
                 "opacity": {half}, "translate": {shift}, "scale": {grow}, "rotation_degrees": {turn}
                 {overflow},
                 "children": [ {{ "flex_grow": {one}, "background": {red} }} ] }} ] }}"#,
            w = len(0.8),
            h = len(0.5),
            pad = c(r#"{ "top": 0.1, "right": 0.1, "bottom": 0.1, "left": 0.1 }"#),
            blue = solid(0.1, 0.3, 0.8, 1.0),
            half = c("0.6"),
            shift = c(r#"{ "x": 0.1, "y": -0.05 }"#),
            grow = c(r#"{ "x": 1.2, "y": 0.9 }"#),
            turn = c("10"),
            one = c("1"),
            red = solid(0.9, 0.1, 0.1, 1.0),
        ))
    };
    let collapsed = render(&renderer, &mut texts, &project(&card(""), "", ""), 0);
    let grouped = render(
        &renderer,
        &mut texts,
        &project(&card(r#", "overflow": "hidden""#), "", ""),
        0,
    );
    assert_rgba8_near(&collapsed, &grouped, 3);
    assert_ne!(at(&collapsed, 160, 90), at(&collapsed, 5, 5));
}

#[test]
fn hidden_overflow_clips_children_and_text() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let mut texts = texts();
    let panel = |overflow: &str| {
        layout(&format!(
            r#"{{ "align_items": "start", "padding": {pad}, "children": [ {{
                 "width": {side}, "height": {side}, "flex_direction": "column" {overflow},
                 "children": [
                   {{ "width": {wide}, "height": {half}, "flex_shrink": {zero}, "background": {red} }},
                   {{ "width": {wide}, "flex_shrink": {zero}, "children": [ {word} ] }} ] }} ] }}"#,
            pad = c(r#"{ "top": 0.1, "left": 0.1 }"#),
            side = len(0.4),
            wide = len(1.2),
            half = len(0.2),
            zero = c("0"),
            red = solid(0.9, 0.1, 0.1, 1.0),
            word = text("Overflowing words run on", 0.12, ""),
        ))
    };
    let shown = render(&renderer, &mut texts, &project(&panel(""), "", ""), 0);
    let clipped = render(
        &renderer,
        &mut texts,
        &project(&panel(r#", "overflow": "hidden""#), "", ""),
        0,
    );
    let grey = at(&shown, 2, 2);
    // The box spans 18..90 on both axes: inside it the two agree, past it only the visible one
    // draws.
    assert_eq!(at(&shown, 50, 30), at(&clipped, 50, 30));
    assert!(at(&shown, 150, 30)[0] > 180, "the red box runs past");
    assert_eq!(at(&clipped, 150, 30), grey);
    let past = |pixels: &[u8]| {
        (95..W)
            .flat_map(|x| (54..90).map(move |y| (x, y)))
            .filter(|&(x, y)| at(pixels, x, y) != grey)
            .count()
    };
    assert!(past(&shown) > 100, "the text runs past the box");
    assert_eq!(past(&clipped), 0);
}

#[test]
fn a_batched_fill_matches_one_drawn_alone() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let mut texts = texts();
    // A background, a text, and a later background clear of the text: the later one joins the
    // first's scene unless an opacity draws it as a layer of its own.
    let scene = |alone: &str| {
        layout(&format!(
            r#"{{ "flex_direction": "column", "align_items": "start", "background": {navy}, "children": [
                 {word},
                 {{ "width": {side}, "height": {side}, "background": {red}, "border_radius": {round} {alone} }} ] }}"#,
            navy = solid(0.05, 0.1, 0.3, 0.8),
            word = text("Text", 0.15, ""),
            side = len(0.3),
            red = solid(0.9, 0.1, 0.1, 0.7),
            round = c("0.05"),
        ))
    };
    let batched = render(&renderer, &mut texts, &project(&scene(""), "", ""), 0);
    let alone = render(
        &renderer,
        &mut texts,
        &project(
            &scene(r#", "opacity": { "mode": "constant", "value": 1 }"#),
            "",
            "",
        ),
        0,
    );
    assert_rgba8_near(&batched, &alone, 2);
}

#[test]
fn a_layout_draws_as_a_generator_node_and_through_a_media_node() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let mut texts = texts();
    let direct = project(&lower_third(), "", "");
    let node = |op: &str| {
        format!(r#"{{ "id": "g", "nodes": [ {{ "id": "n", "op": {op} }} ], "output": "n" }}"#)
    };
    let generator = project(
        &lower_third(),
        &node(&format!(
            r#"{{ "type": "generator", "generator": {} }}"#,
            lower_third()
        )),
        r#", "composition": { "id": "g" }"#,
    );
    let media = project(
        &lower_third(),
        &node(r#"{ "type": "media", "source_id": "layout", "start": 12000 }"#),
        r#", "composition": { "id": "g" }"#,
    );
    for ticks in [0, 12_000, 60_000] {
        let expected = render(&renderer, &mut texts, &direct, ticks);
        assert_rgba8_near(
            &render(&renderer, &mut texts, &generator, ticks),
            &expected,
            1,
        );
    }
    // A media node starts its source 0.1 s in.
    assert_rgba8_near(
        &render(&renderer, &mut texts, &media, 0),
        &render(&renderer, &mut texts, &direct, 12_000),
        1,
    );
    assert_ne!(
        render(&renderer, &mut texts, &media, 0),
        render(&renderer, &mut texts, &direct, 0)
    );
}

#[test]
fn a_static_layout_resolves_once_and_an_animated_one_each_change() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let mut texts = texts();
    let still = project(&title_card(), "", "");
    for frame in 0..10 {
        render(&renderer, &mut texts, &still, frame * 4_000);
    }
    assert_eq!(texts.layout_resolves(), 1);
    let moving = project(&lower_third(), "", "");
    for frame in 0..10 {
        render(&renderer, &mut texts, &moving, frame * 4_000);
    }
    // Ten frames 1/30 s apart: nine inside the 0.3 s fade, then the held end.
    assert_eq!(texts.layout_resolves(), 1 + 10);
    render(&renderer, &mut texts, &moving, 120_000);
    assert_eq!(texts.layout_resolves(), 1 + 10);
}

#[test]
fn the_unchanged_frame_skip_holds_for_a_static_layout_only() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let mut texts = texts();
    let size = RenderSize::new(W, H).expect("size");
    let mut unchanged = |project: &EngineProject, ticks: &[i64]| {
        let mut player = Player::new(QualityProfile::export()).expect("quality");
        ticks
            .iter()
            .map(|&ticks| {
                texts
                    .prepare_at(&renderer, project, MediaTime(ticks), size)
                    .expect("prepared");
                player
                    .render_at(
                        project,
                        &renderer,
                        &FramesWithText::new(&MissingFrames, &texts),
                        MediaTime(ticks),
                    )
                    .expect("a frame")
                    .unchanged
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        unchanged(&project(&title_card(), "", ""), &[0, 4_000, 8_000]),
        [false, true, true]
    );
    assert_eq!(
        unchanged(&project(&lower_third(), "", ""), &[0, 4_000, 8_000]),
        [false, false, false]
    );
}

#[test]
fn a_layout_its_host_did_not_prepare_is_missing_text() {
    let _lock = gpu_test_lock();
    let renderer = renderer();
    let texts = texts();
    let project = project(&title_card(), "", "");
    let frame = project.evaluate(MediaTime::ZERO).expect("evaluates");
    assert_eq!(
        renderer
            .render_frame(
                RenderSize::new(W, H).expect("size"),
                &frame,
                &FramesWithText::new(&MissingFrames, &texts),
            )
            .err(),
        Some(RenderError::MissingText)
    );
}
