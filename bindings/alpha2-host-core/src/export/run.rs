//! The export loop: frame needs in, decoded and prepared frames into the store, rendered planes
//! out to the encoder.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Weak};

use alpha2_engine::ExportStepTimings;
use alpha2_engine::{
    EngineProject, ExportPlaneStep, Exporter, FrameProvider, FramesWithText, TextTextures,
};
use alpha2_eval::FrameNeed;
use alpha2_media::{CancelToken, DecodedVideoFrame, EncodeError, PixelLayout, SourceId};
use alpha2_render::{
    ColorPolicy, CpuRenderer, CpuSource, Nv12PlaneTargets, PlaneTarget, RenderSize, Renderer,
};
use alpha2_time::MediaTime;

use super::budget::FrameBudget;
use super::prepare::{GpuSource, SurfaceImport, prepare_cpu, prepare_gpu};
use crate::cursor::{CursorReads, DecodeCursorKey, decode_cursor_key};
use crate::encode::{LentBuffer, PlaneBuffer};
use crate::video_stream::{BackendImage, DroppedLayer, FrameFacts, StreamFrame, SurfaceLease};
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

    /// Lets go of what only frames before timeline `time` read; called before the frame at `time`
    /// is decoded.
    fn release_finished(&mut self, _time: MediaTime) {}

    /// Bytes of decoded frames held outside the prepared-frame cache.
    fn held_bytes(&self) -> u64 {
        0
    }
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

    /// Finishes the frames the encoder holds, before the export waits. The default does nothing.
    ///
    /// # Errors
    /// An encoder or output failure.
    fn settle(&mut self) -> Result<(), EncodeError> {
        Ok(())
    }

    /// The encoder's buffers to render frames into, when it lends them. The default lends none.
    fn lender(&mut self) -> Option<&mut dyn SinkLender> {
        None
    }
}

/// A sink whose encoder lends its input buffers: the renderer draws a frame into one, and the
/// sink encodes it.
pub trait SinkLender {
    /// A buffer for the next frame, waiting while the encoder is busy.
    ///
    /// # Errors
    /// An encoder failure, or an encoder that stays busy.
    fn lend(&mut self) -> Result<LentBuffer, EncodeError>;

    /// Encodes frame `index` at `time`, drawn into `buffer`.
    ///
    /// # Errors
    /// An encoder or output failure.
    fn write_lent(
        &mut self,
        index: u64,
        time: MediaTime,
        buffer: LentBuffer,
    ) -> Result<(), EncodeError>;
}

/// The luma and chroma render targets of a lent buffer, as the NV12 pack writes them.
#[derive(Clone, Debug)]
pub struct LentPlanes {
    /// `R8Unorm`, at the output size.
    pub luma: wgpu::TextureView,
    /// `Rg8Unorm`, at half the output size each way.
    pub chroma: wgpu::TextureView,
}

/// Makes lent encoder buffers render targets on the GPU renderer's device: the platform's half
/// of rendering frames in place, as [`SurfaceImport`] is of decoding them.
pub trait EncoderTargets {
    /// The planes of `buffer` as render targets for a frame of `size`.
    ///
    /// # Errors
    /// A buffer that cannot be a render target there; the export then reads frames back.
    fn planes(&mut self, buffer: &LentBuffer, size: RenderSize) -> Result<LentPlanes, String>;

    /// Waits until the GPU finished the frame rendered into `buffer`'s planes.
    ///
    /// # Errors
    /// A wait that failed or ran out of time.
    fn finished(&mut self, buffer: &LentBuffer) -> Result<(), String>;
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

