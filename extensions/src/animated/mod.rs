//! GIF, animated WebP, and APNG, composited frame by frame as each format defines.
//!
//! Probing reads only headers and refuses, before allocating, files past the canvas limit,
//! [`MAX_ANIMATION_FRAMES`], or [`MAX_ANIMATION_PIXELS`]. Opening decodes once to check the file
//! and keeps what fits the frame budget. A request for an uncached frame decodes forward,
//! restarting for earlier frames and caching the frames just before the target, so reverse
//! playback restarts once per budget of frames.
//!
//! Extra readers (clips at unrelated times) each get their own decoder but share the file and the
//! kept frames under one budget.

mod compositor;

use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use alpha2_time::{MediaTime, TICKS_PER_SECOND};

use alpha2_model::ProviderSource;

use crate::api::{
    Diagnostic, ExtensionError, Format, FramePixels, MAX_NORMALIZED_DELAY_MS, NORMALIZED_DELAY_MS,
    ProbeReport, ResourceResolver, Timing, VisualFrame, VisualProvider, VisualSource, premultiply,
};
use crate::budget::{
    DEFAULT_FRAME_BUDGET, MAX_ANIMATION_FRAMES, MAX_ANIMATION_PIXELS, MAX_CANVAS_PIXELS,
    check_canvas,
};
use compositor::{Blend, Compositor, Dispose, PartialFrame};

/// Reads GIF, animated WebP, and APNG. Still files of those formats are left to the host.
pub struct AnimatedImageProvider;

impl VisualProvider for AnimatedImageProvider {
    fn name(&self) -> &'static str {
        "animated-image"
    }

    fn probe(&self, bytes: &[u8]) -> Option<Result<ProbeReport, ExtensionError>> {
        let format = sniff(bytes)?;
        Some(index(format, bytes).map(|index| index.report(format)))
            .filter(|report| !matches!(report, Ok(report) if report.timing.frame_count() < 2))
    }

    fn open(
        &self,
        bytes: Arc<[u8]>,
        _source: &ProviderSource,
        _resolver: &dyn ResourceResolver,
    ) -> Result<Box<dyn VisualSource>, ExtensionError> {
        Ok(Box::new(AnimatedSource::open(bytes, DEFAULT_FRAME_BUDGET)?))
    }
}

/// The format by signature; APNG and WebP only when they declare animation.
fn sniff(bytes: &[u8]) -> Option<Format> {
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(Format::Gif);
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return png_is_animated(bytes).then_some(Format::Apng);
    }
    if bytes.len() >= 21 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        // An extended header whose flags byte sets the animation bit.
        return (&bytes[12..16] == b"VP8X" && bytes[20] & 0x02 != 0).then_some(Format::Webp);
    }
    None
}

/// Whether a PNG has an animation control before its image data. A chunk length that would
/// overflow the address space (easy on wasm32) ends the walk.
fn png_is_animated(bytes: &[u8]) -> bool {
    let mut at = 8_usize;
    while bytes.len().saturating_sub(at) >= 8 {
        let length = u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        let kind = &bytes[at + 4..at + 8];
        if kind == b"acTL" {
            return true;
        }
        if kind == b"IDAT" {
            return false;
        }
        let Some(next) = usize::try_from(length)
            .ok()
            .and_then(|length| at.checked_add(12)?.checked_add(length))
        else {
            return false;
        };
        at = next;
    }
    false
}

/// What an animation's headers declare.
struct Index {
    width: u32,
    height: u32,
    loop_count: Option<u32>,
    frames: Vec<Delay>,
    /// Pixels of all frame rectangles together.
    pixels: u64,
}

impl Index {
    fn new(width: u32, height: u32) -> Result<Self, ExtensionError> {
        check_canvas(width, height)?;
        Ok(Self {
            width,
            height,
            loop_count: None,
            frames: Vec::new(),
            pixels: 0,
        })
    }

