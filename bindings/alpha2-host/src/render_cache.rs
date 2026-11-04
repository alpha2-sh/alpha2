//! Baked export frames on disk: the YUV420 planes the encoder received, found again by a later
//! export of the same snapshot instead of rendered.
//!
//! A frame's key is a SHA-256 over everything its planes depend on: the host binary, the `FFmpeg`
//! runtime, the GPU, the project, each file it reads (path, size, modification time), effects,
//! fonts, the output size, the YUV encoding (chroma siting differs by codec), and the frame's
//! time and rate. A stored frame is bit for bit what the encoder received, and its header repeats
//! the size and encoding so a mismatched frame is never taken.
//!
//! Layout: `v2/<snapshot>/<shard>/<key>.frame` (or `<key>.pinned.frame`), sharded by the key's
//! first two hex digits. The cache counts and deletes only entries it names this way, never
//! following a symbolic link; anything else in the directory is left alone.
//!
//! Each frame is written under a temporary name and renamed into place, with its planes' SHA-256
//! in its header, so a reader never sees a partial frame and a corrupt one is deleted and rendered
//! again. Files are not synced: the checksum is what a frame is trusted by, and a sync per frame
//! costs more than rendering it. Reading a frame touches its modification time for eviction.
//!
//! [`READER_THREADS`] readers read, inflate and check frames ahead of the export (at most
//! [`READ_AHEAD_FRAMES`]), so the export thread makes no filesystem call; results are taken in
//! request order. A reader also makes the snapshot directory.
//!
//! The quota holds while an export runs: room is made by deleting other snapshots' frames, unpinned
//! before pinned, least recently accessed first; when none is left the frame and every later one
//! are not written. An export never deletes its own snapshot's frames, which it and its retries
//! read. Concurrent exports are seen by walking the directory again after each sixteenth of the
//! quota written.
//!
//! A writer thread compresses, hashes and writes, holding at most [`BACKLOG_FRAMES`]; when it is
//! full the request's [`Backlog`] either skips the frame or waits. An export that ends, even by
//! failing, lets the writer store what it holds.
//!
//! The writer and readers are watched ([`crate::heartbeat`]). One inside a step longer than
//! [`crate::config::configured_stall_timeout`] is stuck in a dead filesystem: it is left running,
//! the export continues without the cache, the result says the cache was abandoned, and this
//! process never uses the directory again. A cache problem never fails or hangs an export.
//!
//! A baked frame is only the finished picture, not encoder, muxer, audio DSP or temporal effect
//! state, so an export still encodes and mixes from its start and its output matches a live
//! render's.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

#[cfg(test)]
use crate::heartbeat::InjectedStall;
use crate::heartbeat::{CommandProgress, Joined, STALL_POLL, WorkerHeartbeat, join_or_leave};
use alpha2_media::{
    CancelToken, ChromaLocation, ColorMatrix, ColorPrimaries, ColorRange, DynamicRange,
    TransferFunction, YuvEncoding,
};

use alpha2_time::{FrameRate, MediaTime};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MAGIC: &[u8; 4] = b"A2RC";
/// A new frame format alone keeps it: older frames stay counted and evicted by the quota, and are
/// never taken because their header names another format.
const LAYOUT_VERSION: u32 = 2;
const FRAME_VERSION: u32 = 3;
/// Magic, frame version, width, height, encoding, planes' length, planes' SHA-256.
const HEADER_BYTES: usize = 4 + 4 + 4 + 4 + ENCODING_BYTES + 8 + 32;
/// [`encoding_bytes`], padded.
const ENCODING_BYTES: usize = 8;
/// A temporary file this old belongs to a writer that is gone.
const STALE_TEMPORARY: Duration = Duration::from_hours(1);
pub const DEFAULT_QUOTA_BYTES: u64 = 4 << 30;
/// Includes the frame being stored. Two frames of output planes plus one compressed copy, beyond
/// what the export's memory budgets bound.
pub const BACKLOG_FRAMES: usize = 2;
/// An export taking every frame from the cache consumes them faster than one thread can read,
/// inflate and checksum.
pub const READER_THREADS: usize = 2;
/// Including the one the export waits for; output planes beyond the export's memory budgets.
pub const READ_AHEAD_FRAMES: usize = 2 * READER_THREADS;
/// The export rescans the directory after writing this fraction of the quota, to see what other
/// exports wrote and deleted.
const RESCAN_FRACTION: u64 = 16;

const OPEN: u8 = 0;
const FULL: u8 = 1;
const FAILED: u8 = 2;
/// The writer or the reader was left stuck in the filesystem.
const ABANDONED: u8 = 3;

static TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// None of these is used again by this process.
static ABANDONED_DIRECTORIES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn abandoned_directories() -> std::sync::MutexGuard<'static, Vec<PathBuf>> {
    ABANDONED_DIRECTORIES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
