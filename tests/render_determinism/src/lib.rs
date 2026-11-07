//! CPU renderer frames whose hashes must match on every target, natively and as wasm.
//!
//! Each hash is pinned from linux-x64. `scripts/test-render-determinism.mjs` compares the native
//! values with this crate built for wasm32 and with the packaged browser binding.

use std::sync::Arc;

use alpha2_engine::{
    EngineProject, FrameProvider, FrameRenderer, FramesWithText, QualityProfile, TextTextures,
};
use alpha2_geometry::kurbo::{Point, Vec2};
use alpha2_geometry::{
    CanvasSize, EvaluatedGeometry, EvaluatedItem, EvaluatedPath, EvaluatedSubpath, EvaluatedVertex,
    build_scene,
};
use alpha2_media::{ColorMetadata, SourceId, VideoCodec, YuvEncoding};
use alpha2_model::{LineCap, LineJoin, Paint, Project, Rgba};
use alpha2_render::{
    BlendMode, BlendSpace, Color, CpuImage, CpuRenderer, CpuSource, EffectId, EffectParams,
    EffectRuntime, GAUSSIAN_BLUR_ID, GraphLayer, LayerTransform, Lut3dData, ParamValue,
    PlaneTarget, RenderControls, RenderGraph, RenderSize, SourceView, Yuv420pTargets,
};
use alpha2_time::MediaTime;

/// [`graph_hash`] on linux-x64; equals the renderer's own determinism test pin.
pub const PINNED_GRAPH_HASH: u64 = 4_158_224_015_832_628_688;

/// [`blur_and_stroke_hash`] on linux-x64.
pub const PINNED_BLUR_AND_STROKE_HASH: u64 = 12_803_358_345_075_414_110;

/// [`project_hash`] on linux-x64.
pub const PINNED_PROJECT_HASH: u64 = 8_340_324_065_314_071_693;

/// [`text_fit_hash`] on linux-x64.
pub const PINNED_TEXT_FIT_HASH: u64 = 2_492_912_094_599_430_155;

/// Captions, text generators and a layout with fitted font sizes and clamped lines.
pub const TEXT_FIT_PROJECT_JSON: &str = include_str!("../../cases/text-fit-project.json");

/// The font [`text_fit_hash`] draws with.
const FIXTURE_FONT: &[u8] = include_bytes!("../../fonts/Alpha2Fixture.ttf");

/// The times [`text_fit_hash`] draws, in ticks; the layout's font size animates between them.
pub const TEXT_FIT_TIMES: [i64; 3] = [0, 36_000, 108_000];

/// The host parity project [`project_hash`] renders.
pub const PROJECT_JSON: &str = include_str!("../../cases/host-parity-effects-project.json");

/// The 3D LUT the project's `grade` resource names.
pub const GRADE_CUBE: &str = include_str!("../../corpus/parity-grade.cube");

