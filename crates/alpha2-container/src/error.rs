//! Demux and mux failures.

use alpha2_media::{DecodeError, EncodeError};

/// Why a file could not be read.
#[derive(Debug)]
pub enum DemuxError {
    /// A format, codec layout or feature this layer does not read.
    Unsupported(String),
    /// Data that breaks its format's rules or a size limit.
    Invalid(String),
    /// A track id the reader does not have.
    NoSuchTrack(u32),
    /// The file could not be read.
    Io(std::io::Error),
}

impl std::fmt::Display for DemuxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(reason) => write!(formatter, "{reason}"),
            Self::Invalid(reason) => write!(formatter, "invalid media: {reason}"),
            Self::NoSuchTrack(track) => write!(formatter, "no track {track}"),
            Self::Io(error) => write!(formatter, "cannot read the file: {error}"),
        }
    }
}

impl std::error::Error for DemuxError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for DemuxError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<DemuxError> for DecodeError {
    fn from(error: DemuxError) -> Self {
        match error {
            DemuxError::Unsupported(reason) => Self::Unsupported(reason),
            error => Self::Adapter(error.to_string()),
        }
    }
}

/// Why a file could not be written.
#[derive(Debug)]
pub enum MuxError {
    /// A codec or track layout the container cannot carry.
    Unsupported(String),
    /// Packets out of order, or a track added too late.
    Invalid(String),
    /// The file could not be written.
    Io(std::io::Error),
}

impl std::fmt::Display for MuxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(reason) | Self::Invalid(reason) => write!(formatter, "{reason}"),
            Self::Io(error) => write!(formatter, "cannot write the file: {error}"),
        }
    }
}

impl std::error::Error for MuxError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for MuxError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<MuxError> for EncodeError {
    fn from(error: MuxError) -> Self {
        match error {
            MuxError::Io(error) => Self::Output(error.to_string()),
            error => Self::Adapter(error.to_string()),
        }
    }
}
