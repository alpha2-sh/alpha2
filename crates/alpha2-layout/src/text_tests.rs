use super::*;
use alpha2_model::{TextStyle, parse_srt, parse_webvtt};
use alpha2_text::TextFont;

#[test]
fn imported_cues_whose_last_line_is_blank_after_markup_are_drawn() {
    // Lines that end a cue only once markup or references are read: blank or zero width.
    let srt = "1\n00:00:01,000 --> 00:00:02,000\nHello\n<i> </i>\n\n\
                   2\n00:00:03,000 --> 00:00:04,000\nHello\n{\\an8} \n\n\
                   3\n00:00:05,000 --> 00:00:06,000\nHi\n\u{200b}\n\n\
                   4\n00:00:07,000 --> 00:00:08,000\nHello\n<font color=\"#ffffff\"> </font>\n";
    let vtt = "WEBVTT\n\n00:01.000 --> 00:02.000\nHello\n&nbsp;\n\n\
                   00:03.000 --> 00:04.000\nשלום\n&lrm;\n\n\
                   00:05.000 --> 00:06.000\nHello\n\u{feff}\n";
    let cues = [
        parse_srt(srt).expect("SRT").cues,
        parse_webvtt(vtt).expect("WebVTT").cues,
    ]
    .concat();
    assert_eq!(cues.len(), 7);
    let mut rasterizer = TextRasterizer::new(vec![TextFont {
        family: "Alpha2 Fixture".into(),
        bytes: std::fs::read("../../tests/fonts/Alpha2Fixture.ttf")
            .expect("fixture font")
            .into(),
        face_index: 0,
    }])
    .expect("font");
    let size = PictureSize::new(1920, 1080).expect("size");
    for style in [
        TextStyle::default(),
        TextStyle {
            strokes: vec![alpha2_model::TextStroke {
                color: Rgba::new(0.0, 0.0, 0.0, 1.0),
                width: 0.08,
            }],
            ..TextStyle::default()
        },
    ] {
        for cue in &cues {
            let text = cue.text.text();
            let key = text_key(&text, &style, size);
            let raster = rasterizer
                .rasterize(&text_request(&text, &style, size, key))
                .unwrap_or_else(|error| panic!("{:?}: {error}", cue.text));
            assert_eq!(raster.descriptor.line_count, 2, "{:?}", cue.text);
        }
    }
}

#[test]
fn portrait_text_sizes_by_the_shorter_side() {
    let style = TextStyle {
        font_size: 0.05,
        ..TextStyle::default()
    };
    for (width, height) in [(540, 960), (960, 540)] {
        let size = PictureSize::new(width, height).expect("size");
        let request = text_request("Hi", &style, size, text_key("Hi", &style, size));
        assert_eq!(
            request.font_size.to_bits(),
            27.0_f32.to_bits(),
            "{width}x{height}"
        );
    }
}

#[test]
fn baselines_lie_a_half_leading_and_an_ascent_below_each_line_top() {
    let rasterizer = TextRasterizer::new(vec![TextFont {
        family: "Alpha2 Fixture".into(),
        bytes: std::fs::read("../../tests/fonts/Alpha2Fixture.ttf")
            .expect("fixture font")
            .into(),
        face_index: 0,
    }])
    .expect("font");
    // 40 px lines 56 px apart on a 1000 px tall canvas: 8 px of half-leading, 32 px of ascent.
    let size = PictureSize::new(1000, 1000).expect("size");
    let style = TextStyle {
        font_size: 0.04,
        line_height: 1.4,
        ..TextStyle::default()
    };
    let one = measure_text(&rasterizer, "Hg", &style, size).expect("measures");
    assert_eq!(one.baselines.first.to_bits(), one.baselines.last.to_bits());
    assert!((one.baselines.first - one.layout.top - 0.04).abs() < 1e-6);
    let three = measure_text(&rasterizer, "Hg\nHg\nHg", &style, size).expect("measures");
    assert_eq!(three.line_count, 3);
    assert!(
        (three.baselines.last - three.baselines.first - 2.0 * 1.4 * 40.0 / 1000.0).abs() < 1e-6
    );
}

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

fn shape(
    rasterizer: &TextRasterizer,
) -> impl FnMut(&TextRasterRequest) -> Result<std::sync::Arc<ShapedText>, TextRasterError> + '_ {
    |request| rasterizer.shape(request).map(std::sync::Arc::new)
}

const QUICK: &str = "The quick brown fox jumps over the lazy dog again and again";

