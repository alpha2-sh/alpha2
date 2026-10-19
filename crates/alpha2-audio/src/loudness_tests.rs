use super::{LoudnessMeter, LoudnessNormalization};

/// A stereo 1 kHz sine at `dbfs` peak on both channels for `seconds`,
/// at `rate`, with a phase that carries on from `start` samples.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn sine(rate: u32, dbfs: f64, seconds: f64, start: u64) -> Vec<f32> {
    let amplitude = libm::pow(10.0, dbfs / 20.0);
    let frames = (seconds * f64::from(rate)).round() as u64;
    (start..start + frames)
        .map(|index| {
            let phase = 2.0 * core::f64::consts::PI * 1_000.0 * index as f64 / f64::from(rate);
            (amplitude * libm::sin(phase)) as f32
        })
        .collect()
}

/// Feeds segments of `(dbfs, seconds)` as one stereo program in blocks
/// of 1024 frames.
fn measure(rate: u32, segments: &[(f64, f64)]) -> super::LoudnessReport {
    let mut meter = LoudnessMeter::new(rate, 2).expect("meter");
    let mut position = 0;
    for &(dbfs, seconds) in segments {
        let signal = sine(rate, dbfs, seconds, position);
        position += signal.len() as u64;
        for block in signal.chunks(1_024) {
            meter.push(&[block, block]).expect("block");
        }
    }
    meter.report()
}

fn near(actual: Option<f64>, expected: f64, tolerance: f64) {
    let actual = actual.expect("a measurement");
    assert!(
        (actual - expected).abs() <= tolerance,
        "{actual} is not {expected} +/- {tolerance}"
    );
}

#[test]
fn ebu_tech_3341_cases_1_to_5() {
    // Case 1 and 2: a -23 and a -33 dBFS stereo sine read -23 and -33 LUFS, momentary,
    // short-term, and integrated.
    for (level, rate) in [(-23.0, 48_000), (-33.0, 48_000), (-23.0, 44_100)] {
        let mut meter = LoudnessMeter::new(rate, 2).expect("meter");
        let signal = sine(rate, level, 20.0, 0);
        meter.push(&[&signal, &signal]).expect("block");
        near(meter.momentary_lufs(), level, 0.1);
        near(meter.short_term_lufs(), level, 0.1);
        near(meter.report().integrated_lufs, level, 0.1);
    }
    // Case 3: -36, -23, -36 dBFS; the quiet parts are gated out.
    near(
        measure(48_000, &[(-36.0, 10.0), (-23.0, 60.0), (-36.0, 10.0)]).integrated_lufs,
        -23.0,
        0.1,
    );
    // Case 4: -72, -36, -23, -36, -72 dBFS.
    near(
        measure(
            48_000,
            &[
                (-72.0, 10.0),
                (-36.0, 10.0),
                (-23.0, 60.0),
                (-36.0, 10.0),
                (-72.0, 10.0),
            ],
        )
        .integrated_lufs,
        -23.0,
        0.1,
    );
    // Case 5: -26, -20, -26 dBFS over 20, 20.1, and 20 s.
    near(
        measure(48_000, &[(-26.0, 20.0), (-20.0, 20.1), (-26.0, 20.0)]).integrated_lufs,
        -23.0,
        0.1,
    );
}

#[test]
fn k_weighting_holds_at_every_supported_rate() {
    for rate in [
        8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000,
    ] {
        near(measure(rate, &[(-23.0, 10.0)]).integrated_lufs, -23.0, 0.1);
    }
}

#[test]
fn ebu_tech_3342_loudness_range_cases_1_to_4() {
    for (segments, range) in [
        (vec![(-20.0, 20.0), (-30.0, 20.0)], 10.0),
        (vec![(-20.0, 20.0), (-15.0, 20.0)], 5.0),
        (vec![(-40.0, 20.0), (-20.0, 20.0)], 20.0),
        (
            vec![
                (-50.0, 20.0),
                (-35.0, 20.0),
                (-20.0, 20.0),
                (-35.0, 20.0),
                (-50.0, 20.0),
            ],
            15.0,
        ),
    ] {
        near(measure(48_000, &segments).loudness_range_lu, range, 1.0);
    }
}

