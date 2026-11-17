//! Sources an extension provider supplies: SVG, animated images, Lottie, `OpenEXR`, and image
//! sequences. Each opens when an export first shows it, and its frames enter the same upload path
//! as decoded video (straight RGBA8, or premultiplied half-float RGBA for linear light). An image
//! sequence reads each frame's file only when it shows that frame.
//!
//! An animated image has one decoder positioned on its next frame, so it is opened once per reader,
//! like a video file: readers at unrelated times must not restart each other. The first reader
//! opens the source and the rest are made from it, sharing the file and kept frames within one
//! budget. Sources drawn at any time as cheaply are shared by their readers. Each open source has
//! its own lock, and converting a frame happens outside every lock.
//!
//! Provider diagnostics are collected once per source for the host's report.
//!
//! The open sources share one byte budget. Past it, sources neither the current nor the previous
//! frame showed close, least recently shown first. Those two frames' sources stay
//! open even past the budget, because the current frame asks for its sources one at a time and
//! closing the previous frame's would reopen them every frame.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use alpha2_extensions::{
    Diagnostic, ExtensionError, FramePixels, Registry, ResourceResolver, SharedResolver,
    VisualSource, check_document_size, document_limit,
};
use alpha2_media::{
    ChromaLocation, DecodedVideoFrame, Orientation, PixelFormat, Plane, Rect, SampleAspectRatio,
    SourceId,
};
use alpha2_model::{ClipId, NodeId, Project, ProviderSource};
use alpha2_time::MediaTime;

use crate::cursor::DecodeCursorKey;

/// The files provider sources read, by source ID.
#[derive(Clone, Debug, Default)]
pub struct ProviderFiles {
    /// None for an image sequence.
    pub originals: HashMap<String, PathBuf>,
    /// In project order; `None` where the sequence has a gap.
    pub frames: HashMap<String, Vec<Option<PathBuf>>>,
    /// Read once when the export starts.
    pub fonts: Vec<Arc<[u8]>>,
    /// Files an SVG or Lottie source names outside itself, by source ID and then by locator, as a
    /// browser page registers them with `registerResource`. A missing locator draws as
    /// `missing_image`.
    pub resources: HashMap<String, HashMap<String, PathBuf>>,
}

/// What a provider call tells a watchdog: when it waits for another caller of the same source, and
/// when it starts work of its own.
pub trait ProviderWatch {
    /// Held while the caller waits.
    type Waiting;

    /// The caller waits until the returned guard drops.
    fn waiting(&self) -> Self::Waiting;

    /// The caller starts work.
    fn beat(&self);
}

/// No watchdog.
#[derive(Clone, Copy, Debug)]
pub struct Unwatched;

impl ProviderWatch for Unwatched {
    type Waiting = ();

    fn waiting(&self) {}

    fn beat(&self) {}
}

/// Why a provider source gave no frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderError {
    /// The error code the failure answers (`MISSING_INPUT`, `INVALID_INPUT`), when it is one.
    pub code: Option<&'static str>,
    /// What failed.
    pub message: String,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

impl From<String> for ProviderError {
    fn from(message: String) -> Self {
        Self {
            code: None,
            message,
        }
    }
}

/// Opened on first use and shared by every decode session.
pub struct ProviderMedia {
    registry: Registry,
    specs: HashMap<SourceId, ProviderSource>,
    files: ProviderFiles,
    /// Vector sources are rasterized to fit the output.
    fit: (u32, u32),
    budget: u64,
    open: Mutex<OpenSources>,
    diagnostics: Mutex<Diagnostics>,
}

/// Past this, distinct provider diagnostics are only counted.
pub const MAX_PROVIDER_DIAGNOSTICS: usize = 256;

#[derive(Default)]
struct Diagnostics {
    seen: Vec<(SourceId, Diagnostic)>,
    omitted: u64,
}

/// A provider source, and for one decoded in order, the reader it is open for.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct OpenKey {
    source: SourceId,
    reader: Option<(ClipId, Option<NodeId>)>,
}

#[derive(Default)]
struct OpenSources {
    sources: HashMap<OpenKey, OpenSource>,
    /// Counts renders, to order sources by when they were last shown.
    clock: u64,
    peak_bytes: u64,
    reopens: u64,
    /// Sources closed for the budget, to tell a reopen from a first open.
    closed: std::collections::HashSet<OpenKey>,
    readers: HashMap<SourceId, Weak<Readers>>,
}

