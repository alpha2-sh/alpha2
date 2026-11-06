//! The export loop: frame needs in, decoded and prepared frames into the store, rendered planes
//! out to the encoder.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use alpha2_engine::{
    EngineProject, ExportPlaneStep, Exporter, FrameProvider, FramesWithText, TextTextures,
};
use alpha2_eval::FrameNeed;
use alpha2_media::{CancelToken, DecodedVideoFrame, EncodeError, PixelLayout, SourceId};
use alpha2_render::{ColorPolicy, CpuRenderer, CpuSource, PlaneTarget, RenderSize, Renderer};
use alpha2_time::MediaTime;

use super::budget::FrameBudget;
use super::prepare::{GpuSource, SurfaceImport, prepare_cpu, prepare_gpu};
use crate::cursor::{DecodeCursorKey, decode_cursor_key};
use crate::encode::PlaneBuffer;
use crate::video_stream::{BackendImage, FrameFacts, StreamFrame, SurfaceLease};
use alpha2_engine::frame_store::FrameStore;

/// Steps an export may wait on the same frame, refilling between them, before it fails: a frame
/// still missing after this many refills will not arrive.
pub const MAX_EXPORT_REFILLS: u32 = 4;

/// The renderer an export draws with.
#[derive(Clone, Copy)]
pub enum ExportRenderer<'a> {
    /// The GPU renderer.
    Gpu(&'a Renderer),
    /// The CPU renderer.
    Cpu(&'a CpuRenderer),
}

/// A decoded source frame: planes in memory, or a platform surface the GPU renderer imports.
#[derive(Clone, Debug)]
pub enum SourceFrame {
    /// Planes in memory, for either renderer.
    Cpu(Arc<DecodedVideoFrame>),
    /// A decoder surface, for the GPU renderer only.
    Surface(SurfaceFrame),
}

/// A decoder surface and the import that turns it into a GPU source.
#[derive(Clone)]
pub struct SurfaceFrame {
    /// The frame's facts.
    pub facts: FrameFacts,
    /// The surface, held until the frame is prepared.
    pub lease: Arc<dyn SurfaceLease>,
    /// Prepares the surface for the GPU renderer.
    pub import: Arc<dyn SurfaceImport>,
}

impl std::fmt::Debug for SurfaceFrame {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SurfaceFrame")
            .field("facts", &self.facts)
            .field("lease", &self.lease)
            .finish_non_exhaustive()
    }
}

/// Decoded frames of the project's sources, by need.
pub trait SourceFrames {
    /// The decoded frame `need` of output frame `index` shows: the frame presented at its media
    /// time; `None` for an audio source, which audio clips name but nothing draws.
    ///
    /// # Errors
    /// A source that cannot be opened or decoded there, with the reason.
    fn frame(
        &mut self,
        need: &FrameNeed,
        index: u64,
        cancel: &CancelToken,
    ) -> Result<Option<SourceFrame>, String>;
}

/// Where rendered pictures go: the video encoder.
pub trait VideoSink {
    /// Takes frame `index` at `time`, packed as `layout` in one buffer per plane.
    ///
    /// # Errors
    /// An encoder or output failure.
    fn write_video(
        &mut self,
        index: u64,
        time: MediaTime,
        layout: PixelLayout,
        planes: &[PlaneBuffer],
    ) -> Result<(), EncodeError>;
}

