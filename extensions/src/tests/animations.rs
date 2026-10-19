//! Animated GIF, APNG, and WebP: frames, delays, loop counts, and where a rate source's frames start.

use super::*;

#[cfg(feature = "animated-image")]
#[test]
fn animations_match_an_independent_decoder_frame_for_frame() {
    // Pillow composited every frame of these files (tests/corpus/generate/extensions.py);
    // disposal, blending, and partial frames must land on the same pixels.
    let registry = Registry::with_defaults();
    for (name, format) in [
        ("animation.gif", Format::Gif),
        ("animation.png", Format::Apng),
        ("animation.webp", Format::Webp),
    ] {
        let file = bytes(name);
        let (provider, report) = registry
            .probe(&file)
            .expect("claimed")
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(provider, "animated-image");
        assert_eq!(report.format, format);
        assert_eq!((report.width, report.height), (48, 32), "{name}");
        // 100, 50, 200 ms as declared; the last, 0 ms, normalized to 100 and reported.
        assert_eq!(
            report.timing,
            Timing::Frames {
                durations: vec![12_000, 6_000, 24_000, 12_000]
            },
            "{name}"
        );
        assert_eq!(
            report.diagnostics,
            vec![Diagnostic::NormalizedDelay {
                frame: 3,
                declared_ms: 0
            }],
            "{name}"
        );
        assert_eq!(report.loop_count, Some(0), "{name}");
        let reference = std::fs::read(corpus(&format!("{name}.ref.rgba"))).expect("reference");
        let mut source = registry
            .open(
                &ProviderSource::AnimatedImage,
                Some(file),
                Arc::new(NoResources),
            )
            .expect("open");
        let frame_bytes = 48 * 32 * 4;
        // Out of order on purpose: a seek back re-decodes nothing wrong.
        for (index, at) in [(2, 150), (0, 0), (3, 350), (1, 100)] {
            let frame = source.render(ms(at), (0, 0)).expect("frame");
            assert_eq!(frame.index, index, "{name} at {at} ms");
            let mut expected = reference[index * frame_bytes..(index + 1) * frame_bytes].to_vec();
            premultiply(&mut expected);
            let wrong = frame
                .pixels
                .rgba8()
                .expect("rgba8")
                .as_chunks::<4>()
                .0
                .iter()
                .zip(expected.as_chunks::<4>().0.iter())
                .filter(|(a, b)| a != b)
                .count();
            assert_eq!(wrong, 0, "{name} frame {index}: {wrong} pixels differ");
        }
    }
}

#[test]
fn frames_answer_the_time_they_cover_and_hold_at_the_ends() {
    let timing = Timing::Frames {
        durations: vec![12_000, 6_000, 24_000],
    };
    let at = |t: i64| timing.frame_at(MediaTime::from_ticks(t));
    assert_eq!(at(-5).0, 0, "before the start, the first frame");
    assert_eq!(
        at(11_999),
        (
            0,
            MediaTime::from_ticks(i64::MIN),
            Some(MediaTime::from_ticks(12_000))
        )
    );
    assert_eq!(
        at(12_000),
        (
            1,
            MediaTime::from_ticks(12_000),
            Some(MediaTime::from_ticks(18_000))
        )
    );
    assert_eq!(at(18_000).0, 2);
    assert_eq!(
        at(1_000_000),
        (2, MediaTime::from_ticks(18_000), None),
        "past the end, the last"
    );
    assert_eq!(timing.duration(), Some(MediaTime::from_ticks(42_000)));

    // 24000/1001: frame 1 starts at 5005 ticks exactly; frame 3 at 15015.
    let rate = Timing::Rate {
        rate: FrameRate::new(24_000, 1_001).expect("rate"),
        frames: 4,
    };
    let at = |t: i64| rate.frame_at(MediaTime::from_ticks(t));
    assert_eq!(at(5_004).0, 0);
    assert_eq!(
        at(5_005),
        (
            1,
            MediaTime::from_ticks(5_005),
            Some(MediaTime::from_ticks(10_010))
        )
    );
    assert_eq!(at(15_015), (3, MediaTime::from_ticks(15_015), None));
    assert_eq!(at(99_999).0, 3);
    assert_eq!(rate.duration(), Some(MediaTime::from_ticks(20_020)));
}

/// The corpus APNG with every frame's delay set to `num`/`den` seconds.
#[cfg(feature = "animated-image")]
fn apng_with_delay(num: u16, den: u16) -> Arc<[u8]> {
    let mut file = bytes("animation.png").to_vec();
    let mut at = 8;
    while at + 8 <= file.len() {
        let length = u32::from_be_bytes(file[at..at + 4].try_into().expect("length")) as usize;
        if &file[at + 4..at + 8] == b"fcTL" {
            let data = at + 8;
            file[data + 20..data + 22].copy_from_slice(&num.to_be_bytes());
            file[data + 22..data + 24].copy_from_slice(&den.to_be_bytes());
            let crc = crc32(&file[at + 4..data + length]);
            file[data + length..data + length + 4].copy_from_slice(&crc.to_be_bytes());
        }
        at += 12 + length;
    }
    file.into()
}

