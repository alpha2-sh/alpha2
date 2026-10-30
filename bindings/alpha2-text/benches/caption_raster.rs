//! Standalone benchmark for caption rasterization.
//!
//! Rasterizes a new caption as the engine asks for one (font 5% of the
//! canvas height, wrapped at 90% of its width, centred) over a matrix of
//! canvas size and outline width, the outline a share of the font size up to
//! the 64 px the engine draws, and an animated caption: every cluster of a
//! two-line 1080p caption scaled and turned, with two strokes. Prints one
//! machine-readable line per case: the best of a few runs. Times are
//! recorded, never gated.

use std::{hint::black_box, time::Instant};

use alpha2_media::{EmojiPolicy, TextAlign};
use alpha2_text::{
    ClusterState, TextFill, TextFont, TextRasterRequest, TextRasterizer, TextStroke,
};

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
    animated(&mut rasterizer, &[4.0, 6.0]);
    animated(&mut rasterizer, &[]);
}

/// A two-line 1080p caption of about 48 clusters at 54 px with strokes of
/// `strokes` px (4 and 6, or none), every cluster scaled 1.1 and turned 3
/// degrees: what an animator draws each frame.
fn animated(rasterizer: &mut TextRasterizer, strokes: &[f32]) {
    let request = TextRasterRequest {
        text: "Every caption moves letter\nby letter, frame by frame".into(),
        font_size: 54.0,
        max_width: 1728.0,
        line_height: 1.2,
        align: TextAlign::Center,
        fill: TextFill::Solid([255, 255, 255, 255]),
        emoji_policy: EmojiPolicy::Color,
        content_version: 1,
        strokes: strokes
            .iter()
            .map(|&width| TextStroke {
                color: [0, 0, 0, 255],
                width,
            })
            .collect(),
        background: None,
        split_background: false,
        font_family: None,
        letter_spacing: 0.0,
        font_weight: 400,
    };
    let clusters = rasterizer
        .layout(&request)
        .expect("lays out")
        .clusters
        .len();
    let states = vec![
        ClusterState {
            scale: [1.1, 1.1],
            rotation: 3.0_f32.to_radians(),
            ..ClusterState::IDENTITY
        };
        clusters
    ];
    let mut best = f64::INFINITY;
    let mut best_layout = f64::INFINITY;
    for _ in 0..RUNS * 3 {
        rasterizer.clear();
        let started = Instant::now();
        black_box(rasterizer.layout(&request).expect("lays out"));
        best_layout = best_layout.min(started.elapsed().as_secs_f64() * 1_000.0);
        let started = Instant::now();
        black_box(
            rasterizer
                .rasterize_animated(&request, &states)
                .expect("rasterizes"),
        );
        best = best.min(started.elapsed().as_secs_f64() * 1_000.0);
    }
    // Frame after frame, one cluster turning while the others hold: what a karaoke or a
    // per-character pop-in draws once its earlier characters have settled.
    let mut warm = f64::INFINITY;
    let mut frame_states = states.clone();
    for frame in 0..RUNS * 3 {
        let changing = frame % clusters;
        frame_states[changing].rotation += 0.01;
        let started = Instant::now();
        black_box(
            rasterizer
                .rasterize_animated(
                    &TextRasterRequest {
                        content_version: 100 + frame as u64,
                        ..request.clone()
                    },
                    &frame_states,
                )
                .expect("rasterizes"),
        );
        warm = warm.min(started.elapsed().as_secs_f64() * 1_000.0);
    }
    println!(
        "caption_raster text=animated clusters={clusters} canvas=1920x1080 font=54px strokes={strokes:?} cold_ms={best:.2} one_cluster_changing_ms={warm:.2} layout_ms={best_layout:.2}"
    );
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
        fill: TextFill::Solid([255, 255, 255, 255]),
        emoji_policy: EmojiPolicy::Color,
        content_version: 1,
        strokes: (outline > 0.0)
            .then(|| TextStroke {
                color: [0, 0, 0, 255],
                width: (outline * font_size).min(64.0),
            })
            .into_iter()
            .collect(),
        background: None,
        split_background: false,
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
