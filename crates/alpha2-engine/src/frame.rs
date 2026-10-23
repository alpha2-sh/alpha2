//! Rendering of evaluated timeline frames.
//!
//! The renderer knows render graphs, not timelines. This module is where an
//! evaluated [`FrameDesc`] becomes one: it checks that every clip's source
//! frame has arrived, compiles clips, effect chains, and transitions into a
//! [`RenderGraph`], and hands that graph to the renderer's submission paths.

use alpha2_eval::{
    ActiveClip, ActiveComposition, ActiveEffect, ActiveTransition, EvaluatedVisual, FrameDesc,
    StepOp, sample_scalar, sample_vec2, sample_visual,
};
use alpha2_media::{SourceId, YuvEncoding};
use alpha2_model::{
    ClipBlendMode, EffectValue, Generator, MaskMode, MaskShape, MatteMode, Rgb, ShapeMask,
    TextStyle,
};
use alpha2_render::{
    BlendMode, CacheIntent, Color, EffectId, EffectInputs, EffectParams, EffectResources,
    EffectRuntime, FrameSubmitTimings, GENERATOR_EFFECT_ID, GraphError, GraphLayer, LayerTransform,
    MAX_SHAPE_MASKS, Nv12PlaneTargets, ParamSpec, ParamValue, PendingSourceWork, RenderCacheKey,
    RenderError, RenderGraph, RenderNodeId, RenderNodeKind, RenderSize, RenderedTexture, Renderer,
    SHAPE_MASK_EFFECT_ID, SHAPE_MASK_PARAMS, SourceView, TRACK_MATTE_EFFECT_ID, Yuv420pReadback,
};
use alpha2_time::MediaTime;

use crate::TextTexture;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use web_time::Instant;

/// Supplies decoded source frames as GPU textures to the renderer.
///
/// Implementations own decode caches and the lifetime of their textures, and
/// lend them as [`SourceView`]s. The pure model and evaluator crates never
/// observe this platform-specific type. Textures must store premultiplied,
/// sRGB-encoded RGBA in a non-sRGB GPU format, as returned by
/// `Renderer::upload_rgba8` or `Renderer::render_yuv420p`.
pub trait FrameProvider {
    /// Returns a texture for `source` at the requested source timestamp.
    ///
    /// The texture is the frame presented at `time`: the one whose
    /// presentation interval contains it, the last frame whose timestamp is
    /// at or before `time` plus the stream's presentation slack, as
    /// [`alpha2_media::Decoder::seek`] resolves a time. Before the stream's
    /// first frame that frame is shown; past the last, the last. A host that
    /// looks frames up by the requested time stores each under every time it
    /// was asked for.
    ///
    /// `None` means the frame has not arrived yet; real-time runners may hold
    /// their previous output while deterministic exporters wait and retry.
    fn texture_for(&self, source: &SourceId, time: MediaTime) -> Option<SourceView<'_>>;

    /// A key for the texture [`Self::texture_for`] returns: the host's
    /// promise that the texture's pixels depend on nothing the key leaves
    /// out: the source's content, the frame, and how it was decoded,
    /// converted, and presented. Pictures drawn from keyed sources can be
    /// kept and reused (see [`alpha2_render::Renderer::set_result_cache_budget`]);
    /// the default, no key, keeps nothing drawn from the source.
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

    /// Returns the text [`crate::text_key`] names, a caption or a text
    /// generator prepared by the host's [`crate::TextTextures`]. The default
    /// holds none: a frame that shows text then fails with
    /// [`RenderError::MissingText`].
    fn text(&self, _key: u64) -> Option<TextTexture<'_>> {
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
pub struct FrameRender {
    /// Availability information collected before any GPU work is submitted.
    pub report: RenderReport,
    /// Rendered output, absent when one or more source frames are unavailable.
    pub texture: Option<RenderedTexture>,
}

impl FrameRender {
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
    /// Presentation work behind the graph's source textures that has not
    /// been submitted yet, for the submission to fold in.
    pub pending: Vec<&'a PendingSourceWork>,
}

/// Timeline-frame rendering on top of a [`Renderer`].
///
/// Implemented for [`Renderer`]; import it to call these methods on one.
pub trait FrameRenderer {
    /// Resolves the textures required by an evaluated frame.
    ///
    /// This performs no fallback rendering: it is the common availability pass
    /// used by a real-time player (which may reuse an older image) and a
    /// deterministic exporter (which waits and retries).
    #[must_use]
    fn inspect_frame(&self, frame: &FrameDesc, frames: &dyn FrameProvider) -> RenderReport;

