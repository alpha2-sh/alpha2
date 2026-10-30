#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use alpha2_time::{MediaTime, TICKS_PER_SECOND};

use crate::*;

fn context(sample_position: i64) -> ProcessCtx {
    ProcessCtx {
        position: MediaTime(sample_position * TICKS_PER_SECOND / 48_000),
        sample_position,
        sample_rate: 48_000,
        transport: TransportState::Playing,
    }
}

#[test]
fn gain_processes_planar_channels() {
    let input = [1.0, -0.5, 0.25];
    let mut output = [0.0; 3];
    Gain::new(0.5).process(&context(0), &[&input], &mut [&mut output]);
    assert_samples_close(&output, &[0.5, -0.25, 0.125]);
}

#[test]
fn equal_power_crossfade_has_unity_endpoints_and_no_midpoint_dip() {
    let from = [1.0];
    let to = [0.0];
    let mut output = [0.0];
    EqualPowerCrossfade::new(0.0).process(&context(0), &[&from, &to], &mut [&mut output]);
    assert_samples_close(&output, &[1.0]);
    EqualPowerCrossfade::new(0.5).process(&context(0), &[&from, &to], &mut [&mut output]);
    assert!((output[0] - core::f32::consts::FRAC_1_SQRT_2).abs() < 1.0e-6);
    EqualPowerCrossfade::new(1.0).process(&context(0), &[&from, &to], &mut [&mut output]);
    assert_eq!(output[0].to_bits(), to[0].to_bits());
}

#[test]
fn fade_is_independent_of_host_block_boundaries() {
    let input = vec![1.0; 512];
    let mut whole = vec![0.0; 512];
    let mut fade = LinearFade::new(
        MediaTime::ZERO,
        MediaTime(TICKS_PER_SECOND),
        FadeDirection::In,
    )
    .expect("non-empty fade");
    fade.process(&context(0), &[&input], &mut [&mut whole]);

    let mut chunked = vec![0.0; 512];
    let block_sizes = [31_usize, 128, 17, 255, 81];
    let mut offset = 0;
    for size in block_sizes {
        let end = (offset + size).min(input.len());
        let sample_position = i64::try_from(offset).expect("small index");
        fade.process(
            &context(sample_position),
            &[&input[offset..end]],
            &mut [&mut chunked[offset..end]],
        );
        offset = end;
    }
    assert_eq!(offset, input.len());
    assert_eq!(whole, chunked);
}

#[test]
fn limiter_state_survives_offline_chunk_boundaries() {
    let loud = [2.0, 0.5];
    let mut first = [0.0; 2];
    let mut limiter = PeakLimiter::new(1.0, 50.0);
    limiter.prepare(48_000, 128);
    limiter.process(&context(0), &[&loud], &mut [&mut first]);
    assert!(first[0] <= 1.0);

    let state = limiter.save_state();
    let mut restored = PeakLimiter::new(1.0, 50.0);
    restored.prepare(48_000, 128);
    restored.load_state(&state).expect("same limiter");
    assert_eq!(restored.save_state(), state);
}

#[test]
fn a_released_limiter_envelope_reaches_zero_instead_of_a_subnormal() {
    let mut limiter = PeakLimiter::new(0.5, 50.0);
    limiter.prepare(48_000, 128);
    let burst = [0.9_f32; 128];
    let silence = [0.0_f32; 128];
    let mut output = [0.0_f32; 128];
    limiter.process(&context(0), &[&burst], &mut [&mut output]);
    // Ten seconds of silence: 3750 blocks of 128 at 48 kHz.
    for block in 1..=3_750 {
        limiter.process(&context(block * 128), &[&silence], &mut [&mut output]);
    }
    let envelope = f32::from_le_bytes(limiter.save_state().as_bytes().try_into().expect("one f32"));
    assert_eq!(envelope.to_bits(), 0, "envelope {envelope:e}");
}

#[test]
fn limiter_matches_audio_reference() {
    let left = [2.0; 128];
    let right = [0.5; 128];
    let mut output_left = [0.0; 128];
    let mut output_right = [0.0; 128];
    let mut limiter = PeakLimiter::new(1.0, 50.0);
    limiter.prepare(48_000, 128);
    limiter.process(
        &context(0),
        &[&left, &right],
        &mut [&mut output_left, &mut output_right],
    );
    let reference: serde_json::Value =
        serde_json::from_str(include_str!("../../../tests/cases/audio_reference.json"))
            .expect("audio reference");
    let expected: Vec<f32> =
        serde_json::from_value(reference["limited_stereo_first_frame"].clone())
            .expect("stereo reference");
    assert_samples_close(&[output_left[0], output_right[0]], &expected);
}

#[test]
fn dsp_processors_are_block_size_invariant_below_minus_90_dbfs() {
    let input: Vec<f32> = (0..4_096)
        .map(|index| libm::sinf((index as f32) * 0.013) * 0.7)
        .collect();
    assert_processor_invariant(
        || Biquad::new(BiquadKind::Peaking, 1_200.0, 0.8, 4.0),
        &input,
    );
    assert_processor_invariant(|| Compressor::new(-12.0, 4.0, 5.0, 80.0, 2.0), &input);
    assert_processor_invariant(|| SchroederReverb::new(0.7, 0.25, 0.4), &input);
}

#[test]
fn stateful_effects_continue_after_save_and_load() {
    let input: Vec<f32> = (0..2_048)
        .map(|index| libm::sinf((index as f32) * 0.021))
        .collect();
    assert_state_continuity(|| Compressor::new(-18.0, 6.0, 3.0, 120.0, 1.0), &input);
    assert_state_continuity(|| SchroederReverb::new(0.6, 0.4, 0.5), &input);
}

#[test]
fn graph_routes_clip_track_and_master_chains() {
    let mut graph = AudioGraph::new(ChannelLayout::Stereo, 2);
    graph
        .clip_chain_mut(0)
        .expect("bus")
        .push(Gain::new(0.5))
        .expect("one input");
    graph
        .track_bus_mut(1)
        .expect("bus")
        .push(Gain::new(0.25))
        .expect("one input");
    graph
        .master_mut()
        .push(PeakLimiter::new(1.0, 20.0))
        .expect("one input");
    graph.prepare(48_000, 32);
    let one = [1.0; 32];
    let mut left = [0.0; 32];
    let mut right = [0.0; 32];
    graph.process(
        &context(0),
        &[&one, &one, &one, &one],
        &mut [&mut left, &mut right],
    );
    assert!(
        left.iter().zip(right).all(|(left, right)| {
            (*left - 0.75).abs() < 1.0e-6 && (right - 0.75).abs() < 1.0e-6
        })
    );
}

