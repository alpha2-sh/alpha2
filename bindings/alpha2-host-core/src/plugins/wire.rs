//! The wire between a host and the worker process that runs a native plugin for it.

use std::io::{self, Read, Write};

use alpha2_model::SuppliedFile;
use serde::{Deserialize, Serialize};

use super::{CANCELLED, PLUGIN_CODES, PluginError};

/// What the worker writes before its first frame.
pub const MAGIC: [u8; 8] = *b"A2PLUGW\n";
/// The version of this wire, which a host and its worker must share.
pub const WIRE_VERSION: u32 = 2;
/// The most bytes a host reads looking for [`MAGIC`]: text a test harness or a loader printed
/// before the worker ran.
pub const MAX_PREAMBLE_BYTES: usize = 4096;
/// The longest frame a worker writes.
pub const MAX_CHILD_FRAME_BYTES: usize = 4 << 20;
/// Bytes of a frame's fields before a video or audio payload, rounded up.
const MAX_HEADER_BYTES: usize = 4096;
/// The longest frame a host writes: a delivered RGBA frame of the largest size, and its fields.
#[expect(
    clippy::cast_possible_truncation,
    reason = "256 MiB fits a 32-bit usize"
)]
pub const MAX_HOST_FRAME_BYTES: usize =
    alpha2_media::MAX_FRAME_PIXELS as usize * 4 + MAX_HEADER_BYTES;

/// Tags of frames a host writes.
pub mod host {
    /// JSON [`super::Load`].
    pub const LOAD: u8 = 1;
    /// Empty.
    pub const DESCRIBE: u8 = 2;
    /// The context's JSON.
    pub const BEGIN: u8 = 3;
    /// `u32` pass, `u32` direction.
    pub const PASS: u8 = 4;
    /// `i64` time, `u32` width, height and format, then the pixels.
    pub const VIDEO: u8 = 5;
    /// `i64` start and start sample, `u32` frames, rate and channels, then `f32` samples.
    pub const AUDIO: u8 = 6;
    /// Empty.
    pub const END: u8 = 7;
    /// Empty; the worker ends.
    pub const SHUTDOWN: u8 = 8;
    /// `u64` length of a memory region the worker maps in place of the one it had; on Unix its
    /// descriptor comes with the frame's first byte, on Windows the `u64` value of the handle
    /// the host duplicated into the worker follows.
    pub const REGION: u8 = 9;
    /// `i64` time, `u32` width, height and format, then the `u64` offset and length of the
    /// pixels in the region.
    pub const VIDEO_SHARED: u8 = 10;
}

/// Tags of frames a worker writes.
pub mod child {
    /// JSON [`super::Hello`].
    pub const HELLO: u8 = 0x40;
    /// `u32` level, then the text.
    pub const LOG: u8 = 0x41;
    /// JSON `{ name, sha256, bytes }`: a supplied file read for the first time.
    pub const RESOURCE_READ: u8 = 0x42;
    /// JSON [`super::Failure`]: a resource read failed in the running call.
    pub const RESOURCE_FAILED: u8 = 0x43;
    /// Emit records the worker took, cut at record boundaries.
    pub const RECORDS: u8 = 0x44;
    /// The text of why the worker refused an emit of the running call.
    pub const REFUSED: u8 = 0x45;
    /// `u32` sequence: the plugin loaded.
    pub const LOADED: u8 = 0x46;
    /// `u32` sequence, then JSON [`super::Failure`].
    pub const LOAD_FAILED: u8 = 0x47;
    /// `u32` sequence, then the descriptor's bytes.
    pub const DESCRIBED: u8 = 0x48;
    /// `u32` sequence, `i32` status, then the `fail` text when the plugin gave one.
    pub const RETURNED: u8 = 0x49;
    /// `u32` sequence: the worker mapped the region.
    pub const REGION_MAPPED: u8 = 0x4A;
    /// `u32` sequence, then JSON [`super::Failure`]: the worker cannot map the region and holds
    /// none.
    pub const REGION_FAILED: u8 = 0x4B;
}

/// The worker's first frame.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Hello {
    /// [`WIRE_VERSION`].
    pub wire: u32,
    /// The version of the `alpha2-host-core` the worker was built with.
    pub host_core: String,
}

/// An error that crosses the wire.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Failure {
    /// A plugin error code or `CANCELLED`.
    pub code: String,
    /// What went wrong.
    pub message: String,
    /// The error's details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Map<String, serde_json::Value>>,
}

impl From<&PluginError> for Failure {
    fn from(error: &PluginError) -> Self {
        Self {
            code: error.code.to_owned(),
            message: error.message.clone(),
            details: error.details.clone(),
        }
    }
}

impl Failure {
    /// The error, when its code is one a plugin fails with.
    #[must_use]
    pub fn into_error(self) -> Option<PluginError> {
        let code = PLUGIN_CODES
            .iter()
            .chain(&[CANCELLED])
            .find(|code| **code == self.code)?;
        let error = PluginError::new(code, self.message);
        Some(match self.details {
            Some(details) => error.with_details(details),
            None => error,
        })
    }
}

