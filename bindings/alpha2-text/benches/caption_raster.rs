//! Standalone benchmark for caption rasterization.
//!
//! Rasterizes a new caption as the engine asks for one (font 5% of the
//! canvas height, wrapped at 90% of its width, centred) over a matrix of
//! canvas size and outline width, the outline a share of the font size up to
//! the 64 px the engine draws. Prints one machine-readable line per case:
//! the best of a few runs. Times are recorded, never gated.

use std::{hint::black_box, time::Instant};

use alpha2_media::{EmojiPolicy, TextAlign};
use alpha2_text::{TextFont, TextOutline, TextRasterRequest, TextRasterizer};

const RUNS: usize = 3;

fn main() {
    let font = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fonts/Alpha2Fixture.ttf"
    ))
    .expect("checked-in fixture font");
    let mut rasterizer = TextRasterizer::new(vec![TextFont {
        family: "Alpha2 Fixture".into(),
        bytes: font.into(),
        face_index: 0,
    }])
    .expect("valid fixture");
    let sentence = "The quick brown fox jumps over the lazy dog,\nand keeps on running.";
    for (width, height) in [(960, 540), (1920, 1080), (1080, 1920), (3840, 2160)] {
        for outline in [0.0, 0.08, 0.25, 0.5] {
            run(
                &mut rasterizer,
                sentence,
                "sentence",
                width,
                height,
                0.05,
                outline,
            );
        }
    }
    // Little ink on a wide texture, and the thickest outline the engine draws.
    run(&mut rasterizer, "Hi", "hi", 3840, 2160, 0.05, 0.25);
    run(&mut rasterizer, "Hi", "hi", 3840, 2160, 0.1, 0.3);
    run(&mut rasterizer, sentence, "sentence", 3840, 2160, 0.1, 0.3);
}

#[allow(clippy::cast_precision_loss)]
fn run(
    rasterizer: &mut TextRasterizer,
    text: &str,
    name: &str,
    width: u32,
    height: u32,
    font: f32,
    outline: f32,
) {
    let font_size = font * height as f32;
    let request = TextRasterRequest {
        text: text.into(),
        font_size,
        max_width: 0.9 * width as f32,
        line_height: 1.2,
        align: TextAlign::Center,
        color: [255, 255, 255, 255],
        emoji_policy: EmojiPolicy::Color,
        content_version: 1,
        outline: (outline > 0.0).then(|| TextOutline {
            color: [0, 0, 0, 255],
            width: (outline * font_size).min(64.0),
        }),
        background: None,
        font_family: None,
        letter_spacing: 0.0,
        font_weight: 400,
    };
    let mut best = f64::INFINITY;
    for _ in 0..RUNS {
        rasterizer.clear();
        let started = Instant::now();
        black_box(rasterizer.rasterize(&request).expect("rasterizes"));
        best = best.min(started.elapsed().as_secs_f64() * 1_000.0);
    }
    println!(
        "caption_raster text={name} canvas={width}x{height} font={font} outline={outline} best_ms={best:.2}"
    );
}
