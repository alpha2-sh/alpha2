//! Box headers and big-endian field reads over in-memory box payloads.

use bytes::Bytes;

use crate::{DemuxError, FourCc};

/// A box kept as stored: header and payload, so it can be written back unchanged.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RawBox {
    pub kind: FourCc,
    /// Header length within `bytes`: 8, or 16 with a 64-bit size.
    pub header: usize,
    pub bytes: Bytes,
}

impl RawBox {
    pub fn payload(&self) -> &[u8] {
        &self.bytes[self.header..]
    }
}

/// The child boxes of `data`, as `FFmpeg` walks them: a size of 0 runs to the end, a child that
/// claims more than its parent holds is cut to what remains, and a size below the header ends
/// the walk.
pub(crate) fn children(data: &Bytes) -> Vec<RawBox> {
    let mut found = Vec::new();
    let mut at = 0_usize;
    while data.len().saturating_sub(at) >= 8 {
        let size = u64::from(u32::from_be_bytes([
            data[at],
            data[at + 1],
            data[at + 2],
            data[at + 3],
        ]));
        let kind = FourCc([data[at + 4], data[at + 5], data[at + 6], data[at + 7]]);
        let remaining = (data.len() - at) as u64;
        let (size, header) = match size {
            0 => (remaining, 8),
            1 if remaining >= 16 => {
                let mut wide = [0; 8];
                wide.copy_from_slice(&data[at + 8..at + 16]);
                (u64::from_be_bytes(wide), 16)
            }
            1 => break,
            size => (size, 8),
        };
        if size < header as u64 {
            break;
        }
        let size = usize::try_from(size.min(remaining)).unwrap_or(data.len() - at);
        found.push(RawBox {
            kind,
            header,
            bytes: data.slice(at..at + size),
        });
        at += size;
    }
    found
}

/// The first child of `boxes` of type `kind`.
pub(crate) fn find<'a>(boxes: &'a [RawBox], kind: &[u8; 4]) -> Option<&'a RawBox> {
    boxes.iter().find(|child| &child.kind.0 == kind)
}

fn truncated(what: &str) -> DemuxError {
    DemuxError::Invalid(format!("the {what} box is truncated"))
}

/// A cursor over one box payload; every read is bounds-checked.
pub(crate) struct Reader<'a> {
    data: &'a [u8],
    at: usize,
    what: &'static str,
}

impl<'a> Reader<'a> {
    /// A reader over `data`, naming `what` in errors.
    pub fn new(data: &'a [u8], what: &'static str) -> Self {
        Self { data, at: 0, what }
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.at
    }

    pub fn position(&self) -> usize {
        self.at
    }

    pub fn rest(&self) -> &'a [u8] {
        &self.data[self.at..]
    }

    pub fn bytes(&mut self, count: usize) -> Result<&'a [u8], DemuxError> {
        let end = self
            .at
            .checked_add(count)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| truncated(self.what))?;
        let bytes = &self.data[self.at..end];
        self.at = end;
        Ok(bytes)
    }

    pub fn skip(&mut self, count: usize) -> Result<(), DemuxError> {
        self.bytes(count).map(|_| ())
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DemuxError> {
        let mut out = [0; N];
        out.copy_from_slice(self.bytes(N)?);
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8, DemuxError> {
        Ok(self.array::<1>()?[0])
    }

    pub fn u16(&mut self) -> Result<u16, DemuxError> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    pub fn u24(&mut self) -> Result<u32, DemuxError> {
        let [a, b, c] = self.array()?;
        Ok(u32::from_be_bytes([0, a, b, c]))
    }

    pub fn u32(&mut self) -> Result<u32, DemuxError> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    pub fn i32(&mut self) -> Result<i32, DemuxError> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    pub fn u64(&mut self) -> Result<u64, DemuxError> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    pub fn fourcc(&mut self) -> Result<FourCc, DemuxError> {
        Ok(FourCc(self.array()?))
    }

    /// A full box's version and flags.
    pub fn version_flags(&mut self) -> Result<(u8, u32), DemuxError> {
        let word = self.u32()?;
        Ok(((word >> 24) as u8, word & 0x00ff_ffff))
    }
}
