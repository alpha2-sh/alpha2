//! Emit records: what a plugin passes to `emit`, one batch per call.
//!
//! A batch is records back to back. Each record is an 8-byte header (`u8` tag, `u8` 0, `u16`
//! output index in the descriptor's `outputs`, `u32` body length) and its body. Integers and floats
//! are little-endian and unaligned.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// Header tag of a [`Record::Sample`].
pub const SAMPLE: u8 = 1;
/// Header tag of a [`Record::Word`].
pub const WORD: u8 = 2;
/// Header tag of a [`Record::Interval`].
pub const INTERVAL: u8 = 3;

/// Length of a text field that is absent.
const ABSENT: u32 = u32::MAX;

/// One emitted value, by output index.
#[derive(Clone, Debug, PartialEq)]
pub enum Record {
    /// A sample of a `points`, `scalars` or `similarity` output; `None` is a lost channel.
    Sample {
        /// Index in the descriptor's `outputs`.
        output: u16,
        /// Ticks on the input's clock.
        time: i64,
        /// One per channel component.
        values: Vec<Option<f32>>,
    },
    /// A word of a `words` output.
    Word {
        /// Index in the descriptor's `outputs`.
        output: u16,
        /// Ticks on the input's clock.
        start: i64,
        /// Ticks on the input's clock.
        end: i64,
        /// The word.
        text: String,
        /// Who said it.
        speaker: Option<String>,
    },
    /// An interval of an `intervals` output.
    Interval {
        /// Index in the descriptor's `outputs`.
        output: u16,
        /// Ticks on the input's clock.
        start: i64,
        /// Ticks on the input's clock.
        end: i64,
        /// What the interval is.
        label: Option<String>,
        /// How sure the extractor is.
        score: Option<f32>,
    },
}

/// Why a batch could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordError {
    /// Index of the record that could not be read.
    pub index: usize,
    /// What is wrong with it.
    pub reason: &'static str,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "record {}: {}", self.index, self.reason)
    }
}

impl core::error::Error for RecordError {}

