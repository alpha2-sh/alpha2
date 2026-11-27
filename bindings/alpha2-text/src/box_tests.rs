#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "pixel indices from layout positions"
)]

use super::*;

fn fixture() -> TextRasterizer {
    TextRasterizer::new(vec![TextFont {
        family: "Alpha2 Fixture".into(),
        bytes: std::fs::read("../../tests/fonts/Alpha2Fixture.ttf")
            .expect("fixture font")
            .into(),
        face_index: 0,
    }])
    .expect("font")
}

const BOX: [u8; 4] = [0, 0, 255, 255];
const LINE: [u8; 4] = [255, 0, 0, 255];

/// A text drawn with no ink of its own, so only its boxes show.
fn boxed(text: &str, background: TextBackground) -> TextRasterRequest {
    TextRasterRequest {
        text: text.into(),
        font_size: 40.0,
        max_width: 0.0,
        line_height: 1.0,
        align: TextAlign::Start,
        fill: TextFill::Solid([0, 0, 0, 0]),
        emoji_policy: EmojiPolicy::Color,
        content_version: 1,
        strokes: Vec::new(),
        background: Some(background),
        split_background: false,
        font_family: None,
        letter_spacing: 0.0,
        font_weight: 400,
        clamp: None,
        spans: Vec::new(),
        advance_headroom: 1.0,
        animator_grouping: ClusterGrouping::Character,
        animator_pivot: [0.5, 0.5],
    }
}

fn shaped(radius: f32, border: Option<TextBoxBorder>) -> TextBackground {
    TextBackground {
        radius,
        border,
        ..TextBackground::padded(BOX, 10.0)
    }
}

/// The pixel at `[x, y]` of the layout box.
fn at(result: &TextRasterResult, x: f32, y: f32) -> [u8; 4] {
    let (left, top) = (
        (result.descriptor.layout_x + x).floor() as usize,
        (result.descriptor.layout_y + y).floor() as usize,
    );
    let width = result.descriptor.texture.width as usize;
    result.pixels.rgba8[(top * width + left) * 4..][..4]
        .try_into()
        .expect("a pixel")
}

/// The layout-box rectangle the box color covers, from its painted pixels.
fn painted(result: &TextRasterResult) -> [f32; 4] {
    let width = result.descriptor.texture.width as usize;
    let mut bounds = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    for (index, pixel) in result.pixels.rgba8.as_chunks::<4>().0.iter().enumerate() {
        if pixel[3] > 0 {
            let x = (index % width) as f32 - result.descriptor.layout_x;
            let y = (index / width) as f32 - result.descriptor.layout_y;
            bounds = [
                bounds[0].min(x),
                bounds[1].min(y),
                bounds[2].max(x + 1.0),
                bounds[3].max(y + 1.0),
            ];
        }
    }
    bounds
}

#[test]
fn a_number_padding_draws_the_old_box_bytes() {
    let mut rasterizer = fixture();
    let request = boxed("II", TextBackground::padded(BOX, 6.5));
    let drawn = rasterizer.rasterize(&request).expect("draws");
    let line = rasterizer.layout(&request).expect("lays out").lines[0].clone();
    let [left, top, right, bottom] = painted(&drawn);
    assert_eq!(
        [left, top, right, bottom],
        [
            (line.left - 6.5).floor(),
            -7.0,
            (line.left + line.width + 6.5).ceil(),
            (40.0_f32 + 6.5).ceil()
        ]
    );
    assert_eq!(at(&drawn, line.left + 2.0, 20.0), BOX);
    assert_eq!(
        at(&drawn, line.left - 7.0, 20.0)[3],
        128,
        "a half-covered edge pixel"
    );
}

#[test]
fn padding_takes_each_side() {
    let mut rasterizer = fixture();
    let request = boxed(
        "II",
        TextBackground {
            padding: [2.0, 10.0, 4.0, 20.0],
            ..TextBackground::padded(BOX, 0.0)
        },
    );
    let drawn = rasterizer.rasterize(&request).expect("draws");
    let line = rasterizer.layout(&request).expect("lays out").lines[0].clone();
    assert_eq!(
        painted(&drawn),
        [
            line.left - 20.0,
            -2.0,
            (line.left + line.width + 10.0).ceil(),
            44.0
        ]
    );
    assert_eq!(request.decoration_margin(), [20, 4]);
}

