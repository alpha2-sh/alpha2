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

fn resolved(rasterizer: &TextRasterizer, root: &LayoutNode, size: PictureSize) -> ResolvedLayout {
    let evaluated = LayoutPlan::new(root, (size.width, size.height)).evaluate(MediaTime::ZERO);
    let shaped = shape_texts(&evaluated, size, |request| {
        rasterizer.shape(request).map(Arc::new)
    })
    .expect("shapes");
    resolve(&evaluated, size, &shaped)
}

fn size(width: u32, height: u32) -> PictureSize {
    PictureSize::new(width, height).expect("a size")
}

fn shaped(rasterizer: &TextRasterizer, content: &str, pixels: f32) -> ShapedText {
    rasterizer
        .shape(&TextRasterRequest {
            text: content.into(),
            font_size: pixels,
            max_width: 0.0,
            line_height: 1.2,
            align: alpha2_media::TextAlign::Start,
            fill: alpha2_text::TextFill::Solid([255; 4]),
            emoji_policy: EmojiPolicy::Color,
            content_version: 1,
            strokes: Vec::new(),
            split_background: false,
            background: None,
            font_family: None,
            letter_spacing: 0.0,
            font_weight: 400,
            clamp: None,
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
fn a_text_node_reports_baselines_inside_its_content_box() {
    let rasterizer = fixture();
    let root = node(json!({
        "align_items": "start",
        "children": [
            {
                "padding": constant(json!({ "top": 0.02, "right": 0.03, "bottom": 0.02, "left": 0.03 })),
                "line_height": 1.4,
                "children": [text("Hello\nworld", 0.04)],
            },
            { "width": length(0.1), "height": length(0.1) },
        ],
    }));
    let bounds = measure_layout(
        &rasterizer,
        &root,
        MediaTime::ZERO,
        size(1000, 1000),
        (1000, 1000),
    )
    .expect("measures");
    let text = &bounds.nodes[2];
    assert_eq!(text.line_count, Some(2));
    let baselines = text.baselines.expect("a text node has baselines");
    assert!(text.content.top < baselines.first && baselines.last < text.content.bottom);
    // 8 px of half-leading and 32 px of ascent below the content box's top; lines 56 px apart.
    assert!((baselines.first - text.content.top - 0.04).abs() < 1e-6);
    assert!((baselines.last - baselines.first - 0.056).abs() < 1e-6);
    for node in [&bounds.nodes[0], &bounds.nodes[1], &bounds.nodes[3]] {
        assert_eq!(node.baselines, None, "{}", node.at);
    }
}

#[test]
fn a_spaced_text_measures_as_wide_as_its_raster_lines() {
    let rasterizer = fixture();
    let root = node(json!({
        "flex_direction": "row",
        "align_items": "start",
        "letter_spacing": 0.25,
        "children": [text("Spaced", 0.04)],
    }));
    let picture = size(1000, 1000);
    let bounds = measure_layout(&rasterizer, &root, MediaTime::ZERO, picture, (1000, 1000))
        .expect("measures");
    let content = bounds.nodes[1].content;
    let evaluated = LayoutPlan::new(&root, (1000, 1000)).evaluate(MediaTime::ZERO);
    let resolved = resolved(&rasterizer, &root, picture);
    let (_, requests) = layout_texts(&evaluated, &resolved, picture);
    let request = &requests[0].1.request;
    assert!(
        (request.letter_spacing - 10.0).abs() < 1e-6,
        "a quarter of 40 px"
    );
    let raster = rasterizer.measure(request).expect("measures");
    let line = raster.content[2] - raster.content[0];
    assert!(((content.right - content.left) * 1000.0 - line).abs() <= 1.0);
    let unspaced = shaped(&rasterizer, "Spaced", 40.0).max_content_width();
    assert!((line - unspaced - 50.0).abs() < 1e-3, "five gaps of 10 px");
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
    // Insets are percents of the root's padding box.
    assert_eq!(layout.nodes[2].border, [500.0, 250.0, 600.0, 350.0]);
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
    // At 1080 px the title is 129.6 px tall from 409.86 and its box rounds to 410..539 (129 px);
    // the line under it is 48.6 px tall and its box rounds to 48.
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
    // A flex item's automatic minimum width is its min-content, so the word does not split and
    // the text reaches past its parent.
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
        Err(TextRasterError::NoFonts)
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

/// 40 nodes with 8 texts: 8 cards, each holding an icon, a title-and-line column, and a spacer.
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

/// Prints resolve cost; never gated. Run with
/// `cargo test -p alpha2-layout --release --lib layout_resolve_cost -- --ignored --nocapture`.
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
    let start = std::time::Instant::now();
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
    let start = std::time::Instant::now();
    for _ in 0..runs {
        std::hint::black_box(resolve(&evaluated, size, &shaped));
    }
    let warm = start.elapsed() / runs;
    eprintln!("layout_resolve_cost nodes=41 texts=8 1920x1080 cold={cold:?} warm={warm:?}");
}

const QUICK: &str = "The quick brown fox jumps over the lazy dog again and again";

fn measured(root: &LayoutNode, time: MediaTime) -> LayoutBounds {
    measure_layout(&fixture(), root, time, size(1280, 720), (1280, 720)).expect("measures")
}

#[test]
fn a_clamp_by_count_ends_the_automatic_height_at_the_clamp_point() {
    let clamped = node(json!({ "align_items": "start", "children": [
        { "width": length(0.4), "font_size": length(0.05), "line_clamp": { "max_lines": 2 },
          "text": constant(json!(QUICK)) } ] }));
    let free = node(json!({ "align_items": "start", "children": [
        { "width": length(0.4), "font_size": length(0.05), "text": constant(json!(QUICK)) } ] }));
    let (clamped, free) = (
        measured(&clamped, MediaTime::ZERO),
        measured(&free, MediaTime::ZERO),
    );
    let (text, whole) = (&clamped.nodes[1], &free.nodes[1]);
    assert!(whole.line_count > Some(2) && !whole.truncated);
    assert_eq!(text.line_count, Some(2));
    assert!(text.truncated && !text.overflow);
    let height = |node: &NodeBounds| node.border.bottom - node.border.top;
    let line = 0.05 * 1.2 * 720.0 / 720.0;
    assert!(
        (height(text) - 2.0 * line).abs() < 2.0 / 720.0,
        "{}",
        height(text)
    );
    assert_eq!(text.font_size, Some(0.05));
}

#[test]
fn a_clamp_cuts_at_a_fixed_height_and_the_draw_asks_for_the_same_cut() {
    let root = node(json!({ "align_items": "start", "children": [
        { "width": length(0.4), "height": length(0.15), "font_size": length(0.05),
          "line_clamp": { "block_ellipsis": { "string": "..." } },
          "text": constant(json!(QUICK)) } ] }));
    let bounds = measured(&root, MediaTime::ZERO);
    assert_eq!(bounds.nodes[1].line_count, Some(2));
    assert!(bounds.nodes[1].truncated);
    let picture = size(1280, 720);
    let evaluated = LayoutPlan::new(&root, (1280, 720)).evaluate(MediaTime::ZERO);
    let resolved = resolved(&fixture(), &root, picture);
    let (_, requests) = layout_texts(&evaluated, &resolved, picture);
    let clamp = requests[0].1.request.clamp.clone().expect("a clamp");
    assert_eq!(clamp.max_lines, None);
    assert_eq!(clamp.ellipsis.as_deref(), Some("..."));
    assert!((clamp.max_height - (0.15 * 720.0 + LAYOUT_WRAP_SLACK)).abs() < 0.01);
    let measure = fixture().measure(&requests[0].1.request).expect("measures");
    assert_eq!((measure.line_count, measure.truncated), (2, true));
}

#[test]
fn a_fit_fills_a_fixed_box_without_changing_it() {
    let card = |fit: serde_json::Value| {
        node(json!({ "align_items": "start", "children": [
            { "width": length(0.5), "height": length(0.2), "font_size": length(0.1),
              "font_size_fit": fit, "text": constant(json!(QUICK)) } ] }))
    };
    let fitted = measured(&card(json!({ "min": 0.02 })), MediaTime::ZERO);
    let base = measured(&card(serde_json::Value::Null), MediaTime::ZERO);
    let (fitted_text, base_text) = (&fitted.nodes[1], &base.nodes[1]);
    assert_eq!(fitted_text.border, base_text.border);
    assert!(base_text.overflow);
    assert!(!fitted_text.overflow);
    let font_size = fitted_text.font_size.expect("a text");
    assert!(font_size < 0.1 && font_size > 0.02, "{font_size}");
    // The fitted lines, drawn, fit the box.
    let picture = size(1280, 720);
    let root = card(json!({ "min": 0.02 }));
    let evaluated = LayoutPlan::new(&root, (1280, 720)).evaluate(MediaTime::ZERO);
    let rasterizer = fixture();
    let shaped = shape_texts(&evaluated, picture, |request| {
        rasterizer.shape(request).map(Arc::new)
    })
    .expect("shapes");
    let mut resolved = resolve(&evaluated, picture, &shaped);
    fit_texts(&evaluated, &mut resolved, picture, |request| {
        rasterizer.shape(request).map(Arc::new)
    })
    .expect("fits");
    let (_, requests) = layout_texts(&evaluated, &resolved, picture);
    let request = &requests[0].1.request;
    assert_eq!(request.font_size, resolved.nodes[1].font_pixels);
    assert_eq!(request.font_size, (font_size * 720.0).clamp(1.0, 2048.0));
    let measure = rasterizer.measure(request).expect("measures");
    assert!(f64::from(measure.box_height) <= 0.2 * 720.0 + 1.0);
}

#[test]
fn an_animated_font_size_is_fitted_at_each_time() {
    let root = node(json!({ "align_items": "start", "children": [
        { "width": length(0.4), "height": length(0.2),
          "font_size": { "mode": "keyframes", "keyframes": [
              { "time": 0, "value": 0.03, "interpolation": { "type": "linear" } },
              { "time": 120_000, "value": 0.3, "interpolation": { "type": "linear" } } ] },
          "font_size_fit": { "min": 0.01 }, "text": constant(json!(QUICK)) } ] }));
    let at = |ticks| {
        measured(&root, MediaTime(ticks)).nodes[1]
            .font_size
            .expect("a text")
    };
    // Below the fit the text follows its keyframes; above it, the box caps it.
    assert_eq!(at(0), 0.03);
    let (late, later) = (at(100_000), at(120_000));
    assert!(late < 0.03 + 0.27 * 100.0 / 120.0);
    assert_eq!(late, later);
}

#[test]
fn text_animators_never_change_the_fit() {
    let card = |animators: serde_json::Value| {
        node(json!({ "align_items": "start", "children": [
            { "width": length(0.5), "height": length(0.2), "font_size": length(0.1),
              "font_size_fit": { "min": 0.02 }, "text_animators": animators,
              "text": constant(json!(QUICK)) } ] }))
    };
    let still = measured(&card(json!([])), MediaTime::ZERO);
    let moving = measured(
        &card(json!([{ "selector": { "type": "range" },
            "properties": { "scale": constant(json!({ "x": 3, "y": 3 })) } }])),
        MediaTime::ZERO,
    );
    assert_eq!(still.nodes[1].font_size, moving.nodes[1].font_size);
    assert_eq!(still.nodes[1].line_count, moving.nodes[1].line_count);
}

#[test]
fn layout_fit_and_clamp_values_out_of_range_are_refused() {
    let refused = |text: serde_json::Value| {
        let root: LayoutNode =
            serde_json::from_value(json!({ "children": [text] })).expect("a layout node");
        alpha2_model::Generator::Layout {
            root: Box::new(root),
        }
        .validate_on(1000, 1000)
        .expect_err("refused")
    };
    for (field, value) in [
        ("font_size_fit", json!({ "min": 0 })),
        ("font_size_fit", json!({ "min": 0.1, "max": 0.05 })),
        ("font_size_fit", json!({ "min": "10px", "max": 0.005 })),
        ("font_size_fit", json!({ "min": "2000px" })),
        ("line_clamp", json!({ "max_lines": 0 })),
        ("line_clamp", json!({ "block_ellipsis": { "string": "" } })),
    ] {
        let mut text = text("a", 0.05);
        text[field] = value;
        let issue = refused(text);
        assert!(
            matches!(&issue, alpha2_model::GeneratorIssue::Layout(issue)
                if issue.at == format!("root.children[0].{field}")),
            "{field}: {issue:?}"
        );
    }
}
