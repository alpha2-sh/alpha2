//! `alpha2.silence`: the intervals where nobody speaks, each with a `score`, the mean probability
//! that it is not speech, so an editor can choose a threshold without running it again.
//!
//! Every 512-sample block of 16 kHz mono audio goes through Silero VAD (the `vad.onnx` resource)
//! in tract. A block starts speech at `threshold` and ends it below `threshold - 0.15`; between the
//! two it keeps its state. Runs of non-speech become intervals once `end` has every block: each
//! takes the blocks before it that fell below `threshold`, gives up the blocks already rising into
//! the next speech, and is kept when at least `min_silence_ms` long.
//!
//! Params: `threshold` (0.5), `min_silence_ms` (200).

use alpha2_plugin_sdk::{Audio, Context, Emitter, Error, Extractor, Host};
use serde_json::Value;
use tract_onnx::prelude::*;

/// The model's rate.
const RATE: i64 = 16_000;
/// Samples per block, as the descriptor asks.
const BLOCK: usize = 512;
/// Samples of the previous block the model reads before each block.
const CONTEXT: usize = 64;
/// Samples per millisecond at the model's rate.
const SAMPLES_PER_MS: f64 = 16.0;
/// alpha2's clock.
const TICKS_PER_SECOND: i64 = 120_000;
/// The hysteresis below `threshold` at which speech ends.
const RELEASE: f32 = 0.15;
/// The probability from which a block before speech counts as its start: the model rises over a
/// block or two after a word begins.
const RISE: f32 = 0.1;

type Model = std::sync::Arc<TypedRunnableModel>;

fn load(bytes: &[u8]) -> TractResult<Model> {
    tract_onnx::onnx()
        .model_for_read(&mut &*bytes)?
        .with_input_fact(0, f32::fact([1, CONTEXT + BLOCK]).into())?
        .with_input_fact(1, f32::fact([2, 1, 128]).into())?
        .into_optimized()?
        .into_runnable()
}

/// The extractor.
pub struct Silence {
    model: Model,
    state: Tensor,
    context: [f32; CONTEXT],
    /// Each delivered block's first sample and its probability of speech.
    blocks: Vec<(i64, f32)>,
    threshold: f32,
    /// The shortest silence kept, in samples.
    shortest: i64,
    /// The range in samples, the end rounded up.
    range: (i64, i64),
    /// The range's end in ticks.
    range_end: i64,
}

fn number(context: &Context, key: &str, default: f64) -> f64 {
    context
        .params
        .get(key)
        .and_then(Value::as_f64)
        .unwrap_or(default)
}

fn to_sample(ticks: i64) -> i64 {
    ticks * RATE / TICKS_PER_SECOND
}

fn to_ticks(sample: i64) -> i64 {
    sample * TICKS_PER_SECOND / RATE
}

impl Silence {
    fn fresh_state() -> Tensor {
        Tensor::zero::<f32>(&[2, 1, 128]).unwrap_or_else(|_| tensor0(0.0_f32))
    }

    /// The probability of speech in `samples`, one block, after the previous block's context.
    fn probability(&mut self, samples: &[f32]) -> Result<f32, Error> {
        let mut input = Vec::with_capacity(CONTEXT + BLOCK);
        input.extend_from_slice(&self.context);
        input.extend_from_slice(samples);
        input.resize(CONTEXT + BLOCK, 0.0);
        let input = Tensor::from_shape(&[1, CONTEXT + BLOCK], &input)
            .map_err(|error| Error(error.to_string()))?;
        let state = std::mem::replace(&mut self.state, Self::fresh_state());
        let mut outputs = self
            .model
            .run(tvec!(input.into_tvalue(), state.into_tvalue()))
            .map_err(|error| Error(format!("the model failed: {error}")))?;
        let next = outputs.pop().ok_or("the model gave no state")?;
        let probability = outputs.pop().ok_or("the model gave no probability")?;
        self.state = next.into_tensor();
        let start = samples.len().saturating_sub(CONTEXT);
        self.context
            .copy_from_slice(&samples[start..start + CONTEXT.min(samples.len())]);
        let value = probability
            .to_plain_array_view::<f32>()
            .ok()
            .and_then(|values| values.iter().next().copied())
            .ok_or("the probability is not a float")?;
        Ok(value)
    }
}