#[cfg(feature = "animated-image")]
#[test]
fn an_apng_frame_lasts_its_rational_delay_to_the_tick() {
    let registry = Registry::with_defaults();
    let durations = |file: &[u8]| {
        let (_, report) = registry.probe(file).expect("claimed").expect("probe");
        let Timing::Frames { durations } = report.timing else {
            panic!("an animation's timing is its frames'");
        };
        (durations, report.diagnostics)
    };
    // 120 000 ticks a second hold 1/30, 1/24, and NTSC's 1001/30000 exactly.
    for (num, den, ticks) in [
        (1, 30, 4_000),
        (1, 24, 5_000),
        (1_001, 30_000, 4_004),
        (1, 7, 17_143),
    ] {
        assert_eq!(
            durations(&apng_with_delay(num, den)),
            (vec![ticks; 4], Vec::new()),
            "{num}/{den} s"
        );
    }
    // 10.4 ms is longer than the longest delay normalized, whatever it rounds to in
    // milliseconds.
    assert_eq!(
        durations(&apng_with_delay(13, 1_250)),
        (vec![1_248; 4], Vec::new()),
        "13/1250 s"
    );
}

/// The corpus animated WebP with every frame's duration set to `ms` milliseconds.
#[cfg(feature = "animated-image")]
fn webp_with_delay(ms: u32) -> Arc<[u8]> {
    let mut file = bytes("animation.webp").to_vec();
    let mut at = 12;
    while at + 8 <= file.len() {
        let length = u32::from_le_bytes(file[at + 4..at + 8].try_into().expect("length")) as usize;
        if &file[at..at + 4] == b"ANMF" {
            file[at + 8 + 12..at + 8 + 15].copy_from_slice(&ms.to_le_bytes()[..3]);
        }
        at += 8 + length + length % 2;
    }
    file.into()
}

/// A two-frame GIF whose frames both last `centiseconds`.
#[cfg(feature = "animated-image")]
fn gif_with_delay(centiseconds: u16) -> Vec<u8> {
    let mut file = Vec::new();
    {
        let mut encoder = gif::Encoder::new(&mut file, 2, 2, &[]).expect("encoder");
        for shade in [0_u8, 255] {
            let mut pixels = [shade, shade, shade, 255].repeat(4);
            let mut frame = gif::Frame::from_rgba_speed(2, 2, &mut pixels, 30);
            frame.delay = centiseconds;
            encoder.write_frame(&frame).expect("frame");
        }
    }
    file
}

#[cfg(feature = "animated-image")]
#[test]
fn a_delay_of_10_ms_or_less_shows_for_100_ms_and_a_longer_one_as_declared() {
    let registry = Registry::with_defaults();
    let probe = |file: &[u8]| {
        let (_, report) = registry.probe(file).expect("claimed").expect("probe");
        let Timing::Frames { durations } = report.timing else {
            panic!("an animation's timing is its frames'");
        };
        (durations, report.diagnostics)
    };
    let normalized = |count: usize, declared_ms: u32| {
        (
            vec![12_000; count],
            (0..count)
                .map(|frame| Diagnostic::NormalizedDelay { frame, declared_ms })
                .collect::<Vec<_>>(),
        )
    };
    let kept = |count: usize, ticks: i64| (vec![ticks; count], Vec::new());
    // WebP declares whole milliseconds; APNG a fraction of a second. 0 to 10 ms is normalized,
    // and 11 ms and up (16 and 17 ms are 60 fps) plays as declared.
    for ms in [0, 1, 10] {
        assert_eq!(
            probe(&webp_with_delay(ms)),
            normalized(4, ms),
            "WebP {ms} ms"
        );
        assert_eq!(
            probe(&apng_with_delay(u16::try_from(ms).expect("small"), 1_000)),
            normalized(4, ms),
            "APNG {ms} ms"
        );
    }
    assert_eq!(
        probe(&apng_with_delay(1, 100)),
        normalized(4, 10),
        "1/100 s"
    );
    for ms in [11, 16, 17, 19] {
        let ticks = i64::from(ms) * 120;
        assert_eq!(probe(&webp_with_delay(ms)), kept(4, ticks), "WebP {ms} ms");
        assert_eq!(
            probe(&apng_with_delay(u16::try_from(ms).expect("small"), 1_000)),
            kept(4, ticks),
            "APNG {ms} ms"
        );
    }
    assert_eq!(probe(&apng_with_delay(1, 60)), kept(4, 2_000), "1/60 s");
    // A GIF counts centiseconds: 0 and 1 are normalized, 2 (20 ms) and up kept.
    assert_eq!(probe(&gif_with_delay(0)), normalized(2, 0), "GIF 0 cs");
    assert_eq!(probe(&gif_with_delay(1)), normalized(2, 10), "GIF 1 cs");
    assert_eq!(probe(&gif_with_delay(2)), kept(2, 2_400), "GIF 2 cs");
}

