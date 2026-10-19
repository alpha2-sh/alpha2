//! Buses: which buses a block reaches, idle buses, solo with ducking, and ducking chains.

use super::*;

/// Samples every bus of `renderer` holds a buffer for.
fn bus_buffer_samples(renderer: &AudioRenderer) -> usize {
    renderer
        .buses
        .iter()
        .flat_map(|bus| {
            let ducked = bus.ducker.as_ref().map(|ducking| &ducking.output);
            bus.output.iter().chain(ducked.into_iter().flatten())
        })
        .map(Vec::len)
        .sum()
}

#[test]
fn only_a_bus_that_ducks_holds_a_second_block() {
    let seconds = MediaTime::from_seconds(1).ticks();
    let mut project = project(
        (0..8)
            .map(|index| {
                let id = format!("t{index}");
                track(
                    &id,
                    vec![audio_clip(&format!("c{index}"), "tone", 0, seconds)],
                )
            })
            .collect(),
        vec!["tone"],
    );
    duck(&mut project, 0, "t1");
    let snapshot = EngineProject::new(project).expect("valid project");
    let renderer = AudioRenderer::new(&snapshot, 2, 4_096).expect("renderer");
    // Eight buses of two channels, and the ducked block of the one that ducks.
    assert_eq!(bus_buffer_samples(&renderer), (8 + 1) * 2 * 4_096);
}

#[test]
fn a_block_clears_and_sums_only_the_buses_it_reaches() {
    // Sixteen tracks: four sound now, eleven only later, and one runs an EQ throughout, so its
    // chain writes its bus in every block.
    let second = MediaTime::from_seconds(1).ticks();
    let tracks = (0..16)
        .map(|index| {
            let id = format!("t{index}");
            let (start, end) = if index < 4 {
                (0, second)
            } else {
                (2 * second, 3 * second)
            };
            track(
                &id,
                vec![audio_clip(&format!("c{index}"), "tone", start, end)],
            )
        })
        .collect();
    let mut project = project(tracks, vec!["tone"]);
    project.timeline.tracks[15]
        .audio
        .effects
        .push(AudioEffect::Eq {
            bands: vec![alpha2_model::EqBand {
                kind: alpha2_model::EqBandKind::Peaking,
                frequency_hz: 1_000.0,
                q: 0.7,
                gain_db: 3.0,
            }],
        });
    let snapshot = EngineProject::new(project).expect("valid project");
    let fixture = Fixture::new(vec![("tone", 0.5, 2)]);
    let mut renderer = AudioRenderer::new(&snapshot, 2, 4_096).expect("renderer");
    let (cleared, summed) = (BUS_PLANES_CLEARED.get(), BUS_PLANES_SUMMED.get());
    renderer
        .render(&snapshot, 0, 4_096, &fixture, TransportState::Playing)
        .expect("render");
    let cleared = BUS_PLANES_CLEARED.get() - cleared;
    let summed = BUS_PLANES_SUMMED.get() - summed;
    // Four sounding buses and the EQ's, two channels each.
    assert_eq!((cleared, summed), (5 * 2, 5 * 2));
}

