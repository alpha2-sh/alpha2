use super::*;
use alpha2_text::TextFont;
use serde_json::json;

fn fixture() -> TextRasterizer {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fonts/Alpha2Fixture.ttf"
    ))
    .expect("checked-in fixture font");
    TextRasterizer::new(vec![TextFont {
        family: "Alpha2 Fixture".into(),
        bytes: bytes.into(),
        face_index: 0,
    }])
    .expect("valid fixture")
}

fn node(value: serde_json::Value) -> LayoutNode {
    let root: LayoutNode = serde_json::from_value(value).expect("a layout node");
    alpha2_model::Generator::Layout {
        root: Box::new(root.clone()),
    }
    .validate()
    .expect("a valid layout");
    root
}

#[allow(clippy::needless_pass_by_value)] // Called with `json!` literals.
fn constant(value: serde_json::Value) -> serde_json::Value {
    json!({ "mode": "constant", "value": value })
}

fn length(value: f64) -> serde_json::Value {
    constant(json!(value))
}

fn text(content: &str, size: f64) -> serde_json::Value {
    json!({ "text": constant(json!(content)), "font_size": constant(json!(size)) })
}

/// `root` resolved on a picture of `size`, which is also its canvas.
fn resolved(rasterizer: &TextRasterizer, root: &LayoutNode, size: RenderSize) -> ResolvedLayout {
    let evaluated = LayoutPlan::new(root, (size.width, size.height)).evaluate(MediaTime::ZERO);
    let shaped = shape_texts(&evaluated, size, |request| {
        rasterizer.shape(request).map(Arc::new)
    })
    .expect("shapes");
    resolve(&evaluated, size, &shaped)
}

fn size(width: u32, height: u32) -> RenderSize {
    RenderSize::new(width, height).expect("a size")
}

fn shaped(rasterizer: &TextRasterizer, content: &str, pixels: f32) -> ShapedText {
    rasterizer
        .shape(&TextRasterRequest {
            text: content.into(),
            font_size: pixels,
            max_width: 0.0,
            line_height: 1.2,
            align: alpha2_media::TextAlign::Start,
            color: [255; 4],
            emoji_policy: EmojiPolicy::Color,
            content_version: 1,
            outline: None,
            background: None,
            font_family: None,
        })
        .expect("shapes")
}

#[test]
fn a_text_in_a_padded_box_sizes_the_box_to_the_text() {
    let rasterizer = fixture();
    let root = node(json!({
        "align_items": "start",
        "children": [{
            "padding": constant(json!({ "top": 0.02, "right": 0.03, "bottom": 0.02, "left": 0.03 })),
            "children": [text("Ada Lovelace", 0.05)],
        }],
    }));
    let layout = resolved(&rasterizer, &root, size(1000, 800));
    // The shorter side, 800 px, is the unit: 16 and 24 px of padding, a 40 px font.
    let text = shaped(&rasterizer, "Ada Lovelace", 40.0);
    let card = layout.nodes[1];
    let width = text.max_content_width();
    let height = text.lines_at(wrap(width)).height;
    assert_eq!(card.border[0], 0.0);
    assert!(
        (card.border[2] - (width + 48.0).round()).abs() <= 1.0,
        "{card:?}"
    );
    assert!(
        (card.border[3] - (height + 32.0).round()).abs() <= 1.0,
        "{card:?}"
    );
    assert_eq!(card.content[0], 24.0);
    assert_eq!(card.content[1], 16.0);
    let label = layout.nodes[2];
    assert_eq!(label.line_count, Some(1));
    assert!(!label.overflow && !card.overflow);
}

#[test]
fn a_row_shares_space_by_flex_grow() {
    let rasterizer = fixture();
    let root = node(json!({
        "children": [
            { "flex_grow": constant(json!(1.0)) },
            { "flex_grow": constant(json!(3.0)) },
        ],
    }));
    let layout = resolved(&rasterizer, &root, size(1000, 500));
    assert_eq!(layout.nodes[1].border, [0.0, 0.0, 250.0, 500.0]);
    assert_eq!(layout.nodes[2].border, [250.0, 0.0, 1000.0, 500.0]);
}

#[test]
fn rounding_gives_whole_pixels_with_no_gaps_between_siblings() {
    let rasterizer = fixture();
    let child = json!({ "flex_grow": constant(json!(1.0)) });
    let root = node(json!({
        "padding": constant(json!({ "left": 0.0013 })),
        "children": [child, child, child],
    }));
    let layout = resolved(&rasterizer, &root, size(1001, 1000));
    for pair in layout.nodes[1..].windows(2) {
        assert_eq!(pair[0].border[2], pair[1].border[0]);
    }
    for rects in &layout.nodes {
        for edge in rects.border.iter().chain(&rects.content) {
            assert_eq!(edge.fract(), 0.0, "{rects:?}");
        }
    }
    assert_eq!(layout.nodes[3].border[2], 1001.0);
}

