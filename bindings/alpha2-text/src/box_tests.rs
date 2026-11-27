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
        highlight: None,
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

fn animated(
    rasterizer: &mut TextRasterizer,
    request: &TextRasterRequest,
    change: impl Fn(&mut [ClusterState]),
) -> Arc<TextRasterResult> {
    static VERSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 41);
    let mut states =
        vec![ClusterState::IDENTITY; rasterizer.layout(request).expect("lays out").clusters.len()];
    change(&mut states);
    rasterizer
        .rasterize_animated(
            &TextRasterRequest {
                content_version: VERSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                ..request.clone()
            },
            &states,
        )
        .expect("draws")
}

fn extent(extent: BoxExtent) -> TextBackground {
    TextBackground {
        extent,
        ..TextBackground::padded(BOX, 0.0)
    }
}

#[test]
fn a_tracked_line_box_follows_its_clusters() {
    let mut rasterizer = fixture();
    let request = boxed("IIII", extent(BoxExtent::Line));
    let plain = painted(&rasterizer.rasterize(&request).expect("draws"));
    let tracked = painted(&animated(&mut rasterizer, &request, |states| {
        for state in states.iter_mut() {
            state.tracking = 10.0;
        }
    }));
    assert_eq!(tracked[0], plain[0]);
    assert_eq!(tracked[2], plain[2] + 30.0, "three gaps of 10 pixels");
}

#[test]
fn a_revealed_box_spans_only_revealed_clusters() {
    let mut rasterizer = fixture();
    let request = boxed("II II", extent(BoxExtent::Revealed));
    let layout = rasterizer.layout(&request).expect("lays out");
    let drawn = animated(&mut rasterizer, &request, |states| {
        states[1].opacity = 0.0;
        for state in &mut states[3..] {
            state.scale = [0.0, 0.0];
        }
    });
    let [left, _, right, _] = painted(&drawn);
    let first = &layout.clusters[0];
    assert_eq!(
        (left, right),
        (first.x.floor(), (first.x + first.advance).ceil())
    );
    let middle = animated(&mut rasterizer, &request, |states| {
        states[1].opacity = 0.0;
        states[4].opacity = 0.0;
    });
    let [left, _, right, _] = painted(&middle);
    let fourth = &layout.clusters[3];
    assert_eq!(
        (left, right),
        (first.x.floor(), (fourth.x + fourth.advance).ceil()),
        "a hidden cluster between shown ones is covered; the space at the end is not counted"
    );
}

#[test]
fn a_line_with_nothing_revealed_has_no_box() {
    let mut rasterizer = fixture();
    let request = boxed("II\nII", extent(BoxExtent::Revealed));
    let drawn = animated(&mut rasterizer, &request, |states| {
        for state in &mut states[..3] {
            state.opacity = 0.0;
        }
    });
    let [_, top, _, _] = painted(&drawn);
    assert_eq!(top, 40.0, "only the second line has a box");
}

#[test]
fn a_revealed_box_without_animators_is_the_line_box() {
    let mut rasterizer = fixture();
    let line = rasterizer
        .rasterize(&boxed(" II ", extent(BoxExtent::Line)))
        .expect("draws");
    let revealed = rasterizer
        .rasterize(&TextRasterRequest {
            content_version: 2,
            ..boxed(" II ", extent(BoxExtent::Revealed))
        })
        .expect("draws");
    assert_eq!(line.pixels.rgba8, revealed.pixels.rgba8);
    let request = boxed(" II ", extent(BoxExtent::Revealed));
    let shown = animated(&mut rasterizer, &request, |states| {
        states[1].tracking = 0.0;
        states[1].offset = [0.0, 0.0];
        states[1].fill = Some(ColorMix {
            keep: 0.5,
            add: [0.0; 4],
        });
    });
    assert_eq!(
        painted(&shown),
        painted(&line),
        "every cluster shows, so the line box"
    );
}

