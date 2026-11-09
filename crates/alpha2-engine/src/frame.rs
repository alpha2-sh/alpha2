//! Rendering of evaluated timeline frames.
//!
//! The renderer knows render graphs, not timelines. This module checks that every source frame
//! of an evaluated [`FrameDesc`] has arrived, compiles it into a [`RenderGraph`] and submits it.

use alpha2_eval::{
    ActiveClip, ActiveComposition, ActiveEffect, ActiveTransition, EvaluatedVisual, FrameDesc,
    GeneratorPlan, SampleContext, ShapesPlan, StepOp, sample_scalar, sample_visual,
};
use alpha2_media::{PixelLayout, SourceId, YuvEncoding};
use alpha2_model::{
    ClipBlendMode, EffectValue, Generator, MaskMode, MatteMode, Rgb, ShapeMask, TextStyle,
};
use alpha2_render::{
    BlendMode, CacheIntent, Color, CpuImage, CpuRenderer, EffectCatalog, EffectId, EffectInputs,
    EffectParams, EffectResources, EffectRuntime, FrameSubmitTimings, GENERATOR_EFFECT_ID,
    GraphError, GraphLayer, LayerTransform, MASK_COMBINE_EFFECT_IDS, MASK_COMBINE_NAMES,
    MAX_SHAPE_MASKS, Nv12PlaneTargets, ParamSpec, ParamType, ParamValue, PendingSourceWork,
    PlaneTarget, RenderCacheKey, RenderControls, RenderError, RenderGraph, RenderNodeId,
    RenderNodeKind, RenderSize, RenderedTexture, Renderer, SourceView, TRACK_MATTE_EFFECT_ID,
    Yuv420pReadback, Yuv420pTargets,
};
use alpha2_time::MediaTime;

use crate::TextTexture;
use crate::layout::draw::LaidOut;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use web_time::Instant;

/// Supplies decoded source frames as GPU textures to the renderer.
///
/// Implementations own decode caches and texture lifetimes and lend textures as
/// [`SourceView`]s. Textures must hold premultiplied, sRGB-encoded RGBA in a non-sRGB GPU
/// format, as returned by `Renderer::upload_rgba8` or `Renderer::render_yuv420p`.
pub trait FrameProvider {
    /// Returns a texture for `source` at the requested source timestamp.
    ///
    /// The frame presented at `time`, resolved as [`alpha2_media::Decoder::seek`] does (last
    /// timestamp at or before `time` plus the presentation slack, clamped to the stream).
    ///
    /// `None` means the frame has not arrived; players may hold the previous output while
    /// exporters wait and retry.
    fn texture_for(&self, source: &SourceId, time: MediaTime) -> Option<SourceView<'_>>;

    /// A key for the texture [`Self::texture_for`] returns, promising its pixels depend on
    /// nothing outside the key (content, frame, decode and conversion). Keyed sources let drawn
    /// results be reused ([`alpha2_render::Renderer::set_result_cache_budget`]); the default
    /// `None` disables reuse.
    fn source_key(&self, _source: &SourceId, _time: MediaTime) -> Option<RenderCacheKey> {
        None
    }

    /// Returns presentation work that has not yet been submitted for this
    /// physical source texture. The default covers already-ready textures.
    fn pending_source_work(
        &self,
        _source: &SourceId,
        _time: MediaTime,
    ) -> Option<&PendingSourceWork> {
        None
    }

    /// Returns the prepared text [`crate::text_key`] names. With the default (none), a frame
    /// showing text fails with [`RenderError::MissingText`].
    fn text(&self, _key: u64) -> Option<TextTexture<'_>> {
        None
    }

    /// Returns the prepared layout [`crate::layout_key`] names. With the default (none), a frame
    /// showing a layout fails with [`RenderError::MissingText`].
    fn layout(&self, _key: u128) -> Option<std::sync::Arc<crate::PreparedLayout>> {
        None
    }
}

/// Outcome of one render attempt when sources may arrive asynchronously.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderReport {
    /// Sources whose requested frame was unavailable.
    pub missing_sources: Vec<SourceId>,
}

impl RenderReport {
    /// Returns whether every requested source frame was available.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.missing_sources.is_empty()
    }
}

/// Result of attempting to render one evaluated timeline frame.
#[derive(Debug)]
pub struct FrameRender<I = RenderedTexture> {
    /// Availability information collected before anything is drawn.
    pub report: RenderReport,
    /// Rendered output, absent when one or more source frames are unavailable.
    pub texture: Option<I>,
}

impl<I> FrameRender<I> {
    /// Returns whether a texture was rendered from a complete source set.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.texture.is_some()
    }
}

/// Result of rendering and scheduling one frame for planar YUV420 readback.
#[derive(Debug)]
pub struct FrameYuv420pRender {
    /// Availability information collected before any GPU work is submitted.
    pub report: RenderReport,
    /// Submitted readback, absent when source frames are unavailable.
    pub readback: Option<Yuv420pReadback>,
}

impl FrameYuv420pRender {
    /// Returns whether a complete frame was submitted for readback.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.readback.is_some()
    }
}

/// Result of rendering one frame into caller-owned NV12 plane textures.
#[derive(Debug)]
#[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
pub struct FrameNv12Render {
    /// Availability information collected before any GPU work is submitted.
    pub report: RenderReport,
    /// Whether the pack was submitted; false when a source was missing.
    pub submitted: bool,
}

/// One evaluated frame compiled into a render graph, not yet submitted.
#[derive(Debug)]
pub struct FrameGraph<'a> {
    /// Availability information collected before the graph was built.
    pub report: RenderReport,
    /// The frame's graph, absent when source frames are unavailable.
    pub graph: Option<RenderGraph<'a>>,
    /// Unsubmitted presentation work behind the source textures, folded into the submission.
    pub pending: Vec<&'a PendingSourceWork>,
}

/// Timeline-frame rendering on the GPU [`Renderer`] or the [`alpha2_render::CpuRenderer`].
/// GPU-only readback and encoder paths are in [`GpuFrameRenderer`].
pub trait FrameRenderer: EffectCatalog + RenderControls {
    /// A [`RenderedTexture`] on the GPU, an [`alpha2_render::CpuImage`] on the CPU.
    type Image;

    /// Resolves the textures required by an evaluated frame.
    ///
    /// Performs no fallback; players and exporters apply their own policy.
    #[must_use]
    fn inspect_frame(&self, frame: &FrameDesc, frames: &dyn FrameProvider) -> RenderReport {
        available_textures(frame, frames).0
    }

    /// Compiles one evaluator result into a render graph without drawing.
    ///
    /// An incomplete source set returns its report and no graph.
    ///
    /// # Errors
    /// Returns [`RenderError`] when an effect is unknown or a visual or
    /// caption is invalid.
    fn build_frame_graph<'a>(
        &self,
        size: RenderSize,
        frame: &'a FrameDesc<'a>,
        frames: &'a dyn FrameProvider,
    ) -> Result<FrameGraph<'a>, RenderError>;

    /// Draws a graph from [`Self::build_frame_graph`] with its pending source work.
    ///
    /// # Errors
    /// Returns [`RenderError`] when a pass fails.
    fn render_frame_graph(
        &self,
        graph: &RenderGraph<'_>,
        pending: &[&PendingSourceWork],
    ) -> Result<Self::Image, RenderError>;

    /// Renders one pure evaluator result using sources supplied by the host.
    ///
    /// An incomplete source set returns its report and draws nothing.
    ///
    /// # Errors
    /// Returns [`RenderError`] when a visual or render target is invalid.
    fn render_frame(
        &self,
        size: RenderSize,
        frame: &FrameDesc,
        frames: &dyn FrameProvider,
    ) -> Result<FrameRender<Self::Image>, RenderError> {
        let FrameGraph {
            report,
            graph,
            pending,
        } = self.build_frame_graph(size, frame, frames)?;
        let texture = match graph {
            Some(graph) => Some(self.render_frame_graph(&graph, &pending)?),
            None => None,
        };
        Ok(FrameRender { report, texture })
    }

    /// Renders one evaluator result into caller-owned planar YUV420 `targets`.
    ///
    /// An incomplete source set writes nothing. The output size must be even.
    ///
    /// # Errors
    /// Returns [`RenderError`] for an invalid graph or a target too small.
    fn render_frame_into_yuv420p(
        &self,
        size: RenderSize,
        frame: &FrameDesc,
        frames: &dyn FrameProvider,
        encoding: YuvEncoding,
        targets: &mut Yuv420pTargets<'_>,
    ) -> Result<RenderReport, RenderError>;

    /// [`Self::render_frame_into_yuv420p`] for any `layout`, one target per plane in
    /// [`PixelLayout::planes`] order.
    ///
    /// # Errors
    /// Returns [`RenderError`] for an invalid graph, a picture the layout
    /// cannot subsample, or a target too small.
    fn render_frame_into_planes(
        &self,
        size: RenderSize,
        frame: &FrameDesc,
        frames: &dyn FrameProvider,
        encoding: YuvEncoding,
        layout: PixelLayout,
        targets: &mut [PlaneTarget<'_>],
    ) -> Result<RenderReport, RenderError>;
}

