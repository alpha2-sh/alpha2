//! Standalone benchmark for DSP cost after a signal decays to silence.
//!
//! A recursive processor whose state decays into the subnormal range runs an
//! order of magnitude slower on x86 until the state reaches zero, which a
//! burst followed by silence reproduces. For each processor this prints the
//! time per 128-sample block of silence for a fresh processor (zero state)
//! and for one ten seconds after a burst, their ratio, and whether the
//! processor's state or output was left holding subnormal values, and the
//! same two times for a stereo loudness meter. It then prints the time per
//! block of a compressor reducing gain on every sample of a loud sine, the
//! path silence never reaches. Times are recorded, never gated.

use std::{hint::black_box, time::Instant};

use alpha2_audio::{
    Biquad, BiquadKind, Compressor, Ducker, LoudnessMeter, PeakLimiter, ProcessCtx, Processor,
    SchroederReverb, TransportState,
};
use alpha2_time::MediaTime;

const RATE: u32 = 48_000;
const BLOCK: usize = 128;
const MEASURED_BLOCKS: usize = 2_000;

/// Builds a fresh processor for one measurement.
type Make = Box<dyn Fn() -> Box<dyn Processor>>;

fn main() {
    let cases: Vec<(&str, Make, StateFloats)> = vec![
        (
            "limiter",
            Box::new(|| Box::new(PeakLimiter::new(0.5, 50.0))),
            StateFloats::All,
        ),
        (
            "compressor",
            Box::new(|| Box::new(Compressor::new(-24.0, 4.0, 5.0, 80.0, 0.0))),
            StateFloats::First,
        ),
        (
            "reverb",
            Box::new(|| Box::new(SchroederReverb::new(0.8, 0.2, 0.5))),
            StateFloats::DelayLines,
        ),
        (
            "biquad",
            Box::new(|| Box::new(Biquad::new(BiquadKind::LowPass, 1_000.0, 0.7, 0.0))),
            StateFloats::AllF64,
        ),
        (
            "ducker",
            Box::new(|| Box::new(Ducker::new(0.1, -12.0, 5.0, 200.0))),
            StateFloats::All,
        ),
    ];
    for (name, make, state_floats) in cases {
        let fresh = measure(make(), Signal::Silence).0;
        let (silent, processor, output) = measure(make(), Signal::BurstThenSilence);
        let subnormal_state = match state_floats {
            StateFloats::All => processor
                .save_state()
                .as_bytes()
                .as_chunks::<4>()
                .0
                .iter()
                .any(|bytes| is_subnormal(bytes)),
            StateFloats::AllF64 => processor
                .save_state()
                .as_bytes()
                .as_chunks::<8>()
                .0
                .iter()
                .any(|bytes| f64::from_le_bytes(*bytes).is_subnormal()),
            StateFloats::First => processor
                .save_state()
                .as_bytes()
                .get(..4)
                .is_some_and(is_subnormal),
            StateFloats::DelayLines => reverb_state_floats(processor.save_state().as_bytes())
                .into_iter()
                .any(|bytes| is_subnormal(bytes)),
        };
        let subnormal_output = output.iter().any(|sample| sample.is_subnormal());
        println!(
            "dsp processor={name} fresh_silent_ns_per_block={fresh:.0} \
             decayed_silent_ns_per_block={silent:.0} ratio={:.2} \
             subnormal_state={subnormal_state} subnormal_output={subnormal_output}",
            silent / fresh,
        );
    }
    let fresh = measure_meter(Signal::Silence);
    let silent = measure_meter(Signal::BurstThenSilence);
    println!(
        "dsp meter=loudness fresh_silent_ns_per_block={fresh:.0} \
         decayed_silent_ns_per_block={silent:.0} ratio={:.2}",
        silent / fresh,
    );
    let compressing = measure(
        Box::new(Compressor::new(-24.0, 4.0, 5.0, 80.0, 0.0)),
        Signal::Loud,
    )
    .0;
    println!("dsp processor=compressor loud_ns_per_block={compressing:.0}");
}

/// Which bytes of a processor's saved state are `f32` values to inspect.
#[derive(Copy, Clone)]
enum StateFloats {
    /// Every 4-byte word.
    All,
    /// Every 8-byte word, as `f64`: the filter's state.
    AllF64,
    /// Only the first word: the compressor's envelope, ahead of its cursor, latency and delay.
    First,
    /// The reverb's delay lines: per line a `u64` cursor, then the `f32`
    /// damping filter and samples.
    DelayLines,
}