    /// Sets the cache's budget to `budget` less the `held` bytes the sources keep, evicting at once
    /// to fit. It never goes below the last output frame's working set, so a budget lowered under
    /// memory pressure still lets the next frame of the same shape render.
    pub fn follow(&mut self, budget: &FrameBudget, held: u64) {
        let bytes = budget.bytes().saturating_sub(held);
        match self {
            Self::Gpu(store) => {
                store.set_video_byte_budget(bytes.max(store.last_working_set_bytes()));
            }
            Self::Cpu(store) => {
                store.set_video_byte_budget(bytes.max(store.last_working_set_bytes()));
            }
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
/// decode timing. Decoded frames stay within `budget`, read before every frame. `before_frame`
/// runs at the top of every turn with the sink (it may block, as a host's pause does), and
/// `progress` is told after each frame. With the GPU renderer, `targets` and a sink that lends,
/// frames are drawn into the encoder's buffers; once `targets` refuses one, the rest of the
/// export reads frames back.
///
/// # Errors
/// Cancellation, a source or render failure, a frame whose sources never arrive, or a sink
/// failure.
#[allow(clippy::too_many_arguments)]
pub fn export_video(
    snapshot: &EngineProject,
    exporter: &mut Exporter,
    (renderer, mut targets): (ExportRenderer<'_>, Option<&mut (dyn EncoderTargets + '_)>),
    texts: &mut TextTextures,
    sources: &mut dyn SourceFrames,
    sink: &mut dyn VideoSink,
    (budget, cancel): (&FrameBudget, &CancelToken),
    before_frame: &mut dyn FnMut(&mut dyn VideoSink) -> Result<(), EncodeError>,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(), ExportError> {
    let size = exporter
        .render_size(snapshot)
        .map_err(|error| ExportError::Render(error.to_string()))?;
    let color = alpha2_engine::color_policy(&snapshot.project().settings.color);
    let mut store = SourceFrameCache::for_renderer(renderer, budget.bytes());
    let layout = exporter.pixels();
    let mut planes = plane_buffers(layout, size);
    let mut refills = 0;
    let mut kept: Option<LentBuffer> = None;
    loop {
        before_frame(sink).map_err(ExportError::Encode)?;
        if cancel.is_cancelled() {
            return Err(ExportError::Cancelled);
        }
        let needs = exporter
            .prefetch_needs(snapshot, 1)
            .map_err(|error| ExportError::Render(error.to_string()))?;
        let Some(frame) = needs.first() else {
            return Ok(());
        };
        sources.release_finished(frame.time);
        store.follow(budget, sources.held_bytes());
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
        let lent = match (renderer, targets.as_deref_mut(), sink.lender()) {
            (ExportRenderer::Gpu(gpu), Some(lending), Some(lender)) => Some(render_lent(
                (snapshot, exporter, gpu, &provider, size),
                lending,
                lender,
                &mut kept,
            )?),
            _ => None,
        };
        let (step, buffer) = match lent {
            Some(LentStep::Drawn(step, buffer)) => (step, Some(buffer)),
            refused => {
                if matches!(refused, Some(LentStep::Refused)) {
                    targets = None;
                }
                let step = render_readback(snapshot, exporter, renderer, &provider, &mut planes)?;
                (step, None)
            }
        };
        match step {
            ExportPlaneStep::Rendered { index, time } => {
                refills = 0;
                store.unpin();
                match buffer {
                    Some(buffer) => write_lent(targets.as_deref_mut(), sink, index, time, buffer)?,
                    None => sink
                        .write_video(index, time, layout, &planes)
                        .map_err(ExportError::Encode)?,
                }
                let done = exporter.progress();
                progress(done.completed, done.total);
            }
            ExportPlaneStep::Waiting {
                time,
                missing_sources,
            } => {
                kept = buffer;
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

fn plane_buffers(layout: PixelLayout, size: RenderSize) -> Vec<PlaneBuffer> {
    layout
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
        .collect()
}

enum LentStep {
    Drawn(ExportPlaneStep, LentBuffer),
    Refused,
}

fn render_lent(
    (snapshot, exporter, renderer, provider, size): (
        &EngineProject,
        &mut Exporter,
        &Renderer,
        &dyn FrameProvider,
        RenderSize,
    ),
    targets: &mut (dyn EncoderTargets + '_),
    lender: &mut dyn SinkLender,
    kept: &mut Option<LentBuffer>,
) -> Result<LentStep, ExportError> {
    let buffer = match kept.take() {
        Some(buffer) => buffer,
        None => lender.lend().map_err(ExportError::Encode)?,
    };
    let Ok(planes) = targets.planes(&buffer, size) else {
        return Ok(LentStep::Refused);
    };
    let step = exporter
        .render_next_nv12_timed(
            snapshot,
            renderer,
            provider,
            Nv12PlaneTargets {
                luma: &planes.luma,
                chroma: &planes.chroma,
            },
            &mut ExportStepTimings::default(),
        )
        .map_err(|error| ExportError::Render(error.to_string()))?;
    Ok(LentStep::Drawn(step, buffer))
}

fn render_readback(
    snapshot: &EngineProject,
    exporter: &mut Exporter,
    renderer: ExportRenderer<'_>,
    provider: &dyn FrameProvider,
    planes: &mut [PlaneBuffer],
) -> Result<ExportPlaneStep, ExportError> {
    let mut targets = planes
        .iter_mut()
        .map(|plane| PlaneTarget::new(&mut plane.bytes, plane.stride))
        .collect::<Vec<_>>();
    match renderer {
        ExportRenderer::Gpu(renderer) => {
            exporter.render_next_into_planes(snapshot, renderer, provider, &mut targets, None)
        }
        ExportRenderer::Cpu(renderer) => {
            exporter.render_next_into_planes(snapshot, renderer, provider, &mut targets, None)
        }
    }
    .map_err(|error| ExportError::Render(error.to_string()))
}

fn write_lent(
    targets: Option<&mut (dyn EncoderTargets + '_)>,
    sink: &mut dyn VideoSink,
    index: u64,
    time: MediaTime,
    buffer: LentBuffer,
) -> Result<(), ExportError> {
    let (Some(targets), Some(lender)) = (targets, sink.lender()) else {
        return Err(ExportError::Render(
            "a frame drawn into a lent buffer lost its lender".into(),
        ));
    };
    targets.finished(&buffer).map_err(ExportError::Render)?;
    lender
        .write_lent(index, time, buffer)
        .map_err(ExportError::Encode)
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
///
/// A cursor its reads say is finished is closed, at most a set number of video streams stay open,
/// and one decoded still serves every cursor of its source. A closed cursor reopens with a seek.
pub struct StreamSources<F> {
    open: F,
    streams: HashMap<DecodeCursorKey, Cursor>,
    /// When each cursor reads, which closes a finished one.
    reads: Option<CursorReads>,
    /// Video streams open at most, beyond the ones the current output frame reads.
    session_limit: Option<usize>,
    /// Each still source's decoded picture while a cursor holds it.
    stills: HashMap<SourceId, Weak<DecodedVideoFrame>>,
    /// Sources that opened as something other than a video stream.
    not_video: HashSet<SourceId>,
    /// What each source's pictures go without, noted when it first opens.
    dropped: BTreeMap<SourceId, DroppedLayer>,
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

/// One cursor's source, the time it was last asked for and the frame it answered, and the output
/// frame that last read it.
struct Cursor {
    stream: SourceStream,
    last: Option<(MediaTime, StreamFrame)>,
    used: u64,
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
            reads: None,
            session_limit: None,
            stills: HashMap::new(),
            not_video: HashSet::new(),
            dropped: BTreeMap::new(),
            surfaces: None,
            #[cfg(feature = "providers")]
            providers: None,
        }
    }

    /// Closes a cursor once `reads` say it reads nothing more ([`SourceFrames::release_finished`]).
    /// A cursor `reads` does not know is never closed this way.
    #[must_use]
    pub fn with_reads(mut self, reads: CursorReads) -> Self {
        self.reads = Some(reads);
        self
    }

    /// Before opening a video stream while `limit` are open, closes the one read longest ago that
    /// the current output frame does not read.
    #[must_use]
    pub fn with_session_limit(mut self, limit: usize) -> Self {
        self.session_limit = Some(limit.max(1));
        self
    }

    /// Video streams open now.
    #[must_use]
    pub fn open_streams(&self) -> usize {
        self.streams
            .values()
            .filter(|cursor| matches!(cursor.stream, SourceStream::Video(_)))
            .count()
    }

    /// Closes the least recently read video stream other than those output frame `index` read,
    /// while `limit` or more are open.
    fn make_room(&mut self, limit: usize, index: u64) {
        while self.open_streams() >= limit {
            let Some(oldest) = self
                .streams
                .iter()
                .filter(|(_, cursor)| {
                    cursor.used != index && matches!(cursor.stream, SourceStream::Video(_))
                })
                .min_by_key(|(_, cursor)| cursor.used)
                .map(|(key, _)| key.clone())
            else {
                return;
            };
            self.streams.remove(&oldest);
            self.collect_surfaces();
        }
    }

    /// Lets the surface import free what the closed streams' frames held.
    fn collect_surfaces(&self) {
        if let Some(import) = &self.surfaces {
            import.collect();
        }
    }

    /// Opens `source`'s stream, reusing a still another cursor holds.
    fn open_stream(&mut self, source: &SourceId, index: u64) -> Result<SourceStream, String> {
        if let Some(still) = self.stills.get(source).and_then(Weak::upgrade) {
            return Ok(SourceStream::Still(still));
        }
        if let Some(limit) = self.session_limit
            && !self.not_video.contains(source)
        {
            self.make_room(limit, index);
        }
        let stream = (self.open)(source)?;
        match &stream {
            SourceStream::Still(still) => {
                self.stills.insert(source.clone(), Arc::downgrade(still));
                self.not_video.insert(source.clone());
            }
            SourceStream::AudioOnly => {
                self.not_video.insert(source.clone());
            }
            SourceStream::Video(video) => {
                if let Some(dropped) = video.dropped_layer() {
                    self.dropped
                        .entry(source.clone())
                        .or_insert_with(|| dropped.clone());
                }
            }
        }
        Ok(stream)
    }

    /// What the pictures of each source opened so far go without, one entry per source.
    #[must_use]
    pub const fn dropped_layers(&self) -> &BTreeMap<SourceId, DroppedLayer> {
        &self.dropped
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
            let stream = self.open_stream(&need.source_id, index)?;
            self.streams.insert(
                cursor.clone(),
                Cursor {
                    stream,
                    last: None,
                    used: index,
                },
            );
        }
        let entry = self
            .streams
            .get_mut(&cursor)
            .ok_or_else(|| "the stream was not opened".to_owned())?;
        entry.used = index;
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

    fn release_finished(&mut self, time: MediaTime) {
        if let Some(reads) = &self.reads {
            let open = self.streams.len();
            self.streams
                .retain(|cursor, _| !reads.finished(cursor, time));
            if self.streams.len() < open {
                self.collect_surfaces();
            }
        }
    }

    fn held_bytes(&self) -> u64 {
        let mut stills = HashSet::new();
        self.streams
            .values()
            .map(|cursor| match &cursor.stream {
                SourceStream::Video(stream) => {
                    stream.lookahead_bytes()
                        + cursor
                            .last
                            .as_ref()
                            .map_or(0, |(_, frame)| frame.retained_bytes())
                }
                SourceStream::Still(still) if stills.insert(Arc::as_ptr(still)) => still
                    .planes
                    .iter()
                    .map(|plane| plane.bytes.len() as u64)
                    .sum(),
                SourceStream::Still(_) | SourceStream::AudioOnly => 0,
            })
            .sum()
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
/// last answered, still shown when no later frame is due. That holds for a `previous` later than
/// `time` too: a seek before the stream's first frame answers that frame, shown until it is due.
fn decode_forward(
    stream: &mut crate::video_stream::VideoStream,
    time: MediaTime,
    previous: Option<StreamFrame>,
    cancel: &CancelToken,
) -> Result<StreamFrame, String> {
    let deadline = time.presentation_deadline(stream.presentation_slack());
    let mut shown = previous;
    while let Some(pts) = stream.peek_pts(cancel).map_err(|error| error.to_string())?
        && pts <= deadline
    {
        shown = stream.next(cancel).map_err(|error| error.to_string())?;
    }
    match shown {
        Some(frame) => Ok(frame),
        None => stream.seek(time, cancel).map_err(|error| error.to_string()),
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