fn yuv420p_targets<'a>(
    targets: &'a mut [PlaneTarget<'_>],
) -> Result<Yuv420pTargets<'a>, RenderError> {
    let [y, u, v] = targets else {
        return Err(RenderError::TextureReadback(
            alpha2_render::ReadbackFailure::Layout,
        ));
    };
    Ok(Yuv420pTargets {
        y: y.reborrow(),
        u: u.reborrow(),
        v: v.reborrow(),
    })
}

/// GPU frame paths that end on the GPU: an unmapped YUV420 readback or encoder NV12 planes.
pub trait GpuFrameRenderer: FrameRenderer<Image = RenderedTexture> {
    /// Renders and converts to YUV420 in one command buffer.
    ///
    /// An incomplete source set submits nothing. The output size must be even.
    ///
    /// # Errors
    /// Returns [`RenderError`] for invalid graph, target, or YUV layout data.
    fn render_frame_yuv420p(
        &self,
        size: RenderSize,
        frame: &FrameDesc,
        frames: &dyn FrameProvider,
        encoding: YuvEncoding,
    ) -> Result<FrameYuv420pRender, RenderError>;

    /// Renders one frame for YUV420 readback and reports where the call went.
    ///
    /// Same output as [`GpuFrameRenderer::render_frame_yuv420p`], with stage spans written to
    /// `timings`.
    ///
    /// # Errors
    /// Returns [`RenderError`] for invalid graph, target, or YUV layout data.
    fn render_frame_yuv420p_timed(
        &self,
        size: RenderSize,
        frame: &FrameDesc,
        frames: &dyn FrameProvider,
        encoding: YuvEncoding,
        timings: &mut FrameSubmitTimings,
    ) -> Result<FrameYuv420pRender, RenderError>;

    /// Renders one frame and packs it straight into caller-owned NV12 plane
    /// textures, with no readback.
    ///
    /// [`GpuFrameRenderer::render_frame_yuv420p_timed`] ending in two textures for a hardware
    /// encoder instead of a staging buffer. An incomplete source set submits nothing.
    ///
    /// # Errors
    /// Returns [`RenderError`] for invalid graph or plane-target data.
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    fn render_frame_nv12_timed(
        &self,
        size: RenderSize,
        frame: &FrameDesc,
        frames: &dyn FrameProvider,
        planes: Nv12PlaneTargets<'_>,
        encoding: YuvEncoding,
        timings: &mut FrameSubmitTimings,
    ) -> Result<FrameNv12Render, RenderError>;
}

/// [`FrameRenderer::build_frame_graph`] for any renderer; only the effect catalog differs.
fn build_frame_graph<'a>(
    catalog: &dyn EffectCatalog,
    size: RenderSize,
    frame: &'a FrameDesc<'a>,
    frames: &'a dyn FrameProvider,
) -> Result<FrameGraph<'a>, RenderError> {
    validate_project_effects(catalog, frame)?;
    let (report, mut available) = available_textures(frame, frames);
    if !report.is_complete() {
        return Ok(FrameGraph {
            report,
            graph: None,
            pending: Vec::new(),
        });
    }
    attach_texts(&mut available, frames, size);
    let captions = caption_layers(frame, frames, size)?;
    let graph = frame_graph(catalog, size, frame, &available, &captions)?;
    Ok(FrameGraph {
        report,
        graph: Some(graph),
        pending: pending_source_work(&available, frames),
    })
}

impl FrameRenderer for Renderer {
    type Image = RenderedTexture;

    fn build_frame_graph<'a>(
        &self,
        size: RenderSize,
        frame: &'a FrameDesc<'a>,
        frames: &'a dyn FrameProvider,
    ) -> Result<FrameGraph<'a>, RenderError> {
        build_frame_graph(self, size, frame, frames)
    }

    fn render_frame_graph(
        &self,
        graph: &RenderGraph<'_>,
        pending: &[&PendingSourceWork],
    ) -> Result<RenderedTexture, RenderError> {
        self.render_graph(graph, pending)
    }

    fn render_frame_into_yuv420p(
        &self,
        size: RenderSize,
        frame: &FrameDesc,
        frames: &dyn FrameProvider,
        encoding: YuvEncoding,
        targets: &mut Yuv420pTargets<'_>,
    ) -> Result<RenderReport, RenderError> {
        let render = self.render_frame_yuv420p(size, frame, frames, encoding)?;
        if let Some(readback) = render.readback {
            // A browser maps buffers only on its event loop, which a blocking call never yields to.
            #[cfg(target_arch = "wasm32")]
            {
                drop((readback, targets));
                return Err(RenderError::TextureReadback(
                    alpha2_render::ReadbackFailure::NotMapped,
                ));
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                readback.wait_ready()?;
                readback.finish_into(targets)?;
            }
        }
        Ok(render.report)
    }

    fn render_frame_into_planes(
        &self,
        size: RenderSize,
        frame: &FrameDesc,
        frames: &dyn FrameProvider,
        encoding: YuvEncoding,
        layout: PixelLayout,
        targets: &mut [PlaneTarget<'_>],
    ) -> Result<RenderReport, RenderError> {
        if layout.is_yuv420p() {
            let mut planes = yuv420p_targets(targets)?;
            return self.render_frame_into_yuv420p(size, frame, frames, encoding, &mut planes);
        }
        let render = self.render_frame(size, frame, frames)?;
        if let Some(texture) = render.texture {
            let readback = self.begin_layout_readback(&texture, encoding, layout)?;
            // A browser cannot block on a mapping.
            #[cfg(target_arch = "wasm32")]
            {
                drop((readback, targets));
                return Err(RenderError::TextureReadback(
                    alpha2_render::ReadbackFailure::NotMapped,
                ));
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                readback.wait_ready()?;
                readback.finish_into_planes(targets)?;
            }
        }
        Ok(render.report)
    }
}

impl FrameRenderer for CpuRenderer {
    type Image = Arc<CpuImage>;