#[test]
fn graph_zero_pads_short_bus_inputs_instead_of_reusing_scratch() {
    let mut graph = AudioGraph::new(ChannelLayout::Stereo, 2);
    graph.prepare(48_000, 64);
    let full = [1.0; 64];
    let short = [0.0; 32];
    let mut left = [0.0; 64];
    let mut right = [0.0; 64];
    graph.process(
        &context(0),
        &[&full, &full, &short, &short],
        &mut [&mut left, &mut right],
    );
    assert_samples_close(&left, &full);
    assert_samples_close(&right, &full);
}

#[test]
fn automation_handles_the_full_sample_index_range() {
    let curve = AutomationCurve::new(vec![
        AutomationPoint {
            sample: i64::MIN,
            value: 0.0,
        },
        AutomationPoint {
            sample: i64::MAX,
            value: 1.0,
        },
    ])
    .expect("unique finite points");
    let midpoint = curve.value_at(0, -1.0);
    assert!(midpoint.is_finite());
    assert!((midpoint - 0.5).abs() < 1.0e-6);
}

#[test]
fn non_finite_samples_do_not_poison_audio_detectors() {
    let input = [f32::INFINITY, 0.5, 0.5];

    let mut limited = [0.0; 3];
    let mut limiter_processor = PeakLimiter::new(1.0, 50.0);
    limiter_processor.prepare(48_000, 3);
    limiter_processor.process(&context(0), &[&input], &mut [&mut limited]);
    assert!(limited.iter().all(|sample| sample.is_finite()));
    assert!((limited[1] - 0.5).abs() < f32::EPSILON);

    let mut compressed = [0.0; 3];
    let mut compressor = Compressor::new(-12.0, 4.0, 5.0, 80.0, 0.0);
    compressor.prepare(48_000, 3);
    compressor.process(&context(0), &[&input], &mut [&mut compressed]);
    assert!(compressed.iter().all(|sample| sample.is_finite()));
    assert!(compressed[1] > 0.0);

    let program = [1.0; 3];
    let sidechain = [f32::NAN, 0.0, 0.0];
    let mut ducked = [0.0; 3];
    let mut ducker_processor = Ducker::new(0.1, 12.0, 1.0, 100.0);
    ducker_processor.prepare(48_000, 3);
    ducker_processor.process(&context(0), &[&program, &sidechain], &mut [&mut ducked]);
    assert!(ducked.iter().all(|sample| sample.is_finite()));
    assert_samples_close(&ducked, &program);
}

#[test]
fn graph_state_transfers_across_offline_chunk_boundaries() {
    fn graph() -> AudioGraph {
        let mut graph = AudioGraph::new(ChannelLayout::Mono, 1);
        graph
            .master_mut()
            .push(Compressor::new(-18.0, 4.0, 2.0, 90.0, 1.0))
            .expect("one input");
        graph.prepare(48_000, 256);
        graph
    }
    let input = [0.8; 256];
    let mut continuous = graph();
    let mut expected = [0.0; 256];
    continuous.process(&context(0), &[&input], &mut [&mut expected]);

    let mut first = graph();
    let mut actual = [0.0; 256];
    first.process(&context(0), &[&input[..128]], &mut [&mut actual[..128]]);
    let state = first.save_state();
    let mut restored = graph();
    restored.load_state(&state).expect("matching graph");
    restored.process(&context(128), &[&input[128..]], &mut [&mut actual[128..]]);
    assert_samples_close(&actual, &expected);
}

#[test]
fn automation_and_ducking_are_sample_continuous() {
    let curve = AutomationCurve::new(vec![
        AutomationPoint {
            sample: 0,
            value: 0.0,
        },
        AutomationPoint {
            sample: 128,
            value: 1.0,
        },
    ])
    .expect("curve");
    let input = [1.0; 256];
    let whole = render_blocks(AutomatedGain::new(curve.clone(), 0.0), &input, &[256]);
    let chunked = render_blocks(AutomatedGain::new(curve, 0.0), &input, &[17, 32, 51]);
    assert_samples_close(&whole, &chunked);

    let program = [1.0; 128];
    let sidechain = [1.0; 128];
    let mut output = [0.0; 128];
    let mut ducker = Ducker::new(0.05, 12.0, 1.0, 100.0);
    ducker.prepare(48_000, 128);
    ducker.process(&context(0), &[&program, &sidechain], &mut [&mut output]);
    assert!(output[127] < output[0]);
}

#[test]
fn prepared_graph_survives_long_irregular_realtime_sequence() {
    let mut graph = AudioGraph::new(ChannelLayout::Stereo, 1);
    graph
        .clip_chain_mut(0)
        .expect("clip")
        .push(Biquad::new(BiquadKind::HighPass, 80.0, 0.707, 0.0))
        .expect("one input");
    graph
        .track_bus_mut(0)
        .expect("track")
        .push(SchroederReverb::new(0.4, 0.3, 0.1))
        .expect("one input");
    graph
        .master_mut()
        .push(PeakLimiter::new(0.95, 40.0))
        .expect("one input");
    graph.prepare(48_000, 512);
    let input = [0.1; 512];
    let mut left = [0.0; 512];
    let mut right = [0.0; 512];
    let mut position = 0_i64;
    let mut random = 0x1234_5678_u32;
    for _ in 0..10_000 {
        random = random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let frames = usize::try_from(random % 481 + 32).expect("bounded");
        graph.process(
            &context(position),
            &[&input[..frames], &input[..frames]],
            &mut [&mut left[..frames], &mut right[..frames]],
        );
        position += i64::try_from(frames).expect("bounded");
    }
    assert!(left.iter().chain(&right).all(|sample| sample.is_finite()));
}

fn assert_processor_invariant<P>(factory: impl Fn() -> P, input: &[f32])
where
    P: Processor,
{
    let whole = render_blocks(factory(), input, &[input.len()]);
    let chunked = render_blocks(factory(), input, &[32, 128, 17, 512, 63]);
    let threshold = libm::powf(10.0, -90.0 / 20.0);
    assert!(
        whole
            .iter()
            .zip(chunked)
            .all(|(left, right)| (*left - right).abs() <= threshold),
        "processor output changed across block sizes"
    );
}

fn assert_state_continuity<P>(factory: impl Fn() -> P, input: &[f32])
where
    P: Processor,
{
    let mut continuous = factory();
    continuous.prepare(48_000, input.len());
    let mut expected = vec![0.0; input.len()];
    continuous.process(&context(0), &[input], &mut [&mut expected]);

    let split = input.len() / 2;
    let mut first = factory();
    first.prepare(48_000, input.len());
    let mut actual = vec![0.0; input.len()];
    first.process(&context(0), &[&input[..split]], &mut [&mut actual[..split]]);
    let state = first.save_state();
    let mut restored = factory();
    restored.prepare(48_000, input.len());
    restored.load_state(&state).expect("same processor");
    restored.process(
        &context(i64::try_from(split).expect("small")),
        &[&input[split..]],
        &mut [&mut actual[split..]],
    );
    assert_samples_close(&actual, &expected);
}

