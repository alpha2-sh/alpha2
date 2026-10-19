//! Held and incomplete blocks, and a held render that reads each source sample once.

use super::*;

/// A provider that has `source` only once it is told to.
struct Arriving {
    whole: Fixture,
    arrived: std::cell::Cell<bool>,
}

impl AudioBlockProvider for Arriving {
    fn block(&self, source: &SourceId, start: i64, frames: usize) -> Option<AudioSpan<'_>> {
        if !self.arrived.get() && source.as_str() == "late" {
            return None;
        }
        self.whole.block(source, start, frames)
    }
}

#[test]
fn a_held_incomplete_block_changes_nothing_and_its_retry_continues_exactly() {
    let seconds = MediaTime::from_seconds(1).ticks();
    let mut tone = audio_clip("c1", "tone", 0, seconds);
    tone.audio.effects = vec![alpha2_model::AudioEffect::Reverb {
        room_size: 0.8,
        damping: 0.3,
        mix: 0.5,
    }];
    let late = audio_clip("c2", "late", 0, seconds);
    let project = project(
        vec![track("a1", vec![tone]), track("a2", vec![late])],
        vec!["tone", "late"],
    );
    let snapshot = EngineProject::new(project).expect("valid project");
    let whole = || Fixture::new(vec![("tone", 0.25, 2), ("late", 0.125, 2)]);
    let continuous = render_all(&snapshot, &whole(), 2_048, 512);

    let provider = Arriving {
        whole: whole(),
        arrived: std::cell::Cell::new(true),
    };
    let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    renderer.set_hold_incomplete_blocks(true);
    renderer.set_loudness_metering(true).expect("meter");
    let mut held = Vec::new();
    for block in 0..4_i64 {
        if block == 2 {
            provider.arrived.set(false);
            let status = renderer
                .render(
                    &snapshot,
                    block * 512,
                    512,
                    &provider,
                    TransportState::Playing,
                )
                .expect("render");
            assert!(!status.is_complete());
            assert!(
                renderer.channel(0)[..512]
                    .iter()
                    .all(|sample| *sample == 0.0)
            );
            provider.arrived.set(true);
        }
        let status = renderer
            .render(
                &snapshot,
                block * 512,
                512,
                &provider,
                TransportState::Playing,
            )
            .expect("render");
        assert!(status.is_complete());
        held.extend_from_slice(&renderer.channel(0)[..512]);
    }
    assert_eq!(
        held.iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>(),
        continuous
            .iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>()
    );
    // Each block is metered once: the incomplete attempt is not.
    assert_eq!(renderer.loudness_report().expect("on").frames, 2_048);

    // Without holding, the incomplete block is mixed but still not metered.
    let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    renderer.set_loudness_metering(true).expect("meter");
    provider.arrived.set(false);
    let status = renderer
        .render(&snapshot, 0, 512, &provider, TransportState::Playing)
        .expect("render");
    assert!(!status.is_complete());
    assert!(
        renderer.channel(0)[..512]
            .iter()
            .any(|sample| *sample != 0.0)
    );
    assert_eq!(renderer.loudness_report().expect("on").frames, 0);
}

/// One source whose `data` starts `lead` samples into it, given either
/// as a dense block holding zeros before and after it, or as just `data`
/// saying the source starts and ends there.
struct Edges {
    block: AudioBlock,
}

impl Edges {
    fn new(data: &[f32], lead: usize, flagged: bool) -> Self {
        let (start, plane) = if flagged {
            (i64::try_from(lead).expect("small"), data.to_vec())
        } else {
            let mut plane = vec![0.0; lead];
            plane.extend_from_slice(data);
            plane.extend_from_slice(&[0.0; 4]);
            (0, plane)
        };
        Self {
            block: AudioBlock {
                start_sample: start,
                sample_rate: RATE,
                speakers: alpha2_media::SpeakerLayout::STEREO,
                channels: vec![plane.clone(), plane],
            },
        }
    }
}

impl AudioBlockProvider for Edges {
    fn block(&self, _: &SourceId, _: i64, _: usize) -> Option<AudioSpan<'_>> {
        Some(AudioSpan {
            block: &self.block,
            starts_source: true,
            ends_source: true,
        })
    }
}

#[test]
fn silence_beside_a_source_mixes_the_same_however_the_provider_gives_it() {
    // Off the sample grid, the edge samples must not depend on whether the silence is zeros
    // or a flag.
    #[allow(clippy::cast_precision_loss)]
    let data = (0..4_800)
        .map(|index| 0.5 + index as f32 / 20_000.0)
        .collect::<Vec<_>>();
    let lead = 2_400;
    let source_ticks = sample_ticks_floor(i64::try_from(lead + data.len()).expect("small"), RATE);
    // Starting at a 29.97 fps frame, 1601.6 samples in: every position is fractional.
    let mut clip = audio_clip("c1", "tone", 4_004, 4_004 + source_ticks);
    clip.source_range = TimeRange::new(MediaTime(0), MediaTime(source_ticks)).expect("range");
    let snapshot = rate_project(RATE, vec![clip]);
    let total = 8_960;
    let flagged = render_all(&snapshot, &Edges::new(&data, lead, true), total, 512);
    let dense = render_all(&snapshot, &Edges::new(&data, lead, false), total, 512);
    let differ = flagged
        .iter()
        .zip(&dense)
        .enumerate()
        .filter(|(_, (a, b))| a.to_bits() != b.to_bits())
        .map(|(index, (a, b))| (index, *a, *b))
        .collect::<Vec<_>>();
    assert_eq!(differ, Vec::<(usize, f32, f32)>::new());
}

