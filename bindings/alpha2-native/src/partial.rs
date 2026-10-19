//! The partial file an output is written to until it is finished.
//!
//! Every transactional output writes `<stem>.alpha2-partial.<ext>` beside its
//! final path and renames it into place only when it finishes; anything else
//! removes it. A process that ends without unwinding (`SIGKILL`, the OOM
//! killer, a crash, `TerminateProcess`) removes nothing, so the file can
//! outlive its export. Whether a partial belongs to a live export is decided
//! by a lock, not by the file existing: the export that creates a partial
//! holds an exclusive advisory lock on it for as long as it writes, and the
//! operating system drops the lock when that process ends, however it ends.
//! A partial nobody holds is stale, and the next output to that path takes it
//! over; one that is held is refused, naming it.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use alpha2_ffmpeg_bridge::OutputFile;
use alpha2_media::EncodeError;

/// The words an output refused because a live export writes its partial file
/// ([`EncodeError::OutputInUse`]) starts with.
pub const PARTIAL_OUTPUT_IN_USE: &str = "another export is writing this output";

/// A failure to create or claim an output file, as [`EncodeError::Output`].
fn output_error(error: impl std::fmt::Display) -> EncodeError {
    EncodeError::Output(error.to_string())
}

fn output_error_at(path: &Path, error: impl std::fmt::Display) -> EncodeError {
    EncodeError::Output(format!("{}: {error}", path.display()))
}

/// How many times a claim opens the partial again when the file it locked
/// was removed or replaced under it, by the export that held it finishing.
const CLAIM_ATTEMPTS: usize = 8;

