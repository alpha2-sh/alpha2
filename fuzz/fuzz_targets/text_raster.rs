#![no_main]

use std::sync::{Mutex, OnceLock};

use alpha2_media::{EmojiPolicy, TextAlign};
use alpha2_text::{
    ClusterGrouping, ClusterState, MAX_CLUSTER_SCALE, RevealEdge, TextFill, TextFont,
    TextRasterRequest, TextRasterizer, TextStroke,
};
use libfuzzer_sys::fuzz_target;

fn rasterizer() -> &'static Mutex<TextRasterizer> {
    static RASTERIZER: OnceLock<Mutex<TextRasterizer>> = OnceLock::new();
    RASTERIZER.get_or_init(|| {
        let font = |family: &str, bytes: &'static [u8]| TextFont {
            family: family.into(),
            bytes: bytes.into(),
            face_index: 0,
        };
        Mutex::new(
            TextRasterizer::new(vec![
                font(
                    "Fixture",
                    include_bytes!("../../tests/fonts/Alpha2Fixture.ttf"),
                ),
                font(
                    "Hangul",
                    include_bytes!("../../tests/fonts/Alpha2FixtureHangul.ttf"),
                ),
                font(
                    "Colr",
                    include_bytes!("../../tests/fonts/Alpha2FixtureEmojiColr.ttf"),
                ),
                font(
                    "Color",
                    include_bytes!("../../tests/fonts/Alpha2FixtureColor.ttf"),
                ),
            ])
            .expect("fixture fonts"),
        )
    })
}

/// Reads parameters off the front of the input.
struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    fn byte(&mut self) -> u8 {
        let (&first, rest) = self.0.split_first().unwrap_or((&0, &[]));
        self.0 = rest;
        first
    }

    fn unit(&mut self) -> f32 {
        f32::from(self.byte()) / 255.0
    }
}

// Text and request values over the fixture fonts shape, measure and draw, plain and animated, or
// are refused with a typed error; nothing panics, and a raster is the size its descriptor says.
fuzz_target!(|data: &[u8]| {
    static VERSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let mut bytes = Bytes(data);
    let size = f32::from(u16::from(bytes.byte()) << 3 | u16::from(bytes.byte() & 7)) + 1.0;
    let max_width = f32::from(bytes.byte()) * 8.0;
    let line_height = 0.5 + bytes.unit() * 2.0;
    let spacing = (bytes.unit() - 0.25) * size;
    let align =
        [TextAlign::Start, TextAlign::Center, TextAlign::End][usize::from(bytes.byte() % 3)];
    let emoji = [
        EmojiPolicy::Color,
        EmojiPolicy::Monochrome,
        EmojiPolicy::Replace,
    ][usize::from(bytes.byte() % 3)];
    let strokes = (0..bytes.byte() % 3)
        .map(|_| TextStroke {
            color: [0, 0, 0, 255],
            width: 0.5 + bytes.unit() * 8.0,
        })
        .collect();
    let mut state = ClusterState {
        offset: [(bytes.unit() - 0.5) * size, (bytes.unit() - 0.5) * size],
        scale: [
            bytes.unit() * MAX_CLUSTER_SCALE,
            bytes.unit() * MAX_CLUSTER_SCALE,
        ],
        rotation: bytes.unit() * 7.0,
        opacity: bytes.unit(),
        tracking: (bytes.unit() - 0.5) * size,
        advance_scale: bytes.unit() * MAX_CLUSTER_SCALE,
        ..ClusterState::IDENTITY
    };
    let advance_headroom = 1.0 + bytes.unit() * (MAX_CLUSTER_SCALE - 1.0);
    let animator_grouping = [
        ClusterGrouping::Character,
        ClusterGrouping::Word,
        ClusterGrouping::Line,
        ClusterGrouping::All,
    ][usize::from(bytes.byte() % 4)];
    let animator_pivot = [bytes.unit(), bytes.unit()];
    let turn = bytes.unit() * std::f32::consts::TAU;
    state.reveals[0] = Some(RevealEdge {
        direction: [turn.cos(), turn.sin()],
        progress: bytes.unit(),
        feather: bytes.unit() * size,
    });
    let Ok(text) = std::str::from_utf8(bytes.0) else {
        return;
    };
    let request = TextRasterRequest {
        text: text.into(),
        font_size: size,
        max_width,
        line_height,
        align,
        fill: TextFill::Solid([255, 255, 255, 255]),
        emoji_policy: emoji,
        content_version: VERSION.fetch_add(2, std::sync::atomic::Ordering::Relaxed),
        strokes,
        split_background: false,
        background: None,
        font_family: None,
        letter_spacing: spacing,
        font_weight: 400,
        clamp: None,
        spans: Vec::new(),
        advance_headroom,
        animator_grouping,
        animator_pivot,
        highlight: None,
    };
    let mut rasterizer = rasterizer()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = rasterizer.shape(&request);
    let _ = rasterizer.measure(&request);
    let check = |raster: &alpha2_text::TextRasterResult| {
        let texture = &raster.descriptor.texture;
        assert_eq!(
            raster.pixels.rgba8.len() as u64,
            u64::from(texture.width) * u64::from(texture.height) * 4
        );
    };
    if let Ok(raster) = rasterizer.rasterize(&request) {
        check(&raster);
    }
    if let Ok(layout) = rasterizer.layout(&request) {
        let animated = TextRasterRequest {
            content_version: request.content_version + 1,
            ..request
        };
        if let Ok(raster) =
            rasterizer.rasterize_animated(&animated, &vec![state; layout.clusters.len()])
        {
            check(&raster);
        }
    }
});