#[test]
fn a_fitted_word_no_longer_splits_and_the_next_step_would() {
    let rasterizer = fixture();
    let size = PictureSize::new(1920, 1080).expect("size");
    let style = TextStyle {
        font_size: 0.2,
        max_width: 0.3,
        font_size_fit: Some(alpha2_model::FontSizeFit {
            min: 0.01,
            max: None,
        }),
        ..TextStyle::default()
    };
    let word = "Internationalization";
    let fitted = fit_text(word, &style, size, shape(&rasterizer)).expect("fits");
    assert!(fitted.font_size < 0.2 && fitted.font_size > 0.01);
    let width = |pixels| {
        rasterizer
            .shape(&text_request_at(word, &style, size, 1, pixels))
            .expect("shapes")
            .min_content_width()
    };
    assert!(width(fitted.font_pixels) <= 0.3 * 1920.0);
    let next = (fitted.font_size + 1.0 / alpha2_model::FONT_SIZE_FIT_STEPS) * size.unit();
    assert!(width(next) > 0.3 * 1920.0);
    let bounds = measure_text(&rasterizer, word, &style, size).expect("measures");
    assert_eq!(bounds.font_size, fitted.font_size);
    assert_eq!(bounds.line_count, 1);
    assert!(!bounds.overflow && !bounds.truncated);
}

#[test]
fn a_fit_that_needs_no_change_draws_the_style_as_it_is() {
    let rasterizer = fixture();
    let size = PictureSize::new(1920, 1080).expect("size");
    let plain = TextStyle::default();
    let fitted = TextStyle {
        font_size_fit: Some(alpha2_model::FontSizeFit {
            min: 0.01,
            max: None,
        }),
        ..TextStyle::default()
    };
    let found = fit_text("Hi", &fitted, size, shape(&rasterizer)).expect("fits");
    assert_eq!(found.font_size, plain.font_size);
    assert_eq!(found.font_pixels, font_pixels(&plain, size));
    let unfitted = fit_text("Hi", &plain, size, |_: &TextRasterRequest| {
        Err::<std::sync::Arc<ShapedText>, _>(TextRasterError::NoFonts)
    })
    .expect("a style without a fit shapes nothing");
    assert_eq!(unfitted, found);
}

#[test]
fn text_fits_to_its_box_height_and_grows_to_its_maximum() {
    let rasterizer = fixture();
    let size = PictureSize::new(1920, 1080).expect("size");
    let shrink = TextStyle {
        font_size: 0.12,
        max_width: 0.5,
        max_height: Some(0.3),
        font_size_fit: Some(alpha2_model::FontSizeFit {
            min: 0.02,
            max: None,
        }),
        ..TextStyle::default()
    };
    let bounds = measure_text(&rasterizer, QUICK, &shrink, size).expect("measures");
    assert!(bounds.font_size < 0.12);
    assert!(bounds.layout.bottom - bounds.layout.top <= 0.3 + 1.0e-6);
    assert!(!bounds.overflow);
    let grow = TextStyle {
        font_size: 0.05,
        max_width: 0.8,
        max_height: Some(0.4),
        font_size_fit: Some(alpha2_model::FontSizeFit {
            min: 0.02,
            max: Some(0.5),
        }),
        ..TextStyle::default()
    };
    let bounds = measure_text(&rasterizer, "Big", &grow, size).expect("measures");
    assert!(bounds.font_size > 0.3, "{}", bounds.font_size);
    assert_eq!(bounds.line_count, 1);
}

#[test]
fn text_that_fits_at_no_size_is_clamped_at_its_minimum_and_reported() {
    let rasterizer = fixture();
    let size = PictureSize::new(1920, 1080).expect("size");
    let style = TextStyle {
        font_size: 0.1,
        max_width: 0.3,
        font_size_fit: Some(alpha2_model::FontSizeFit {
            min: 0.06,
            max: None,
        }),
        line_clamp: Some(alpha2_model::LineClamp {
            max_lines: Some(2),
            ..alpha2_model::LineClamp::default()
        }),
        ..TextStyle::default()
    };
    let bounds = measure_text(&rasterizer, QUICK, &style, size).expect("measures");
    assert_eq!(bounds.font_size, 0.06);
    assert_eq!(bounds.line_count, 2);
    assert!(bounds.truncated && !bounds.overflow);
    let unclamped = TextStyle {
        line_clamp: None,
        max_height: Some(0.1),
        ..style
    };
    let bounds = measure_text(&rasterizer, QUICK, &unclamped, size).expect("measures");
    assert!(bounds.overflow && !bounds.truncated);
}

#[test]
fn a_text_key_changes_with_the_box_height_fit_and_clamp() {
    let size = PictureSize::new(1920, 1080).expect("size");
    let base = TextStyle::default();
    let key = text_key("Hi", &base, size);
    let styles = [
        TextStyle {
            max_height: Some(0.5),
            ..base.clone()
        },
        TextStyle {
            font_size_fit: Some(alpha2_model::FontSizeFit {
                min: 0.01,
                max: None,
            }),
            ..base.clone()
        },
        TextStyle {
            line_clamp: Some(alpha2_model::LineClamp::default()),
            ..base.clone()
        },
        TextStyle {
            line_clamp: Some(alpha2_model::LineClamp {
                block_ellipsis: alpha2_model::BlockEllipsis::NoEllipsis,
                ..alpha2_model::LineClamp::default()
            }),
            ..base.clone()
        },
    ];
    let keys = styles
        .iter()
        .map(|style| text_key("Hi", style, size))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(keys.len(), styles.len());
    assert!(!keys.contains(&key));
}

