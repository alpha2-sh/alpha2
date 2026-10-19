//! The engine's sample-rate converter (`alpha2_audio::Resampler`) for a
//! browser decoder: the same filter, and the same samples, a native host
//! converts with.

use alpha2_audio::Resampler;
use wasm_bindgen::prelude::*;

use crate::web::invalid_input;

/// A sample index from JavaScript: a whole number a double holds exactly.
fn index(value: f64, name: &str) -> Result<i64, JsValue> {
    const LIMIT: f64 = 9_007_199_254_740_991.0;
    if value.is_finite() && value.fract() == 0.0 && value.abs() <= LIMIT {
        #[allow(clippy::cast_possible_truncation)]
        return Ok(value as i64);
    }
    Err(invalid_input(format!(
        "{name} must be a whole sample index, not {value}"
    )))
}

#[allow(clippy::cast_precision_loss)]
const fn number(value: i64) -> f64 {
    value as f64
}

/// Converts one source's audio to the project rate. Positions are sample
/// indexes: of the source at its own rate, of the output at the project's.
#[wasm_bindgen(js_name = AudioResampler)]
pub struct WasmAudioResampler {
    resampler: Resampler,
}

#[wasm_bindgen(js_class = AudioResampler)]
impl WasmAudioResampler {
    /// A converter from `source_rate` to `target_rate`.
    ///
    /// # Errors
    /// Rejects a rate of zero or above 768 kHz, and a source rate more than
    /// 8192 times the target rate, whose filter would not fit the table.
    #[wasm_bindgen(constructor)]
    pub fn new(source_rate: u32, target_rate: u32) -> Result<Self, JsValue> {
        Resampler::new(source_rate, target_rate)
            .map(|resampler| Self { resampler })
            .map_err(invalid_input)
    }

    /// The first source sample output `target_start` weighs.
    ///
    /// # Errors
    /// Rejects a position that is not a whole sample index.
    #[wasm_bindgen(js_name = sourceStart)]
    pub fn source_start(&self, target_start: f64) -> Result<f64, JsValue> {
        let target = index(target_start, "targetStart")?;
        Ok(number(self.resampler.source_span(target..target + 1).start))
    }

    /// The source sample after the last one output `target_end - 1` weighs.
    ///
    /// # Errors
    /// Rejects a position that is not a whole sample index.
    #[wasm_bindgen(js_name = sourceEnd)]
    pub fn source_end(&self, target_end: f64) -> Result<f64, JsValue> {
        let target = index(target_end, "targetEnd")?;
        Ok(number(self.resampler.source_span(target - 1..target).end))
    }

    /// The first output sample at or after source sample `source`.
    ///
    /// # Errors
    /// Rejects a position that is not a whole sample index.
    #[wasm_bindgen(js_name = targetAtOrAfter)]
    pub fn target_at_or_after(&self, source: f64) -> Result<f64, JsValue> {
        Ok(number(
            self.resampler.target_at_or_after(index(source, "source")?),
        ))
    }

    /// `frames` outputs from `target_start`, from `input`, whose first sample
    /// is source sample `input_start`. The source is silent outside `input`.
    ///
    /// # Errors
    /// Rejects a position that is not a whole sample index.
    pub fn resample(
        &self,
        input: &[f32],
        input_start: f64,
        target_start: f64,
        frames: u32,
    ) -> Result<Vec<f32>, JsValue> {
        let mut output = vec![0.0; frames as usize];
        self.resampler.resample(
            input,
            index(input_start, "inputStart")?,
            index(target_start, "targetStart")?,
            &mut output,
        );
        Ok(output)
    }
}