    /// Compiles one evaluator result into the render graph every render
    /// method below submits, without encoding GPU work.
    ///
    /// The graph borrows the frame's effect and parameter names and the
    /// provider's textures; an incomplete source set returns its report and
    /// no graph.
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

    /// Renders one pure evaluator result using textures supplied by the host.
    ///
    /// Source availability is checked before encoding GPU work. An incomplete
    /// attempt returns its missing-source report with no partially rendered
    /// texture, allowing real-time and deterministic runners to apply their
    /// own hold or wait policy.
    ///
    /// # Errors
    /// Returns [`RenderError`] when a visual or render target is invalid.
    fn render_frame(
        &self,
        size: RenderSize,
        frame: &FrameDesc,
        frames: &dyn FrameProvider,
    ) -> Result<FrameRender, RenderError>;

    /// Renders one evaluator result, converts it to YUV420, and submits both
    /// workloads in one command buffer.
    ///
    /// An incomplete source set returns without encoding or submitting work.
    /// The output size must be even for chroma subsampling.
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
    /// This produces exactly what [`FrameRenderer::render_frame_yuv420p`]
    /// produces; the only difference is that it reads a clock between stages
    /// and writes the spans to `timings`. An incomplete source set leaves
    /// `timings` holding only the stages that ran.
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
    /// This is [`FrameRenderer::render_frame_yuv420p_timed`] with the staging
    /// buffer removed: the same sources, the same graph and the same
    /// conversion, ending in two textures a hardware encoder can be fed from
    /// instead of a mapped buffer. An incomplete source set submits nothing.
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

impl FrameRenderer for Renderer {
    fn inspect_frame(&self, frame: &FrameDesc, frames: &dyn FrameProvider) -> RenderReport {
        available_textures(frame, frames).0
    }

    fn build_frame_graph<'a>(
        &self,
        size: RenderSize,
        frame: &'a FrameDesc<'a>,
        frames: &'a dyn FrameProvider,
    ) -> Result<FrameGraph<'a>, RenderError> {
        validate_project_effects(self, frame)?;
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
        let graph = frame_graph(self, size, frame, &available, &captions)?;
        Ok(FrameGraph {
            report,
            graph: Some(graph),
            pending: pending_source_work(&available, frames),
        })
    }

    fn render_frame(
        &self,
        size: RenderSize,
        frame: &FrameDesc,
        frames: &dyn FrameProvider,
    ) -> Result<FrameRender, RenderError> {
        let FrameGraph {
            report,
            graph,
            pending,
        } = self.build_frame_graph(size, frame, frames)?;
        let texture = match graph {
            Some(graph) => Some(self.render_graph(&graph, &pending)?),
            None => None,
        };
        Ok(FrameRender { report, texture })
    }

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

    /// Returns the span since the previous lap and starts a new one.
    fn lap(&mut self) -> Duration {
        let Some(mark) = self.0 else {
            return Duration::ZERO;
        };
        let now = Instant::now();
        self.0 = Some(now);
        now.duration_since(mark)
    }
}

fn validate_project_effects(renderer: &Renderer, frame: &FrameDesc) -> Result<(), RenderError> {
    require_effects(renderer, frame_effects(frame)).map_err(|_| RenderError::UnknownEffect)
}

/// Every effect `frame` draws, by id and version: its compositions' steps
/// and its transitions.
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

/// Checks that `renderer` draws every effect `frame` uses, naming the first
/// it does not. [`FrameRenderer`]'s methods check the same, but answer with
/// a [`RenderError`], which cannot say which effect.
///
/// # Errors
/// Returns [`crate::EngineError::UnregisteredEffect`].
pub(crate) fn check_frame_effects(
    renderer: &Renderer,
    frame: &FrameDesc,
) -> Result<(), crate::EngineError> {
    require_effects(renderer, frame_effects(frame))
}