    /// Adds a frame, refusing it past the frame or pixel budget.
    fn push(&mut self, width: u32, height: u32, delay: Delay) -> Result<(), ExtensionError> {
        if self.frames.len() == MAX_ANIMATION_FRAMES {
            return Err(ExtensionError::Budget(format!(
                "more than {MAX_ANIMATION_FRAMES} frames"
            )));
        }
        let pixels = u64::from(width) * u64::from(height);
        if pixels > MAX_CANVAS_PIXELS {
            return Err(ExtensionError::Budget(format!(
                "a {width}x{height} frame is larger than {MAX_CANVAS_PIXELS} pixels"
            )));
        }
        self.pixels += pixels;
        if self.pixels > MAX_ANIMATION_PIXELS {
            return Err(ExtensionError::Budget(format!(
                "the frames together cover more than {MAX_ANIMATION_PIXELS} pixels"
            )));
        }
        self.frames.push(delay);
        Ok(())
    }

    fn report(&self, format: Format) -> ProbeReport {
        let mut diagnostics = Vec::new();
        let durations = self
            .frames
            .iter()
            .enumerate()
            .map(|(frame, delay)| {
                if delay.ticks <= i64::from(MAX_NORMALIZED_DELAY_MS) * TICKS_PER_MS {
                    diagnostics.push(Diagnostic::NormalizedDelay {
                        frame,
                        declared_ms: delay.ms,
                    });
                    i64::from(NORMALIZED_DELAY_MS) * TICKS_PER_MS
                } else {
                    delay.ticks
                }
            })
            .collect();
        ProbeReport {
            format,
            width: self.width,
            height: self.height,
            timing: Timing::Frames { durations },
            loop_count: self.loop_count,
            items: Vec::new(),
            item: None,
            color: None,
            diagnostics,
        }
    }
}

/// Reads an animation's headers without decoding any frame.
fn index(format: Format, bytes: &[u8]) -> Result<Index, ExtensionError> {
    let index = match format {
        Format::Gif => gif_index(bytes)?,
        Format::Apng => apng_index(bytes)?,
        Format::Webp => WebpLayout::read(bytes)?.index()?,
        other => {
            return Err(ExtensionError::Unsupported(format!(
                "{other:?} is not an animation"
            )));
        }
    };
    if index.frames.is_empty() {
        return Err(ExtensionError::Malformed(
            "the animation has no frames".into(),
        ));
    }
    Ok(index)
}

fn gif_index(bytes: &[u8]) -> Result<Index, ExtensionError> {
    let mut options = gif_options();
    options.skip_frame_decoding(true);
    let mut decoder = options.read_info(Cursor::new(bytes)).map_err(malformed)?;
    let mut index = Index::new(u32::from(decoder.width()), u32::from(decoder.height()))?;
    while let Some(frame) = decoder.next_frame_info().map_err(malformed)? {
        let (width, height) = (u32::from(frame.width), u32::from(frame.height));
        // Hundredths of a second.
        let delay = Delay {
            ticks: i64::from(frame.delay) * 10 * TICKS_PER_MS,
            ms: u32::from(frame.delay) * 10,
        };
        index.push(width, height, delay)?;
    }
    // The loop extension counts repeats after the first play (0 is forever). Without one, which
    // the decoder reports as `Finite(0)`, the GIF plays once.
    index.loop_count = Some(match decoder.repeat() {
        gif::Repeat::Infinite => 0,
        gif::Repeat::Finite(count) => u32::from(count) + 1,
    });
    Ok(index)
}

fn apng_index(bytes: &[u8]) -> Result<Index, ExtensionError> {
    let mut reader = png::Decoder::new(Cursor::new(bytes))
        .read_info()
        .map_err(malformed)?;
    let info = reader.info();
    let mut index = Index::new(info.width, info.height)?;
    index.loop_count = info.animation_control.map(|control| control.num_plays);
    // The default image is a frame when its frame control comes before it.
    if let Some(control) = info.frame_control {
        index.push(control.width, control.height, apng_delay(&control))?;
    }
    loop {
        // Skip the image data unread.
        let control = match reader.next_frame_info() {
            Ok(control) => *control,
            Err(png::DecodingError::Parameter(_)) => break,
            Err(error) => return Err(malformed(error)),
        };
        index.push(control.width, control.height, apng_delay(&control))?;
    }
    Ok(index)
}