#[test]
fn idle_buses_mix_as_buses_of_silence() {
    // Buses fall idle while a stage still reads them (a key that stops early, a reverb ringing on);
    // leaving the gaps must mix the same bits as filling them with clips of silence.
    let ms = |value: i64| value * TICKS_PER_SECOND / 1_000;
    let sounding = [
        ("music", "tone", ms(0), ms(600)),
        ("voice", "voice", ms(100), ms(300)),
        ("pad", "tone", ms(0), ms(50)),
        ("room", "tone", ms(100), ms(150)),
    ];
    let end = ms(700);
    let gappy = |filled: bool| {
        let tracks = sounding
            .iter()
            .map(|&(id, source, start, stop)| {
                let mut clips = vec![audio_clip(&format!("{id}-sound"), source, start, stop)];
                if filled {
                    if start > 0 {
                        clips.push(audio_clip(&format!("{id}-before"), "silence", 0, start));
                    }
                    clips.push(audio_clip(&format!("{id}-after"), "silence", stop, end));
                }
                track(id, clips)
            })
            .collect();
        let mut project = project(tracks, vec!["tone", "voice", "silence"]);
        duck(&mut project, 0, "voice");
        duck(&mut project, 2, "voice");
        project.timeline.tracks[3]
            .audio
            .effects
            .push(AudioEffect::Reverb {
                room_size: 0.8,
                damping: 0.3,
                mix: 0.5,
            });
        if !filled {
            for index in 0..8 {
                project
                    .timeline
                    .tracks
                    .push(track(&format!("empty{index}"), Vec::new()));
            }
        }
        project
    };
    let fixture = Fixture::new(vec![
        ("tone", 0.5, 2),
        ("voice", 0.9, 2),
        ("silence", 0.0, 2),
    ]);
    let mix = |project: Project, block: usize| {
        let snapshot = EngineProject::new(project).expect("valid project");
        let mut renderer = AudioRenderer::new(&snapshot, 2, block).expect("renderer");
        let mut bits = Vec::new();
        for start in (0..RATE as usize * 7 / 10).step_by(block) {
            renderer
                .render(
                    &snapshot,
                    i64::try_from(start).expect("small"),
                    block,
                    &fixture,
                    TransportState::Playing,
                )
                .expect("render");
            for channel in 0..2 {
                bits.extend(
                    renderer
                        .channel(channel)
                        .iter()
                        .map(|sample| sample.to_bits()),
                );
            }
        }
        bits
    };
    for block in [4_096, 512] {
        let filled = mix(gappy(true), block);
        assert!(filled.iter().any(|&bits| bits != 0), "the mix sounds");
        assert!(mix(gappy(false), block) == filled, "block {block}");
    }
}

#[test]
fn a_soloed_ducked_track_sounds_as_it_does_in_the_whole_mix() {
    let seconds = MediaTime::from_seconds(1).ticks();
    let mut base = project(
        vec![
            track("music", vec![audio_clip("c1", "tone", 0, seconds)]),
            track("voice", vec![audio_clip("c2", "voice", 0, seconds)]),
        ],
        vec!["tone", "voice"],
    );
    base.timeline.tracks[0].audio.ducking = Some(Ducking {
        sidechain_track: TrackId::new("voice".to_owned()).expect("id"),
        threshold: 0.05,
        depth_db: -24.0,
        attack_ms: 1.0,
        release_ms: 50.0,
    });
    let loud = Fixture::new(vec![("tone", 0.5, 2), ("voice", 0.9, 2)]);
    let bits = |project: Project| {
        let (renderer, _) = render(project, 512, &loud);
        renderer.channel(0)[..512]
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>()
    };
    // The whole mix with the voice made inaudible but still keying the ducker: the music as
    // the mix ducks it.
    let mut keyed = base.clone();
    keyed.timeline.tracks[1].audio.gain = Animated::Constant { value: 0.0 };
    let mut soloed = base.clone();
    soloed.timeline.tracks[0].audio.solo = true;
    let (soloed_bits, keyed_bits) = (bits(soloed.clone()), bits(keyed));
    assert!(
        soloed_bits == keyed_bits,
        "soloed music reads {} at the block's end, ducked in the whole mix {}",
        f32::from_bits(soloed_bits[511]),
        f32::from_bits(keyed_bits[511]),
    );
    let mut unducked = base.clone();
    unducked.timeline.tracks[0].audio.ducking = None;
    unducked.timeline.tracks[1].audio.muted = true;
    let unducked = bits(unducked);
    assert_ne!(bits(soloed.clone()), unducked);
    // The key is read for the solo, so a host is asked for it.
    let snapshot = EngineProject::new(soloed.clone()).expect("project");
    let renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    let needs = renderer.audio_needs(&snapshot, 0, 512).expect("needs");
    let mut sources: Vec<&str> = needs.iter().map(|need| need.source_id.as_str()).collect();
    sources.sort_unstable();
    assert_eq!(sources, ["tone", "voice"]);
    // A muted track is not there: it keys nothing, soloed or not.
    let mut muted_key = soloed;
    muted_key.timeline.tracks[1].audio.muted = true;
    assert_eq!(bits(muted_key.clone()), unducked);
    let snapshot = EngineProject::new(muted_key).expect("project");
    let renderer = AudioRenderer::new(&snapshot, 2, 512).expect("renderer");
    let needs = renderer.audio_needs(&snapshot, 0, 512).expect("needs");
    assert_eq!(needs.len(), 1, "only the music is read");
    let mut muted_whole = base;
    muted_whole.timeline.tracks[1].audio.muted = true;
    assert_eq!(bits(muted_whole), unducked);
}