fn fnv(hash: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(hash, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// A premultiplied RGBA8 test card, with a translucent corner unless `opaque`.
/// `scripts/test-render-determinism.mjs` mirrors it in JavaScript.
#[must_use]
#[allow(clippy::cast_possible_truncation)] // Every value is at most 255.
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

/// FNV-1a hash of the fixed graph's I420 frame (blur, transition, blended transformed layers).
///
/// # Panics
/// Panics if the renderer refuses the fixed graph.
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

/// FNV-1a hash of a 1920x1080 I420 frame through the paths that use transcendental math: a
/// multi-pass blur, a rotated layer, and strokes with a cusp and arcs.
///
/// # Panics
/// Panics if the renderer refuses the graph.
#[must_use]
pub fn blur_and_stroke_hash() -> u64 {
    let size = RenderSize::new(1920, 1080).expect("size");
    // Output does not depend on the thread count.
    let renderer = CpuRenderer::new(4);
    let card = CpuSource::image(Arc::new(
        CpuImage::from_premultiplied_rgba8(size, &test_card(size, false)).expect("card"),
    ));
    let mut graph = RenderGraph::new(size);
    graph.set_blend_space(BlendSpace::LinearLight);
    let source = graph.add_source(SourceView::cpu(&card));
    let blurred = graph
        .add_gaussian_pixels(source, 40.0, EffectRuntime::default())
        .expect("blur");
    let vertex = |x, y, in_tangent: [f64; 2], out_tangent: [f64; 2]| EvaluatedVertex {
        point: Point::new(x, y),
        in_tangent: Vec2::new(in_tangent[0], in_tangent[1]),
        out_tangent: Vec2::new(out_tangent[0], out_tangent[1]),
    };
    // Control points mirrored across the chord put a cusp at the segment's middle.
    let cusp = EvaluatedPath {
        subpaths: vec![EvaluatedSubpath {
            closed: false,
            vertices: vec![
                vertex(0.3, 0.6, [0.0, 0.0], [0.4, -0.2]),
                vertex(0.7, 0.6, [-0.4, -0.2], [0.0, 0.0]),
            ],
        }],
    };
    let stroke = |width, color: [f32; 4]| EvaluatedItem::Stroke {
        paint: Paint::Solid {
            color: Rgba::new(color[0], color[1], color[2], color[3]),
        },
        width,
        opacity: 0.9,
        join: LineJoin::Round,
        miter_limit: 4.0,
        cap: LineCap::Round,
        dash: Vec::new(),
        dash_offset: 0.0,
    };
    let scene = Arc::new(build_scene(
        &[
            EvaluatedItem::Geometry(EvaluatedGeometry::Path(Arc::new(cusp))),
            stroke(0.03, [0.95, 0.8, 0.2, 1.0]),
            EvaluatedItem::Geometry(EvaluatedGeometry::Ellipse {
                center: Point::new(0.62, 0.38),
                size: Vec2::new(0.37, 0.23),
                rotation_degrees: 21.0,
            }),
            stroke(0.011, [0.2, 0.6, 0.95, 0.8]),
        ],
        CanvasSize::new(size.width, size.height),
    ));
    let shapes = graph.add_vector(scene);
    let layer = |source, rotation: f32, scale: f32| GraphLayer {
        source,
        visual: LayerTransform {
            position: [0.5, 0.5],
            anchor: [0.5, 0.5],
            scale: [scale, scale],
            rotation_degrees: rotation,
            opacity: 1.0,
        },
        blend_mode: BlendMode::Normal,
    };
    let output = graph
        .add_composite_over(
            vec![
                layer(blurred, 0.0, 1.0),
                layer(source, 23.0, 0.45),
                layer(shapes, 0.0, 1.0),
            ],
            Color::rgba(0.05, 0.05, 0.08, 1.0),
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

/// Translucent card for the video source, opaque card for the still, at every time.
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

/// No source frames: the text fit project draws generators and captions only.
struct NoFrames;

impl FrameProvider for NoFrames {
    fn texture_for(&self, _: &SourceId, _: MediaTime) -> Option<SourceView<'_>> {
        None
    }
}

/// FNV-1a hash of the text fit project's frames at [`TEXT_FIT_TIMES`], drawn on the CPU with
/// the fixture font. The fit search, the clamp and the ellipsis all take part.
///
/// # Panics
/// Panics if the engine refuses the fixed project or its font.
#[must_use]
pub fn text_fit_hash() -> u64 {
    let project = EngineProject::new(Project::from_json(TEXT_FIT_PROJECT_JSON).expect("project"))
        .expect("snapshot");
    let settings = &project.project().settings;
    let size = RenderSize::new(settings.width, settings.height).expect("size");
    let renderer = CpuRenderer::new(1);
    let mut texts = TextTextures::new();
    texts
        .add_font("Alpha2 Fixture", FIXTURE_FONT, 0)
        .expect("font");
    let mut hash = FNV_OFFSET;
    for ticks in TEXT_FIT_TIMES {
        let frame = project.evaluate(MediaTime(ticks)).expect("frame");
        texts.prepare(&renderer, &frame, size).expect("text");
        let output = renderer
            .render_frame(size, &frame, &FramesWithText::new(&NoFrames, &texts))
            .expect("render");
        hash = fnv(hash, &output.texture.expect("complete").read_rgba8());
    }
    hash
}

/// Every frame time of the parity project, in ticks.
///
/// # Panics
/// Panics if the fixed project does not load.
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

/// FNV-1a hash of every parity project frame rendered on the CPU and packed as an H.264 export.
///
/// # Panics
/// Panics if the engine refuses the fixed project.
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

/// The wasm entry point for [`blur_and_stroke_hash`], a `BigInt` in JavaScript.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(js_name = cpuBlurAndStrokeHash)]
#[must_use]
pub fn cpu_blur_and_stroke_hash() -> u64 {
    blur_and_stroke_hash()
}

/// The wasm entry point for [`text_fit_hash`], a `BigInt` in JavaScript.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(js_name = cpuTextFitHash)]
#[must_use]
pub fn cpu_text_fit_hash() -> u64 {
    text_fit_hash()
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

    #[test]
    fn a_multi_pass_blur_and_a_stroke_hash_to_their_pin() {
        assert_eq!(
            super::blur_and_stroke_hash(),
            super::PINNED_BLUR_AND_STROKE_HASH
        );
    }

    #[test]
    fn the_every_effect_project_hashes_to_its_pin() {
        assert_eq!(super::project_hash(), super::PINNED_PROJECT_HASH);
    }

    #[test]
    fn fitted_and_clamped_text_hashes_to_its_pin() {
        assert_eq!(super::text_fit_hash(), super::PINNED_TEXT_FIT_HASH);
    }
}
