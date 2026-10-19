//! Sources an extension provider supplies: SVG, animated images, Lottie,
//! OpenEXR, and image sequences. Each is opened when an export first shows
//! it and asked for frames by source time; its frames enter the same upload
//! path as decoded video, as straight RGBA8 or, for linear-light sources,
//! premultiplied half-float RGBA. An image sequence reads each frame's file
//! when it shows that frame: it never holds its files, only the frames it
//! decoded lately.
//!
//! A source whose frames are decoded one after another (an animated image,
//! which has one decoder positioned on its next frame) is opened once per
//! reader, as a video file gets a decoder per reader: two clips, or two
//! composition nodes, showing it at unrelated times must not make each
//! other restart it from its first frame. Only the decoder is the
//! reader's: the first reader opens the source (reads the file and decodes
//! it once to check it), and the readers after it are made from it while
//! any of them is open, sharing the file and the frames kept between
//! requests within one frame budget. One drawn at any time
//! as cheaply (SVG, Lottie, OpenEXR, an image sequence, which reads each
//! frame's file) is shared by its readers. Each open source has its own
//! lock: shards drawing different sources draw at once, and turning a
//! frame into the bytes a decoder hands over happens outside every lock.
//!
//! What a provider reports it could not draw as asked (a missing font or
//! image, a feature it leaves out, samples it had to change, a delay it
//! normalized) is collected, each once per source, from the report of every
//! source it opens and from every frame it draws, and returned with the
//! export's result and in its failure report.
//!
//! What the open sources hold together is the export's
//! `providerSourceBytes` budget, counting what readers of one source share
//! once: past it, the sources neither the frame
//! being supplied nor the one before it showed are closed, least recently
//! shown first, and opened again if a later frame shows them. A source
//! opened again answers the same frames. What those two frames show stays
//! open even past the budget: the frame being supplied asks for its sources
//! one after another, so a source the previous frame showed may be one this
//! frame has yet to ask for, and closing it would open it again every frame.

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

use crate::decode::DecodeCursorKey;

/// Files a request supplies for its provider sources.
#[derive(Clone, Debug, Default)]
pub struct ProviderFiles {
    /// Each source's own file, by source ID (none for an image sequence).
    pub originals: HashMap<String, PathBuf>,
    /// Each image sequence's frame files in the project's order, by source
    /// ID; `None` where the sequence has a gap.
    pub frames: HashMap<String, Vec<Option<PathBuf>>>,
    /// The fonts an SVG's text may draw with: the export's fonts' bytes, read
    /// once when the export starts.
    pub fonts: Vec<Arc<[u8]>>,
    /// Files an SVG or Lottie source names outside itself (an `<image href>`,
    /// an image asset), by source ID and then by locator as the source
    /// writes it: what a browser page registers with `registerResource`.
    /// A locator not here is drawn as missing (`missing_image`).
    pub resources: HashMap<String, HashMap<String, PathBuf>>,
}

/// Refuses resources for a source that reads none: one that is not an SVG
/// or Lottie provider source of `project`, or is not in it at all.
///
/// # Errors
/// Returns an `INVALID_INPUT` message naming the source.
pub fn check_resources(
    project: &Project,
    resources: &HashMap<String, HashMap<String, PathBuf>>,
) -> Result<(), String> {
    let mut named = resources.keys().collect::<Vec<_>>();
    named.sort();
    for id in named {
        let reads_resources = project.sources.iter().any(|source| {
            source.id.as_str() == id
                && matches!(
                    source.provider,
                    Some(ProviderSource::Svg | ProviderSource::Lottie { .. })
                )
        });
        if !reads_resources {
            return Err(crate::protocol::code_tag(
                "INVALID_INPUT",
                format!(
                    "resources names {id}, which is not an SVG or Lottie source of the project: \
                     only those read files outside themselves"
                ),
            ));
        }
    }
    Ok(())
}

/// The provider sources of one export, opened on first use and shared by
/// every decode session.
pub struct ProviderMedia {
    registry: Registry,
    specs: HashMap<SourceId, ProviderSource>,
    files: ProviderFiles,
    /// The size a vector source is rasterized to fit: the output's.
    fit: (u32, u32),
    /// What the open sources may hold together, in bytes.
    budget: u64,
    open: Mutex<OpenSources>,
    diagnostics: Mutex<Diagnostics>,
}

/// The most distinct provider diagnostics an export keeps; past it they are
/// only counted.
pub const MAX_PROVIDER_DIAGNOSTICS: usize = 256;

/// What the provider sources reported, each distinct diagnostic once per
/// source, in the order first seen.
#[derive(Default)]
struct Diagnostics {
    seen: Vec<(SourceId, Diagnostic)>,
    /// Distinct diagnostics past [`MAX_PROVIDER_DIAGNOSTICS`].
    omitted: u64,
}