fn render_blocks<P: Processor>(mut processor: P, input: &[f32], sizes: &[usize]) -> Vec<f32> {
    processor.prepare(48_000, input.len());
    let mut output = vec![0.0; input.len()];
    let mut offset = 0;
    let mut block = 0;
    while offset < input.len() {
        let size = sizes[block % sizes.len()].min(input.len() - offset);
        processor.process(
            &context(i64::try_from(offset).expect("small")),
            &[&input[offset..offset + size]],
            &mut [&mut output[offset..offset + size]],
        );
        offset += size;
        block += 1;
    }
    output
}

fn assert_samples_close(actual: &[f32], expected: &[f32]) {
    assert_eq!(actual.len(), expected.len());
    assert!(
        actual
            .iter()
            .zip(expected)
            .all(|(actual, expected)| (*actual - *expected).abs() <= 1.0e-7)
    );
}

#[test]
fn recursive_processors_settle_from_their_reported_seek_history() {
    let mut processors: Vec<Box<dyn Processor>> = vec![
        Box::new(Biquad::new(BiquadKind::LowPass, 200.0, 8.0, 0.0)),
        Box::new(Biquad::new(BiquadKind::HighPass, 200.0, 0.05, 0.0)),
        Box::new(Biquad::new(BiquadKind::Peaking, 200.0, 8.0, 12.0)),
        Box::new(SchroederReverb::new(0.7, 0.3, 0.5)),
        Box::new(SchroederReverb::new(1.0, 0.95, 0.5)),
        Box::new(SchroederReverb::new(0.7, 1.0, 0.5)),
        Box::new(Ducker::new(0.1, 12.0, 50.0, 100.0)),
        Box::new(PeakLimiter::new(0.2, 100.0)),
    ];
    for (index, processor) in processors.iter_mut().enumerate() {
        processor.prepare(8_000, 128);
        let history = processor.history_samples() as usize;
        let target = history * 2 + 13;
        let signal: Vec<_> = (0..target + 128)
            .map(|sample| {
                let base = if (sample / 400) % 3 == 0 { 0.4 } else { 0.01 };
                base + (sample * 97 % 1_000) as f32 / 5_000.0
            })
            .collect();
        let mut left = [0.0; 128];
        let mut right = [0.0; 128];
        let mut process = |processor: &mut dyn Processor, first: usize, last: usize| {
            let mut position = first;
            while position < last {
                let end = (position + 128).min(last);
                let input = &signal[position..end];
                processor.process(
                    &ProcessCtx {
                        sample_rate: 8_000,
                        ..context(i64::try_from(position).expect("position"))
                    },
                    &[input, input, input, input],
                    &mut [&mut left[..input.len()], &mut right[..input.len()]],
                );
                position = end;
            }
            left
        };
        process(processor.as_mut(), 0, target);
        let expected = process(processor.as_mut(), target, target + 128);
        processor.reset();
        process(processor.as_mut(), target - history, target);
        let actual = process(processor.as_mut(), target, target + 128);
        let error = actual
            .iter()
            .zip(expected)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f32, f32::max);
        assert!(
            error < 1e-6,
            "processor {index}, history {history}: error {error}"
        );
    }
}

/// Outputs `range` of `source`, sampled at `from`, resampled to `to` in one call.
fn resampled(source: &[f32], from: u32, to: u32, range: std::ops::Range<i64>) -> Vec<f32> {
    let resampler = Resampler::new(from, to).expect("rates");
    let mut output = vec![0.0; usize::try_from(range.end - range.start).expect("range")];
    resampler.resample(source, 0, range.start, &mut output);
    output
}

#[test]
fn equal_rates_pass_the_samples_through() {
    let source: Vec<f32> = (0..100).map(|n| n as f32 / 100.0).collect();
    assert_eq!(resampled(&source, 48_000, 48_000, 0..100), source);
    assert_eq!(
        resampled(&source, 48_000, 48_000, 98..102),
        [0.98, 0.99, 0.0, 0.0]
    );
}

#[test]
fn a_constant_source_stays_constant_at_every_output() {
    // 44.1 kHz into 48 kHz leaves exact zeros at packet joins in the browser's per-packet
    // interpolation.
    for (from, to) in [
        (44_100, 48_000),
        (22_050, 48_000),
        (44_100, 96_000),
        (48_000, 44_100),
    ] {
        let source = vec![0.5_f32; usize::try_from(from).expect("rate")];
        let resampler = Resampler::new(from, to).expect("rates");
        let first = resampler.target_at_or_after(resampler.half_taps());
        let last = resampler.target_known_before(i64::from(from));
        let output = resampled(&source, from, to, first..last);
        for (n, sample) in (first..).zip(output) {
            assert!(
                (sample - 0.5).abs() < 1e-6,
                "{from}->{to} sample {n}: {sample}"
            );
        }
    }
}

#[test]
fn the_output_does_not_depend_on_how_the_source_is_split() {
    let source: Vec<f32> = (0..20_000_u32)
        .map(|n| ((n * 7_919) % 2_003) as f32 / 1_001.5 - 1.0)
        .collect();
    for (from, to) in [(44_100, 48_000), (48_000, 44_100), (96_000, 8_000)] {
        let resampler = Resampler::new(from, to).expect("rates");
        let end = resampler.target_at_or_after(20_000);
        let whole = resampled(&source, from, to, 0..end);
        let mut pieces = Vec::new();
        let mut start = 0;
        while start < end {
            let stop = (start + 37).min(end);
            let span = resampler.source_span(start..stop);
            let from_index = span.start.clamp(0, 20_000);
            let to_index = span.end.clamp(0, 20_000);
            let slice = &source[usize::try_from(from_index).expect("index")
                ..usize::try_from(to_index).expect("index")];
            let mut output = vec![0.0; usize::try_from(stop - start).expect("small")];
            resampler.resample(slice, from_index, start, &mut output);
            pieces.extend(output);
            start = stop;
        }
        assert_eq!(pieces, whole, "{from}->{to}");
    }
}