#[test]
fn wrapping_text_breaks_at_the_box_width() {
    let rasterizer = fixture();
    let content = "Wrote the first published algorithm for a machine, 1843";
    let root = node(json!({
        "align_items": "start",
        "children": [{
            "width": length(0.3),
            "padding": constant(json!({ "left": 0.01, "right": 0.01 })),
            "children": [text(content, 0.03)],
        }],
    }));
    let layout = resolved(&rasterizer, &root, size(1000, 1000));
    let label = layout.nodes[2];
    assert_eq!(label.content[2] - label.content[0], 280.0);
    let expected = shaped(&rasterizer, content, 30.0).lines_at(wrap(280.0));
    assert!(expected.line_count > 1);
    assert_eq!(label.line_count, Some(expected.line_count));
    assert_eq!(label.content[3] - label.content[1], expected.height.round());
}

#[test]
fn max_content_text_never_wraps_its_last_word() {
    let rasterizer = fixture();
    for content in ["Ada Lovelace", "Wrote the first", "a b c d e f", "x"] {
        for (font, width) in [(0.031, 1920), (0.0517, 1280), (0.0423, 997)] {
            let root = node(json!({
                "align_items": "start",
                "children": [{ "children": [text(content, font)] }],
            }));
            let layout = resolved(&rasterizer, &root, size(width, 1080));
            assert_eq!(layout.nodes[2].line_count, Some(1), "{content} at {font}");
            assert!(!layout.nodes[2].overflow);
        }
    }
}

#[test]
fn justify_content_end_and_align_items_start_place_a_card_bottom_left() {
    let rasterizer = fixture();
    let root = node(json!({
        "flex_direction": "column",
        "justify_content": "end",
        "align_items": "start",
        "padding": constant(json!({ "top": 0.06, "right": 0.06, "bottom": 0.08, "left": 0.06 })),
        "children": [{ "width": length(0.4), "height": length(0.1) }],
    }));
    let layout = resolved(&rasterizer, &root, size(1920, 1080));
    // 0.06 and 0.08 of 1080: 64.8 and 86.4 px, rounded by absolute position.
    let card = layout.nodes[1].border;
    assert_eq!(card[0], 65.0);
    assert_eq!(card[3], 1080.0 - 86.0);
    assert_eq!(card[2] - card[0], 432.0);
    assert_eq!(card[3] - card[1], 108.0);
}

#[test]
fn an_absolute_box_lies_at_its_percent_inset_in_the_padding_box() {
    let rasterizer = fixture();
    let root = node(json!({
        "padding": constant(json!({ "left": 0.1, "top": 0.1 })),
        "children": [
            { "width": length(0.2), "height": length(0.2) },
            {
                "position": "absolute",
                "inset": {
                    "left": constant(json!("50%")),
                    "top": constant(json!("25%")),
                },
                "width": length(0.1),
                "height": constant(json!("10%")),
            },
        ],
    }));
    let layout = resolved(&rasterizer, &root, size(1000, 1000));
    // The padding box is the whole root: insets are percents of it.
    assert_eq!(layout.nodes[2].border, [500.0, 250.0, 600.0, 350.0]);
    // An absolute box takes no room in the flow.
    assert_eq!(layout.nodes[1].border, [100.0, 100.0, 300.0, 300.0]);
    assert!(!layout.nodes[0].overflow);
}

#[test]
fn overflow_is_reported_for_text_taller_than_its_box() {
    let rasterizer = fixture();
    let root = node(json!({
        "align_items": "start",
        "children": [{
            "width": length(0.2),
            "height": length(0.05),
            "children": [text("Several words that wrap onto more lines", 0.04)],
        }],
    }));
    let layout = resolved(&rasterizer, &root, size(1000, 1000));
    // The column-less row stretches the text to the box's height; its lines are taller.
    assert!(layout.nodes[2].overflow);
    let fits = node(json!({
        "align_items": "start",
        "children": [{ "width": length(0.2), "children": [text("Several words", 0.04)] }],
    }));
    let layout = resolved(&rasterizer, &fits, size(1000, 1000));
    assert!(!layout.nodes.iter().any(|rects| rects.overflow));
}