/// The `f32` words of a reverb state saved at [`RATE`]: every delay line's
/// filter and samples, skipping its cursor.
fn reverb_state_floats(state: &[u8]) -> Vec<&[u8; 4]> {
    // The line lengths `SchroederReverb::prepare` scales from 44.1 kHz.
    #[allow(clippy::cast_precision_loss)]
    let scale = RATE as f32 / 44_100.0;
    let mut words = Vec::new();
    let mut offset = 0;
    for length in [1116_u16, 1188, 1277, 1356, 556, 441] {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let samples = (f32::from(length) * scale).round() as usize;
        offset += 8;
        let end = offset + 4 * (samples + 1);
        words.extend(state[offset..end].as_chunks::<4>().0);
        offset = end;
    }
    assert_eq!(offset, state.len(), "the reverb state layout changed");
    words
}

/// Nanoseconds per block a stereo [`LoudnessMeter`] takes over the measured
/// stretch of `signal`.
fn measure_meter(signal: Signal) -> f64 {
    let mut meter = LoudnessMeter::new(RATE, 2).expect("meter");
    let blocks_per_second = RATE as usize / BLOCK;
    let (lead, burst) = match signal {
        Signal::Silence | Signal::Loud => (blocks_per_second, 0),
        Signal::BurstThenSilence => (blocks_per_second * 21 / 2, blocks_per_second / 2),
    };
    let mut input = vec![0.0_f32; BLOCK];
    let mut elapsed = 0.0;
    for index in 0..lead + MEASURED_BLOCKS {
        for (offset, sample) in input.iter_mut().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let phase = (index * BLOCK + offset) as f32 * 1_000.0 / RATE as f32;
            *sample = if index < burst {
                0.5 * libm::sinf(core::f32::consts::TAU * phase)
            } else {
                0.0
            };
        }
        let started = Instant::now();
        meter.push(&[&input, &input]).expect("block");
        if index >= lead {
            elapsed += started.elapsed().as_secs_f64();
        }
    }
    black_box(meter.report());
    #[allow(clippy::cast_precision_loss)]
    let per_block = elapsed / MEASURED_BLOCKS as f64 * 1e9;
    per_block
}

#[derive(Copy, Clone)]
enum Signal {
    Silence,
    BurstThenSilence,
    /// A 1 kHz sine at -6 dBFS throughout.
    Loud,
}

fn is_subnormal(bytes: &[u8]) -> bool {
    <[u8; 4]>::try_from(bytes).is_ok_and(|bytes| f32::from_le_bytes(bytes).is_subnormal())
}

/// Nanoseconds per block over the measured stretch, the processor, and the
/// last output block.
fn measure(
    mut processor: Box<dyn Processor>,
    signal: Signal,
) -> (f64, Box<dyn Processor>, Vec<f32>) {
    processor.prepare(RATE, BLOCK);
    let mut seed = 0x2545_f491_u32;
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        #[allow(clippy::cast_precision_loss)]
        let unit = seed as f32 / u32::MAX as f32;
        unit - 0.5
    };
    let mut input = [vec![0.0_f32; BLOCK], vec![0.0_f32; BLOCK]];
    let mut output = [vec![0.0_f32; BLOCK], vec![0.0_f32; BLOCK]];
    let blocks_per_second = RATE as usize / BLOCK;
    // Half a second of burst, ten of decay, then the measured stretch.
    let (lead, burst) = match signal {
        Signal::Silence | Signal::Loud => (blocks_per_second, 0),
        Signal::BurstThenSilence => (blocks_per_second * 21 / 2, blocks_per_second / 2),
    };
    let mut position = 0_i64;
    let mut elapsed = 0.0;
    for index in 0..lead + MEASURED_BLOCKS {
        for plane in &mut input {
            for (offset, sample) in plane.iter_mut().enumerate() {
                *sample = match signal {
                    Signal::Loud => {
                        #[allow(clippy::cast_precision_loss)]
                        let phase = (index * BLOCK + offset) as f32 * 1_000.0 / RATE as f32;
                        0.5 * libm::sinf(core::f32::consts::TAU * phase)
                    }
                    _ if index < burst => noise(),
                    _ => 0.0,
                };
            }
        }
        let ctx = ProcessCtx {
            position: MediaTime::ZERO,
            sample_position: position,
            sample_rate: RATE,
            transport: TransportState::Playing,
        };
        let [left_in, right_in] = &input;
        let [left_out, right_out] = &mut output;
        let started = Instant::now();
        processor.process(
            &ctx,
            &[left_in.as_slice(), right_in.as_slice()],
            &mut [left_out.as_mut_slice(), right_out.as_mut_slice()],
        );
        if index >= lead {
            elapsed += started.elapsed().as_secs_f64();
        }
        black_box(&output);
        position += i64::try_from(BLOCK).expect("small");
    }
    #[allow(clippy::cast_precision_loss)]
    let per_block = elapsed / MEASURED_BLOCKS as f64 * 1e9;
    let [left, _] = output;
    (per_block, processor, left)
}