#[test]
fn tones_below_the_cutoff_pass_and_tones_above_it_are_removed() {
    let tone = |rate: u32, hz: f64, n: i64| {
        (libm::sin(2.0 * std::f64::consts::PI * hz * n as f64 / f64::from(rate)) * 0.5) as f32
    };
    // A browser's linear interpolation takes 1.5 dB off a 10 kHz tone and aliases it at -23 dB, and
    // 18 kHz loses 5 dB; this converter must pass all three.
    for hz in [1_000.0, 10_000.0, 18_000.0] {
        let source: Vec<f32> = (0..44_100).map(|n| tone(44_100, hz, n)).collect();
        let output = resampled(&source, 44_100, 48_000, 2_000..46_000);
        let error = (2_000..)
            .zip(&output)
            .map(|(n, sample)| (sample - tone(48_000, hz, n)).abs())
            .fold(0.0_f32, f32::max);
        assert!(error < 2e-3, "{hz} Hz: error {error}");
    }
    // Above the 44.1 kHz target's Nyquist frequency a tone has nowhere to go but an alias.
    let source: Vec<f32> = (0..96_000).map(|n| tone(96_000, 30_000.0, n)).collect();
    let output = resampled(&source, 96_000, 44_100, 2_000..42_000);
    let peak = output
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    assert!(peak < 5e-3, "23 kHz alias peak {peak}");
}

#[test]
fn a_source_sample_lands_at_its_exact_output_position() {
    // Source sample 441 at 44.1 kHz is 10 ms: output sample 480 at 48 kHz.
    let mut source = vec![0.0_f32; 2_000];
    source[441] = 1.0;
    let output = resampled(&source, 44_100, 48_000, 470..491);
    let peak = output
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(index, _)| 470 + index);
    assert_eq!(peak, Some(480));
}

#[test]
fn unusable_rates_are_refused() {
    assert!(Resampler::new(0, 48_000).is_err());
    assert!(Resampler::new(48_000, 0).is_err());
    assert!(Resampler::new(MAX_RESAMPLE_RATE + 1, 48_000).is_err());
    // A rate pair with more phases than the table holds still converts.
    let resampler = Resampler::new(44_101, 47_999).expect("rates");
    let source = vec![0.25_f32; 44_101];
    let mut output = vec![0.0; 100];
    resampler.resample(&source, 0, 1_000, &mut output);
    assert!(output.iter().all(|sample| (sample - 0.25).abs() < 1e-6));
}

#[test]
fn a_rate_pair_whose_filter_would_not_fit_the_table_is_refused() {
    // 768 kHz to 1 Hz would build a 24.6M-tap table (about 94 MiB). Not `expect_err`, which would
    // print the whole table.
    let Err(refused) = Resampler::new(MAX_RESAMPLE_RATE, 1) else {
        panic!("768 kHz to 1 Hz converts");
    };
    assert_eq!(
        (refused.source_rate, refused.target_rate),
        (MAX_RESAMPLE_RATE, 1)
    );
    assert!(refused.to_string().contains("8192"), "{refused}");
    // The bound is the fall itself: 8192 times converts, anything steeper does not.
    assert!(Resampler::new(8_192 * 90, 90).is_ok());
    assert!(Resampler::new(8_192 * 90 + 1, 90).is_err());
    assert!(Resampler::new(MAX_RESAMPLE_RATE, 93).is_err());
    let resampler = Resampler::new(MAX_RESAMPLE_RATE, 94).expect("a fall of 8170");
    let source = vec![0.25_f32; 800_000];
    let mut output = vec![0.0; 4];
    resampler.resample(&source, 0, 40, &mut output);
    assert!(
        output.iter().all(|sample| (sample - 0.25).abs() < 1e-5),
        "{output:?}"
    );
    // Rising rates keep the 32-tap filter at any ratio.
    assert!(Resampler::new(1, MAX_RESAMPLE_RATE).is_ok());
}

#[test]
fn a_filter_prepares_at_any_rate() {
    // At rates of 20 Hz and below the Nyquist cap falls under the 10 Hz floor, which must not make
    // `clamp` panic.
    for rate in [0, 1, 19, 20, 21, 8_000] {
        for kind in [
            BiquadKind::Peaking,
            BiquadKind::LowPass,
            BiquadKind::HighPass,
        ] {
            let mut chain = ProcessorChain::new(ChannelLayout::Stereo);
            chain
                .push(Biquad::new(kind, 1_000.0, 0.7, 6.0))
                .expect("one input");
            chain.prepare(rate, 64);
            let input = [0.25; 64];
            let mut left = [0.0; 64];
            let mut right = [0.0; 64];
            chain.process(&context(0), &[&input, &input], &mut [&mut left, &mut right]);
        }
    }
}

/// Builds a fresh processor for one case.
type MakeProcessor = Box<dyn Fn() -> Box<dyn Processor>>;

#[test]
fn a_sample_the_filters_cannot_hold_does_not_poison_them() {
    let sine = |index: usize| 0.5 * libm::sinf(index as f32 * 0.05);
    for bad in [
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
        3.0e38,
        -3.0e38,
        2.0e38,
    ] {
        let factories: [(&str, MakeProcessor); 4] = [
            (
                "peaking",
                Box::new(|| Box::new(Biquad::new(BiquadKind::Peaking, 1_000.0, 0.7, 0.0))),
            ),
            (
                "high-pass",
                Box::new(|| Box::new(Biquad::new(BiquadKind::HighPass, 80.0, 0.707, 0.0))),
            ),
            (
                "low-pass",
                Box::new(|| Box::new(Biquad::new(BiquadKind::LowPass, 8_000.0, 0.707, 0.0))),
            ),
            (
                "reverb",
                Box::new(|| Box::new(SchroederReverb::new(0.7, 0.3, 0.0))),
            ),
        ];
        for (name, factory) in factories {
            let mut processor = factory();
            processor.prepare(48_000, 4_800);
            // Both channels at once: a reverb sums them to mono, where two large values overflow.
            let input = (0..4_800)
                .map(|index| if index == 100 { bad } else { sine(index) })
                .collect::<Vec<_>>();
            let mut left = vec![0.0; 4_800];
            let mut right = vec![0.0; 4_800];
            processor.process(&context(0), &[&input, &input], &mut [&mut left, &mut right]);
            assert!(
                left.iter().chain(&right).all(|sample| sample.is_finite()),
                "{name} after {bad}: {} non-finite",
                left.iter()
                    .chain(&right)
                    .filter(|sample| !sample.is_finite())
                    .count()
            );
            let clean = (0..4_800).map(sine).collect::<Vec<_>>();
            let mut more_left = vec![0.0; 4_800];
            let mut more_right = vec![0.0; 4_800];
            processor.process(
                &context(4_800),
                &[&clean, &clean],
                &mut [&mut more_left, &mut more_right],
            );
            assert!(
                more_left.iter().any(|sample| sample.abs() > 0.01),
                "{name} after {bad} is silent"
            );
        }
    }
}