/// Makes more readers of a source decoded in order. Each open reader holds it, so what they share
/// lives while one is open.
#[derive(Default)]
struct Readers {
    /// Locked while the first reader opens, so a concurrent reader waits instead of decoding the
    /// file again.
    maker: Mutex<Option<Box<dyn VisualSource>>>,
}

struct OpenSource {
    slot: Arc<Slot>,
    frame: u64,
    used: u64,
}

#[derive(Default)]
struct Slot {
    visual: Mutex<Option<Box<dyn VisualSource>>>,
    /// What the budget counts, readable without waiting for a render in progress.
    resident: AtomicU64,
    shared: AtomicU64,
    readers: Option<Arc<Readers>>,
}

impl ProviderMedia {
    /// The provider sources `project` uses, reading `files`, rasterizing vector ones to fit
    /// `fit`, with `budget` bytes for the open sources.
    #[must_use]
    pub fn new(project: &Project, files: ProviderFiles, fit: (u32, u32), budget: u64) -> Self {
        let specs = project
            .used_sources()
            .into_iter()
            .filter_map(|source| {
                source
                    .provider
                    .clone()
                    .map(|provider| (source.id.clone(), provider))
            })
            .collect();
        Self {
            registry: Registry::with_defaults(),
            specs,
            files,
            fit,
            budget,
            open: Mutex::new(OpenSources::default()),
            diagnostics: Mutex::default(),
        }
    }

    /// No provider sources.
    #[must_use]
    pub fn none() -> Arc<Self> {
        Arc::new(Self {
            registry: Registry::default(),
            specs: HashMap::new(),
            files: ProviderFiles::default(),
            fit: (0, 0),
            budget: u64::MAX,
            open: Mutex::new(OpenSources::default()),
            diagnostics: Mutex::default(),
        })
    }

    /// The most bytes the open sources held at once.
    pub fn peak_bytes(&self) -> u64 {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .peak_bytes
    }

    /// How many sources the budget closed that were shown again.
    pub fn reopens(&self) -> u64 {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reopens
    }

    /// The bytes the open sources hold now.
    pub fn resident_bytes(&self) -> u64 {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .resident_bytes()
    }

    /// How many sources are open.
    pub fn open_sources(&self) -> usize {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sources
            .len()
    }

    /// `[{source, kind, ...}]` in first-seen order, and how many past
    /// [`MAX_PROVIDER_DIAGNOSTICS`] were only counted.
    pub fn diagnostics_json(&self) -> (serde_json::Value, u64) {
        let diagnostics = self
            .diagnostics
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let list = diagnostics
            .seen
            .iter()
            .map(|(source, diagnostic)| diagnostic_json(source, diagnostic))
            .collect();
        (serde_json::Value::Array(list), diagnostics.omitted)
    }

