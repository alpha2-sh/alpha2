//! Where a source's bytes are read from: a file, or a remote object this request probed.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alpha2_media::{DecodeError, SourceFailure};

use super::{RemoteError, RemoteFailure, RemoteObject};

/// A source as decoders open it.
#[derive(Clone)]
pub enum SourceLocation {
    /// A local file.
    File(PathBuf),
    /// A remote object, read by range through its request's session.
    Remote(Arc<RemoteObject>),
}

impl SourceLocation {
    /// The file, if it is one.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::File(path) => Some(path),
            Self::Remote(_) => None,
        }
    }

    /// The remote object, if it is one.
    #[must_use]
    pub const fn remote(&self) -> Option<&Arc<RemoteObject>> {
        match self {
            Self::File(_) => None,
            Self::Remote(object) => Some(object),
        }
    }

    /// The source as messages show it: a path whole, a URL without user info, query or fragment.
    #[must_use]
    pub fn shown(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::Remote(object) => object.shown_url().to_owned(),
        }
    }

    /// The source's length in bytes.
    ///
    /// # Errors
    /// When a file's metadata cannot be read.
    pub fn byte_length(&self) -> std::io::Result<u64> {
        match self {
            Self::File(path) => std::fs::metadata(path).map(|metadata| metadata.len()),
            Self::Remote(object) => Ok(object.len()),
        }
    }
}

impl std::fmt::Debug for SourceLocation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.shown())
    }
}

impl PartialEq for SourceLocation {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::File(a), Self::File(b)) => a == b,
            (Self::Remote(a), Self::Remote(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl Eq for SourceLocation {}

impl Hash for SourceLocation {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::File(path) => path.hash(state),
            Self::Remote(object) => std::ptr::hash(Arc::as_ptr(object), state),
        }
    }
}

impl From<PathBuf> for SourceLocation {
    fn from(path: PathBuf) -> Self {
        Self::File(path)
    }
}

impl From<&Path> for SourceLocation {
    fn from(path: &Path) -> Self {
        Self::File(path.to_path_buf())
    }
}

impl From<&PathBuf> for SourceLocation {
    fn from(path: &PathBuf) -> Self {
        Self::File(path.clone())
    }
}

impl From<&SourceLocation> for SourceLocation {
    fn from(location: &SourceLocation) -> Self {
        location.clone()
    }
}

impl From<&RemoteError> for SourceFailure {
    fn from(error: &RemoteError) -> Self {
        Self {
            code: error.failure.code(),
            message: error.message.clone(),
            url: error.url.clone(),
            status: error.status,
            retry_after_ms: match error.failure {
                RemoteFailure::Busy { retry_after_ms } => Some(retry_after_ms),
                _ => None,
            },
            policy: match error.failure {
                RemoteFailure::Refused(policy) => Some(policy.name()),
                _ => None,
            },
        }
    }
}

/// The decode error a reader's failure is: a cancel, or the remote failure by its code. `None`
/// for an I/O error no remote reader made.
#[must_use]
pub fn decode_error(error: &std::io::Error) -> Option<DecodeError> {
    let remote = RemoteError::of(error)?;
    Some(if remote.failure == RemoteFailure::Cancelled {
        DecodeError::Cancelled
    } else {
        DecodeError::Source(SourceFailure::from(remote))
    })
}