thread_local! {
    /// Blocks the next cache's writer where the stall says, watched with this limit instead of the
    /// configured one.
    pub static INJECT_WRITER_STALL: std::cell::Cell<Option<(InjectedStall, Duration)>> =
        const { std::cell::Cell::new(None) };
    /// The reader's first operation makes the directory; each later one reads a frame.
    pub static INJECT_READER_STALL: std::cell::Cell<Option<(InjectedStall, Duration)>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub static FRAMES_READ: Mutex<Vec<(PathBuf, Option<String>)>> = Mutex::new(Vec::new());

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backlog {
    /// Reported as `busySkips`.
    #[default]
    Skip,
    Wait,
}

impl Backlog {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "skip" => Some(Self::Skip),
            "wait" => Some(Self::Wait),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Wait => "wait",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderCacheRequest {
    pub directory: PathBuf,
    pub quota_bytes: u64,
    pub backlog: Backlog,
}

impl RenderCacheRequest {
    /// The only constructor, so the protocol and the CLI refuse the same values.
    ///
    /// # Errors
    /// An empty directory, which would bake into the working directory, or a zero quota.
    pub fn new(directory: PathBuf, quota_bytes: u64, backlog: Backlog) -> Result<Self, String> {
        if directory.as_os_str().is_empty() {
            return Err("renderCache.directory must be a non-empty path".to_owned());
        }
        if quota_bytes == 0 {
            return Err("renderCache.quotaBytes must be a positive integer".to_owned());
        }
        Ok(Self {
            directory,
            quota_bytes,
            backlog,
        })
    }

    /// # Errors
    /// Describes the first malformed field.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let object = value.as_object().ok_or_else(|| {
            format!(
                "renderCache must be an object {{ directory, quotaBytes?, backlog? }}, received {value}"
            )
        })?;
        for key in object.keys() {
            if !["directory", "quotaBytes", "backlog"].contains(&key.as_str()) {
                return Err(format!(
                    "renderCache has an unknown key \"{key}\"; it takes directory, quotaBytes, and backlog"
                ));
            }
        }
        let directory = object
            .get("directory")
            .and_then(Value::as_str)
            .ok_or_else(|| "renderCache.directory must be a non-empty path".to_owned())?;
        let quota_bytes = match object.get("quotaBytes") {
            None | Some(Value::Null) => DEFAULT_QUOTA_BYTES,
            Some(quota) => quota
                .as_u64()
                .ok_or_else(|| "renderCache.quotaBytes must be a positive integer".to_owned())?,
        };
        let backlog = match object.get("backlog") {
            None | Some(Value::Null) => Backlog::default(),
            Some(backlog) => backlog
                .as_str()
                .and_then(Backlog::parse)
                .ok_or_else(|| "renderCache.backlog must be \"skip\" or \"wait\"".to_owned())?,
        };
        Self::new(PathBuf::from(directory), quota_bytes, backlog)
    }
}

/// Everything an export's frames depend on besides their time, hashed.
#[derive(Default)]
pub struct SnapshotIdentity(Sha256);