#[test]
fn true_peak_finds_the_peak_between_samples() {
    // A sine at a quarter of the rate, 45 degrees out of phase with the samples: every sample
    // reads 1/sqrt(2) of the peak, which lies halfway between two samples.
    let rate = 48_000;
    let amplitude = 0.5_f64;
    let signal = (0..48_000)
        .map(|index| {
            let phase =
                core::f64::consts::FRAC_PI_2 * f64::from(index) + core::f64::consts::FRAC_PI_4;
            #[allow(clippy::cast_possible_truncation)]
            let sample = (amplitude * libm::sin(phase)) as f32;
            sample
        })
        .collect::<Vec<_>>();
    let mut meter = LoudnessMeter::new(rate, 1).expect("meter");
    meter.push(&[&signal]).expect("block");
    let report = meter.report();
    let expected = 20.0 * libm::log10(amplitude);
    near(report.sample_peak_dbfs, expected - 3.0103, 0.01);
    // BS.1770-4 Annex 2 allows +0.2 / -0.4 dB.
    let peak = report.true_peak_dbtp.expect("a peak");
    assert!(
        peak <= expected + 0.2 && peak >= expected - 0.4,
        "{peak} against {expected}"
    );
    let mut silent = LoudnessMeter::new(rate, 1).expect("meter");
    silent.push(&[&vec![0.0; 48_000]]).expect("block");
    assert_eq!(silent.report().true_peak_dbtp, None);
    assert_eq!(silent.report().integrated_lufs, None);
}

#[test]
fn block_boundaries_do_not_change_a_measurement() {
    let signal = sine(44_100, -18.0, 5.0, 0)
        .into_iter()
        .enumerate()
        .map(|(index, sample)| if index % 3 == 0 { sample * 0.5 } else { sample })
        .collect::<Vec<_>>();
    let whole = {
        let mut meter = LoudnessMeter::new(44_100, 1).expect("meter");
        meter.push(&[&signal]).expect("block");
        meter.report()
    };
    for block in [1, 37, 4_410, 100_000] {
        let mut meter = LoudnessMeter::new(44_100, 1).expect("meter");
        for chunk in signal.chunks(block) {
            meter.push(&[chunk]).expect("block");
        }
        assert_eq!(meter.report(), whole, "block {block}");
    }
}

#[test]
fn normalization_meets_the_target_unless_the_ceiling_is_lower() {
    let report = measure(48_000, &[(-23.0, 10.0)]);
    let quiet = LoudnessNormalization::plan(&report, -14.0, -1.0, None).expect("a plan");
    assert!(!quiet.peak_limited);
    assert!((quiet.integrated_lufs + 14.0).abs() < 1e-9);
    // A sine at -23 dBFS peaks near -23 dBTP; +22 dB would pass -1 dBTP.
    let loud = LoudnessNormalization::plan(&report, 0.0, -1.0, None).expect("a plan");
    assert!(loud.peak_limited);
    assert!((loud.true_peak_dbtp + 1.0).abs() < 1e-9);
    assert!(loud.integrated_lufs < 0.0);
    assert!((loud.linear_gain() - libm::pow(10.0, loud.gain_db / 20.0)).abs() < 1e-12);
    let silent = LoudnessMeter::new(48_000, 1).expect("meter").report();
    assert_eq!(
        LoudnessNormalization::plan(&silent, -14.0, -1.0, None),
        None
    );
}

#[test]
fn normalization_keeps_the_sample_peak_under_a_master_limiter() {
    // A -30 dBFS sine (-30 LUFS stereo) asked for -16 LUFS: +14 dB would put its peaks at
    // -16 dBFS.
    let report = measure(48_000, &[(-30.0, 10.0)]);
    let peak = report.sample_peak_dbfs.expect("a peak");
    let unlimited = LoudnessNormalization::plan(&report, -16.0, -1.0, None).expect("a plan");
    // The 0 dBFS limiter every project has by default is above the ceiling: the same plan, to
    // the bit.
    assert_eq!(
        LoudnessNormalization::plan(&report, -16.0, -1.0, Some(1.0)),
        Some(unlimited)
    );
    assert!(!unlimited.peak_limited && !unlimited.limiter_limited);
    // A 0.1 (-20 dBFS) limiter: the gain stops short of it, so the limiter never acts.
    let limited = LoudnessNormalization::plan(&report, -16.0, -1.0, Some(0.1)).expect("a plan");
    assert!(
        limited.peak_limited && limited.limiter_limited,
        "{limited:?}"
    );
    let expected = -20.0 - super::LIMITER_MARGIN_DB - peak;
    assert!((limited.gain_db - expected).abs() < 1e-9, "{limited:?}");
    assert!((limited.integrated_lufs - (-30.0 + expected)).abs() < 0.1);
    assert!(peak + limited.gain_db < -20.0);
    let ceiling = LoudnessNormalization::plan(&report, -16.0, -25.0, Some(0.1)).expect("a plan");
    assert!(
        ceiling.peak_limited && !ceiling.limiter_limited,
        "{ceiling:?}"
    );
    assert!((ceiling.true_peak_dbtp + 25.0).abs() < 1e-9);
    let quiet = LoudnessNormalization::plan(&report, -24.0, -1.0, Some(0.1)).expect("a plan");
    assert!(!quiet.peak_limited && !quiet.limiter_limited);
    for threshold in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(
            LoudnessNormalization::plan(&report, -16.0, -1.0, Some(threshold)),
            None,
            "{threshold}"
        );
    }
}