#[test]
fn a_compressor_follows_its_static_curve_and_is_exactly_unity_under_it() {
    // The gain comes from `exp10f` and the logarithm is skipped under the threshold; neither
    // shortcut may move the static curve.
    for (level_db, threshold_db, ratio) in [
        (-6.0_f32, -24.0_f32, 4.0_f32),
        (-1.0, -12.0, 2.0),
        (-30.0, -36.0, 100.0),
    ] {
        let amplitude = libm::exp10f(level_db / 20.0);
        let mut compressor = Compressor::new(threshold_db, ratio, 1.0, 50.0, 0.0);
        compressor.prepare(48_000, 4_800);
        let input = vec![amplitude; 4_800];
        let mut output = vec![0.0; 4_800];
        compressor.process(&context(0), &[&input], &mut [&mut output]);
        let gain_db = 20.0 * libm::log10f(output[4_799] / amplitude);
        let expected = (threshold_db + (level_db - threshold_db) / ratio) - level_db;
        assert!(
            (gain_db - expected).abs() < 0.01,
            "{level_db} dB over {threshold_db} at {ratio}:1 gives {gain_db}, not {expected}"
        );
    }
    for level_db in [-24.0_f32, -24.01, -60.0, -200.0] {
        let amplitude = libm::exp10f(level_db / 20.0);
        let mut compressor = Compressor::new(-24.0, 4.0, 1.0, 50.0, 0.0);
        compressor.prepare(48_000, 4_800);
        let input = vec![amplitude; 4_800];
        let mut output = vec![0.0; 4_800];
        compressor.process(&context(0), &[&input], &mut [&mut output]);
        assert!(
            input
                .iter()
                .zip(&output)
                .all(|(input, output)| input.to_bits() == output.to_bits()),
            "{level_db} dB"
        );
    }
}

#[test]
fn a_chain_refuses_a_processor_that_reads_a_second_bus() {
    // A chain gives each node only its own channels, so a Ducker would duck L under R and leave R
    // holding stale scratch, and a crossfade would write nothing.
    let mut chain = ProcessorChain::new(ChannelLayout::Stereo);
    assert_eq!(
        chain.push(Ducker::new(0.1, 12.0, 1.0, 100.0)),
        Err(ChainError::MultipleInputs)
    );
    assert_eq!(
        chain.push(EqualPowerCrossfade::new(0.5)),
        Err(ChainError::MultipleInputs)
    );
    chain.push(Gain::new(0.5)).expect("one input");
    chain.prepare(48_000, 8);
    let mut left = [7.0; 8];
    let mut right = [7.0; 8];
    chain.process(
        &context(0),
        &[&[0.2; 8], &[0.4; 8]],
        &mut [&mut left, &mut right],
    );
    assert_samples_close(&left, &[0.1; 8]);
    assert_samples_close(&right, &[0.2; 8]);
}

#[test]
fn a_chain_that_is_not_prepared_passes_its_input_through() {
    let input = [0.5; 8];
    let run = |chain: &mut ProcessorChain| {
        let mut output = [7.0; 8];
        chain.process(&context(0), &[&input], &mut [&mut output]);
        output
    };
    let mut never = ProcessorChain::new(ChannelLayout::Mono);
    never.push(Gain::new(0.5)).expect("one input");
    assert!(!never.is_prepared());
    assert_samples_close(&run(&mut never), &input);
    let mut emptied = ProcessorChain::new(ChannelLayout::Mono);
    emptied.prepare(48_000, 8);
    emptied.push(Gain::new(0.5)).expect("one input");
    assert_samples_close(&run(&mut emptied), &input);
    emptied.prepare(48_000, 8);
    assert_samples_close(&run(&mut emptied), &[0.25; 8]);
    let mut grown = ProcessorChain::new(ChannelLayout::Mono);
    grown.push(Gain::new(0.5)).expect("one input");
    grown.prepare(48_000, 8);
    grown
        .push(Compressor::new(-60.0, 100.0, 0.1, 10.0, 0.0))
        .expect("one input");
    assert_samples_close(&run(&mut grown), &input);
    grown.prepare(48_000, 8);
    assert!(run(&mut grown)[7] < 0.1);
    // Missing a channel there is nothing to process: the outputs are silenced.
    let mut stereo = ProcessorChain::new(ChannelLayout::Stereo);
    stereo.push(Gain::new(0.5)).expect("one input");
    stereo.prepare(48_000, 8);
    let mut left = [7.0; 8];
    stereo.process(&context(0), &[&input], &mut [&mut left]);
    assert_samples_close(&left, &[0.0; 8]);
}

#[test]
fn a_block_longer_than_the_prepared_one_runs_in_pieces() {
    let input = (0..100)
        .map(|index| index as f32 / 100.0)
        .collect::<Vec<_>>();
    let ramp = |chain: &mut ProcessorChain| {
        chain.push(Gain::new(0.5)).expect("one input");
        chain
            .push(Biquad::new(BiquadKind::LowPass, 2_000.0, 0.7, 0.0))
            .expect("one input");
    };
    let mut whole = ProcessorChain::new(ChannelLayout::Mono);
    ramp(&mut whole);
    whole.prepare(48_000, 100);
    let mut expected = vec![0.0; 100];
    whole.process(&context(0), &[&input], &mut [&mut expected]);
    let mut pieces = ProcessorChain::new(ChannelLayout::Mono);
    ramp(&mut pieces);
    pieces.prepare(48_000, 32);
    let mut actual = vec![7.0; 100];
    pieces.process(&context(0), &[&input], &mut [&mut actual]);
    assert_eq!(actual, expected);

    let mut graph = AudioGraph::new(ChannelLayout::Mono, 1);
    ramp(graph.clip_chain_mut(0).expect("bus"));
    graph.prepare(48_000, 32);
    let mut mixed = vec![7.0; 100];
    graph.process(&context(0), &[&input], &mut [&mut mixed]);
    assert_eq!(mixed, expected);
}

#[test]
fn a_graph_that_cannot_mix_a_block_silences_it() {
    let one = [1.0; 16];
    let mut graph = AudioGraph::new(ChannelLayout::Mono, 2);
    let mut output = [7.0; 16];
    graph.process(&context(0), &[&one, &one], &mut [&mut output]);
    assert_samples_close(&output, &[0.0; 16]);
    graph.prepare(48_000, 16);
    let mut output = [7.0; 16];
    graph.process(&context(0), &[&one], &mut [&mut output]);
    assert_samples_close(&output, &[0.0; 16]);
    graph
        .clip_chain_mut(0)
        .expect("bus")
        .push(Gain::new(0.5))
        .expect("one input");
    graph.process(&context(0), &[&one, &one], &mut [&mut output]);
    assert_samples_close(&output, &[2.0; 16]);
    graph.prepare(48_000, 16);
    graph.process(&context(0), &[&one, &one], &mut [&mut output]);
    assert_samples_close(&output, &[1.5; 16]);
}

