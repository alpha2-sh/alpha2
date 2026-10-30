//! Clips that keep their pitch at a speed other than 1.

use super::*;

/// A 440 Hz sine, `seconds` long, mono.
fn sine(seconds: usize) -> Plane {
    Plane(AudioBlock {
        start_sample: 0,
        sample_rate: RATE,
        speakers: SpeakerLayout::MONO,
        channels: vec![
            (0..RATE as usize * seconds)
                .map(|index| {
                    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                    let value = (0.5
                        * libm::sin(std::f64::consts::TAU * 440.0 * index as f64 / f64::from(RATE)))
                        as f32;
                    value
                })
                .collect(),
        ],
    })
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

/// One second of timeline over `source_end` ticks of source, keeping its pitch or not.
fn kept(time_map: TimeMap, source_end: i64, preserve_pitch: bool) -> EngineProject {
    let mut clip = mapped_clip(time_map, TICKS_PER_SECOND, source_end);
    clip.audio.preserve_pitch = preserve_pitch;
    rate_project(RATE, vec![clip])
}

#[test]
fn preserve_pitch_at_unit_speed_is_the_plain_read() {
    let plane = distinct_plane();
    let total = RATE as usize;
    for time_map in [TimeMap::Forward, TimeMap::Reverse] {
        let plain = render_all(&kept(time_map, TICKS_PER_SECOND, false), &plane, total, 512);
        let flagged = render_all(&kept(time_map, TICKS_PER_SECOND, true), &plane, total, 512);
        assert!(
            plain
                .iter()
                .zip(&flagged)
                .all(|(a, b)| a.to_bits() == b.to_bits()),
            "{time_map:?}"
        );
    }
}

#[test]
fn a_preserved_clip_keeps_its_pitch_where_the_plain_read_raises_it() {
    let source = sine(3);
    let total = RATE as usize;
    for time_map in [TimeMap::Forward, TimeMap::Reverse] {
        let plain = render_all(
            &kept(time_map, TICKS_PER_SECOND * 3 / 2, false),
            &source,
            total,
            512,
        );
        let stretched = render_all(
            &kept(time_map, TICKS_PER_SECOND * 3 / 2, true),
            &source,
            total,
            512,
        );
        let middle = total / 8..total * 7 / 8;
        let raised = frequency(&plain[middle.clone()]);
        let held = frequency(&stretched[middle]);
        assert!(
            (raised - 660.0).abs() < 2.0,
            "{time_map:?}: plain {raised} Hz"
        );
        assert!(
            (held - 440.0).abs() < 1.5,
            "{time_map:?}: stretched {held} Hz"
        );
    }
}

#[test]
fn a_preserved_clip_keeps_video_alignment() {
    // A click at 1.2 s of source in a clip playing 1.5x lands at 0.8 s of timeline, where video
    // shows that source frame: within the stretch's search tolerance (10 ms).
    let click = RATE as usize * 6 / 5;
    let source = Plane(AudioBlock {
        start_sample: 0,
        sample_rate: RATE,
        speakers: SpeakerLayout::MONO,
        channels: vec![impulse_at(click, RATE as usize * 3).0.channels[0].clone()],
    });
    let output = render_all(
        &kept(TimeMap::Forward, TICKS_PER_SECOND * 3 / 2, true),
        &source,
        RATE as usize,
        512,
    );
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
    #[allow(clippy::cast_precision_loss)]
    let expected = click as f64 / 1.5;
    assert!(
        (centroid - expected).abs() <= f64::from(RATE) * 0.010 + 2.0,
        "at {centroid}, expected {expected}"
    );
}

#[test]
fn blocks_and_seeks_give_a_preserved_clip_the_same_samples() {
    let plane = distinct_plane();
    let total = RATE as usize;
    for time_map in [TimeMap::Forward, TimeMap::Reverse] {
        let snapshot = kept(time_map, TICKS_PER_SECOND * 5 / 4, true);
        let straight = render_all(&snapshot, &plane, total, 512);
        let blocked = render_all(&snapshot, &plane, total, 333);
        assert!(
            straight
                .iter()
                .zip(&blocked)
                .all(|(a, b)| a.to_bits() == b.to_bits()),
            "{time_map:?}"
        );
        // A seek into the middle reads what playing through reached.
        let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
        let at = RATE as usize * 5 / 8;
        renderer
            .render(
                &snapshot,
                i64::try_from(at).expect("small"),
                512,
                &plane,
                TransportState::Playing,
            )
            .expect("render");
        assert!(
            renderer.channel(0)[..512]
                .iter()
                .zip(&straight[at..])
                .all(|(a, b)| a.to_bits() == b.to_bits()),
            "{time_map:?} seek"
        );
    }
}

/// Supplies `plane` only for reads inside `needs`.
struct Within<'a> {
    plane: &'a Plane,
    needs: Vec<AudioNeed>,
}

impl AudioBlockProvider for Within<'_> {
    fn block(&self, source: &SourceId, start_sample: i64, frames: usize) -> Option<AudioSpan<'_>> {
        let end = start_sample + i64::try_from(frames).ok()?;
        let inside = self.needs.iter().any(|need| {
            need.source_id == *source
                && sample_floor(need.source_range.start().ticks(), RATE) <= start_sample
                && sample_floor(need.source_range.end().ticks(), RATE) + 1 >= end
        });
        inside.then(|| AudioSpan::whole(&self.plane.0))
    }
}

#[test]
fn needs_cover_a_preserved_clip_read() {
    let plane = distinct_plane();
    for time_map in [TimeMap::Forward, TimeMap::Reverse] {
        for speed_end in [TICKS_PER_SECOND / 2, TICKS_PER_SECOND * 2] {
            let snapshot = kept(time_map, speed_end, true);
            for start in [0, 7_000, 30_000] {
                let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
                let needs = renderer.audio_needs(&snapshot, start, 512).expect("needs");
                let within = Within {
                    plane: &plane,
                    needs,
                };
                let status = renderer
                    .render(&snapshot, start, 512, &within, TransportState::Playing)
                    .expect("render");
                assert!(
                    status.is_complete(),
                    "{time_map:?}, source {speed_end}, from {start}"
                );
            }
        }
    }
}

#[test]
fn stretched_clips_count_against_the_effect_budget() {
    let clip = |preserve_pitch, source_end| {
        let mut clip = mapped_clip(TimeMap::Forward, TICKS_PER_SECOND, source_end);
        clip.audio.preserve_pitch = preserve_pitch;
        clip
    };
    let load = |clip| project(vec![track("a1", vec![clip])], vec!["tone"]).audio_effect_load();
    assert_eq!(load(clip(true, TICKS_PER_SECOND * 2)).effects, 1);
    assert_eq!(load(clip(true, TICKS_PER_SECOND * 2)).delay_ticks, 0);
    // At unit speed or without the flag, the clip reads as before and holds nothing.
    assert_eq!(load(clip(true, TICKS_PER_SECOND)).effects, 0);
    assert_eq!(load(clip(false, TICKS_PER_SECOND * 2)).effects, 0);
}