#[cfg(feature = "animated-image")]
#[test]
fn an_animations_loop_count_is_how_many_times_it_plays() {
    let registry = Registry::with_defaults();
    let loops = |file: &[u8]| {
        let (_, report) = registry.probe(file).expect("claimed").expect("probe");
        report.loop_count
    };
    // A GIF's loop extension counts repetitions after the first play; without one it plays
    // once.
    let gif = |repeat: Option<gif::Repeat>| {
        let mut file = Vec::new();
        {
            let mut encoder = gif::Encoder::new(&mut file, 2, 2, &[]).expect("encoder");
            if let Some(repeat) = repeat {
                encoder.set_repeat(repeat).expect("repeat");
            }
            for shade in [0_u8, 255] {
                let mut pixels = [shade, shade, shade, 255].repeat(4);
                let mut frame = gif::Frame::from_rgba_speed(2, 2, &mut pixels, 30);
                frame.delay = 10;
                encoder.write_frame(&frame).expect("frame");
            }
        }
        file
    };
    assert_eq!(loops(&gif(None)), Some(1), "no loop extension");
    assert_eq!(loops(&gif(Some(gif::Repeat::Finite(2)))), Some(3));
    assert_eq!(loops(&gif(Some(gif::Repeat::Infinite))), Some(0));
    // APNG and WebP count plays: the corpus files loop for ever.
    assert_eq!(loops(&bytes("animation.png")), Some(0));
    assert_eq!(loops(&bytes("animation.webp")), Some(0));
    // A WebP without its animation header does not say.
    let mut webp = bytes("animation.webp").to_vec();
    let anim = webp
        .windows(4)
        .position(|kind| kind == b"ANIM")
        .expect("ANIM chunk");
    webp[anim..anim + 4].copy_from_slice(b"XNIM");
    assert_eq!(loops(&webp), None);
}

#[test]
fn a_rate_sources_frames_start_where_the_engine_samples_them() {
    // The engine evaluates output frame k at FrameRate::frame_start(k), the floored tick; a source
    // at the same rate shows frame k there, whether or not a frame lasts a whole number of ticks.
    for (num, den) in [(18, 1), (2_997, 125), (24_000, 1_001), (7, 1)] {
        let rate = FrameRate::new(num, den).expect("rate");
        let frames = 40_usize;
        let at = |k: usize| rate.frame_start(i64::try_from(k).expect("small"));
        let timing = Timing::Rate { rate, frames };
        for k in 0..frames {
            let start = at(k).expect("start");
            let (index, first, last) = timing.frame_at(start);
            assert_eq!(index, k, "{num}/{den}: frame {k}");
            if k > 0 {
                assert_eq!(first, start, "{num}/{den}: frame {k} starts");
                assert_eq!(
                    timing.frame_at(MediaTime::from_ticks(start.ticks() - 1)).0,
                    k - 1
                );
            }
            assert_eq!(
                last,
                (k + 1 < frames).then(|| at(k + 1).expect("end")),
                "{num}/{den}: frame {k} ends"
            );
        }
        assert_eq!(timing.duration(), at(frames), "{num}/{den}");
    }
}

#[cfg(feature = "animated-image")]
#[test]
fn a_lossy_animated_webp_matches_libwebp_frame_for_frame() {
    // tests/corpus/generate/extensions.py: lossy pictures, with alpha planes, over and in place
    // of the canvas, the second disposed to the background; Pillow's (libwebp's) compositing is
    // the reference.
    let file = bytes("lossy.webp");
    let chunks = |kind: &[u8]| file.windows(4).filter(|window| *window == kind).count();
    assert_eq!((chunks(b"ANMF"), chunks(b"VP8L")), (4, 0), "lossy frames");
    assert!(chunks(b"ALPH") >= 3, "frames with alpha planes");
    let registry = Registry::with_defaults();
    let (_, report) = registry.probe(&file).expect("claimed").expect("probe");
    assert_eq!(report.format, Format::Webp);
    assert_eq!(
        report.timing,
        Timing::Frames {
            durations: vec![12_000; 4]
        }
    );
    let reference = std::fs::read(corpus("lossy.webp.ref.rgba")).expect("reference");
    let mut source = registry
        .open(
            &ProviderSource::AnimatedImage,
            Some(file),
            Arc::new(NoResources),
        )
        .expect("open");
    let frame_bytes = 48 * 32 * 4;
    for (index, at) in [(0, 0), (1, 100), (2, 200), (3, 300)] {
        let frame = source.render(ms(at), (0, 0)).expect("frame");
        let mut expected = reference[index * frame_bytes..][..frame_bytes].to_vec();
        premultiply(&mut expected);
        let wrong = frame
            .pixels
            .rgba8()
            .expect("rgba8")
            .as_chunks::<4>()
            .0
            .iter()
            .zip(expected.as_chunks::<4>().0.iter())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(wrong, 0, "frame {index}: {wrong} pixels differ");
    }
}