/// One open source: a provider source, and for one decoded in order, the
/// reader it is open for.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct OpenKey {
    source: SourceId,
    reader: Option<(ClipId, Option<NodeId>)>,
}

/// The sources open now, and what holding them has cost.
#[derive(Default)]
struct OpenSources {
    sources: HashMap<OpenKey, OpenSource>,
    /// Counts renders, to order sources by when they were last shown.
    clock: u64,
    /// The most the open sources held once a frame's render was answered.
    peak_bytes: u64,
    /// How many times a source closed for the budget was opened again.
    reopens: u64,
    /// Sources closed for the budget, to tell a reopen from a first open.
    closed: std::collections::HashSet<OpenKey>,
    /// The readers of each source decoded in order, while any is open.
    readers: HashMap<SourceId, Weak<Readers>>,
}

/// What makes another reader of a source decoded in order: one made when
/// its first reader opened, which renders nothing. Each open reader holds
/// it, so what they share lives while one of them is open.
#[derive(Default)]
struct Readers {
    /// Locked while the first reader opens: a reader opening meanwhile
    /// waits for it instead of decoding the file again.
    maker: Mutex<Option<Box<dyn VisualSource>>>,
}

struct OpenSource {
    slot: Arc<Slot>,
    /// The latest output frame that showed it.
    frame: u64,
    /// The clock at its latest render.
    used: u64,
}

/// A source, opened on its first render, under its own lock.
#[derive(Default)]
struct Slot {
    visual: Mutex<Option<Box<dyn VisualSource>>>,
    /// What it held after its latest render: what the budget counts,
    /// without waiting for a render in progress.
    resident: AtomicU64,
    /// Of `resident`, what it shares with the other readers of its source.
    shared: AtomicU64,
    /// For a source decoded in order, what makes its other readers.
    readers: Option<Arc<Readers>>,
}

impl ProviderMedia {
    /// The provider sources `project` renders, with the request's files,
    /// whose open sources hold at most `budget` bytes together beyond what
    /// the frame being supplied and the one before it show.
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

    /// The most the open sources held together once a frame's render was
    /// answered and the budget applied.
    pub fn peak_bytes(&self) -> u64 {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .peak_bytes
    }

    /// How many times a source closed for the budget was opened again.
    pub fn reopens(&self) -> u64 {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reopens
    }

    /// What the open sources hold together now.
    #[cfg(all(test, feature = "extensions"))]
    pub fn resident_bytes(&self) -> u64 {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .resident_bytes()
    }

    /// How many sources are open now.
    #[cfg(all(test, feature = "extensions"))]
    pub fn open_sources(&self) -> usize {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sources
            .len()
    }

    /// What the provider sources reported, as the protocol gives it:
    /// `[{source, kind, ...}]`, each distinct diagnostic once per source in
    /// the order first seen, and how many distinct ones past
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

    /// Records what `source` reported, each distinct diagnostic once.
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

    /// Whether an extension provider supplies `source`.
    pub fn supplies(&self, source: &SourceId) -> bool {
        self.specs.contains_key(source)
    }

    /// The frame `cursor`'s source shows its reader at `time`, for output
    /// frame `frame`, as a decoder would hand it over.
    ///
    /// # Errors
    /// Why the source cannot be opened or the frame produced.
    pub fn frame(
        &self,
        cursor: &DecodeCursorKey,
        time: MediaTime,
        frame: u64,
    ) -> Result<DecodedVideoFrame, String> {
        self.frame_watched(cursor, time, frame, None)
    }

