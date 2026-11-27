use serde_json::json;

use crate::{TextBackground, TextInsets};

fn background(value: serde_json::Value) -> Result<TextBackground, serde_json::Error> {
    serde_json::from_value(value)
}

#[test]
fn text_box_padding_reads_a_number_the_shorthand_or_the_sides() {
    let sides = |padding: serde_json::Value| {
        background(json!({ "color": "#000000", "padding": padding }))
            .expect("a box")
            .padding
            .sides()
    };
    assert_eq!(sides(json!(0.2)), [0.2; 4]);
    assert_eq!(sides(json!("0.1 0.3")), [0.1, 0.3, 0.1, 0.3]);
    assert_eq!(sides(json!("0.1 0.3 0.2")), [0.1, 0.3, 0.2, 0.3]);
    assert_eq!(sides(json!(" 0.1  0.2 0.3 0.4 ")), [0.1, 0.2, 0.3, 0.4]);
    assert_eq!(sides(json!({ "left": 0.5 })), [0.0, 0.0, 0.0, 0.5]);
    for refused in [
        json!("4px"),
        json!("0.1 0.2 0.3 0.4 0.5"),
        json!(""),
        json!([0.1]),
    ] {
        assert!(
            background(json!({ "color": "#000000", "padding": refused })).is_err(),
            "{refused}"
        );
    }
}

#[test]
fn a_text_box_writes_its_padding_as_one_number_when_every_side_is_the_same() {
    let uniform = background(json!({ "color": "#000000", "padding": "0.25" })).expect("a box");
    assert_eq!(
        serde_json::to_value(&uniform).expect("writes")["padding"],
        json!(0.25)
    );
    let sides = TextInsets {
        top: 0.1,
        ..TextInsets::all(0.2)
    };
    let written = serde_json::to_value(TextBackground {
        padding: sides,
        ..uniform
    })
    .expect("writes");
    assert_eq!(
        written["padding"],
        json!({ "top": 0.1_f32, "right": 0.2_f32, "bottom": 0.2_f32, "left": 0.2_f32 })
    );
    assert_eq!(
        background(json!({ "color": "#000000", "padding": 0.2 })).expect("a box"),
        TextBackground::padded(crate::Rgba::new(0.0, 0.0, 0.0, 1.0), 0.2)
    );
}
