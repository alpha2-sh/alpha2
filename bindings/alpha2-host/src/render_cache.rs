//! Baked export frames on disk: the YUV420 planes the encoder received,
//! found again by a later export of the same snapshot instead of rendered.
//!
//! A frame's key is a SHA-256 over everything its planes depend on: the host
//! binary, the `FFmpeg` runtime and the GPU that rendered it, the project,
//! each file it reads (path, size, and modification time), its effects and
//! fonts, the output size, the YUV encoding the planes are packed in (its
//! matrix, primaries, transfer, range, and chroma siting, which the output
//! codec decides: H.264 sites chroma left, AV1 and VP9 at the center), and
//! the frame's time and rate. A stored frame is bit for bit what the encoder
//! received, so an export that finds it encodes exactly what a live render
//! would have given it. Its header names its size and encoding again, and a
//! frame whose header does not match the export reading it is not taken.
//!
//! A cache directory holds `v2/<snapshot>/<shard>/<key>.frame`: one
//! directory per snapshot (the identity every key of an export shares, its
//! output size and encoding included), its
//! frames sharded by their key's first two hex digits, a pinned frame named
//! `<key>.pinned.frame`. The cache counts and deletes only what it names so,
//! in directories it names so, and never follows a symbolic link: anything
//! else in the directory is not its own and is left alone.
//!
//! Each frame is one file, written under a temporary name and renamed into
//! place, with its planes' SHA-256 in its header: a reader never sees a
//! partial frame, and a corrupt one is deleted and rendered again. Files are
//! not synced: the checksum, not the filesystem, is what a frame is trusted
//! by, so one a crash tore is found and rendered again, and a sync per frame
//! costs more than rendering it. Reading one touches its modification time,
//! which is its last access.
//!
//! Frames are read, inflated, and checked on [`READER_THREADS`] reader
//! threads, ahead of the export: it asks for the frames it will reach next
//! ([`RenderCache::request`], at most [`READ_AHEAD_FRAMES`] held or being
//! read), and takes each in turn ([`RenderCache::read`]), so a frame the cache
//! holds costs the export's own thread no filesystem call, inflate, or
//! checksum. The export takes results in the order it asked for them,
//! whichever reader finished first. One reader also makes the snapshot's
//! directory when the cache opens: no call into the cache's filesystem is
//! made on the export's thread.
//!
//! The quota holds while an export runs: a frame that would take the
//! directory past it is first made room for by deleting other snapshots'
//! frames, unpinned before pinned and least recently accessed first; if
//! there is still no room it is not written, and neither is any later frame
//! of the export. When an export ends the directory is brought within its
//! quota again the same way. An export never deletes a frame of its own
//! snapshot: they are what it and its retries read, whatever their
//! modification times say. Another process exporting into
//! the same directory is seen by walking the directory again whenever this
//! export has written a sixteenth of the quota since it last looked, and
//! when it runs out of frames to delete.
//!
//! Frames are compressed, hashed, and written on a writer thread, so baking
//! overlaps rendering and encoding instead of adding to each frame. The
//! writer holds at most [`BACKLOG_FRAMES`] frames; what a frame rendered
//! while it is full does is the request's [`Backlog`]: `skip` leaves it
//! unbaked, so the cache never slows an export, and `wait` holds the export
//! until there is room, so every frame is baked. An export that ends, even
//! by failing, first lets the writer store the frames it holds.
//!
//! The writer and the reader are watched as the decode and encode workers are
//! ([`crate::heartbeat`]): the writer beats before each frame it stores and as
//! it walks or deletes, the reader before it makes the directory and before
//! each frame it reads, and one that stays inside a single step longer than
//! the stall limit ([`crate::config::configured_stall_timeout`]) is stuck in a
//! filesystem that stopped answering, as a hard-mounted share does. Both are
//! then left running instead of joined, the export ends as it would have
//! without the cache (rendering every frame it has not taken yet: a cache
//! problem never fails an export, nor holds it forever), its result says the
//! cache was abandoned, and the directory is not used again by this process:
//! each export into it would leave more threads behind. A directory that
//! cannot be made within the limit is abandoned the same way, and the export
//! runs without the cache.
//!
//! A baked frame is the finished picture only. It is not a checkpoint of the
//! encoder, the muxer, the audio mix's DSP state, or a temporal effect's
//! history: an export that takes frames from here still encodes from its
//! first frame and mixes its audio from its first sample, so its output is
//! the same as a live render's, and what the cache saves is the rendering.

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
/// The directory layout's version, `v<n>` under the cache directory. A new
/// frame format alone keeps it: the frames an older one wrote stay counted
/// and deleted by the quota, and are never taken, since their header names
/// another format.
const LAYOUT_VERSION: u32 = 2;
/// The frame file's version, in its header.
const FRAME_VERSION: u32 = 3;
/// Magic, frame version, width, height, encoding, planes' length, planes'
/// SHA-256.
const HEADER_BYTES: usize = 4 + 4 + 4 + 4 + ENCODING_BYTES + 8 + 32;
/// [`encoding_bytes`], padded.
const ENCODING_BYTES: usize = 8;
/// A temporary file this old belongs to a writer that is gone.
const STALE_TEMPORARY: Duration = Duration::from_secs(60 * 60);
/// Quota when a request names none.
pub const DEFAULT_QUOTA_BYTES: u64 = 4 << 30;
/// Frames the writer holds at most, the one it is storing included: two
/// frames of the output's planes, and the compressed copy of the one it is
/// storing, beyond what the export's memory budgets bound.
pub const BACKLOG_FRAMES: usize = 2;
/// Threads that read one export's frames: reading one is a file read, an
/// inflate, and a checksum, each frame on one thread, and an export taking
/// every frame from the cache takes them faster than one thread reads them.
pub const READER_THREADS: usize = 2;
/// Frames the readers hold or are reading at most, the one the export waits
/// for included: enough for each reader to read one while the export takes
/// one read before. Like [`BACKLOG_FRAMES`], they are the output's planes
/// beyond what the export's memory budgets bound.
pub const READ_AHEAD_FRAMES: usize = 2 * READER_THREADS;
/// The share of the quota this export writes before it walks the directory
/// again, to see what other exports into it wrote and deleted.
const RESCAN_FRACTION: u64 = 16;