#[test]
fn the_filters_never_settle_into_subnormal_state() {
    // In digital silence the K-weighting recursions would otherwise sink into the f64 subnormal
    // range and stay there, where every multiply is slow.
    for rate in [8_000, 44_100, 48_000, 96_000] {
        let mut meter = LoudnessMeter::new(rate, 2).expect("meter");
        let burst = sine(rate, -6.0, 0.5, 0);
        meter.push(&[&burst, &burst]).expect("block");
        let silence = vec![0.0_f32; 128];
        for block in 0..(rate as usize * 10 / 128) {
            meter.push(&[&silence, &silence]).expect("block");
            let subnormal = meter
                .channels
                .iter()
                .flat_map(|channel| {
                    [channel.shelf, channel.high_pass]
                        .into_iter()
                        .flat_map(|filter| filter.x.into_iter().chain(filter.y))
                })
                .find(|value| value.is_subnormal());
            assert_eq!(subnormal, None, "{rate} Hz, block {block}");
        }
        assert!(meter.report().integrated_lufs.is_some());
    }
}

#[test]
fn a_sample_that_is_not_a_number_measures_as_silence() {
    // +inf at the last sample of step 29 would make a short-term power of +inf, which the
    // loudness range cannot sort into a percentile; elsewhere one bad sample could silently
    // truncate the measurement.
    let rate = 48_000;
    let clean = sine(rate, -20.0, 10.0, 0);
    let measure_with = |index: usize, value: f32| {
        let mut signal = clean.clone();
        signal[index] = value;
        let mut meter = LoudnessMeter::new(rate, 2).expect("meter");
        for block in signal.chunks(4_096) {
            meter.push(&[block, block]).expect("block");
        }
        meter.report()
    };
    for index in [143_999, 191_999, 143_998, 28_799, 100_000] {
        let silent = measure_with(index, 0.0);
        for value in [f32::INFINITY, f32::NEG_INFINITY, f32::NAN] {
            assert_eq!(measure_with(index, value), silent, "{value} at {index}");
        }
    }
    let range = |powers: &[f64]| {
        let mut histogram = super::Histogram::new();
        for &power in powers {
            histogram.add(power);
        }
        histogram.range(super::RANGE_RELATIVE_GATE)
    };
    assert_eq!(range(&[f64::INFINITY]), None);
    assert_eq!(range(&[f64::NAN, 1.0e-3]), Some(0.0));
    let unmeasurable = super::LoudnessReport {
        integrated_lufs: Some(-20.0),
        true_peak_dbtp: Some(f64::INFINITY),
        ..super::LoudnessReport::default()
    };
    assert_eq!(
        LoudnessNormalization::plan(&unmeasurable, -14.0, -1.0, None),
        None
    );
    let unmeasurable = super::LoudnessReport {
        integrated_lufs: Some(f64::NAN),
        ..super::LoudnessReport::default()
    };
    assert_eq!(
        LoudnessNormalization::plan(&unmeasurable, -14.0, -1.0, None),
        None
    );
}

/// BS.1770-4 eq. 7 and EBU Tech 3342 over every value kept, as the
/// histograms stand in for.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn kept(powers: &[f64], relative: f64) -> (Option<f64>, Option<f64>) {
    let over = |threshold: f64| {
        powers
            .iter()
            .copied()
            .filter(|&power| super::lufs(power) > threshold)
            .collect::<Vec<_>>()
    };
    let absolute = over(super::ABSOLUTE_GATE_LUFS);
    if absolute.is_empty() {
        return (None, None);
    }
    let mean = |values: &[f64]| values.iter().sum::<f64>() / values.len() as f64;
    let threshold = (super::lufs(mean(&absolute)) + relative).max(super::ABSOLUTE_GATE_LUFS);
    let gated = over(threshold);
    let mut loudness = gated.iter().copied().map(super::lufs).collect::<Vec<_>>();
    loudness.sort_by(f64::total_cmp);
    let last = loudness.len() as f64 - 1.0;
    let at = |percentile: f64| loudness[(last * percentile + 0.5) as usize];
    (Some(super::lufs(mean(&gated))), Some(at(0.95) - at(0.10)))
}