#[test]
fn text_with_a_fractional_height_fits_its_rounded_box() {
    let rasterizer = fixture();
    // At 1080 pixels the title is 129.6 pixels tall from 409.86, and its box
    // rounds to 410..539, 129 pixels; the line under it is 48.6 pixels tall
    // and its box rounds to 48.
    let root = node(json!({
        "flex_direction": "column",
        "justify_content": "center",
        "align_items": "center",
        "gap": length(0.035),
        "padding": constant(json!({ "left": 0.08, "top": 0.08, "right": 0.08, "bottom": 0.08 })),
        "line_height": 1.0,
        "children": [
            text("Your title", 0.12),
            { "width": length(0.09), "height": length(0.006) },
            text("A line about what comes next", 0.045),
        ],
    }));
    let layout = resolved(&rasterizer, &root, size(1920, 1080));
    let [_, title, _, line] = layout.nodes[..] else {
        panic!("four nodes");
    };
    assert_eq!(title.content[3] - title.content[1], 129.0);
    assert_eq!(line.content[3] - line.content[1], 48.0);
    assert!(!layout.nodes.iter().any(|rects| rects.overflow));
}

#[test]
fn min_content_overflow_is_reported_for_a_long_word() {
    let rasterizer = fixture();
    // A flex item's automatic minimum width is its min-content: the word does not split, and the
    // text reaches past its parent.
    let root = node(json!({
        "align_items": "start",
        "children": [{
            "width": length(0.05),
            "children": [text("Supercalifragilistic", 0.04)],
        }],
    }));
    let layout = resolved(&rasterizer, &root, size(1000, 1000));
    assert!(layout.nodes[1].overflow);
    assert!(!layout.nodes[2].overflow);
    assert_eq!(layout.nodes[2].line_count, Some(1));
    // A text given a width narrower than the word splits it.
    let mut narrow = text("Supercalifragilistic", 0.04);
    narrow["width"] = length(0.05);
    let root = node(json!({ "align_items": "start", "children": [narrow] }));
    let layout = resolved(&rasterizer, &root, size(1000, 1000));
    assert!(layout.nodes[1].overflow);
    assert!(layout.nodes[1].line_count.is_some_and(|count| count > 1));
}

#[test]
fn a_child_past_its_parent_overflows_it_and_an_absolute_one_does_not() {
    let rasterizer = fixture();
    let root = node(json!({
        "align_items": "start",
        "children": [{
            "width": length(0.1),
            "height": length(0.1),
            "children": [{ "width": length(0.3), "height": length(0.05), "flex_shrink": constant(json!(0.0)) }],
        }],
    }));
    let layout = resolved(&rasterizer, &root, size(1000, 1000));
    assert!(layout.nodes[1].overflow);
    let absolute = node(json!({
        "align_items": "start",
        "children": [{
            "width": length(0.1),
            "height": length(0.1),
            "children": [{ "position": "absolute", "width": length(0.3), "height": length(0.05) }],
        }],
    }));
    let layout = resolved(&rasterizer, &absolute, size(1000, 1000));
    assert!(!layout.nodes[1].overflow);
}

#[test]
fn empty_text_measures_nothing_and_needs_no_fonts() {
    let root = node(json!({
        "align_items": "start",
        "children": [{ "padding": constant(json!({ "left": 0.01 })), "children": [text("", 0.05)] }],
    }));
    let bounds = measure_layout(
        &TextRasterizer::empty(),
        &root,
        MediaTime::ZERO,
        size(1000, 1000),
        (1000, 1000),
    )
    .expect("no text to shape");
    assert_eq!(bounds.nodes.len(), 3);
    assert_eq!(bounds.nodes[2].line_count, Some(0));
    assert_eq!(bounds.nodes[1].border.right, 0.01);
    assert_eq!(bounds.nodes[1].border.bottom, 0.0);
    let words = node(json!({ "children": [text("a", 0.05)] }));
    assert_eq!(
        measure_layout(
            &TextRasterizer::empty(),
            &words,
            MediaTime::ZERO,
            size(10, 10),
            (10, 10)
        ),
        Err(TextError::Raster(TextRasterError::NoFonts))
    );
}

#[test]
fn measured_bounds_name_every_node_as_fractions_of_the_picture() {
    let rasterizer = fixture();
    let root = node(json!({
        "justify_content": "center",
        "align_items": "center",
        "children": [
            { "width": length(0.5), "height": length(0.25) },
            { "children": [text("Title", 0.1)] },
        ],
    }));
    let bounds = measure_layout(
        &rasterizer,
        &root,
        MediaTime::ZERO,
        size(800, 400),
        (800, 400),
    )
    .expect("measures");
    let at = bounds
        .nodes
        .iter()
        .map(|node| node.at.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        at,
        [
            "root",
            "root.children[0]",
            "root.children[1]",
            "root.children[1].children[0]"
        ]
    );
    assert_eq!(
        bounds.nodes[0].border,
        CanvasRect {
            left: 0.0,
            top: 0.0,
            right: 1.0,
            bottom: 1.0
        }
    );
    let first = bounds.nodes[1].border;
    assert_eq!(first.top, 0.375);
    assert_eq!(first.bottom - first.top, 0.25);
    assert_eq!(bounds.nodes[3].line_count, Some(1));
}