/// The writer takes frames.
const OPEN: u8 = 0;
/// A frame found no room in the quota; the export bakes nothing more.
const FULL: u8 = 1;
/// A write failed; the export bakes nothing more.
const FAILED: u8 = 2;
/// The writer or the reader was left stuck in the filesystem; the export
/// bakes and takes nothing more.
const ABANDONED: u8 = 3;

static TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// Cache directories whose writer or reader this process left stuck in their
/// filesystem, as each export named them: none is used again.
static ABANDONED_DIRECTORIES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn abandoned_directories() -> std::sync::MutexGuard<'static, Vec<PathBuf>> {
    ABANDONED_DIRECTORIES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
thread_local! {
    /// Makes the next render cache opened on this thread block its writer
    /// forever where the stall says, as a filesystem call that never returns
    /// does, and watches it with this limit instead of the configured one.
    pub static INJECT_WRITER_STALL: std::cell::Cell<Option<(InjectedStall, Duration)>> =
        const { std::cell::Cell::new(None) };
    /// The same for the reader: its first operation makes the directory,
    /// each later one reads a frame.
    pub static INJECT_READER_STALL: std::cell::Cell<Option<(InjectedStall, Duration)>> =
        const { std::cell::Cell::new(None) };
}

/// Each frame file a reader read, with the name of the thread that read it.
#[cfg(test)]
pub static FRAMES_READ: Mutex<Vec<(PathBuf, Option<String>)>> = Mutex::new(Vec::new());

/// What a frame rendered while the writer is full does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backlog {
    /// It is not baked (`busySkips`): the cache never slows the export.
    #[default]
    Skip,
    /// The export waits for room: every frame is baked.
    Wait,
}