    fn build_frame_graph<'a>(
        &self,
        size: RenderSize,
        frame: &'a FrameDesc<'a>,
        frames: &'a dyn FrameProvider,
    ) -> Result<FrameGraph<'a>, RenderError> {
        build_frame_graph(self, size, frame, frames)
    }

    fn render_frame_graph(
        &self,
        graph: &RenderGraph<'_>,
        _pending: &[&PendingSourceWork],
    ) -> Result<Arc<CpuImage>, RenderError> {
        self.execute_graph(graph)
            .map_err(GraphError::into_render_error)
    }

    fn render_frame_into_yuv420p(
        &self,
        size: RenderSize,
        frame: &FrameDesc,
        frames: &dyn FrameProvider,
        encoding: YuvEncoding,
        targets: &mut Yuv420pTargets<'_>,
    ) -> Result<RenderReport, RenderError> {
        let FrameGraph { report, graph, .. } = self.build_frame_graph(size, frame, frames)?;
        if let Some(graph) = graph {
            self.render_graph_into_yuv420p(&graph, encoding, targets)?;
        }
        Ok(report)
    }

    fn render_frame_into_planes(
        &self,
        size: RenderSize,
        frame: &FrameDesc,
        frames: &dyn FrameProvider,
        encoding: YuvEncoding,
        layout: PixelLayout,
        targets: &mut [PlaneTarget<'_>],
    ) -> Result<RenderReport, RenderError> {
        if layout.is_yuv420p() {
            let mut planes = yuv420p_targets(targets)?;
            return self.render_frame_into_yuv420p(size, frame, frames, encoding, &mut planes);
        }
        let render = self.render_frame(size, frame, frames)?;
        if let Some(image) = render.texture {
            alpha2_render::pack_cpu_image(&image, encoding, layout, targets)?;
        }
        Ok(render.report)
    }
}

impl GpuFrameRenderer for Renderer {
    fn render_frame_yuv420p(
        &self,
        size: RenderSize,
        frame: &FrameDesc,
        frames: &dyn FrameProvider,
        encoding: YuvEncoding,
    ) -> Result<FrameYuv420pRender, RenderError> {
        render_frame_yuv420p_measured(self, size, frame, frames, encoding, None)
    }

    fn render_frame_yuv420p_timed(
        &self,
        size: RenderSize,
        frame: &FrameDesc,
        frames: &dyn FrameProvider,
        encoding: YuvEncoding,
        timings: &mut FrameSubmitTimings,
    ) -> Result<FrameYuv420pRender, RenderError> {
        render_frame_yuv420p_measured(self, size, frame, frames, encoding, Some(timings))
    }

    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    fn render_frame_nv12_timed(
        &self,
        size: RenderSize,
        frame: &FrameDesc,
        frames: &dyn FrameProvider,
        planes: Nv12PlaneTargets<'_>,
        encoding: YuvEncoding,
        timings: &mut FrameSubmitTimings,
    ) -> Result<FrameNv12Render, RenderError> {
        let mut clock = StageClock::start(true);
        let mut collected = FrameSubmitTimings::default();
        let FrameGraph {
            report,
            graph,
            pending,
        } = self.build_frame_graph(size, frame, frames)?;
        collected.graph_build = clock.lap();
        let submitted = graph.is_some();
        if let Some(graph) = graph {
            self.render_graph_nv12_timed(&graph, &pending, planes, encoding, &mut collected)?;
        }
        *timings = collected;
        Ok(FrameNv12Render { report, submitted })
    }
}

fn render_frame_yuv420p_measured(
    renderer: &Renderer,
    size: RenderSize,
    frame: &FrameDesc,
    frames: &dyn FrameProvider,
    encoding: YuvEncoding,
    timings: Option<&mut FrameSubmitTimings>,
) -> Result<FrameYuv420pRender, RenderError> {
    let measure = timings.is_some();
    let mut clock = StageClock::start(measure);
    let mut collected = FrameSubmitTimings::default();
    let FrameGraph {
        report,
        graph,
        pending,
    } = renderer.build_frame_graph(size, frame, frames)?;
    collected.graph_build = clock.lap();
    let readback = match graph {
        Some(graph) if measure => {
            Some(renderer.render_graph_yuv420p_timed(&graph, &pending, encoding, &mut collected)?)
        }
        Some(graph) => Some(renderer.render_graph_yuv420p(&graph, &pending, encoding)?),
        None => None,
    };
    if let Some(timings) = timings {
        *timings = collected;
    }
    Ok(FrameYuv420pRender { report, readback })
}

/// A lap timer that reads no clock unless a caller asked for timings.
#[derive(Copy, Clone)]
struct StageClock(Option<Instant>);

impl StageClock {
    fn start(enabled: bool) -> Self {
        Self(enabled.then(Instant::now))
    }

    fn lap(&mut self) -> Duration {
        let Some(mark) = self.0 else {
            return Duration::ZERO;
        };
        let now = Instant::now();
        self.0 = Some(now);
        now.duration_since(mark)
    }
}

fn validate_project_effects(
    renderer: &dyn EffectCatalog,
    frame: &FrameDesc,
) -> Result<(), RenderError> {
    require_effects(renderer, frame_effects(frame)).map_err(|_| RenderError::UnknownEffect)
}

/// Every effect `frame` draws, by id and version.
fn frame_effects<'a>(frame: &'a FrameDesc) -> impl Iterator<Item = (&'a str, u32)> {
    let composition_effects = frame
        .visual
        .iter()
        .flat_map(|clip| clip.composition.into_iter().chain(clip.source_composition))
        .flat_map(|composition| composition.plan().effects())
        .map(|effect| (effect.id.as_str(), effect.version));
    let transition_effects = frame
        .transitions
        .iter()
        .map(|transition| (transition.effect.id(), transition.effect.version()));
    composition_effects.chain(transition_effects)
}

/// Checks that `renderer` draws every effect `frame` uses, naming the first missing one
/// (unlike the [`RenderError`] that [`FrameRenderer`] returns).
///
/// # Errors
/// Returns [`crate::EngineError::UnregisteredEffect`].
pub fn check_frame_effects<C: EffectCatalog + ?Sized>(
    renderer: &C,
    frame: &FrameDesc,
) -> Result<(), crate::EngineError> {
    require_effects(renderer, frame_effects(frame))
}

pub(crate) fn require_effects<'a, C: EffectCatalog + ?Sized>(
    renderer: &C,
    effects: impl IntoIterator<Item = (&'a str, u32)>,
) -> Result<(), crate::EngineError> {
    for (effect, version) in effects {
        if renderer.project_effect_id(effect, version).is_none() {
            return Err(crate::EngineError::UnregisteredEffect {
                effect: effect.to_owned(),
                version,
            });
        }
    }
    Ok(())
}

fn pending_source_work<'a>(
    available: &[(&ActiveClip, ClipFrames<'_>)],
    frames: &'a dyn FrameProvider,
) -> Vec<&'a PendingSourceWork> {
    let mut work = Vec::new();
    for (clip, textures) in available {
        if textures.source.is_some() {
            work.extend(frames.pending_source_work(clip.source_id, clip.source_time));
        }
        for composition in clip.composition.iter().chain(&clip.source_composition) {
            for (_, source, time) in composition.media() {
                work.extend(frames.pending_source_work(source, time));
            }
        }
    }
    work
}

#[derive(Copy, Clone, Debug)]
struct Frame<'t> {
    view: SourceView<'t>,
    key: Option<RenderCacheKey>,
}

impl<'t> Frame<'t> {
    fn add_to(self, graph: &mut RenderGraph<'t>) -> RenderNodeId {
        match self.key {
            Some(key) => graph.add_source_with_key(self.view, key),
            None => graph.add_source(self.view),
        }
    }
}

/// A text generator's prepared texture, placed on a transparent canvas-sized picture.
#[derive(Copy, Clone, Debug)]
struct PlacedText<'t> {
    texture: TextTexture<'t>,
    visual: LayerTransform,
}

impl<'t> PlacedText<'t> {
    fn add_to(self, graph: &mut RenderGraph<'t>) -> Result<RenderNodeId, RenderError> {
        let layer = text_layer(graph, self.texture, self.visual)?;
        graph
            .add_composite(vec![layer])
            .map_err(GraphError::into_render_error)
    }
}

fn keyed_source<'t>(
    graph: &mut RenderGraph<'t>,
    view: SourceView<'t>,
    key: Option<RenderCacheKey>,
) -> RenderNodeId {
    match key {
        Some(key) => graph.add_source_with_key(view, key),
        None => graph.add_source(view),
    }
}

