//! `SimpleBlock` and `BlockGroup` payloads: their header, lacing and group elements.

use bytes::Bytes;

use super::describe::signed;
use super::ebml::{Children, id, read_number, read_signed, uint};
use crate::DemuxError;

/// A block with the facts of its group.
#[derive(Clone, Debug)]
pub(crate) struct Block {
    pub track: u64,
    /// Timestamp relative to the cluster's, in the track's units.
    pub relative: i16,
    pub keyframe: bool,
    /// `BlockDuration`, 0 when absent.
    pub duration: u64,
    /// `DiscardPadding` in nanoseconds.
    pub discard_padding: i64,
    pub frames: Vec<Bytes>,
}

fn invalid(reason: &str) -> DemuxError {
    DemuxError::Invalid(format!("a Matroska block {reason}"))
}

/// The track number and frame count of a block from its first bytes.
pub(crate) fn head(data: &[u8]) -> Option<(u64, usize)> {
    let (number, len) = read_number(data)?;
    let flags = *data.get(len + 2)?;
    let laces = if flags & 0x06 == 0 {
        1
    } else {
        usize::from(*data.get(len + 3)?) + 1
    };
    Some((number, laces))
}

/// Reads a `SimpleBlock` payload, or a `BlockGroup` payload with `group`.
pub(crate) fn parse(data: &Bytes, group: bool) -> Result<Option<Block>, DemuxError> {
    if !group {
        return parse_block(data, None);
    }
    let mut block = None;
    let mut duration = 0;
    let mut references = 0_usize;
    let mut discard_padding = 0;
    for (element, body) in Children::new(data) {
        match element {
            id::BLOCK => block = Some(data.slice_ref(body)),
            id::BLOCK_DURATION => duration = uint(body).unwrap_or(0),
            id::REFERENCE_BLOCK => references += 1,
            id::DISCARD_PADDING => discard_padding = signed(body),
            _ => {}
        }
    }
    let Some(block) = block else {
        return Ok(None);
    };
    let found = parse_block(&block, Some(references == 0))?;
    Ok(found.map(|mut found| {
        found.duration = duration.min(i64::MAX as u64);
        found.discard_padding = discard_padding;
        found
    }))
}

/// The header and frames of a block; `keyframe` comes from the group when there is one.
fn parse_block(data: &Bytes, keyframe: Option<bool>) -> Result<Option<Block>, DemuxError> {
    let Some((track, len)) = read_number(data) else {
        return Err(invalid("has no track number"));
    };
    let Some(header) = data.get(len..len + 3) else {
        return Err(invalid("is truncated"));
    };
    let relative = i16::from_be_bytes([header[0], header[1]]);
    let flags = header[2];
    let frames = laces(&data.slice(len + 3..), (flags >> 1) & 3)?;
    Ok(Some(Block {
        track,
        relative,
        keyframe: keyframe.unwrap_or(flags & 0x80 != 0),
        duration: 0,
        discard_padding: 0,
        frames,
    }))
}

/// Splits a block's payload into frames by its lacing (none, Xiph, fixed or EBML).
fn laces(data: &Bytes, lacing: u8) -> Result<Vec<Bytes>, DemuxError> {
    if lacing == 0 {
        return Ok(vec![data.clone()]);
    }
    let Some(&count) = data.first() else {
        return Err(invalid("has an empty laced payload"));
    };
    let count = usize::from(count) + 1;
    let mut at = 1;
    let mut sizes = Vec::with_capacity(count);
    let total = data.len() - 1;
    match lacing {
        1 => {
            let mut sum = 0_usize;
            for _ in 1..count {
                let mut size = 0_usize;
                loop {
                    let Some(&byte) = data.get(at) else {
                        return Err(invalid("has truncated Xiph lacing"));
                    };
                    if total - (at - 1) <= sum {
                        return Err(invalid("has Xiph lace sizes past its end"));
                    }
                    at += 1;
                    size += usize::from(byte);
                    sum += usize::from(byte);
                    if byte != 255 {
                        break;
                    }
                }
                sizes.push(size);
            }
        }
        2 => {
            if !total.is_multiple_of(count) {
                return Err(invalid("has fixed lacing that does not divide it"));
            }
            sizes.resize(count - 1, total / count);
        }
        _ => {
            let Some((first, len)) = read_number(&data[at..]) else {
                return Err(invalid("has an unreadable EBML lace size"));
            };
            let first = usize::try_from(first)
                .ok()
                .filter(|size| i32::try_from(*size).is_ok())
                .ok_or_else(|| invalid("has an EBML lace size out of range"))?;
            at += len;
            sizes.push(first);
            for _ in 2..count {
                let Some((difference, len)) = read_signed(data.get(at..).unwrap_or(&[])) else {
                    return Err(invalid("has an unreadable EBML lace size"));
                };
                at += len;
                let previous = i64::try_from(*sizes.last().unwrap_or(&0)).unwrap_or(0);
                let size = previous
                    .checked_add(difference)
                    .and_then(|size| usize::try_from(size).ok())
                    .filter(|size| i32::try_from(*size).is_ok())
                    .ok_or_else(|| invalid("has an EBML lace size out of range"))?;
                sizes.push(size);
            }
        }
    }
    // As `FFmpeg` reads it, a lone EBML lace still has a size, which only shortens it.
    let used = sizes
        .iter()
        .try_fold(at, |sum: usize, size| sum.checked_add(*size));
    let Some(last) = used.and_then(|used| data.len().checked_sub(used)) else {
        return Err(invalid("has lace sizes past its end"));
    };
    sizes.truncate(count - 1);
    sizes.push(last);
    let mut frames = Vec::with_capacity(count);
    for size in sizes {
        frames.push(data.slice(at..at + size));
        at += size;
    }
    Ok(frames)
}