    fn record<'a>(&self, source: &SourceId, reported: impl IntoIterator<Item = &'a Diagnostic>) {
        let mut diagnostics = self
            .diagnostics
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for diagnostic in reported {
            if diagnostics
                .seen
                .iter()
                .any(|(seen, known)| seen == source && known == diagnostic)
            {
                continue;
            }
            if diagnostics.seen.len() >= MAX_PROVIDER_DIAGNOSTICS {
                diagnostics.omitted += 1;
                continue;
            }
            alpha2_os::diagnostic!(
                "alpha2 native export: extension source {}: {}",
                source.as_str(),
                diagnostic_json(source, diagnostic)
            );
            diagnostics.seen.push((source.clone(), diagnostic.clone()));
        }
    }

    /// Whether `source` is a provider source.
    #[must_use]
    pub fn supplies(&self, source: &SourceId) -> bool {
        self.specs.contains_key(source)
    }

    /// # Errors
    /// Why the source cannot be opened or the frame produced.
    pub fn frame(
        &self,
        cursor: &DecodeCursorKey,
        time: MediaTime,
        frame: u64,
    ) -> Result<DecodedVideoFrame, ProviderError> {
        self.frame_watched(cursor, time, frame, None::<&Unwatched>)
    }

    /// Opening and rendering are one watched operation, kept alive by every frame an animation
    /// decodes ([`alpha2_extensions::watch_progress`]). Waiting for another shard's call to the
    /// same source leaves this shard idle, so only a call stuck inside a provider is its stall.
    ///
    /// # Errors
    /// Why the source cannot be opened or the frame produced.
    pub fn frame_watched<W: ProviderWatch>(
        &self,
        cursor: &DecodeCursorKey,
        time: MediaTime,
        frame: u64,
        heartbeat: Option<&W>,
    ) -> Result<DecodedVideoFrame, ProviderError> {
        let source = &cursor.0;
        let key = self.open_key(cursor)?;
        let slot = {
            let mut open = {
                let _waiting = heartbeat.map(ProviderWatch::waiting);
                self.open.lock().unwrap_or_else(PoisonError::into_inner)
            };
            open.shown(key, frame)
        };
        let rendered = {
            let mut visual = {
                let _waiting = heartbeat.map(ProviderWatch::waiting);
                slot.visual.lock().unwrap_or_else(PoisonError::into_inner)
            };
            if let Some(heartbeat) = heartbeat {
                heartbeat.beat();
            }
            if visual.is_none() {
                let opened = match &slot.readers {
                    Some(readers) => self.open_reader(source, readers, heartbeat),
                    None => self.open_source(source),
                }
                .map_err(|error| refused(source, &error))?;
                // A probe without the resources would report every font and image missing.
                self.record(source, &opened.report().diagnostics);
                *visual = Some(opened);
            }
            let visual = visual
                .as_mut()
                .ok_or_else(|| "an opened extension source disappeared".to_owned())?;
            let rendered = visual.render(time, self.fit);
            slot.resident.store(
                u64::try_from(visual.resident_bytes()).unwrap_or(u64::MAX),
                Ordering::Release,
            );
            slot.shared.store(
                u64::try_from(visual.shared_bytes()).unwrap_or(u64::MAX),
                Ordering::Release,
            );
            rendered
        };
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .within(self.budget, frame);
        let frame = rendered.map_err(|error| refused(source, &error))?;
        self.record(source, &frame.diagnostics);
        let (pixel_format, bytes, bytes_per_pixel) = match frame.pixels {
            FramePixels::Rgba8(pixels) => {
                // The source may keep these pixels, so the straight copy is a separate buffer.
                let mut pixels = pixels.to_vec();
                unpremultiply(&mut pixels);
                (PixelFormat::Rgba8, pixels.into(), 4)
            }
            FramePixels::LinearF16(halves) => (PixelFormat::LinearRgbaF16, half_bytes(halves), 8),
            other => {
                return Err(
                    format!("extension source {}: frames of {other:?}", source.as_str()).into(),
                );
            }
        };
        Ok(DecodedVideoFrame {
            pts: MediaTime::from_ticks(frame.start.ticks().max(0)),
            dts: None,
            duration: frame
                .end
                .map(|end| MediaTime::from_ticks(end.ticks() - frame.start.ticks().max(0))),
            visible_rect: Rect {
                x: 0,
                y: 0,
                width: frame.width,
                height: frame.height,
            },
            sample_aspect_ratio: SampleAspectRatio::default(),
            orientation: Orientation::UPRIGHT,
            pixel_format,
            chroma_location: ChromaLocation::Unspecified,
            color: None,
            planes: vec![Plane {
                bytes,
                stride: frame.width * bytes_per_pixel,
                width: frame.width,
                height: frame.height,
            }],
        })
    }

    fn open_key(&self, cursor: &DecodeCursorKey) -> Result<OpenKey, String> {
        let (source, clip, node) = cursor;
        let spec = self.specs.get(source).ok_or_else(|| {
            format!(
                "extension source {}: not an extension source",
                source.as_str()
            )
        })?;
        Ok(OpenKey {
            source: source.clone(),
            reader: decodes_in_order(spec).then(|| (clip.clone(), node.clone())),
        })
    }

    fn open_reader<W: ProviderWatch>(
        &self,
        source: &SourceId,
        readers: &Readers,
        heartbeat: Option<&W>,
    ) -> Result<Box<dyn VisualSource>, ExtensionError> {
        let mut maker = {
            let _waiting = heartbeat.map(ProviderWatch::waiting);
            readers.maker.lock().unwrap_or_else(PoisonError::into_inner)
        };
        if let Some(heartbeat) = heartbeat {
            heartbeat.beat();
        }
        if let Some(reader) = maker.as_ref().and_then(|maker| maker.reader()) {
            return Ok(reader);
        }
        let opened = self.open_source(source)?;
        *maker = opened.reader();
        Ok(opened)
    }

    fn open_source(&self, source: &SourceId) -> Result<Box<dyn VisualSource>, ExtensionError> {
        let spec = self
            .specs
            .get(source)
            .ok_or_else(|| ExtensionError::Unsupported("not an extension source".into()))?;
        let own = self
            .files
            .originals
            .get(source.as_str())
            .map(|path| read_source(path, spec).map(Arc::from))
            .transpose()?;
        let resolver: SharedResolver = Arc::new(FileResolver::new(
            spec,
            self.files.frames.get(source.as_str()),
            self.files.resources.get(source.as_str()),
            &self.files.fonts,
        )?);
        self.registry.open(spec, own, resolver)
    }
}