/// The first of `effects` `renderer` does not draw, as an error.
pub(crate) fn require_effects<'a>(
    renderer: &Renderer,
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

/// Collects the pending presentation work behind each available clip's
/// textures, in composition order, for the renderer to fold into its
/// submission.
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

/// A host-supplied frame and, when the host vouches for it, its key.
#[derive(Copy, Clone, Debug)]
struct Frame<'t> {
    view: SourceView<'t>,
    key: Option<RenderCacheKey>,
}

impl<'t> Frame<'t> {
    /// The frame as a graph source: keyed when the host gave a key.
    fn add_to(self, graph: &mut RenderGraph<'t>) -> RenderNodeId {
        match self.key {
            Some(key) => graph.add_source_with_key(self.view, key),
            None => graph.add_source(self.view),
        }
    }
}

/// Text a generator draws: its prepared texture, placed on the
/// generator's transparent, canvas-sized picture.
#[derive(Copy, Clone, Debug)]
struct PlacedText<'t> {
    texture: TextTexture<'t>,
    visual: LayerTransform,
}

impl<'t> PlacedText<'t> {
    /// The generator's picture: the texture, keyed as its store keys it,
    /// over transparency.
    fn add_to(self, graph: &mut RenderGraph<'t>) -> Result<RenderNodeId, RenderError> {
        let source = match self.texture.cache_key {
            Some(key) => graph.add_source_with_key(self.texture.texture, key),
            None => graph.add_source(self.texture.texture),
        };
        graph
            .add_composite(vec![GraphLayer {
                source,
                visual: self.visual,
                blend_mode: BlendMode::Normal,
            }])
            .map_err(GraphError::into_render_error)
    }
}

/// What one composition step draws from outside the graph.
#[derive(Copy, Clone, Debug)]
enum StepFrame<'t> {
    /// A media step's host-supplied frame.
    Media(Frame<'t>),
    /// A text generator step's text.
    Text(PlacedText<'t>),
}

/// The host-supplied frames one visual clip draws.
#[derive(Clone, Debug, Default)]
struct ClipFrames<'t> {
    /// The clip's index in [`FrameDesc::visual`], its place in track order,
    /// which a caption's [`alpha2_eval::ActiveCaption::layers_below`] counts
    /// in: the clips drawn are a subset, so their positions among
    /// themselves do not say which tracks a caption lies between.
    layer: usize,
    /// The clip's own source frame; `None` when the engine draws the source
    /// or the clip's composition does not show it.
    source: Option<Frame<'t>>,
    /// The text of the clip's text generator source, once prepared.
    text: Option<PlacedText<'t>>,
    /// Each media step's frame and each text generator step's text, by step
    /// index; `None` for other steps.
    media: Vec<Option<StepFrame<'t>>>,
    /// The same for the clip's composition source.
    source_media: Vec<Option<StepFrame<'t>>>,
}

/// Each visual clip with the source frames it draws. A frame the host has
/// not supplied is reported missing, and its clip left out.
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

/// Each available clip's text generators with their prepared text, the
/// clip's own source and its compositions' steps. Text `frames` does not
/// hold is left out, for the graph to report.
fn attach_texts<'t>(
    available: &mut [(&ActiveClip<'_>, ClipFrames<'t>)],
    frames: &'t dyn FrameProvider,
    size: RenderSize,
) {
    let lookup = |generator: &Generator| match generator {
        Generator::Text { text, style } => {
            frames
                .text(crate::text_key(text, style, size))
                .map(|texture| PlacedText {
                    texture,
                    visual: text_placement(texture, style, size),
                })
        }
        _ => None,
    };
    for (clip, textures) in available {
        textures.text = clip.generator.and_then(lookup);
        for (composition, media) in [
            (clip.composition, &mut textures.media),
            (clip.source_composition, &mut textures.source_media),
        ] {
            let Some(composition) = composition else {
                continue;
            };
            for (step, slot) in composition.plan().steps.iter().zip(media.iter_mut()) {
                if let StepOp::Generator(generator) = &step.op {
                    *slot = lookup(generator).map(StepFrame::Text);
                }
            }
        }
    }
}