/// Runs `input` through `chain` in `block`-sample blocks, each with the
/// context of its first sample.
fn run_chain(chain: &mut ProcessorChain, input: &[f32], block: usize) -> Vec<f32> {
    let mut output = vec![0.0; input.len()];
    for (index, (input, output)) in input
        .chunks(block)
        .zip(output.chunks_mut(block))
        .enumerate()
    {
        chain.process(
            &context(i64::try_from(index * block).expect("small")),
            &[input],
            &mut [output],
        );
    }
    output
}

/// A compressor that never reaches its threshold: a pure 10 ms (480-sample) delay.
fn lookahead_delay() -> Compressor {
    Compressor::new(0.0, 4.0, 5.0, 80.0, 10.0)
}

/// The output of `addressed` before and after a 480-sample lookahead, fed
/// a second of a constant in 512-sample blocks.
fn before_and_after_a_lookahead<P: Processor + 'static>(
    addressed: impl Fn() -> P,
) -> (Vec<f32>, Vec<f32>) {
    let input = vec![0.5_f32; 48_000];
    let mut before = ProcessorChain::new(ChannelLayout::Mono);
    before.push(addressed()).expect("one input");
    before.push(lookahead_delay()).expect("one input");
    before.prepare(48_000, 512);
    let mut after = ProcessorChain::new(ChannelLayout::Mono);
    after.push(lookahead_delay()).expect("one input");
    after.push(addressed()).expect("one input");
    after.prepare(48_000, 512);
    assert_eq!(before.latency_samples(), 480);
    assert_eq!(after.latency_samples(), 480);
    (
        run_chain(&mut before, &input, 512),
        run_chain(&mut after, &input, 512),
    )
}

#[test]
fn a_node_after_a_latent_one_sees_the_position_of_the_audio_it_gets() {
    // With the chain input's position, a fade or automation after a lookahead would run 480 samples
    // ahead of the audio it scales.
    let (expected, actual) = before_and_after_a_lookahead(|| {
        LinearFade::new(
            MediaTime::ZERO,
            MediaTime(TICKS_PER_SECOND),
            FadeDirection::Out,
        )
        .expect("fade")
    });
    assert!(expected[481..].iter().all(|sample| *sample < 0.5));
    assert_same_bits(&actual, &expected, "fade");
    let (expected, actual) = before_and_after_a_lookahead(|| {
        AutomatedGain::new(
            AutomationCurve::new(vec![
                AutomationPoint {
                    sample: 0,
                    value: 1.0,
                },
                AutomationPoint {
                    sample: 24_000,
                    value: 0.0,
                },
            ])
            .expect("curve"),
            0.0,
        )
    });
    assert_same_bits(&actual, &expected, "automation");
}

#[test]
fn a_bypassed_chain_keeps_its_latency_and_its_nodes_keep_running() {
    for (in_place, recorded) in [(false, true), (true, true), (false, false), (true, false)] {
        let name = format!("in place {in_place}, recorded {recorded}");
        let mut chain = ProcessorChain::new(ChannelLayout::Mono);
        chain
            .push(Compressor::new(-12.0, 4.0, 10.0, 100.0, 50.0))
            .expect("one input");
        if recorded {
            // A host that may bypass says so before it prepares, and the chain keeps a record of
            // its input from then on.
            chain.set_bypass(false);
        }
        if in_place {
            chain.prepare_nodes(48_000, 512);
        } else {
            chain.prepare(48_000, 512);
        }
        assert_eq!(chain.latency_samples(), 2_400);
        let mut position = 0;
        let mut run = |chain: &mut ProcessorChain, level: f32, blocks: usize| {
            let mut output = Vec::new();
            for _ in 0..blocks {
                let ctx = context(position);
                position += 512;
                if in_place {
                    let mut buffer = [vec![level; 512], Vec::new()];
                    let mut spare = [vec![0.0; 512], Vec::new()];
                    chain.process_in_place(&ctx, &mut buffer, &mut spare, 512);
                    output.extend_from_slice(&buffer[0]);
                } else {
                    let mut block = [0.0; 512];
                    chain.process(&ctx, &[&[level; 512]], &mut [&mut block]);
                    output.extend_from_slice(&block);
                }
            }
            output
        };
        // Under the threshold (0.25) the compressor is a pure 2400-sample delay.
        run(&mut chain, 0.05, 10);
        chain.set_bypass(true);
        assert_eq!(chain.latency_samples(), 2_400, "{name}");
        let bypassed = run(&mut chain, 0.1, 10);
        // Bypassed, the chain is still 2400 samples late: the audio from before the bypass first,
        // or silence where it kept no record of it.
        let before = if recorded { 0.05 } else { 0.0 };
        assert!(all_exactly(&bypassed[..2_400], before), "{name}");
        assert!(all_exactly(&bypassed[2_400..], 0.1), "{name}");
        chain.set_bypass(false);
        let resumed = run(&mut chain, 0.15, 10);
        // The nodes kept running through the bypass: next comes the audio of 2400 samples ago.
        assert!(all_exactly(&resumed[..2_400], 0.1), "{name}");
        assert!(all_exactly(&resumed[2_400..], 0.15), "{name}");
        if recorded && !in_place {
            chain.set_bypass(true);
            let state = chain.save_state();
            let mut restored = ProcessorChain::new(ChannelLayout::Mono);
            restored
                .push(Compressor::new(-12.0, 4.0, 10.0, 100.0, 50.0))
                .expect("one input");
            restored.set_bypass(true);
            restored.prepare(48_000, 512);
            restored.load_state(&state).expect("same chain");
            let mut block = [0.0; 512];
            restored.process(&context(position), &[&[0.2; 512]], &mut [&mut block]);
            assert!(all_exactly(&block, 0.15), "{name}");
        }
    }
}

/// A two-bus mono graph with a 480-sample lookahead on bus 0's `chain`
/// (clip or track) and none on bus 1.
fn graph_with_one_latent_bus(track: bool) -> AudioGraph {
    let mut graph = AudioGraph::new(ChannelLayout::Mono, 2);
    let chain = if track {
        graph.track_bus_mut(0)
    } else {
        graph.clip_chain_mut(0)
    };
    chain
        .expect("bus")
        .push(lookahead_delay())
        .expect("one input");
    graph.prepare(48_000, 512);
    graph
}