/// Why the wire was broken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireError {
    /// The stream ended.
    Closed {
        /// Whether it ended between frames.
        clean: bool,
    },
    /// The frame states a length past the bound.
    Oversized(usize),
    /// The frame cannot be what its tag says, or comes when no such frame may.
    Malformed(String),
    /// Reading or writing failed.
    Io(String),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed { clean: true } => formatter.write_str("the stream ended"),
            Self::Closed { clean: false } => formatter.write_str("a frame was cut short"),
            Self::Oversized(length) => write!(formatter, "a frame of {length} bytes"),
            Self::Malformed(why) => formatter.write_str(why),
            Self::Io(error) => formatter.write_str(error),
        }
    }
}

/// One frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// What it is.
    pub tag: u8,
    /// Its bytes.
    pub payload: Vec<u8>,
}

/// Reads one frame of at most `max` payload bytes: a `u32` length, a `u8` tag and the payload,
/// little-endian.
///
/// # Errors
/// [`WireError::Closed`] at the end of the stream, [`WireError::Oversized`] past `max`, or
/// [`WireError::Io`].
pub fn read_frame(reader: &mut impl Read, max: usize) -> Result<Frame, WireError> {
    let mut header = [0; 5];
    let mut filled = 0;
    while filled < header.len() {
        match reader.read(&mut header[filled..]) {
            Ok(0) => return Err(WireError::Closed { clean: filled == 0 }),
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(WireError::Io(error.to_string())),
        }
    }
    let length = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let length = usize::try_from(length).unwrap_or(usize::MAX);
    if length > max {
        return Err(WireError::Oversized(length));
    }
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(length)
        .map_err(|error| WireError::Io(error.to_string()))?;
    reader
        .take(length as u64)
        .read_to_end(&mut payload)
        .map_err(|error| WireError::Io(error.to_string()))?;
    if payload.len() < length {
        return Err(WireError::Closed { clean: false });
    }
    Ok(Frame {
        tag: header[4],
        payload,
    })
}

/// Writes one frame whose payload is the concatenation of `parts`.
///
/// # Errors
/// When writing fails or the payload is longer than a `u32` states.
pub fn write_frame(writer: &mut impl Write, tag: u8, parts: &[&[u8]]) -> io::Result<()> {
    let length = parts.iter().map(|part| part.len()).sum::<usize>();
    let length = u32::try_from(length)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a frame past 4 GiB"))?;
    let mut header = [0; 5];
    header[..4].copy_from_slice(&length.to_le_bytes());
    header[4] = tag;
    writer.write_all(&header)?;
    for part in parts {
        writer.write_all(part)?;
    }
    writer.flush()
}

/// Skips what precedes [`MAGIC`], at most [`MAX_PREAMBLE_BYTES`].
///
/// # Errors
/// [`WireError::Malformed`] when the magic does not come in time, [`WireError::Closed`] when the
/// stream ends first.
pub fn skip_to_magic(reader: &mut impl Read) -> Result<(), WireError> {
    let mut window = [0_u8; MAGIC.len()];
    let mut seen = 0_usize;
    let mut byte = [0];
    while seen < MAX_PREAMBLE_BYTES + MAGIC.len() {
        match reader.read(&mut byte) {
            Ok(0) => return Err(WireError::Closed { clean: true }),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(WireError::Io(error.to_string())),
        }
        window.copy_within(1.., 0);
        window[MAGIC.len() - 1] = byte[0];
        seen += 1;
        if seen >= MAGIC.len() && window == MAGIC {
            return Ok(());
        }
    }
    Err(WireError::Malformed(format!(
        "no worker magic in the first {MAX_PREAMBLE_BYTES} bytes"
    )))
}

/// A frame a worker wrote, read.
#[derive(Clone, Debug, PartialEq)]
pub enum ChildMessage {
    /// The first frame.
    Hello(Hello),
    /// A diagnostic.
    Log {
        /// Its level.
        level: u32,
        /// Its bytes.
        text: Vec<u8>,
    },
    /// A supplied file read for the first time.
    ResourceRead(SuppliedFile),
    /// A resource read failed in the running call.
    ResourceFailed(Failure),
    /// Records the worker took.
    Records(Vec<u8>),
    /// Why the worker refused an emit.
    Refused(String),
    /// The plugin loaded.
    Loaded {
        /// The request's sequence number.
        sequence: u32,
    },
    /// The plugin did not load.
    LoadFailed {
        /// The request's sequence number.
        sequence: u32,
        /// Why.
        failure: Failure,
    },
    /// The descriptor.
    Described {
        /// The request's sequence number.
        sequence: u32,
        /// Its bytes.
        descriptor: Vec<u8>,
    },
    /// The worker mapped the region.
    RegionMapped {
        /// The request's sequence number.
        sequence: u32,
    },
    /// The worker cannot map the region.
    RegionFailed {
        /// The request's sequence number.
        sequence: u32,
        /// Why.
        failure: Failure,
    },
    /// A call returned.
    Returned {
        /// The request's sequence number.
        sequence: u32,
        /// The plugin's status as it gave it.
        status: i32,
        /// What it passed to `fail`.
        fail: Option<Vec<u8>>,
    },
}