/// A prepared text placed on the graph's picture by `visual`, as one layer.
///
/// Without shadows the layer is the keyed texture itself. With shadows it is a new picture:
/// background boxes, then shadows (last listed lowest), then glyphs. Shadows are cast by the
/// placed glyphs, so offset and blur are in picture pixels.
pub(crate) fn text_layer<'t>(
    graph: &mut RenderGraph<'t>,
    texture: TextTexture<'t>,
    visual: LayerTransform,
) -> Result<GraphLayer, RenderError> {
    let glyphs = keyed_source(graph, texture.texture, texture.cache_key);
    if texture.shadows.is_empty() {
        return Ok(GraphLayer {
            source: glyphs,
            visual,
            blend_mode: BlendMode::Normal,
        });
    }
    let placed = graph
        .add_composite(vec![GraphLayer {
            source: glyphs,
            visual,
            blend_mode: BlendMode::Normal,
        }])
        .map_err(GraphError::into_render_error)?;
    let mut layers = Vec::with_capacity(texture.shadows.len() + 2);
    if let Some((background, key)) = texture.background {
        layers.push(GraphLayer {
            source: keyed_source(graph, background, key),
            visual,
            blend_mode: BlendMode::Normal,
        });
    }
    for shadow in texture.shadows.iter().rev() {
        let cast = graph
            .add_shadow(placed, shadow.offset, shadow.sigma, shadow.color)
            .map_err(GraphError::into_render_error)?;
        layers.push(identity_layer(cast));
    }
    layers.push(identity_layer(placed));
    let look = graph
        .add_composite(layers)
        .map_err(GraphError::into_render_error)?;
    Ok(identity_layer(look))
}