/// A frame control's delay of `delay_num`/`delay_den` seconds (zero denominator means
/// hundredths). Denominators dividing 120 000 are exact in ticks; others round to the nearest.
fn apng_delay(control: &png::FrameControl) -> Delay {
    let den = if control.delay_den == 0 {
        100
    } else {
        i64::from(control.delay_den)
    };
    let num = i64::from(control.delay_num);
    let rounded = |per_second: i64| (num * per_second + den / 2) / den;
    Delay {
        ticks: rounded(TICKS_PER_SECOND),
        ms: u32::try_from(rounded(1_000)).unwrap_or(u32::MAX),
    }
}

#[derive(Clone, Copy)]
struct Delay {
    ticks: i64,
    /// Rounded, for diagnostics.
    ms: u32,
}

const TICKS_PER_MS: i64 = TICKS_PER_SECOND / 1_000;

/// An LRU cache of premultiplied canvases within a byte budget.
struct Kept {
    /// Canvas and last-use time by frame index.
    frames: BTreeMap<usize, (Arc<[u8]>, u64)>,
    by_use: BTreeMap<u64, usize>,
    clock: u64,
    frame_bytes: usize,
    budget: usize,
}

impl Kept {
    fn new(frame_bytes: usize, budget: usize) -> Self {
        Self {
            frames: BTreeMap::new(),
            by_use: BTreeMap::new(),
            clock: 0,
            frame_bytes,
            budget,
        }
    }

    fn capacity(&self) -> usize {
        self.budget / self.frame_bytes.max(1)
    }

    fn bytes(&self) -> usize {
        self.frames.len() * self.frame_bytes
    }

    fn contains(&self, index: usize) -> bool {
        self.frames.contains_key(&index)
    }

    /// A kept frame, marked as most recently used.
    fn get(&mut self, index: usize) -> Option<Arc<[u8]>> {
        self.clock += 1;
        let (canvas, used) = self.frames.get_mut(&index)?;
        self.by_use.remove(used);
        *used = self.clock;
        self.by_use.insert(self.clock, index);
        Some(Arc::clone(canvas))
    }

    /// Keeps a frame, evicting the least recently used when full.
    fn insert(&mut self, index: usize, canvas: Arc<[u8]>) {
        if self.capacity() == 0 {
            return;
        }
        if let Some((_, used)) = self.frames.remove(&index) {
            self.by_use.remove(&used);
        }
        while self.frames.len() >= self.capacity() {
            let Some((_, oldest)) = self.by_use.pop_first() else {
                break;
            };
            self.frames.remove(&oldest);
        }
        self.clock += 1;
        self.frames.insert(index, (canvas, self.clock));
        self.by_use.insert(self.clock, index);
    }
}

/// Decodes every frame once to check the file, keeping the canvases that fit the budget.
fn decode_whole(
    format: Format,
    bytes: &Arc<[u8]>,
    index: &Index,
    budget: usize,
) -> Result<Kept, ExtensionError> {
    let mut cursor = open_cursor(format, bytes)?;
    let mut kept = Kept::new(index.width as usize * index.height as usize * 4, budget);
    for at in 0..index.frames.len() {
        if !cursor.advance()? {
            return Err(ExtensionError::Malformed(
                "the animation ended before a frame it listed".into(),
            ));
        }
        crate::progress::progressed();
        if at < kept.capacity() {
            kept.insert(at, premultiplied(cursor.canvas()));
        }
    }
    Ok(kept)
}

/// What all readers of one open animation share.
struct Animation {
    format: Format,
    bytes: Arc<[u8]>,
    report: ProbeReport,
    /// Locked only to look up or keep a frame, never while decoding.
    frames: Mutex<Kept>,
}

impl Animation {
    /// The kept frames. After a reader panics holding the lock, every reader fails, since the
    /// cache may be half changed.
    fn kept(&self) -> Result<MutexGuard<'_, Kept>, ExtensionError> {
        self.frames.lock().map_err(|_| {
            ExtensionError::Malformed(
                "another reader of this animation failed while keeping its frames".into(),
            )
        })
    }

    /// Works even after a reader panicked.
    fn kept_bytes(&self) -> usize {
        self.frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .bytes()
    }
}

/// One reader of a shared animation, with its own decoder.
struct AnimatedSource {
    shared: Arc<Animation>,
    /// The decoder and the index of the next frame it produces.
    cursor: Option<(Box<dyn FrameCursor>, usize)>,
}

