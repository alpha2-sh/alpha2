//! The partial file an output is written to until it is finished, and how a
//! finished one is published.
//!
//! Every transactional output writes `<stem>.a2partial.<ext>` beside its final
//! path and moves it into place only when it finishes; anything else removes
//! it. A process that ends without unwinding (`SIGKILL`, the OOM killer, a
//! crash, `TerminateProcess`) removes nothing, so the file can outlive its
//! export. Whether a partial belongs to a live export is decided by a lock,
//! not by the file existing: the export that creates a partial holds an
//! exclusive advisory lock on it for as long as it writes, and the operating
//! system drops the lock when that process ends, however it ends. A partial
//! nobody holds is stale, and the next output to that path removes it and
//! creates its own; one that is held is refused, naming it.
//!
//! Once claimed, the folder is probed with two small files,
//! `<stem>.a2probe1.<ext>` and `<stem>.a2probe2.<ext>`: whether a file there
//! can be rewritten at an earlier offset (an MP4's index and a WAV's header
//! are), and whether a file can be renamed there without replacing one. A
//! folder that cannot take the rename is refused before anything renders; one
//! that cannot seek (a FUSE mount that only writes in order) gets the output
//! written to a staging file in the temporary directory, copied into the
//! partial once finished.
//!
//! A finished partial is synced, then published by
//! [`alpha2_os::publish_no_replace`] (or [`alpha2_os::publish_replace`] when
//! the output replaces a file), and its folder is synced. A finished render is
//! never deleted: when publishing fails it is moved to `<stem>.a2kept.<ext>`
//! and the error names that path ([`EncodeError::Publish`]).
//!
//! A name too long for its folder once the tag is added keeps every sibling
//! within the limit as `<stem prefix>~<8 hex digits>.<tag>.<ext>`, the digits
//! a hash of the final file name, so the same output always gets the same
//! sibling names.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use alpha2_ffmpeg_bridge::{CreateOutputError, OutputFile};
use alpha2_media::{CancelToken, Container, EncodeError};
use alpha2_os::{NameLimit, NameUnit, PublishMethod, Published, RetryPolicy};

/// The words an output refused because a live export writes its partial file
/// ([`EncodeError::OutputInUse`]) starts with.
pub const PARTIAL_OUTPUT_IN_USE: &str = "another export is writing this output";

/// The tag of the partial file an output is written to.
const TAG_PARTIAL: &str = "a2partial";
/// The tag of a finished render that could not be published.
const TAG_KEPT: &str = "a2kept";
/// The tags of the two probe files a claim renames one to the other.
const TAG_PROBE: [&str; 2] = ["a2probe1", "a2probe2"];
/// The most kept files one output path gets: `a2kept`, then `a2kept2` to
/// `a2kept99`.
const KEPT_NAMES: u32 = 99;

/// How long publishing a finished output keeps trying while another program
/// holds the target or the partial open without sharing deletion (Windows).
pub(crate) const PUBLISH_RETRY_BUDGET: Duration = Duration::from_secs(10);
/// How long the claim's probe rename keeps trying the same way.
const PROBE_RETRY_BUDGET: Duration = Duration::from_secs(2);
/// How long a claim waits for a partial whose lock another handle holds: a
/// child process between `fork` and `exec` holds a copy of a just-closed lock
/// handle for a moment on macOS.
const CLAIM_IN_USE_WAIT: Duration = Duration::from_millis(250);
/// How often a claim tries a held lock again within [`CLAIM_IN_USE_WAIT`].
const CLAIM_IN_USE_POLL: Duration = Duration::from_millis(10);
/// Bytes copied at a time from a staging file into the partial.
const STAGING_COPY_CHUNK: usize = 4 << 20;

/// A failure to create or claim an output file, as [`EncodeError::Output`].
fn output_error(error: impl std::fmt::Display) -> EncodeError {
    EncodeError::Output(error.to_string())
}

fn output_error_at(path: &Path, error: impl std::fmt::Display) -> EncodeError {
    EncodeError::Output(format!("{}: {error}", path.display()))
}

/// What an output does with a file already at its path.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ExistingOutput {
    /// Refuse it, before writing and again at the rename (the default).
    #[default]
    Refuse,
    /// Replace a regular file there once the output is whole, by rename.
    /// A symbolic link, a device, a FIFO, a socket or a directory is refused.
    Replace,
}

/// Where an output goes and what it does with a file already there.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputTarget {
    /// The output's final path.
    pub path: PathBuf,
    /// What happens to a file already at `path`.
    pub existing: ExistingOutput,
}

impl OutputTarget {
    /// `path`, replacing a regular file there.
    #[must_use]
    pub fn replacing(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            existing: ExistingOutput::Replace,
        }
    }
}

impl From<PathBuf> for OutputTarget {
    fn from(path: PathBuf) -> Self {
        Self {
            path,
            existing: ExistingOutput::Refuse,
        }
    }
}

impl From<&Path> for OutputTarget {
    fn from(path: &Path) -> Self {
        path.to_owned().into()
    }
}

impl From<&PathBuf> for OutputTarget {
    fn from(path: &PathBuf) -> Self {
        path.clone().into()
    }
}

impl From<String> for OutputTarget {
    fn from(path: String) -> Self {
        PathBuf::from(path).into()
    }
}

impl From<&String> for OutputTarget {
    fn from(path: &String) -> Self {
        PathBuf::from(path).into()
    }
}

impl From<&str> for OutputTarget {
    fn from(path: &str) -> Self {
        PathBuf::from(path).into()
    }
}

impl From<&OutputTarget> for OutputTarget {
    fn from(target: &OutputTarget) -> Self {
        target.clone()
    }
}

/// How a finished output was published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublishReport {
    /// How the partial file was moved into place.
    pub method: PublishMethod,
    /// The output folder could not seek within a file being written, so the
    /// output was written to a staging file in the temporary directory and
    /// copied into the folder once finished.
    pub staged_locally: bool,
    /// How many times a step was tried again while another program held a
    /// file (Windows).
    pub retries: u32,
}

