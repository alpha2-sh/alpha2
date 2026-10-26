//! Frames of the CPU renderer whose hashes must be the same natively and as
//! wasm: the renderer promises the same bytes on every target.
//!
//! [`graph_hash`] draws the fixed graph of the renderer's own determinism
//! test (`crates/alpha2-render/src/tests/cpu_determinism.rs`) and must equal
//! its pin. [`project_hash`] renders every frame of the host parity project
//! (`tests/cases/host-parity-effects-project.json`: every built-in effect
//! and transition) through the engine, as an export does. `src/main.rs`
//! prints both natively; `scripts/test-render-determinism.mjs` compares them
//! with this crate built for wasm32 and with the packaged browser binding's
//! CPU renderer drawing the same project.

use std::sync::Arc;

use alpha2_engine::{EngineProject, FrameProvider, FrameRenderer, QualityProfile};
use alpha2_media::{ColorMetadata, SourceId, VideoCodec, YuvEncoding};
use alpha2_model::Project;
use alpha2_render::{
    BlendMode, BlendSpace, Color, CpuImage, CpuRenderer, CpuSource, EffectId, EffectParams,
    EffectRuntime, GAUSSIAN_BLUR_ID, GraphLayer, LayerTransform, Lut3dData, ParamValue,
    PlaneTarget, RenderControls, RenderGraph, RenderSize, SourceView, Yuv420pTargets,
};
use alpha2_time::MediaTime;

/// [`graph_hash`]'s value on linux-x64, where it was first drawn: the pin of
/// the renderer's own determinism test.
pub const PINNED_GRAPH_HASH: u64 = 4_158_224_015_832_628_688;

/// The host parity project [`project_hash`] renders.
pub const PROJECT_JSON: &str = include_str!("../../cases/host-parity-effects-project.json");

/// The 3D LUT the project's `grade` resource names.
pub const GRADE_CUBE: &str = include_str!("../../corpus/parity-grade.cube");

/// FNV-1a over `bytes`, continuing from `hash`: a pin, not a security
/// property.
fn fnv(hash: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(hash, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// A premultiplied RGBA8 test card of `size`: ramps, a checker, noise, and a
/// translucent corner unless `opaque`. The renderer's tests draw the same
/// card; `scripts/test-render-determinism.mjs` makes it in JavaScript too.
#[must_use]
#[allow(clippy::cast_possible_truncation)] // Every value is at most 255 by construction.
pub fn test_card(size: RenderSize, opaque: bool) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(size.width as usize * size.height as usize * 4);
    let mut state = 0x1234_5678_u32;
    for y in 0..size.height {
        for x in 0..size.width {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (state >> 24) as u8;
            let ramp = (x * 255 / size.width.max(1)) as u8;
            let fall = (y * 255 / size.height.max(1)) as u8;
            let checker = if (x / 8 + y / 8) % 2 == 0 { 230 } else { 40 };
            let mut pixel = [
                ramp,
                fall,
                if x < size.width / 2 { checker } else { noise },
                255,
            ];
            if !opaque && x > size.width * 3 / 4 && y > size.height * 3 / 4 {
                let alpha = 96_u16;
                pixel = [
                    (u16::from(pixel[0]) * alpha / 255) as u8,
                    (u16::from(pixel[1]) * alpha / 255) as u8,
                    (u16::from(pixel[2]) * alpha / 255) as u8,
                    alpha as u8,
                ];
            }
            pixels.extend_from_slice(&pixel);
        }
    }
    pixels
}

/// `size`'s I420 planes, packed back to back, and their targets.
fn i420(size: RenderSize) -> Vec<u8> {
    let luma = size.width as usize * size.height as usize;
    vec![0; luma + luma / 2]
}

fn targets(planes: &mut [u8], size: RenderSize) -> Yuv420pTargets<'_> {
    let width = size.width as usize;
    let luma = width * size.height as usize;
    let (y, chroma) = planes.split_at_mut(luma);
    let (u, v) = chroma.split_at_mut(luma / 4);
    Yuv420pTargets {
        y: PlaneTarget::new(y, width),
        u: PlaneTarget::new(u, width / 2),
        v: PlaneTarget::new(v, width / 2),
    }
}

