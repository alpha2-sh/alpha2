use super::*;

const RATE: u32 = 48_000;

/// Grain `k`'s position at `speed`: its centre, a hop after its start, reads where the map puts
/// that output sample.
fn at_speed(stretch: &Stretch, speed: f64) -> impl Fn(i64) -> i64 + use<> {
    let hop = stretch.hop();
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    move |grain| (((grain + 1) * hop) as f64 * speed).floor() as i64 - hop
}

fn tone(frequency: f64, seconds: f64) -> Vec<f32> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let count = (f64::from(RATE) * seconds) as usize;
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    (0..count)
        .map(|index| {
            (0.5 * libm::sin(std::f64::consts::TAU * frequency * index as f64 / f64::from(RATE)))
                as f32
        })
        .collect()
}

/// `length` output samples from `first` of `source` at `speed`, in one read.
fn stretched(source: &[f32], speed: f64, first: i64, length: usize) -> Vec<f32> {
    let mut reader = StretchReader::new(RATE);
    let position = at_speed(reader.stretch(), speed);
    let signal = StretchSignal {
        start: 0,
        samples: source,
    };
    let mut output = vec![0.0; length];
    reader.render(first, &mut [&mut output], position, &[signal], signal);
    output
}

/// Rising zero crossings per second over `samples`, by linear interpolation.
fn frequency(samples: &[f32]) -> f64 {
    let mut crossings = Vec::new();
    for (index, pair) in samples.windows(2).enumerate() {
        if pair[0] < 0.0 && pair[1] >= 0.0 {
            #[allow(clippy::cast_precision_loss)]
            crossings.push(index as f64 + f64::from(-pair[0] / (pair[1] - pair[0])));
        }
    }
    let (first, last) = (crossings[0], crossings[crossings.len() - 1]);
    #[allow(clippy::cast_precision_loss)]
    let cycles = (crossings.len() - 1) as f64;
    cycles * f64::from(RATE) / (last - first)
}

#[test]
fn a_stretched_tone_keeps_its_pitch() {
    for frequency_hz in [440.0, 3_000.0] {
        let source = tone(frequency_hz, 4.5);
        for speed in [0.5, 0.8, 1.25, 2.0] {
            // One second from half a second in, inside the source at every speed.
            let output = stretched(&source, speed, 24_000, 48_000);
            let measured = frequency(&output);
            let error = (measured - frequency_hz).abs() / frequency_hz;
            assert!(
                error < 0.003,
                "{frequency_hz} Hz at {speed}x measured {measured} Hz"
            );
        }
    }
}

#[test]
fn output_length_is_the_timeline_length() {
    // A clip shows its timeline length of output whatever its speed: one second of output
    // reads a second's worth of grain positions, the last reading where the map puts it.
    let source = tone(440.0, 3.0);
    for speed in [0.5, 2.0] {
        let output = stretched(&source, speed, 0, 48_000);
        assert_eq!(output.len(), 48_000);
        let tail = &output[40_000..];
        assert!(tail.iter().any(|sample| sample.abs() > 0.1), "{speed}x");
    }
}

#[test]
fn a_click_lands_where_the_map_puts_it() {
    let stretch = Stretch::new(RATE);
    for speed in [0.5, 0.8, 1.25, 2.0] {
        let mut source = vec![0.0; 192_000];
        let click = 60_000_u32;
        source[click as usize] = 1.0;
        let output = stretched(&source, speed, 0, 150_000);
        // Slowed down, the grains that overlap the click each carry a copy of it, spread
        // symmetrically by the window: their energy centroid is where it lands.
        let (mut weight, mut moment) = (0.0_f64, 0.0_f64);
        for (index, sample) in output.iter().enumerate() {
            let energy = f64::from(*sample) * f64::from(*sample);
            weight += energy;
            #[allow(clippy::cast_precision_loss)]
            {
                moment += energy * index as f64;
            }
        }
        let centroid = moment / weight;
        let expected = f64::from(click) / speed;
        #[allow(clippy::cast_precision_loss)]
        let tolerance = (stretch.tolerance() + 2) as f64;
        assert!(
            (centroid - expected).abs() <= tolerance,
            "{speed}x: at {centroid}, expected {expected}"
        );
    }
}