#[test]
fn buses_of_different_latency_mix_aligned() {
    // Summed as they come, a bus with a lookahead and one without would mix 480 samples apart while
    // the graph reports 480 samples of latency.
    let mut impulse = vec![0.0_f32; 4_096];
    impulse[1_000] = 0.5;
    for track in [false, true] {
        let mut graph = graph_with_one_latent_bus(track);
        assert_eq!(graph.latency_samples(), 480);
        let mut output = vec![0.0; 4_096];
        for (index, (input, output)) in impulse.chunks(512).zip(output.chunks_mut(512)).enumerate()
        {
            graph.process(
                &context(i64::try_from(index * 512).expect("small")),
                &[input, input],
                &mut [output],
            );
        }
        let sounding = output
            .iter()
            .enumerate()
            .filter(|(_, sample)| **sample != 0.0)
            .map(|(index, sample)| (index, *sample))
            .collect::<Vec<_>>();
        assert_eq!(sounding, [(1_480, 1.0)], "track {track}");
    }
}

#[test]
fn an_aligned_graph_continues_across_saved_state() {
    // The delay that aligns the buses is history, so a chunk boundary must carry it.
    let input = (0..2_048)
        .map(|index| libm::sinf(index as f32 * 0.05))
        .collect::<Vec<_>>();
    let mut continuous = graph_with_one_latent_bus(false);
    let mut expected = vec![0.0; 2_048];
    continuous.process(&context(0), &[&input, &input], &mut [&mut expected]);
    let mut first = graph_with_one_latent_bus(false);
    let mut actual = vec![0.0; 2_048];
    first.process(
        &context(0),
        &[&input[..700], &input[..700]],
        &mut [&mut actual[..700]],
    );
    let mut restored = graph_with_one_latent_bus(false);
    restored
        .load_state(&first.save_state())
        .expect("same graph");
    restored.process(
        &context(700),
        &[&input[700..], &input[700..]],
        &mut [&mut actual[700..]],
    );
    assert_same_bits(&actual, &expected, "restored");
}

/// Whether every sample is exactly `value`.
fn all_exactly(samples: &[f32], value: f32) -> bool {
    samples
        .iter()
        .all(|sample| sample.to_bits() == value.to_bits())
}

/// Fails at the first sample whose bits differ.
fn assert_same_bits(actual: &[f32], expected: &[f32], name: &str) {
    assert_eq!(actual.len(), expected.len(), "{name}");
    if let Some(index) = actual
        .iter()
        .zip(expected)
        .position(|(actual, expected)| actual.to_bits() != expected.to_bits())
    {
        panic!(
            "{name}: sample {index} is {}, not {}",
            actual[index], expected[index]
        );
    }
}

/// A stereo sine run through `processor`, so its history holds audio.
fn played<P: Processor>(mut processor: P, sample_rate: u32, phase: f32) -> P {
    processor.prepare(sample_rate, 512);
    let left = (0..512)
        .map(|index| libm::sinf(index as f32 * 0.05 + phase) * 0.8)
        .collect::<Vec<_>>();
    let right = left.iter().map(|sample| -sample).collect::<Vec<_>>();
    let mut out_left = vec![0.0; 512];
    let mut out_right = vec![0.0; 512];
    for _ in 0..8 {
        processor.process(
            &context(0),
            &[&left, &right],
            &mut [&mut out_left, &mut out_right],
        );
    }
    processor
}

/// `state` with the f32 at byte `offset` replaced by NaN.
fn with_nan_at(state: &StateBlob, offset: usize) -> StateBlob {
    let mut bytes = state.as_bytes().to_vec();
    bytes[offset..offset + 4].copy_from_slice(&f32::NAN.to_le_bytes());
    StateBlob::new(bytes)
}

/// Loads `state` into `processor` and asserts it is refused whole: the
/// processor saves the same state as before.
fn assert_refused<P: Processor>(processor: &mut P, state: &StateBlob, what: &str) {
    let before = processor.save_state();
    #[allow(clippy::let_unit_value, clippy::ignored_unit_patterns)]
    let _ = processor.load_state(state);
    assert!(
        processor.save_state() == before,
        "{what}: a refused state changed the processor"
    );
}

#[test]
fn a_processor_refuses_a_state_it_cannot_take_whole() {
    let reverb_44 = played(SchroederReverb::new(0.8, 0.3, 0.5), 44_100, 0.0);
    let reverb_48 = played(SchroederReverb::new(0.8, 0.3, 0.5), 48_000, 1.0);
    let saved_48 = reverb_48.save_state();
    let mut target = reverb_48;
    assert_refused(&mut target, &reverb_44.save_state(), "reverb from 44.1 kHz");
    let half = StateBlob::new(saved_48.as_bytes()[..saved_48.as_bytes().len() / 2].to_vec());
    assert_refused(&mut target, &half, "reverb cut in half");
    let last = saved_48.as_bytes().len() - 4;
    assert_refused(
        &mut target,
        &with_nan_at(&saved_48, last),
        "reverb with a NaN sample",
    );

    let mut biquad = played(
        Biquad::new(BiquadKind::LowPass, 2_000.0, 0.7, 0.0),
        48_000,
        0.0,
    );
    let saved = biquad.save_state();
    let mut nan_z2 = saved.as_bytes().to_vec();
    nan_z2[24..].copy_from_slice(&f64::NAN.to_le_bytes());
    assert_refused(&mut biquad, &StateBlob::new(nan_z2), "biquad with a NaN z2");

    let mut compressor = played(Compressor::new(-30.0, 4.0, 1.0, 50.0, 5.0), 48_000, 0.0);
    let saved = compressor.save_state();
    assert_refused(
        &mut compressor,
        &with_nan_at(&saved, 20),
        "compressor with a NaN delay",
    );

    let mut gain = Gain::new(0.5);
    assert_refused(&mut gain, &saved, "a gain given another processor's state");

    let mut reverb = played(SchroederReverb::new(0.8, 0.3, 0.5), 48_000, 1.0);
    assert_eq!(
        reverb.load_state(&reverb_44.save_state()),
        Err(StateError::Mismatch)
    );
    assert_eq!(
        reverb.load_state(&with_nan_at(&saved_48, last)),
        Err(StateError::Invalid)
    );
    assert_eq!(reverb.load_state(&saved_48), Ok(()));
    assert_eq!(gain.load_state(&StateBlob::default()), Ok(()));
}