#[test]
fn a_radius_is_capped_at_half_the_shorter_side() {
    let mut rasterizer = fixture();
    let huge = rasterizer
        .rasterize(&boxed("IIIII", shaped(1_000.0, None)))
        .expect("draws");
    let half = rasterizer
        .rasterize(&TextRasterRequest {
            content_version: 2,
            ..boxed("IIIII", shaped(30.0, None))
        })
        .expect("draws");
    assert_eq!(
        huge.pixels.rgba8, half.pixels.rgba8,
        "a 60-pixel tall box caps at 30"
    );
    let [left, top, _, bottom] = painted(&huge);
    assert_eq!(at(&huge, left, top), [0; 4], "the corner is round");
    assert_eq!(
        at(&huge, left + 1.0, f32::midpoint(top, bottom)),
        BOX,
        "the side's middle is not"
    );
}

#[test]
fn a_border_lies_inside_the_box() {
    let mut rasterizer = fixture();
    let border = TextBoxBorder {
        width: 4.0,
        color: LINE,
        dash: None,
    };
    let drawn = rasterizer
        .rasterize(&boxed("IIII", shaped(0.0, Some(border))))
        .expect("draws");
    let plain = rasterizer
        .rasterize(&TextRasterRequest {
            content_version: 2,
            ..boxed("IIII", shaped(0.0, None))
        })
        .expect("draws");
    assert_eq!(
        painted(&drawn),
        painted(&plain),
        "the border adds nothing outside"
    );
    let [left, top, _, bottom] = painted(&drawn);
    let middle = f32::midpoint(top, bottom);
    assert_eq!(at(&drawn, left + 1.0, middle), LINE);
    assert_eq!(at(&drawn, left + 3.0, middle), LINE);
    assert_eq!(at(&drawn, left + 5.0, middle), BOX);
}

#[test]
fn a_border_is_hidden_where_another_box_covers_it() {
    let mut rasterizer = fixture();
    let border = TextBoxBorder {
        width: 3.0,
        color: LINE,
        dash: None,
    };
    let drawn = rasterizer
        .rasterize(&boxed("IIIIII\nIIIIII", shaped(0.0, Some(border))))
        .expect("draws");
    let layout = rasterizer
        .layout(&boxed("IIIIII\nIIIIII", shaped(0.0, None)))
        .expect("lays out");
    let first = &layout.lines[0];
    let x = first.left + first.width / 2.0;
    let (em_bottom, padding, half_border) = (40.0, 10.0, 1.5);
    let seam = em_bottom + padding - half_border;
    assert_eq!(
        at(&drawn, x, seam),
        BOX,
        "line one's bottom border is hidden"
    );
    assert_eq!(
        at(&drawn, x, -10.0 + 1.0),
        LINE,
        "the outer top border shows"
    );
}

#[test]
fn a_dash_starts_after_the_top_left_corner_and_runs_clockwise() {
    let mut rasterizer = fixture();
    let border = TextBoxBorder {
        width: 2.0,
        color: LINE,
        dash: Some(TextBoxDash {
            pattern: vec![8.0, 6.0],
            offset: 0.0,
        }),
    };
    let request = boxed("IIIIIIII", shaped(6.0, Some(border)));
    let drawn = rasterizer.rasterize(&request).expect("draws");
    let [left, top, ..] = painted(&drawn);
    let (radius, half_width) = (6.0, 1.0);
    let start = left + half_width + (radius - half_width);
    let row = top + 0.5;
    for offset in [1.0, 4.0, 7.0] {
        assert_eq!(at(&drawn, start + offset, row), LINE, "dash at {offset}");
    }
    for offset in [9.0, 12.0] {
        assert_eq!(at(&drawn, start + offset, row), BOX, "gap at {offset}");
    }
    assert_eq!(at(&drawn, start + 15.0, row), LINE, "the next dash");
}

#[test]
fn a_line_box_spans_the_em_box_of_its_largest_run() {
    let mut rasterizer = fixture();
    let base = boxed("II", TextBackground::padded(BOX, 0.0));
    let request = TextRasterRequest {
        spans: vec![TextSpan {
            bytes: 1..2,
            font_family: None,
            font_weight: 400,
            font_style: FontStyle::Normal,
            font_size: 80.0,
            letter_spacing: 0.0,
            fill: base.fill.clone(),
            strokes: Vec::new(),
            background_color: None,
            underline: false,
            line_through: false,
        }],
        ..base
    };
    let drawn = rasterizer.rasterize(&request).expect("draws");
    let baseline = rasterizer.layout(&request).expect("lays out").lines[0].baseline;
    let [_, top, _, bottom] = painted(&drawn);
    assert_eq!((top, bottom), (baseline - 64.0, baseline + 16.0));
}
