use super::*;
use alpha2_model::{TextStyle, parse_srt, parse_webvtt};
use alpha2_text::TextFont;

#[test]
fn imported_cues_whose_last_line_is_blank_after_markup_are_drawn() {
    // Lines that end a cue only once their markup or references are read: blank or zero
    // width, not a block's end.
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
    let size = RenderSize::new(1920, 1080).expect("size");
    for style in [
        TextStyle::default(),
        TextStyle {
            outline: Some(alpha2_model::TextOutline {
                color: Rgba::new(0.0, 0.0, 0.0, 1.0),
                width: 0.08,
            }),
            ..TextStyle::default()
        },
    ] {
        for cue in &cues {
            let key = text_key(&cue.text, &style, size);
            let raster = rasterizer
                .rasterize(&text_request(&cue.text, &style, size, key))
                .unwrap_or_else(|error| panic!("{:?}: {error}", cue.text));
            assert_eq!(raster.descriptor.line_count, 2, "{:?}", cue.text);
        }
    }
}