impl AnimatedSource {
    fn open(bytes: Arc<[u8]>, budget: usize) -> Result<Self, ExtensionError> {
        let format = sniff(&bytes)
            .ok_or_else(|| ExtensionError::Mismatch("not an animated GIF, WebP, or APNG".into()))?;
        let index = index(format, &bytes)?;
        if index.frames.len() < 2 {
            return Err(ExtensionError::Mismatch(
                "a still image, which the host's image decoders read".into(),
            ));
        }
        let frames = decode_whole(format, &bytes, &index, budget)?;
        Ok(Self {
            shared: Arc::new(Animation {
                format,
                bytes,
                report: index.report(format),
                frames: Mutex::new(frames),
            }),
            cursor: None,
        })
    }

    /// A frame's premultiplied canvas. An uncached frame is decoded forward (from the start if
    /// the decoder is past it), caching the frames just before it so reverse playback rarely
    /// restarts.
    fn frame(&mut self, index: usize) -> Result<Arc<[u8]>, ExtensionError> {
        let shared = Arc::clone(&self.shared);
        let capacity = {
            let mut kept = shared.kept()?;
            if let Some(frame) = kept.get(index) {
                return Ok(frame);
            }
            kept.capacity()
        };
        let restart = self.cursor.as_ref().is_none_or(|(_, next)| *next > index);
        if restart {
            self.cursor = Some((open_cursor(shared.format, &shared.bytes)?, 0));
        }
        let window = (index + 1).saturating_sub(capacity);
        loop {
            let (cursor, next) = self.cursor.as_mut().ok_or_else(|| {
                ExtensionError::Malformed("the animation's decoder is gone".into())
            })?;
            if !cursor.advance()? {
                return Err(ExtensionError::Malformed(
                    "the animation ended before a frame it listed".into(),
                ));
            }
            crate::progress::progressed();
            let at = *next;
            *next += 1;
            // Every frame on the way is drawn; only those the budget keeps are copied.
            if at != index && (at < window || shared.kept()?.contains(at)) {
                continue;
            }
            let canvas = premultiplied(cursor.canvas());
            shared.kept()?.insert(at, Arc::clone(&canvas));
            if at == index {
                return Ok(canvas);
            }
        }
    }
}

impl VisualSource for AnimatedSource {
    fn report(&self) -> &ProbeReport {
        &self.shared.report
    }

    fn render(&mut self, time: MediaTime, _fit: (u32, u32)) -> Result<VisualFrame, ExtensionError> {
        let (index, start, end) = self.shared.report.timing.frame_at(time);
        let pixels = self.frame(index)?;
        Ok(VisualFrame {
            width: self.shared.report.width,
            height: self.shared.report.height,
            pixels: FramePixels::Rgba8(pixels),
            index,
            start,
            end,
            diagnostics: Vec::new(),
        })
    }

    fn resident_bytes(&self) -> usize {
        let canvas = self.shared.report.width as usize * self.shared.report.height as usize * 4;
        // The decoder reads the shared file, so it adds only its canvas.
        self.shared_bytes() + self.cursor.as_ref().map_or(0, |_| canvas)
    }

    /// A reader with no decoder until it needs an uncached frame.
    fn reader(&self) -> Option<Box<dyn VisualSource>> {
        Some(Box::new(Self {
            shared: Arc::clone(&self.shared),
            cursor: None,
        }))
    }

    fn shared_bytes(&self) -> usize {
        self.shared.bytes.len() + self.shared.kept_bytes()
    }
}

fn premultiplied(canvas: &[u8]) -> Arc<[u8]> {
    let mut shared = Arc::<[u8]>::from(canvas);
    if let Some(pixels) = Arc::get_mut(&mut shared) {
        premultiply(pixels);
    }
    shared
}

trait FrameCursor: Send {
    /// Draws the next frame; `false` past the last.
    fn advance(&mut self) -> Result<bool, ExtensionError>;
    /// The canvas in straight RGBA8 after the last frame drawn.
    fn canvas(&self) -> &[u8];
}