impl Backlog {
    /// Parses `skip` or `wait`; `None` for anything else.
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

/// Where an export bakes its frames, how much the directory may hold, and
/// what a frame the writer has no room for does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderCacheRequest {
    pub directory: PathBuf,
    pub quota_bytes: u64,
    pub backlog: Backlog,
}

impl RenderCacheRequest {
    /// A cache in `directory` holding at most `quota_bytes`: the one way
    /// to make one, so the protocol and the CLI refuse the same values.
    ///
    /// # Errors
    /// An empty directory, which would bake into the working directory, or
    /// a zero quota.
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

    /// Parses the protocol's `renderCache` value.
    ///
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
    /// A labelled piece of the identity; labels keep adjacent values apart.
    pub fn add(&mut self, label: &str, bytes: &[u8]) {
        self.0.update((label.len() as u64).to_le_bytes());
        self.0.update(label.as_bytes());
        self.0.update((bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
    }

    /// A file by what can change under the same path: its length and its
    /// modification time. A missing file is an identity too.
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

    /// The SHA-256 of everything added.
    #[must_use]
    pub fn finish(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}

/// What an export's baked planes are: their size and the YUV encoding they
/// are packed in. Both key the cache and are named in each frame's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameFormat {
    pub width: u32,
    pub height: u32,
    /// What the output codec packs to ([`YuvEncoding::export`]): a frame
    /// packed for one codec's chroma siting is not another's.
    pub encoding: YuvEncoding,
}

/// `encoding` as the bytes that key the cache and fill a frame header's
/// encoding field. Each value has its own code, fixed here rather than taken
/// from the enum's declaration order, so reordering a variant never makes
/// another export's frames look like this one's; a new variant does not
/// compile until it has one.
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

/// The snapshot directory of an export: its identity with the format of
/// the frames it bakes.
fn format_snapshot(snapshot: [u8; 32], format: FrameFormat) -> [u8; 32] {
    let mut identity = SnapshotIdentity::default();
    identity.add("snapshot", &snapshot);
    identity.add("width", &format.width.to_le_bytes());
    identity.add("height", &format.height.to_le_bytes());
    identity.add("encoding", &encoding_bytes(format.encoding));
    identity.finish()
}

/// One frame's planes, rows packed with no padding.
pub struct BakedFrame {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

/// What the cache did during one export.
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
    /// Frames not written because the quota had no room for them.
    pub quota_skips: u64,
    /// Frames not written because the writer was full (`Backlog::Skip`).
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

/// Where one snapshot's frames of one format live in a cache directory.
#[derive(Clone)]
struct Layout {
    /// `<directory>/v2`: every snapshot's directory.
    root: PathBuf,
    /// This snapshot's directory.
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

/// Lowercase hex, as the cache writes digests.
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

/// What a file the cache may have written is, by its name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Entry {
    Frame {
        pinned: bool,
    },
    /// `<key>.<pid>.<n>.tmp`, a frame being written.
    Temporary,
}

impl Entry {
    /// The entry named `name` in shard `shard`, or `None` for a name the
    /// cache never writes there.
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

/// A frame file of another snapshot: whether it is pinned, when it was last
/// read or written, its length, and where it is.
type Evictable = (bool, SystemTime, u64, PathBuf);

/// One walk of a cache directory's own entries.
#[derive(Default)]
struct Scan {
    /// Bytes of every snapshot's frames.
    total: u64,
    /// Other snapshots' frames, unpinned first, least recently accessed
    /// first: what this export may delete.
    others: Vec<Evictable>,
    /// Temporaries, with when each was last written.
    temporaries: Vec<(PathBuf, SystemTime)>,
    /// The shard and snapshot directories walked, shards first, to remove
    /// once emptied.
    directories: Vec<PathBuf>,
}

/// Subdirectories of `directory` whose names pass `named`, never through a
/// symbolic link.
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

/// Walks `layout`'s cache directory: its snapshots' shards and the frames
/// and temporaries in them, and nothing else.
fn scan(layout: &Layout, progressed: &dyn Fn()) -> Scan {
    let mut scan = Scan::default();
    for (snapshot, _) in named_directories(&layout.root, |name| is_hex(name, 64)) {
        let own = snapshot == layout.own;
        for (shard, shard_name) in named_directories(&snapshot, |name| is_hex(name, 2)) {
            let Ok(files) = fs::read_dir(&shard) else {
                continue;
            };
            for file in files.flatten() {
                // A walk is many filesystem calls: each entry is the walk alive, not stuck.
                progressed();
                let Ok(metadata) = file.metadata() else {
                    continue;
                };
                // `DirEntry::metadata` does not follow a symbolic link: only a file the cache could
                // have written is its own.
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

/// Deletes a frame another snapshot stored; whether its bytes left the
/// directory (deleted now, or already gone).
fn delete_frame(path: &Path) -> Option<bool> {
    match fs::remove_file(path) {
        Ok(()) => Some(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

/// A rendered frame on its way to the writer.
struct Bake {
    key: String,
    frame: BakedFrame,
    pinned: bool,
}

/// The writer thread and what it shares with the export.
struct Writer {
    sender: mpsc::SyncSender<Bake>,
    thread: JoinHandle<Store>,
    /// Beaten before each frame the writer stores and as it walks or
    /// deletes; see [`WorkerHeartbeat`].
    heartbeat: Arc<WorkerHeartbeat>,
    /// Frames the writer has taken off its queue: a `Backlog::Wait` write
    /// waits on this, not in a blocking send.
    progress: Arc<CommandProgress>,
    /// Set by [`RenderCache::finish`]: the writer brings the directory
    /// within its quota once its queue closes.
    finishing: Arc<AtomicBool>,
}

/// What the reader found under a key.
enum Found {
    /// No frame is stored under it.
    Missing,
    /// A frame was stored and failed its checks: it was deleted, to be
    /// rendered again.
    Corrupt,
    /// The frame, and the bytes its file held.
    Frame(BakedFrame, u64),
}

/// The reader threads and the frames asked of them.
struct Reader {
    keys: mpsc::Sender<String>,
    found: mpsc::Receiver<(String, Found)>,
    threads: Vec<JoinHandle<()>>,
    /// One per thread, beaten before the first makes the directory and
    /// before each frame one reads; see [`WorkerHeartbeat`].
    heartbeats: Vec<Arc<WorkerHeartbeat>>,
    /// Keys asked for whose result the export has not taken, in the order
    /// asked: the order the export takes them in.
    asked: std::collections::VecDeque<String>,
    /// Results that came back before those of keys asked ahead of them.
    arrived: std::collections::HashMap<String, Found>,
}

impl Reader {
    /// Whether a reader has been inside one step longer than `limit`.
    fn stuck(&self, limit: Duration) -> bool {
        self.heartbeats
            .iter()
            .any(|beat| beat.stuck_past(limit).is_some())
    }
}

/// One export's use of a cache directory.
pub struct RenderCache {
    quota_bytes: u64,
    snapshot: [u8; 32],
    backlog: Backlog,
    /// What the export itself did: reads, bypasses, and frames it did not
    /// hand the writer. The writer counts what it did with the rest.
    stats: RenderCacheStats,
    /// `OPEN`, `FULL`, or `FAILED`, set by the writer; `ABANDONED`.
    state: Arc<AtomicU8>,
    /// Frames handed to the writer and not yet stored or dropped.
    in_flight: Arc<AtomicUsize>,
    writer: Option<Writer>,
    /// What the writer has done so far, published after each frame: what a
    /// writer left running is reported to have done.
    writer_stats: Arc<Mutex<RenderCacheStats>>,
    reader: Option<Reader>,
    /// The directory as the request named it, which this process stops
    /// using once its writer or reader is abandoned.
    directory: PathBuf,
    /// The export's cancel: a `Backlog::Wait` write stops waiting for room,
    /// and a read for its frame, once it is set.
    cancel: CancelToken,
    /// See [`crate::config::configured_stall_timeout`].
    stall_timeout: Duration,
    /// Set once the writer or the reader was left stuck in the filesystem.
    abandoned: bool,
}

/// Leaves `what`, stuck in the filesystem of `directory`, running: this
/// process uses the directory no more.
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

/// Takes keys off `keys` and answers each with what is stored under it,
/// until `keys` or `answer` closes.
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
    /// Opens `request`'s directory for frames of `format` and starts its
    /// reader and writer. The snapshot directory, and so every key, is
    /// `snapshot` with `format`: frames of another size or encoding are never
    /// taken.
    ///
    /// The directory is made on the reader, watched: one that does not
    /// return within the stall limit is stuck in its filesystem, and the
    /// directory is abandoned.
    ///
    /// # Errors
    /// Reports a directory that cannot be made, or not within the stall
    /// limit, a reader or writer that cannot be started, and a directory
    /// whose reader or writer an earlier export of this process left stuck in
    /// its filesystem. The export runs without the cache then: a cache
    /// problem never fails an export.
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

    /// Starts the writer, which stores what it is handed into `store`'s
    /// directory, beating `heartbeat`; `in_flight` counts the frames it holds.
    fn start_writer(
        mut store: Store,
        heartbeat: Arc<WorkerHeartbeat>,
        in_flight: Arc<AtomicUsize>,
    ) -> Result<Writer, String> {
        let progress = Arc::new(CommandProgress::default());
        let finishing = Arc::new(AtomicBool::new(false));
        // The writer takes the frame it stores out of the queue, so the queue holds one fewer than
        // it may.
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

    /// Starts the readers, one for each of `heartbeats`: the first makes the
    /// snapshot's directory, and the others start once it has. Waits for
    /// that, or until the first has been inside it longer than
    /// `stall_timeout`.
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

    /// The key of the frame at `time` on `rate`'s grid.
    #[must_use]
    pub fn key(&self, time: MediaTime, rate: FrameRate) -> String {
        let mut hash = Sha256::new();
        hash.update(self.snapshot);
        hash.update(time.ticks().to_le_bytes());
        hash.update(rate.numerator().to_le_bytes());
        hash.update(rate.denominator().to_le_bytes());
        hex(&hash.finalize())
    }

    /// Records a frame a clip's `bypass` keeps out of the cache.
    pub fn bypass(&mut self) {
        self.stats.bypassed_frames += 1;
    }

    /// Asks the readers for the frame stored under `key`, which the export
    /// will [`Self::read`] after those it asked for before: a reader reads it
    /// meanwhile. Nothing is asked once the readers hold or read
    /// [`READ_AHEAD_FRAMES`], nor twice.
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

    /// The frame stored under `key`, if a whole, uncorrupted one is there:
    /// taken from the readers, after it is asked for if it was not. A frame
    /// asked for before it and not taken is let go.
    ///
    /// The wait for it ends early, as a miss, when the export is cancelled,
    /// when a reader is stuck in the filesystem, which abandons the cache,
    /// and when one has stopped (it panicked), which ends the reading.
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
                    // Readers end only when their queue closes: one that ended panicked, and the
                    // frames it took are never answered.
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

    /// Whether a frame rendered now is to be baked: the writer is taking
    /// frames and, under `Backlog::Skip`, has room for one more. A frame
    /// that is not is counted here, so the export need not pack its planes
    /// for [`Self::write`].
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

    /// Hands a rendered frame's planes to the writer, waiting for room if
    /// it is full. A write that fails is counted and stops further writes;
    /// it never fails the export.
    ///
    /// The wait is not a blocking send: it ends when the writer takes a
    /// frame, and checks as it goes for the export's cancel, which drops the
    /// frame, and for a writer stuck in the filesystem, which is abandoned
    /// with it.
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

    /// Leaves the writer and the reader running, one of them (`stuck`)
    /// stuck in the filesystem: the export bakes and takes nothing more, and
    /// this process uses the directory no more.
    fn abandon(&mut self, stuck: &str) {
        // Neither is joined: what each holds stays with it, and the one not stuck ends once its
        // queue closes.
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

    /// Lets the writer store what it holds, brings the directory within its
    /// quota, and reports what the cache did. A writer or reader stuck in
    /// the filesystem is left running, and reported as `abandoned`, with what
    /// the writer had done by then.
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

    /// Frames handed to the writer and not yet stored or dropped.
    #[cfg(test)]
    pub fn held(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    /// Frames asked of the reader and not yet taken.
    #[cfg(test)]
    pub fn asked(&self) -> usize {
        self.reader.as_ref().map_or(0, |reader| reader.asked.len())
    }

    /// Waits until the writer holds no frame.
    #[cfg(test)]
    pub fn settle(&self) {
        while self.in_flight.load(Ordering::Acquire) > 0 {
            std::thread::yield_now();
        }
    }

    /// Closes the readers' queue and waits for them to read what they were
    /// asked, counting the corrupt frames they found among what the export
    /// did not take, or leaves them running once one has been inside one
    /// frame longer than the stall limit.
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

    /// Closes the writer's queue and waits for it to store what it holds,
    /// or leaves it running once it has been inside one frame or walk longer
    /// than the stall limit; `None` if it was gone already.
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

/// Reads the frame stored under `key` on the reader: deletes one that fails
/// its checks, and touches the modification time of one that passes, its
/// last access for eviction.
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

/// A frame file's planes, checked against its header and `layout`'s format,
/// and the bytes the file held.
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
    /// An export that ends without [`RenderCache::finish`], by failing or
    /// being cancelled, still stores the frames it handed the writer: they
    /// are what its retry takes. A writer or reader stuck in the filesystem
    /// is left running instead, as [`RenderCache::finish`] leaves it.
    fn drop(&mut self) {
        self.join_reader();
        let _ = self.join();
    }
}

/// The writer's side of a cache directory: its quota and what it stored.
struct Store {
    layout: Layout,
    quota_bytes: u64,
    state: Arc<AtomicU8>,
    stats: RenderCacheStats,
    /// Where `stats` is published after each frame and walk.
    published: Arc<Mutex<RenderCacheStats>>,
    /// The writer's; walks and deletions report progress on it.
    heartbeat: Arc<WorkerHeartbeat>,
    /// Bytes of every snapshot's frames in the directory: as the last walk
    /// found them, with what this export wrote and deleted since.
    stored_bytes: u64,
    /// Bytes this export wrote since the last walk.
    written_since_scan: u64,
    /// Other snapshots' frames as the last walk found them, unpinned and
    /// least recently accessed first: what this export may delete to make
    /// room.
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
        // Another export may be writing into the same directory: what it wrote counts against the
        // quota too.
        if self.written_since_scan >= (self.quota_bytes / RESCAN_FRACTION).max(1) {
            self.rescan();
        }
        // Room comes from other snapshots' frames; once a frame finds none, later ones are not
        // compressed only to be dropped.
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

    /// Publishes what the writer has done so far.
    fn publish(&self) {
        *self
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = self.stats;
    }

    /// Walks the directory again: its frames' bytes, and what this export may
    /// delete.
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

    /// Deletes other snapshots' frames, unpinned and least recently accessed
    /// first, until `bytes` more fit the quota or none is left to delete.
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

    /// A frame's file contents: its header, with the planes' SHA-256, and
    /// the planes compressed.
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

    /// Writes `encoded` under a temporary name and renames it into place.
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

    /// Deletes other snapshots' frames, unpinned and least recently accessed
    /// first, until the directory fits its quota, and temporaries a writer
    /// left an hour or more ago; removes the shard and snapshot directories
    /// that leaves empty. This snapshot's frames stay.
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

/// Why an export did not use the render cache it asked for.
#[must_use]
pub fn unavailable(reason: &str) -> Value {
    json!({ "status": "unavailable", "reason": reason })
}