/// The FNV-1a hash of the fixed graph's I420 frame: a blur, a transition,
/// rotated and scaled layers in several blend modes over a background.
///
/// # Panics
/// Panics if the renderer refuses the fixed graph, which is a bug here.
#[must_use]
pub fn graph_hash() -> u64 {
    let size = RenderSize::new(160, 90).expect("size");
    let renderer = CpuRenderer::new(1);
    let card = CpuSource::image(Arc::new(
        CpuImage::from_premultiplied_rgba8(size, &test_card(size, false)).expect("card"),
    ));
    let mut graph = RenderGraph::new(size);
    graph.set_blend_space(BlendSpace::LinearLight);
    let source = graph.add_source(SourceView::cpu(&card));
    let blurred = graph
        .add_effect(
            EffectId::from_static(GAUSSIAN_BLUR_ID, 1),
            source,
            EffectParams::new().with("radius", ParamValue::Float(0.04)),
            EffectRuntime::default(),
        )
        .expect("blur");
    let crossed = graph
        .add_transition(
            EffectId::from_static("alpha2.transition.cross-warp", 1),
            source,
            blurred,
            EffectParams::new(),
            EffectRuntime {
                progress: 0.4,
                ratio: 160.0 / 90.0,
                ..EffectRuntime::default()
            },
        )
        .expect("transition");
    let layer = |source, rotation: f32, scale: f32, blend_mode| GraphLayer {
        source,
        visual: LayerTransform {
            position: [0.45, 0.55],
            anchor: [0.5, 0.5],
            scale: [scale, scale * 0.9],
            rotation_degrees: rotation,
            opacity: 0.8,
        },
        blend_mode,
    };
    let output = graph
        .add_composite_over(
            vec![
                layer(crossed, 0.0, 1.0, BlendMode::Normal),
                layer(source, 17.0, 0.6, BlendMode::SoftLight),
                layer(blurred, -8.0, 0.35, BlendMode::Hue),
            ],
            Color::rgba(0.1, 0.2, 0.3, 1.0),
        )
        .expect("composite");
    graph.set_output(output).expect("output");
    let mut planes = i420(size);
    renderer
        .render_graph_into_yuv420p(
            &graph,
            YuvEncoding::new(ColorMetadata::EXPORT, alpha2_media::ChromaLocation::Left),
            &mut targets(&mut planes, size),
        )
        .expect("render");
    fnv(FNV_OFFSET, &planes)
}

/// Every source of the parity project shows one card at every time: the
/// translucent card for the video, the opaque one for the still.
struct Cards {
    video: CpuSource,
    still: CpuSource,
}

impl FrameProvider for Cards {
    fn texture_for(&self, source: &SourceId, _time: MediaTime) -> Option<SourceView<'_>> {
        match source.as_str() {
            "video" => Some(SourceView::cpu(&self.video)),
            "still" => Some(SourceView::cpu(&self.still)),
            _ => None,
        }
    }
}

/// The frame times [`project_hash`] renders: every frame of the project's
/// timeline at its frame rate, as tick counts.
///
/// # Panics
/// Panics if the fixed project does not load, which is a bug here.
#[must_use]
pub fn project_frame_times() -> Vec<i64> {
    let project = Project::from_json(PROJECT_JSON).expect("project");
    let end = project
        .timeline
        .tracks
        .iter()
        .flat_map(|track| &track.clips)
        .map(|clip| clip.timeline_range.end().ticks())
        .max()
        .unwrap_or(0);
    let rate = project.settings.frame_rate;
    let mut times = Vec::new();
    for frame in 0_i64.. {
        let ticks = rate.frame_start(frame).expect("frame time").ticks();
        if ticks >= end {
            break;
        }
        times.push(ticks);
    }
    times
}

/// The FNV-1a hash of every frame of the parity project rendered on the CPU
/// at export quality, packed for H.264 as an export packs it, in frame
/// order.
///
/// # Panics
/// Panics if the engine refuses the fixed project, which is a bug here.
#[must_use]
pub fn project_hash() -> u64 {
    let project =
        EngineProject::new(Project::from_json(PROJECT_JSON).expect("project")).expect("snapshot");
    let settings = &project.project().settings;
    let quality = QualityProfile::export();
    let size = quality.export_size(settings).expect("size");
    let mut renderer = CpuRenderer::new(1);
    renderer
        .register_lut3d("grade", &Lut3dData::from_cube(GRADE_CUBE).expect("cube"))
        .expect("lut");
    renderer.set_texture_budget_bytes(quality.texture_budget_bytes);
    renderer.set_quality_controls(quality.max_filter_samples, quality.motion_blur_subframes);
    let card = |opaque| {
        CpuSource::image(Arc::new(
            CpuImage::from_premultiplied_rgba8(size, &test_card(size, opaque)).expect("card"),
        ))
    };
    let cards = Cards {
        video: card(false),
        still: card(true),
    };
    let mut hash = FNV_OFFSET;
    for ticks in project_frame_times() {
        let frame = project.evaluate(MediaTime(ticks)).expect("frame");
        let mut planes = i420(size);
        let report = renderer
            .render_frame_into_yuv420p(
                size,
                &frame,
                &cards,
                YuvEncoding::export(VideoCodec::H264),
                &mut targets(&mut planes, size),
            )
            .expect("render");
        assert!(report.is_complete(), "every source is supplied");
        hash = fnv(hash, &planes);
    }
    hash
}

/// The wasm entry point for [`graph_hash`], a `BigInt` in JavaScript.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(js_name = cpuGraphHash)]
#[must_use]
pub fn cpu_graph_hash() -> u64 {
    graph_hash()
}

/// The wasm entry point for [`project_hash`], a `BigInt` in JavaScript.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(js_name = cpuProjectHash)]
#[must_use]
pub fn cpu_project_hash() -> u64 {
    project_hash()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_fixed_graph_draws_its_pinned_frame() {
        assert_eq!(super::graph_hash(), super::PINNED_GRAPH_HASH);
    }
}
