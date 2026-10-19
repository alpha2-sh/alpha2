//! Reversed, frozen, looped, and retimed clips.

use super::*;

#[test]
fn a_reversed_clip_plays_the_forward_samples_backwards_bit_for_bit() {
    let plane = distinct_plane();
    let second = MediaTime::from_seconds(1).ticks();
    // Untouched speed, and three quarters speed, where every position falls between source
    // samples.
    for source_end in [second, second * 3 / 4] {
        let forward = rate_project(
            RATE,
            vec![mapped_clip(TimeMap::Forward, second, source_end)],
        );
        let reverse = rate_project(
            RATE,
            vec![mapped_clip(TimeMap::Reverse, second, source_end)],
        );
        let total = RATE as usize;
        let forward = render_all(&forward, &plane, total, 512);
        for block in [512, 333] {
            let reversed = render_all(&reverse, &plane, total, block);
            for (index, sample) in reversed.iter().enumerate() {
                assert_eq!(
                    sample.to_bits(),
                    forward[total - 1 - index].to_bits(),
                    "source end {source_end}, block {block}, sample {index}"
                );
            }
        }
    }
}

#[test]
fn a_frozen_clip_is_silent_and_needs_no_media() {
    let second = MediaTime::from_seconds(1).ticks();
    let frozen = rate_project(
        RATE,
        vec![mapped_clip(
            TimeMap::Freeze {
                source_time: MediaTime(second / 2),
            },
            second,
            second,
        )],
    );
    let mut renderer = AudioRenderer::new(&frozen, 2, 512).expect("renderer");
    let status = renderer
        .render(
            &frozen,
            0,
            512,
            &Fixture::new(Vec::new()),
            TransportState::Playing,
        )
        .expect("render");
    assert_eq!(status.missing_sources, [] as [alpha2_media::SourceId; 0]);
    assert!(
        renderer.channel(0)[..512]
            .iter()
            .all(|sample| *sample == 0.0)
    );
}

#[test]
fn a_repeating_loop_fades_through_each_wrap_and_ping_pong_turns_back() {
    let plane = distinct_plane();
    // A tenth of a second, looped for 0.35 seconds, against the same tenth played once: the
    // mix path is the same, so a pass matches it bit for bit wherever no fade applies.
    let period = RATE as usize / 10;
    let (timeline_end, source_end) = (
        MediaTime::from_seconds(1).ticks() * 35 / 100,
        MediaTime::from_seconds(1).ticks() / 10,
    );
    let total = RATE as usize * 35 / 100;
    let once = rate_project(
        RATE,
        vec![mapped_clip(TimeMap::Forward, source_end, source_end)],
    );
    let once = render_all(&once, &plane, period, 512);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let fade = (f64::from(RATE) * alpha2_model::LOOP_FADE_SECONDS).round() as usize;
    let looped = |mode| {
        let project = rate_project(
            RATE,
            vec![mapped_clip(
                TimeMap::Loop { mode },
                timeline_end,
                source_end,
            )],
        );
        let output = render_all(&project, &plane, total, 512);
        assert_eq!(
            output,
            render_all(&project, &plane, total, 333),
            "block size"
        );
        output
    };
    let output = looped(LoopMode::Repeat);
    assert_eq!(output[..fade], once[..fade]);
    for wrap in [period, 2 * period, 3 * period] {
        assert_eq!(output[wrap + fade].to_bits(), once[fade].to_bits());
        assert_eq!(
            output[wrap - 1 - fade].to_bits(),
            once[period - 1 - fade].to_bits()
        );
        // Half a sample from it either side, the gain is 0.5 / fade.
        #[allow(clippy::cast_precision_loss)]
        let gain = 0.5 / fade as f32;
        let (before, after) = (output[wrap - 1], output[wrap]);
        assert!((before - once[period - 1] * gain).abs() < 1e-6, "{before}");
        assert!((after - once[0] * gain).abs() < 1e-6, "{after}");
    }

    // Even passes forward, odd ones backward, with no fade.
    let output = looped(LoopMode::PingPong);
    for offset in 0..period {
        assert_eq!(output[offset].to_bits(), once[offset].to_bits());
        assert_eq!(
            output[period + offset].to_bits(),
            once[period - 1 - offset].to_bits()
        );
        assert_eq!(
            output[2 * period + offset].to_bits(),
            once[offset].to_bits()
        );
    }
}

/// 8 192 samples from the middle of a clip `timeline` seconds long that
/// plays `source` seconds of a `frequency` Hz sine of amplitude 0.5.
fn retimed_tone(frequency: f64, source: i64, timeline: i64) -> Vec<f32> {
    let frames = RATE as usize * 4;
    let plane = (0..frames)
        .map(|index| {
            #[allow(clippy::cast_precision_loss)]
            let phase = std::f64::consts::TAU * frequency * index as f64 / f64::from(RATE);
            #[allow(clippy::cast_possible_truncation)]
            let sample = (libm::sin(phase) * 0.5) as f32;
            sample
        })
        .collect::<Vec<_>>();
    let tone = Plane(AudioBlock {
        start_sample: 0,
        sample_rate: RATE,
        speakers: SpeakerLayout::MONO,
        channels: vec![plane],
    });
    let length = MediaTime::from_seconds(1).ticks() * timeline;
    let mut clip = audio_clip("c1", "tone", 0, length);
    clip.source_range =
        TimeRange::new(MediaTime::ZERO, MediaTime::from_seconds(source)).expect("range");
    let snapshot = rate_project(RATE, vec![clip]);
    let mut renderer = AudioRenderer::new(&snapshot, 1, 8_192).expect("renderer");
    let start = i64::from(RATE) * timeline / 2 - 4_096;
    let status = renderer
        .render(&snapshot, start, 8_192, &tone, TransportState::Playing)
        .expect("render");
    assert!(status.is_complete());
    renderer.channel(0)[..8_192].to_vec()
}

