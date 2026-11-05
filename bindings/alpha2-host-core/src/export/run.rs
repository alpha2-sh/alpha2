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

use super::frame_store::FrameStore;
use super::prepare::{GpuSource, prepare_cpu, prepare_gpu};
use crate::cursor::{DecodeCursorKey, decode_cursor_key};

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

/// Decoded frames of the project's sources, by need.
pub trait SourceFrames {
    /// The decoded frame `need` shows: the frame presented at its media time.
    ///
    /// # Errors
    /// A source that cannot be opened or decoded there, with the reason.
    fn frame(
        &mut self,
        need: &FrameNeed,
        cancel: &CancelToken,
    ) -> Result<Arc<DecodedVideoFrame>, String>;
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

/// One plane of a rendered picture.
#[derive(Clone, Debug, Default)]
pub struct PlaneBuffer {
    /// The rows, `stride` bytes apart.
    pub bytes: Vec<u8>,
    /// Bytes between rows.
    pub stride: usize,
    /// Width in samples.
    pub width: u32,
    /// Height in rows.
    pub height: u32,
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
enum Store {
    Gpu(FrameStore<GpuSource>),
    Cpu(FrameStore<CpuSource>),
}

impl Store {
    fn provider(&self) -> &dyn FrameProvider {
        match self {
            Self::Gpu(store) => store,
            Self::Cpu(store) => store,
        }
    }

    fn unpin(&mut self) {
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
/// decode timing. `progress` is told after each frame.
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
    cancel: &CancelToken,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(), ExportError> {
    let size = exporter
        .render_size(snapshot)
        .map_err(|error| ExportError::Render(error.to_string()))?;
    let color = alpha2_engine::color_policy(&snapshot.project().settings.color);
    let mut store = match renderer {
        ExportRenderer::Gpu(_) => Store::Gpu(FrameStore::sized(GpuSource::retained_bytes, 1 << 30)),
        ExportRenderer::Cpu(_) => Store::Cpu(FrameStore::default()),
    };
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
        fill(
            &mut store,
            renderer,
            size,
            color,
            &frame.needs,
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
    store: &mut Store,
    renderer: ExportRenderer<'_>,
    size: RenderSize,
    color: ColorPolicy,
    needs: &[FrameNeed],
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
        let frame = sources.frame(need, cancel).map_err(failed)?;
        let inserted = match (store as &mut Store, renderer) {
            (Store::Gpu(store), ExportRenderer::Gpu(renderer)) => {
                let prepared = prepare_gpu(renderer, size, color, &frame).map_err(failed)?;
                store.insert(key, prepared).map(|_| ())
            }
            (Store::Cpu(store), ExportRenderer::Cpu(renderer)) => {
                let prepared = prepare_cpu(renderer, size, color, frame).map_err(failed)?;
                store.insert(key, prepared).map(|_| ())
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
}

/// One cursor's stream, the time it was last asked for and the frame it answered.
struct Cursor {
    stream: crate::video_stream::VideoStream,
    last: Option<(MediaTime, crate::video_stream::StreamFrame)>,
}

impl<F> StreamSources<F>
where
    F: FnMut(&SourceId) -> Result<crate::video_stream::VideoStream, String>,
{
    /// Opens sources with `open`, a source's stream from the start.
    pub fn new(open: F) -> Self {
        Self {
            open,
            streams: HashMap::new(),
        }
    }
}

impl<F> SourceFrames for StreamSources<F>
where
    F: FnMut(&SourceId) -> Result<crate::video_stream::VideoStream, String>,
{
    fn frame(
        &mut self,
        need: &FrameNeed,
        cancel: &CancelToken,
    ) -> Result<Arc<DecodedVideoFrame>, String> {
        let cursor = decode_cursor_key(need);
        if !self.streams.contains_key(&cursor) {
            let stream = (self.open)(&need.source_id)?;
            self.streams
                .insert(cursor.clone(), Cursor { stream, last: None });
        }
        let entry = self
            .streams
            .get_mut(&cursor)
            .ok_or_else(|| "the stream was not opened".to_owned())?;
        let time = need.media_time;
        let frame = match entry.last.take() {
            Some((previous, frame)) if crate::cursor::decodes_on(previous, time) => {
                decode_forward(&mut entry.stream, time, Some(frame), cancel)?
            }
            _ => entry
                .stream
                .seek(time, cancel)
                .map_err(|error| error.to_string())?,
        };
        entry.last = Some((time, frame.clone()));
        frame
            .to_cpu()
            .map(Arc::new)
            .ok_or_else(|| "the decoder produced a surface where CPU planes were asked".to_owned())
    }
}

/// The frame shown at `time`, decoding forward from where `stream` is; `previous` is the frame it
/// last answered, still shown when no later frame is due.
fn decode_forward(
    stream: &mut crate::video_stream::VideoStream,
    time: MediaTime,
    previous: Option<crate::video_stream::StreamFrame>,
    cancel: &CancelToken,
) -> Result<crate::video_stream::StreamFrame, String> {
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