#[derive(Clone, Debug)]
enum StepFrame<'t> {
    Media(Frame<'t>),
    Text(PlacedText<'t>),
    Layout(LaidOut<'t>),
}

/// The host-supplied frames one visual clip draws.
#[derive(Clone, Debug, Default)]
struct ClipFrames<'t> {
    /// Index in [`FrameDesc::visual`], which [`alpha2_eval::ActiveCaption::layers_below`]
    /// counts in (drawn clips are only a subset).
    layer: usize,
    /// The clip's own source frame; `None` when the source is generated or not shown.
    source: Option<Frame<'t>>,
    /// The text of the clip's text generator source, once prepared.
    text: Option<PlacedText<'t>>,
    /// The clip's layout generator source, once prepared.
    layout: Option<LaidOut<'t>>,
    /// Per-step frame or text by step index; `None` for other steps.
    media: Vec<Option<StepFrame<'t>>>,
    /// The same for the clip's composition source.
    source_media: Vec<Option<StepFrame<'t>>>,
}

/// Each visual clip with its source frames. A missing frame is reported and its clip left out.
fn available_textures<'frame, 'eval, 'texture>(
    frame: &'frame FrameDesc<'eval>,
    frames: &'texture dyn FrameProvider,
) -> (
    RenderReport,
    Vec<(&'frame ActiveClip<'eval>, ClipFrames<'texture>)>,
) {
    let mut report = RenderReport::default();
    let mut available = Vec::with_capacity(frame.visual.len());
    let mut lookup = |source: &SourceId, time| {
        let texture = frames.texture_for(source, time);
        if texture.is_none() && !report.missing_sources.contains(source) {
            report.missing_sources.push(source.clone());
        }
        texture.map(|view| Frame {
            view,
            key: frames.source_key(source, time),
        })
    };
    // A matte no active clip names is drawn nowhere; it needs no frame.
    for (layer, clip) in frame.visual.iter().enumerate() {
        if !frame.draws(clip) {
            continue;
        }
        let mut textures = ClipFrames {
            layer,
            ..ClipFrames::default()
        };
        let mut complete = true;
        if clip.needs_source() {
            textures.source = lookup(clip.source_id, clip.source_time);
            complete &= textures.source.is_some();
        }
        for (composition, media) in [
            (clip.composition, &mut textures.media),
            (clip.source_composition, &mut textures.source_media),
        ] {
            let Some(composition) = composition else {
                continue;
            };
            *media = vec![None; composition.plan().steps.len()];
            for (step, source, time) in composition.media() {
                media[step] = lookup(source, time).map(StepFrame::Media);
                complete &= media[step].is_some();
            }
        }
        if complete {
            available.push((clip, textures));
        }
    }
    (report, available)
}

/// Attaches prepared text and layouts to each available clip. Missing ones are left for the
/// graph to report.
fn attach_texts<'t>(
    available: &mut [(&ActiveClip<'_>, ClipFrames<'t>)],
    frames: &'t dyn FrameProvider,
    size: RenderSize,
) {
    let laid_out = |plan: Option<&GeneratorPlan>, time: MediaTime| {
        let plan = plan?.layout()?;
        let layout = frames.layout(crate::layout_key(plan.evaluate(time).key, size))?;
        LaidOut::attach(layout, frames)
    };
    let lookup = |generator: &Generator, time: MediaTime| match generator {
        Generator::Text { text, style } => frames
            .text(alpha2_layout::animated_text_key(
                text,
                style,
                crate::text::picture(size),
                time,
            ))
            .map(|texture| PlacedText {
                texture,
                visual: text_placement(texture, style, size),
            }),
        _ => None,
    };
    for (clip, textures) in available {
        textures.text = clip
            .generator
            .and_then(|generator| lookup(generator, clip.source_time));
        textures.layout = laid_out(clip.plan, clip.source_time);
        for (composition, media) in [
            (clip.composition, &mut textures.media),
            (clip.source_composition, &mut textures.source_media),
        ] {
            let Some(composition) = composition else {
                continue;
            };
            for (step, slot) in composition.plan().steps.iter().zip(media.iter_mut()) {
                match &step.op {
                    StepOp::Generator {
                        generator: Generator::Layout { .. },
                        start,
                        plan,
                    } => {
                        let time = MediaTime(
                            step.time(composition.time())
                                .ticks()
                                .saturating_add(start.ticks()),
                        );
                        *slot = laid_out(plan.as_deref(), time).map(StepFrame::Layout);
                    }
                    StepOp::Generator {
                        generator, start, ..
                    } => {
                        let time =
                            crate::text::generator_time(step.time(composition.time()), *start);
                        *slot = lookup(generator, time).map(StepFrame::Text);
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Places prepared text at its pixel size with its anchor at the style position.
fn text_placement(texture: TextTexture<'_>, style: &TextStyle, size: RenderSize) -> LayerTransform {
    #[allow(clippy::cast_precision_loss)]
    let scale = [
        texture.width as f32 / size.width as f32,
        texture.height as f32 / size.height as f32,
    ];
    LayerTransform {
        position: [style.position.x, style.position.y],
        anchor: [
            box_anchor(style.anchor.x, texture.width, texture.margin[0]),
            box_anchor(style.anchor.y, texture.height, texture.margin[1]),
        ],
        scale,
        rotation_degrees: 0.0,
        opacity: 1.0,
    }
}

/// A prepared caption and where it goes in the layer stack.
struct CaptionLayer<'a> {
    /// Visual clips under it, drawn or not.
    layers_below: usize,
    texture: TextTexture<'a>,
    visual: LayerTransform,
}

fn caption_graph_layer<'a>(
    graph: &mut RenderGraph<'a>,
    caption: &CaptionLayer<'a>,
) -> Result<GraphLayer, RenderError> {
    text_layer(graph, caption.texture, caption.visual)
}

fn caption_layers<'a>(
    frame: &FrameDesc,
    frames: &'a dyn FrameProvider,
    size: RenderSize,
) -> Result<Vec<CaptionLayer<'a>>, RenderError> {
    frame
        .captions
        .iter()
        .map(|caption| {
            let texture = frames
                .text(alpha2_layout::animated_text_key(
                    caption.text,
                    caption.style,
                    crate::text::picture(size),
                    caption.local_time,
                ))
                .ok_or(RenderError::MissingText)?;
            Ok(CaptionLayer {
                layers_below: caption.layers_below,
                visual: text_placement(texture, caption.style, size),
                texture,
            })
        })
        .collect()
}

/// Maps a text anchor into texture space, past a `margin` reserved for ink overflow so accents
/// and descenders never move the text.
#[allow(clippy::cast_precision_loss)]
fn box_anchor(anchor: f32, extent: u32, margin: u32) -> f32 {
    if margin == 0 {
        return anchor;
    }
    let inner = extent.saturating_sub(margin * 2) as f32;
    (margin as f32 + anchor * inner) / extent as f32
}

fn frame_runtime(frame: &FrameDesc, size: RenderSize) -> EffectRuntime {
    #[allow(clippy::cast_precision_loss)]
    let ratio = size.width as f32 / size.height as f32;
    EffectRuntime {
        time: frame.time,
        ratio,
        ..EffectRuntime::default()
    }
}

fn frame_graph<'a>(
    renderer: &dyn EffectCatalog,
    size: RenderSize,
    frame: &'a FrameDesc<'a>,
    available: &[(&'a ActiveClip<'a>, ClipFrames<'a>)],
    captions: &[CaptionLayer<'a>],
) -> Result<RenderGraph<'a>, RenderError> {
    let mut graph = RenderGraph::new(size);
    graph.set_blend_space(match frame.blend_space {
        alpha2_model::BlendSpace::LinearLight => alpha2_render::BlendSpace::LinearLight,
        alpha2_model::BlendSpace::SrgbEncoded => alpha2_render::BlendSpace::SrgbEncoded,
    });
    let mut nodes = HashMap::with_capacity(available.len());
    let mut inputs = Vec::with_capacity(available.len() + frame.transitions.len());
    // Each matte's coverage is drawn once however many clips it mattes.
    let mut coverages = HashMap::new();
    let runtime = frame_runtime(frame, size);
    for (clip, textures) in available {
        if clip.is_matte {
            continue;
        }
        let mut layer = clip_layer(renderer, &mut graph, clip, textures, frame.time, runtime)?;
        if let Some(matte) = clip.matte {
            layer = apply_matte(
                renderer,
                &mut graph,
                layer,
                matte,
                available,
                (frame.time, runtime),
                &mut coverages,
            )?;
        }
        nodes.insert(clip.clip_id, inputs.len());
        inputs.push(FrameInput {
            kind: FrameInputKind::Layer(layer),
            uses_backdrop: clip.blend_mode != ClipBlendMode::Normal,
        });
    }
    let background = background_color(frame.background);
    let mut suppressed = HashSet::new();
    for transition in &frame.transitions {
        let from = *nodes
            .get(transition.from_clip)
            .ok_or(RenderError::InvalidGraph)?;
        let to = *nodes
            .get(transition.to_clip)
            .ok_or(RenderError::InvalidGraph)?;
        let output = inputs.len();
        inputs.push(FrameInput {
            kind: FrameInputKind::Transition {
                from,
                to,
                transition,
            },
            uses_backdrop: inputs[from].uses_backdrop || inputs[to].uses_backdrop,
        });
        nodes.insert(transition.to_clip, output);
        suppressed.insert(transition.from_clip);
    }
    let mut layers = Vec::with_capacity(available.len() + captions.len());
    let mut captions = captions.iter().peekable();
    for (clip, textures) in available {
        // Captions on earlier tracks; undrawn clips do not move them.
        while let Some(caption) = captions.next_if(|caption| caption.layers_below <= textures.layer)
        {
            layers.push(caption_graph_layer(&mut graph, caption)?);
        }
        if clip.is_matte || suppressed.contains(clip.clip_id) {
            continue;
        }
        let index = *nodes.get(clip.clip_id).ok_or(RenderError::InvalidGraph)?;
        if let FrameInputKind::Layer(layer) = inputs[index].kind {
            layers.push(layer);
            continue;
        }
        // Normal transitions operate on isolated, transformed layers. Artistic modes need the
        // same backdrop on both sides: transition their composed scenes, then replace that
        // backdrop exactly once.
        let uses_backdrop = inputs[index].uses_backdrop;
        let backdrop = if uses_backdrop {
            layers.as_slice()
        } else {
            &[]
        };
        // The background is part of that backdrop, so blending matches outside transitions.
        let source = compile_frame_input(
            &mut graph,
            &inputs,
            index,
            backdrop,
            uses_backdrop.then_some(background),
            runtime,
            renderer,
        )?;
        if uses_backdrop {
            layers.clear();
        }
        layers.push(identity_layer(source));
    }
    for caption in captions {
        layers.push(caption_graph_layer(&mut graph, caption)?);
    }
    let output = graph
        .add_composite_over(layers, background)
        .map_err(GraphError::into_render_error)?;
    graph
        .set_output(output)
        .map_err(GraphError::into_render_error)?;
    graph
        .set_cache_intent(output, CacheIntent::Auto)
        .map_err(GraphError::into_render_error)?;
    Ok(graph)
}

const fn cache_intent(intent: alpha2_model::CacheIntent) -> CacheIntent {
    match intent {
        alpha2_model::CacheIntent::Auto => CacheIntent::Auto,
        alpha2_model::CacheIntent::Pin => CacheIntent::Pin,
        alpha2_model::CacheIntent::Bypass => CacheIntent::Bypass,
    }
}

/// `layer` kept where `matte` says, the matte clip drawn only into it.
fn apply_matte<'a, 'c>(
    renderer: &dyn EffectCatalog,
    graph: &mut RenderGraph<'a>,
    mut layer: GraphLayer,
    matte: &'c alpha2_model::Matte,
    available: &[(&'a ActiveClip<'a>, ClipFrames<'a>)],
    (time, runtime): (MediaTime, EffectRuntime),
    coverages: &mut HashMap<&'c alpha2_model::ClipId, RenderNodeId>,
) -> Result<GraphLayer, RenderError> {
    let source = available
        .iter()
        .find(|(candidate, _)| candidate.clip_id == &matte.source);
    match source {
        Some((source, textures)) => {
            let coverage = if let Some(coverage) = coverages.get(&matte.source) {
                *coverage
            } else {
                let matte_layer = clip_layer(renderer, graph, source, textures, time, runtime)?;
                let coverage = alone(graph, matte_layer)?;
                coverages.insert(&matte.source, coverage);
                coverage
            };
            layer = matted_layer(graph, layer, coverage, matte.mode, runtime)?;
        }
        // An inactive matte hides the clip (or shows it fully when inverted). A hidden clip keeps
        // its node so a transition can still name it.
        None if is_inverted(matte.mode) => {}
        None => layer.visual.opacity = 0.0,
    }
    Ok(layer)
}

fn background_color(background: Option<Rgb>) -> Color {
    background.map_or(Color::rgba(0.0, 0.0, 0.0, 0.0), |background| {
        Color::rgba(
            f64::from(background.red),
            f64::from(background.green),
            f64::from(background.blue),
            1.0,
        )
    })
}

struct FrameInput<'a> {
    kind: FrameInputKind<'a>,
    uses_backdrop: bool,
}

fn clip_layer<'a>(
    renderer: &dyn EffectCatalog,
    graph: &mut RenderGraph<'a>,
    clip: &'a ActiveClip<'a>,
    textures: &ClipFrames<'a>,
    time: MediaTime,
    runtime: EffectRuntime,
) -> Result<GraphLayer, RenderError> {
    let mut source = match clip.composition {
        None => clip_picture(renderer, graph, clip, textures, runtime)?,
        Some(composition) => plan_node(
            renderer,
            graph,
            composition,
            &textures.media,
            Some((clip, textures)),
            runtime,
        )?,
    };
    if !clip.masks.is_empty() {
        source = mask_node(graph, source, clip.masks, time, runtime, &clip.data)?;
    }
    // Cache the finished clip picture: layers above change often and everything below is in its
    // key. A bare source frame is already the host's, so only a bypass applies to it.
    let intent = cache_intent(clip.cache);
    if intent == CacheIntent::Bypass || graph.node_kind(source) != Some(RenderNodeKind::Source) {
        graph
            .set_cache_intent(source, intent)
            .map_err(GraphError::into_render_error)?;
    }
    if let Some(correction) = clip.correction {
        source = graph
            .add_composite(vec![GraphLayer {
                source,
                visual: correction_transform(correction),
                blend_mode: BlendMode::Normal,
            }])
            .map_err(GraphError::into_render_error)?;
    }
    Ok(GraphLayer {
        source,
        visual: layer_transform(clip.visual.ok_or(RenderError::InvalidVisual)?),
        blend_mode: blend_mode(clip.blend_mode),
    })
}

fn clip_picture<'a>(
    renderer: &dyn EffectCatalog,
    graph: &mut RenderGraph<'a>,
    clip: &'a ActiveClip<'a>,
    textures: &ClipFrames<'a>,
    runtime: EffectRuntime,
) -> Result<RenderNodeId, RenderError> {
    if let Some(source) = clip.source_composition {
        // A composition source's plan has no clip steps.
        return plan_node(
            renderer,
            graph,
            source,
            &textures.source_media,
            None,
            runtime,
        );
    }
    match (clip.generator, textures.source) {
        (Some(Generator::Text { .. }), _) => {
            textures.text.ok_or(RenderError::MissingText)?.add_to(graph)
        }
        (Some(Generator::Layout { .. }), _) => textures
            .layout
            .as_ref()
            .ok_or(RenderError::MissingText)?
            .add_to(graph),
        (Some(Generator::Shapes { .. }), _) => {
            let plan = clip
                .plan
                .and_then(GeneratorPlan::shapes)
                .ok_or(RenderError::InvalidGraph)?;
            Ok(shapes_node(graph, plan, clip.source_time))
        }
        (Some(generator), _) => Ok(generator_node(graph, generator, runtime)),
        (None, Some(frame)) => Ok(frame.add_to(graph)),
        (None, None) => Err(RenderError::InvalidGraph),
    }
}

/// A shapes generator's picture at picture time `time`, drawn as a vector scene at graph size.
fn shapes_node(graph: &mut RenderGraph<'_>, plan: &ShapesPlan, time: MediaTime) -> RenderNodeId {
    let size = graph.size();
    let canvas = alpha2_geometry::CanvasSize::new(size.width, size.height);
    let aspect = f64::from(size.width) / f64::from(size.height);
    let items = plan.evaluate(time, aspect);
    graph.add_vector(std::sync::Arc::new(alpha2_geometry::build_scene(
        &items, canvas,
    )))
}

/// A fill is timeless, so its node has the same cache key in every frame.
fn generator_node(
    graph: &mut RenderGraph<'_>,
    generator: &alpha2_model::Generator,
    runtime: EffectRuntime,
) -> RenderNodeId {
    graph.add_generator(
        EffectId::from_static(GENERATOR_EFFECT_ID, 1),
        crate::generated::generator_params(generator),
        timeless(runtime),
    )
}

/// Draws a compiled composition's steps in order, each sampled at the composition time plus its
/// offset. Clip steps draw `clip`'s picture.
fn plan_node<'a>(
    renderer: &dyn EffectCatalog,
    graph: &mut RenderGraph<'a>,
    composition: ActiveComposition<'a>,
    media: &[Option<StepFrame<'a>>],
    clip: Option<(&'a ActiveClip<'a>, &ClipFrames<'a>)>,
    runtime: EffectRuntime,
) -> Result<RenderNodeId, RenderError> {
    let plan = composition.plan();
    let time = composition.time();
    let mut built: Vec<RenderNodeId> = Vec::with_capacity(plan.steps.len());
    let mut picture = None;
    for (index, step) in plan.steps.iter().enumerate() {
        let at = step.time(time);
        let step_runtime = EffectRuntime {
            time: at,
            ..runtime
        };
        let node = match &step.op {
            // Every clip step draws the same picture.
            StepOp::Clip => {
                if let Some(node) = picture {
                    node
                } else {
                    let (clip, textures) = clip.ok_or(RenderError::InvalidGraph)?;
                    *picture.insert(clip_picture(renderer, graph, clip, textures, runtime)?)
                }
            }
            StepOp::Media { .. } => match media.get(index).and_then(Option::as_ref) {
                Some(StepFrame::Media(frame)) => frame.add_to(graph),
                _ => return Err(RenderError::InvalidGraph),
            },
            StepOp::Generator {
                generator: Generator::Text { .. },
                ..
            } => match media.get(index).and_then(Option::as_ref) {
                Some(StepFrame::Text(text)) => text.add_to(graph)?,
                _ => return Err(RenderError::MissingText),
            },
            StepOp::Generator {
                generator: Generator::Layout { .. },
                ..
            } => match media.get(index).and_then(Option::as_ref) {
                Some(StepFrame::Layout(layout)) => layout.add_to(graph)?,
                _ => return Err(RenderError::MissingText),
            },
            op => step_node(
                renderer,
                graph,
                op,
                &built,
                at,
                step_runtime,
                composition.step_data(index),
            )?,
        };
        built.push(node);
    }
    step_input(&built, plan.output)
}

// A node names at most as many inputs as an effect may declare.
const _: () = assert!(alpha2_model::MAX_EFFECT_INPUTS == alpha2_render::MAX_EFFECT_INPUTS);

fn step_input(built: &[RenderNodeId], step: usize) -> Result<RenderNodeId, RenderError> {
    built.get(step).copied().ok_or(RenderError::InvalidGraph)
}

#[allow(clippy::too_many_lines)]
fn step_node<'a>(
    renderer: &dyn EffectCatalog,
    graph: &mut RenderGraph<'a>,
    op: &'a StepOp,
    built: &[RenderNodeId],
    time: MediaTime,
    runtime: EffectRuntime,
    context: SampleContext<'a>,
) -> Result<RenderNodeId, RenderError> {
    let input = |step: usize| step_input(built, step);
    let node = match op {
        StepOp::Clip | StepOp::Media { .. } => return Err(RenderError::InvalidGraph),
        StepOp::Generator {
            generator: Generator::Shapes { .. },
            start,
            plan,
        } => {
            let plan = plan
                .as_deref()
                .and_then(GeneratorPlan::shapes)
                .ok_or(RenderError::InvalidGraph)?;
            let at = MediaTime(time.ticks().saturating_add(start.ticks()));
            shapes_node(graph, plan, at)
        }
        StepOp::Generator { generator, .. } => generator_node(graph, generator, runtime),
        StepOp::Effect {
            input: from,
            inputs,
            effect,
        } => {
            let id = project_effect_id(renderer, &effect.id, effect.version)?;
            let active = ActiveEffect::new(effect, time, context);
            let params = effect_params(renderer, &id, active);
            let runtime = effect_runtime(renderer, &id, runtime);
            let mut named = EffectInputs::new();
            for (name, step) in inputs {
                named.insert(name.as_str(), input(*step)?);
            }
            graph.add_effect_with_inputs(
                id,
                input(*from)?,
                named,
                params,
                effect_resources(active),
                runtime,
            )
        }
        .map_err(GraphError::into_render_error)?,
        StepOp::Transition {
            from,
            to,
            effect,
            progress,
        } => {
            let id = project_effect_id(renderer, &effect.id, effect.version)?;
            let active = ActiveEffect::new(effect, time, context);
            let params = effect_params(renderer, &id, active);
            let runtime = EffectRuntime {
                progress: sample_scalar(progress, time, &context).clamp(0.0, 1.0),
                ..effect_runtime(renderer, &id, runtime)
            };
            graph.add_transition_with_resources(
                id,
                input(*from)?,
                input(*to)?,
                params,
                effect_resources(active),
                runtime,
            )
        }
        .map_err(GraphError::into_render_error)?,
        StepOp::Transform {
            input: from,
            transform,
        } => graph
            .add_composite(vec![GraphLayer {
                source: input(*from)?,
                visual: layer_transform(sample_visual(
                    transform,
                    time,
                    graph_aspect(graph),
                    &context,
                )),
                blend_mode: BlendMode::Normal,
            }])
            .map_err(GraphError::into_render_error)?,
        StepOp::Merge { layers } => {
            let layers = layers
                .iter()
                .map(|layer| {
                    let mut placed = identity_layer(input(layer.input)?);
                    placed.visual.opacity =
                        sample_scalar(&layer.opacity, time, &context).clamp(0.0, 1.0);
                    placed.blend_mode = blend_mode(layer.blend_mode);
                    Ok(placed)
                })
                .collect::<Result<Vec<_>, RenderError>>()?;
            graph
                .add_composite(layers)
                .map_err(GraphError::into_render_error)?
        }
        StepOp::Matte {
            input: from,
            matte,
            mode,
        } => matte_node(graph, input(*from)?, input(*matte)?, *mode, runtime)?,
        StepOp::ShapeMask { input: from, masks } => {
            mask_node(graph, input(*from)?, masks, time, runtime, &context)?
        }
    };
    Ok(node)
}

/// Graph width over height, for motion along spatial curves.
fn graph_aspect(graph: &RenderGraph<'_>) -> f64 {
    let size = graph.size();
    f64::from(size.width) / f64::from(size.height)
}

/// `source` kept inside `masks` at `time`. Each outline is drawn as opaque white; feathering is
/// a Gaussian with deviation `0.3125 * feather` (of the shorter side), giving a 10-90 percent
/// ramp `0.8 * feather` wide. One pass combines the masks in order.
fn mask_node(
    graph: &mut RenderGraph<'_>,
    source: RenderNodeId,
    masks: &[ShapeMask],
    time: MediaTime,
    runtime: EffectRuntime,
    context: &SampleContext<'_>,
) -> Result<RenderNodeId, RenderError> {
    let size = graph.size();
    let canvas = alpha2_geometry::CanvasSize::new(size.width, size.height);
    let aspect = graph_aspect(graph);
    let unit = size.width.min(size.height);
    let white = alpha2_model::Paint::Solid {
        color: alpha2_model::Rgba::new(1.0, 1.0, 1.0, 1.0),
    };
    let shown = &masks[..masks.len().min(MAX_SHAPE_MASKS)];
    if shown.is_empty() {
        return Ok(source);
    }
    let mut coverages = Vec::with_capacity(shown.len());
    for mask in shown {
        let outline = alpha2_eval::evaluate_geometry(&mask.shape, time, aspect, context);
        let items = [
            alpha2_geometry::EvaluatedItem::Geometry(outline),
            alpha2_geometry::EvaluatedItem::Fill {
                paint: white.clone(),
                opacity: 1.0,
                rule: mask.fill_rule,
            },
        ];
        let coverage = graph.add_vector(std::sync::Arc::new(alpha2_geometry::build_scene(
            &items, canvas,
        )));
        #[allow(clippy::cast_precision_loss)]
        let sigma = 0.3125 * sample_scalar(&mask.feather, time, context).max(0.0) * unit as f32;
        let coverage = graph
            .add_gaussian_pixels(coverage, sigma, timeless(runtime))
            .map_err(GraphError::into_render_error)?;
        coverages.push((coverage, mask.invert, mask.mode));
    }
    coverage_mask(graph, source, &coverages)
}

/// `source` kept by the alpha of up to [`MAX_SHAPE_MASKS`] `coverages`, combined in one pass.
pub(crate) fn coverage_mask(
    graph: &mut RenderGraph<'_>,
    source: RenderNodeId,
    coverages: &[(RenderNodeId, bool, MaskMode)],
) -> Result<RenderNodeId, RenderError> {
    if coverages.is_empty() || coverages.len() > MAX_SHAPE_MASKS {
        return Err(RenderError::InvalidGraph);
    }
    let size = graph.size();
    #[allow(clippy::cast_precision_loss)]
    let runtime = EffectRuntime {
        ratio: size.width as f32 / size.height as f32,
        ..EffectRuntime::default()
    };
    let mut inputs = EffectInputs::new();
    let mut params = EffectParams::with_capacity(coverages.len() * 2);
    for (&(coverage, invert, mode), names) in coverages.iter().zip(MASK_COMBINE_NAMES) {
        inputs.insert(names.input, coverage);
        params.insert(names.invert, ParamValue::Bool(invert));
        params.insert(
            names.mode,
            ParamValue::Enum(match mode {
                MaskMode::Add => 0,
                MaskMode::Subtract => 1,
                MaskMode::Intersect => 2,
            }),
        );
    }
    graph
        .add_effect_with_inputs(
            EffectId::from_static(MASK_COMBINE_EFFECT_IDS[coverages.len() - 1], 1),
            source,
            inputs,
            params,
            EffectResources::new(),
            runtime,
        )
        .map_err(GraphError::into_render_error)
}

const fn is_inverted(mode: MatteMode) -> bool {
    matches!(mode, MatteMode::AlphaInverted | MatteMode::LumaInverted)
}

/// `layer` kept by `matte`. Both are drawn onto the canvas first, so the matte applies where
/// they land regardless of transforms.
fn matted_layer(
    graph: &mut RenderGraph<'_>,
    layer: GraphLayer,
    coverage: RenderNodeId,
    mode: MatteMode,
    runtime: EffectRuntime,
) -> Result<GraphLayer, RenderError> {
    let picture = alone(graph, layer)?;
    let matted = matte_node(graph, picture, coverage, mode, runtime)?;
    Ok(GraphLayer {
        blend_mode: layer.blend_mode,
        ..identity_layer(matted)
    })
}

fn alone(graph: &mut RenderGraph<'_>, layer: GraphLayer) -> Result<RenderNodeId, RenderError> {
    graph
        .add_composite(vec![GraphLayer {
            blend_mode: BlendMode::Normal,
            ..layer
        }])
        .map_err(GraphError::into_render_error)
}

/// Drops the frame time from `runtime` when `effect` cannot read it
/// ([`Renderer::effect_reads_time`]), so its cache key holds across frames.
fn effect_runtime(
    renderer: &dyn EffectCatalog,
    effect: &EffectId,
    runtime: EffectRuntime,
) -> EffectRuntime {
    if renderer.effect_reads_time(effect) {
        runtime
    } else {
        EffectRuntime {
            time: MediaTime::ZERO,
            ..runtime
        }
    }
}

/// `runtime` without the frame time, for timeless engine passes so their cache key holds.
fn timeless(runtime: EffectRuntime) -> EffectRuntime {
    EffectRuntime {
        ratio: runtime.ratio,
        ..EffectRuntime::default()
    }
}

fn matte_node(
    graph: &mut RenderGraph<'_>,
    picture: RenderNodeId,
    coverage: RenderNodeId,
    mode: MatteMode,
    runtime: EffectRuntime,
) -> Result<RenderNodeId, RenderError> {
    graph
        .add_transition_with_resources(
            EffectId::from_static(TRACK_MATTE_EFFECT_ID, 1),
            picture,
            coverage,
            EffectParams::new()
                .with(
                    "luma",
                    ParamValue::Bool(matches!(mode, MatteMode::Luma | MatteMode::LumaInverted)),
                )
                .with("inverted", ParamValue::Bool(is_inverted(mode))),
            EffectResources::new(),
            // The matte ignores progress and time; an interior progress runs its body.
            EffectRuntime {
                progress: 0.5,
                ..timeless(runtime)
            },
        )
        .map_err(GraphError::into_render_error)
}

#[derive(Copy, Clone)]
enum FrameInputKind<'a> {
    Layer(GraphLayer),
    Transition {
        from: usize,
        to: usize,
        transition: &'a ActiveTransition<'a>,
    },
}

fn compile_frame_input<'a>(
    graph: &mut RenderGraph<'a>,
    inputs: &[FrameInput<'a>],
    index: usize,
    backdrop: &[GraphLayer],
    background: Option<Color>,
    runtime: EffectRuntime,
    renderer: &dyn EffectCatalog,
) -> Result<RenderNodeId, RenderError> {
    if index >= inputs.len() {
        return Err(RenderError::InvalidGraph);
    }
    let mut compiled = vec![None; inputs.len()];
    let mut pending = vec![(index, false)];
    while let Some((current, expanded)) = pending.pop() {
        if compiled[current].is_some() {
            continue;
        }
        match inputs[current].kind {
            FrameInputKind::Layer(layer) => {
                let mut layers = Vec::with_capacity(backdrop.len() + 1);
                layers.extend_from_slice(backdrop);
                layers.push(layer);
                let composite = match background {
                    Some(background) => graph.add_composite_over(layers, background),
                    None => graph.add_composite(layers),
                };
                compiled[current] = Some(composite.map_err(GraphError::into_render_error)?);
            }
            FrameInputKind::Transition {
                from,
                to,
                transition,
            } => {
                if from >= inputs.len() || to >= inputs.len() {
                    return Err(RenderError::InvalidGraph);
                }
                if !expanded {
                    pending.push((current, true));
                    if compiled[to].is_none() {
                        pending.push((to, false));
                    }
                    if compiled[from].is_none() {
                        pending.push((from, false));
                    }
                    continue;
                }
                let from = compiled[from].ok_or(RenderError::InvalidGraph)?;
                let to = compiled[to].ok_or(RenderError::InvalidGraph)?;
                let id = project_effect_id(
                    renderer,
                    transition.effect.id(),
                    transition.effect.version(),
                )?;
                let params = effect_params(renderer, &id, transition.effect);
                let runtime = EffectRuntime {
                    progress: transition.progress,
                    ..effect_runtime(renderer, &id, runtime)
                };
                compiled[current] = Some(
                    graph
                        .add_transition_with_resources(
                            id,
                            from,
                            to,
                            params,
                            effect_resources(transition.effect),
                            runtime,
                        )
                        .map_err(GraphError::into_render_error)?,
                );
            }
        }
    }
    compiled[index].ok_or(RenderError::InvalidGraph)
}

fn identity_layer(source: RenderNodeId) -> GraphLayer {
    GraphLayer {
        source,
        visual: LayerTransform {
            position: [0.5, 0.5],
            anchor: [0.5, 0.5],
            scale: [1.0, 1.0],
            rotation_degrees: 0.0,
            opacity: 1.0,
        },
        blend_mode: BlendMode::Normal,
    }
}

/// A correction as a layer transform: turned and scaled about the layer's centre, then moved by
/// its translation in fractions of the displayed picture, as `Correction::apply` maps points.
fn correction_transform(correction: alpha2_eval::Correction) -> LayerTransform {
    LayerTransform {
        position: [
            0.5 + correction.translate.x * correction.picture.x,
            0.5 + correction.translate.y * correction.picture.y,
        ],
        anchor: [0.5, 0.5],
        scale: [correction.scale, correction.scale],
        rotation_degrees: correction.rotation_degrees,
        opacity: 1.0,
    }
}

const fn layer_transform(visual: EvaluatedVisual) -> LayerTransform {
    LayerTransform {
        position: [visual.position.x, visual.position.y],
        anchor: [visual.anchor.x, visual.anchor.y],
        scale: [visual.scale.x, visual.scale.y],
        rotation_degrees: visual.rotation_degrees,
        opacity: visual.opacity,
    }
}

/// The invocation's parameters at the frame's time.
///
/// Interpolated values are clamped to the parameter range since overshoot is intended motion.
/// Authored values pass as written, and the renderer rejects out-of-range ones.
fn effect_params<'a>(
    renderer: &dyn EffectCatalog,
    id: &EffectId,
    effect: ActiveEffect<'a>,
) -> EffectParams<'a> {
    let mut params = EffectParams::with_capacity(effect.authored().params.len());
    let definition = renderer.effect_definition(id);
    for (name, sample) in effect.params() {
        let mut value = param_value(sample.value);
        let spec = definition
            .as_ref()
            .and_then(|definition| definition.params.iter().find(|spec| spec.name == name));
        if let Some(spec) = spec {
            // A scalar data set feeding a Vec2 parameter sets both components.
            if let (ParamType::Vec2, ParamValue::Float(scalar)) = (spec.param_type, &value) {
                value = ParamValue::Vec2([*scalar, *scalar]);
            }
            if sample.interpolated {
                value = clamp_to_range(spec, value);
            }
        }
        params.insert(name, value);
    }
    params
}