impl OpenSources {
    /// Shared bytes count once per source.
    fn resident_bytes(&self) -> u64 {
        let mut shared = HashMap::<&SourceId, u64>::new();
        let own = self
            .sources
            .iter()
            .map(|(key, open)| {
                let resident = open.slot.resident.load(Ordering::Acquire);
                let common = open.slot.shared.load(Ordering::Acquire).min(resident);
                if common > 0 {
                    let counted = shared.entry(&key.source).or_default();
                    *counted = (*counted).max(common);
                }
                resident - common
            })
            .fold(0, u64::saturating_add);
        shared.into_values().fold(own, u64::saturating_add)
    }

    fn shown(&mut self, key: OpenKey, frame: u64) -> Arc<Slot> {
        self.clock += 1;
        let clock = self.clock;
        if !self.sources.contains_key(&key) && self.closed.remove(&key) {
            self.reopens += 1;
        }
        let readers = (key.reader.is_some() && !self.sources.contains_key(&key))
            .then(|| self.readers(&key.source));
        let entry = self.sources.entry(key).or_insert_with(|| OpenSource {
            slot: Arc::new(Slot {
                readers,
                ..Slot::default()
            }),
            frame,
            used: 0,
        });
        entry.frame = entry.frame.max(frame);
        entry.used = clock;
        Arc::clone(&entry.slot)
    }

    fn readers(&mut self, source: &SourceId) -> Arc<Readers> {
        if let Some(readers) = self.readers.get(source).and_then(Weak::upgrade) {
            return readers;
        }
        self.readers.retain(|_, readers| readers.strong_count() > 0);
        let readers = Arc::new(Readers::default());
        self.readers
            .insert(source.clone(), Arc::downgrade(&readers));
        readers
    }

    fn within(&mut self, budget: u64, frame: u64) {
        let mut held = self.resident_bytes();
        while held > budget {
            let Some(oldest) = self
                .sources
                .iter()
                .filter(|(_, open)| open.frame.saturating_add(1) < frame)
                .min_by_key(|(_, open)| open.used)
                .map(|(source, _)| source.clone())
            else {
                break;
            };
            // A shard still drawing it keeps it alive until that render ends. Closing one reader
            // frees what the others share only when it was the last.
            if self.sources.remove(&oldest).is_some() {
                held = self.resident_bytes();
            }
            self.closed.insert(oldest);
        }
        self.peak_bytes = self.peak_bytes.max(held);
    }
}

fn diagnostic_json(source: &SourceId, diagnostic: &Diagnostic) -> serde_json::Value {
    let mut value = serde_json::to_value(diagnostic).unwrap_or_else(
        |error| serde_json::json!({"kind": "unreported", "error": error.to_string()}),
    );
    if let Some(object) = value.as_object_mut() {
        object.insert("source".to_owned(), serde_json::json!(source.as_str()));
    }
    value
}

const fn decodes_in_order(spec: &ProviderSource) -> bool {
    matches!(spec, ProviderSource::AnimatedImage)
}

/// Lottie, dotLottie and SVG files are read whole, so one larger than its provider reads is
/// refused from its size and first bytes before it is read.
///
/// # Errors
/// [`ExtensionError::Budget`] for a file past its limit, or
/// [`ExtensionError::MissingResource`] when it cannot be read.
pub fn read_source(
    path: &std::path::Path,
    spec: &ProviderSource,
) -> Result<Vec<u8>, ExtensionError> {
    use std::io::{Read as _, Seek as _};
    let unreadable = |error: std::io::Error| {
        ExtensionError::MissingResource(format!("{}: {error}", path.display()))
    };
    let mut file = std::fs::File::open(path).map_err(unreadable)?;
    let length = file.metadata().map_err(unreadable)?.len();
    let mut head = Vec::with_capacity(4);
    (&mut file)
        .take(4)
        .read_to_end(&mut head)
        .map_err(unreadable)?;
    check_document_size(spec, &head, length)?;
    // A file that grows after its size was read is read only one byte past the limit, which the
    // provider refuses.
    let most = document_limit(spec, &head).map_or(u64::MAX, |(limit, _)| limit as u64 + 1);
    file.rewind().map_err(unreadable)?;
    let mut bytes = Vec::with_capacity(usize::try_from(length.min(most)).unwrap_or(0));
    file.take(most)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    Ok(bytes)
}

