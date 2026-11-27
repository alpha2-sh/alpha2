#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "pixel indices from layout positions"
)]

use super::*;

fn font(path: &str) -> TextFont {
    TextFont {
        family: path.into(),
        bytes: std::fs::read(format!("../../tests/fonts/{path}"))
            .expect("checked-in fixture font")
            .into(),
        face_index: 0,
    }
}

fn rasterizer(paths: &[&str]) -> TextRasterizer {
    TextRasterizer::new(paths.iter().map(|path| font(path)).collect()).expect("fixture fonts")
}

const STYLES: &str = "Alpha2 Fixture Styles";

fn styles() -> TextRasterizer {
    rasterizer(&[
        "Alpha2FixtureStyles.ttf",
        "Alpha2FixtureStylesBold.ttf",
        "Alpha2FixtureStylesItalic.ttf",
        "Alpha2FixtureHangul.ttf",
        "Alpha2Fixture.ttf",
    ])
}

fn request(text: &str, size: f32) -> TextRasterRequest {
    TextRasterRequest {
        text: text.into(),
        font_size: size,
        max_width: 0.0,
        line_height: 1.2,
        align: TextAlign::Start,
        fill: TextFill::Solid([255, 255, 255, 255]),
        emoji_policy: EmojiPolicy::Color,
        content_version: 1,
        strokes: Vec::new(),
        split_background: false,
        background: None,
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

/// A span of `bytes` with `request`'s own values.
fn span(request: &TextRasterRequest, bytes: Range<usize>) -> TextSpan {
    TextSpan {
        bytes,
        font_family: request.font_family.clone(),
        font_weight: request.font_weight,
        font_style: FontStyle::Normal,
        font_size: request.font_size,
        letter_spacing: request.letter_spacing,
        fill: request.fill.clone(),
        strokes: request.strokes.clone(),
        background_color: None,
        underline: false,
        line_through: false,
    }
}

fn sized(request: &TextRasterRequest, bytes: Range<usize>, size: f32) -> TextSpan {
    TextSpan {
        font_size: size,
        ..span(request, bytes)
    }
}

fn with(request: &TextRasterRequest, spans: Vec<TextSpan>) -> TextRasterRequest {
    TextRasterRequest {
        spans,
        content_version: request.content_version + 1,
        ..request.clone()
    }
}

#[test]
fn spans_with_the_base_values_draw_the_plain_bytes() {
    let mut rasterizer = rasterizer(&["Alpha2Fixture.ttf", "Alpha2FixtureHangul.ttf"]);
    let plain = TextRasterRequest {
        max_width: 120.0,
        background: Some(TextBackground::padded([0, 0, 0, 128], 4.0)),
        strokes: vec![TextStroke {
            color: [0, 0, 0, 255],
            width: 2.0,
        }],
        ..request("Alpha2 draws 안녕 text", 24.0)
    };
    let spanned = with(&plain, vec![span(&plain, 0..6), span(&plain, 13..19)]);
    let expected = rasterizer.rasterize(&plain).expect("plain");
    let drawn = rasterizer.rasterize(&spanned).expect("spanned");
    assert_eq!(
        drawn.descriptor,
        TextTexture {
            texture: HostTexture {
                content_version: spanned.content_version,
                ..expected.descriptor.texture.clone()
            },
            ..expected.descriptor.clone()
        }
    );
    assert_eq!(drawn.pixels.rgba8, expected.pixels.rgba8);
    assert_eq!(
        rasterizer.measure(&spanned).expect("measures"),
        rasterizer.measure(&plain).expect("measures")
    );
}

#[test]
fn a_larger_run_rewraps_the_line() {
    let rasterizer = rasterizer(&["Alpha2Fixture.ttf"]);
    let plain = TextRasterRequest {
        max_width: 300.0,
        ..request("one two three", 20.0)
    };
    let fits = rasterizer.measure(&plain).expect("measures");
    assert_eq!(fits.line_count, 1);
    let larger = with(&plain, vec![sized(&plain, 4..7, 150.0)]);
    let layout = rasterizer.layout(&larger).expect("lays out");
    assert_eq!(
        layout.lines.len(),
        3,
        "the large word takes a line of its own"
    );
    assert_eq!(layout.clusters[4].line, 1);
    assert_eq!(rasterizer.measure(&larger).expect("measures").line_count, 3);
    let shaped = rasterizer.shape(&larger).expect("shapes");
    assert_eq!(shaped.lines_at(300.0).line_count, 3);
    assert!(
        shaped.max_content_width()
            > rasterizer
                .shape(&plain)
                .expect("shapes")
                .max_content_width()
    );
}

#[test]
fn a_line_takes_the_inline_box_of_its_largest_run() {
    let rasterizer = rasterizer(&["Alpha2Fixture.ttf"]);
    let base = TextRasterRequest {
        line_height: 1.5,
        ..request("small\nBIG small", 20.0)
    };
    let mixed = with(&base, vec![sized(&base, 6..9, 40.0)]);
    let measured = rasterizer.measure(&mixed).expect("measures");
    let (strut, large) = (20.0_f32, 40.0_f32);
    let first = half_leading(strut, 1.5) + 0.8 * strut;
    let second_top = strut * 1.5;
    let second = second_top + half_leading(large, 1.5) + 0.8 * large;
    assert_eq!(measured.first_baseline, first);
    assert_eq!(measured.last_baseline, second);
    let layout = rasterizer.layout(&mixed).expect("lays out");
    assert_eq!(layout.lines[1].baseline, second);
    assert!(
        layout
            .clusters
            .iter()
            .all(|cluster| cluster.line != 1 || cluster.baseline.to_bits() == second.to_bits()),
        "every cluster of a line shares its baseline"
    );
    let height = strut * 1.5 + (large * 1.5).max(large);
    assert_eq!(f64::from(measured.box_height), f64::from(height.ceil()));
    let shaped = rasterizer.shape(&mixed).expect("shapes").lines_at(0.0);
    assert_eq!(
        (shaped.first_baseline, shaped.last_baseline),
        (first, second)
    );
    assert_eq!(shaped.height, height);
}

#[test]
fn a_line_of_smaller_runs_keeps_the_strut() {
    let rasterizer = rasterizer(&["Alpha2Fixture.ttf"]);
    let plain = request("tiny words\nhere", 30.0);
    let smaller = with(&plain, vec![sized(&plain, 0..10, 10.0)]);
    let (small, base) = (
        rasterizer.measure(&smaller).expect("measures"),
        rasterizer.measure(&plain).expect("measures"),
    );
    assert_eq!(
        (small.first_baseline, small.last_baseline, small.box_height),
        (base.first_baseline, base.last_baseline, base.box_height)
    );
    let layout = rasterizer.layout(&smaller).expect("lays out");
    assert_eq!(layout.clusters[0].font_size, 10.0);
    assert_eq!(layout.lines[0].baseline, base.first_baseline);
}

#[test]
#[allow(clippy::cast_precision_loss)]
fn one_size_lines_keep_the_nominal_formula() {
    for (size, line_height) in [(20.0_f32, 1.2_f32), (37.5, 0.8), (13.0, 2.75)] {
        let boxes = LineBoxes::of(&[size; 4], size, line_height);
        let advance = size * line_height;
        for (index, top) in boxes.tops.iter().enumerate() {
            assert_eq!(
                *top,
                index as f32 * advance + half_leading(size, line_height)
            );
            assert_eq!(boxes.ascents[index], size * 0.8);
        }
        assert_eq!(boxes.height, 3.0 * advance + advance.max(size));
        assert_eq!(
            mixed_line_limit(&[size; 9], line_height, None, boxes.height),
            line_limit(size, line_height, None, boxes.height).map(|limit| limit.min(9))
        );
    }
}

#[test]
fn a_color_only_span_keeps_a_ligature() {
    let rasterizer = rasterizer(&["Alpha2Fixture.ttf"]);
    let plain = request("fit", 40.0);
    assert_eq!(
        rasterizer.layout(&plain).expect("lays out").clusters.len(),
        2
    );
    let colored = with(
        &plain,
        vec![TextSpan {
            fill: TextFill::Solid([255, 0, 0, 255]),
            ..span(&plain, 1..2)
        }],
    );
    let layout = rasterizer.layout(&colored).expect("lays out");
    assert_eq!(layout.clusters.len(), 2, "fi stays one ligature cluster");
    assert_eq!(layout.clusters[0].bytes, 0..2);
    let resized = with(&plain, vec![sized(&plain, 1..2, 44.0)]);
    assert_eq!(
        rasterizer
            .layout(&resized)
            .expect("lays out")
            .clusters
            .len(),
        3,
        "a size change shapes the i apart"
    );
}

fn glyphs(rasterizer: &TextRasterizer, request: &TextRasterRequest) -> Vec<(u16, bool, f32)> {
    let (faces, shaping) = rasterizer.prepare(request).expect("prepares");
    let layout = TextRasterizer::lay_out(&faces, &shaping, request).expect("lays out");
    layout
        .lines
        .iter()
        .flat_map(|line| line.glyphs.iter())
        .map(|glyph| (glyph.glyph_index, glyph.shear, glyph.size))
        .collect()
}

#[test]
fn arabic_joins_across_a_size_change() {
    let rasterizer = rasterizer(&["Alpha2Fixture.ttf"]);
    let plain = request("بيت", 30.0);
    let resized = with(&plain, vec![sized(&plain, 2..6, 36.0)]);
    let ids =
        |glyphs: Vec<(u16, bool, f32)>| glyphs.into_iter().map(|(id, ..)| id).collect::<Vec<_>>();
    let (joined, split) = (
        ids(glyphs(&rasterizer, &plain)),
        ids(glyphs(&rasterizer, &resized)),
    );
    assert_eq!(joined.len(), 3);
    assert_eq!(
        split, joined,
        "each letter keeps the contextual form it has unsplit"
    );
    let isolated = ids(glyphs(&rasterizer, &request("ي", 30.0)));
    assert!(
        !joined.contains(&isolated[0]),
        "the medial yeh is not its isolated form"
    );
}

#[test]
fn italic_narrows_by_style_before_weight() {
    let rasterizer = styles();
    let family = Some(STYLES);
    assert_eq!(rasterizer.font_order(family, FontStyle::Italic, 700)[0], 2);
    assert_eq!(rasterizer.font_order(family, FontStyle::Oblique, 700)[0], 2);
    assert_eq!(
        rasterizer.font_order(family, FontStyle::Normal, 700)[..3],
        [1, 0, 2]
    );
    assert_eq!(
        rasterizer.font_order(family, FontStyle::Normal, 400)[..3],
        [0, 1, 2]
    );
    assert_eq!(
        rasterizer.font_order(family, FontStyle::Italic, 400)[..3],
        [2, 0, 1]
    );
    let base = TextRasterRequest {
        font_family: Some(STYLES.into()),
        ..request("HI", 40.0)
    };
    let italic = with(
        &base,
        vec![TextSpan {
            font_style: FontStyle::Italic,
            font_weight: 700,
            ..span(&base, 0..2)
        }],
    );
    let layout = rasterizer.layout(&italic).expect("lays out");
    assert_eq!(
        layout.clusters[0].advance,
        40.0 * 0.56,
        "the italic face's advance"
    );
    assert!(
        glyphs(&rasterizer, &italic)
            .iter()
            .all(|(_, shear, _)| !shear)
    );
}

#[test]
fn a_face_without_italic_is_sheared_and_keeps_its_advances() {
    let mut rasterizer = rasterizer(&["Alpha2Fixture.ttf"]);
    let plain = request("Il", 60.0);
    let italic = with(
        &plain,
        vec![TextSpan {
            font_style: FontStyle::Italic,
            ..span(&plain, 0..2)
        }],
    );
    let (upright, leaning) = (
        rasterizer.layout(&plain).expect("lays out"),
        rasterizer.layout(&italic).expect("lays out"),
    );
    assert_eq!(
        upright, leaning,
        "advances, lines and positions are unchanged"
    );
    assert!(
        glyphs(&rasterizer, &italic)
            .iter()
            .all(|(_, shear, _)| *shear)
    );
    let (straight, sheared) = (
        rasterizer
            .measure(&plain)
            .expect("measures")
            .ink
            .expect("ink"),
        rasterizer
            .measure(&italic)
            .expect("measures")
            .ink
            .expect("ink"),
    );
    assert!(
        sheared[2] > straight[2] + 60.0 * 0.7 * 0.2,
        "the ink leans right at the top"
    );
    assert_eq!([sheared[1], sheared[3]], [straight[1], straight[3]]);
    let drawn = rasterizer.rasterize(&italic).expect("draws");
    assert_ne!(
        drawn.pixels.rgba8,
        rasterizer.rasterize(&plain).expect("draws").pixels.rgba8
    );
}

#[test]
fn a_hangul_fallback_glyph_leans_beside_a_real_italic() {
    let rasterizer = styles();
    let base = TextRasterRequest {
        font_family: Some(STYLES.into()),
        ..request("HI 안녕", 40.0)
    };
    let italic = with(
        &base,
        vec![TextSpan {
            font_style: FontStyle::Italic,
            ..span(&base, 0..base.text.len())
        }],
    );
    let (faces, shaping) = rasterizer.prepare(&italic).expect("prepares");
    let layout = TextRasterizer::lay_out(&faces, &shaping, &italic).expect("lays out");
    let drawn = layout.lines[0]
        .glyphs
        .iter()
        .map(|glyph| (faces.of[glyph.font].0, glyph.shear))
        .collect::<Vec<_>>();
    assert_eq!(
        drawn[..2],
        [(2, false), (2, false)],
        "Latin draws the real italic"
    );
    assert_eq!(
        drawn[drawn.len() - 2..],
        [(3, true), (3, true)],
        "Hangul from a face without italics leans"
    );
}

#[test]
fn bitmap_glyphs_are_not_sheared() {
    let mut rasterizer = rasterizer(&["Alpha2Fixture.ttf", "Alpha2FixtureEmojiBitmap.ttf"]);
    let plain = request("\u{2764}\u{fe0f}", 48.0);
    let italic = with(
        &plain,
        vec![TextSpan {
            font_style: FontStyle::Italic,
            ..span(&plain, 0..plain.text.len())
        }],
    );
    let upright = rasterizer.rasterize(&plain).expect("draws");
    let leaning = rasterizer.rasterize(&italic).expect("draws");
    assert!(upright.pixels.rgba8.iter().any(|byte| *byte > 0));
    assert_eq!(leaning.pixels.rgba8, upright.pixels.rgba8);
}

#[test]
fn span_ranges_off_character_boundaries_are_invalid() {
    let mut rasterizer = rasterizer(&["Alpha2Fixture.ttf"]);
    let base = request("é and more", 20.0);
    for spans in [
        vec![span(&base, 0..1)],
        vec![span(&base, 0..5), span(&base, 4..8)],
        vec![span(&base, 6..8), span(&base, 0..2)],
        vec![span(&base, 3..3)],
        vec![span(&base, 8..40)],
        vec![sized(&base, 0..2, 0.0)],
        vec![TextSpan {
            letter_spacing: 30.0,
            ..span(&base, 0..2)
        }],
        vec![span(&base, 0..2); MAX_TEXT_SPANS + 1],
    ] {
        let spanned = with(&base, spans);
        assert_eq!(
            rasterizer.rasterize(&spanned),
            Err(TextRasterError::InvalidRequest)
        );
        assert_eq!(
            rasterizer.shape(&spanned).err(),
            Some(TextRasterError::InvalidRequest)
        );
    }
    rasterizer
        .rasterize(&with(&base, vec![span(&base, 0..2), span(&base, 2..6)]))
        .expect("adjacent spans on boundaries");
}

#[test]
fn a_span_draws_a_variable_face_at_its_own_weight() {
    let mut rasterizer = rasterizer(&["Alpha2FixtureVariable.ttf"]);
    let plain = request("I", 100.0);
    let heavy = with(
        &plain,
        vec![TextSpan {
            font_weight: 900,
            ..span(&plain, 0..1)
        }],
    );
    let ink = |result: &TextRasterResult| -> u64 {
        result
            .pixels
            .rgba8
            .chunks(4)
            .map(|pixel| u64::from(pixel[3]))
            .sum()
    };
    let regular = ink(&rasterizer.rasterize(&plain).expect("draws"));
    let bold = ink(&rasterizer.rasterize(&heavy).expect("draws"));
    assert!(
        bold > regular * 2,
        "a 260-unit stem against a 100-unit one: {bold} {regular}"
    );
    let (faces, shaping) = rasterizer.prepare(&heavy).expect("prepares");
    assert_eq!(faces.of[shaping.orders[1][0]], (0, 900, false));
}

fn decorated(request: &TextRasterRequest, bytes: Range<usize>) -> TextSpan {
    TextSpan {
        underline: true,
        line_through: true,
        ..span(request, bytes)
    }
}

fn styled_base(text: &str, size: f32) -> TextRasterRequest {
    TextRasterRequest {
        font_family: Some(STYLES.into()),
        ..request(text, size)
    }
}

#[test]
fn underline_and_line_through_take_the_primary_face_metrics() {
    let mut rasterizer = styles();
    let base = styled_base("HI", 100.0);
    let underlined = with(&base, vec![decorated(&base, 0..2)]);
    let (faces, shaping) = rasterizer.prepare(&underlined).expect("prepares");
    let layout = TextRasterizer::lay_out(&faces, &shaping, &underlined).expect("lays out");
    let paint = layout.paint(&faces, &shaping, underlined.align);
    let baseline = layout.baseline(0);
    let rects = paint
        .decorations
        .iter()
        .map(|decoration| decoration.rect)
        .collect::<Vec<_>>();
    let width = 2.0 * 60.0;
    assert_eq!(
        rects,
        [
            [0.0, baseline + 12.0, width, baseline + 17.0],
            [0.0, baseline - 32.0, width, baseline - 27.0]
        ],
        "post underline -120/50 and OS/2 strikeout 320/50 at 100 px"
    );
    let ink = rasterizer
        .measure(&underlined)
        .expect("measures")
        .ink
        .expect("ink");
    assert_eq!(ink[3], baseline + 17.0, "the underline is the lowest ink");
    let drawn = rasterizer.rasterize(&underlined).expect("draws");
    let row = (drawn.descriptor.layout_y + baseline + 14.0) as usize;
    let at =
        |x: usize| drawn.pixels.rgba8[(row * drawn.descriptor.texture.width as usize + x) * 4 + 3];
    let left = drawn.descriptor.layout_x as usize;
    assert!(
        (left + 2..left + 118).all(|x| at(x) == 255),
        "a solid underline row"
    );
    let bold = with(
        &base,
        vec![TextSpan {
            font_weight: 700,
            ..decorated(&base, 0..2)
        }],
    );
    let (faces, shaping) = rasterizer.prepare(&bold).expect("prepares");
    let layout = TextRasterizer::lay_out(&faces, &shaping, &bold).expect("lays out");
    let rect = layout.paint(&faces, &shaping, bold.align).decorations[0].rect;
    assert_eq!(
        (rect[1] - baseline, rect[3] - rect[1]),
        (13.0, 8.0),
        "the bold face's own values"
    );
}

fn with_underline(bytes: &[u8], position: i16, thickness: i16) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let count = usize::from(u16::from_be_bytes([bytes[4], bytes[5]]));
    let record = (0..count)
        .map(|index| 12 + 16 * index)
        .find(|&at| &bytes[at..at + 4] == b"post")
        .expect("a post table");
    let offset = u32::from_be_bytes(bytes[record + 8..record + 12].try_into().expect("offset"));
    let at = usize::try_from(offset).expect("small");
    out[at + 8..at + 10].copy_from_slice(&position.to_be_bytes());
    out[at + 10..at + 12].copy_from_slice(&thickness.to_be_bytes());
    out
}

#[test]
fn decoration_metrics_fall_back_when_out_of_range() {
    let bytes = std::fs::read("../../tests/fonts/Alpha2FixtureStyles.ttf").expect("fixture");
    let metrics =
        |bytes: &[u8]| paint::decoration_metrics(&ttf_parser::Face::parse(bytes, 0).expect("face"));
    let own = metrics(&bytes);
    assert_eq!((own[0].top, own[0].thickness), (120.0, 50.0));
    assert_eq!((own[1].top, own[1].thickness), (-320.0, 50.0));
    for (position, thickness) in [(-100, 0), (-100, -20), (-100, 251), (-1001, 50), (1200, 50)] {
        let patched = metrics(&with_underline(&bytes, position, thickness));
        assert_eq!(
            (patched[0].top, patched[0].thickness),
            (100.0, 50.0),
            "{position}/{thickness}: 0.1 em below, 0.05 em thick"
        );
        assert_eq!(patched[1], own[1], "the strikeout keeps its own values");
    }
    let edge = metrics(&with_underline(&bytes, -1000, 250));
    assert_eq!((edge[0].top, edge[0].thickness), (1000.0, 250.0));
}

#[test]
fn a_decoration_has_no_seam_between_still_clusters() {
    let mut rasterizer = styles();
    let base = styled_base("HHHHH", 37.0);
    let underlined = with(
        &base,
        vec![TextSpan {
            underline: true,
            ..span(&base, 0..5)
        }],
    );
    let (faces, shaping) = rasterizer.prepare(&underlined).expect("prepares");
    let layout = TextRasterizer::lay_out(&faces, &shaping, &underlined).expect("lays out");
    assert_eq!(
        layout
            .paint(&faces, &shaping, underlined.align)
            .decorations
            .len(),
        1
    );
    let drawn = rasterizer.rasterize(&underlined).expect("draws");
    let texture = &drawn.descriptor;
    let row = (texture.layout_y + layout.baseline(0) + 37.0 * 0.145) as usize;
    let width = texture.texture.width as usize;
    let left = texture.layout_x as usize;
    let advance = 37.0 * 0.6 * 5.0;
    for x in left + 1..left + advance as usize - 1 {
        assert_eq!(
            drawn.pixels.rgba8[(row * width + x) * 4 + 3],
            255,
            "column {x}"
        );
    }
    let mut moving = rasterizer.layout(&underlined).expect("lays out");
    let states = vec![
        ClusterState {
            offset: [0.3, 0.0],
            ..ClusterState::IDENTITY
        };
        std::mem::take(&mut moving.clusters).len()
    ];
    let shifted = rasterizer
        .rasterize_animated(&with(&underlined, underlined.spans.clone()), &states)
        .expect("draws");
    let moved = &shifted.descriptor;
    let (row, width, left) = (
        (moved.layout_y + layout.baseline(0) + 37.0 * 0.145) as usize,
        moved.texture.width as usize,
        moved.layout_x as usize,
    );
    for x in left + 2..left + advance as usize - 1 {
        assert_eq!(
            shifted.pixels.rgba8[(row * width + x) * 4 + 3],
            255,
            "moved column {x}"
        );
    }
}

#[test]
fn a_run_background_covers_its_em_box_per_line() {
    let mut rasterizer = rasterizer(&["Alpha2Fixture.ttf"]);
    let base = TextRasterRequest {
        max_width: 130.0,
        ..request("aa bbb ccc", 30.0)
    };
    let boxed = with(
        &base,
        vec![TextSpan {
            background_color: Some([0, 0, 255, 255]),
            ..span(&base, 3..10)
        }],
    );
    let (faces, shaping) = rasterizer.prepare(&boxed).expect("prepares");
    let layout = TextRasterizer::lay_out(&faces, &shaping, &boxed).expect("lays out");
    assert_eq!(layout.lines.len(), 2);
    let paint = layout.paint(&faces, &shaping, boxed.align);
    assert_eq!(paint.boxes.len(), 2, "one box per line fragment");
    let info = rasterizer.layout(&boxed).expect("lays out");
    for (line, run) in paint.boxes.iter().enumerate() {
        let baseline = layout.baseline(line);
        assert_eq!((run.top, run.bottom), (baseline - 24.0, baseline + 6.0));
        let clusters = info
            .clusters
            .iter()
            .filter(|cluster| cluster.line == u32::try_from(line).expect("small"))
            .filter(|cluster| cluster.bytes.start >= 3 && cluster.advance > 0.0)
            .collect::<Vec<_>>();
        let last = clusters.last().expect("clusters");
        assert_eq!(
            (run.left, run.right),
            (clusters[0].x, last.x + last.advance)
        );
    }
    let drawn = rasterizer.rasterize(&boxed).expect("draws");
    let texture = &drawn.descriptor;
    let pixel = |x: f32, y: f32| {
        let at = ((texture.layout_y + y) as usize * texture.texture.width as usize
            + (texture.layout_x + x) as usize)
            * 4;
        drawn.pixels.rgba8[at..at + 4].to_vec()
    };
    let first = paint.boxes[0];
    assert_eq!(pixel(first.left + 2.0, first.top + 1.0), [0, 0, 255, 255]);
    assert_eq!(pixel(first.left + 2.0, first.top - 2.0), [0, 0, 0, 0]);
    assert_eq!(
        pixel(1.0, first.top + 1.0),
        [0, 0, 0, 0],
        "the unspanned word has none"
    );
}

#[test]
fn eight_strokes_stack_within_the_extent() {
    let mut rasterizer = rasterizer(&["Alpha2Fixture.ttf"]);
    let band = |width: f32, shade: u8| TextStroke {
        color: [shade, shade, shade, 255],
        width,
    };
    let eight = TextRasterRequest {
        strokes: (0..8).map(|index| band(8.0, index * 30)).collect(),
        ..request("I", 40.0)
    };
    let drawn = rasterizer
        .rasterize(&eight)
        .expect("eight bands reaching 64 pixels");
    let plain = rasterizer
        .measure(&TextRasterRequest {
            strokes: Vec::new(),
            ..eight.clone()
        })
        .expect("measures")
        .ink
        .expect("ink");
    let outlined = rasterizer
        .measure(&eight)
        .expect("measures")
        .ink
        .expect("ink");
    assert_eq!(
        outlined[2] - plain[2],
        65.0,
        "the outermost band reaches 64 pixels and a rim"
    );
    assert!(drawn.descriptor.layout_x >= 64.0);
    for strokes in [
        (0..9).map(|_| band(1.0, 0)).collect::<Vec<_>>(),
        (0..8).map(|_| band(8.125, 0)).collect(),
    ] {
        let refused = TextRasterRequest {
            strokes,
            content_version: 2,
            ..eight.clone()
        };
        assert_eq!(
            rasterizer.rasterize(&refused),
            Err(TextRasterError::InvalidRequest)
        );
    }
    let span = TextRasterRequest {
        spans: vec![TextSpan {
            strokes: eight.strokes.clone(),
            ..span(&request("I", 40.0), 0..1)
        }],
        content_version: 3,
        ..request("I", 40.0)
    };
    assert_eq!(
        rasterizer
            .measure(&span)
            .expect("a span with eight bands")
            .ink,
        rasterizer.measure(&eight).expect("measures").ink
    );
}
