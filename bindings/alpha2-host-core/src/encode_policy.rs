//! Encoder settings every native backend applies the same way.

use alpha2_media::EncodeError;

/// Frames between forced keyframes: two seconds of output.
///
/// libavcodec's default of 12 is half a second at 24 fps. Two seconds keeps nearly all of the
/// quality gain of longer intervals (about 0.5-0.8 dB PSNR-Y over 12 on hardware H.264) while a
/// player can still seek within two seconds.
#[must_use]
pub fn keyframe_interval(frame_rate: alpha2_time::FrameRate) -> i32 {
    let frames = (2 * u64::from(frame_rate.numerator()))
        .div_ceil(u64::from(frame_rate.denominator()))
        .max(1);
    i32::try_from(frames).unwrap_or(i32::MAX)
}

/// The time base for frames at `rate`: the frame length (1001/30000 for 29.97 fps) when its
/// denominator is within `limit`, else 1/D for the D within `limit` with the smallest worst-case
/// rounding of frame times. 119.88 fps becomes 1/64000, off by at most 7/960000 s.
///
/// # Errors
/// For a rate whose terms do not fit a 32-bit rational.
pub fn encoder_time_base(
    rate: alpha2_time::FrameRate,
    limit: Option<u32>,
) -> Result<(i32, i32), EncodeError> {
    let out_of_range = || EncodeError::Adapter("frame rate is out of range".into());
    let (num, den) = (u64::from(rate.numerator()), u64::from(rate.denominator()));
    let exact = || {
        Ok((
            i32::try_from(den).map_err(|_| out_of_range())?,
            i32::try_from(num).map_err(|_| out_of_range())?,
        ))
    };
    let Some(limit) = limit.map(u64::from).filter(|limit| num > *limit) else {
        return exact();
    };
    // Frame k is at k*den/num s, k*den*D/num units of 1/D. Its fractional part is a multiple of
    // 1/q, q = num / gcd(num, den*D), so the largest rounding is floor(q/2)/q units. A unit no
    // longer than a frame keeps every frame its own time.
    let mut best: Option<(u64, u64, u64)> = None;
    for units in num.div_ceil(den).max(1)..=limit {
        let q = num / gcd_u64(num, den * units);
        let (error, scale) = (q / 2, q * units);
        if best.is_none_or(|(_, best_error, best_scale)| {
            u128::from(error) * u128::from(best_scale) < u128::from(best_error) * u128::from(scale)
        }) {
            best = Some((units, error, scale));
        }
    }
    let (units, _, _) = best.ok_or_else(out_of_range)?;
    Ok((1, i32::try_from(units).map_err(|_| out_of_range())?))
}

const fn gcd_u64(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    use alpha2_time::FrameRate;

    #[test]
    fn keyframes_are_two_seconds_apart_at_any_rate() {
        let interval = |num, den| super::keyframe_interval(FrameRate::new(num, den).expect("rate"));
        assert_eq!(interval(24, 1), 48);
        assert_eq!(interval(24_000, 1001), 48);
        assert_eq!(interval(30_000, 1001), 60);
        assert_eq!(interval(60, 1), 120);
        assert_eq!(interval(1, 1), 2);
        assert_eq!(interval(1, 4), 1);
    }

    #[test]
    fn an_encoder_time_base_is_the_frame_length_or_the_finest_unit_the_codec_holds() {
        // MPEG-4 Part 2 stores the time base denominator in 16 bits.
        let limit = Some(65_535);
        let base = |num, den, limit| {
            super::encoder_time_base(FrameRate::new(num, den).expect("rate"), limit)
                .expect("time base")
        };
        for (num, den) in [(30_000, 1_001), (60_000, 1_001), (25, 1), (65_535, 1)] {
            let exact = (i32::try_from(den).unwrap(), i32::try_from(num).unwrap());
            assert_eq!(base(num, den, limit), exact, "{num}/{den}");
        }
        assert_eq!(base(120_000, 1_001, None), (1_001, 120_000));
        // 1/64000 rounds 119.88 fps frame times by at most 7/960000 s, finer than 1/60000 or 1/65535.
        for (num, den, units) in [
            (120_000, 1_001, 64_000),
            (240_000, 1_001, 64_000),
            (360_000, 34_913, 65_535),
            (176_000, 18_133, 64_000),
        ] {
            assert_eq!(base(num, den, limit), (1, units), "{num}/{den}");
            let mut last = -1_i64;
            for frame in 0..10_000_i64 {
                let exact = frame * i64::from(den) * i64::from(units) / i64::from(num);
                let remainder = frame * i64::from(den) * i64::from(units) % i64::from(num);
                let rounded = exact + i64::from(remainder * 2 >= i64::from(num));
                assert!(rounded > last, "{num}/{den} frame {frame}");
                last = rounded;
            }
        }
    }
}