fn read(path: &std::path::Path) -> Result<Arc<[u8]>, ExtensionError> {
    std::fs::read(path)
        .map(Arc::from)
        .map_err(|error| ExtensionError::MissingResource(format!("{}: {error}", path.display())))
}

struct FileResolver {
    files: HashMap<String, PathBuf>,
    fonts: Vec<Arc<[u8]>>,
}

impl FileResolver {
    fn new(
        spec: &ProviderSource,
        frames: Option<&Vec<Option<PathBuf>>>,
        resources: Option<&HashMap<String, PathBuf>>,
        fonts: &[Arc<[u8]>],
    ) -> Result<Self, ExtensionError> {
        let mut files = resources.cloned().unwrap_or_default();
        if let ProviderSource::ImageSequence {
            frames: locators, ..
        } = spec
        {
            let paths = frames.ok_or_else(|| {
                ExtensionError::MissingResource("the sequence's frame files".into())
            })?;
            files = frame_files(locators, paths).map_err(ExtensionError::Mismatch)?;
        }
        Ok(Self {
            files,
            fonts: if matches!(spec, ProviderSource::Svg) {
                fonts.to_vec()
            } else {
                Vec::new()
            },
        })
    }
}

impl ResourceResolver for FileResolver {
    fn bytes(&self, locator: &str) -> Result<Arc<[u8]>, ExtensionError> {
        let path = self
            .files
            .get(locator)
            .ok_or_else(|| ExtensionError::MissingResource(locator.to_owned()))?;
        read(path)
    }

    fn fonts(&self) -> Vec<Arc<[u8]>> {
        self.fonts.clone()
    }
}

/// # Errors
/// Where the request's files and the project's frames disagree.
pub fn frame_files(
    locators: &[Option<String>],
    paths: &[Option<PathBuf>],
) -> Result<HashMap<String, PathBuf>, String> {
    if paths.len() != locators.len() {
        return Err(format!(
            "the request lists {} frame files for a sequence of {}",
            paths.len(),
            locators.len()
        ));
    }
    let mut files = HashMap::new();
    for (index, (locator, path)) in locators.iter().zip(paths).enumerate() {
        match (locator, path) {
            (Some(locator), Some(path)) => {
                files.insert(locator.clone(), path.clone());
            }
            (None, None) => {}
            (Some(_), None) => {
                return Err(format!(
                    "the request has no file for frame {index}, which the sequence lists"
                ));
            }
            (None, Some(_)) => {
                return Err(format!(
                    "the request has a file for frame {index}, a gap in the sequence"
                ));
            }
        }
    }
    Ok(files)
}

/// A missing resource is `MISSING_INPUT`; a file the provider will not draw is `INVALID_INPUT`.
fn refused(source: &SourceId, error: &ExtensionError) -> ProviderError {
    let code = match error {
        ExtensionError::MissingResource(_) => "MISSING_INPUT",
        _ => "INVALID_INPUT",
    };
    ProviderError {
        code: Some(code),
        message: format!("extension source {}: {error}", source.as_str()),
    }
}

/// On a little-endian target the halves already are the plane's bytes, so the plane shares the
/// buffer (a 4K frame is 66 MB the upload would otherwise copy twice).
fn half_bytes(halves: Arc<[u16]>) -> bytes::Bytes {
    if cfg!(target_endian = "little") {
        bytes::Bytes::from_owner(LittleEndianHalves(halves))
    } else {
        halves
            .iter()
            .flat_map(|half| half.to_le_bytes())
            .collect::<Vec<u8>>()
            .into()
    }
}

struct LittleEndianHalves(Arc<[u16]>);

impl AsRef<[u8]> for LittleEndianHalves {
    fn as_ref(&self) -> &[u8] {
        bytemuck::cast_slice(&self.0)
    }
}

/// Rounds to nearest, so the renderer's premultiply returns the same values.
fn unpremultiply(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let alpha = u16::from(pixel[3]);
        if alpha == 255 {
            continue;
        }
        if alpha == 0 {
            *pixel = [0, 0, 0, 0];
            continue;
        }
        for channel in &mut pixel[..3] {
            *channel = u8::try_from(((u16::from(*channel) * 255 + alpha / 2) / alpha).min(255))
                .unwrap_or(255);
        }
    }
}
