use super::*;

fn fixture() -> TextRasterizer {
    let bytes =
        std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("checked-in fixture font");
    TextRasterizer::new(vec![TextFont {
        family: "Alpha2 Fixture".into(),
        bytes: bytes.into(),
        face_index: 0,
    }])
    .expect("valid fixture")
}

const QUICK: &str = "The quick brown fox jumps over the lazy dog again and again";

fn request(text: &str, max_width: f32, clamp: Option<TextClamp>) -> TextRasterRequest {
    TextRasterRequest {
        text: text.into(),
        font_size: 24.0,
        max_width,
        line_height: 1.2,
        align: TextAlign::Start,
        fill: TextFill::Solid([255, 255, 255, 255]),
        emoji_policy: EmojiPolicy::Color,
        content_version: 1,
        strokes: Vec::new(),
        background: None,
        split_background: false,
        font_family: None,
        letter_spacing: 0.0,
        font_weight: 400,
        clamp,
    }
}

fn lines(max_lines: u32) -> TextClamp {
    TextClamp {
        max_lines: Some(max_lines),
        max_height: 0.0,
        ellipsis: Some("\u{2026}".into()),
    }
}

/// The ellipsis cluster of a layout, which must be its last.
fn ellipsis(layout: &TextLayout) -> &ClusterInfo {
    let last = layout.clusters.last().expect("clusters");
    assert!(last.ellipsis, "the last cluster is the ellipsis");
    assert_eq!(
        layout
            .clusters
            .iter()
            .filter(|cluster| cluster.ellipsis)
            .count(),
        1
    );
    last
}

#[test]
fn a_line_limit_counts_lines_and_the_height_and_keeps_one_line() {
    // Lines are 24 * 1.2 = 28.8 apart; one line's box is 28.8 tall.
    assert_eq!(line_limit(24.0, 1.2, None, 0.0), None);
    assert_eq!(line_limit(24.0, 1.2, Some(3), 0.0), Some(3));
    assert_eq!(line_limit(24.0, 1.2, Some(0), 0.0), Some(1));
    assert_eq!(line_limit(24.0, 1.2, None, 28.8), Some(1));
    assert_eq!(line_limit(24.0, 1.2, None, 57.7), Some(2));
    assert_eq!(line_limit(24.0, 1.2, None, 57.5), Some(1));
    assert_eq!(line_limit(24.0, 1.2, None, 1.0), Some(1));
    assert_eq!(line_limit(24.0, 1.2, Some(1), 1_000.0), Some(1));
    assert_eq!(line_limit(24.0, 1.2, Some(9), 86.5), Some(3));
    // Lines closer than an em still need an em for the last line.
    assert_eq!(line_limit(24.0, 0.5, None, 24.0 + 12.0), Some(2));
    assert_eq!(line_limit(24.0, 0.5, None, 24.0 + 11.9), Some(1));
}

#[test]
fn a_line_limit_is_the_largest_count_whose_box_fits() {
    for font_size in [1.0_f32, 7.3, 24.0, 113.0] {
        for line_height in [0.5_f32, 0.9, 1.0, 1.2, 3.0] {
            for max_height in [1.0_f32, 30.0, 99.9, 1_000.0, 16_000.0] {
                let limit = line_limit(font_size, line_height, None, max_height).expect("limit");
                let advance = font_size * line_height;
                let fits = |count| box_height(count, advance, font_size) <= max_height;
                assert!(
                    limit == 1 || fits(limit),
                    "{font_size} {line_height} {max_height}"
                );
                assert!(!fits(limit + 1), "{font_size} {line_height} {max_height}");
            }
        }
    }
}

#[test]
fn a_clamp_keeps_its_lines_and_ends_the_last_with_the_ellipsis() {
    let rasterizer = fixture();
    let free = rasterizer
        .measure(&request(QUICK, 200.0, None))
        .expect("measures");
    assert!(free.line_count > 2 && !free.truncated);
    let clamped = request(QUICK, 200.0, Some(lines(2)));
    let measure = rasterizer.measure(&clamped).expect("measures");
    assert_eq!(measure.line_count, 2);
    assert!(measure.truncated);
    assert_eq!(measure.box_height, 58);
    let layout = rasterizer.layout(&clamped).expect("lays out");
    let mark = ellipsis(&layout);
    assert_eq!(mark.line, 1);
    assert_eq!(mark.word, None);
    assert!(mark.bytes.is_empty());
    // It follows the last kept word, cut at a word's end, and the line still fits.
    let kept = &QUICK[..mark.bytes.start];
    assert!(kept.ends_with(' ') || QUICK[mark.bytes.start..].starts_with(' '));
    let line = &layout.lines[1];
    assert!(line.left + line.width <= 200.0 + 1.0e-3);
    assert!(mark.x + mark.advance <= 200.0 + 1.0e-3);
    assert!(
        layout.clusters[..layout.clusters.len() - 1]
            .iter()
            .filter(|cluster| cluster.line == 1)
            .all(|cluster| cluster.x + cluster.advance <= mark.x + 1.0e-3)
    );
    assert!(
        rasterizer
            .layout(&clamped)
            .expect("lays out")
            .clusters
            .len()
            < QUICK.len()
    );
}

