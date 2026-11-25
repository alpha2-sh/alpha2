//! `alpha2.beats`: beats, onsets and an onset envelope of 22050 Hz mono audio.
//!
//! Each 512-sample hop is one frame: a 1024-sample Hann window centred on the hop's first sample,
//! and the mean rise over the previous frame of every bin's `ln(1 + 1000 |X| / sum(w))`, rises
//! only (the onset flux). `end` divides the flux and the window RMS by their largest values, picks
//! onsets as local peaks of the flux, takes the beat period from the flux's autocorrelation and
//! places the beats by dynamic programming. A run reads one forward pass from the range start.
//!
//! Params: `min_bpm` (60), `max_bpm` (200), `sensitivity` (0.5). Another key, a value that is not
//! a number in the range the descriptor declares, or `min_bpm` not below `max_bpm` fails `begin`.

mod analysis;

use std::f64::consts::TAU;
use std::sync::Arc;

use alpha2_plugin_sdk::{
    Audio, Context, Direction, Emitter, Error, Extractor, Host, MAX_DATA_INTERVALS,
    MAX_DATA_VALUES, NumberParam, TICKS_PER_SECOND,
};
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

const RATE: u32 = 22_050;
const HOP_SAMPLES: u32 = 512;
const HOP: usize = HOP_SAMPLES as usize;
const WINDOW: usize = 1_024;
const BINS: usize = WINDOW / 2 + 1;
const FRAMES_PER_SECOND: f64 = RATE as f64 / HOP_SAMPLES as f64;
const MAGNITUDE_GAIN_BEFORE_LOG: f64 = 1_000.0;
const SILENT_FLUX: f32 = 0.01;
const SILENT_RMS: f32 = 1e-4;
const ONSET_RISE_AT_SENSITIVITY_0: f32 = 0.2;
const ONSET_RISE_AT_SENSITIVITY_1: f32 = 0.02;
const ENVELOPE_CHANNELS: usize = 2;

const PARAMS: [NumberParam; 3] = [
    NumberParam {
        key: "min_bpm",
        default: 60.0,
        min: 30.0,
        max: 300.0,
    },
    NumberParam {
        key: "max_bpm",
        default: 200.0,
        min: 30.0,
        max: 300.0,
    },
    NumberParam {
        key: "sensitivity",
        default: 0.5,
        min: 0.0,
        max: 1.0,
    },
];

fn params(context: &Context) -> Result<[f64; 3], Error> {
    let values = context.number_params("alpha2.beats", &PARAMS)?;
    if values[0] >= values[1] {
        return Err(Error(format!(
            "params.min_bpm ({}) must be below params.max_bpm ({})",
            values[0], values[1]
        )));
    }
    Ok(values)
}

fn ticks_at_or_before(sample: i64) -> i64 {
    (sample * TICKS_PER_SECOND).div_euclid(i64::from(RATE))
}

fn lag_frames(bpm: f64) -> f64 {
    60.0 * FRAMES_PER_SECOND / bpm
}

struct Spectrum {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    buffer: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    levels: Vec<f32>,
    previous: Option<Vec<f32>>,
}

