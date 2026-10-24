#![no_main]

use alpha2_engine::measure_layout;
use alpha2_model::{Generator, LayoutNode};
use alpha2_render::RenderSize;
use alpha2_text::{TextFont, TextRasterizer};
use alpha2_time::MediaTime;
use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;

/// The fixture font, parsed once.
fn rasterizer() -> &'static TextRasterizer {
    static RASTERIZER: OnceLock<TextRasterizer> = OnceLock::new();
    RASTERIZER.get_or_init(|| {
        TextRasterizer::new(vec![TextFont {
            family: "Alpha2 Fixture".into(),
            bytes: include_bytes!("../../tests/fonts/Alpha2Fixture.ttf")
                .to_vec()
                .into(),
            face_index: 0,
        }])
        .expect("the fixture font")
    })
}

// A layout tree from JSON, valid or not, lays out at any size and time without a panic, its
// pixels those of a canvas the picture's size; a valid one gives finite rects for every node.
fuzz_target!(|data: &[u8]| {
    let Some((head, json)) = data.split_at_checked(6) else {
        return;
    };
    let width = u32::from(u16::from_le_bytes([head[0], head[1]]) % 4096) + 1;
    let height = u32::from(u16::from_le_bytes([head[2], head[3]]) % 4096) + 1;
    let time = MediaTime(i64::from(i16::from_le_bytes([head[4], head[5]])) * 100);
    let Ok(root) = serde_json::from_slice::<LayoutNode>(json) else {
        return;
    };
    let valid = Generator::Layout {
        root: Box::new(root.clone()),
    }
    .validate_on(width, height)
    .is_ok();
    let size = RenderSize::new(width, height).expect("a size within the limits");
    let Ok(bounds) = measure_layout(rasterizer(), &root, time, size, (width, height)) else {
        return;
    };
    if valid {
        let mut count = 0;
        root.walk(|_, _| count += 1);
        assert_eq!(bounds.nodes.len(), count);
        for node in &bounds.nodes {
            for rect in [node.border, node.content] {
                for edge in [rect.left, rect.top, rect.right, rect.bottom] {
                    assert!(edge.is_finite(), "{node:?}");
                }
            }
        }
    }
});