    /// [`Self::frame`], for a decode shard that passes its `heartbeat`:
    /// opening and rendering are one watched operation, beaten as it starts
    /// and kept alive by every frame an animation decodes on the way
    /// ([`alpha2_extensions::watch_progress`]).
    /// Waiting for another shard's call to the same source is not this
    /// shard's operation, so the shard is idle while it waits; one stuck
    /// inside a provider is that shard's stall.
    ///
    /// # Errors
    /// Why the source cannot be opened or the frame produced.
    pub fn frame_watched(
        &self,
        cursor: &DecodeCursorKey,
        time: MediaTime,
        frame: u64,
        heartbeat: Option<&Arc<crate::heartbeat::WorkerHeartbeat>>,
    ) -> Result<DecodedVideoFrame, String> {
        let source = &cursor.0;
        let key = self.open_key(cursor)?;
        let slot = {
            let mut open = {
                let _waiting = heartbeat.map(crate::heartbeat::WorkerHeartbeat::idle);
                self.open.lock().unwrap_or_else(PoisonError::into_inner)
            };
            open.shown(key, frame)
        };
        let rendered = {
            let mut visual = {
                let _waiting = heartbeat.map(crate::heartbeat::WorkerHeartbeat::idle);
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
                // What the source is as opened, with its resources: a probe without them would
                // report every font and image missing.
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
                // The source may keep these pixels: the straight ones a decoder hands over are a
                // copy.
                let mut pixels = pixels.to_vec();
                unpremultiply(&mut pixels);
                (PixelFormat::Rgba8, pixels.into(), 4)
            }
            FramePixels::LinearF16(halves) => (PixelFormat::LinearRgbaF16, half_bytes(halves), 8),
            other => {
                return Err(format!(
                    "extension source {}: frames of {other:?}",
                    source.as_str()
                ));
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

    /// Which open source answers `cursor`: the source's own, or for one
    /// decoded in order, its reader's.
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

    /// A reader of `source`, a source decoded in order: made from what its
    /// open readers share, or when none is open, the source opened, with
    /// what makes the readers after it.
    fn open_reader(
        &self,
        source: &SourceId,
        readers: &Readers,
        heartbeat: Option<&Arc<crate::heartbeat::WorkerHeartbeat>>,
    ) -> Result<Box<dyn VisualSource>, ExtensionError> {
        let mut maker = {
            let _waiting = heartbeat.map(crate::heartbeat::WorkerHeartbeat::idle);
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
    /// What the open sources hold: each one's own bytes, and once for each
    /// source what its readers share.
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

    /// The slot of the source `key` names, shown by output frame `frame`:
    /// the open one, or a new one, opened on its first render.
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

    /// What makes the readers of `source`, shared with its open readers.
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

    /// Closes the least recently shown sources that neither `frame` nor the
    /// frame before it showed until what stays open holds at most `budget`,
    /// and records what it holds.
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
            // of a source frees what the others share only when it was the last.
            if self.sources.remove(&oldest).is_some() {
                held = self.resident_bytes();
            }
            self.closed.insert(oldest);
        }
        self.peak_bytes = self.peak_bytes.max(held);
    }
}

/// One provider diagnostic as the protocol gives it: its `kind` and fields,
/// and the `source` that reported it.
fn diagnostic_json(source: &SourceId, diagnostic: &Diagnostic) -> serde_json::Value {
    let mut value = serde_json::to_value(diagnostic).unwrap_or_else(
        |error| serde_json::json!({"kind": "unreported", "error": error.to_string()}),
    );
    if let Some(object) = value.as_object_mut() {
        object.insert("source".to_owned(), serde_json::json!(source.as_str()));
    }
    value
}

/// Whether `spec`'s frames are decoded one after another from its start,
/// so that a reader far from another's position restarts it.
const fn decodes_in_order(spec: &ProviderSource) -> bool {
    matches!(spec, ProviderSource::AnimatedImage)
}

/// A source's own file, for its provider to open. A file in a format its
/// provider reads whole (Lottie, dotLottie, SVG) is refused before it is
/// read when it is larger than the provider reads: from its size and its
/// first bytes, which tell a dotLottie package from a Lottie JSON file.
///
/// # Errors
/// [`ExtensionError::Budget`] for a file past its limit, or
/// [`ExtensionError::MissingResource`] when it cannot be read.
pub(crate) fn read_source(
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
    // A file that grows past its limit after its size was read is read only one byte past it,
    // which the provider refuses.
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

/// A sequence's frame locators, or an SVG's or Lottie file's resource
/// locators, mapped to the request's files, and the fonts.
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

/// An image sequence's frame files by locator: `paths` must list a file
/// wherever the project lists a frame, and none where it has a gap.
///
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

/// A provider's refusal of `source`, with the code its kind names: a
/// resource the request did not supply is `MISSING_INPUT`, and a file the
/// provider will not draw (unsupported, malformed, past a budget, not what
/// the project says) `INVALID_INPUT`, whatever its words.
fn refused(source: &SourceId, error: &ExtensionError) -> String {
    let code = match error {
        ExtensionError::MissingResource(_) => "MISSING_INPUT",
        _ => "INVALID_INPUT",
    };
    crate::protocol::code_tag(
        code,
        format!("extension source {}: {error}", source.as_str()),
    )
}

/// An OpenEXR frame's halves as the little-endian bytes a linear-light plane
/// holds. On a little-endian target they already are those bytes, so the
/// plane shares the source's buffer (a 4K frame is 66 MB the upload would
/// otherwise copy twice); only a big-endian one swaps them into a copy.
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

/// Halves read as their in-memory bytes: little-endian ones only
/// ([`half_bytes`]).
struct LittleEndianHalves(Arc<[u16]>);

impl AsRef<[u8]> for LittleEndianHalves {
    fn as_ref(&self) -> &[u8] {
        bytemuck::cast_slice(&self.0)
    }
}

/// Straight alpha from premultiplied RGBA8, rounding to nearest: the
/// renderer premultiplies it again, back to the same values.
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
