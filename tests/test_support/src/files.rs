//! Paths every suite shares: the repository's test files and per-test scratch directories.

use std::path::{Path, PathBuf};

/// The repository's `tests/`, or `ALPHA2_TESTS_DIR` where a device runs the tests from a copy.
#[must_use]
pub fn tests_dir() -> PathBuf {
    std::env::var_os("ALPHA2_TESTS_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join(".."),
        PathBuf::from,
    )
}

/// A file of `tests/corpus`.
#[must_use]
pub fn corpus(name: &str) -> PathBuf {
    tests_dir().join("corpus").join(name)
}

/// An empty directory `alpha2-<name>-<pid>` under the system temporary directory, removed on drop
/// so that a failed assertion does not leave it behind.
///
/// A panicked run keeps its directory and a later process can reuse the pid, so a leftover is
/// removed first.
///
/// # Panics
/// When the directory cannot be made.
#[must_use]
pub fn scratch_dir(name: &str) -> ScratchDir {
    let path = std::env::temp_dir().join(format!("alpha2-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("scratch directory");
    ScratchDir(path)
}

/// A directory from [`scratch_dir`].
#[derive(Debug)]
pub struct ScratchDir(PathBuf);

impl std::ops::Deref for ScratchDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for ScratchDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scratch_directory_starts_empty_and_goes_on_drop() {
        let first = scratch_dir("test-support-scratch");
        std::fs::write(first.join("left"), "").expect("file");
        let path = first.to_path_buf();
        std::mem::forget(first);
        let second = scratch_dir("test-support-scratch");
        assert_eq!(*second, *path);
        assert_eq!(std::fs::read_dir(&*second).expect("listing").count(), 0);
        drop(second);
        assert!(!path.exists());
    }

    #[test]
    fn the_corpus_is_the_repositorys() {
        assert!(corpus("h264-long-gop.mp4").is_file());
    }
}