/// Whether an output's container goes back to rewrite what it wrote earlier
/// (an MP4's index, a WAV's header), which the claim checks the folder takes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SeekNeed {
    /// The writer seeks: a folder that cannot seek gets a staging file.
    Required,
    /// The file is written in order, start to end.
    NotNeeded,
}

/// How many times a claim opens the partial again when the file it locked
/// was removed or replaced under it, by the export that held it finishing.
const CLAIM_ATTEMPTS: usize = 8;

/// The folder `final_path` is in: `.` for a bare file name.
fn parent_dir(final_path: &Path) -> &Path {
    final_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// FNV-1a, 64-bit: a small, fixed hash, the same in every build, for names
/// that must come out the same in every process.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The name of the sibling of a file called `name` (stem `stem`, extension
/// `extension`) that carries `tag`, within `limit`: `<stem>.<tag>.<ext>`,
/// unless the longest sibling (`a2partial`) would not fit, in which case every
/// sibling is `<stem prefix>~<8 hex>.<tag>.<ext>`, the prefix cut at a
/// character boundary and the digits the low 32 bits of FNV-1a-64 over the
/// UTF-8 bytes of `name`.
fn sibling_name(name: &str, stem: &str, extension: &str, tag: &str, limit: NameLimit) -> String {
    let named = |stem: &str, tag: &str| format!("{stem}.{tag}.{extension}");
    if limit.length(&named(stem, TAG_PARTIAL)) <= limit.max {
        return named(stem, tag);
    }
    let hash = format!("~{:08x}", fnv1a_64(name.as_bytes()) & 0xffff_ffff);
    let room = limit
        .max
        .saturating_sub(limit.length(&named(&hash, TAG_PARTIAL)));
    let mut used = 0;
    let mut prefix = 0;
    for (index, character) in stem.char_indices() {
        let units = match limit.unit {
            NameUnit::Bytes => character.len_utf8(),
            NameUnit::Utf16 => character.len_utf16(),
        };
        if used + units > room {
            break;
        }
        used += units;
        prefix = index + character.len_utf8();
    }
    named(&format!("{}{hash}", &stem[..prefix]), tag)
}

/// The sibling of `final_path` that carries `tag` ([`sibling_name`]), in the
/// same folder (`bin` stands for a missing extension).
///
/// # Errors
/// When the file name is not UTF-8.
fn sibling_path(final_path: &Path, tag: &str) -> Result<PathBuf, EncodeError> {
    let name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| EncodeError::Output("output path has no UTF-8 file name".into()))?;
    let extension = final_path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("bin");
    let stem = final_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(name);
    let limit = alpha2_os::name_limit(parent_dir(final_path));
    Ok(final_path.with_file_name(sibling_name(name, stem, extension, tag, limit)))
}

/// `<stem>.a2partial.<ext>` beside `final_path` ([`sibling_path`]).
///
/// # Errors
/// When the file name is not UTF-8.
pub(crate) fn partial_output_path(final_path: &Path) -> Result<PathBuf, EncodeError> {
    sibling_path(final_path, TAG_PARTIAL)
}

/// The `n`th kept name: `a2kept`, then `a2kept2` and on.
fn kept_tag(n: u32) -> String {
    if n <= 1 {
        TAG_KEPT.to_owned()
    } else {
        format!("{TAG_KEPT}{n}")
    }
}

/// Names discarded partials and staging files apart within this process.
static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

/// A short name, unique to this process, in the folder of `partial`: a
/// discarded partial is renamed to it before it is removed.
fn discard_path(partial: &Path) -> PathBuf {
    let n = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
    partial.with_file_name(format!(".a2del-{}-{n}", std::process::id()))
}

fn in_use(partial: &Path) -> EncodeError {
    EncodeError::OutputInUse(format!(
        "{PARTIAL_OUTPUT_IN_USE}: its partial file {} is held by an export still running",
        partial.display()
    ))
}

/// Locks a partial file for the claim that opened it: `Ok(false)` when
/// another handle holds it. [`alpha2_os::try_lock_exclusive`] outside tests.
type Lock<'a> = &'a mut dyn FnMut(&File) -> std::io::Result<bool>;

/// The partial file at `path`, opened and locked by this process, and
/// whether this call created it; `None` when it does not exist and `create`
/// is false.
///
/// A lock another handle holds is tried again every 10 ms for 250 ms before
/// the claim is refused. A file this call created and then could not lock (a
/// file system without working locks) is removed before the error returns, as
/// a failed export removes its partial; one that was there before is left as
/// it is, since it may be another export's.
///
/// # Errors
/// [`PARTIAL_OUTPUT_IN_USE`] when another open handle holds its lock, or the
/// file system's error, naming the path.
fn lock_partial(
    path: &Path,
    create: bool,
    lock: Lock<'_>,
) -> Result<Option<(File, bool)>, EncodeError> {
    let at = |error: std::io::Error| output_error_at(path, error);
    let started = Instant::now();
    let mut changes = 0;
    while changes < CLAIM_ATTEMPTS {
        let Some((file, created)) = open_partial(path, create).map_err(at)? else {
            if create {
                // Removed between two opens, or a name an SMB server keeps until the last handle of
                // a removed file closes: look again.
                changes += 1;
                continue;
            }
            return Ok(None);
        };
        let locked = match lock(&file) {
            Ok(locked) => locked,
            Err(error) => {
                if created && alpha2_os::same_file(&file, path).unwrap_or(false) {
                    let _ = fs::remove_file(path);
                }
                return Err(at(error));
            }
        };
        if !locked {
            if started.elapsed() < CLAIM_IN_USE_WAIT {
                drop(file);
                std::thread::sleep(CLAIM_IN_USE_POLL);
                continue;
            }
            return Err(in_use(path));
        }
        // The export that held it may have removed or renamed it between the open and the lock; the
        // lock is then on a file that path no longer names.
        if alpha2_os::same_file(&file, path).map_err(at)? {
            return Ok(Some((file, created)));
        }
        changes += 1;
    }
    Err(EncodeError::Output(format!(
        "the partial file {} kept changing while it was claimed",
        path.display()
    )))
}