#[test]
fn text_within_its_clamp_draws_as_without_one() {
    let mut rasterizer = fixture();
    let free = rasterizer
        .rasterize(&request(QUICK, 200.0, None))
        .expect("draws");
    let lines_needed = rasterizer
        .measure(&request(QUICK, 200.0, None))
        .expect("measures")
        .line_count;
    let mut within = request(QUICK, 200.0, Some(lines(lines_needed)));
    within.content_version = 2;
    let clamped = rasterizer.rasterize(&within).expect("draws");
    assert_eq!(clamped.pixels.rgba8, free.pixels.rgba8);
    assert!(!rasterizer.measure(&within).expect("measures").truncated);
    assert!(
        !rasterizer
            .layout(&within)
            .expect("lays out")
            .clusters
            .iter()
            .any(|cluster| cluster.ellipsis)
    );
}

#[test]
fn blank_lines_after_the_clamp_point_cut_nothing_but_text_after_them_does() {
    let rasterizer = fixture();
    let blank = rasterizer
        .measure(&request("one\n\n\n", 0.0, Some(lines(1))))
        .expect("measures");
    assert_eq!((blank.line_count, blank.truncated), (1, false));
    let measure = rasterizer
        .measure(&request("one\n\nthree", 0.0, Some(lines(2))))
        .expect("measures");
    assert_eq!((measure.line_count, measure.truncated), (2, true));
    // The kept blank line holds only the ellipsis.
    let layout = rasterizer
        .layout(&request("one\n\nthree", 0.0, Some(lines(2))))
        .expect("lays out");
    let mark = ellipsis(&layout);
    assert_eq!(mark.line, 1);
    assert_eq!(mark.bytes, 4..4);
    assert_eq!(layout.lines[1].clusters.len(), 1);
}

#[test]
fn without_wrapping_the_ellipsis_follows_the_whole_line() {
    let rasterizer = fixture();
    let layout = rasterizer
        .layout(&request("first line\nsecond", 0.0, Some(lines(1))))
        .expect("lays out");
    let mark = ellipsis(&layout);
    assert_eq!(mark.bytes, 10..10);
    // "fi" is one ligature cluster, so the line's last letter is the one before the ellipsis.
    let last = &layout.clusters[layout.clusters.len() - 2];
    assert_eq!(last.bytes, 9..10);
    assert!((mark.x - (last.x + last.advance)).abs() < 1.0e-3);
}

#[test]
fn a_word_wider_than_the_line_is_cut_between_clusters() {
    let rasterizer = fixture();
    let text = "Internationalization is long";
    let layout = rasterizer
        .layout(&request(text, 150.0, Some(lines(1))))
        .expect("lays out");
    let mark = ellipsis(&layout);
    assert!(mark.bytes.start > 0 && mark.bytes.start < "Internationalization".len());
    assert!(mark.x + mark.advance <= 150.0 + 1.0e-3);
}

#[test]
fn a_cut_line_keeps_whole_words_when_one_fits() {
    let rasterizer = fixture();
    // The second line is "three four"; " (more)" fits after "three" but not after "four".
    let text = "one two three four five six seven eight";
    for (width, ellipsis_text) in [(160.0, " (more)"), (200.0, "\u{2026}")] {
        let clamp = Some(TextClamp {
            max_lines: Some(2),
            max_height: 0.0,
            ellipsis: Some(ellipsis_text.into()),
        });
        let layout = rasterizer
            .layout(&request(text, width, clamp))
            .expect("lays out");
        let cut = ellipsis(&layout).bytes.start;
        assert!(
            text[..cut].ends_with(' ') || text[cut..].starts_with(' '),
            "{width}: cut at {cut} of {text:?}"
        );
    }
}

#[test]
fn a_clamp_by_height_keeps_the_lines_that_fit() {
    let rasterizer = fixture();
    let clamp = Some(TextClamp {
        max_lines: None,
        max_height: 60.0,
        ellipsis: None,
    });
    let measure = rasterizer
        .measure(&request(QUICK, 200.0, clamp.clone()))
        .expect("measures");
    assert_eq!((measure.line_count, measure.truncated), (2, true));
    assert!(measure.box_height <= 60);
    // No ellipsis: the kept lines are laid out as they broke.
    let free = rasterizer
        .layout(&request(QUICK, 200.0, None))
        .expect("lays out");
    let cut = rasterizer
        .layout(&request(QUICK, 200.0, clamp))
        .expect("lays out");
    assert_eq!(cut.lines[..], free.lines[..2]);
    assert_eq!(cut.clusters[..], free.clusters[..cut.clusters.len()]);
}

