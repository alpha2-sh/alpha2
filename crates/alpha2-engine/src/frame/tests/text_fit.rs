use super::*;
use crate::{EngineProject, FramesWithText, TextTextures};

const W: u32 = 320;
const H: u32 = 180;

/// The card's font size animates between these times, and the karaoke runs move the ellipsis.
const TIMES: [i64; 3] = [0, 36_000, 108_000];

fn project() -> EngineProject {
    let json = include_str!("../../../../../tests/cases/text-fit-project.json");
    EngineProject::new(alpha2_model::Project::from_json(json).expect("a valid project"))
        .expect("indexed")
}

fn texts() -> TextTextures {
    let mut texts = TextTextures::new();
    texts
        .add_font(
            "Alpha2 Fixture",
            std::fs::read("../../tests/fonts/Alpha2Fixture.ttf").expect("fixture font"),
            0,
        )
        .expect("font");
    texts
}

fn side_by_side(images: &[Vec<u8>]) -> Vec<u8> {
    let row = (W * 4) as usize;
    (0..H as usize)
        .flat_map(|y| {
            images
                .iter()
                .flat_map(move |image| image[y * row..(y + 1) * row].iter().copied())
        })
        .collect()
}

fn gpu_frames() -> Vec<u8> {
    let mut renderer = Renderer::new(gpu());
    renderer.register_builtins().expect("the built-ins compile");
    let (project, mut texts) = (project(), texts());
    let size = RenderSize::new(W, H).expect("size");
    let frames = TIMES.map(|ticks| {
        let frame = project.evaluate(MediaTime(ticks)).expect("evaluates");
        texts.prepare(&renderer, &frame, size).expect("prepared");
        let scope = gpu()
            .device()
            .push_error_scope(wgpu::ErrorFilter::Validation);
        let output = renderer
            .render_frame(size, &frame, &FramesWithText::new(&MissingFrames, &texts))
            .expect("frame renders");
        let error = pollster::block_on(scope.pop());
        assert!(error.is_none(), "{error:?}");
        read_rgba8(&renderer, &output.texture.expect("a complete frame"))
    });
    side_by_side(&frames)
}

fn cpu_frames() -> Vec<u8> {
    let renderer = alpha2_render::CpuRenderer::new(3);
    let (project, mut texts) = (project(), texts());
    let size = RenderSize::new(W, H).expect("size");
    let frames = TIMES.map(|ticks| {
        let frame = project.evaluate(MediaTime(ticks)).expect("evaluates");
        texts.prepare(&renderer, &frame, size).expect("prepared");
        renderer
            .render_frame(size, &frame, &FramesWithText::new(&MissingFrames, &texts))
            .expect("frame renders")
            .texture
            .expect("a complete frame")
            .read_rgba8()
    });
    side_by_side(&frames)
}

#[test]
fn fitted_and_clamped_texts_match_their_golden() {
    let _lock = gpu_test_lock();
    assert_golden_or_rgba8("text_fit", "text_fit_rgba8", (W * 3, H), &gpu_frames(), 2);
}

#[test]
fn fitted_and_clamped_texts_match_their_golden_on_the_cpu() {
    assert_cpu_golden("text_fit", (W * 3, H), &cpu_frames(), 2);
}