/// The partial file at `path` opened for reading and writing, and whether
/// this call created it; `None` when nothing is there (and, with `create`,
/// when what is there could be neither created nor opened: it went between
/// the two).
fn open_partial(path: &Path, create: bool) -> std::io::Result<Option<(File, bool)>> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create {
        match options.clone().create_new(true).open(path) {
            Ok(file) => return Ok(Some((file, true))),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    match options.open(path) {
        Ok(file) => Ok(Some((file, false))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// ` (it had <n> other names)` when the stale partial `file` is also known by
/// other names: an older version's publish linked the final name to the
/// partial before removing the partial, and a crash between the two left
/// both.
fn other_names(file: &File) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match file.metadata().map(|metadata| metadata.nlink()) {
            Ok(links) if links > 1 => format!(" (it had {} other names)", links - 1),
            _ => String::new(),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        String::new()
    }
}

/// Where an output stands, for what its partial file becomes if it goes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PartialState {
    /// Being written: removed if the output goes.
    Writing,
    /// Written in full and closed, not yet published: kept if the output goes.
    Finished,
    /// Moved into place.
    Published,
    /// Moved aside after a failed publish, or left as it was when even that
    /// failed: the user's to deal with.
    Kept,
    /// Removed.
    Discarded,
}

/// The partial file of one output, from its claim until it is published or
/// removed: its path, the lock that marks it as this export's, and the
/// staging file a folder that cannot seek gets.
///
/// Every sink keeps one, declared after the field that holds the writer of
/// the file, so the writer is closed first when the sink drops: Windows
/// renames and removes no file that is still open without sharing deletion.
/// Dropped while being written it removes the file; dropped finished but not
/// published it keeps it ([`EncodeError::Publish`]).
pub(crate) struct PartialOutput {
    target: OutputTarget,
    path: PathBuf,
    lock: Option<File>,
    staging: Option<PathBuf>,
    state: PartialState,
}

impl PartialOutput {
    /// Claims the partial file of an output at `target`: refuses an existing
    /// output (or, replacing, anything there but a regular file), creates the
    /// partial and locks it, or removes a stale one and creates it again, then
    /// probes the folder. A folder that cannot seek gets a staging file when
    /// `seek` is [`SeekNeed::Required`].
    ///
    /// # Errors
    /// For an output the target refuses, a partial a live export holds
    /// ([`PARTIAL_OUTPUT_IN_USE`]), a path without a UTF-8 file name, a folder
    /// that cannot take a file without replacing one, or a file-system
    /// failure.
    pub(crate) fn claim(target: &OutputTarget, seek: SeekNeed) -> Result<Self, EncodeError> {
        match target.existing {
            ExistingOutput::Refuse => {
                if target.path.exists() {
                    return Err(EncodeError::Output(
                        "refusing to overwrite an existing output file".into(),
                    ));
                }
            }
            ExistingOutput::Replace => {
                replaceable(&target.path).map_err(EncodeError::Output)?;
                #[cfg(windows)]
                warn_if_held(&target.path);
            }
        }
        let mut claimed = Self::claim_partial_locking(target, &mut alpha2_os::try_lock_exclusive)?;
        if !claimed.probe(seek)? && seek == SeekNeed::Required {
            let staging = create_staging(&target.path)?;
            alpha2_os::diagnostic!(
                "alpha2: {} cannot rewrite a file at an earlier offset; this export is written to {} and copied there when it is finished",
                parent_dir(&target.path).display(),
                staging.display()
            );
            claimed.staging = Some(staging);
        }
        Ok(claimed)
    }

    /// Creates and locks the partial file of `target`, removing a stale one
    /// first, locking with `lock`. Neither the target nor the folder is
    /// looked at.
    fn claim_partial_locking(target: &OutputTarget, lock: Lock<'_>) -> Result<Self, EncodeError> {
        let path = partial_output_path(&target.path)?;
        for _ in 0..CLAIM_ATTEMPTS {
            let (file, created) = lock_partial(&path, true, lock)?
                .ok_or_else(|| EncodeError::Output("the partial file was not created".into()))?;
            if created {
                return Ok(Self {
                    target: target.clone(),
                    path,
                    lock: Some(file),
                    staging: None,
                    state: PartialState::Writing,
                });
            }
            alpha2_os::diagnostic!(
                "alpha2: replacing {}, a partial file no running export holds{}",
                path.display(),
                other_names(&file)
            );
            // Removed while locked and never emptied: a stale partial may be a second name of a
            // published file, whose bytes emptying it would destroy.
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(output_error_at(&path, error)),
            }
            drop(file);
        }
        Err(EncodeError::Output(format!(
            "the partial file {} kept changing while it was claimed",
            path.display()
        )))
    }

    /// Probes the output folder with two sibling files: whether a file there
    /// can be rewritten at an earlier offset (only when `seek` asks), and
    /// whether one can be renamed there without replacing another, by the
    /// same ladder a finished output is published with. Probe files a crashed
    /// export left are this export's to remove, since it holds the partial's
    /// lock. Returns whether the folder can seek.
    ///
    /// # Errors
    /// When the folder cannot take a rename that does not replace, or a probe
    /// file cannot be written.
    fn probe(&self, seek: SeekNeed) -> Result<bool, EncodeError> {
        let first = sibling_path(&self.target.path, TAG_PROBE[0])?;
        let second = sibling_path(&self.target.path, TAG_PROBE[1])?;
        for probe in [&first, &second] {
            match fs::remove_file(probe) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(output_error_at(probe, error)),
            }
        }
        let probed = self.probe_with(&first, &second, seek);
        let _ = fs::remove_file(&first);
        let _ = fs::remove_file(&second);
        probed
    }

    fn probe_with(&self, first: &Path, second: &Path, seek: SeekNeed) -> Result<bool, EncodeError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(first)
            .map_err(|error| output_error_at(first, error))?;
        let seekable = match seek {
            SeekNeed::NotNeeded => true,
            SeekNeed::Required => match rewrite_at_start(&mut file) {
                Ok(()) => !hooks::not_seekable(),
                Err(error) if cannot_seek(&error) => false,
                Err(error) => return Err(output_error_at(first, error)),
            },
        };
        drop(file);
        let keep_trying = || true;
        publish_no_replace(
            first,
            second,
            &RetryPolicy {
                budget: PROBE_RETRY_BUDGET,
                on_wait: &keep_trying,
            },
        )
        .map_err(|error| {
            EncodeError::Output(format!(
                "{} cannot take a finished file without replacing one: {error}",
                parent_dir(&self.target.path).display()
            ))
        })?;
        Ok(seekable)
    }

    /// The file a writer writes the output to: the staging file when the
    /// folder cannot seek, the partial file otherwise.
    pub(crate) fn write_path(&self) -> &Path {
        self.staging.as_deref().unwrap_or(&self.path)
    }

    /// Opens [`Self::write_path`] for the `FFmpeg` muxer that writes
    /// `container`. The output must drop before `self` (a local declared
    /// after it, or a field declared before it), so the file is closed before
    /// `self` moves or removes it.
    ///
    /// # Errors
    /// When the path is not UTF-8 or has a NUL byte, or the muxer cannot be
    /// created.
    pub(crate) fn open_muxer(&self, container: Container) -> Result<OutputFile, EncodeError> {
        OutputFile::create(self.write_path(), crate::convert::muxer_name(container)).map_err(
            |error| match error {
                CreateOutputError::Path(error) => {
                    EncodeError::Output(format!("output path {error}"))
                }
                CreateOutputError::Ffmpeg(error) => output_error(error),
            },
        )
    }

    /// Opens [`Self::write_path`] for this process to write.
    ///
    /// # Errors
    /// When the file system refuses.
    pub(crate) fn open_file(&self) -> Result<File, EncodeError> {
        OpenOptions::new()
            .write(true)
            .open(self.write_path())
            .map_err(output_error)
    }

    /// `error`, a failure writing the output, saying it was written to a
    /// staging file when it was: a small temporary directory fills up where
    /// the output folder would not have.
    pub(crate) fn explain(&self, error: EncodeError) -> EncodeError {
        match (error, &self.staging) {
            (EncodeError::Adapter(message), Some(staging)) => EncodeError::Adapter(format!(
                "{message}; the output folder cannot be written in place, so this export was written to the staging file {} first",
                staging.display()
            )),
            (error, _) => error,
        }
    }

    /// Publishes the finished output, called once its writer has closed it
    /// successfully: copies a staging file into the partial, syncs the
    /// partial, moves it into place (trying again for up to 10 s while
    /// another program holds a file without sharing deletion, beating the
    /// encode watch between tries), and syncs the folder.
    ///
    /// # Errors
    /// [`EncodeError::Cancelled`] when `cancel` stopped the retries (the
    /// partial is removed). Any other failure keeps the finished file beside
    /// the target ([`EncodeError::Publish`]).
    pub(crate) fn finish(&mut self, cancel: &CancelToken) -> Result<PublishReport, EncodeError> {
        if self.state != PartialState::Writing {
            return Err(EncodeError::Adapter(
                "the output was already finished".into(),
            ));
        }
        self.state = PartialState::Finished;
        let staged_locally = self.staging.is_some();
        if let Some(staging) = self.staging.clone()
            && let Err(error) = self.copy_staging(&staging)
        {
            return Err(self.keep_staging(staging, &error));
        }
        let beat_unless_cancelled = || {
            crate::encode::beat_encode_call();
            !cancel.is_cancelled()
        };
        let retry = RetryPolicy {
            budget: PUBLISH_RETRY_BUDGET,
            on_wait: &beat_unless_cancelled,
        };
        match self.sync_and_publish(&retry) {
            Ok(published) => {
                self.state = PartialState::Published;
                drop(self.lock.take());
                self.remove_staging();
                #[cfg(unix)]
                if let Err(error) = sync_directory(parent_dir(&self.target.path)) {
                    alpha2_os::diagnostic!(
                        "alpha2: {} is in place, but syncing its folder failed: {error}",
                        self.target.path.display()
                    );
                }
                Ok(PublishReport {
                    method: published.method,
                    staged_locally,
                    retries: published.retries,
                })
            }
            Err(_) if cancel.is_cancelled() => {
                self.state = PartialState::Writing;
                self.discard()?;
                Err(EncodeError::Cancelled)
            }
            Err(message) => Err(self.keep(message)),
        }
    }

    /// Copies the finished staging file into the partial, through the lock
    /// handle: a FUSE server that writes a file only in order may also
    /// upload it each time a write handle closes, so a second handle would
    /// race this one.
    fn copy_staging(&self, staging: &Path) -> Result<(), String> {
        let mut source = File::open(staging).map_err(|error| error.to_string())?;
        let mut lock = self
            .lock
            .as_ref()
            .ok_or_else(|| "the partial file is no longer held".to_owned())?;
        lock.seek(SeekFrom::Start(0))
            .map_err(|error| error.to_string())?;
        let mut chunk = vec![0_u8; STAGING_COPY_CHUNK];
        loop {
            crate::encode::beat_encode_call();
            let read = source
                .read(&mut chunk)
                .map_err(|error| format!("reading {}: {error}", staging.display()))?;
            if read == 0 {
                return Ok(());
            }
            lock.write_all(&chunk[..read])
                .map_err(|error| format!("writing {}: {error}", self.path.display()))?;
        }
    }

    /// Syncs the partial and moves it into place; the message of a failure.
    fn sync_and_publish(&self, retry: &RetryPolicy<'_>) -> Result<Published, String> {
        let final_path = self.target.path.as_path();
        let lock = self
            .lock
            .as_ref()
            .ok_or_else(|| "the partial file is no longer held".to_owned())?;
        lock.sync_all()
            .map_err(|error| format!("syncing {}: {error}", self.path.display()))?;
        hooks::record("sync");
        let published = match self.target.existing {
            ExistingOutput::Refuse => {
                publish_no_replace(&self.path, final_path, retry).map_err(|error| {
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        format!(
                            "a file appeared at {} while this export ran; it was left as it is",
                            final_path.display()
                        )
                    } else {
                        format!("{}: {error}", final_path.display())
                    }
                })
            }
            // What changes between this look and the rename is not guarded against: the caller
            // chose to replace the path.
            ExistingOutput::Replace => replaceable(final_path).and_then(|()| {
                alpha2_os::publish_replace(&self.path, final_path, retry)
                    .map_err(|error| format!("{}: {error}", final_path.display()))
            }),
        }?;
        hooks::record("rename");
        Ok(published)
    }

    /// Moves the finished partial aside after a failed publish (`message`
    /// says what failed), to the first free kept name, and releases it.
    fn keep(&mut self, message: String) -> EncodeError {
        let kept = self.move_aside(&self.path);
        self.state = PartialState::Kept;
        drop(self.lock.take());
        self.remove_staging();
        match kept {
            Ok(kept) => EncodeError::Publish {
                message,
                kept: Some(kept),
            },
            Err(error) => EncodeError::Publish {
                message: format!(
                    "{message}; moving it aside failed too ({error}), so it is left as {}, which the next export to this path removes",
                    self.path.display()
                ),
                kept: None,
            },
        }
    }

    /// A staging file that could not be copied into the folder is itself the
    /// kept file, left in the temporary directory; the half-copied partial is
    /// removed.
    fn keep_staging(&mut self, staging: PathBuf, error: &str) -> EncodeError {
        self.staging = None;
        self.state = PartialState::Writing;
        let _ = self.discard();
        self.state = PartialState::Kept;
        EncodeError::Publish {
            message: format!(
                "{}: copying the finished render into {} failed: {error}",
                self.target.path.display(),
                self.path.display()
            ),
            kept: Some(staging),
        }
    }

    /// Renames `source` to the first free kept name beside the target, never
    /// replacing a file; the kept path.
    fn move_aside(&self, source: &Path) -> Result<PathBuf, String> {
        let beat = || {
            crate::encode::beat_encode_call();
            true
        };
        let retry = RetryPolicy {
            budget: PUBLISH_RETRY_BUDGET,
            on_wait: &beat,
        };
        for n in 1..=KEPT_NAMES {
            let kept =
                sibling_path(&self.target.path, &kept_tag(n)).map_err(|error| error.to_string())?;
            match publish_no_replace(source, &kept, &retry) {
                Ok(_) => return Ok(kept),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(format!("{}: {error}", kept.display())),
            }
        }
        Err(format!(
            "every kept name from {} to {} is taken",
            kept_tag(1),
            kept_tag(KEPT_NAMES)
        ))
    }

    fn remove_staging(&mut self) {
        if let Some(staging) = self.staging.take() {
            let _ = fs::remove_file(staging);
        }
    }

    /// Removes the partial of an output that will not finish.
    ///
    /// The partial is first renamed, still locked, to a short name only this
    /// process uses, and removed under that name once the lock is closed: no
    /// other export can take the partial's name over before it is gone, and a
    /// FUSE server that writes a file back when its last write handle closes
    /// writes it back under the discard name, which is then removed. When
    /// the rename fails, the partial is removed under its own name, still
    /// locked. A finished output is kept instead ([`EncodeError::Publish`]),
    /// and the kept path is written to standard error, since no caller
    /// receives the error.
    ///
    /// # Errors
    /// When the file system refuses the removal.
    pub(crate) fn discard(&mut self) -> Result<(), EncodeError> {
        match self.state {
            PartialState::Writing => {}
            PartialState::Finished => {
                let kept = self.keep("the export stopped before its output was published".into());
                alpha2_os::diagnostic!("alpha2: {kept}");
                return Ok(());
            }
            PartialState::Published | PartialState::Kept | PartialState::Discarded => {
                return Ok(());
            }
        }
        self.state = PartialState::Discarded;
        self.remove_staging();
        let Some(lock) = self.lock.take() else {
            return Ok(());
        };
        let discard = discard_path(&self.path);
        let (removed, at) = if publish_no_replace(&self.path, &discard, &RetryPolicy::NONE).is_ok()
        {
            close_lock(lock, &discard);
            let keep_trying = || true;
            let retry = RetryPolicy {
                budget: PUBLISH_RETRY_BUDGET,
                on_wait: &keep_trying,
            };
            let removed =
                alpha2_os::with_publish_retries(&retry, || fs::remove_file(&discard)).map(|_| ());
            (removed, discard)
        } else {
            let removed = fs::remove_file(&self.path);
            close_lock(lock, &self.path);
            (removed, self.path.clone())
        };
        match removed {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(output_error_at(&at, error)),
        }
    }
}