impl Spectrum {
    fn new() -> Self {
        let fft = FftPlanner::new().plan_fft_forward(WINDOW);
        let size = f64::from(u32::try_from(WINDOW).unwrap_or(u32::MAX));
        #[expect(
            clippy::cast_possible_truncation,
            reason = "window weights are between 0 and 4"
        )]
        let window = (0..u32::try_from(WINDOW).unwrap_or(0))
            .map(|at| {
                let hann = 0.5 - 0.5 * (TAU * f64::from(at) / size).cos();
                (hann * MAGNITUDE_GAIN_BEFORE_LOG / (size / 2.0)) as f32
            })
            .collect();
        let scratch = vec![Complex::default(); fft.get_inplace_scratch_len()];
        Self {
            fft,
            window,
            buffer: vec![Complex::default(); WINDOW],
            scratch,
            levels: vec![0.0; BINS],
            previous: None,
        }
    }

    fn onset_flux(&mut self, samples: &[f32]) -> f32 {
        for ((slot, &sample), &weight) in self.buffer.iter_mut().zip(samples).zip(&self.window) {
            *slot = Complex::new(sample * weight, 0.0);
        }
        self.fft
            .process_with_scratch(&mut self.buffer, &mut self.scratch);
        for (level, bin) in self.levels.iter_mut().zip(&self.buffer) {
            *level = (bin.re * bin.re + bin.im * bin.im).sqrt().ln_1p();
        }
        let flux = self.previous.as_ref().map_or(0.0, |previous| {
            let rise = self
                .levels
                .iter()
                .zip(previous)
                .map(|(level, before)| f64::from((level - before).max(0.0)))
                .sum::<f64>();
            #[expect(clippy::cast_possible_truncation, reason = "a mean of f32 values")]
            let mean = (rise / f64::from(u32::try_from(BINS).unwrap_or(u32::MAX))) as f32;
            mean
        });
        match &mut self.previous {
            Some(previous) => previous.copy_from_slice(&self.levels),
            None => self.previous = Some(self.levels.clone()),
        }
        flux
    }
}

/// The extractor.
pub struct Beats {
    spectrum: Spectrum,
    window_samples: Vec<f32>,
    flux: Vec<f32>,
    rms: Vec<f32>,
    first_sample: Option<i64>,
    next_sample: i64,
    lag_bounds: (f64, f64),
    onset_rise: f32,
}

impl Beats {
    fn frame(&mut self) {
        let samples = &self.window_samples[..WINDOW];
        self.flux.push(self.spectrum.onset_flux(samples));
        let energy = samples
            .iter()
            .map(|&sample| f64::from(sample) * f64::from(sample))
            .sum::<f64>();
        #[expect(clippy::cast_possible_truncation, reason = "an RMS of f32 samples")]
        let rms = (energy / f64::from(u32::try_from(WINDOW).unwrap_or(u32::MAX))).sqrt() as f32;
        self.rms.push(rms);
        self.window_samples.drain(..HOP);
    }
}

fn largest(values: &[f32]) -> f32 {
    values.iter().copied().fold(0.0, f32::max)
}

fn strongest_in_frame_order(mut picked: Vec<(usize, f32)>, limit: usize) -> Vec<(usize, f32)> {
    if picked.len() > limit {
        picked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        picked.truncate(limit);
        picked.sort_by_key(|&(frame, _)| frame);
    }
    picked
}

impl Extractor for Beats {
    const DESCRIPTOR: &'static str = include_str!("../descriptor.json");

    fn begin(context: &Context, _host: &Host) -> Result<Self, Error> {
        let [min_bpm, max_bpm, sensitivity] = params(context)?;
        let one_forward_pass = matches!(
            context.passes.as_slice(),
            [pass] if pass.direction == Direction::Forward && pass.start == context.range.start
        );
        if !one_forward_pass {
            return Err("alpha2.beats reads one forward pass from the range start".into());
        }
        #[expect(clippy::cast_possible_truncation, reason = "sensitivity is 0 to 1")]
        let sensitivity = sensitivity as f32;
        Ok(Self {
            spectrum: Spectrum::new(),
            window_samples: vec![0.0; WINDOW / 2],
            flux: Vec::new(),
            rms: Vec::new(),
            first_sample: None,
            next_sample: 0,
            lag_bounds: (lag_frames(max_bpm), lag_frames(min_bpm)),
            onset_rise: ONSET_RISE_AT_SENSITIVITY_0
                + (ONSET_RISE_AT_SENSITIVITY_1 - ONSET_RISE_AT_SENSITIVITY_0) * sensitivity,
        })
    }