#[test]
fn the_model_accepts_only_effect_values_the_dsp_renders_as_written() {
    let same = |model: f32, dsp: f32| {
        assert_eq!(model.to_bits(), dsp.to_bits(), "{model} against {dsp}");
    };
    same(
        alpha2_model::MAX_EQ_GAIN_DB,
        alpha2_audio::MAX_PEAKING_GAIN_DB,
    );
    same(
        *alpha2_model::EQ_Q_RANGE.start(),
        *alpha2_audio::Q_RANGE.start(),
    );
    same(
        *alpha2_model::EQ_Q_RANGE.end(),
        *alpha2_audio::Q_RANGE.end(),
    );
    same(
        *alpha2_model::EQ_FREQUENCY_RANGE_HZ.start(),
        alpha2_audio::MIN_FREQUENCY_HZ,
    );
    // The top band of the highest rate renders within a hundredth of a hertz of the
    // limit.
    #[allow(clippy::cast_precision_loss)]
    let nyquist =
        *alpha2_model::SAMPLE_RATE_RANGE.end() as f32 * alpha2_audio::MAX_FREQUENCY_FRACTION;
    assert!((alpha2_model::EQ_FREQUENCY_RANGE_HZ.end() - nyquist).abs() < 0.01);
    same(alpha2_model::MAX_COMPRESSOR_RATIO, alpha2_audio::MAX_RATIO);
}

#[test]
fn a_ducking_chain_mixes_the_same_in_any_track_order() {
    let seconds = MediaTime::from_seconds(1).ticks();
    let ducking = |sidechain: &str| Ducking {
        sidechain_track: TrackId::new(sidechain.to_owned()).expect("id"),
        threshold: 0.05,
        depth_db: -24.0,
        attack_ms: 1.0,
        release_ms: 50.0,
    };
    // Music ducks under dialogue, which itself ducks under narration.
    let chain = |order: [&str; 3]| {
        let mut tracks: Vec<Track> = order
            .iter()
            .map(|id| track(id, vec![audio_clip(&format!("c-{id}"), id, 0, seconds)]))
            .collect();
        for track in &mut tracks {
            track.audio.ducking = match track.id.as_str() {
                "dialogue" => Some(ducking("narration")),
                "music" => Some(ducking("dialogue")),
                _ => None,
            };
        }
        project(tracks, vec!["narration", "dialogue", "music"])
    };
    let loud = Fixture::new(vec![
        ("narration", 0.9, 2),
        ("dialogue", 0.5, 2),
        ("music", 0.3, 2),
    ]);
    let (forward, _) = render(chain(["narration", "dialogue", "music"]), 512, &loud);
    let (reverse, _) = render(chain(["music", "dialogue", "narration"]), 512, &loud);
    // Only the order of the sum differs, so the mixes agree to rounding.
    for (index, (a, b)) in forward.channel(0)[..512]
        .iter()
        .zip(&reverse.channel(0)[..512])
        .enumerate()
    {
        assert!((a - b).abs() < 1.0e-6, "sample {index}: {a} against {b}");
    }
}
