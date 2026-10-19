//! Loudness measurement (`alpha2-audio`, ITU-R BS.1770-4 / EBU R128) for the
//! web: a standalone meter, the report's JSON, and normalization planning.

use alpha2_audio::{LoudnessMeter, LoudnessNormalization, LoudnessReport};
use wasm_bindgen::prelude::*;

use crate::web::invalid_input;

/// A report as the JSON `alpha2/loudness` reads.
pub(crate) fn report_json(report: &LoudnessReport) -> String {
    report_value(report).to_string()
}

fn report_value(report: &LoudnessReport) -> serde_json::Value {
    serde_json::json!({
        "integratedLufs": report.integrated_lufs,
        "loudnessRangeLu": report.loudness_range_lu,
        "truePeakDbtp": report.true_peak_dbtp,
        "samplePeakDbfs": report.sample_peak_dbfs,
        "momentaryMaxLufs": report.momentary_max_lufs,
        "shortTermMaxLufs": report.short_term_max_lufs,
        "frames": report.frames,
    })
}

/// The engine's report of a mix measured before its master limiter: a
/// report, and `limiterThreshold`, the limiter's linear threshold (`null` for
/// a mix without one), which a plan keeps the normalized mix under.
pub(crate) fn mix_report_json(report: &LoudnessReport, limiter_threshold: Option<f64>) -> String {
    let mut value = report_value(report);
    value["limiterThreshold"] = serde_json::json!(limiter_threshold);
    value.to_string()
}

/// A report and the limiter threshold it states, if any.
fn report_from_json(json: &str) -> Result<(LoudnessReport, Option<f64>), JsValue> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(invalid_input)?;
    // Anything else would read as a report with nothing measured, which plans as silence: the
    // engine's `loudnessReport()` string itself, or its `null` while metering is off.
    let Some(report) = value.as_object() else {
        return Err(invalid_input(
            "a loudness report is a JSON object; parse loudnessReport() before planning, and \
             measure with metering on",
        ));
    };
    let Some(frames) = report.get("frames").and_then(serde_json::Value::as_u64) else {
        return Err(invalid_input(
            "a loudness report states the frames it measured",
        ));
    };
    // Absent from a standalone measurement, `null` for an unlimited mix.
    let limiter_threshold = match report.get("limiterThreshold") {
        None | Some(serde_json::Value::Null) => None,
        Some(threshold) => match threshold.as_f64() {
            Some(threshold) if threshold.is_finite() && threshold > 0.0 => Some(threshold),
            _ => {
                return Err(invalid_input(
                    "a loudness report's limiterThreshold is a positive number or null",
                ));
            }
        },
    };
    let number = |key: &str| report.get(key).and_then(serde_json::Value::as_f64);
    Ok((
        LoudnessReport {
            integrated_lufs: number("integratedLufs"),
            loudness_range_lu: number("loudnessRangeLu"),
            true_peak_dbtp: number("truePeakDbtp"),
            sample_peak_dbfs: number("samplePeakDbfs"),
            momentary_max_lufs: number("momentaryMaxLufs"),
            short_term_max_lufs: number("shortTermMaxLufs"),
            frames,
        },
        limiter_threshold,
    ))
}

/// The gain that brings a measured mix (a `loudnessReport` JSON) to
/// `target_lufs` without its true peak passing `ceiling_dbtp`, or its sample
/// peak reaching the master limiter the report states (`limiterThreshold`),
/// as JSON `{ gainDb, integratedLufs, truePeakDbtp, peakLimited,
/// limiterLimited }`, or `null` for a mix silent below the absolute gate.
///
/// # Errors
/// Rejects a report that is not a JSON object with its `frames`, or whose
/// `limiterThreshold` is neither a positive number nor `null`.
#[wasm_bindgen(js_name = planLoudnessNormalization)]
pub fn plan_loudness_normalization(
    report_json: &str,
    target_lufs: f64,
    ceiling_dbtp: f64,
) -> Result<String, JsValue> {
    let (report, limiter_threshold) = report_from_json(report_json)?;
    Ok(
        LoudnessNormalization::plan(&report, target_lufs, ceiling_dbtp, limiter_threshold)
            .map_or_else(
                || "null".to_owned(),
                |plan| {
                    serde_json::json!({
                        "gainDb": plan.gain_db,
                        "integratedLufs": plan.integrated_lufs,
                        "truePeakDbtp": plan.true_peak_dbtp,
                        "peakLimited": plan.peak_limited,
                        "limiterLimited": plan.limiter_limited,
                    })
                    .to_string()
                },
            ),
    )
}

/// Measures planar audio streamed in blocks.
#[wasm_bindgen(js_name = LoudnessMeter)]
pub struct WasmLoudnessMeter {
    meter: LoudnessMeter,
    channels: usize,
}

#[wasm_bindgen(js_class = LoudnessMeter)]
impl WasmLoudnessMeter {
    /// A meter for `channels` channels at `sample_rate`.
    ///
    /// # Errors
    /// Rejects zero channels or a rate outside 8 to 96 kHz.
    #[wasm_bindgen(constructor)]
    pub fn new(sample_rate: u32, channels: usize) -> Result<Self, JsValue> {
        let meter = LoudnessMeter::new(sample_rate, channels).map_err(invalid_input)?;
        Ok(Self { meter, channels })
    }

    /// Adds one block: the channels' samples concatenated, channel after
    /// channel, all of one length. A NaN or infinite sample measures as
    /// silence.
    ///
    /// # Errors
    /// Rejects a block whose length is not a multiple of the channel count.
    pub fn push(&mut self, planar: &[f32]) -> Result<(), JsValue> {
        if !planar.len().is_multiple_of(self.channels) {
            return Err(invalid_input(
                "a planar block holds the same number of samples per channel",
            ));
        }
        let frames = planar.len() / self.channels;
        if frames == 0 {
            return Ok(());
        }
        let planes = planar.chunks(frames).collect::<Vec<_>>();
        self.meter.push(&planes).map_err(invalid_input)
    }

    /// Everything measured so far, as JSON.
    #[must_use]
    pub fn report(&self) -> String {
        report_json(&self.meter.report())
    }

    /// Momentary (400 ms) loudness in LUFS, or `undefined` before 400 ms.
    #[wasm_bindgen(getter, js_name = momentaryLufs)]
    #[must_use]
    pub fn momentary_lufs(&self) -> Option<f64> {
        self.meter.momentary_lufs()
    }

    /// Short-term (3 s) loudness in LUFS, or `undefined` before 3 s.
    #[wasm_bindgen(getter, js_name = shortTermLufs)]
    #[must_use]
    pub fn short_term_lufs(&self) -> Option<f64> {
        self.meter.short_term_lufs()
    }
}