impl ChildMessage {
    /// The sequence number of a reply that ends a request.
    #[must_use]
    pub const fn sequence(&self) -> Option<u32> {
        match self {
            Self::Loaded { sequence }
            | Self::LoadFailed { sequence, .. }
            | Self::Described { sequence, .. }
            | Self::RegionMapped { sequence }
            | Self::RegionFailed { sequence, .. }
            | Self::Returned { sequence, .. } => Some(*sequence),
            _ => None,
        }
    }
}

fn malformed(what: &str) -> WireError {
    WireError::Malformed(format!("a malformed {what} frame"))
}

fn split_u32(payload: &[u8], what: &str) -> Result<(u32, Vec<u8>), WireError> {
    let (head, rest) = payload
        .split_first_chunk::<4>()
        .ok_or_else(|| malformed(what))?;
    Ok((u32::from_le_bytes(*head), rest.to_vec()))
}

fn json<T: for<'de> Deserialize<'de>>(payload: &[u8], what: &str) -> Result<T, WireError> {
    serde_json::from_slice(payload).map_err(|_| malformed(what))
}

fn sha256_hex(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Reads a frame a worker wrote.
///
/// # Errors
/// [`WireError::Malformed`] for an unknown tag or a payload its tag does not allow.
pub fn parse_child(frame: Frame) -> Result<ChildMessage, WireError> {
    let Frame { tag, payload } = frame;
    Ok(match tag {
        child::HELLO => ChildMessage::Hello(json(&payload, "hello")?),
        child::LOG => {
            let (level, text) = split_u32(&payload, "log")?;
            ChildMessage::Log { level, text }
        }
        child::RESOURCE_READ => {
            let file: SuppliedFile = json(&payload, "resource_read")?;
            if !sha256_hex(&file.sha256) || file.name.is_empty() {
                return Err(malformed("resource_read"));
            }
            ChildMessage::ResourceRead(file)
        }
        child::RESOURCE_FAILED => ChildMessage::ResourceFailed(json(&payload, "resource_failed")?),
        child::RECORDS => ChildMessage::Records(payload),
        child::REFUSED => ChildMessage::Refused(String::from_utf8_lossy(&payload).into_owned()),
        child::LOADED if payload.len() == 4 => ChildMessage::Loaded {
            sequence: split_u32(&payload, "loaded")?.0,
        },
        child::LOAD_FAILED => {
            let (sequence, failure) = split_u32(&payload, "load_failed")?;
            ChildMessage::LoadFailed {
                sequence,
                failure: json(&failure, "load_failed")?,
            }
        }
        child::DESCRIBED => {
            let (sequence, descriptor) = split_u32(&payload, "described")?;
            ChildMessage::Described {
                sequence,
                descriptor,
            }
        }
        child::REGION_MAPPED if payload.len() == 4 => ChildMessage::RegionMapped {
            sequence: split_u32(&payload, "region_mapped")?.0,
        },
        child::REGION_FAILED => {
            let (sequence, failure) = split_u32(&payload, "region_failed")?;
            ChildMessage::RegionFailed {
                sequence,
                failure: json(&failure, "region_failed")?,
            }
        }
        child::RETURNED => {
            let (sequence, rest) = split_u32(&payload, "returned")?;
            let (status, fail) = split_u32(&rest, "returned")?;
            ChildMessage::Returned {
                sequence,
                status: status.cast_signed(),
                fail: (!fail.is_empty()).then_some(fail),
            }
        }
        other => {
            return Err(WireError::Malformed(format!(
                "a frame with the unknown tag {other}"
            )));
        }
    })
}

/// What a host asks a worker to load.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Load {
    pub(super) library: std::path::PathBuf,
    pub(super) sha256: String,
    /// Whether the library file holds the pinned bytes, which the worker checks before loading.
    pub(super) check: bool,
    /// The descriptor's outputs and the items each takes in one call.
    pub(super) caps: Vec<(String, usize)>,
    /// What the run may read.
    pub(super) resources: super::resources::ResourceSet,
}

/// Each run of whole records in `batch` that fits a frame, in order.
pub(super) fn record_runs(batch: &[u8]) -> Vec<&[u8]> {
    let mut runs = Vec::new();
    let (mut start, mut at) = (0, 0);
    while at < batch.len() {
        let body = batch
            .get(at + 4..at + 8)
            .and_then(|length| <[u8; 4]>::try_from(length).ok())
            .map_or(usize::MAX, |length| {
                usize::try_from(u32::from_le_bytes(length)).unwrap_or(usize::MAX)
            });
        let next = at.saturating_add(8).saturating_add(body).min(batch.len());
        if next - start > MAX_CHILD_FRAME_BYTES && at > start {
            runs.push(&batch[start..at]);
            start = at;
        }
        at = next;
    }
    if start < batch.len() {
        runs.push(&batch[start..]);
    }
    runs
}