/// Where prepared text lies on the canvas: its texture keeps its pixel
/// size, and the anchor point of the text with its decorations lies at its
/// position.
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
    /// How many of the frame's visual clips, drawn or not, lie under it.
    layers_below: usize,
    texture: TextTexture<'a>,
    visual: LayerTransform,
}

/// A caption's texture as a layer of `graph`: keyed as its store keys it,
/// so a frame showing the same caption over the same picture is found again.
fn caption_graph_layer<'a>(graph: &mut RenderGraph<'a>, caption: &CaptionLayer<'a>) -> GraphLayer {
    let source = match caption.texture.cache_key {
        Some(key) => graph.add_source_with_key(caption.texture.texture, key),
        None => graph.add_source(caption.texture.texture),
    };
    GraphLayer {
        source,
        visual: caption.visual,
        blend_mode: BlendMode::Normal,
    }
}

/// Each caption `frame` shows, from `frames`, placed on the canvas
/// ([`text_placement`]).
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
                .text(crate::text_key(caption.text, caption.style, size))
                .ok_or(RenderError::MissingText)?;
            Ok(CaptionLayer {
                layers_below: caption.layers_below,
                visual: text_placement(texture, caption.style, size),
                texture,
            })
        })
        .collect()
}

/// `anchor`, a point of a text with its decorations, as a point of its
/// texture, which has `margin` pixels around that for ink past it: so an
/// accent or a descender never moves the text.
#[allow(clippy::cast_precision_loss)]
fn box_anchor(anchor: f32, extent: u32, margin: u32) -> f32 {
    if margin == 0 {
        return anchor;
    }
    let inner = extent.saturating_sub(margin * 2) as f32;
    (margin as f32 + anchor * inner) / extent as f32
}

/// The runtime a frame's passes see: its time and the canvas's aspect ratio.
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
    renderer: &Renderer,
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
    // Each matte's coverage, drawn once however many clips it mattes.
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
        // Captions on tracks before this clip's, by track order: a clip before it that is not
        // drawn (a matte nothing names) does not move them.
        while let Some(caption) = captions.next_if(|caption| caption.layers_below <= textures.layer)
        {
            layers.push(caption_graph_layer(&mut graph, caption));
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
        // The background is the bottom layer of that backdrop too, so an artistic mode blends
        // against the same color inside a transition as outside one.
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
        layers.push(caption_graph_layer(&mut graph, caption));
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
    renderer: &Renderer,
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
        // An inactive matte covers nothing: the clip is hidden, or wholly visible when the
        // matte is inverted. A hidden clip keeps its node so a transition can still name it.
        None if is_inverted(matte.mode) => {}
        None => layer.visual.opacity = 0.0,
    }
    Ok(layer)
}

/// The project background as an opaque, sRGB-encoded storage value.
fn background_color(background: Rgb) -> Color {
    Color::rgba(
        f64::from(background.red),
        f64::from(background.green),
        f64::from(background.blue),
        1.0,
    )
}

struct FrameInput<'a> {
    kind: FrameInputKind<'a>,
    uses_backdrop: bool,
}

fn clip_layer<'a>(
    renderer: &Renderer,
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
        source = graph
            .add_effect_with_resources(
                EffectId::from_static(SHAPE_MASK_EFFECT_ID, 1),
                source,
                mask_params(clip.masks, time),
                EffectResources::new(),
                timeless(runtime),
            )
            .map_err(GraphError::into_render_error)?;
    }
    // The clip's finished picture is where reuse pays: everything above it
    // changes with the layer stack, everything below it is in its key. A bare
    // source frame is already the host's; only a bypass on it matters.
    let intent = cache_intent(clip.cache);
    if intent == CacheIntent::Bypass || graph.node_kind(source) != Some(RenderNodeKind::Source) {
        graph
            .set_cache_intent(source, intent)
            .map_err(GraphError::into_render_error)?;
    }
    Ok(GraphLayer {
        source,
        visual: layer_transform(clip.visual.ok_or(RenderError::InvalidVisual)?),
        blend_mode: blend_mode(clip.blend_mode),
    })
}