fn project_effect_id(
    renderer: &dyn EffectCatalog,
    id: &str,
    version: u32,
) -> Result<EffectId, RenderError> {
    renderer
        .project_effect_id(id, version)
        .ok_or(RenderError::UnknownEffect)
}

fn clamp_to_range(spec: &ParamSpec, value: ParamValue) -> ParamValue {
    let clamp = |value: f32| {
        let value = spec.min.map_or(value, |minimum| value.max(minimum));
        spec.max.map_or(value, |maximum| value.min(maximum))
    };
    match value {
        ParamValue::Float(value) => ParamValue::Float(clamp(value)),
        ParamValue::Vec2([x, y]) => ParamValue::Vec2([clamp(x), clamp(y)]),
        ParamValue::Vec4(values) => ParamValue::Vec4(values.map(clamp)),
        other => other,
    }
}

fn effect_resources(effect: ActiveEffect<'_>) -> EffectResources {
    effect
        .resources()
        .fold(EffectResources::new(), |resources, (name, id)| {
            resources.with(name, id.as_str())
        })
}

const fn param_value(value: EffectValue) -> ParamValue {
    match value {
        EffectValue::Float(value) => ParamValue::Float(value),
        EffectValue::Vec2(value) => ParamValue::Vec2(value),
        EffectValue::Vec4(value) => ParamValue::Vec4(value),
        EffectValue::Color(value) => ParamValue::Color(value),
        EffectValue::Enum(value) => ParamValue::Enum(value),
        EffectValue::Bool(value) => ParamValue::Bool(value),
    }
}

