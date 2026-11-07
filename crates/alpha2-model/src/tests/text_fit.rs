use crate::{
    BlockEllipsis, FontSizeFit, Generator, LineClamp, MAX_BLOCK_ELLIPSIS_BYTES, TextStyle,
    TextStylePatch,
};

fn text_generator(style: TextStyle) -> Generator {
    Generator::Text {
        text: "Hello".into(),
        style: Box::new(style),
    }
}

fn fit(min: f32, max: Option<f32>) -> FontSizeFit {
    FontSizeFit { min, max }
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_text_style_holds_its_box_fit_and_clamp_to_their_ranges() {
    let valid = |style: TextStyle| text_generator(style).is_valid();
    let base = TextStyle::default();
    for (style, expected) in [
        (
            TextStyle {
                max_height: Some(0.5),
                ..base.clone()
            },
            true,
        ),
        (
            TextStyle {
                max_height: Some(1.0),
                ..base.clone()
            },
            true,
        ),
        (
            TextStyle {
                max_height: Some(0.0),
                ..base.clone()
            },
            false,
        ),
        (
            TextStyle {
                max_height: Some(1.5),
                ..base.clone()
            },
            false,
        ),
        (
            TextStyle {
                max_height: Some(f32::NAN),
                ..base.clone()
            },
            false,
        ),
        (
            TextStyle {
                font_size_fit: Some(fit(0.01, None)),
                ..base.clone()
            },
            true,
        ),
        (
            TextStyle {
                font_size_fit: Some(fit(0.01, Some(1.0))),
                ..base.clone()
            },
            true,
        ),
        // A minimum above `font_size` is valid: it wins over the default maximum.
        (
            TextStyle {
                font_size_fit: Some(fit(0.2, None)),
                ..base.clone()
            },
            true,
        ),
        (
            TextStyle {
                font_size_fit: Some(fit(0.0, None)),
                ..base.clone()
            },
            false,
        ),
        (
            TextStyle {
                font_size_fit: Some(fit(-0.1, None)),
                ..base.clone()
            },
            false,
        ),
        (
            TextStyle {
                font_size_fit: Some(fit(0.1, Some(0.05))),
                ..base.clone()
            },
            false,
        ),
        (
            TextStyle {
                font_size_fit: Some(fit(0.01, Some(1.5))),
                ..base.clone()
            },
            false,
        ),
        (
            TextStyle {
                font_size_fit: Some(fit(f32::INFINITY, None)),
                ..base.clone()
            },
            false,
        ),
        (
            TextStyle {
                line_clamp: Some(LineClamp::default()),
                ..base.clone()
            },
            true,
        ),
        (
            TextStyle {
                line_clamp: Some(LineClamp {
                    max_lines: Some(1),
                    ..LineClamp::default()
                }),
                ..base.clone()
            },
            true,
        ),
        (
            TextStyle {
                line_clamp: Some(LineClamp {
                    max_lines: Some(0),
                    ..LineClamp::default()
                }),
                ..base.clone()
            },
            false,
        ),
    ] {
        assert_eq!(valid(style.clone()), expected, "{style:?}");
    }
}

#[test]
fn a_block_ellipsis_string_is_short_text_on_one_line() {
    let valid = |text: &str| {
        text_generator(TextStyle {
            line_clamp: Some(LineClamp {
                max_lines: None,
                block_ellipsis: BlockEllipsis::String(text.into()),
            }),
            ..TextStyle::default()
        })
        .is_valid()
    };
    assert!(valid(" (more)"));
    assert!(valid("\u{2192}"));
    assert!(valid(&"x".repeat(MAX_BLOCK_ELLIPSIS_BYTES)));
    for refused in [
        String::new(),
        "x".repeat(MAX_BLOCK_ELLIPSIS_BYTES + 1),
        "a\nb".into(),
        "a\rb".into(),
        "tab\t".into(),
        "a\u{2028}b".into(),
        "a\u{2029}b".into(),
        "a\u{85}b".into(),
    ] {
        assert!(!valid(&refused), "{refused:?}");
    }
}

#[test]
fn a_caption_fit_is_held_to_the_caption_limit() {
    let caption = |fit: Option<FontSizeFit>| crate::Captions {
        style: TextStyle {
            font_size_fit: fit,
            ..TextStyle::default()
        },
        styles: std::collections::BTreeMap::new(),
        cues: Vec::new(),
    };
    assert!(
        caption(Some(fit(0.02, Some(crate::MAX_CAPTION_FONT_SIZE))))
            .validate()
            .is_ok()
    );
    assert!(caption(Some(fit(0.02, Some(0.3)))).validate().is_err());
    assert!(
        text_generator(TextStyle {
            font_size_fit: Some(fit(0.02, Some(0.3))),
            ..TextStyle::default()
        })
        .is_valid()
    );
}

#[test]
fn line_clamp_reads_and_writes_as_css_spells_it() {
    let read = |json: &str| serde_json::from_str::<LineClamp>(json);
    assert_eq!(
        read("{}").expect("reads"),
        LineClamp {
            max_lines: None,
            block_ellipsis: BlockEllipsis::Ellipsis,
        }
    );
    assert_eq!(
        read(r#"{"max_lines":2,"block_ellipsis":"no_ellipsis"}"#).expect("reads"),
        LineClamp {
            max_lines: Some(2),
            block_ellipsis: BlockEllipsis::NoEllipsis,
        }
    );
    let custom = read(r#"{"block_ellipsis":{"string":" ..."}}"#).expect("reads");
    assert_eq!(custom.block_ellipsis.text(), Some(" ..."));
    assert_eq!(BlockEllipsis::Ellipsis.text(), Some("\u{2026}"));
    assert_eq!(BlockEllipsis::NoEllipsis.text(), None);
    assert_eq!(
        serde_json::to_value(&custom).expect("writes"),
        serde_json::json!({ "block_ellipsis": { "string": " ..." } })
    );
    assert!(read(r#"{"lines":2}"#).is_err());
    assert!(read(r#"{"block_ellipsis":"auto"}"#).is_err());
    assert!(serde_json::from_str::<FontSizeFit>(r#"{"min":0.1,"step":1}"#).is_err());
    let style: TextStyle = serde_json::from_str(
        r#"{"max_height":0.3,"font_size_fit":{"min":0.02},"line_clamp":{"max_lines":3}}"#,
    )
    .expect("reads");
    let written = serde_json::to_string(&style).expect("writes");
    assert_eq!(
        serde_json::from_str::<TextStyle>(&written).expect("reads"),
        style
    );
    let plain = serde_json::to_value(TextStyle::default()).expect("writes");
    for field in ["max_height", "font_size_fit", "line_clamp"] {
        assert!(plain.get(field).is_none(), "{field} is left out when unset");
    }
}

#[test]
fn a_caption_patch_sets_or_clears_the_box_fit_and_clamp() {
    let base = TextStyle {
        max_height: Some(0.2),
        font_size_fit: Some(fit(0.02, None)),
        line_clamp: Some(LineClamp::default()),
        ..TextStyle::default()
    };
    let read = |json: &str| serde_json::from_str::<TextStylePatch>(json).expect("patch");
    assert_eq!(read("{}").apply(&base), base);
    let cleared =
        read(r#"{"max_height":null,"font_size_fit":null,"line_clamp":null}"#).apply(&base);
    assert_eq!(
        (
            cleared.max_height,
            cleared.font_size_fit,
            cleared.line_clamp
        ),
        (None, None, None)
    );
    let set = read(
        r#"{"max_height":0.5,"font_size_fit":{"min":0.01,"max":0.1},"line_clamp":{"max_lines":1}}"#,
    )
    .apply(&TextStyle::default());
    assert_eq!(set.max_height, Some(0.5));
    assert_eq!(set.font_size_fit, Some(fit(0.01, Some(0.1))));
    assert_eq!(set.line_clamp.and_then(|clamp| clamp.max_lines), Some(1));
}