/// The amplitude of the `frequency` Hz component of `samples`, through a
/// Hann window, in dB relative to 0.5.
fn component_db(samples: &[f32], frequency: f64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let length = samples.len() as f64;
    let (mut re, mut im, mut window_sum) = (0.0, 0.0, 0.0);
    for (index, sample) in samples.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let n = index as f64;
        let window = 0.5 - 0.5 * libm::cos(std::f64::consts::TAU * n / length);
        let phase = std::f64::consts::TAU * frequency * n / f64::from(RATE);
        re += f64::from(*sample) * window * libm::cos(phase);
        im += f64::from(*sample) * window * libm::sin(phase);
        window_sum += window;
    }
    let amplitude = 2.0 * libm::sqrt(re * re + im * im) / window_sum;
    20.0 * libm::log10(amplitude.max(1e-12) / 0.5)
}

fn rms_db(samples: &[f32]) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let mean = samples
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum::<f64>()
        / samples.len() as f64;
    // Relative to a full-level sine of amplitude 0.5.
    10.0 * libm::log10(mean.max(1e-24) / 0.125)
}

#[test]
fn a_retimed_clip_is_band_limited_at_every_speed() {
    // Twice as fast, 15 kHz becomes 30 kHz, above the Nyquist frequency: it must go, not fold
    // back to 18 kHz.
    let fast = retimed_tone(15_000.0, 2, 1);
    let alias = component_db(&fast, 18_000.0);
    let level = rms_db(&fast);
    println!("speed 2, 15 kHz: 18 kHz alias {alias:.1} dB, output {level:.1} dB");
    assert!(alias < -60.0, "18 kHz alias at {alias:.1} dB");
    assert!(level < -60.0, "output at {level:.1} dB");
    // What the speed keeps under the Nyquist frequency keeps its level: 5 kHz plays at 10.
    let kept = retimed_tone(5_000.0, 2, 1);
    let passband = component_db(&kept, 10_000.0);
    println!("speed 2, 5 kHz: 10 kHz at {passband:.3} dB");
    assert!(passband.abs() < 0.1, "10 kHz at {passband:.3} dB");
    // Half as fast, 15 kHz plays at 7.5 kHz, with no image of the source spectrum at 16.5.
    let slow = retimed_tone(15_000.0, 1, 2);
    let tone = component_db(&slow, 7_500.0);
    let image = component_db(&slow, 16_500.0);
    println!("speed 0.5, 15 kHz: 7.5 kHz at {tone:.3} dB, 16.5 kHz image {image:.1} dB");
    assert!(tone.abs() < 0.1, "7.5 kHz at {tone:.3} dB");
    assert!(image < -60.0, "16.5 kHz image at {image:.1} dB");
    // At 1.25, 15 kHz becomes 18.75 kHz, under the cutoff (0.97 of the Nyquist frequency).
    let near = retimed_tone(15_000.0, 5, 4);
    let at = component_db(&near, 18_750.0);
    println!("speed 1.25, 15 kHz: 18.75 kHz at {at:.3} dB");
    assert!(at.abs() < 0.1, "18.75 kHz at {at:.3} dB");
}

#[test]
fn a_retimed_clip_mixes_the_same_bits_in_any_block_size_and_after_a_seek() {
    let second = MediaTime::from_seconds(1).ticks();
    let mut clip = audio_clip("c1", "tone", 12_345, second);
    clip.source_range =
        TimeRange::new(MediaTime(33), MediaTime(33 + 2 * second - 7_001)).expect("range");
    clip.audio.fade_in = MediaTime(9_999);
    let snapshot = rate_project(RATE, vec![clip]);
    #[allow(clippy::cast_precision_loss)]
    let provider = Plane(AudioBlock {
        start_sample: 10,
        sample_rate: RATE,
        speakers: SpeakerLayout::MONO,
        channels: vec![
            (0..2 * RATE)
                .map(|index| libm::sinf(index as f32 * 0.01))
                .collect(),
        ],
    });
    let total = RATE as usize;
    let reference = render_all(&snapshot, &provider, total, 4_096);
    for block in [128, 37] {
        let output = render_all(&snapshot, &provider, total, block);
        let first_difference = output
            .iter()
            .zip(&reference)
            .position(|(left, right)| left.to_bits() != right.to_bits());
        assert_eq!(first_difference, None, "blocks of {block}");
    }
    let mut renderer = AudioRenderer::new(&snapshot, 2, 4_096).expect("renderer");
    for start in [30_000, 5_000] {
        renderer
            .render(&snapshot, start, 1_000, &provider, TransportState::Playing)
            .expect("render");
        let start = usize::try_from(start).expect("small");
        let first_difference = renderer.channel(0)[..1_000]
            .iter()
            .zip(&reference[start..])
            .position(|(left, right)| left.to_bits() != right.to_bits());
        assert_eq!(first_difference, None, "seek to {start}");
    }
}