#[test]
fn the_histograms_measure_as_keeping_every_value_does_to_their_bins() {
    // Powers from -90 to +6 LUFS in a few clusters, as a programme with quiet passages has,
    // and spread evenly; a meter keeping every one and the histograms agree to their bin.
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        #[allow(clippy::cast_precision_loss)]
        let unit = (state >> 11) as f64 / (1_u64 << 53) as f64;
        unit
    };
    for (centres, spread) in [
        (&[-23.0][..], 3.0),
        (&[-40.0, -18.0][..], 2.0),
        (&[-75.0, -62.0, -30.0, -9.0][..], 6.0),
        (&[-42.0][..], 48.0),
    ] {
        let powers = (0..20_000)
            .map(|index| {
                let loudness = centres[index % centres.len()] + spread * (2.0 * next() - 1.0);
                libm::pow(10.0, (loudness + 0.691) / 10.0)
            })
            .collect::<Vec<_>>();
        let mut histogram = super::Histogram::new();
        for &power in &powers {
            histogram.add(power);
        }
        for relative in [super::INTEGRATED_RELATIVE_GATE, super::RANGE_RELATIVE_GATE] {
            let (integrated, range) = kept(&powers, relative);
            let binned = histogram.gated_mean(relative).map(super::lufs);
            near(binned, integrated.expect("loud enough"), 0.01);
            near(histogram.range(relative), range.expect("loud enough"), 0.02);
        }
    }
}

#[test]
fn a_block_under_the_absolute_gate_never_counts() {
    // BS.1770-4 eq. 7 (and pyloudnorm, libebur128): only blocks over both gates count, so a
    // block the absolute gate refused never counts again under a relative gate below -70 LUFS.
    for (segments, expected) in [
        (vec![(-62.0, 10.0), (-71.0, 30.0)], -62.05),
        (vec![(-65.0, 10.0), (-73.0, 10.0)], -65.05),
        (vec![(-66.0, 10.0), (-72.0, 10.0), (-75.0, 10.0)], -66.04),
    ] {
        near(measure(48_000, &segments).integrated_lufs, expected, 0.05);
    }
}

/// The integrated loudness of a -20 dBFS sine of `frequency` on channel
/// `lit` alone of `channels`, at 48 kHz, as `meter` weights them.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn one_channel(
    meter: &mut LoudnessMeter,
    channels: usize,
    lit: usize,
    frequency: f64,
) -> Option<f64> {
    let tone = (0..96_000)
        .map(|index| {
            let phase = 2.0 * core::f64::consts::PI * frequency * f64::from(index) / 48_000.0;
            (0.1 * libm::sin(phase)) as f32
        })
        .collect::<Vec<_>>();
    let silence = vec![0.0_f32; tone.len()];
    let planes = (0..channels)
        .map(|channel| {
            if channel == lit {
                tone.as_slice()
            } else {
                silence.as_slice()
            }
        })
        .collect::<Vec<_>>();
    meter.push(&planes).expect("block");
    meter.report().integrated_lufs
}

#[test]
fn surround_channels_weigh_as_bs_1770_4_weighs_them() {
    let level = |channels: usize, lit: usize, frequency: f64| {
        one_channel(
            &mut LoudnessMeter::new(48_000, channels).expect("meter"),
            channels,
            lit,
            frequency,
        )
    };
    let front = level(2, 0, 1_000.0).expect("loud enough");
    near(Some(front), -23.0, 0.1);
    // +1.5 dB (10 log10 1.41) for a side surround: Ls and Rs of 5.0 and 5.1, SL and SR of 6.1
    // and 7.1.
    for (channels, sides) in [(5, [3, 4]), (6, [4, 5]), (7, [5, 6]), (8, [6, 7])] {
        for side in sides {
            near(level(channels, side, 1_000.0), front + 1.492, 0.01);
        }
        for unity in 0..3 {
            near(level(channels, unity, 1_000.0), front, 1.0e-9);
        }
    }
    // The rear surrounds of 6.1 (BC) and 7.1 (BL, BR) sit past 120 degrees and weigh 1.0.
    for (channels, lit) in [(7, 4), (8, 4), (8, 5)] {
        near(level(channels, lit, 1_000.0), front, 1.0e-9);
    }
    for channels in [6, 7, 8] {
        assert_eq!(level(channels, 3, 60.0), None, "{channels} channels");
    }
    // The pipeline's own layouts for other counts (`SpeakerLayout::default_for`) weigh by speaker
    // too: their LFE does not count and their side surrounds weigh 1.41.
    for (channels, lfe) in [(3, 2), (9, 3), (10, 3), (12, 3)] {
        assert_eq!(level(channels, lfe, 60.0), None, "{channels} channels");
    }
    // 5.1.4 is FL FR FC LFE SL SR and four tops; 7.1.4 is 7.1 and four tops: the tops sit 30
    // degrees up and weigh 1.0.
    for (channels, sides, tops) in [(10, [4, 5], 6..10), (12, [6, 7], 8..12)] {
        for side in sides {
            near(level(channels, side, 1_000.0), front + 1.492, 0.01);
        }
        for top in tops {
            near(level(channels, top, 1_000.0), front, 1.0e-9);
        }
    }
    // 4.0 (FL FR FC BC) and a 9th, unnamed channel weigh 1.0.
    for (channels, lit) in [(4, 3), (9, 8)] {
        near(level(channels, lit, 1_000.0), front, 1.0e-9);
    }
}