/// A decoder at the first frame. It shares the file, so a restart copies nothing.
fn open_cursor(format: Format, bytes: &Arc<[u8]>) -> Result<Box<dyn FrameCursor>, ExtensionError> {
    let bytes = Arc::clone(bytes);
    match format {
        Format::Gif => Ok(Box::new(GifCursor::new(bytes)?)),
        Format::Apng => Ok(Box::new(ApngCursor::new(bytes)?)),
        Format::Webp => Ok(Box::new(WebpCursor::new(bytes)?)),
        other => Err(ExtensionError::Unsupported(format!(
            "{other:?} is not an animation"
        ))),
    }
}

/// GIF decoding to RGBA, with frame buffers capped at the largest canvas.
fn gif_options() -> gif::DecodeOptions {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    options.set_memory_limit(gif::MemoryLimit::Bytes(
        std::num::NonZeroU64::new(MAX_CANVAS_PIXELS * 4).unwrap_or(std::num::NonZeroU64::MIN),
    ));
    options
}

fn malformed(error: impl std::fmt::Display) -> ExtensionError {
    ExtensionError::Malformed(error.to_string())
}

struct GifCursor {
    decoder: gif::Decoder<Cursor<Arc<[u8]>>>,
    compositor: Compositor,
}

impl GifCursor {
    fn new(bytes: Arc<[u8]>) -> Result<Self, ExtensionError> {
        let decoder = gif_options()
            .read_info(Cursor::new(bytes))
            .map_err(malformed)?;
        let compositor = Compositor::new(u32::from(decoder.width()), u32::from(decoder.height()))?;
        Ok(Self {
            decoder,
            compositor,
        })
    }
}

impl FrameCursor for GifCursor {
    fn canvas(&self) -> &[u8] {
        self.compositor.canvas()
    }

    fn advance(&mut self) -> Result<bool, ExtensionError> {
        let Some(frame) = self.decoder.read_next_frame().map_err(malformed)? else {
            return Ok(false);
        };
        let partial = PartialFrame {
            x: u32::from(frame.left),
            y: u32::from(frame.top),
            width: u32::from(frame.width),
            height: u32::from(frame.height),
            pixels: frame.buffer.to_vec(),
            // A transparent index leaves the canvas as it is: GIF draws over.
            blend: Blend::Over,
            dispose: match frame.dispose {
                gif::DisposalMethod::Background => Dispose::Background,
                gif::DisposalMethod::Previous => Dispose::Previous,
                _ => Dispose::None,
            },
        };
        self.compositor.draw(&partial)?;
        Ok(true)
    }
}

struct ApngCursor {
    reader: png::Reader<Cursor<Arc<[u8]>>>,
    compositor: Compositor,
    /// The default image is not a frame and has not been skipped yet.
    skip_default: bool,
    first: bool,
    buffer: Vec<u8>,
}

impl ApngCursor {
    fn new(bytes: Arc<[u8]>) -> Result<Self, ExtensionError> {
        let mut decoder = png::Decoder::new(Cursor::new(bytes));
        decoder.set_transformations(
            png::Transformations::EXPAND
                | png::Transformations::STRIP_16
                | png::Transformations::ALPHA,
        );
        let reader = decoder.read_info().map_err(malformed)?;
        let info = reader.info();
        let compositor = Compositor::new(info.width, info.height)?;
        let skip_default = info.frame_control.is_none();
        let size = reader
            .output_buffer_size()
            .ok_or_else(|| ExtensionError::Budget("the PNG is too large".into()))?;
        Ok(Self {
            reader,
            compositor,
            skip_default,
            first: true,
            buffer: vec![0; size],
        })
    }
}

impl FrameCursor for ApngCursor {
    fn canvas(&self) -> &[u8] {
        self.compositor.canvas()
    }