#[test]
fn a_block_box_spans_the_layout_box() {
    let mut rasterizer = fixture();
    let request = TextRasterRequest {
        max_width: 300.0,
        ..boxed("II\nIIII", extent(BoxExtent::Block))
    };
    let drawn = rasterizer.rasterize(&request).expect("draws");
    assert_eq!(painted(&drawn), [0.0, 0.0, 300.0, 80.0]);
    let hidden = animated(&mut rasterizer, &request, |states| {
        for state in states.iter_mut() {
            state.opacity = 0.0;
        }
    });
    assert_eq!(
        painted(&hidden),
        [0.0, 0.0, 300.0, 80.0],
        "whatever is revealed"
    );
}

#[test]
fn run_backgrounds_follow_the_reveal() {
    let mut rasterizer = fixture();
    let base = boxed("IIII", extent(BoxExtent::Revealed));
    let request = TextRasterRequest {
        background: None,
        spans: vec![TextSpan {
            bytes: 0..4,
            font_family: None,
            font_weight: 400,
            font_style: FontStyle::Normal,
            font_size: 40.0,
            letter_spacing: 0.0,
            fill: base.fill.clone(),
            strokes: Vec::new(),
            background_color: Some(LINE),
            underline: false,
            line_through: false,
        }],
        ..base.clone()
    };
    let layout = rasterizer.layout(&request).expect("lays out");
    let tracked = animated(&mut rasterizer, &request, |states| {
        for state in states.iter_mut() {
            state.tracking = 10.0;
        }
    });
    let last = &layout.clusters[3];
    assert_eq!(
        painted(&tracked)[2],
        (last.x + last.advance + 30.0).ceil(),
        "it follows tracking"
    );
    let revealing = TextRasterRequest {
        background: Some(TextBackground {
            color: [0; 4],
            ..extent(BoxExtent::Revealed)
        }),
        ..request.clone()
    };
    let half = animated(&mut rasterizer, &revealing, |states| {
        states[2].opacity = 0.0;
        states[3].opacity = 0.0;
    });
    let second = &layout.clusters[1];
    assert_eq!(
        painted(&half)[2],
        (second.x + second.advance).ceil(),
        "under a revealed box it is drawn behind revealed clusters only"
    );
}

#[test]
fn highlight_steps_fold_to_the_eased_rectangle() {
    let mut rasterizer = fixture();
    let words = "II IIII";
    let highlight = |steps: Vec<(std::ops::Range<usize>, f32)>| TextRasterRequest {
        background: None,
        highlight: Some(TextHighlightRequest {
            look: TextBackground::padded(BOX, 0.0),
            steps,
        }),
        ..boxed(words, extent(BoxExtent::Line))
    };
    let layout = rasterizer.layout(&highlight(Vec::new())).expect("lays out");
    let span = |first: usize, last: usize| {
        let (start, end) = (&layout.clusters[first], &layout.clusters[last]);
        (start.x, end.x + end.advance)
    };
    let (first, second) = (span(0, 1), span(3, 6));
    let mut at = |progress: f32| {
        painted(
            &rasterizer
                .rasterize(&TextRasterRequest {
                    content_version: u64::from(progress.to_bits()) + 1,
                    ..highlight(vec![(0..2, 1.0), (3..7, progress)])
                })
                .expect("draws"),
        )
    };
    let start = at(0.0);
    assert_eq!((start[0], start[2]), (first.0.floor(), first.1.ceil()));
    let end = at(1.0);
    assert_eq!((end[0], end[2]), (second.0.floor(), second.1.ceil()));
    let half = at(0.5);
    let middle = |a: f32, b: f32| f32::midpoint(a, b);
    assert_eq!(
        (half[0], half[2]),
        (
            middle(first.0, second.0).floor(),
            middle(first.1, second.1).ceil()
        )
    );
    assert_eq!((half[1], half[3]), (0.0, 40.0), "the em box down");
}