#[test]
fn a_meter_refuses_a_shape_it_cannot_measure() {
    use super::LoudnessError::{InvalidShape, MismatchedBlock};
    assert_eq!(LoudnessMeter::new(48_000, 0).err(), Some(InvalidShape));
    for rate in [0, 7_999, 96_001] {
        assert_eq!(
            LoudnessMeter::new(rate, 2).err(),
            Some(InvalidShape),
            "{rate}"
        );
    }
    for rate in [8_000, 96_000] {
        assert!(LoudnessMeter::new(rate, 2).is_ok(), "{rate}");
    }
    for weights in [
        &[][..],
        &[1.0, -0.5][..],
        &[f64::NAN][..],
        &[1.0, f64::INFINITY][..],
    ] {
        assert_eq!(
            LoudnessMeter::with_weights(48_000, weights).err(),
            Some(InvalidShape),
            "{weights:?}"
        );
    }
    assert!(LoudnessMeter::with_weights(48_000, &[0.0, 2.0]).is_ok());
    let mut meter = LoudnessMeter::new(48_000, 2).expect("meter");
    let plane = [0.25_f32; 64];
    let short = [0.25_f32; 63];
    for planes in [
        &[][..],
        &[&plane[..]][..],
        &[&plane[..], &plane[..], &plane[..]][..],
        &[&plane[..], &short[..]][..],
    ] {
        assert_eq!(
            meter.push(planes),
            Err(MismatchedBlock),
            "{} planes",
            planes.len()
        );
    }
    assert_eq!(meter.report().frames, 0);
    meter.push(&[&plane, &plane]).expect("block");
    assert_eq!(meter.report().frames, 64);
    let mut muted = LoudnessMeter::with_weights(48_000, &[1.0, 0.0]).expect("meter");
    assert_eq!(one_channel(&mut muted, 2, 1, 1_000.0), None);
}

#[test]
fn true_peak_finds_the_peak_between_samples_at_96_khz() {
    // 96 kHz oversamples twice, not four times.
    // A 12 kHz sine (an eighth of the rate), 22.5 degrees out of phase with the samples: its
    // peak lies halfway between two, which every sample misses by 0.69 dB and the 2x
    // interpolator lands on.
    let amplitude = 0.5_f64;
    let signal = (0..96_000)
        .map(|index| {
            let phase =
                core::f64::consts::FRAC_PI_4 * f64::from(index) + core::f64::consts::FRAC_PI_8;
            #[allow(clippy::cast_possible_truncation)]
            let sample = (amplitude * libm::sin(phase)) as f32;
            sample
        })
        .collect::<Vec<_>>();
    let mut meter = LoudnessMeter::new(96_000, 1).expect("meter");
    assert_eq!(meter.phases.len(), 2);
    meter.push(&[&signal]).expect("block");
    let report = meter.report();
    let expected = 20.0 * libm::log10(amplitude);
    let missed = 20.0 * libm::log10(libm::sin(3.0 * core::f64::consts::FRAC_PI_8));
    near(report.sample_peak_dbfs, expected + missed, 0.01);
    // BS.1770-4 Annex 2 allows +0.2 / -0.4 dB.
    let peak = report.true_peak_dbtp.expect("a peak");
    assert!(
        peak <= expected + 0.2 && peak >= expected - 0.4,
        "{peak} against {expected}"
    );
}
