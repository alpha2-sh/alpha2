//! The CSS value forms: what each reads, what it writes back, and what it
//! refuses, naming the token.

use super::*;
use crate::{Paint, Rgba};

fn read<T: for<'de> Deserialize<'de>>(json: &str) -> Result<T, String> {
    serde_json::from_str(json).map_err(|error| error.to_string())
}

/// `value` written and read back, and its text.
fn round_trip<T>(value: &T) -> String
where
    T: Serialize + for<'de> Deserialize<'de> + PartialEq + fmt::Debug,
{
    let text = serde_json::to_string(value).expect("writes");
    assert_eq!(&read::<T>(&text).expect("reads back"), value, "{text}");
    text
}

#[test]
fn a_length_is_a_number_px_or_percent_and_writes_back_as_written() {
    assert_eq!(
        read::<LayoutLength>("0.05"),
        Ok(LayoutLength::Fraction(0.05))
    );
    assert_eq!(read::<LayoutLength>("2"), Ok(LayoutLength::Fraction(2.0)));
    assert_eq!(
        read::<LayoutLength>(r#""12px""#),
        Ok(LayoutLength::Px(12.0))
    );
    assert_eq!(
        read::<LayoutLength>(r#""-.5px""#),
        Ok(LayoutLength::Px(-0.5))
    );
    assert_eq!(
        read::<LayoutLength>(r#""50%""#),
        Ok(LayoutLength::Percent(50.0))
    );
    assert_eq!(
        read::<LayoutLength>(r#""0.25""#),
        Ok(LayoutLength::Fraction(0.25))
    );
    for length in [
        LayoutLength::Fraction(0.05),
        LayoutLength::Px(12.5),
        LayoutLength::Percent(33.3),
    ] {
        round_trip(&length);
    }
    assert_eq!(round_trip(&LayoutLength::Px(12.0)), r#""12px""#);
    assert_eq!(round_trip(&LayoutLength::Percent(50.0)), r#""50%""#);
    // A px string holds the f32 the same number does as JSON.
    assert_eq!(
        read::<LayoutLength>(r#""0.1px""#),
        Ok(LayoutLength::Px(read::<f32>("0.1").expect("number")))
    );
}

#[test]
fn a_length_that_does_not_parse_names_its_token_and_the_spelling_meant() {
    let error = read::<LayoutLength>(r#""12 px""#).expect_err("a space");
    assert!(
        error.starts_with(r#"invalid length "12 px" (write 12px): write a number"#),
        "{error}"
    );
    let error = read::<LayoutLength>(r#""2em""#).expect_err("em");
    assert!(error.contains("em and rem are not supported"), "{error}");
    for text in [r#""1e3px""#, r#""px""#, r#""12pt""#, r#""""#, r#""."#] {
        assert!(read::<LayoutLength>(text).is_err(), "{text}");
    }
    let error = read::<LayoutSize>(r#""50%""#).expect_err("a size takes no percent");
    assert!(
        error.starts_with(r#"percent is not allowed here: "50%""#),
        "{error}"
    );
}

#[test]
fn insets_take_one_length_the_shorthand_or_the_sides() {
    let px = LayoutSize::Px;
    let sides = |top, right, bottom, left| Insets {
        top,
        right,
        bottom,
        left,
    };
    assert_eq!(
        read::<Insets>("0.02"),
        Ok(Insets::from_sides([LayoutSize::Fraction(0.02); 4]))
    );
    assert_eq!(
        read::<Insets>(r#""8px 16px""#),
        Ok(sides(px(8.0), px(16.0), px(8.0), px(16.0)))
    );
    assert_eq!(
        read::<Insets>(r#""1px 2px 3px""#),
        Ok(sides(px(1.0), px(2.0), px(3.0), px(2.0)))
    );
    assert_eq!(
        read::<Insets>(r#"" 1px  2px 3px 0.5 ""#),
        Ok(sides(px(1.0), px(2.0), px(3.0), LayoutSize::Fraction(0.5)))
    );
    assert_eq!(
        read::<Insets>(r#"{"left":"4px"}"#),
        Ok(sides(
            LayoutSize::default(),
            LayoutSize::default(),
            LayoutSize::default(),
            px(4.0)
        ))
    );
    let written = round_trip(&sides(px(1.0), LayoutSize::Fraction(0.5), px(3.0), px(4.0)));
    assert_eq!(
        written,
        r#"{"top":"1px","right":0.5,"bottom":"3px","left":"4px"}"#
    );
    let error = read::<Insets>(r#""1px 2px 3px 4px 5px""#).expect_err("five");
    assert!(
        error.starts_with("expected 1 to 4 lengths, got 5"),
        "{error}"
    );
    let error = read::<Insets>(r#""10%""#).expect_err("a percent");
    assert!(error.starts_with("percent is not allowed here"), "{error}");
    assert!(read::<Insets>(r#"{"middle":1}"#).is_err());
}

#[test]
fn a_track_list_reads_css_and_writes_it_back() {
    for text in [
        "1fr 2fr 120px",
        "repeat(3, minmax(100px, 1fr))",
        "repeat(auto-fill, 160px)",
        "repeat(auto-fit, minmax(160px, 1fr)) 20%",
        "auto min-content max-content",
        "fit-content(200px) 0.1 50%",
        "repeat(2, 1fr 2fr) minmax(auto, max-content)",
    ] {
        let tracks: TrackList = read(&format!("\"{text}\"")).expect(text);
        assert_eq!(tracks.to_string(), text);
        round_trip(&tracks);
    }
    let tracks: TrackList = read(r#""  1fr   repeat( 2 ,  10px )""#).expect("spaces");
    assert_eq!(tracks.to_string(), "1fr repeat(2, 10px)");
    assert_eq!(tracks.track_count(), 3);
    assert_eq!(
        tracks.0[0],
        TrackComponent::Single(TrackSize::Breadth(TrackBreadth::Fr(1.0)))
    );
}

#[test]
fn a_track_list_that_does_not_parse_names_the_token() {
    let refused = |text: &str| read::<TrackList>(&format!("\"{text}\"")).expect_err(text);
    let starts = |text: &str, message: &str| {
        let error = refused(text);
        assert!(error.starts_with(message), "{text}: {error}");
    };
    starts("1fr 1 fr", r#"unknown track "1 fr" (write 1fr)"#);
    starts(
        "min_content",
        r#"unknown track "min_content" (write min-content)"#,
    );
    starts(
        "repeat(auto_fill, 10px)",
        r#"unknown repeat count "auto_fill" (write auto-fill)"#,
    );
    starts(
        "fit_content(10px)",
        r#"unknown track "fit_content" (write fit-content)"#,
    );
    starts(
        "[start] 1fr",
        r#"named grid lines are not supported: "[start]""#,
    );
    starts("minmax(1fr, 2fr)", r#"a minimum size cannot be fr: "1fr""#);
    starts("fit-content(auto)", "fit-content takes a length");
    starts("repeat(2, repeat(2, 1fr))", "repeat() cannot hold repeat()");
    starts("", "expected at least one track");
    starts("minmax(1px 2px)", r#"expected ",""#);
    starts("1fr)", "expected a track");
    starts("bogus", r#"unknown track "bogus""#);
}

#[test]
fn a_grid_placement_reads_lines_and_spans() {
    let line = GridEdge::Line;
    let placement = |start, end| GridPlacement { start, end };
    assert_eq!(
        read::<GridPlacement>("2"),
        Ok(placement(line(2), GridEdge::Auto))
    );
    assert_eq!(
        read::<GridPlacement>("-1"),
        Ok(placement(line(-1), GridEdge::Auto))
    );
    assert_eq!(
        read::<GridPlacement>(r#""1 / 3""#),
        Ok(placement(line(1), line(3)))
    );
    assert_eq!(
        read::<GridPlacement>(r#""span 2""#),
        Ok(placement(GridEdge::Span(2), GridEdge::Auto))
    );
    assert_eq!(
        read::<GridPlacement>(r#""2/span 3""#),
        Ok(placement(line(2), GridEdge::Span(3)))
    );
    assert_eq!(
        read::<GridPlacement>(r#""auto""#),
        Ok(GridPlacement::default())
    );
    assert_eq!(round_trip(&placement(line(2), GridEdge::Auto)), "2");
    assert_eq!(
        round_trip(&placement(GridEdge::Span(2), line(-1))),
        r#""span 2 / -1""#
    );
    let error = read::<GridPlacement>(r#""header""#).expect_err("a name");
    assert!(
        error.starts_with("named grid lines are not supported"),
        "{error}"
    );
    let error = read::<GridPlacement>(r#""1 / 2 / 3""#).expect_err("three");
    assert!(
        error.starts_with(r#"invalid grid line "1 / 2 / 3""#),
        "{error}"
    );
    assert!(read::<GridPlacement>(r#""span -2""#).is_err());
    let error = read::<GridPlacement>("1.5").expect_err("a fraction");
    assert!(error.starts_with(r#"invalid grid line "1.5""#), "{error}");
    assert_eq!(read::<GridPlacement>("2.0"), read::<GridPlacement>("2"));
}

#[test]
fn an_offset_reads_one_or_two_lengths_or_the_object() {
    assert_eq!(
        read::<LayoutOffset>(r#""10px""#),
        Ok(LayoutOffset {
            x: LayoutSize::Px(10.0),
            y: LayoutSize::default()
        })
    );
    let offset = read::<LayoutOffset>(r#""10px -0.1""#).expect("two");
    assert_eq!(offset.y, LayoutSize::Fraction(-0.1));
    assert_eq!(round_trip(&offset), r#"{"x":"10px","y":-0.1}"#);
    assert!(read::<LayoutOffset>(r#"{"x":1}"#).is_err());
    assert!(read::<LayoutOffset>(r#""1px 2px 3px""#).is_err());
}

#[test]
fn a_color_reads_css_hex_and_writes_the_object() {
    assert_eq!(
        read::<Rgba>(r##""#fff""##),
        Ok(Rgba::new(1.0, 1.0, 1.0, 1.0))
    );
    assert_eq!(
        read::<Rgba>(r##""#FF000080""##),
        Ok(Rgba::new(1.0, 0.0, 0.0, 128.0 / 255.0))
    );
    assert_eq!(
        read::<Rgba>(r##""#1e2230""##),
        Ok(Rgba::new(30.0 / 255.0, 34.0 / 255.0, 48.0 / 255.0, 1.0))
    );
    assert_eq!(
        read::<Rgba>(r##""#0f08""##),
        Ok(Rgba::new(0.0, 1.0, 0.0, 136.0 / 255.0))
    );
    let written = serde_json::to_value(Rgba::from_hex("#336699").expect("hex")).expect("writes");
    assert!(written.is_object(), "{written}");
    for text in [r##""#ff""##, r##""#ggg""##, r#""red""#, r##""#fffff""##] {
        let error = read::<Rgba>(text).expect_err(text);
        assert!(error.starts_with("invalid color"), "{text}: {error}");
    }
    assert!(read::<Rgba>(r#"{"red":1,"green":1,"blue":1}"#).is_err());
}

#[test]
fn a_color_paints_solid_and_a_paint_writes_back_tagged() {
    let solid = Paint::Solid {
        color: Rgba::new(1.0, 0.0, 0.0, 1.0),
    };
    assert_eq!(read::<Paint>(r##""#f00""##), Ok(solid.clone()));
    assert_eq!(
        read::<Paint>(r#"{"red":1,"green":0,"blue":0,"alpha":1}"#),
        Ok(solid.clone())
    );
    assert_eq!(
        read::<Paint>(r##"{"type":"solid","color":"#f00"}"##),
        Ok(solid.clone())
    );
    let gradient = read::<Paint>(
        r##"{"type":"linear_gradient","start":{"x":0,"y":0},"end":{"x":1,"y":0},
            "stops":[{"offset":0,"color":"#000"},{"offset":1,"color":"#fff"}]}"##,
    )
    .expect("hex stops");
    round_trip(&gradient);
    assert_eq!(
        serde_json::to_value(&solid).expect("writes")["type"],
        "solid"
    );
    let error = read::<Paint>(r##"{"type":"solid","colour":"#f00"}"##).expect_err("a typo");
    assert!(error.contains("unknown field `colour`"), "{error}");
    let error = read::<Paint>(r#"{"red":1}"#).expect_err("a part of a color");
    assert!(error.contains("missing field `green`"), "{error}");
}