/// Why an export stopped.
#[derive(Debug)]
pub enum ExportError {
    /// The caller cancelled.
    Cancelled,
    /// A source could not be decoded.
    Source {
        /// The source.
        source: SourceId,
        /// Why.
        reason: String,
    },
    /// A frame's sources never arrived after [`MAX_EXPORT_REFILLS`] refills.
    Stalled {
        /// The frame's timeline time.
        time: MediaTime,
        /// The sources still missing.
        missing: Vec<SourceId>,
    },
    /// The engine failed to evaluate or render.
    Render(String),
    /// The encoder or output failed.
    Encode(EncodeError),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(formatter, "the export was cancelled"),
            Self::Source { source, reason } => write!(formatter, "source {source}: {reason}"),
            Self::Stalled { time, missing } => write!(
                formatter,
                "frame at {} ticks still lacks {} after {MAX_EXPORT_REFILLS} refills",
                time.ticks(),
                missing
                    .iter()
                    .map(SourceId::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Render(reason) => write!(formatter, "{reason}"),
            Self::Encode(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for ExportError {}

/// Prepared frames for whichever renderer runs.
/// Decoded source frames prepared for one renderer, kept within a byte budget: what an export or a
/// preview renders from.
pub enum SourceFrameCache {
    /// Frames uploaded for the GPU renderer.
    Gpu(FrameStore<GpuSource>),
    /// Frames converted for the CPU renderer.
    Cpu(FrameStore<CpuSource>),
}

impl SourceFrameCache {
    /// An empty cache for `renderer`, keeping up to `bytes` of frames.
    #[must_use]
    pub fn for_renderer(renderer: ExportRenderer<'_>, bytes: u64) -> Self {
        match renderer {
            ExportRenderer::Gpu(_) => {
                Self::Gpu(FrameStore::sized(GpuSource::retained_bytes, bytes))
            }
            ExportRenderer::Cpu(_) => {
                Self::Cpu(FrameStore::sized(CpuSource::retained_bytes, bytes))
            }
        }
    }

    /// Takes `budget`'s bytes as the cache's budget, evicting at once to fit.
    pub fn follow(&mut self, budget: &FrameBudget) {
        match self {
            Self::Gpu(store) => store.set_video_byte_budget(budget.bytes()),
            Self::Cpu(store) => store.set_video_byte_budget(budget.bytes()),
        }
    }

    /// The frames, for the renderer.
    #[must_use]
    pub fn provider(&self) -> &dyn FrameProvider {
        match self {
            Self::Gpu(store) => store,
            Self::Cpu(store) => store,
        }
    }

    /// Decodes and prepares every frame `needs` (of output frame `index`) names that the cache
    /// does not hold, and keeps them until [`Self::unpin`].
    ///
    /// # Errors
    /// A source that cannot be decoded or prepared, or one past the budget.
    pub fn fill(
        &mut self,
        renderer: ExportRenderer<'_>,
        size: RenderSize,
        color: ColorPolicy,
        (index, needs): (u64, &[FrameNeed]),
        sources: &mut dyn SourceFrames,
        cancel: &CancelToken,
    ) -> Result<(), ExportError> {
        fill(self, renderer, size, color, (index, needs), sources, cancel)
    }

    /// Lets the frames the last [`Self::fill`] kept go when the budget needs room.
    pub fn unpin(&mut self) {
        match self {
            Self::Gpu(store) => store.unpin(),
            Self::Cpu(store) => store.unpin(),
        }
    }

    fn pin(&mut self, keys: Vec<(String, i64)>) {
        match self {
            Self::Gpu(store) => store.pin(keys),
            Self::Cpu(store) => store.pin(keys),
        }
    }

    fn holds(&self, source: &str, time: i64) -> bool {
        match self {
            Self::Gpu(store) => store.get(source, time).is_some(),
            Self::Cpu(store) => store.get(source, time).is_some(),
        }
    }
}

/// Renders every frame of `exporter` into `sink`, decoding sources through `sources`.
///
/// Frames wait for their sources and never repeat a late one, so output does not depend on
/// decode timing. Decoded frames stay within `budget`, read before every frame. `progress` is
/// told after each frame.
///
/// # Errors
/// Cancellation, a source or render failure, a frame whose sources never arrive, or a sink
/// failure.
#[allow(clippy::too_many_arguments)]
pub fn export_video(
    snapshot: &EngineProject,
    exporter: &mut Exporter,
    renderer: ExportRenderer<'_>,
    texts: &mut TextTextures,
    sources: &mut dyn SourceFrames,
    sink: &mut dyn VideoSink,
    (budget, cancel): (&FrameBudget, &CancelToken),
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(), ExportError> {
    let size = exporter
        .render_size(snapshot)
        .map_err(|error| ExportError::Render(error.to_string()))?;
    let color = alpha2_engine::color_policy(&snapshot.project().settings.color);
    let mut store = SourceFrameCache::for_renderer(renderer, budget.bytes());
    let layout = exporter.pixels();
    let mut planes = layout
        .planes(size.width, size.height)
        .into_iter()
        .map(|(width, height, bytes)| {
            let stride = usize::try_from(width * bytes).unwrap_or(0);
            PlaneBuffer {
                bytes: vec![0; stride * usize::try_from(height).unwrap_or(0)],
                stride,
                width,
                height,
            }
        })
        .collect::<Vec<_>>();
    let mut refills = 0;
    loop {
        if cancel.is_cancelled() {
            return Err(ExportError::Cancelled);
        }
        let needs = exporter
            .prefetch_needs(snapshot, 1)
            .map_err(|error| ExportError::Render(error.to_string()))?;
        let Some(frame) = needs.first() else {
            return Ok(());
        };
        store.follow(budget);
        fill(
            &mut store,
            renderer,
            size,
            color,
            (frame.index, &frame.needs),
            sources,
            cancel,
        )?;
        if let Some(time) = exporter.pending_time() {
            let upload: &dyn alpha2_engine::TextUpload = match renderer {
                ExportRenderer::Gpu(renderer) => renderer,
                ExportRenderer::Cpu(renderer) => renderer,
            };
            texts
                .prepare_at(upload, snapshot, time, size)
                .map_err(|error| ExportError::Render(error.to_string()))?;
        }
        let provider = FramesWithText::new(store.provider(), texts);
        let mut targets = planes
            .iter_mut()
            .map(|plane| PlaneTarget::new(&mut plane.bytes, plane.stride))
            .collect::<Vec<_>>();
        let step = match renderer {
            ExportRenderer::Gpu(renderer) => {
                exporter.render_next_into_planes(snapshot, renderer, &provider, &mut targets, None)
            }
            ExportRenderer::Cpu(renderer) => {
                exporter.render_next_into_planes(snapshot, renderer, &provider, &mut targets, None)
            }
        }
        .map_err(|error| ExportError::Render(error.to_string()))?;
        drop(targets);
        match step {
            ExportPlaneStep::Rendered { index, time } => {
                refills = 0;
                store.unpin();
                sink.write_video(index, time, layout, &planes)
                    .map_err(ExportError::Encode)?;
                let done = exporter.progress();
                progress(done.completed, done.total);
            }
            ExportPlaneStep::Waiting {
                time,
                missing_sources,
            } => {
                refills += 1;
                if refills > MAX_EXPORT_REFILLS {
                    return Err(ExportError::Stalled {
                        time,
                        missing: missing_sources,
                    });
                }
            }
            ExportPlaneStep::Complete => return Ok(()),
        }
    }
}

/// Decodes and prepares every frame `needs` names that the store does not hold, pinning them.
fn fill(
    store: &mut SourceFrameCache,
    renderer: ExportRenderer<'_>,
    size: RenderSize,
    color: ColorPolicy,
    (index, needs): (u64, &[FrameNeed]),
    sources: &mut dyn SourceFrames,
    cancel: &CancelToken,
) -> Result<(), ExportError> {
    let keys = needs
        .iter()
        .map(|need| (need.source_id.as_str().to_owned(), need.media_time.ticks()))
        .collect::<Vec<_>>();
    store.pin(keys);
    let mut done = HashSet::new();
    for need in needs {
        let key = (need.source_id.as_str().to_owned(), need.media_time.ticks());
        if !done.insert(key.clone()) || store.holds(&key.0, key.1) {
            continue;
        }
        let failed = |reason: String| ExportError::Source {
            source: need.source_id.clone(),
            reason,
        };
        let Some(frame) = sources.frame(need, index, cancel).map_err(failed)? else {
            continue;
        };
        let inserted = match (store as &mut SourceFrameCache, renderer, frame) {
            (
                SourceFrameCache::Gpu(store),
                ExportRenderer::Gpu(renderer),
                SourceFrame::Cpu(frame),
            ) => {
                let prepared = prepare_gpu(renderer, size, color, &frame).map_err(failed)?;
                store.insert(key, prepared).map(|_| ())
            }
            (
                SourceFrameCache::Gpu(store),
                ExportRenderer::Gpu(renderer),
                SourceFrame::Surface(surface),
            ) => {
                let prepared = surface
                    .import
                    .prepare(renderer, size, color, &surface.facts, &surface.lease)
                    .map_err(failed)?;
                store.insert(key, prepared).map(|_| ())
            }
            (
                SourceFrameCache::Cpu(store),
                ExportRenderer::Cpu(renderer),
                SourceFrame::Cpu(frame),
            ) => {
                let prepared = prepare_cpu(renderer, size, color, frame).map_err(failed)?;
                store.insert(key, prepared).map(|_| ())
            }
            (SourceFrameCache::Cpu(_), ExportRenderer::Cpu(_), SourceFrame::Surface(_)) => {
                return Err(failed(
                    "the decoder produced a GPU surface, which the CPU renderer cannot draw".into(),
                ));
            }
            _ => unreachable!("the store matches the renderer"),
        };
        inserted.map_err(|over| {
            failed(format!(
                "the frame's sources need {} bytes, over the {} byte budget",
                over.needed(),
                over.budget
            ))
        })?;
    }
    Ok(())
}

/// Opens one [`crate::video_stream::VideoStream`] per cursor and keeps decoding forward while
/// requests stay within the sequential gap, seeking otherwise.
pub struct StreamSources<F> {
    open: F,
    streams: HashMap<DecodeCursorKey, Cursor>,
    /// Prepares the decoder surfaces the streams produce.
    surfaces: Option<Arc<dyn SurfaceImport>>,
    /// Supplies the extension sources.
    #[cfg(feature = "providers")]
    providers: Option<Arc<crate::providers::ProviderMedia>>,
}

/// What a source decodes from: a video stream, or a still shown at every time.
pub enum SourceStream {
    /// A video track's frames.
    Video(Box<crate::video_stream::VideoStream>),
    /// One picture.
    Still(Arc<DecodedVideoFrame>),
    /// Audio alone: audio clips name the source, but it has no pictures.
    AudioOnly,
}

/// One cursor's source, the time it was last asked for and the frame it answered.
struct Cursor {
    stream: SourceStream,
    last: Option<(MediaTime, StreamFrame)>,
}

impl<F> StreamSources<F>
where
    F: FnMut(&SourceId) -> Result<SourceStream, String>,
{
    /// Opens sources with `open`, a source's stream from the start.
    pub fn new(open: F) -> Self {
        Self {
            open,
            streams: HashMap::new(),
            surfaces: None,
            #[cfg(feature = "providers")]
            providers: None,
        }
    }

    /// Prepares the decoder surfaces the streams produce with `import`; without it a surface
    /// frame fails its source.
    #[must_use]
    pub fn with_surface_import(mut self, import: Arc<dyn SurfaceImport>) -> Self {
        self.surfaces = Some(import);
        self
    }

    /// Also takes the frames of the sources `providers` supplies from it.
    #[cfg(feature = "providers")]
    #[must_use]
    pub fn with_providers(mut self, providers: Arc<crate::providers::ProviderMedia>) -> Self {
        self.providers = Some(providers);
        self
    }
}

impl<F> SourceFrames for StreamSources<F>
where
    F: FnMut(&SourceId) -> Result<SourceStream, String>,
{
    fn frame(
        &mut self,
        need: &FrameNeed,
        index: u64,
        cancel: &CancelToken,
    ) -> Result<Option<SourceFrame>, String> {
        let cursor = decode_cursor_key(need);
        // Only provider sources keep frames by output frame.
        #[cfg(not(feature = "providers"))]
        let _ = index;
        #[cfg(feature = "providers")]
        if let Some(providers) = self
            .providers
            .as_ref()
            .filter(|providers| providers.supplies(&need.source_id))
        {
            return providers
                .frame(&cursor, need.media_time, index)
                .map(|frame| Some(SourceFrame::Cpu(Arc::new(frame))))
                .map_err(|error| error.to_string());
        }
        if !self.streams.contains_key(&cursor) {
            let stream = (self.open)(&need.source_id)?;
            self.streams
                .insert(cursor.clone(), Cursor { stream, last: None });
        }
        let entry = self
            .streams
            .get_mut(&cursor)
            .ok_or_else(|| "the stream was not opened".to_owned())?;
        let stream = match &mut entry.stream {
            SourceStream::Still(frame) => return Ok(Some(SourceFrame::Cpu(Arc::clone(frame)))),
            SourceStream::AudioOnly => return Ok(None),
            SourceStream::Video(stream) => stream,
        };
        let time = need.media_time;
        let frame = match entry.last.take() {
            Some((previous, frame)) if crate::cursor::decodes_on(previous, time) => {
                decode_forward(stream, time, Some(frame), cancel)?
            }
            _ => stream
                .seek(time, cancel)
                .map_err(|error| error.to_string())?,
        };
        entry.last = Some((time, frame.clone()));
        source_frame(frame, self.surfaces.as_ref()).map(Some)
    }
}

fn source_frame(
    frame: StreamFrame,
    surfaces: Option<&Arc<dyn SurfaceImport>>,
) -> Result<SourceFrame, String> {
    match frame.image {
        BackendImage::Surface(lease) => {
            let import = surfaces.ok_or_else(|| {
                "the decoder produced a GPU surface, and nothing here imports it".to_owned()
            })?;
            Ok(SourceFrame::Surface(SurfaceFrame {
                facts: frame.facts,
                lease,
                import: Arc::clone(import),
            }))
        }
        BackendImage::Cpu { .. } => frame
            .to_cpu()
            .map(|cpu| SourceFrame::Cpu(Arc::new(cpu)))
            .ok_or_else(|| "a CPU frame without planes".to_owned()),
    }
}

/// The frame shown at `time`, decoding forward from where `stream` is; `previous` is the frame it
/// last answered, still shown when no later frame is due.
fn decode_forward(
    stream: &mut crate::video_stream::VideoStream,
    time: MediaTime,
    previous: Option<StreamFrame>,
    cancel: &CancelToken,
) -> Result<StreamFrame, String> {
    let deadline = time.presentation_deadline(stream.presentation_slack());
    let mut shown = previous.filter(|frame| frame.facts.pts <= deadline);
    loop {
        match stream.peek_pts(cancel).map_err(|error| error.to_string())? {
            Some(pts) if pts <= deadline || shown.is_none() => {
                shown = stream.next(cancel).map_err(|error| error.to_string())?;
            }
            _ => break,
        }
    }
    match shown {
        Some(frame) => Ok(frame),
        None => stream.seek(time, cancel).map_err(|error| error.to_string()),
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