/// `<stem>.alpha2-partial.<ext>` beside `final_path` (`bin` when it has no extension).
///
/// # Errors
/// When the file name is not UTF-8.
pub(crate) fn partial_output_path(final_path: &Path) -> Result<PathBuf, EncodeError> {
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
    Ok(final_path.with_file_name(format!("{stem}.alpha2-partial.{extension}")))
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
/// A file this call created and then could not lock (a file system without
/// working locks) is removed before the error returns, as a failed export
/// removes its partial; one that was there before is left as it is, since it
/// may be another export's.
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
    for _ in 0..CLAIM_ATTEMPTS {
        let Some((file, created)) = open_partial(path, create).map_err(at)? else {
            if create {
                // Removed between two opens, or a name an SMB server keeps until the last handle of
                // a removed file closes: look again.
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
            return Err(in_use(path));
        }
        // The export that held it may have removed or renamed it between the open and the lock; the
        // lock is then on a file that path no longer names.
        if alpha2_os::same_file(&file, path).map_err(at)? {
            return Ok(Some((file, created)));
        }
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

/// A partial file a constructor is creating: removed when the constructor
/// fails on any path after it was claimed, until [`Self::keep`] hands its
/// lock to the value whose own `Drop` removes it from then on.
pub(crate) struct PartialOutput {
    path: PathBuf,
    lock: Option<PartialLock>,
}

/// The lock on a partial file, held by the output writing it. Kept until the
/// output has renamed the file into place or removed it, so no other export
/// takes the file over while it is still being written.
pub(crate) struct PartialLock {
    _file: File,
}

impl PartialOutput {
    /// Claims the partial file of an output at `final_path`: refuses an
    /// existing output, creates the partial and locks it, or takes over a
    /// stale one, emptied.
    ///
    /// # Errors
    /// For an existing output, a partial a live export holds
    /// ([`PARTIAL_OUTPUT_IN_USE`]), a path without a UTF-8 file name, or a
    /// file-system failure.
    pub(crate) fn claim(final_path: &Path) -> Result<Self, EncodeError> {
        if final_path.exists() {
            return Err(EncodeError::Output(
                "refusing to overwrite an existing output file".into(),
            ));
        }
        Self::claim_partial(final_path)
    }

    /// [`Self::claim`] without looking at what is at `final_path`.
    fn claim_partial(final_path: &Path) -> Result<Self, EncodeError> {
        Self::claim_partial_locking(final_path, &mut alpha2_os::try_lock_exclusive)
    }

    /// [`Self::claim_partial`], locking with `lock`.
    fn claim_partial_locking(final_path: &Path, lock: Lock<'_>) -> Result<Self, EncodeError> {
        let path = partial_output_path(final_path)?;
        let (file, created) = lock_partial(&path, true, lock)?
            .ok_or_else(|| EncodeError::Output("the partial file was not created".into()))?;
        if !created {
            alpha2_os::diagnostic!(
                "alpha2: replacing {}, a partial file no running export holds",
                path.display()
            );
        }
        let emptied = file.set_len(0);
        // From here a failure removes the file, the stale one included.
        let claimed = Self {
            path,
            lock: Some(PartialLock { _file: file }),
        };
        emptied.map_err(|error| output_error_at(&claimed.path, error))?;
        Ok(claimed)
    }

    /// The partial file's path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Opens the partial file for an `FFmpeg` muxer to write. Declared after
    /// `self`, the output closes the file before `self` removes it.
    ///
    /// # Errors
    /// When the path is not UTF-8 or has a NUL byte, or the muxer cannot be
    /// created.
    pub(crate) fn open_muxer(&self) -> Result<OutputFile, EncodeError> {
        let text = self
            .path
            .to_str()
            .ok_or_else(|| EncodeError::Output("output path is not UTF-8".into()))?;
        let name = CString::new(text)
            .map_err(|_| EncodeError::Output("output path contains a NUL byte".into()))?;
        OutputFile::create(&name).map_err(output_error)
    }

    /// Opens the partial file for this process to write.
    ///
    /// # Errors
    /// When the file system refuses.
    pub(crate) fn open_file(&self) -> Result<File, EncodeError> {
        OpenOptions::new()
            .write(true)
            .open(&self.path)
            .map_err(output_error)
    }

    /// The constructor succeeded: its value owns the file now, and holds the
    /// lock until it has renamed or removed it.
    pub(crate) fn keep(mut self) -> PartialLock {
        let lock = self.lock.take();
        lock.unwrap_or_else(|| unreachable!("a claimed partial holds its lock until it is kept"))
    }
}

impl Drop for PartialOutput {
    fn drop(&mut self) {
        // Removed while still locked, so no other export takes over a file about to go.
        if let Some(lock) = self.lock.take() {
            let _ = fs::remove_file(&self.path);
            drop(lock);
        }
    }
}

/// Renames a finished partial file into place at `final_path`, which must
/// still be free: a file that appeared there while the export ran (another
/// program saved one, or another export of the same path finished first) is
/// left as it is, and the export fails rather than replace it.
///
/// # Errors
/// When a file is at `final_path`, or the rename fails.
pub(crate) fn publish(partial_path: &Path, final_path: &Path) -> Result<(), EncodeError> {
    alpha2_os::rename_no_replace(partial_path, final_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            EncodeError::Output(format!(
                "a file appeared at {} while this export ran; it was left as it is",
                final_path.display()
            ))
        } else {
            output_error_at(final_path, error)
        }
    })
}

/// Refuses a `final_path` that names something other than a regular file:
/// a symbolic link, a device, a FIFO, a socket or a directory is never
/// renamed over. Nothing there is fine.
fn ensure_replaceable(final_path: &Path) -> Result<(), EncodeError> {
    match fs::symlink_metadata(final_path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(EncodeError::Output(format!(
            "refusing to replace {}: not a regular file",
            final_path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(output_error_at(final_path, error)),
    }
}

/// Writes a whole file at `final_path`, replacing a regular file there only
/// once `write` has written all of it: `write` fills the claimed partial file
/// beside it (the same claim and lock a video output takes), which is synced
/// and renamed into place. Any failure removes the partial and leaves what was
/// at `final_path`. Only a regular file is replaced; anything else there is
/// refused before anything is written (and again just before the rename, as
/// it may have changed meanwhile; what changes between that look and the
/// rename is not guarded against, as the caller chose the path).
///
/// # Errors
/// When `final_path` names something other than a regular file, its partial
/// is held by a live export ([`PARTIAL_OUTPUT_IN_USE`]), `write` fails, or
/// the file system refuses.
pub fn write_replacing(
    final_path: impl AsRef<Path>,
    write: impl FnOnce(&mut File) -> Result<(), String>,
) -> Result<(), EncodeError> {
    let final_path = final_path.as_ref();
    ensure_replaceable(final_path)?;
    let partial = PartialOutput::claim_partial(final_path)?;
    let mut file = partial.open_file()?;
    write(&mut file).map_err(|error| output_error_at(partial.path(), error))?;
    file.sync_all()
        .map_err(|error| output_error_at(partial.path(), error))?;
    drop(file);
    ensure_replaceable(final_path)?;
    fs::rename(partial.path(), final_path).map_err(|error| output_error_at(final_path, error))?;
    // Renamed: nothing is left to remove, and the lock goes with the file it was on.
    drop(partial.keep());
    Ok(())
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

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    /// A final path in the temporary directory, unique to this test and
    /// process, with neither it nor its partial file there.
    fn final_path(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("alpha2-partial-{label}-{}.mp4", std::process::id()));
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
        let Err(error) = PartialOutput::claim_partial_locking(&path, &mut no_locks) else {
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
        assert!(PartialOutput::claim_partial_locking(&path, &mut no_locks).is_err());
        assert_eq!(
            fs::read(&partial).expect("still there"),
            b"another export's"
        );
        fs::remove_file(&partial).expect("clean up");
    }

    #[test]
    fn a_partial_its_holder_removes_while_it_is_claimed_is_claimed_new() {
        // The holder removes its partial and unlocks between this claim's open and lock. On a Linux
        // CIFS mount the removed file keeps its name until our handle closes, so the claim must not
        // take it.
        let path = final_path("removed-while-claimed");
        let partial = partial_output_path(&path).expect("partial path");
        let mut holder = Some(PartialOutput::claim(&path).expect("holder"));
        let claimed = PartialOutput::claim_partial_locking(&path, &mut |file| {
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
}
