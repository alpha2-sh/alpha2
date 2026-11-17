//! Frames of the host protocol: an 8-byte preamble, a JSON header and binary attachments.
//!
//! The preamble holds the header's length and the attachments' total length, both little-endian
//! `u32`, so a reader finds the next frame without parsing the header. The header's `attachments`
//! lists each attachment's `type` and `bytes`, in order.

use std::io::{self, BufRead, Read, Write};

use serde_json::Value;

/// What `serve` writes before its first frame, so a reader skips whatever reached the stream
/// before the host owned it.
pub const MAGIC: [u8; 8] = *b"\x89A2HOST\n";

const PREAMBLE_BYTES: usize = 8;

/// The most an attachment's `type` may hold.
pub const MAX_ATTACHMENT_TYPE_BYTES: usize = 255;

/// The largest frame a reader takes; a larger one is read through and dropped.
#[derive(Clone, Copy, Debug)]
pub struct FrameLimits {
    /// Header bytes.
    pub header_bytes: u32,
    /// Attachment bytes, all together.
    pub attachment_bytes: u32,
}

/// A frame as read: its header and its attachments' bytes, not yet split.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Frame {
    /// The header's bytes, which should be one JSON object in UTF-8.
    pub header: Vec<u8>,
    /// Every attachment's bytes, in order.
    pub attachments: Vec<u8>,
}

/// One attachment of a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    /// A media type, such as `image/png`.
    pub kind: String,
    /// Its bytes.
    pub bytes: Vec<u8>,
}

/// What [`read_frame`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadFrame {
    /// A frame within the limits.
    Frame(Frame),
    /// A frame over a limit, skipped whole so the next one is read from its start.
    TooLarge {
        /// Its header's length.
        header_bytes: u32,
        /// Its attachments' length.
        attachment_bytes: u32,
    },
    /// The stream ended, or ended inside a frame.
    End,
}

/// Reads the next frame.
///
/// # Errors
/// A read error other than the end of the stream.
pub fn read_frame(input: &mut impl Read, limits: FrameLimits) -> io::Result<ReadFrame> {
    let mut preamble = [0_u8; PREAMBLE_BYTES];
    if !read_whole(input, &mut preamble)? {
        return Ok(ReadFrame::End);
    }
    let header_bytes = u32::from_le_bytes([preamble[0], preamble[1], preamble[2], preamble[3]]);
    let attachment_bytes = u32::from_le_bytes([preamble[4], preamble[5], preamble[6], preamble[7]]);
    if header_bytes > limits.header_bytes || attachment_bytes > limits.attachment_bytes {
        let skip = u64::from(header_bytes) + u64::from(attachment_bytes);
        let skipped = io::copy(&mut input.take(skip), &mut io::sink())?;
        if skipped < skip {
            return Ok(ReadFrame::End);
        }
        return Ok(ReadFrame::TooLarge {
            header_bytes,
            attachment_bytes,
        });
    }
    let mut header = vec![0_u8; header_bytes as usize];
    let mut attachments = vec![0_u8; attachment_bytes as usize];
    if !read_whole(input, &mut header)? || !read_whole(input, &mut attachments)? {
        return Ok(ReadFrame::End);
    }
    Ok(ReadFrame::Frame(Frame {
        header,
        attachments,
    }))
}

/// `false` when the stream ended first.
fn read_whole(input: &mut impl Read, buffer: &mut [u8]) -> io::Result<bool> {
    match input.read_exact(buffer) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error),
    }
}

/// One frame's bytes, preamble included, so it is written in one piece.
///
/// # Errors
/// `InvalidInput` when the header or the attachments together exceed 4 GiB.
pub fn frame_bytes(header: &[u8], attachments: &[&[u8]]) -> io::Result<Vec<u8>> {
    let too_large = |what: &str| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("a frame's {what} is at most 4 GiB"),
        )
    };
    let header_bytes = u32::try_from(header.len()).map_err(|_| too_large("header"))?;
    let total = attachments
        .iter()
        .try_fold(0_u32, |total, attachment| {
            u32::try_from(attachment.len())
                .ok()
                .and_then(|bytes| total.checked_add(bytes))
        })
        .ok_or_else(|| too_large("attachments"))?;
    let mut frame = Vec::with_capacity(PREAMBLE_BYTES + header.len() + total as usize);
    frame.extend_from_slice(&header_bytes.to_le_bytes());
    frame.extend_from_slice(&total.to_le_bytes());
    frame.extend_from_slice(header);
    for attachment in attachments {
        frame.extend_from_slice(attachment);
    }
    Ok(frame)
}

/// Writes one frame and flushes it.
///
/// # Errors
/// A frame over 4 GiB, or the output's error.
pub fn write_frame(
    output: &mut impl Write,
    header: &[u8],
    attachments: &[&[u8]],
) -> io::Result<()> {
    output.write_all(&frame_bytes(header, attachments)?)?;
    output.flush()
}

