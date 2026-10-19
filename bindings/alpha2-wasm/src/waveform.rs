//! `WaveformBuilder`: waveform summaries (`alpha2-audio`) for the web.

use alpha2_audio::{Waveform, WaveformSummary};
use wasm_bindgen::prelude::*;

use crate::web::{invalid_input, invalid_state};

/// Builds a multi-resolution waveform summary from planar samples streamed in
/// blocks.
#[wasm_bindgen]
pub struct WaveformBuilder {
    summary: Option<WaveformSummary>,
    channels: usize,
}

#[wasm_bindgen]
impl WaveformBuilder {
    /// A builder for `channels` channels, `bucket_samples` samples per finest
    /// bucket, and `levels` zoom levels, each four times coarser.
    ///
    /// # Errors
    /// Rejects zero channels, a zero bucket, or 0 or more than 16 levels.
    #[wasm_bindgen(constructor)]
    pub fn new(channels: usize, bucket_samples: usize, levels: usize) -> Result<Self, JsValue> {
        let summary =
            WaveformSummary::new(channels, bucket_samples, levels).map_err(invalid_input)?;
        Ok(Self {
            summary: Some(summary),
            channels,
        })
    }

    /// Adds one block: the channels' samples concatenated, channel after
    /// channel, all of one length.
    ///
    /// # Errors
    /// Rejects a block whose length is not a multiple of the channel count,
    /// or a builder already finished.
    pub fn push(&mut self, planar: &[f32]) -> Result<(), JsValue> {
        let summary = self
            .summary
            .as_mut()
            .ok_or_else(|| invalid_state("the waveform is already finished"))?;
        if !planar.len().is_multiple_of(self.channels) {
            return Err(invalid_input(
                "a planar block holds the same number of samples per channel",
            ));
        }
        let frames = planar.len() / self.channels;
        let planes = planar.chunks(frames.max(1)).collect::<Vec<_>>();
        if frames == 0 {
            return Ok(());
        }
        summary.push(&planes).map_err(invalid_input)
    }

    /// Closes the partial buckets and returns the summary.
    ///
    /// # Errors
    /// Rejects a builder already finished.
    pub fn finish(&mut self) -> Result<WaveformResult, JsValue> {
        self.summary
            .take()
            .map(|summary| WaveformResult {
                waveform: summary.finish(),
            })
            .ok_or_else(|| invalid_state("the waveform is already finished"))
    }
}

/// A finished waveform summary.
#[wasm_bindgen]
pub struct WaveformResult {
    waveform: Waveform,
}

#[wasm_bindgen]
impl WaveformResult {
    /// Samples summarized per channel.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn frames(&self) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let frames = self.waveform.frames as f64;
        frames
    }

    /// Number of zoom levels.
    #[wasm_bindgen(getter, js_name = levelCount)]
    #[must_use]
    pub fn level_count(&self) -> usize {
        self.waveform.levels.len()
    }

    /// Samples per bucket at `level`, or 0 past the last level.
    #[wasm_bindgen(js_name = bucketSamples)]
    #[must_use]
    pub fn bucket_samples(&self, level: usize) -> usize {
        self.waveform
            .levels
            .get(level)
            .map_or(0, |level| level.bucket_samples)
    }

    /// One channel's buckets at `level` as `[min, max, rms]` triples; empty
    /// past the last level or channel.
    #[must_use]
    pub fn buckets(&self, level: usize, channel: usize) -> Vec<f32> {
        self.waveform
            .levels
            .get(level)
            .and_then(|level| level.channels.get(channel))
            .map_or_else(Vec::new, |buckets| {
                buckets
                    .iter()
                    .flat_map(|bucket| [bucket.min, bucket.max, bucket.rms])
                    .collect()
            })
    }
}
