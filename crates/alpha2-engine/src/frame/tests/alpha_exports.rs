//! Exports that keep alpha or write 10 bits: the layers without the project
//! background, packed the same by the GPU and the CPU renderer.

use super::captions_and_text::text_project;
use super::text_look::{LOOK, caption_track, fixture_texts, look_renderer};
use super::*;
use crate::{ExportPlaneStep, ExportStep, Exporter, QualityProfile};
use alpha2_media::{BitDepth, Chroma, PixelLayout, PlaneLayout, VideoCodec};

/// A caption over a blue project background and nothing else: the frame is
/// transparent wherever the caption is not.
fn captioned() -> crate::EngineProject {
    let mut project = text_project("", "", &caption_track(LOOK, "Look"))
        .project()
        .clone();
    project.timeline.tracks.remove(0);
    project.settings.background = alpha2_model::Rgb::new(0.2, 0.4, 0.8);
    crate::EngineProject::new(project).expect("indexed")
}

fn ten_bit(chroma: Chroma, alpha: bool) -> PixelLayout {
    PixelLayout {
        chroma,
        bit_depth: BitDepth::Ten,
        alpha,
        planes: PlaneLayout::Planar,
    }
}

/// The frame at one second exported in `layout`, rendered on `gpu` or, when
/// it is `None`, on the CPU: each plane's 16-bit samples. `None` when `gpu`
/// renders through the 8-bit intermediate and `layout` has 10 bits, which it
/// refuses.
fn exported(gpu: Option<&Renderer>, layout: PixelLayout) -> Option<Vec<Vec<u16>>> {
    let snapshot = captioned();
    let size = RenderSize::new(160, 90).expect("size");
    let at = MediaTime::from_ticks(120_000);
    let range = alpha2_time::TimeRange::new(at, MediaTime::from_ticks(124_000)).expect("a frame");
    let mut exporter = Exporter::new(
        &snapshot,
        range,
        alpha2_time::FrameRate::new(30, 1).expect("rate"),
        QualityProfile::export(),
        VideoCodec::ProRes,
    )
    .expect("exporter")
    .with_pixels(layout);
    let mut texts = fixture_texts();
    let planes = layout.planes(size.width, size.height);
    let mut memory: Vec<Vec<u8>> = planes
        .iter()
        .map(|&(width, rows, bytes)| vec![0; (width * rows * bytes) as usize])
        .collect();
    let mut targets: Vec<alpha2_render::PlaneTarget<'_>> = memory
        .iter_mut()
        .zip(&planes)
        .map(|(bytes, &(width, _, sample))| {
            alpha2_render::PlaneTarget::new(bytes, (width * sample) as usize)
        })
        .collect();
    if let Some(renderer) = gpu {
        texts
            .prepare_at(renderer, &snapshot, at, size)
            .expect("text");
        let step = exporter.render_next(&snapshot, renderer, &CaptionFrames(&texts));
        // The 8-bit intermediate cannot hold 10-bit samples, and its readback says so.
        if rgba8_intermediate(renderer) && layout.bit_depth != BitDepth::Eight {
            assert!(
                matches!(
                    step,
                    Err(crate::EngineError::Render(
                        alpha2_render::RenderError::TextureReadback(
                            alpha2_render::ReadbackFailure::UnsupportedTarget
                        )
                    ))
                ),
                "{step:?}"
            );
            return None;
        }
        let ExportStep::Rendered { readback, .. } = step.expect("a step") else {
            panic!("the frame renders");
        };
        assert_eq!(readback.pixel_layout(), Some(layout));
        readback.wait_ready().expect("mapped");
        readback.finish_into_planes(&mut targets).expect("packs");
    } else {
        let renderer = alpha2_render::CpuRenderer::new(2);
        texts
            .prepare_at(&renderer, &snapshot, at, size)
            .expect("text");
        let step = exporter
            .render_next_into_planes(
                &snapshot,
                &renderer,
                &CaptionFrames(&texts),
                &mut targets,
                None,
            )
            .expect("a step");
        assert!(matches!(step, ExportPlaneStep::Rendered { .. }), "{step:?}");
    }
    drop(targets);
    Some(
        memory
            .iter()
            .map(|bytes| {
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_le_bytes(*pair))
                    .collect()
            })
            .collect(),
    )
}

#[test]
fn an_alpha_export_has_no_background() {
    let _lock = gpu_test_lock();
    let renderer = look_renderer();
    let Some(planes) = exported(Some(&renderer), ten_bit(Chroma::Yuv444, true)) else {
        return;
    };
    let (luma, alpha) = (&planes[0], &planes[3]);
    // The corner shows nothing: transparent, and black where nothing is.
    assert_eq!((alpha[0], luma[0]), (0, 64));
    // The caption's shadow and fill are opaque.
    assert_eq!(alpha.iter().max(), Some(&1023));
    assert!(alpha.iter().filter(|&&code| code == 0).count() > alpha.len() / 2);
}

#[test]
fn an_opaque_export_still_draws_the_background() {
    let _lock = gpu_test_lock();
    let renderer = look_renderer();
    let Some(planes) = exported(Some(&renderer), ten_bit(Chroma::Yuv422, false)) else {
        return;
    };
    // The blue background's luma: BT.709 of sRGB-encoded (0.2, 0.4, 0.8), limited range.
    let luma: f64 = 0.2126 * 0.2 + 0.7152 * 0.4 + 0.0722 * 0.8;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let expected = (64.0 + 876.0 * luma).round() as u16;
    assert!(
        planes[0][0].abs_diff(expected) <= 1,
        "corner luma {} for {expected}",
        planes[0][0]
    );
    assert_eq!(planes.len(), 3, "no alpha plane");
}

#[test]
fn the_cpu_renderer_packs_an_alpha_export_as_the_gpu_does() {
    let _lock = gpu_test_lock();
    let renderer = look_renderer();
    let layout = ten_bit(Chroma::Yuv444, true);
    let Some(gpu) = exported(Some(&renderer), layout) else {
        return;
    };
    let cpu = exported(None, layout).expect("the CPU renderer packs 10 bits");
    for (plane, (gpu, cpu)) in gpu.iter().zip(&cpu).enumerate() {
        let largest = gpu
            .iter()
            .zip(cpu)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0);
        // The pictures are within 8 codes of 255 of each other (`caption_look_draws_the_same_on_
        // the_cpu`): 33 of 1023, and more in a straight color divided by a small alpha.
        let bound = if plane == 3 { 36 } else { 160 };
        assert!(
            largest <= bound,
            "plane {plane}: CPU and GPU differ by {largest}"
        );
    }
}