#[test]
fn pixels_are_canvas_pixels_and_scale_with_the_render_size() {
    let rasterizer = fixture();
    let root = node(json!({
        "padding": constant(json!("54px 108px")),
        "align_items": "start",
        "children": [{ "width": constant(json!("540px")), "height": constant(json!("10%")) }],
    }));
    // On its 1920x1080 canvas: pixels are pixels.
    let at_canvas = measure_layout(
        &rasterizer,
        &root,
        MediaTime::ZERO,
        size(1920, 1080),
        (1920, 1080),
    )
    .expect("measures");
    let child = at_canvas.nodes[1].border;
    assert_eq!(child.left * 1920.0, 108.0);
    assert_eq!(child.top * 1080.0, 54.0);
    assert_eq!((child.right - child.left) * 1920.0, 540.0);
    // A preview at half the size keeps the proportions: the same fractions of the picture.
    let half = measure_layout(
        &rasterizer,
        &root,
        MediaTime::ZERO,
        size(960, 540),
        (1920, 1080),
    )
    .expect("measures");
    // Rounding to whole pixels of each picture may differ by a pixel; nothing else does.
    let moved = half.nodes[1].border;
    assert_eq!(
        [moved.left, moved.top, moved.right],
        [child.left, child.top, child.right]
    );
    assert!((moved.bottom - child.bottom).abs() * 540.0 <= 1.0);
    // The same tree in fractions of the shorter side lays out alike.
    let fractions = node(json!({
        "padding": constant(json!("0.05 0.1")),
        "align_items": "start",
        "children": [{ "width": length(0.5), "height": constant(json!("10%")) }],
    }));
    let layout = resolved(&rasterizer, &fractions, size(1920, 1080));
    assert_eq!(layout.nodes[1].border, [108.0, 54.0, 648.0, 54.0 + 97.0]);
}

#[test]
fn a_grid_lays_its_children_out_in_tracks() {
    let rasterizer = fixture();
    let root = node(json!({
        "display": "grid",
        "grid_template_columns": "1fr 2fr 100px",
        "grid_auto_rows": "50px",
        "column_gap": constant(json!("10px")),
        "row_gap": constant(json!("20px")),
        "children": [{}, {}, {}, { "grid_column": "2 / span 2" }],
    }));
    let layout = resolved(&rasterizer, &root, size(1000, 500));
    // 1000 - 100 - 2 * 10 = 880 px for the fr tracks: 293.33 and 586.67.
    assert_eq!(layout.nodes[1].border, [0.0, 0.0, 293.0, 50.0]);
    assert_eq!(layout.nodes[2].border, [303.0, 0.0, 890.0, 50.0]);
    assert_eq!(layout.nodes[3].border, [900.0, 0.0, 1000.0, 50.0]);
    // The second row; the item spans the last two columns and the gap between them.
    assert_eq!(layout.nodes[4].border, [303.0, 70.0, 1000.0, 120.0]);
    assert!(!layout.nodes.iter().any(|rects| rects.overflow));
}

#[test]
fn a_grid_repeats_tracks_to_fill_its_width() {
    let rasterizer = fixture();
    let root = node(json!({
        "display": "grid",
        "grid_template_columns": "repeat(auto-fill, minmax(150px, 1fr))",
        "grid_auto_rows": "40px",
        "children": (0..5).map(|_| json!({})).collect::<Vec<_>>(),
    }));
    // 640 px hold four 150 px columns, grown to 160 px each; the fifth child wraps.
    let layout = resolved(&rasterizer, &root, size(640, 400));
    let lefts = layout.nodes[1..]
        .iter()
        .map(|rects| rects.border[0])
        .collect::<Vec<_>>();
    assert_eq!(lefts, [0.0, 160.0, 320.0, 480.0, 0.0]);
    assert_eq!(layout.nodes[5].border[1], 40.0);
}