/// A small generator for repeatable block sizes and seeks.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % bound
    }
}

#[test]
fn blocks_and_seeks_give_the_same_samples() {
    // A chirp so every grain's search has something to find.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let source: Vec<f32> = (0..200_000_i32)
        .map(|index| {
            let t = f64::from(index) / f64::from(RATE);
            (0.4 * libm::sin(std::f64::consts::TAU * (200.0 * t + 300.0 * t * t))) as f32
        })
        .collect();
    let signal = StretchSignal {
        start: 0,
        samples: &source,
    };
    for speed in [0.75, 1.5] {
        let straight = stretched(&source, speed, 0, 120_000);
        let mut reader = StretchReader::new(RATE);
        let position = at_speed(reader.stretch(), speed);
        let mut random = Lcg(7);
        let mut at = 0_u64;
        for _ in 0..200 {
            if random.below(5) == 0 {
                at = random.below(110_000);
            }
            let length = 1 + random.below(4_000);
            let length = length.min(120_000 - at);
            if length == 0 {
                at = 0;
                continue;
            }
            let first = i64::try_from(at).expect("small");
            let mut output = vec![0.0; usize::try_from(length).expect("small")];
            reader.render(first, &mut [&mut output], &position, &[signal], signal);
            let from = usize::try_from(at).expect("small");
            for (index, (got, want)) in output.iter().zip(&straight[from..]).enumerate() {
                assert_eq!(
                    got.to_bits(),
                    want.to_bits(),
                    "{speed}x sample {}",
                    from + index
                );
            }
            at += length;
        }
    }
}

#[test]
fn segment_boundaries_do_not_click() {
    // A sine sweep: the largest step between samples near a segment boundary stays within half
    // again the largest inside segments.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let source: Vec<f32> = (0..400_000_i32)
        .map(|index| {
            let t = f64::from(index) / f64::from(RATE);
            (0.5 * libm::sin(std::f64::consts::TAU * (150.0 * t + 60.0 * t * t))) as f32
        })
        .collect();
    let stretch = Stretch::new(RATE);
    let segment = STRETCH_SEGMENT_GRAINS * stretch.hop();
    for speed in [0.75, 1.25, 2.0] {
        let output = stretched(&source, speed, 0, 180_000);
        let (mut near, mut inside) = (0.0_f32, 0.0_f32);
        for (index, pair) in output.windows(2).enumerate().skip(4_000) {
            let step = (pair[1] - pair[0]).abs();
            let sample = i64::try_from(index).expect("small");
            let from_boundary =
                (sample - (sample + segment / 2).div_euclid(segment) * segment).abs();
            if from_boundary <= stretch.hop() {
                near = near.max(step);
            } else {
                inside = inside.max(step);
            }
        }
        assert!(
            near <= 1.5 * inside,
            "{speed}x: {near} near, {inside} inside"
        );
    }
}

#[test]
fn silence_in_silence_out() {
    let output = stretched(&vec![0.0; 96_000], 1.5, 0, 48_000);
    assert!(output.iter().all(|&sample| sample.to_bits() == 0));
}

#[test]
fn nan_samples_are_dropped() {
    let mut source = tone(440.0, 2.0);
    for index in (1_000..60_000).step_by(997) {
        source[index] = f32::NAN;
    }
    source[30_000] = f32::INFINITY;
    let output = stretched(&source, 1.25, 0, 48_000);
    assert!(output.iter().all(|sample| sample.is_finite()));
}

#[test]
fn neighbouring_windows_sum_to_one() {
    let stretch = Stretch::new(RATE);
    let hop = usize::try_from(stretch.hop()).expect("small");
    for index in 0..hop {
        let sum = stretch.window[index] + stretch.window[index + hop];
        assert!((sum - 1.0).abs() < 1e-6, "{index}: {sum}");
    }
}