impl Drop for PartialOutput {
    fn drop(&mut self) {
        let _ = self.discard();
    }
}

/// Closes the lock handle of a partial file now named `name`.
fn close_lock(lock: File, name: &Path) {
    drop(lock);
    hooks::lock_closed(name);
}

/// Writes two bytes, goes back to the start and rewrites one, then syncs:
/// what an MP4 muxer does to its index and a WAV writer to its header.
fn rewrite_at_start(file: &mut File) -> std::io::Result<()> {
    file.write_all(&[0, 0])?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&[1])?;
    file.sync_all()
}

/// Whether `error` says a file cannot be rewritten at an earlier offset:
/// `ESPIPE`, `EINVAL` or `ENOTSUP`/`EOPNOTSUPP` (what a FUSE server that
/// writes in order answers), by kind so every platform reads it alike.
fn cannot_seek(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotSeekable
            | std::io::ErrorKind::InvalidInput
            | std::io::ErrorKind::Unsupported
    )
}

/// A new staging file for an output at `final_path`, in the temporary
/// directory, with the output's extension.
fn create_staging(final_path: &Path) -> Result<PathBuf, EncodeError> {
    let extension = final_path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("bin");
    for _ in 0..CLAIM_ATTEMPTS {
        let n = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
        let staging = std::env::temp_dir().join(format!(
            "alpha2-staging-{}-{n}.{extension}",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
        {
            Ok(_) => return Ok(staging),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(output_error_at(&staging, error)),
        }
    }
    Err(EncodeError::Output(
        "no staging file could be created in the temporary directory".into(),
    ))
}

/// Says on standard error that the file a replacing output will replace is
/// open in a program that does not share deletion (Windows refuses the
/// rename while it is): the publish then waits for it, for a while.
#[cfg(windows)]
fn warn_if_held(final_path: &Path) {
    use std::os::windows::fs::OpenOptionsExt;
    const DELETE: u32 = 0x0001_0000;
    const SHARE_READ_WRITE_DELETE: u32 = 0x1 | 0x2 | 0x4;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    if let Err(error) = OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(SHARE_READ_WRITE_DELETE)
        .open(final_path)
        && error.raw_os_error() == Some(ERROR_SHARING_VIOLATION)
    {
        alpha2_os::diagnostic!(
            "alpha2: {} is open in another program; the export will wait up to {} s for it when it finishes",
            final_path.display(),
            PUBLISH_RETRY_BUDGET.as_secs()
        );
    }
}

/// [`alpha2_os::publish_no_replace`], or what a test made it answer.
fn publish_no_replace(
    from: &Path,
    to: &Path,
    retry: &RetryPolicy<'_>,
) -> std::io::Result<Published> {
    if let Some(error) = hooks::publish_failure(to) {
        return Err(error);
    }
    alpha2_os::publish_no_replace(from, to, retry)
}

/// [`alpha2_os::sync_directory`], recorded for a test.
#[cfg(unix)]
fn sync_directory(dir: &Path) -> std::io::Result<()> {
    alpha2_os::sync_directory(dir)?;
    hooks::record("directory sync");
    Ok(())
}

/// Refuses a `final_path` that names something other than a regular file:
/// a symbolic link, a device, a FIFO, a socket or a directory is never
/// renamed over. Nothing there is fine.
fn replaceable(final_path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(final_path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(format!(
            "refusing to replace {}: not a regular file",
            final_path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}: {error}", final_path.display())),
    }
}

/// Writes a whole file at `target`, published only once `write` has written
/// all of it: `write` fills the claimed partial file beside it (the same
/// claim and lock a video output takes), which is synced and moved into place
/// as a video output is. A failure while writing removes the partial and
/// leaves what was at the target's path. What is there is looked at before
/// anything is written, and again at the rename, as it may have changed
/// meanwhile.
///
/// # Errors
/// When the target refuses what is at its path, its partial is held by a
/// live export ([`PARTIAL_OUTPUT_IN_USE`]), `write` fails, or the file system
/// refuses; [`EncodeError::Publish`] when the written file could not be moved
/// into place and was kept beside it.
pub fn write_whole(
    target: impl Into<OutputTarget>,
    write: impl FnOnce(&mut File) -> Result<(), String>,
) -> Result<PublishReport, EncodeError> {
    let target = target.into();
    let mut partial = PartialOutput::claim(&target, SeekNeed::NotNeeded)?;
    let mut file = partial.open_file()?;
    write(&mut file).map_err(|error| output_error_at(partial.write_path(), error))?;
    drop(file);
    partial.finish(&CancelToken::default())
}

/// Writes `rgba`, `width` by `height` 8-bit RGBA pixels with straight alpha,
/// row by row, as a PNG at `target`, published as [`write_whole`] publishes a
/// file: whole or not at all.
///
/// # Errors
/// When `rgba` is not that many pixels, or as [`write_whole`].
pub fn write_png(
    target: impl Into<OutputTarget>,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<PublishReport, EncodeError> {
    let expected = u64::from(width) * u64::from(height) * 4;
    if u64::try_from(rgba.len()).ok() != Some(expected) {
        return Err(EncodeError::Adapter(format!(
            "a {width}x{height} RGBA image is {expected} bytes, not {}",
            rgba.len()
        )));
    }
    write_whole(target, |file| {
        let mut buffered = std::io::BufWriter::new(file);
        let mut encoder = png::Encoder::new(&mut buffered, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        // The pixels are display-encoded sRGB; the chunk says so rather than leaving viewers to
        // assume it.
        encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
        let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
        writer
            .write_image_data(rgba)
            .map_err(|error| error.to_string())?;
        writer.finish().map_err(|error| error.to_string())?;
        buffered
            .into_inner()
            .map(|_| ())
            .map_err(|error| error.error().to_string())
    })
}

/// Removes a sibling partial file left by an interrupted transactional export.
///
/// A partial file a live export holds is not touched: the lock that tells a
/// stale partial from a live one is the same one an output takes over a stale
/// partial by. It returns `false` when no partial file exists.
///
/// # Errors
/// [`PARTIAL_OUTPUT_IN_USE`] when a live export holds it; an encoder error for
/// an invalid target path or a file-system failure.
pub fn remove_stale_partial_output(path: impl AsRef<Path>) -> Result<bool, EncodeError> {
    let partial = partial_output_path(path.as_ref())?;
    let Some((file, _)) = lock_partial(&partial, false, &mut alpha2_os::try_lock_exclusive)? else {
        return Ok(false);
    };
    let removed = fs::remove_file(&partial);
    drop(file);
    match removed {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(output_error(error)),
    }
}

/// What a test makes the file system do to the outputs this thread
/// publishes, and what it records of them. Every function is a no-op outside
/// tests.
#[cfg(not(test))]
mod hooks {
    use std::path::Path;

    pub(super) fn publish_failure(_to: &Path) -> Option<std::io::Error> {
        None
    }

    pub(super) fn not_seekable() -> bool {
        false
    }

    pub(super) fn record(_event: &'static str) {}

    pub(super) fn lock_closed(_name: &Path) {}
}

/// What a test makes the file system do to the outputs this thread
/// publishes, and what it records of them.
#[cfg(test)]
pub(crate) mod hooks {
    use std::cell::{Cell, RefCell};
    use std::path::{Path, PathBuf};

    /// What [`ON_LOCK_CLOSE`] calls.
    pub(crate) type LockCloseHook = Box<dyn Fn(&Path)>;

    thread_local! {
        /// A no-replace publish to this path fails with this OS error.
        pub(crate) static FAIL_PUBLISH_TO: RefCell<Option<(PathBuf, i32)>> =
            const { RefCell::new(None) };
        /// The claim's probe reports a folder that cannot seek.
        pub(crate) static NOT_SEEKABLE: Cell<bool> = const { Cell::new(false) };
        /// The publish steps taken, in order, when `Some`.
        pub(crate) static EVENTS: RefCell<Option<Vec<&'static str>>> =
            const { RefCell::new(None) };
        /// Called with the name a partial file has when its lock handle
        /// closes.
        pub(crate) static ON_LOCK_CLOSE: RefCell<Option<LockCloseHook>> =
            const { RefCell::new(None) };
    }

    pub(super) fn publish_failure(to: &Path) -> Option<std::io::Error> {
        FAIL_PUBLISH_TO.with_borrow(|fail| {
            fail.as_ref()
                .filter(|(path, _)| path == to)
                .map(|(_, code)| std::io::Error::from_raw_os_error(*code))
        })
    }

    pub(super) fn not_seekable() -> bool {
        NOT_SEEKABLE.get()
    }

    pub(super) fn record(event: &'static str) {
        EVENTS.with_borrow_mut(|events| {
            if let Some(events) = events {
                events.push(event);
            }
        });
    }

    pub(super) fn lock_closed(name: &Path) {
        ON_LOCK_CLOSE.with_borrow(|hook| {
            if let Some(hook) = hook {
                hook(name);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::{Read, Write};

    use super::*;

    /// A final path in the temporary directory, unique to this test and
    /// process, with neither it nor its partial file there.
    fn final_path(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("alpha2-claim-{label}-{}.mp4", std::process::id()));
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(partial_output_path(&path).expect("partial path"));
        path
    }

    /// The lock of a file system without working locks (`ENOLCK` on Linux).
    fn no_locks(_: &File) -> std::io::Result<bool> {
        Err(std::io::Error::other("No locks available"))
    }

    #[test]
    fn a_partial_the_claim_created_and_could_not_lock_is_removed() {
        let path = final_path("unlockable");
        let partial = partial_output_path(&path).expect("partial path");
        let Err(error) =
            PartialOutput::claim_partial_locking(&OutputTarget::from(&path), &mut no_locks)
        else {
            panic!("a claim without a lock succeeded");
        };
        assert!(
            matches!(&error, EncodeError::Output(message) if message.contains(&partial.display().to_string())),
            "{error}"
        );
        assert!(
            !partial.exists(),
            "the refused claim left the partial it created"
        );
    }

    #[test]
    fn a_partial_that_was_there_is_left_when_the_lock_fails() {
        let path = final_path("unlockable-existing");
        let partial = partial_output_path(&path).expect("partial path");
        fs::write(&partial, b"another export's").expect("existing partial");
        assert!(
            PartialOutput::claim_partial_locking(&OutputTarget::from(&path), &mut no_locks)
                .is_err()
        );
        assert_eq!(
            fs::read(&partial).expect("still there"),
            b"another export's"
        );
        fs::remove_file(&partial).expect("clean up");
    }

    #[test]
    fn a_partial_its_holder_removes_while_it_is_claimed_is_claimed_new() {
        // The holder's lock is released and taken again below.
        let _child = crate::tests::child_process_lock();
        // The holder removes its partial and unlocks between this claim's open and lock. On a Linux
        // CIFS mount the removed file keeps its name until our handle closes, so the claim must not
        // take it.
        let path = final_path("removed-while-claimed");
        let partial = partial_output_path(&path).expect("partial path");
        let target = OutputTarget::from(&path);
        let mut holder = Some(PartialOutput::claim(&target, SeekNeed::Required).expect("holder"));
        let claimed = PartialOutput::claim_partial_locking(&target, &mut |file| {
            drop(holder.take());
            alpha2_os::try_lock_exclusive(file)
        })
        .expect("claimed after the holder finished");
        assert!(holder.is_none());
        let mut writer = claimed.open_file().expect("the claimed partial opens");
        writer.write_all(b"new export").expect("write");
        drop(writer);
        let mut read = String::new();
        File::open(&partial)
            .and_then(|mut file| file.read_to_string(&mut read))
            .expect("read back");
        assert_eq!(read, "new export");
        drop(claimed);
        assert!(!partial.exists());
    }

    #[test]
    fn a_stale_partial_is_replaced_not_emptied() {
        let path = final_path("stale-replaced");
        let partial = partial_output_path(&path).expect("partial path");
        fs::write(&partial, b"what a killed export left").expect("stale partial");
        let stale = File::open(&partial).expect("the stale file");
        let claimed =
            PartialOutput::claim(&OutputTarget::from(&path), SeekNeed::Required).expect("claim");
        assert!(
            !alpha2_os::same_file(&stale, &partial).expect("identity"),
            "the stale file was reused"
        );
        let mut bytes = Vec::new();
        (&stale)
            .read_to_end(&mut bytes)
            .expect("read the stale file");
        assert_eq!(
            bytes, b"what a killed export left",
            "the stale file was emptied"
        );
        assert_eq!(fs::read(&partial).expect("new partial"), b"");
        drop(claimed);
        assert!(!partial.exists());
    }

    /// An older version published by linking the final name to the partial before removing the
    /// partial; a crash between the two left the partial as a second name of the output.
    #[test]
    #[cfg(unix)]
    fn a_stale_partial_that_is_a_second_name_of_the_output_leaves_the_output_intact() {
        let path = final_path("stale-second-name");
        let partial = partial_output_path(&path).expect("partial path");
        fs::write(&path, b"the published output").expect("output");
        fs::hard_link(&path, &partial).expect("second name");
        let claimed = PartialOutput::claim(&OutputTarget::replacing(&path), SeekNeed::Required)
            .expect("claim");
        assert_eq!(
            fs::read(&path).expect("output"),
            b"the published output",
            "claiming the partial changed the output"
        );
        assert!(!alpha2_os::same_file_at(&path, &partial).expect("identity"));
        drop(claimed);
        assert!(!partial.exists());
        assert_eq!(fs::read(&path).expect("output"), b"the published output");
        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_held_partial_is_refused_after_a_short_wait() {
        let path = final_path("held-refused");
        let target = OutputTarget::from(&path);
        let held = PartialOutput::claim(&target, SeekNeed::Required).expect("holder");
        let started = Instant::now();
        let refused = PartialOutput::claim(&target, SeekNeed::Required);
        let waited = started.elapsed();
        assert!(
            matches!(&refused, Err(EncodeError::OutputInUse(message)) if message.starts_with(PARTIAL_OUTPUT_IN_USE)),
            "{:?}",
            refused.err()
        );
        assert!(waited >= CLAIM_IN_USE_WAIT, "refused after {waited:?}");
        drop(held);
    }

    #[test]
    fn a_partial_released_within_the_wait_is_claimed() {
        let path = final_path("held-released");
        let partial = partial_output_path(&path).expect("partial path");
        let holder = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&partial)
            .expect("holder's partial");
        assert!(alpha2_os::try_lock_exclusive(&holder).expect("holder's lock"));
        let released = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(holder);
        });
        let claimed = PartialOutput::claim(&OutputTarget::from(&path), SeekNeed::Required)
            .expect("claimed once the holder let go");
        released.join().expect("holder");
        drop(claimed);
        assert!(!partial.exists());
    }

    #[test]
    fn a_name_near_the_limit_gets_a_shortened_partial_name() {
        let directory = std::env::temp_dir();
        let limit = alpha2_os::name_limit(&directory);
        // A name 247 long in the folder's own unit, which the 14 of `.a2partial.mp4` would take
        // past 255: 81 Hangul syllables of three UTF-8 bytes each where the limit counts bytes,
        // 243 of one UTF-16 unit each where it counts those (Windows).
        let syllables = match limit.unit {
            alpha2_os::NameUnit::Bytes => 81,
            alpha2_os::NameUnit::Utf16 => 243,
        };
        let name = format!("{}.mp4", "\u{d55c}".repeat(syllables));
        assert_eq!(limit.length(&name), 247);
        let path = directory.join(&name);
        let _ = fs::remove_file(&path);
        let partial = partial_output_path(&path).expect("partial path");
        assert_eq!(
            partial,
            partial_output_path(&path).expect("again"),
            "deterministic"
        );
        let siblings = [
            partial.clone(),
            sibling_path(&path, &kept_tag(1)).expect("kept"),
            sibling_path(&path, &kept_tag(KEPT_NAMES)).expect("last kept"),
            sibling_path(&path, TAG_PROBE[0]).expect("probe"),
            sibling_path(&path, TAG_PROBE[1]).expect("probe"),
        ];
        for sibling in &siblings {
            let sibling_name = sibling
                .file_name()
                .and_then(|name| name.to_str())
                .expect("name");
            assert!(
                limit.length(sibling_name) <= limit.max.min(255),
                "{sibling_name} is {} long",
                limit.length(sibling_name)
            );
            assert!(sibling_name.contains('~'), "{sibling_name}");
        }
        let partial_name = partial
            .file_name()
            .and_then(|name| name.to_str())
            .expect("name");
        assert!(partial_name.starts_with('\u{d55c}'), "{partial_name}");
        assert!(partial_name.ends_with(".a2partial.mp4"), "{partial_name}");

        let mut claimed =
            PartialOutput::claim(&OutputTarget::from(&path), SeekNeed::Required).expect("claim");
        assert!(partial.exists());
        // A crash leaves it, unlocked; the remover derives the same name.
        drop(claimed.lock.take());
        claimed.state = PartialState::Kept;
        drop(claimed);
        assert!(remove_stale_partial_output(&path).expect("stale partial"));
        assert!(!partial.exists());

        // A name with room keeps its stem whole.
        let short = directory.join("short.mp4");
        assert_eq!(
            partial_output_path(&short).expect("partial path"),
            directory.join("short.a2partial.mp4")
        );
    }

    #[test]
    fn a_discarded_partial_leaves_nothing_even_when_its_name_is_recreated_on_close() {
        // A FUSE server that writes a file back when its last write handle closes (rclone
        // without a cache) resurrects it under whatever name it has then.
        let path = final_path("recreated-on-close");
        let partial = partial_output_path(&path).expect("partial path");
        let closed_as = std::rc::Rc::new(RefCell::new(None));
        let record = std::rc::Rc::clone(&closed_as);
        hooks::ON_LOCK_CLOSE.set(Some(Box::new(move |name: &Path| {
            fs::write(name, b"written back").expect("write back");
            *record.borrow_mut() = Some(name.to_owned());
        })));
        let claimed =
            PartialOutput::claim(&OutputTarget::from(&path), SeekNeed::Required).expect("claim");
        drop(claimed);
        hooks::ON_LOCK_CLOSE.set(None);
        let closed_as = closed_as.borrow().clone().expect("the lock closed");
        assert_ne!(
            closed_as, partial,
            "the lock closed under the partial's name"
        );
        assert!(!partial.exists(), "the partial name came back");
        assert!(
            !closed_as.exists(),
            "the written-back discard name was left"
        );
    }
}