#[test]
fn right_to_left_text_ends_with_the_ellipsis_at_its_left() {
    let rasterizer = fixture();
    let text = "\u{05e9}\u{05dc}\u{05d5}\u{05dd} \u{05e2}\u{05d5}\u{05dc}\u{05dd} \u{05de}\u{05d4} \u{05e9}\u{05dc}\u{05d5}\u{05de}\u{05da}";
    let layout = rasterizer
        .layout(&request(text, 160.0, Some(lines(1))))
        .expect("lays out");
    let mark = ellipsis(&layout);
    let line = &layout.lines[0];
    assert!((mark.x - line.left).abs() < 1.0e-3, "{mark:?} {line:?}");
    // The first letter stays rightmost.
    assert!(
        layout.clusters[..layout.clusters.len() - 1]
            .iter()
            .filter(|cluster| !cluster.is_space)
            .all(|cluster| cluster.x <= layout.clusters[0].x)
    );
}

#[test]
fn a_cut_never_splits_a_letter_from_its_marks() {
    let rasterizer = fixture();
    // Each "e" carries a combining acute accent.
    let word = "e\u{0301}".repeat(30);
    let layout = rasterizer
        .layout(&request(&word, 100.0, Some(lines(1))))
        .expect("lays out");
    let cut = ellipsis(&layout).bytes.start;
    assert_eq!(cut % "e\u{0301}".len(), 0, "cut at byte {cut}");
}

#[test]
fn letter_spacing_separates_the_last_kept_letter_from_the_ellipsis() {
    let rasterizer = fixture();
    let mut spaced = request("spaced letters wrap here", 200.0, Some(lines(1)));
    spaced.letter_spacing = 4.0;
    let layout = rasterizer.layout(&spaced).expect("lays out");
    let mark = ellipsis(&layout);
    let before = &layout.clusters[layout.clusters.len() - 2];
    let before = if before.is_space {
        &layout.clusters[layout.clusters.len() - 3]
    } else {
        before
    };
    assert!((mark.x - (before.x + before.advance) - 4.0).abs() < 1.0e-3);
}

#[test]
fn an_animated_clamped_text_takes_a_state_for_its_ellipsis() {
    let mut rasterizer = fixture();
    let clamped = request(QUICK, 200.0, Some(lines(1)));
    let count = rasterizer
        .layout(&clamped)
        .expect("lays out")
        .clusters
        .len();
    let mut states = vec![ClusterState::IDENTITY; count];
    states[count - 1].opacity = 0.0;
    let mut hidden = clamped.clone();
    hidden.content_version = 2;
    let drawn = rasterizer
        .rasterize_animated(&hidden, &states)
        .expect("draws");
    let plain = rasterizer.rasterize(&clamped).expect("draws");
    assert_ne!(drawn.pixels.rgba8, plain.pixels.rgba8);
    assert!(
        rasterizer
            .rasterize_animated(&hidden, &states[..count - 1])
            .is_err()
    );
}

#[test]
fn shaped_lines_agree_with_the_clamped_layout() {
    let rasterizer = fixture();
    let shaped = rasterizer
        .shape(&request(QUICK, 0.0, None))
        .expect("shapes");
    for width in [80.0_f32, 150.0, 200.0, 333.0, 0.0] {
        for limit in [1_u32, 2, 3, 9] {
            let clamped = request(QUICK, width, Some(lines(limit)));
            let measure = rasterizer.measure(&clamped).expect("measures");
            let within = shaped.lines_within(width, Some(limit));
            assert_eq!(
                within.lines.line_count, measure.line_count,
                "{width} {limit}"
            );
            assert_eq!(within.truncated, measure.truncated, "{width} {limit}");
            assert_eq!(
                f64::from(within.lines.height.ceil()),
                f64::from(measure.box_height)
            );
        }
    }
    // A kept line that splits a word splits it; words cut away do not count.
    let long = rasterizer
        .shape(&request("short Internationalization", 0.0, None))
        .expect("shapes");
    assert!(!long.lines_within(150.0, Some(1)).splits_word);
    assert!(long.lines_within(150.0, Some(2)).splits_word);
    assert!(long.lines_within(150.0, None).splits_word);
}

#[test]
fn a_clamp_out_of_range_is_refused() {
    let rasterizer = fixture();
    let refused = |clamp: TextClamp| {
        rasterizer
            .measure(&request(QUICK, 200.0, Some(clamp)))
            .expect_err("refused")
    };
    let base = TextClamp {
        max_lines: Some(2),
        max_height: 0.0,
        ellipsis: None,
    };
    for clamp in [
        TextClamp {
            max_lines: Some(0),
            ..base.clone()
        },
        TextClamp {
            max_height: f32::NAN,
            ..base.clone()
        },
        TextClamp {
            ellipsis: Some("a\nb".into()),
            ..base.clone()
        },
        TextClamp {
            ellipsis: Some("x".repeat(MAX_ELLIPSIS_BYTES + 1)),
            ..base
        },
    ] {
        assert_eq!(refused(clamp), TextRasterError::InvalidRequest);
    }
}