impl SnapshotIdentity {
    /// Labels keep adjacent values apart.
    pub fn add(&mut self, label: &str, bytes: &[u8]) {
        self.0.update((label.len() as u64).to_le_bytes());
        self.0.update(label.as_bytes());
        self.0.update((bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
    }

    /// By length and modification time; a missing file is an identity too.
    pub fn add_file(&mut self, label: &str, path: &Path) {
        let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        self.add(label, canonical.to_string_lossy().as_bytes());
        match fs::metadata(&canonical) {
            Ok(metadata) => {
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map_or(0, |since| since.as_nanos());
                self.add("length", &metadata.len().to_le_bytes());
                self.add("modified", &modified.to_le_bytes());
            }
            Err(_) => self.add("missing", &[]),
        }
    }

    #[must_use]
    pub fn finish(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}

/// Keys the cache and is named in each frame's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameFormat {
    pub width: u32,
    pub height: u32,
    /// A frame packed for one codec's chroma siting is not another's.
    pub encoding: YuvEncoding,
}

/// Each value has a fixed code rather than its declaration order, so reordering a variant never
/// makes another export's frames look like this one's; a new variant must be given one.
fn encoding_bytes(encoding: YuvEncoding) -> [u8; ENCODING_BYTES] {
    let color = encoding.color;
    let primaries = match color.primaries {
        ColorPrimaries::Bt601 => 1,
        ColorPrimaries::Bt709 => 2,
        ColorPrimaries::Bt2020 => 3,
    };
    let transfer = match color.transfer {
        TransferFunction::Bt709 => 1,
        TransferFunction::Srgb => 2,
        TransferFunction::Pq => 3,
        TransferFunction::Hlg => 4,
    };
    let matrix = match color.matrix {
        ColorMatrix::Bt601 => 1,
        ColorMatrix::Bt709 => 2,
        ColorMatrix::Bt2020Ncl => 3,
        ColorMatrix::Rgb => 4,
    };
    let range = match color.range {
        ColorRange::Limited => 1,
        ColorRange::Full => 2,
    };
    let dynamic_range = match color.dynamic_range {
        DynamicRange::Sdr => 1,
        DynamicRange::Hdr => 2,
    };
    let chroma = match encoding.chroma {
        ChromaLocation::Unspecified => 1,
        ChromaLocation::Left => 2,
        ChromaLocation::Center => 3,
        ChromaLocation::TopLeft => 4,
    };
    [
        primaries,
        transfer,
        matrix,
        range,
        dynamic_range,
        chroma,
        0,
        0,
    ]
}

fn format_snapshot(snapshot: [u8; 32], format: FrameFormat) -> [u8; 32] {
    let mut identity = SnapshotIdentity::default();
    identity.add("snapshot", &snapshot);
    identity.add("width", &format.width.to_le_bytes());
    identity.add("height", &format.height.to_le_bytes());
    identity.add("encoding", &encoding_bytes(format.encoding));
    identity.finish()
}

/// Rows packed with no padding.
pub struct BakedFrame {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderCacheStats {
    pub hits: u64,
    pub writes: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub bypassed_frames: u64,
    pub corrupt: u64,
    pub write_errors: u64,
    pub evictions: u64,
    pub quota_skips: u64,
    /// The writer was full (`Backlog::Skip`).
    pub busy_skips: u64,
}

impl RenderCacheStats {
    fn add(&mut self, other: &Self) {
        self.hits += other.hits;
        self.writes += other.writes;
        self.bytes_read += other.bytes_read;
        self.bytes_written += other.bytes_written;
        self.bypassed_frames += other.bypassed_frames;
        self.corrupt += other.corrupt;
        self.write_errors += other.write_errors;
        self.evictions += other.evictions;
        self.quota_skips += other.quota_skips;
        self.busy_skips += other.busy_skips;
    }
}

#[derive(Clone)]
struct Layout {
    /// `<directory>/v2`.
    root: PathBuf,
    own: PathBuf,
    format: FrameFormat,
}

impl Layout {
    fn plane_sizes(&self) -> (usize, usize) {
        let (width, height) = (self.format.width as usize, self.format.height as usize);
        (width * height, width.div_ceil(2) * height.div_ceil(2))
    }

    fn path(&self, key: &str, pinned: bool) -> PathBuf {
        let suffix = if pinned { "pinned.frame" } else { "frame" };
        self.own.join(&key[..2]).join(format!("{key}.{suffix}"))
    }

    fn existing(&self, key: &str) -> Option<(PathBuf, bool)> {
        [false, true]
            .into_iter()
            .map(|pinned| (self.path(key, pinned), pinned))
            .find(|(path, _)| path.is_file())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

fn is_hex(name: &str, length: usize) -> bool {
    name.len() == length
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Entry {
    Frame {
        pinned: bool,
    },
    /// `<key>.<pid>.<n>.tmp`, a frame being written.
    Temporary,
}

impl Entry {
    /// `None` for a name the cache never writes there.
    fn parse(shard: &str, name: &str) -> Option<Self> {
        let key = name.get(..64).filter(|key| is_hex(key, 64))?;
        if !key.starts_with(shard) {
            return None;
        }
        match &name[64..] {
            ".frame" => Some(Self::Frame { pinned: false }),
            ".pinned.frame" => Some(Self::Frame { pinned: true }),
            rest => {
                let mut parts = rest.strip_prefix('.')?.strip_suffix(".tmp")?.split('.');
                let numbers = [parts.next()?, parts.next()?];
                (parts.next().is_none()
                    && numbers.iter().all(|number| {
                        !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
                    }))
                .then_some(Self::Temporary)
            }
        }
    }
}

/// Another snapshot's frame: pinned, last access, length, path.
type Evictable = (bool, SystemTime, u64, PathBuf);

#[derive(Default)]
struct Scan {
    total: u64,
    /// What this export may delete, unpinned and least recently accessed first.
    others: Vec<Evictable>,
    temporaries: Vec<(PathBuf, SystemTime)>,
    /// Shards first, to remove once emptied.
    directories: Vec<PathBuf>,
}

/// Never through a symbolic link.
fn named_directories(directory: &Path, named: impl Fn(&str) -> bool) -> Vec<(PathBuf, String)> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            named(&name).then(|| (entry.path(), name))
        })
        .collect()
}

/// Walks only the snapshots' shards and the frames and temporaries in them.
fn scan(layout: &Layout, progressed: &dyn Fn()) -> Scan {
    let mut scan = Scan::default();
    for (snapshot, _) in named_directories(&layout.root, |name| is_hex(name, 64)) {
        let own = snapshot == layout.own;
        for (shard, shard_name) in named_directories(&snapshot, |name| is_hex(name, 2)) {
            let Ok(files) = fs::read_dir(&shard) else {
                continue;
            };
            for file in files.flatten() {
                // Each entry keeps the walk alive, not stuck.
                progressed();
                let Ok(metadata) = file.metadata() else {
                    continue;
                };
                // `DirEntry::metadata` does not follow a symbolic link.
                if !metadata.is_file() {
                    continue;
                }
                let Some(entry) = file
                    .file_name()
                    .to_str()
                    .and_then(|name| Entry::parse(&shard_name, name))
                else {
                    continue;
                };
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                match entry {
                    Entry::Temporary => scan.temporaries.push((file.path(), modified)),
                    Entry::Frame { pinned } => {
                        scan.total = scan.total.saturating_add(metadata.len());
                        if !own {
                            scan.others
                                .push((pinned, modified, metadata.len(), file.path()));
                        }
                    }
                }
            }
            scan.directories.push(shard);
        }
        scan.directories.push(snapshot);
    }
    scan.others.sort_by_key(|entry| (entry.0, entry.1));
    scan
}

/// Returns whether its bytes left the directory (deleted now, or already gone).
fn delete_frame(path: &Path) -> Option<bool> {
    match fs::remove_file(path) {
        Ok(()) => Some(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

struct Bake {
    key: String,
    frame: BakedFrame,
    pinned: bool,
}

struct Writer {
    sender: mpsc::SyncSender<Bake>,
    thread: JoinHandle<Store>,
    heartbeat: Arc<WorkerHeartbeat>,
    /// A `Backlog::Wait` write waits on this, not in a blocking send.
    progress: Arc<CommandProgress>,
    /// The writer brings the directory within its quota once its queue closes.
    finishing: Arc<AtomicBool>,
}

enum Found {
    Missing,
    /// Failed its checks and was deleted, to be rendered again.
    Corrupt,
    Frame(BakedFrame, u64),
}

struct Reader {
    keys: mpsc::Sender<String>,
    found: mpsc::Receiver<(String, Found)>,
    threads: Vec<JoinHandle<()>>,
    heartbeats: Vec<Arc<WorkerHeartbeat>>,
    /// In request order, which is the order the export takes them in.
    asked: std::collections::VecDeque<String>,
    /// Results that came back before those of keys asked ahead of them.
    arrived: std::collections::HashMap<String, Found>,
}

impl Reader {
    fn stuck(&self, limit: Duration) -> bool {
        self.heartbeats
            .iter()
            .any(|beat| beat.stuck_past(limit).is_some())
    }
}

pub struct RenderCache {
    quota_bytes: u64,
    snapshot: [u8; 32],
    backlog: Backlog,
    /// What the export itself did; the writer counts what it did with the rest.
    stats: RenderCacheStats,
    state: Arc<AtomicU8>,
    in_flight: Arc<AtomicUsize>,
    writer: Option<Writer>,
    /// Published after each frame, so a writer left running is reported for what it did.
    writer_stats: Arc<Mutex<RenderCacheStats>>,
    reader: Option<Reader>,
    directory: PathBuf,
    /// Stops a `Backlog::Wait` write waiting for room, and a read waiting for its frame.
    cancel: CancelToken,
    stall_timeout: Duration,
    abandoned: bool,
}

fn abandon_directory(directory: &Path, what: &str) {
    let mut abandoned = abandoned_directories();
    if !abandoned.iter().any(|known| known == directory) {
        abandoned.push(directory.to_owned());
    }
    drop(abandoned);
    alpha2_os::diagnostic!(
        "alpha2 native render cache {what} left running: it is stuck in the filesystem of {}, which this process uses no more",
        directory.display()
    );
}

fn serve_reads(
    layout: &Layout,
    beat: &Arc<WorkerHeartbeat>,
    keys: &Mutex<mpsc::Receiver<String>>,
    answer: &mpsc::Sender<(String, Found)>,
) {
    loop {
        // Idle while it waits, not stuck: the lock is held only by a reader waiting for a key.
        let key = keys.lock().unwrap_or_else(PoisonError::into_inner).recv();
        let Ok(key) = key else {
            return;
        };
        let work = beat.begin_work();
        let found = read_frame(layout, &key);
        drop(work);
        if answer.send((key, found)).is_err() {
            return;
        }
    }
}

impl RenderCache {
    /// The snapshot directory, and so every key, includes `format`, so frames of another size or
    /// encoding are never taken. The directory is made on a watched reader.
    ///
    /// # Errors
    /// A directory that cannot be made (in time), a thread that cannot start, or a directory an
    /// earlier export of this process abandoned. The export then runs without the cache.
    pub fn open(
        request: &RenderCacheRequest,
        snapshot: [u8; 32],
        format: FrameFormat,
        cancel: &CancelToken,
    ) -> Result<Self, String> {
        // Not even looked at: a filesystem that stopped answering would hold this export too.
        if abandoned_directories().contains(&request.directory) {
            return Err(format!(
                "render cache directory {} is off for the rest of this process: an earlier export left a thread stuck in its filesystem",
                request.directory.display()
            ));
        }
        let snapshot = format_snapshot(snapshot, format);
        let root = request.directory.join(format!("v{LAYOUT_VERSION}"));
        let own = root.join(hex(&snapshot));
        let layout = Layout { root, own, format };
        #[cfg(test)]
        let (read_stall, write_stall) = (
            INJECT_READER_STALL.with(std::cell::Cell::take),
            INJECT_WRITER_STALL.with(std::cell::Cell::take),
        );
        #[cfg(test)]
        let heartbeat = |stall: Option<(InjectedStall, Duration)>| {
            Arc::new(WorkerHeartbeat::stalling(stall.map(|(stall, _)| stall)))
        };
        #[cfg(test)]
        let stall_timeout = [read_stall, write_stall]
            .into_iter()
            .flatten()
            .map(|(_, limit)| limit)
            .min()
            .unwrap_or_else(crate::config::configured_stall_timeout);
        #[cfg(not(test))]
        let (read_stall, write_stall) = ((), ());
        #[cfg(not(test))]
        let heartbeat = |(): ()| Arc::new(WorkerHeartbeat::default());
        #[cfg(not(test))]
        let stall_timeout = crate::config::configured_stall_timeout();
        let reader = Self::start_reader(
            &layout,
            (0..READER_THREADS).map(|_| heartbeat(read_stall)).collect(),
            stall_timeout,
            &request.directory,
        )?;
        let state = Arc::new(AtomicU8::new(OPEN));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let published = Arc::new(Mutex::new(RenderCacheStats::default()));
        let write_beat = heartbeat(write_stall);
        let store = Store {
            layout,
            quota_bytes: request.quota_bytes,
            state: Arc::clone(&state),
            stats: RenderCacheStats::default(),
            published: Arc::clone(&published),
            heartbeat: Arc::clone(&write_beat),
            stored_bytes: 0,
            written_since_scan: 0,
            evictable: std::collections::VecDeque::new(),
        };
        let writer = Self::start_writer(store, write_beat, Arc::clone(&in_flight));
        let mut cache = Self {
            quota_bytes: request.quota_bytes,
            snapshot,
            backlog: request.backlog,
            stats: RenderCacheStats::default(),
            state,
            in_flight,
            writer: None,
            writer_stats: published,
            reader: Some(reader),
            directory: request.directory.clone(),
            cancel: cancel.clone(),
            stall_timeout,
            abandoned: false,
        };
        // Dropping a cache whose writer did not start joins its readers.
        cache.writer = Some(writer?);
        Ok(cache)
    }

    fn start_writer(
        mut store: Store,
        heartbeat: Arc<WorkerHeartbeat>,
        in_flight: Arc<AtomicUsize>,
    ) -> Result<Writer, String> {
        let progress = Arc::new(CommandProgress::default());
        let finishing = Arc::new(AtomicBool::new(false));
        // The writer takes the frame it stores out of the queue, so the queue holds one fewer.
        let (sender, receiver) = mpsc::sync_channel::<Bake>(BACKLOG_FRAMES - 1);
        let (beat, taken, closing) = (
            Arc::clone(&heartbeat),
            Arc::clone(&progress),
            Arc::clone(&finishing),
        );
        let thread = std::thread::Builder::new()
            .name("alpha2-render-cache".to_owned())
            .spawn(move || {
                let work = beat.begin_work();
                store.rescan();
                drop(work);
                for bake in receiver {
                    taken.took_one();
                    let work = beat.begin_work();
                    store.write(&bake.key, &bake.frame, bake.pinned);
                    store.publish();
                    drop(work);
                    in_flight.fetch_sub(1, Ordering::Release);
                }
                if closing.load(Ordering::Acquire) {
                    let _work = beat.begin_close();
                    store.evict();
                    store.publish();
                }
                store
            })
            .map_err(|error| format!("render cache writer cannot be started: {error}"))?;
        Ok(Writer {
            sender,
            thread,
            heartbeat,
            progress,
            finishing,
        })
    }

    /// The first reader makes the snapshot directory and the others start once it has. Waits for
    /// that, or until the first is stuck past `stall_timeout`.
    fn start_reader(
        layout: &Layout,
        heartbeats: Vec<Arc<WorkerHeartbeat>>,
        stall_timeout: Duration,
        directory: &Path,
    ) -> Result<Reader, String> {
        let (keys, queue) = mpsc::channel::<String>();
        let queue = Arc::new(Mutex::new(queue));
        let (answer, found) = mpsc::channel::<(String, Found)>();
        let (made, opened) = mpsc::sync_channel::<Result<(), String>>(1);
        let spawn = |beat: &Arc<WorkerHeartbeat>, make: Option<mpsc::SyncSender<_>>| {
            let (layout, beat, queue, answer) = (
                layout.clone(),
                Arc::clone(beat),
                Arc::clone(&queue),
                answer.clone(),
            );
            std::thread::Builder::new()
                .name("alpha2-render-cache-read".to_owned())
                .spawn(move || {
                    if let Some(made) = make {
                        let work = beat.begin_work();
                        let created = fs::create_dir_all(&layout.own).map_err(|error| {
                            format!(
                                "render cache directory {} cannot be created: {error}",
                                layout.own.display()
                            )
                        });
                        drop(work);
                        let created_ok = created.is_ok();
                        if made.send(created).is_err() || !created_ok {
                            return;
                        }
                    }
                    serve_reads(&layout, &beat, &queue, &answer);
                })
                .map_err(|error| format!("render cache reader cannot be started: {error}"))
        };
        let mut heartbeats = heartbeats.into_iter();
        let first_beat = heartbeats
            .next()
            .ok_or("render cache reader has no thread")?;
        let first = spawn(&first_beat, Some(made))?;
        loop {
            match opened.recv_timeout(STALL_POLL) {
                Ok(Ok(())) => break,
                Ok(Err(reason)) => {
                    let _ = first.join();
                    return Err(reason);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if first_beat.stuck_past(stall_timeout).is_some() {
                        drop(first);
                        abandon_directory(directory, "reader");
                        return Err(format!(
                            "render cache directory {} is off for the rest of this process: making it did not return within {} ms, its filesystem stopped answering",
                            directory.display(),
                            stall_timeout.as_millis()
                        ));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("render cache reader stopped before it made the directory".into());
                }
            }
        }
        let mut threads = vec![first];
        let mut beats = vec![first_beat];
        for beat in heartbeats {
            if let Ok(thread) = spawn(&beat, None) {
                threads.push(thread);
                beats.push(beat);
            }
        }
        Ok(Reader {
            keys,
            found,
            threads,
            heartbeats: beats,
            asked: std::collections::VecDeque::new(),
            arrived: std::collections::HashMap::new(),
        })
    }

    #[must_use]
    pub fn key(&self, time: MediaTime, rate: FrameRate) -> String {
        let mut hash = Sha256::new();
        hash.update(self.snapshot);
        hash.update(time.ticks().to_le_bytes());
        hash.update(rate.numerator().to_le_bytes());
        hash.update(rate.denominator().to_le_bytes());
        hex(&hash.finalize())
    }

    pub fn bypass(&mut self) {
        self.stats.bypassed_frames += 1;
    }

    /// Nothing is asked twice, nor once the readers hold or read [`READ_AHEAD_FRAMES`].
    pub fn request(&mut self, key: &str) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        if reader.asked.len() >= READ_AHEAD_FRAMES || reader.asked.iter().any(|asked| asked == key)
        {
            return;
        }
        if reader.keys.send(key.to_owned()).is_ok() {
            reader.asked.push_back(key.to_owned());
        }
    }

    /// A frame asked for before `key` and not taken is let go. The wait ends early, as a miss,
    /// when the export is cancelled, a reader is stuck (abandoning the cache), or one panicked.
    pub fn read(&mut self, key: &str) -> Option<BakedFrame> {
        let reader = self.reader.as_mut()?;
        if !reader.asked.iter().any(|asked| asked == key) {
            if reader.keys.send(key.to_owned()).is_err() {
                self.join_reader();
                return None;
            }
            reader.asked.push_back(key.to_owned());
        }
        loop {
            // Results are taken in the order asked, whichever reader finished first.
            while let Some(found) = reader
                .asked
                .front()
                .and_then(|front| reader.arrived.remove(front))
            {
                let wanted = reader.asked.pop_front().is_some_and(|front| front == key);
                match found {
                    Found::Corrupt => self.stats.corrupt += 1,
                    Found::Frame(frame, bytes) if wanted => {
                        self.stats.hits += 1;
                        self.stats.bytes_read += bytes;
                        return Some(frame);
                    }
                    Found::Missing | Found::Frame(..) => {}
                }
                if wanted {
                    return None;
                }
            }
            match reader.found.recv_timeout(STALL_POLL) {
                Ok((found_key, found)) => {
                    reader.arrived.insert(found_key, found);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.cancel.is_cancelled() {
                        return None;
                    }
                    if reader.stuck(self.stall_timeout) {
                        self.abandon("reader");
                        return None;
                    }
                    // Readers end only when their queue closes, so one that ended panicked.
                    if reader.threads.iter().any(JoinHandle::is_finished) {
                        self.join_reader();
                        return None;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.join_reader();
                    return None;
                }
            }
        }
    }

    /// A frame that is not to be baked is counted here, so the export need not pack its planes.
    pub fn accepts(&mut self) -> bool {
        match self.state.load(Ordering::Acquire) {
            OPEN => {}
            FULL => {
                self.stats.quota_skips += 1;
                return false;
            }
            _ => return false,
        }
        if self.backlog == Backlog::Skip && self.in_flight.load(Ordering::Acquire) >= BACKLOG_FRAMES
        {
            self.stats.busy_skips += 1;
            return false;
        }
        true
    }

    /// A failed write is counted and stops further writes; it never fails the export.
    ///
    /// The wait for room is not a blocking send: it ends when the writer takes a frame, and checks
    /// for the export's cancel and for a stuck writer as it goes.
    pub fn write(&mut self, key: String, frame: BakedFrame, pinned: bool) {
        let Some(writer) = self.writer.as_ref() else {
            return;
        };
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        let mut bake = Bake { key, frame, pinned };
        loop {
            // Read before trying, so a frame the writer takes in between still ends the wait.
            let seen = writer.progress.current();
            match writer.sender.try_send(bake) {
                Ok(()) => return,
                Err(TrySendError::Full(back)) => bake = back,
                Err(TrySendError::Disconnected(_)) => {
                    self.in_flight.fetch_sub(1, Ordering::AcqRel);
                    self.stats.write_errors += 1;
                    return;
                }
            }
            if self.cancel.is_cancelled() {
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                return;
            }
            if writer.heartbeat.stuck_past(self.stall_timeout).is_some() {
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                self.abandon("writer");
                return;
            }
            writer.progress.wait_past(seen, STALL_POLL);
        }
    }

    fn abandon(&mut self, stuck: &str) {
        // Neither is joined: each keeps what it holds, and the one not stuck ends once its queue
        // closes.
        if let Some(Writer { sender, thread, .. }) = self.writer.take() {
            drop(sender);
            drop(thread);
        }
        if let Some(Reader { keys, threads, .. }) = self.reader.take() {
            drop(keys);
            drop(threads);
        }
        self.state.store(ABANDONED, Ordering::Release);
        self.abandoned = true;
        abandon_directory(&self.directory, stuck);
    }

    /// A writer or reader stuck in the filesystem is left running and reported as `abandoned`.
    #[must_use]
    pub fn finish(mut self) -> Value {
        if let Some(writer) = self.writer.as_ref() {
            writer.finishing.store(true, Ordering::Release);
        }
        self.join_reader();
        let mut stats = self.stats;
        match self.join() {
            Some(Joined::Finished(store)) => stats.add(&store.stats),
            Some(Joined::Panicked) => stats.write_errors += 1,
            Some(Joined::Left) | None => {
                stats.add(
                    &self
                        .writer_stats
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner),
                );
            }
        }
        json!({
            "status": "used",
            "hits": stats.hits,
            "writes": stats.writes,
            "bytesRead": stats.bytes_read,
            "bytesWritten": stats.bytes_written,
            "bypassedFrames": stats.bypassed_frames,
            "corrupt": stats.corrupt,
            "writeErrors": stats.write_errors,
            "evictions": stats.evictions,
            "quotaSkips": stats.quota_skips,
            "busySkips": stats.busy_skips,
            "quotaBytes": self.quota_bytes,
            "backlog": self.backlog.as_str(),
            "abandoned": self.abandoned,
        })
    }

    #[cfg(test)]
    pub fn held(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub fn asked(&self) -> usize {
        self.reader.as_ref().map_or(0, |reader| reader.asked.len())
    }

    #[cfg(test)]
    pub fn settle(&self) {
        while self.in_flight.load(Ordering::Acquire) > 0 {
            std::thread::yield_now();
        }
    }

    /// Counts the corrupt frames found among those the export did not take, or leaves the readers
    /// running once one is stuck.
    fn join_reader(&mut self) {
        let Some(Reader {
            keys,
            found,
            threads,
            heartbeats,
            arrived,
            ..
        }) = self.reader.take()
        else {
            return;
        };
        drop(keys);
        let mut left = false;
        for thread in threads {
            left |= matches!(
                join_or_leave(thread, &heartbeats, self.stall_timeout, None),
                Joined::Left
            );
        }
        if left {
            self.abandon("reader");
            return;
        }
        let corrupt = arrived
            .into_values()
            .chain(found.try_iter().map(|(_, found)| found))
            .filter(|found| matches!(found, Found::Corrupt))
            .count();
        self.stats.corrupt += corrupt as u64;
    }

    /// `None` if it was gone already.
    fn join(&mut self) -> Option<Joined<Store>> {
        let Writer {
            sender,
            thread,
            heartbeat,
            ..
        } = self.writer.take()?;
        drop(sender);
        let joined = join_or_leave(
            thread,
            std::slice::from_ref(&heartbeat),
            self.stall_timeout,
            None,
        );
        if matches!(joined, Joined::Left) {
            self.abandon("writer");
        }
        Some(joined)
    }
}

/// Deletes a frame that fails its checks and touches a good one's modification time.
fn read_frame(layout: &Layout, key: &str) -> Found {
    let Some((path, _)) = layout.existing(key) else {
        return Found::Missing;
    };
    #[cfg(test)]
    FRAMES_READ
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((
            path.clone(),
            std::thread::current().name().map(str::to_owned),
        ));
    let Ok((frame, bytes)) = read_file(layout, &path) else {
        let _ = fs::remove_file(&path);
        return Found::Corrupt;
    };
    if let Ok(file) = File::options().write(true).open(&path) {
        let _ = file.set_modified(SystemTime::now());
    }
    Found::Frame(frame, bytes)
}

fn read_file(layout: &Layout, path: &Path) -> Result<(BakedFrame, u64), String> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|mut file| file.read_to_end(&mut bytes))
        .map_err(|error| error.to_string())?;
    let header = bytes.get(..HEADER_BYTES).ok_or("short header")?;
    let word = |at: usize| u32::from_le_bytes(header[at..at + 4].try_into().expect("4 bytes"));
    let format = layout.format;
    let encoding_at = 16;
    let length_at = encoding_at + ENCODING_BYTES;
    let digest_at = length_at + 8;
    if &header[..4] != MAGIC
        || word(4) != FRAME_VERSION
        || word(8) != format.width
        || word(12) != format.height
        || header[encoding_at..length_at] != encoding_bytes(format.encoding)
    {
        return Err("unexpected header".to_owned());
    }
    let length = u64::from_le_bytes(header[length_at..digest_at].try_into().expect("8 bytes"));
    let (luma, chroma) = layout.plane_sizes();
    if length != (luma + 2 * chroma) as u64 {
        return Err("unexpected length".to_owned());
    }
    let mut planes = Vec::with_capacity(luma + 2 * chroma);
    flate2::read::DeflateDecoder::new(&bytes[HEADER_BYTES..])
        .read_to_end(&mut planes)
        .map_err(|error| error.to_string())?;
    if planes.len() != luma + 2 * chroma
        || Sha256::digest(&planes)[..] != header[digest_at..HEADER_BYTES]
    {
        return Err("checksum mismatch".to_owned());
    }
    let v = planes.split_off(luma + chroma);
    let u = planes.split_off(luma);
    Ok((BakedFrame { y: planes, u, v }, bytes.len() as u64))
}

impl Drop for RenderCache {
    /// An export that ends without [`RenderCache::finish`] still stores the frames it handed the
    /// writer, since its retry takes them.
    fn drop(&mut self) {
        self.join_reader();
        let _ = self.join();
    }
}

struct Store {
    layout: Layout,
    quota_bytes: u64,
    state: Arc<AtomicU8>,
    stats: RenderCacheStats,
    published: Arc<Mutex<RenderCacheStats>>,
    heartbeat: Arc<WorkerHeartbeat>,
    /// As the last walk found them, plus what this export wrote and deleted since.
    stored_bytes: u64,
    written_since_scan: u64,
    /// Other snapshots' frames this export may delete, unpinned and least recently accessed first.
    evictable: std::collections::VecDeque<(PathBuf, u64)>,
}

impl Store {
    fn write(&mut self, key: &str, frame: &BakedFrame, pinned: bool) {
        match self.state.load(Ordering::Acquire) {
            OPEN => {}
            FULL => {
                self.stats.quota_skips += 1;
                return;
            }
            _ => return,
        }
        if self.layout.existing(key).is_some() {
            return;
        }
        let Ok(encoded) = self.encode(frame) else {
            self.fail();
            return;
        };
        let bytes = encoded.len() as u64;
        // Another export may be writing into the same directory.
        if self.written_since_scan >= (self.quota_bytes / RESCAN_FRACTION).max(1) {
            self.rescan();
        }
        // Once a frame finds no room, later ones are not compressed only to be dropped.
        self.make_room(bytes);
        if self.stored_bytes.saturating_add(bytes) > self.quota_bytes {
            self.stats.quota_skips += 1;
            self.state.store(FULL, Ordering::Release);
            return;
        }
        if self.store(key, &encoded, pinned).is_ok() {
            self.stats.writes += 1;
            self.stats.bytes_written += bytes;
            self.stored_bytes += bytes;
            self.written_since_scan += bytes;
        } else {
            self.fail();
        }
    }

    fn fail(&mut self) {
        self.stats.write_errors += 1;
        self.state.store(FAILED, Ordering::Release);
    }

    fn publish(&self) {
        *self
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = self.stats;
    }

    fn rescan(&mut self) {
        let scan = scan(&self.layout, &|| self.heartbeat.progressed());
        self.stored_bytes = scan.total;
        self.written_since_scan = 0;
        self.evictable = scan
            .others
            .into_iter()
            .map(|(_, _, length, path)| (path, length))
            .collect();
    }

    fn make_room(&mut self, bytes: u64) {
        let mut walked = false;
        while self.stored_bytes.saturating_add(bytes) > self.quota_bytes {
            let Some((path, length)) = self.evictable.pop_front() else {
                if walked {
                    break;
                }
                walked = true;
                self.rescan();
                if self.evictable.is_empty() {
                    break;
                }
                continue;
            };
            self.heartbeat.progressed();
            if let Some(deleted) = delete_frame(&path) {
                self.stored_bytes = self.stored_bytes.saturating_sub(length);
                if deleted {
                    self.stats.evictions += 1;
                }
            }
        }
    }

    fn encode(&self, frame: &BakedFrame) -> Result<Vec<u8>, String> {
        let error = |error: std::io::Error| error.to_string();
        let mut planes = Sha256::new();
        let mut compressed =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        for plane in [&frame.y, &frame.u, &frame.v] {
            planes.update(plane);
            compressed.write_all(plane).map_err(error)?;
        }
        let compressed = compressed.finish().map_err(error)?;
        let length = (frame.y.len() + frame.u.len() + frame.v.len()) as u64;
        let mut encoded = Vec::with_capacity(HEADER_BYTES + compressed.len());
        encoded.extend_from_slice(MAGIC);
        let format = self.layout.format;
        encoded.extend_from_slice(&FRAME_VERSION.to_le_bytes());
        encoded.extend_from_slice(&format.width.to_le_bytes());
        encoded.extend_from_slice(&format.height.to_le_bytes());
        encoded.extend_from_slice(&encoding_bytes(format.encoding));
        encoded.extend_from_slice(&length.to_le_bytes());
        encoded.extend_from_slice(&planes.finalize());
        encoded.extend_from_slice(&compressed);
        Ok(encoded)
    }

    fn store(&self, key: &str, encoded: &[u8], pinned: bool) -> Result<(), String> {
        let path = self.layout.path(key, pinned);
        let parent = path.parent().ok_or("no parent")?;
        let temporary = parent.join(format!(
            "{key}.{}.{}.tmp",
            std::process::id(),
            TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        let written = (|| {
            // Another export may remove a shard it found empty: make it again and retry once.
            let mut file = if let Ok(file) = File::create(&temporary) {
                file
            } else {
                fs::create_dir_all(parent)?;
                File::create(&temporary)?
            };
            file.write_all(encoded)?;
            fs::rename(&temporary, &path)
        })();
        if let Err(failure) = written {
            let _ = fs::remove_file(&temporary);
            return Err(failure.to_string());
        }
        Ok(())
    }

    /// Also deletes temporaries left an hour or more ago and removes emptied directories. This
    /// snapshot's frames stay.
    fn evict(&mut self) {
        let now = SystemTime::now();
        let scan = scan(&self.layout, &|| self.heartbeat.progressed());
        for (path, modified) in &scan.temporaries {
            if now.duration_since(*modified).unwrap_or_default() > STALE_TEMPORARY {
                let _ = fs::remove_file(path);
            }
        }
        let mut total = scan.total;
        for (_, _, length, path) in scan.others {
            if total <= self.quota_bytes {
                break;
            }
            self.heartbeat.progressed();
            if let Some(deleted) = delete_frame(&path) {
                total = total.saturating_sub(length);
                if deleted {
                    self.stats.evictions += 1;
                }
            }
        }
        for directory in scan.directories {
            let _ = fs::remove_dir(directory);
        }
        self.stored_bytes = total;
    }
}

#[must_use]
pub fn unavailable(reason: &str) -> Value {
    json!({ "status": "unavailable", "reason": reason })
}