    fn advance(&mut self) -> Result<bool, ExtensionError> {
        loop {
            let output = match self.reader.next_frame(&mut self.buffer) {
                Ok(output) => output,
                Err(png::DecodingError::Parameter(_)) => return Ok(false),
                Err(error) => return Err(malformed(error)),
            };
            if std::mem::take(&mut self.skip_default) {
                continue;
            }
            let control = self.reader.info().frame_control.ok_or_else(|| {
                ExtensionError::Malformed("an APNG frame has no frame control".into())
            })?;
            let pixels = crate::raster::to_rgba8(
                &self.buffer[..output.buffer_size()],
                output.color_type,
                output.width,
                output.height,
                output.line_size,
            )?;
            let first = std::mem::take(&mut self.first);
            let partial = PartialFrame {
                x: control.x_offset,
                y: control.y_offset,
                width: control.width,
                height: control.height,
                pixels,
                blend: match control.blend_op {
                    png::BlendOp::Over => Blend::Over,
                    png::BlendOp::Source => Blend::Source,
                },
                dispose: match control.dispose_op {
                    png::DisposeOp::None => Dispose::None,
                    png::DisposeOp::Background => Dispose::Background,
                    // The first frame has nothing before it to return to.
                    png::DisposeOp::Previous if first => Dispose::Background,
                    png::DisposeOp::Previous => Dispose::Previous,
                },
            };
            self.compositor.draw(&partial)?;
            return Ok(true);
        }
    }
}

/// An animated WebP composited as libwebp does; `image-webp` decodes each frame's picture.
struct WebpCursor {
    bytes: Arc<[u8]>,
    frames: Vec<WebpFrame>,
    next: usize,
    compositor: Compositor,
}

struct WebpLayout {
    width: u32,
    height: u32,
    /// Plays, 0 for forever.
    loop_count: Option<u32>,
    frames: Vec<WebpFrame>,
}

/// One `ANMF` chunk.
struct WebpFrame {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    duration_ms: u32,
    blend: Blend,
    dispose: Dispose,
    data: std::ops::Range<usize>,
}

fn riff_chunks(bytes: &[u8], mut at: usize, end: usize) -> Vec<([u8; 4], std::ops::Range<usize>)> {
    let mut chunks = Vec::new();
    while at + 8 <= end {
        let kind = [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]];
        let size = u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]])
            as usize;
        let start = at + 8;
        let Some(stop) = start.checked_add(size).filter(|stop| *stop <= end) else {
            break;
        };
        chunks.push((kind, start..stop));
        at = stop + (size & 1);
    }
    chunks
}

fn le24(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) | u32::from(bytes[1]) << 8 | u32::from(bytes[2]) << 16
}

impl WebpLayout {
    fn read(bytes: &[u8]) -> Result<Self, ExtensionError> {
        let end = bytes.len();
        let chunks = riff_chunks(bytes, 12, end);
        let header = chunks
            .iter()
            .find(|(kind, _)| kind == b"VP8X")
            .filter(|(_, range)| range.len() >= 10)
            .ok_or_else(|| ExtensionError::Malformed("the WebP has no extended header".into()))?;
        let width = le24(&bytes[header.1.start + 4..]) + 1;
        let height = le24(&bytes[header.1.start + 7..]) + 1;
        let loop_count = chunks
            .iter()
            .find(|(kind, range)| kind == b"ANIM" && range.len() >= 6)
            .map(|(_, range)| {
                u32::from(u16::from_le_bytes([
                    bytes[range.start + 4],
                    bytes[range.start + 5],
                ]))
            });
        let frames = chunks
            .iter()
            .filter(|(kind, range)| kind == b"ANMF" && range.len() >= 16)
            .map(|(_, range)| {
                let at = range.start;
                let flags = bytes[at + 15];
                WebpFrame {
                    x: le24(&bytes[at..]) * 2,
                    y: le24(&bytes[at + 3..]) * 2,
                    width: le24(&bytes[at + 6..]) + 1,
                    height: le24(&bytes[at + 9..]) + 1,
                    duration_ms: le24(&bytes[at + 12..]),
                    blend: if flags & 0x02 == 0 {
                        Blend::OverWebp
                    } else {
                        Blend::Source
                    },
                    dispose: if flags & 0x01 == 0 {
                        Dispose::None
                    } else {
                        Dispose::Background
                    },
                    data: at + 16..range.end,
                }
            })
            .collect();
        Ok(Self {
            width,
            height,
            loop_count,
            frames,
        })
    }