impl Extractor for Silence {
    const DESCRIPTOR: &'static str = include_str!("../descriptor.json");

    fn begin(context: &Context, host: &Host) -> Result<Self, Error> {
        let bytes = host
            .resource("vad.onnx")
            .ok_or("no resource named vad.onnx")?;
        let model =
            load(&bytes).map_err(|error| Error(format!("vad.onnx does not load: {error}")))?;
        #[allow(clippy::cast_possible_truncation)]
        let threshold = number(context, "threshold", 0.5) as f32;
        #[allow(clippy::cast_possible_truncation)]
        let shortest = (number(context, "min_silence_ms", 200.0) * SAMPLES_PER_MS) as i64;
        Ok(Self {
            model,
            state: Self::fresh_state(),
            context: [0.0; CONTEXT],
            blocks: Vec::new(),
            threshold,
            shortest,
            range: (
                to_sample(context.range.start),
                (context.range.end * RATE).div_euclid(TICKS_PER_SECOND)
                    + i64::from((context.range.end * RATE).rem_euclid(TICKS_PER_SECOND) != 0),
            ),
            range_end: context.range.end,
        })
    }

    fn pass(&mut self, _pass: u32, _direction: alpha2_plugin_sdk::Direction) -> Result<(), Error> {
        self.state = Self::fresh_state();
        self.context = [0.0; CONTEXT];
        Ok(())
    }

    fn audio(&mut self, block: &Audio<'_>, _out: &mut Emitter) -> Result<(), Error> {
        let frames = usize::try_from(block.frames)
            .unwrap_or(0)
            .min(block.samples.len());
        let probability = self.probability(&block.samples[..frames])?;
        self.blocks.push((block.start_sample, probability));
        Ok(())
    }

    fn end(&mut self, out: &mut Emitter) -> Result<(), Error> {
        self.blocks.sort_by_key(|(start, _)| *start);
        self.blocks.dedup_by_key(|(start, _)| *start);
        let speech = self.speech();
        let (first, last) = self.range;
        let block = i64::try_from(BLOCK).unwrap_or(i64::MAX);
        let mut index = 0;
        while index < self.blocks.len() {
            if speech[index] {
                index += 1;
                continue;
            }
            let run_end = (index..self.blocks.len())
                .find(|&at| speech[at])
                .unwrap_or(self.blocks.len());
            // Blocks that already fell below the threshold before the run belong to it; blocks
            // already rising into the next speech do not.
            let mut from = index;
            while from > 0 && self.blocks[from - 1].1 < self.threshold {
                from -= 1;
            }
            let mut to = run_end;
            while run_end < self.blocks.len() && to > from + 1 && self.blocks[to - 1].1 >= RISE {
                to -= 1;
            }
            let start = if from == 0 {
                first
            } else {
                self.blocks[from].0
            };
            let end = if run_end == self.blocks.len() {
                last
            } else {
                self.blocks[to].0
            };
            let (start, end) = (
                start.max(first),
                end.min(last).min(self.blocks[to - 1].0 + block).max(start),
            );
            let end = if run_end == self.blocks.len() {
                last
            } else {
                end
            };
            #[allow(clippy::cast_precision_loss)]
            let score = 1.0
                - self.blocks[from..to]
                    .iter()
                    .map(|(_, probability)| probability)
                    .sum::<f32>()
                    / (to - from) as f32;
            if end - start >= self.shortest.max(1) {
                let end = to_ticks(end).min(self.range_end);
                out.interval("silence", to_ticks(start), end, None, Some(score));
            }
            index = run_end;
        }
        Ok(())
    }
}

impl Silence {
    /// Whether each block is speech: it starts at `threshold` and ends below `threshold - 0.15`.
    fn speech(&self) -> Vec<bool> {
        let mut speaking = false;
        self.blocks
            .iter()
            .map(|&(_, probability)| {
                if probability >= self.threshold {
                    speaking = true;
                } else if probability < self.threshold - RELEASE {
                    speaking = false;
                }
                speaking
            })
            .collect()
    }
}

alpha2_plugin_sdk::export_extractor!(Silence);