    fn audio(&mut self, block: &Audio<'_>, _out: &mut Emitter) -> Result<(), Error> {
        if block.rate != RATE || block.channels != 1 {
            return Err(Error(format!(
                "alpha2.beats reads mono audio at {RATE} Hz, not {} channels at {} Hz",
                block.channels, block.rate
            )));
        }
        let frames = usize::try_from(block.frames)
            .unwrap_or(0)
            .min(block.samples.len());
        match self.first_sample {
            None => {
                self.first_sample = Some(block.start_sample);
                self.next_sample = block.start_sample;
            }
            Some(_) if block.start_sample != self.next_sample => {
                return Err(Error(format!(
                    "audio block at sample {} after the one ending at {}",
                    block.start_sample, self.next_sample
                )));
            }
            Some(_) => {}
        }
        self.next_sample += i64::try_from(frames).unwrap_or(0);
        self.window_samples
            .extend_from_slice(&block.samples[..frames]);
        while self.window_samples.len() >= WINDOW {
            self.frame();
        }
        Ok(())
    }

    fn end(&mut self, out: &mut Emitter) -> Result<(), Error> {
        let Some(first) = self.first_sample else {
            return Ok(());
        };
        let delivered = usize::try_from(self.next_sample - first).unwrap_or(0);
        while self.flux.len() < delivered.div_ceil(HOP) {
            self.window_samples
                .resize(WINDOW.max(self.window_samples.len()), 0.0);
            self.frame();
        }
        let time = |frame: usize| {
            ticks_at_or_before(first + i64::try_from(frame).unwrap_or(0) * i64::from(HOP_SAMPLES))
        };

        let peak_flux = largest(&self.flux);
        let onset = {
            let scale = peak_flux.max(SILENT_FLUX);
            self.flux
                .iter()
                .map(|value| value / scale)
                .collect::<Vec<_>>()
        };
        let rms = {
            let scale = largest(&self.rms).max(SILENT_RMS);
            self.rms
                .iter()
                .map(|value| value / scale)
                .collect::<Vec<_>>()
        };
        let group = (ENVELOPE_CHANNELS * onset.len())
            .div_ceil(MAX_DATA_VALUES)
            .max(1);
        for (index, (onsets, levels)) in onset.chunks(group).zip(rms.chunks(group)).enumerate() {
            out.sample(
                "envelope",
                time(index * group),
                &[Some(largest(onsets)), Some(largest(levels))],
            );
        }
        if peak_flux < SILENT_FLUX {
            return Ok(());
        }

        let onsets = analysis::onsets(&onset, self.onset_rise)
            .into_iter()
            .map(|frame| (frame, onset[frame]))
            .collect();
        for (frame, score) in strongest_in_frame_order(onsets, MAX_DATA_INTERVALS) {
            out.interval("onsets", time(frame), time(frame), None, Some(score));
        }

        let Some(period) = analysis::period(&self.flux, self.lag_bounds.0, self.lag_bounds.1)
        else {
            return Ok(());
        };
        let beats = analysis::beats(&self.flux, period)
            .into_iter()
            .map(|frame| {
                let around = &onset[frame.saturating_sub(1)..(frame + 2).min(onset.len())];
                (frame, largest(around))
            })
            .collect();
        for (frame, score) in strongest_in_frame_order(beats, MAX_DATA_INTERVALS) {
            out.interval("beats", time(frame), time(frame), None, Some(score));
        }
        Ok(())
    }
}

alpha2_plugin_sdk::export_extractor!(Beats);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_declares_the_defaults_and_bounds_the_plugin_uses() {
        let descriptor: serde_json::Value =
            serde_json::from_str(<Beats as Extractor>::DESCRIPTOR).expect("descriptor");
        for param in PARAMS {
            let schema = &descriptor["params_schema"]["properties"][param.key];
            assert_eq!(schema["default"].as_f64(), Some(param.default), "{param:?}");
            assert_eq!(schema["minimum"].as_f64(), Some(param.min), "{param:?}");
            assert_eq!(schema["maximum"].as_f64(), Some(param.max), "{param:?}");
        }
        let input = &descriptor["input"];
        assert_eq!(input["sample_rate"].as_u64(), Some(u64::from(RATE)));
        assert_eq!(input["block_frames"].as_u64(), Some(u64::from(HOP_SAMPLES)));
    }
}