#[test]
fn a_chain_refuses_a_state_one_of_its_nodes_cannot_take_and_keeps_the_others() {
    fn chain(lookahead_ms: f32, sample_rate: u32, phase: f32) -> ProcessorChain {
        let mut chain = ProcessorChain::new(ChannelLayout::Mono);
        chain
            .push(SchroederReverb::new(0.6, 0.4, 0.5))
            .expect("one input");
        chain
            .push(Compressor::new(-30.0, 4.0, 1.0, 50.0, lookahead_ms))
            .expect("one input");
        chain.prepare(sample_rate, 256);
        let input = (0..256)
            .map(|index| libm::sinf(index as f32 * 0.05 + phase) * 0.8)
            .collect::<Vec<_>>();
        let mut output = vec![0.0; 256];
        for _ in 0..8 {
            chain.process(&context(0), &[&input], &mut [&mut output]);
        }
        chain
    }
    let other = chain(5.0, 48_000, 0.0).save_state();
    let mut target = chain(2.0, 48_000, 1.0);
    let before = target.save_state();
    #[allow(clippy::let_unit_value, clippy::ignored_unit_patterns)]
    let _ = target.load_state(&other);
    assert!(
        target.save_state() == before,
        "the reverb took its part of a state the compressor after it refused"
    );
    let error = target.load_state(&other).expect_err("a mismatched state");
    assert_eq!((error.node, error.error), (Some(1), StateError::Mismatch));
    assert_eq!(
        target
            .load_state(&StateBlob::new(other.as_bytes()[..10].to_vec()))
            .map_err(|error| (error.node, error.error)),
        Err((None, StateError::Mismatch))
    );
    let same = chain(2.0, 48_000, 0.0).save_state();
    assert_eq!(target.load_state(&same), Ok(()));
    assert!(target.save_state() == same, "a fitting state loads whole");
}

#[test]
fn a_graph_refuses_a_state_one_of_its_chains_cannot_take_and_keeps_the_others() {
    /// Two buses, each clip chain a reverb; track bus 0 an EQ or a gain,
    /// track bus 1 a lookahead compressor.
    fn graph(lookahead_ms: f32, eq: bool, phase: f32) -> AudioGraph {
        let mut graph = AudioGraph::new(ChannelLayout::Mono, 2);
        for bus in 0..2 {
            graph
                .clip_chain_mut(bus)
                .expect("bus")
                .push(SchroederReverb::new(0.6, 0.4, 0.5))
                .expect("one input");
        }
        let track = graph.track_bus_mut(0).expect("bus");
        if eq {
            track
                .push(Biquad::new(BiquadKind::LowPass, 2_000.0, 0.7, 0.0))
                .expect("one input");
        } else {
            track.push(Gain::new(0.5)).expect("one input");
        }
        graph
            .track_bus_mut(1)
            .expect("bus")
            .push(Compressor::new(-30.0, 4.0, 1.0, 50.0, lookahead_ms))
            .expect("one input");
        graph.prepare(48_000, 256);
        let input = (0..256)
            .map(|index| libm::sinf(index as f32 * 0.05 + phase) * 0.8)
            .collect::<Vec<_>>();
        let mut output = vec![0.0; 256];
        for _ in 0..8 {
            graph.process(&context(0), &[&input, &input], &mut [&mut output]);
        }
        graph
    }
    let mut target = graph(2.0, true, 1.0);
    let before = target.save_state();
    // Another lookahead: bus 0 waits for bus 1 by another length.
    let other = graph(5.0, true, 0.0).save_state();
    assert!(
        target.load_state(&other).is_err(),
        "a mismatched state loads"
    );
    assert!(
        target.save_state() == before,
        "a refused state changed the graph"
    );
    // The same latencies, but a gain where the EQ was: the clip chains before that track bus take
    // their parts first.
    let other = graph(2.0, false, 0.0).save_state();
    assert!(
        target.load_state(&other).is_err(),
        "a mismatched state loads"
    );
    assert!(
        target.save_state() == before,
        "the clip chains took their part of a state a track bus refused"
    );
}

#[test]
fn one_node_state_moves_to_the_node_that_replaces_it() {
    let input = (0..512)
        .map(|index| libm::sinf(index as f32 * 0.05))
        .collect::<Vec<_>>();
    let chain_of = |frequency: f32| {
        let mut chain = ProcessorChain::new(ChannelLayout::Mono);
        chain
            .push(Biquad::new(BiquadKind::LowPass, frequency, 0.7, 0.0))
            .expect("one input");
        chain.push(Gain::new(0.5)).expect("one input");
        chain.prepare(48_000, 512);
        chain
    };
    let mut first = chain_of(1_000.0);
    let mut output = vec![0.0; 512];
    first.process(&context(0), &[&input], &mut [&mut output]);
    let state = first.node_state(0).expect("a first node");
    assert_eq!(first.node_state(2), None);

    // A node of the same kind with other parameters takes the history.
    let mut edited = chain_of(2_000.0);
    assert_eq!(edited.load_node_state(0, &state), Ok(()));
    let mut reference = Biquad::new(BiquadKind::LowPass, 2_000.0, 0.7, 0.0);
    reference.prepare(48_000, 512);
    reference.load_state(&state).expect("same kind");
    let next = [0.0_f32; 64];
    let mut expected = vec![0.0; 64];
    reference.process(&context(512), &[&next], &mut [&mut expected]);
    for sample in &mut expected {
        *sample *= 0.5;
    }
    let mut actual = vec![0.0; 64];
    edited.process(&context(512), &[&next], &mut [&mut actual]);
    assert_same_bits(&actual, &expected, "edited");
    assert!(actual.iter().any(|sample| *sample != 0.0));

    assert_eq!(edited.load_node_state(2, &state), Err(StateError::Mismatch));
    assert_eq!(
        edited.load_node_state(0, &StateBlob::new(vec![0; 3])),
        Err(StateError::Mismatch)
    );
}

#[test]
fn an_interpolator_widens_with_speed_up_to_its_bound_and_keeps_a_constant_constant() {
    for (speed, half) in [
        (0.25, 16),
        (1.0, 16),
        (1.25, 20),
        (2.0, 32),
        (MAX_FILTERED_SPEED, 1_024),
        (1.0e6, 1_024),
        (f64::NAN, 16),
    ] {
        let interpolator = Interpolator::new(speed);
        assert_eq!(interpolator.half_taps(), half, "speed {speed}");
        let mut weights = Vec::new();
        for fraction in [0.0, 0.125, 0.499, 0.501, 0.999] {
            interpolator.weights(fraction, &mut weights);
            assert_eq!(weights.len(), 2 * usize::try_from(half).expect("small"));
            let sum: f64 = weights.iter().sum();
            assert!(
                (sum - 1.0).abs() < 1e-12,
                "speed {speed} at {fraction}: {sum}"
            );
            let peak = weights
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|(index, _)| index)
                .expect("weights");
            let nearest = usize::try_from(half - 1).expect("small") + usize::from(fraction > 0.5);
            assert_eq!(peak, nearest, "speed {speed} at {fraction}");
        }
    }
}