    /// Every frame must lie within the canvas (RFC 9649).
    fn index(&self) -> Result<Index, ExtensionError> {
        let mut index = Index::new(self.width, self.height)?;
        index.loop_count = self.loop_count;
        for frame in &self.frames {
            if frame.x + frame.width > self.width || frame.y + frame.height > self.height {
                return Err(ExtensionError::Malformed(format!(
                    "a {}x{} frame at ({}, {}) lies outside the {}x{} canvas",
                    frame.width, frame.height, frame.x, frame.y, self.width, self.height
                )));
            }
            let delay = Delay {
                ticks: i64::from(frame.duration_ms) * TICKS_PER_MS,
                ms: frame.duration_ms,
            };
            index.push(frame.width, frame.height, delay)?;
        }
        Ok(index)
    }
}

impl WebpCursor {
    fn new(bytes: Arc<[u8]>) -> Result<Self, ExtensionError> {
        let layout = WebpLayout::read(&bytes)?;
        let compositor = Compositor::new(layout.width, layout.height)?;
        Ok(Self {
            bytes,
            frames: layout.frames,
            next: 0,
            compositor,
        })
    }

    /// Decodes a frame's picture, rewrapped as a still WebP, to straight RGBA8.
    fn picture(&self, frame: &WebpFrame) -> Result<Vec<u8>, ExtensionError> {
        let chunks = riff_chunks(&self.bytes, frame.data.start, frame.data.end);
        let wanted = |kind: &[u8; 4]| chunks.iter().find(|(chunk, _)| chunk == kind);
        let alpha = wanted(b"ALPH");
        let lossy = wanted(b"VP8 ");
        let lossless = wanted(b"VP8L");
        let mut body = Vec::new();
        let mut push = |kind: &[u8; 4], payload: &[u8]| {
            body.extend_from_slice(kind);
            body.extend_from_slice(
                &u32::try_from(payload.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            body.extend_from_slice(payload);
            if payload.len() % 2 == 1 {
                body.push(0);
            }
        };
        match (lossless, lossy) {
            (Some((_, range)), _) => push(b"VP8L", &self.bytes[range.clone()]),
            (None, Some((_, range))) => {
                // A lossy picture with an alpha plane needs an extended header saying so.
                let mut header = vec![0_u8; 10];
                if alpha.is_some() {
                    header[0] = 0x10;
                }
                header[4..7].copy_from_slice(&(frame.width - 1).to_le_bytes()[..3]);
                header[7..10].copy_from_slice(&(frame.height - 1).to_le_bytes()[..3]);
                push(b"VP8X", &header);
                if let Some((_, range)) = alpha {
                    push(b"ALPH", &self.bytes[range.clone()]);
                }
                push(b"VP8 ", &self.bytes[range.clone()]);
            }
            (None, None) => {
                return Err(ExtensionError::Malformed(
                    "a WebP frame has no picture".into(),
                ));
            }
        }
        let mut file = b"RIFF".to_vec();
        file.extend_from_slice(
            &u32::try_from(body.len() + 4)
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        file.extend_from_slice(b"WEBP");
        file.extend_from_slice(&body);
        let mut decoder = image_webp::WebPDecoder::new(Cursor::new(file)).map_err(malformed)?;
        // A lossless bitstream carries its own size, which must match the frame rectangle.
        if decoder.dimensions() != (frame.width, frame.height) {
            let (width, height) = decoder.dimensions();
            return Err(ExtensionError::Malformed(format!(
                "a frame declares {}x{} and its picture is {width}x{height}",
                frame.width, frame.height
            )));
        }
        let mut buffer =
            vec![
                0;
                decoder
                    .output_buffer_size()
                    .ok_or_else(|| ExtensionError::Budget("a WebP frame is too large".into()))?
            ];
        decoder.read_image(&mut buffer).map_err(malformed)?;
        Ok(if decoder.has_alpha() {
            buffer
        } else {
            buffer
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|[red, green, blue]| [*red, *green, *blue, 255])
                .collect()
        })
    }
}

impl FrameCursor for WebpCursor {
    fn canvas(&self) -> &[u8] {
        self.compositor.canvas()
    }

    fn advance(&mut self) -> Result<bool, ExtensionError> {
        let Some(frame) = self.frames.get(self.next) else {
            return Ok(false);
        };
        let partial = PartialFrame {
            x: frame.x,
            y: frame.y,
            width: frame.width,
            height: frame.height,
            pixels: self.picture(frame)?,
            blend: frame.blend,
            dispose: frame.dispose,
        };
        self.next += 1;
        self.compositor.draw(&partial)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