/// Appends `record` to `out`.
///
/// # Panics
/// When a body or a text is longer than `u32::MAX` bytes.
pub fn encode(record: &Record, out: &mut Vec<u8>) {
    let header = out.len();
    let (tag, output) = match record {
        Record::Sample { output, .. } => (SAMPLE, *output),
        Record::Word { output, .. } => (WORD, *output),
        Record::Interval { output, .. } => (INTERVAL, *output),
    };
    out.extend_from_slice(&[tag, 0]);
    out.extend_from_slice(&output.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    let body = out.len();
    match record {
        Record::Sample { time, values, .. } => {
            out.extend_from_slice(&time.to_le_bytes());
            out.extend_from_slice(&length(values.len()).to_le_bytes());
            for word in values.chunks(32) {
                let lost = word
                    .iter()
                    .enumerate()
                    .filter(|(_, value)| value.is_none())
                    .fold(0_u32, |mask, (bit, _)| mask | 1 << bit);
                out.extend_from_slice(&lost.to_le_bytes());
            }
            for value in values {
                out.extend_from_slice(&value.unwrap_or(0.0).to_le_bytes());
            }
        }
        Record::Word {
            start,
            end,
            text,
            speaker,
            ..
        } => {
            out.extend_from_slice(&start.to_le_bytes());
            out.extend_from_slice(&end.to_le_bytes());
            out.extend_from_slice(&length(text.len()).to_le_bytes());
            out.extend_from_slice(&optional_length(speaker.as_deref()).to_le_bytes());
            out.extend_from_slice(text.as_bytes());
            out.extend_from_slice(speaker.as_deref().unwrap_or_default().as_bytes());
        }
        Record::Interval {
            start,
            end,
            label,
            score,
            ..
        } => {
            out.extend_from_slice(&start.to_le_bytes());
            out.extend_from_slice(&end.to_le_bytes());
            out.extend_from_slice(&u32::from(score.is_some()).to_le_bytes());
            out.extend_from_slice(&score.unwrap_or(0.0).to_le_bytes());
            out.extend_from_slice(&optional_length(label.as_deref()).to_le_bytes());
            out.extend_from_slice(label.as_deref().unwrap_or_default().as_bytes());
        }
    }
    let size = length(out.len() - body).to_le_bytes();
    out[header + 4..header + 8].copy_from_slice(&size);
}

fn length(len: usize) -> u32 {
    u32::try_from(len)
        .ok()
        .filter(|len| *len != ABSENT)
        .expect("record field too long")
}

fn optional_length(text: Option<&str>) -> u32 {
    text.map_or(ABSENT, |text| length(text.len()))
}

/// Reads a whole batch.
///
/// # Errors
/// The first record with an unknown tag, a nonzero reserved byte, a body that ends early or has
/// bytes left over, or text that is not UTF-8.
pub fn decode(mut batch: &[u8]) -> Result<Vec<Record>, RecordError> {
    let mut records = Vec::new();
    while !batch.is_empty() {
        let index = records.len();
        let fail = |reason| RecordError { index, reason };
        let mut header = Reader(batch);
        let tag = header.u8().ok_or(fail("the header ends early"))?;
        let reserved = header.u8().ok_or(fail("the header ends early"))?;
        let output = header.u16().ok_or(fail("the header ends early"))?;
        let size = header.u32().ok_or(fail("the header ends early"))? as usize;
        if reserved != 0 {
            return Err(fail("the reserved header byte is not 0"));
        }
        let body = header
            .take(size)
            .ok_or(fail("the body is shorter than its length"))?;
        batch = header.0;
        let mut body = Reader(body);
        let record = match tag {
            SAMPLE => body.sample(output),
            WORD => body.word(output),
            INTERVAL => body.interval(output),
            _ => Err("unknown record tag"),
        }
        .map_err(fail)?;
        if !body.0.is_empty() {
            return Err(fail("the body is longer than its fields"));
        }
        records.push(record);
    }
    Ok(records)
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        if self.0.len() < count {
            return None;
        }
        let (taken, rest) = self.0.split_at(count);
        self.0 = rest;
        Some(taken)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    fn u8(&mut self) -> Option<u8> {
        self.array::<1>().map(|[byte]| byte)
    }

    fn u16(&mut self) -> Option<u16> {
        self.array().map(u16::from_le_bytes)
    }

    fn u32(&mut self) -> Option<u32> {
        self.array().map(u32::from_le_bytes)
    }

    fn i64(&mut self) -> Option<i64> {
        self.array().map(i64::from_le_bytes)
    }

    fn f32(&mut self) -> Option<f32> {
        self.array().map(f32::from_le_bytes)
    }

    fn text(&mut self, len: u32) -> Result<Option<String>, &'static str> {
        if len == ABSENT {
            return Ok(None);
        }
        let bytes = self.take(len as usize).ok_or("a text runs past the body")?;
        core::str::from_utf8(bytes)
            .map(|text| Some(String::from(text)))
            .map_err(|_| "a text is not UTF-8")
    }

    fn sample(&mut self, output: u16) -> Result<Record, &'static str> {
        const SHORT: &str = "the sample's fields end early";
        let time = self.i64().ok_or(SHORT)?;
        let count = self.u32().ok_or(SHORT)? as usize;
        // Every value takes at least 4 bytes, so a count the body cannot hold is refused before
        // anything is allocated for it.
        if count > self.0.len() / 4 {
            return Err(SHORT);
        }
        let mut lost = Vec::with_capacity(count.div_ceil(32));
        for _ in 0..count.div_ceil(32) {
            lost.push(self.u32().ok_or(SHORT)?);
        }
        let mut values = Vec::with_capacity(count);
        for index in 0..count {
            let value = self.f32().ok_or(SHORT)?;
            let is_lost = lost[index / 32] >> (index % 32) & 1 == 1;
            values.push((!is_lost).then_some(value));
        }
        if lost
            .last()
            .is_some_and(|mask| !count.is_multiple_of(32) && mask >> (count % 32) != 0)
        {
            return Err("a lost mask marks a value past the count");
        }
        Ok(Record::Sample {
            output,
            time,
            values,
        })
    }

    fn word(&mut self, output: u16) -> Result<Record, &'static str> {
        const SHORT: &str = "the word's fields end early";
        let start = self.i64().ok_or(SHORT)?;
        let end = self.i64().ok_or(SHORT)?;
        let text_len = self.u32().ok_or(SHORT)?;
        let speaker_len = self.u32().ok_or(SHORT)?;
        let text = self.text(text_len)?.ok_or("a word has no text")?;
        let speaker = self.text(speaker_len)?;
        Ok(Record::Word {
            output,
            start,
            end,
            text,
            speaker,
        })
    }

    fn interval(&mut self, output: u16) -> Result<Record, &'static str> {
        const SHORT: &str = "the interval's fields end early";
        let start = self.i64().ok_or(SHORT)?;
        let end = self.i64().ok_or(SHORT)?;
        let flags = self.u32().ok_or(SHORT)?;
        let score = self.f32().ok_or(SHORT)?;
        let label_len = self.u32().ok_or(SHORT)?;
        if flags > 1 {
            return Err("unknown interval flags");
        }
        let label = self.text(label_len)?;
        Ok(Record::Interval {
            output,
            start,
            end,
            label,
            score: (flags == 1).then_some(score),
        })
    }
}