#[test]
fn grid_items_align_in_their_areas_and_text_wraps_at_its_cell() {
    let rasterizer = fixture();
    let content = "Wrote the first published algorithm for a machine, 1843";
    let mut label = text(content, 0.03);
    label["align_self"] = json!("start");
    let root = node(json!({
        "display": "grid",
        "grid_template_columns": "300px 1fr",
        "grid_template_rows": "200px",
        "justify_items": "center",
        "align_items": "end",
        "children": [
            label,
            { "width": length(0.1), "height": length(0.1), "justify_self": "end" },
        ],
    }));
    let layout = resolved(&rasterizer, &root, size(1000, 1000));
    let lines = shaped(&rasterizer, content, 30.0).lines_at(wrap(300.0));
    let label = layout.nodes[1];
    assert!(lines.line_count > 1);
    assert_eq!(label.line_count, Some(lines.line_count));
    assert_eq!(label.border[1], 0.0);
    assert!(label.border[2] - label.border[0] <= 300.0);
    // `justify_self: end` and the container's `align_items: end` put the box bottom right.
    assert_eq!(layout.nodes[2].border, [900.0, 100.0, 1000.0, 200.0]);
}

#[test]
fn an_invalid_tree_resolves_without_panicking() {
    let rasterizer = fixture();
    let root: LayoutNode = serde_json::from_value(json!({
        "width": constant(json!(-3.0)),
        "gap": constant(json!(-1.0)),
        "children": [
            {
                "text": constant(json!("both")),
                "shape": { "items": [] },
                "children": [{ "aspect_ratio": 0.0 }],
                "margin": constant(json!({ "left": -50.0 })),
                "display": "grid",
                "grid_template_columns": "repeat(65, 0fr) -1px",
            },
            { "max_width": constant(json!("1000000000000000000000000000000%")) },
            { "display": "grid", "grid_column": "-30000 / span 30000", "children": [{}] },
        ],
    }))
    .expect("parses");
    for (width, height) in [(1, 1), (16_384, 3), (640, 360)] {
        let bounds = measure_layout(
            &rasterizer,
            &root,
            MediaTime::ZERO,
            size(width, height),
            (width, height),
        )
        .expect("measures");
        for node in &bounds.nodes {
            for value in [node.border.left, node.border.right, node.content.top] {
                assert!(value.is_finite(), "{node:?}");
            }
        }
    }
}

/// A 40-node layout with 8 texts: 8 cards, each a box holding an icon, a column of a title and a
/// line, and a spacer.
fn forty_nodes() -> LayoutNode {
    let card = |index: usize| {
        json!({
            "flex_direction": "row", "gap": constant(json!(0.01)),
            "padding": constant(json!({ "top": 0.01, "right": 0.02, "bottom": 0.01, "left": 0.02 })),
            "background": constant(json!({ "type": "solid", "color": { "red": 0.1, "green": 0.1, "blue": 0.1, "alpha": 0.9 } })),
            "border_radius": constant(json!(0.01)),
            "children": [
                { "width": length(0.03), "height": length(0.03) },
                { "flex_direction": "column", "children": [text(&format!("Card {index}: a title that wraps"), 0.03)] },
                { "flex_grow": constant(json!(1.0)) },
            ],
        })
    };
    node(json!({
        "flex_direction": "row", "flex_wrap": "wrap", "gap": constant(json!(0.02)),
        "padding": constant(json!({ "top": 0.05, "right": 0.05, "bottom": 0.05, "left": 0.05 })),
        "children": (0..8).map(card).collect::<Vec<_>>(),
    }))
}

/// What resolving costs, for reference only; never gated. Run with
/// `cargo test -p alpha2-engine --release --lib layout_resolve_cost -- --ignored --nocapture`.
#[test]
#[ignore = "timing only"]
fn layout_resolve_cost() {
    let rasterizer = fixture();
    let root = forty_nodes();
    let mut count = 0;
    root.walk(|_, _| count += 1);
    assert_eq!(count, 41);
    let size = size(1920, 1080);
    let runs = 200;
    let start = web_time::Instant::now();
    for _ in 0..runs {
        std::hint::black_box(
            measure_layout(&rasterizer, &root, MediaTime::ZERO, size, (1920, 1080))
                .expect("measures"),
        );
    }
    let cold = start.elapsed() / runs;
    let evaluated = LayoutPlan::new(&root, (1920, 1080)).evaluate(MediaTime::ZERO);
    let shaped = shape_texts(&evaluated, size, |request| {
        rasterizer.shape(request).map(Arc::new)
    })
    .expect("shapes");
    let start = web_time::Instant::now();
    for _ in 0..runs {
        std::hint::black_box(resolve(&evaluated, size, &shaped));
    }
    let warm = start.elapsed() / runs;
    eprintln!("layout_resolve_cost nodes=41 texts=8 1920x1080 cold={cold:?} warm={warm:?}");
}