/// The clip's own picture: its composition source, its generated source,
/// or its source frame.
fn clip_picture<'a>(
    renderer: &Renderer,
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
        (Some(generator), _) => Ok(generator_node(graph, generator, runtime)),
        (None, Some(frame)) => Ok(frame.add_to(graph)),
        (None, None) => Err(RenderError::InvalidGraph),
    }
}

/// A generated picture does not change over time, so its node has the same
/// cache key in every frame.
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

/// The node drawing a compiled composition's output: each step in order,
/// with its settings sampled at the composition time plus its offset. Its
/// clip steps draw `clip`'s picture; a plan without them takes none.
fn plan_node<'a>(
    renderer: &Renderer,
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
            StepOp::Media { .. } => match media.get(index).copied().flatten() {
                Some(StepFrame::Media(frame)) => frame.add_to(graph),
                _ => return Err(RenderError::InvalidGraph),
            },
            StepOp::Generator(Generator::Text { .. }) => {
                match media.get(index).copied().flatten() {
                    Some(StepFrame::Text(text)) => text.add_to(graph)?,
                    _ => return Err(RenderError::MissingText),
                }
            }
            op => step_node(renderer, graph, op, &built, at, step_runtime)?,
        };
        built.push(node);
    }
    step_input(&built, plan.output)
}

// A node names at most as many inputs as an effect may declare.
const _: () = assert!(alpha2_model::MAX_EFFECT_INPUTS == alpha2_render::MAX_EFFECT_INPUTS);

/// The node an earlier step drew.
fn step_input(built: &[RenderNodeId], step: usize) -> Result<RenderNodeId, RenderError> {
    built.get(step).copied().ok_or(RenderError::InvalidGraph)
}