const fn blend_mode(mode: ClipBlendMode) -> BlendMode {
    match mode {
        ClipBlendMode::Normal => BlendMode::Normal,
        ClipBlendMode::Multiply => BlendMode::Multiply,
        ClipBlendMode::Screen => BlendMode::Screen,
        ClipBlendMode::Overlay => BlendMode::Overlay,
        ClipBlendMode::Darken => BlendMode::Darken,
        ClipBlendMode::Lighten => BlendMode::Lighten,
        ClipBlendMode::ColorDodge => BlendMode::ColorDodge,
        ClipBlendMode::ColorBurn => BlendMode::ColorBurn,
        ClipBlendMode::HardLight => BlendMode::HardLight,
        ClipBlendMode::SoftLight => BlendMode::SoftLight,
        ClipBlendMode::Difference => BlendMode::Difference,
        ClipBlendMode::Exclusion => BlendMode::Exclusion,
        ClipBlendMode::Hue => BlendMode::Hue,
        ClipBlendMode::Saturation => BlendMode::Saturation,
        ClipBlendMode::Color => BlendMode::Color,
        ClipBlendMode::Luminosity => BlendMode::Luminosity,
        ClipBlendMode::PlusLighter => BlendMode::PlusLighter,
    }
}

#[cfg(test)]
mod tests;
