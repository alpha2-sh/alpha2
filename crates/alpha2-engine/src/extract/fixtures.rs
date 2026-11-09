//! Test fixtures that never ship. `scripted` emits what it is given, and `digest` hashes what it
//! is supplied so equal data across surfaces proves byte-identical inputs.

use super::descriptor::{AudioFormat, Descriptor, InputFrom, InputSpec, OutputSpec, VideoFormat};
use super::drive::{Emitter, Extractor, ExtractorError, PassInfo};
use super::session::{AudioInput, Emit, RunContext, VideoInput};
use alpha2_model::{DataInterpolation, DataKind};

/// The digest fixture's id.
pub const DIGEST_ID: &str = "alpha2.test.digest";
/// The scripted fixture's id.
pub const SCRIPTED_ID: &str = "alpha2.test.scripted";

/// 32-bit FNV-1a of `bytes`.
#[must_use]
pub fn fnv1a(bytes: impl IntoIterator<Item = u8>) -> u32 {
    bytes.into_iter().fold(0x811c_9dc5, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

/// A hash as the two scalars the digest emits, its 16-bit halves over 65536, exact in f32.
#[must_use]
pub fn digest_values(hash: u32) -> [f32; 2] {
    let half = |bits: u32| f32::from(u16::try_from(bits & 0xffff).unwrap_or(0)) / 65_536.0;
    [half(hash >> 16), half(hash)]
}

/// The digest's descriptor for `input`, one `scalars` output `digest` with channels `hi`, `lo`.
#[must_use]
pub fn digest_descriptor(input: InputSpec) -> Descriptor {
    Descriptor {
        id: DIGEST_ID.to_owned(),
        version: 1,
        input,
        params_schema: serde_json::json!({ "type": "object" }),
        outputs: vec![OutputSpec {
            name: "digest".to_owned(),
            kind: DataKind::Scalars,
            channels: vec!["hi".to_owned(), "lo".to_owned()],
            interpolation: Some(DataInterpolation::Hold),
            suggest: None,
        }],
    }
}

/// Video input from sources and the timeline in `format`, at most `max_size` pixels.
#[must_use]
pub fn video_input(format: VideoFormat, max_size: u32) -> InputSpec {
    InputSpec::Video {
        from: vec![InputFrom::Source, InputFrom::Timeline],
        format,
        max_size,
    }
}

/// Audio input from sources and the timeline, at any rate and layout, in blocks of `block`.
#[must_use]
pub fn audio_input(block: u32) -> InputSpec {
    InputSpec::Audio {
        from: vec![InputFrom::Source, InputFrom::Timeline],
        format: AudioFormat::F32,
        sample_rate: None,
        channels: None,
        block_frames: block,
    }
}

/// Emits, per delivered frame or block, the FNV-1a hash of its bytes at its time.
#[derive(Clone, Debug)]
pub struct Digest {
    descriptor: Descriptor,
}

impl Digest {
    /// The fixture for `input`.
    #[must_use]
    pub fn new(input: InputSpec) -> Self {
        Self {
            descriptor: digest_descriptor(input),
        }
    }
}

impl Extractor for Digest {
    fn describe(&self) -> &Descriptor {
        &self.descriptor
    }

    fn begin(&mut self, _context: &RunContext) -> Result<(), ExtractorError> {
        Ok(())
    }

    fn pass(&mut self, _pass: &PassInfo) -> Result<(), ExtractorError> {
        Ok(())
    }

    fn video(&mut self, frame: &VideoInput, out: &mut Emitter) -> Result<(), ExtractorError> {
        let [hi, lo] = digest_values(fnv1a(frame.bytes.iter().copied()));
        out.sample("digest", frame.time, &[Some(hi), Some(lo)]);
        Ok(())
    }

    fn audio(&mut self, block: &AudioInput, out: &mut Emitter) -> Result<(), ExtractorError> {
        let [hi, lo] = digest_values(fnv1a(
            block.samples.iter().flat_map(|sample| sample.to_le_bytes()),
        ));
        out.sample("digest", block.start, &[Some(hi), Some(lo)]);
        Ok(())
    }

    fn end(&mut self, _out: &mut Emitter) -> Result<(), ExtractorError> {
        Ok(())
    }
}

/// Emits a given list per delivery, in order, and another at the end.
#[derive(Clone, Debug)]
pub struct Scripted {
    descriptor: Descriptor,
    per_delivery: std::collections::VecDeque<Vec<Emit>>,
    at_end: Vec<Emit>,
}

impl Scripted {
    /// The fixture declaring `outputs` over `input`; deliveries past `per_delivery` emit nothing.
    #[must_use]
    pub fn new(
        input: InputSpec,
        outputs: Vec<OutputSpec>,
        per_delivery: Vec<Vec<Emit>>,
        at_end: Vec<Emit>,
    ) -> Self {
        Self {
            descriptor: Descriptor {
                id: SCRIPTED_ID.to_owned(),
                version: 1,
                input,
                params_schema: serde_json::json!({ "type": "object" }),
                outputs,
            },
            per_delivery: per_delivery.into(),
            at_end,
        }
    }

    fn emit_next(&mut self, out: &mut Emitter) {
        for emit in self.per_delivery.pop_front().unwrap_or_default() {
            push(out, emit);
        }
    }
}

fn push(out: &mut Emitter, emit: Emit) {
    match emit {
        Emit::Sample {
            output,
            time,
            values,
        } => out.sample(&output, time, &values),
        Emit::Word { output, word } => out.word(&output, word),
        Emit::Interval { output, interval } => out.interval(&output, interval),
    }
}

impl Extractor for Scripted {
    fn describe(&self) -> &Descriptor {
        &self.descriptor
    }

    fn begin(&mut self, _context: &RunContext) -> Result<(), ExtractorError> {
        Ok(())
    }

    fn pass(&mut self, _pass: &PassInfo) -> Result<(), ExtractorError> {
        Ok(())
    }

    fn video(&mut self, _frame: &VideoInput, out: &mut Emitter) -> Result<(), ExtractorError> {
        self.emit_next(out);
        Ok(())
    }

    fn audio(&mut self, _block: &AudioInput, out: &mut Emitter) -> Result<(), ExtractorError> {
        self.emit_next(out);
        Ok(())
    }

    fn end(&mut self, out: &mut Emitter) -> Result<(), ExtractorError> {
        for emit in std::mem::take(&mut self.at_end) {
            push(out, emit);
        }
        Ok(())
    }
}