/// The node for a step that draws from its inputs, or from nothing.
fn step_node<'a>(
    renderer: &Renderer,
    graph: &mut RenderGraph<'a>,
    op: &'a StepOp,
    built: &[RenderNodeId],
    time: MediaTime,
    runtime: EffectRuntime,
) -> Result<RenderNodeId, RenderError> {
    let input = |step: usize| step_input(built, step);
    let node = match op {
        StepOp::Clip | StepOp::Media { .. } => return Err(RenderError::InvalidGraph),
        StepOp::Generator(generator) => generator_node(graph, generator, runtime),
        StepOp::Effect {
            input: from,
            inputs,
            effect,
        } => {
            let id = project_effect_id(renderer, &effect.id, effect.version)?;
            let active = ActiveEffect::new(effect, time);
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
            let active = ActiveEffect::new(effect, time);
            let params = effect_params(renderer, &id, active);
            let runtime = EffectRuntime {
                progress: sample_scalar(progress, time).clamp(0.0, 1.0),
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
                visual: layer_transform(sample_visual(transform, time)),
                blend_mode: BlendMode::Normal,
            }])
            .map_err(GraphError::into_render_error)?,
        StepOp::Merge { layers } => {
            let layers = layers
                .iter()
                .map(|layer| {
                    let mut placed = identity_layer(input(layer.input)?);
                    placed.visual.opacity = sample_scalar(&layer.opacity, time).clamp(0.0, 1.0);
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
        StepOp::ShapeMask { input: from, masks } => graph
            .add_effect_with_resources(
                EffectId::from_static(SHAPE_MASK_EFFECT_ID, 1),
                input(*from)?,
                mask_params(masks, time),
                EffectResources::new(),
                timeless(runtime),
            )
            .map_err(GraphError::into_render_error)?,
    };
    Ok(node)
}

/// A clip's shape masks sampled at `time`, as the shape-mask pass's
/// parameters. An overshooting curve cannot make a size, radius, or feather
/// negative here.
fn mask_params(masks: &[ShapeMask], time: MediaTime) -> EffectParams<'static> {
    let shown = masks.len().min(MAX_SHAPE_MASKS);
    let mut params = EffectParams::with_capacity(1 + shown * 8);
    #[allow(clippy::cast_precision_loss)]
    params.insert("count", ParamValue::Float(masks.len() as f32));
    for (mask, names) in masks.iter().zip(SHAPE_MASK_PARAMS) {
        let center = sample_vec2(&mask.center, time);
        let size = sample_vec2(&mask.size, time);
        let (ellipse, radius) = match &mask.shape {
            MaskShape::Ellipse => (true, 0.0),
            MaskShape::Rectangle { corner_radius } => (false, sample_scalar(corner_radius, time)),
        };
        params.insert(names.center, ParamValue::Vec2([center.x, center.y]));
        params.insert(
            names.size,
            ParamValue::Vec2([size.x.max(0.0), size.y.max(0.0)]),
        );
        params.insert(
            names.rotation,
            ParamValue::Float(sample_scalar(&mask.rotation_degrees, time).to_radians()),
        );
        params.insert(names.radius, ParamValue::Float(radius.max(0.0)));
        params.insert(
            names.feather,
            ParamValue::Float(sample_scalar(&mask.feather, time).max(0.0)),
        );
        params.insert(names.ellipse, ParamValue::Bool(ellipse));
        params.insert(names.invert, ParamValue::Bool(mask.invert));
        params.insert(
            names.mode,
            ParamValue::Enum(match mask.mode {
                MaskMode::Add => 0,
                MaskMode::Subtract => 1,
                MaskMode::Intersect => 2,
            }),
        );
    }
    params
}

const fn is_inverted(mode: MatteMode) -> bool {
    matches!(mode, MatteMode::AlphaInverted | MatteMode::LumaInverted)
}

/// `layer` kept where `matte`'s picture says, as an untransformed layer with
/// `layer`'s blend mode. Both are drawn alone onto the canvas first, so the
/// matte applies where the two land, whatever their transforms.
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

/// `layer` drawn alone, as it covers the canvas.
fn alone(graph: &mut RenderGraph<'_>, layer: GraphLayer) -> Result<RenderNodeId, RenderError> {
    graph
        .add_composite(vec![GraphLayer {
            blend_mode: BlendMode::Normal,
            ..layer
        }])
        .map_err(GraphError::into_render_error)
}

/// The runtime `effect` is drawn with: `runtime`, without the frame's time
/// when the effect cannot read it ([`Renderer::effect_reads_time`]). Its
/// result then has the same cache key in every frame that draws it from the
/// same inputs and parameters, and is found again there; an effect that
/// reads the time keeps a key of its own in each frame.
fn effect_runtime(renderer: &Renderer, effect: &EffectId, runtime: EffectRuntime) -> EffectRuntime {
    if renderer.effect_reads_time(effect) {
        runtime
    } else {
        EffectRuntime {
            time: MediaTime::ZERO,
            ..runtime
        }
    }
}

/// The runtime of a pass the engine writes whose picture does not change
/// with time (a shape mask or a track matte; [`generator_node`] likewise):
/// without the frame's time, so its cache key is the same in every frame.
fn timeless(runtime: EffectRuntime) -> EffectRuntime {
    EffectRuntime {
        ratio: runtime.ratio,
        ..EffectRuntime::default()
    }
}

/// `picture` kept where `coverage`, drawn on the same canvas, says.
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
    renderer: &Renderer,
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

/// Carries the evaluator's presentation values into the renderer's layer
/// transform unchanged.
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
/// A value between two keyframes is clamped to its parameter's range: a
/// timing curve may overshoot, and the overshoot is part of the motion, not an
/// authoring error. An authored value (a constant, or the value on a
/// keyframe) is passed as written, and the renderer rejects one outside the
/// range.
fn effect_params<'a>(
    renderer: &Renderer,
    id: &EffectId,
    effect: ActiveEffect<'a>,
) -> EffectParams<'a> {
    let mut params = EffectParams::with_capacity(effect.authored().params.len());
    for (name, sample) in effect.params() {
        let mut value = param_value(sample.value);
        if sample.interpolated
            && let Some(spec) = renderer
                .effect_definition(id)
                .and_then(|definition| definition.params.iter().find(|spec| spec.name == name))
        {
            value = clamp_to_range(spec, value);
        }
        params.insert(name, value);
    }
    params
}

/// The identity a project's effect is registered under, which the graph
/// holds without copying its text.
fn project_effect_id(renderer: &Renderer, id: &str, version: u32) -> Result<EffectId, RenderError> {
    renderer
        .project_effect_id(id, version)
        .cloned()
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
    }
}

#[cfg(test)]
mod tests;