#[test]
fn a_clamped_style_asks_the_text_host_to_cut_at_its_box() {
    let size = PictureSize::new(1920, 1080).expect("size");
    let style = TextStyle {
        max_height: Some(0.25),
        line_clamp: Some(alpha2_model::LineClamp {
            max_lines: Some(3),
            block_ellipsis: alpha2_model::BlockEllipsis::String(" (more)".into()),
        }),
        ..TextStyle::default()
    };
    let request = text_request(QUICK, &style, size, 1);
    assert_eq!(
        request.clamp,
        Some(alpha2_text::TextClamp {
            max_lines: Some(3),
            max_height: 0.25 * 1080.0,
            ellipsis: Some(" (more)".into()),
        })
    );
    assert_eq!(
        text_request(QUICK, &TextStyle::default(), size, 1).clamp,
        None
    );
}

const KEYED: &str =
    r#""font_size": 0.2, "position": { "x": 0.5, "y": 0.5 }, "anchor": { "x": 0.5, "y": 0.5 }"#;

fn keyed_style(extra: &str) -> TextStyle {
    serde_json::from_str(&format!("{{ {KEYED}{extra} }}")).expect("a text style")
}

#[test]
fn animated_keys_differ_for_styles_that_differ_only_in_an_animator() {
    let size = PictureSize::new(640, 360).expect("size");
    let content = alpha2_model::TextContent::Plain("Hello".to_owned());
    let time = alpha2_time::MediaTime::from_ticks(30_000);
    let animated = |animators: &str| keyed_style(&format!(", \"animators\": {animators}"));
    let pairs = [
        (
            r#"[ { "selector": { "type": "range" }, "properties": { "offset": { "mode": "constant", "value": { "x": 0, "y": 0.1 } } } } ]"#,
            r#"[ { "selector": { "type": "range" }, "properties": { "offset": { "mode": "constant", "value": { "x": 0, "y": 0.3 } } } } ]"#,
        ),
        (
            r#"[ { "selector": { "type": "range", "shape": "square" }, "properties": { "opacity": { "mode": "constant", "value": 0 } } } ]"#,
            r#"[ { "selector": { "type": "range", "shape": "ramp_up" }, "properties": { "opacity": { "mode": "constant", "value": 0 } } } ]"#,
        ),
        (
            r##"[ { "selector": { "type": "range" }, "properties": { "fill": { "mode": "constant", "value": "#ff0000" } } } ]"##,
            r##"[ { "selector": { "type": "range" }, "properties": { "fill": { "mode": "constant", "value": "#00ff00" } } } ]"##,
        ),
        (
            r##"[ { "selector": { "type": "range" }, "properties": { "fill": { "mode": "constant", "value": "#ff0000" } } } ]"##,
            r##"[ { "selector": { "type": "range" }, "properties": { "fill": { "mode": "constant", "value": "#ff0000" } } },
                  { "selector": { "type": "range" }, "properties": { "scale": { "mode": "constant", "value": { "x": 2, "y": 2 } } } } ]"##,
        ),
    ];
    for (first, second) in pairs {
        let (first, second) = (animated(first), animated(second));
        assert_ne!(
            animated_text_key(&content, &first, size, time),
            animated_text_key(&content, &second, size, time),
            "{:?} and {:?}",
            first.animators,
            second.animators
        );
        assert_eq!(
            animated_text_key(&content, &first, size, time),
            animated_text_key(&content, &first.clone(), size, time)
        );
    }
}

#[test]
fn the_request_hash_covers_the_background_box_and_the_emoji_policy() {
    let size = PictureSize::new(640, 360).expect("size");
    let request = |extra: &str| text_request("Hello", &keyed_style(extra), size, 1);
    let keys = [
        request(r##", "background": { "color": "#ff0000", "padding": 0.2 }"##),
        request(r##", "background": { "color": "#0000ff", "padding": 0.2 }"##),
        request(r##", "background": { "color": "#ff0000", "padding": 0.6 }"##),
        request(""),
    ]
    .iter()
    .map(|request| layout_text_key(request, &[], &[]))
    .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(keys.len(), 4);
    let plain = request("");
    let monochrome = TextRasterRequest {
        emoji_policy: alpha2_media::EmojiPolicy::Monochrome,
        ..plain.clone()
    };
    assert_ne!(
        layout_text_key(&plain, &[], &[]),
        layout_text_key(&monochrome, &[], &[])
    );
}
