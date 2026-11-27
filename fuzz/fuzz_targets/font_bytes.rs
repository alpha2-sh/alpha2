#![no_main]

use std::sync::Arc;

use alpha2_media::{EmojiPolicy, TextAlign};
use alpha2_text::{TextFill, TextFont, TextRasterRequest, TextRasterizer, check_font, face_count};
use libfuzzer_sys::fuzz_target;

// Any bytes register as a font or are refused, as `check_font` predicts, never panicking. A
// registered font draws a line of text within the time and memory limits of `scripts/fuzz.sh`.
fuzz_target!(|data: &[u8]| {
    let bytes: Arc<[u8]> = data.into();
    let faces = face_count(&bytes).min(4);
    for face_index in 0..faces {
        let checked = check_font(&bytes, face_index);
        let mut rasterizer = TextRasterizer::empty();
        let added = rasterizer.add_font(TextFont {
            family: "Fuzz".into(),
            bytes: Arc::clone(&bytes),
            face_index,
        });
        assert_eq!(
            checked.as_ref().map_err(ToString::to_string),
            added.as_ref().map_err(ToString::to_string)
        );
        if added.is_err() {
            continue;
        }
        let _ = rasterizer.rasterize(&TextRasterRequest {
            text: "Ag Wy 1 \u{e000}\u{1f600} \u{ac00}".into(),
            font_size: 48.0,
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
            animator_grouping: alpha2_text::ClusterGrouping::Character,
            animator_pivot: [0.5, 0.5],
        });
    }
});