/// Every way of reading a source: unity, retimed between samples,
/// reversed, and both loops, behind a clip reverb, so a seek replays
/// history.
fn every_read() -> EngineProject {
    let second = MediaTime::from_seconds(1).ticks();
    let period = second * 105 / 1_000;
    let clips = [
        ("c1", TimeMap::Forward, second),
        ("c2", TimeMap::Forward, second * 3 / 4),
        ("c3", TimeMap::Reverse, second),
        (
            "c4",
            TimeMap::Loop {
                mode: LoopMode::Repeat,
            },
            period,
        ),
        (
            "c5",
            TimeMap::Loop {
                mode: LoopMode::PingPong,
            },
            period,
        ),
    ];
    let tracks = clips
        .into_iter()
        .map(|(id, time_map, source_end)| {
            let mut clip = mapped_clip(time_map, second, source_end);
            clip.id = alpha2_model::ClipId::new(id.to_owned()).expect("id");
            if id == "c1" {
                clip.audio.effects = vec![alpha2_model::AudioEffect::Reverb {
                    room_size: 0.8,
                    damping: 0.3,
                    mix: 0.5,
                }];
            }
            track(&format!("t-{id}"), vec![clip])
        })
        .collect();
    EngineProject::new(project(tracks, vec!["tone"])).expect("valid project")
}

#[test]
fn a_held_render_reads_each_source_sample_once_and_mixes_the_same_bits() {
    // The supply check reads no sample, so a held render reads and mixes what an unheld one does.
    let snapshot = every_read();
    let plane = distinct_plane();
    let mixed = |hold: bool| {
        let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
        renderer.set_hold_incomplete_blocks(hold);
        let mut output = Vec::new();
        // Playback from the start, then a seek into the clips, which replays history.
        for start in [0, 512, 1_024, 1_536, 30_000, 30_512] {
            let status = renderer
                .render(&snapshot, start, 512, &plane, TransportState::Playing)
                .expect("render");
            assert!(status.is_complete());
            output.extend(
                renderer.channel(0)[..512]
                    .iter()
                    .map(|sample| sample.to_bits()),
            );
            output.extend(
                renderer.channel(1)[..512]
                    .iter()
                    .map(|sample| sample.to_bits()),
            );
        }
        (output, renderer.samples_read)
    };
    let (free, free_read) = mixed(false);
    let (held, held_read) = mixed(true);
    assert!(free_read > 0);
    assert_eq!(held_read, free_read, "source samples read");
    assert!(
        held == free,
        "a held render mixes what one that does not hold mixes"
    );
}

/// How [`Faulty`] fails to supply the source `late`.
#[derive(Clone, Copy, Debug)]
enum Fault {
    Absent,
    Uncovered,
    OtherRate,
    FewerPlanes,
}

/// A provider that supplies `late` with `fault`, and `tone` whole.
struct Faulty {
    whole: Fixture,
    broken: AudioBlock,
    fault: Fault,
}

impl Faulty {
    fn new(fault: Fault) -> Self {
        let mut broken = AudioBlock {
            start_sample: 0,
            sample_rate: RATE,
            speakers: SpeakerLayout::STEREO,
            channels: vec![vec![0.125; RATE as usize * 4]; 2],
        };
        match fault {
            Fault::Absent => {}
            Fault::Uncovered => broken.start_sample = 1_000_000,
            Fault::OtherRate => broken.sample_rate = RATE / 2,
            Fault::FewerPlanes => broken.channels.truncate(1),
        }
        Self {
            whole: Fixture::new(vec![("tone", 0.25, 2)]),
            broken,
            fault,
        }
    }
}

impl AudioBlockProvider for Faulty {
    fn block(&self, source: &SourceId, start: i64, frames: usize) -> Option<AudioSpan<'_>> {
        if source.as_str() != "late" {
            return self.whole.block(source, start, frames);
        }
        match self.fault {
            Fault::Absent => None,
            Fault::Uncovered => Some(AudioSpan {
                block: &self.broken,
                starts_source: false,
                ends_source: false,
            }),
            Fault::OtherRate | Fault::FewerPlanes => Some(AudioSpan::whole(&self.broken)),
        }
    }
}

#[test]
fn a_held_render_finds_missing_every_source_a_mix_would() {
    // The check reads no sample, so it must reject exactly the spans the mix rejects, through a
    // plain read and through a loop's.
    let second = MediaTime::from_seconds(1).ticks();
    let mut looped = audio_clip("c3", "late", 0, second);
    looped.source_range = TimeRange::new(MediaTime(0), MediaTime(second / 10)).expect("range");
    looped.time_map = TimeMap::Loop {
        mode: LoopMode::Repeat,
    };
    for late in [audio_clip("c2", "late", 0, second), looped] {
        let project = project(
            vec![
                track("a1", vec![audio_clip("c1", "tone", 0, second)]),
                track("a2", vec![late]),
            ],
            vec!["tone", "late"],
        );
        let snapshot = EngineProject::new(project).expect("valid project");
        for fault in [
            Fault::Absent,
            Fault::Uncovered,
            Fault::OtherRate,
            Fault::FewerPlanes,
        ] {
            let provider = Faulty::new(fault);
            let missing = |hold: bool| {
                let mut renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
                renderer.set_hold_incomplete_blocks(hold);
                let status = renderer
                    .render(&snapshot, 0, 512, &provider, TransportState::Playing)
                    .expect("render");
                let silent = renderer.channel(0)[..512]
                    .iter()
                    .all(|sample| *sample == 0.0);
                (status.missing_sources, silent)
            };
            let late = SourceId::new("late").expect("id");
            assert_eq!(missing(false), (vec![late.clone()], false), "{fault:?}");
            assert_eq!(missing(true), (vec![late], true), "{fault:?}");
        }
    }
}