/// Splits `bytes` as the header's `attachments` list says.
///
/// # Errors
/// A list that is not an array of `{type, bytes}`, an empty or overlong `type`, or lengths that do
/// not add up to `bytes`.
pub fn split_attachments(header: &Value, bytes: Vec<u8>) -> Result<Vec<Attachment>, String> {
    let listed = match header.get("attachments") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(listed)) => listed.as_slice(),
        Some(_) => return Err("`attachments` is a list of {type, bytes}".to_owned()),
    };
    let mut declared = Vec::with_capacity(listed.len());
    let mut total = 0_u64;
    for (index, entry) in listed.iter().enumerate() {
        let kind = entry
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| !kind.is_empty() && kind.len() <= MAX_ATTACHMENT_TYPE_BYTES)
            .ok_or_else(|| {
                format!(
                    "attachments[{index}].type is a media type of 1 to {MAX_ATTACHMENT_TYPE_BYTES} bytes"
                )
            })?;
        let length = entry
            .get("bytes")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("attachments[{index}].bytes is a byte count"))?;
        total = total.saturating_add(length);
        declared.push((kind.to_owned(), length));
    }
    if total != bytes.len() as u64 {
        return Err(format!(
            "the attachments list {total} bytes; the frame carries {}",
            bytes.len()
        ));
    }
    let mut rest = bytes;
    let mut attachments = Vec::with_capacity(declared.len());
    for (kind, length) in declared.into_iter().rev() {
        let at = rest.len() - usize::try_from(length).unwrap_or(rest.len());
        attachments.push(Attachment {
            kind,
            bytes: rest.split_off(at),
        });
    }
    attachments.reverse();
    Ok(attachments)
}

/// Reads up to and through [`MAGIC`] and returns what came before it; `None` when the stream ended
/// or `limit` bytes passed first.
///
/// # Errors
/// A read error other than the end of the stream.
pub fn skip_to_magic(input: &mut impl BufRead, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut seen = Vec::new();
    loop {
        let available = match input.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            return Ok(None);
        }
        let byte = available[0];
        input.consume(1);
        seen.push(byte);
        if seen.ends_with(&MAGIC) {
            seen.truncate(seen.len() - MAGIC.len());
            return Ok(Some(seen));
        }
        if seen.len() > limit.saturating_add(MAGIC.len()) {
            return Ok(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: FrameLimits = FrameLimits {
        header_bytes: 64,
        attachment_bytes: 64,
    };

    #[test]
    fn frames_round_trip_and_an_oversized_one_is_skipped_whole() {
        let mut stream = Vec::new();
        write_frame(&mut stream, br#"{"a":1}"#, &[]).expect("write");
        write_frame(&mut stream, &[b'x'; 65], &[]).expect("write");
        write_frame(&mut stream, br#"{"b":2}"#, &[b"12", b"345"]).expect("write");
        let mut input = stream.as_slice();
        assert_eq!(
            read_frame(&mut input, LIMITS).expect("read"),
            ReadFrame::Frame(Frame {
                header: br#"{"a":1}"#.to_vec(),
                attachments: Vec::new(),
            })
        );
        assert_eq!(
            read_frame(&mut input, LIMITS).expect("read"),
            ReadFrame::TooLarge {
                header_bytes: 65,
                attachment_bytes: 0
            }
        );
        assert_eq!(
            read_frame(&mut input, LIMITS).expect("read"),
            ReadFrame::Frame(Frame {
                header: br#"{"b":2}"#.to_vec(),
                attachments: b"12345".to_vec(),
            })
        );
        assert_eq!(
            read_frame(&mut input, LIMITS).expect("read"),
            ReadFrame::End
        );
    }

    #[test]
    fn a_stream_that_ends_inside_a_frame_ends() {
        let mut stream = Vec::new();
        write_frame(&mut stream, br#"{"a":1}"#, &[b"abc"]).expect("write");
        for cut in 1..stream.len() {
            assert_eq!(
                read_frame(&mut &stream[..cut], LIMITS).expect("read"),
                ReadFrame::End,
                "{cut}"
            );
        }
    }

    #[test]
    fn attachments_split_by_the_header_list() {
        let header = serde_json::json!({
            "attachments": [{"type": "image/png", "bytes": 2}, {"type": "a/b", "bytes": 3}]
        });
        let split = split_attachments(&header, b"12345".to_vec()).expect("split");
        assert_eq!(split.len(), 2);
        assert_eq!(
            (split[0].kind.as_str(), split[0].bytes.as_slice()),
            ("image/png", &b"12"[..])
        );
        assert_eq!(
            (split[1].kind.as_str(), split[1].bytes.as_slice()),
            ("a/b", &b"345"[..])
        );
        assert_eq!(
            split_attachments(&serde_json::json!({}), Vec::new()),
            Ok(Vec::new())
        );
        for refused in [
            serde_json::json!({"attachments": [{"type": "a/b", "bytes": 4}]}),
            serde_json::json!({"attachments": [{"type": "", "bytes": 5}]}),
            serde_json::json!({"attachments": [{"bytes": 5}]}),
            serde_json::json!({"attachments": {"type": "a/b", "bytes": 5}}),
            serde_json::json!({"attachments": [{"type": "a/b", "bytes": u64::MAX}, {"type": "a/b", "bytes": 6}]}),
        ] {
            assert!(
                split_attachments(&refused, b"12345".to_vec()).is_err(),
                "{refused}"
            );
        }
        assert!(split_attachments(&serde_json::json!({}), b"1".to_vec()).is_err());
    }

    #[test]
    fn the_magic_skips_what_came_before_it() {
        let mut stream = b"running 1 test\n".to_vec();
        stream.extend_from_slice(&MAGIC);
        stream.extend_from_slice(b"rest");
        let mut input = stream.as_slice();
        assert_eq!(
            skip_to_magic(&mut input, 1024).expect("read"),
            Some(b"running 1 test\n".to_vec())
        );
        assert_eq!(input, b"rest");
        assert_eq!(
            skip_to_magic(&mut &b"no magic"[..], 1024).expect("read"),
            None
        );
        assert_eq!(skip_to_magic(&mut &stream[..], 4).expect("read"), None);
    }
}
