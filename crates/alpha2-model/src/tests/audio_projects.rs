//! Audio in a project: clip properties, effect budgets, ducking, and track overlap.

use super::*;

#[test]
fn audio_properties_round_trip_and_validate() {
    let project = audio_project();
    project.validate().expect("valid audio project");
    let json = serde_json::to_string(&project).expect("serialize project");
    let decoded: Project = serde_json::from_str(&json).expect("deserialize project");
    assert_eq!(decoded, project);
}

#[test]
fn documents_without_audio_fields_receive_documented_defaults() {
    let mut value = serde_json::to_value(audio_project()).expect("serialize project");
    value
        .as_object_mut()
        .expect("project object")
        .remove("audio");
    let track = value["timeline"]["tracks"][1]
        .as_object_mut()
        .expect("track object");
    track.remove("audio");
    let clip = value["timeline"]["tracks"][1]["clips"][0]
        .as_object_mut()
        .expect("clip object");
    clip.remove("audio");

    let decoded: Project = serde_json::from_value(value).expect("audio fields are optional");
    assert_eq!(decoded.audio, crate::MasterAudio::default());
    assert_eq!(
        decoded.timeline.tracks[1].audio,
        crate::TrackAudio::default()
    );
    let clip_audio = &decoded.timeline.tracks[1].clips[0].audio;
    assert_eq!(*clip_audio, crate::AudioProperties::default());
    assert!(clip_audio.is_audible());
    assert_eq!(
        decoded.audio.limiter.map(|limiter| limiter.threshold),
        Some(1.0)
    );
}

#[test]
fn validation_rejects_out_of_range_audio_values() {
    let invalid = [
        crate::AudioProperties {
            gain: crate::Animated::Constant { value: -0.5 },
            ..crate::AudioProperties::default()
        },
        crate::AudioProperties {
            gain: crate::Animated::Constant {
                value: crate::MAX_GAIN * 2.0,
            },
            ..crate::AudioProperties::default()
        },
        crate::AudioProperties {
            pan: crate::Animated::Constant { value: 1.5 },
            ..crate::AudioProperties::default()
        },
        crate::AudioProperties {
            gain: crate::Animated::Constant { value: f32::NAN },
            ..crate::AudioProperties::default()
        },
        crate::AudioProperties {
            fade_in: MediaTime(-1),
            ..crate::AudioProperties::default()
        },
        crate::AudioProperties {
            effects: vec![crate::AudioEffect::Compressor {
                threshold_db: -12.0,
                ratio: 0.5,
                attack_ms: 5.0,
                release_ms: 80.0,
                lookahead_ms: 0.0,
            }],
            ..crate::AudioProperties::default()
        },
        crate::AudioProperties {
            effects: vec![crate::AudioEffect::Eq { bands: Vec::new() }],
            ..crate::AudioProperties::default()
        },
    ];
    for audio in invalid {
        let mut project = audio_project();
        project.timeline.tracks[1].clips[0].audio = audio;
        assert_eq!(
            project.validate(),
            Err(crate::ModelError::InvalidAudioProperties {
                clip: project.timeline.tracks[1].clips[0].id.clone()
            })
        );
    }
}

#[test]
fn the_project_bounds_the_audio_effects_it_holds_at_once() {
    let eq = crate::AudioEffect::Eq {
        bands: vec![crate::EqBand {
            kind: crate::EqBandKind::Peaking,
            frequency_hz: 1_000.0,
            q: 1.0,
            gain_db: 3.0,
        }],
    };
    // Every clip is within its own sixteen effects, and all sound at once; only the project's
    // total is over.
    let with_clips = |count: usize| {
        let mut project = audio_project();
        let template = project.timeline.tracks[1].clips[0].clone();
        project.timeline.tracks[1].clips = (0..count)
            .map(|index| Clip {
                id: id(ClipId::new, &format!("clip-{index}")),
                audio: crate::AudioProperties {
                    effects: vec![eq.clone(); 16],
                    ..template.audio.clone()
                },
                ..template.clone()
            })
            .collect();
        project
    };
    with_clips(crate::MAX_AUDIO_EFFECTS / 16)
        .validate()
        .expect("as many effects as allowed");
    assert_eq!(
        with_clips(crate::MAX_AUDIO_EFFECTS / 16 + 1).validate(),
        Err(crate::ModelError::TooManyAudioEffects {
            found: crate::MAX_AUDIO_EFFECTS + 16,
            maximum: crate::MAX_AUDIO_EFFECTS,
            at_ticks: 0,
        })
    );

    // Lookahead counts by its milliseconds, a reverb by REVERB_DELAY_MS.
    let compressor = |lookahead_ms| crate::AudioEffect::Compressor {
        threshold_db: -12.0,
        ratio: 4.0,
        attack_ms: 5.0,
        release_ms: 80.0,
        lookahead_ms,
    };
    let mut delayed = with_clips(75);
    for clip in &mut delayed.timeline.tracks[1].clips {
        clip.audio.effects = vec![compressor(100.0); 16];
    }
    delayed.validate().expect("120 s of lookahead is allowed");
    delayed.audio.effects.push(compressor(0.5));
    assert_eq!(
        delayed.validate(),
        Err(crate::ModelError::TooMuchAudioDelay {
            found_ms: 120_001,
            maximum_ms: crate::MAX_AUDIO_DELAY_MS,
            at_ticks: 0,
        })
    );
}

/// `audio_project` with its music clip, one second long and holding one
/// reverb, repeated `count` times `period` ticks apart from `first` on.
fn reverb_clips(count: usize, first: i64, period: i64) -> Project {
    let mut project = audio_project();
    let template = project.timeline.tracks[1].clips[0].clone();
    project.timeline.tracks[1].clips = (0..count)
        .map(|index| {
            let start = first + period * i64::try_from(index).expect("index");
            Clip {
                id: id(ClipId::new, &format!("clip-{index}")),
                timeline_range: TimeRange::new(MediaTime(start), MediaTime(start + 120_000))
                    .expect("range"),
                source_range: TimeRange::new(MediaTime(0), MediaTime(120_000)).expect("range"),
                ..template.clone()
            }
        })
        .collect();
    project
}

#[test]
fn effects_that_never_hold_state_at_once_are_not_summed() {
    // 751 reverbs hold 120 160 ms of delay in all, but three seconds apart only one holds state
    // at a time.
    for count in [751, 2_000] {
        assert_eq!(
            reverb_clips(count, 0, 3 * 120_000).validate(),
            Ok(()),
            "{count} reverbs one after another"
        );
    }
}

#[test]
fn effects_holding_state_at_once_are_bounded_where_they_overlap() {
    // At ten seconds 751 reverbs start at once: they hold state together from the margin before.
    let margin = i64::from(crate::AUDIO_EFFECT_STATE_MARGIN_MS) * 120;
    let start = 10 * 120_000;
    assert_eq!(
        reverb_clips(751, start, 0).validate(),
        Err(crate::ModelError::TooMuchAudioDelay {
            found_ms: 120_160,
            maximum_ms: crate::MAX_AUDIO_DELAY_MS,
            at_ticks: start - margin,
        })
    );
    reverb_clips(750, start, 0)
        .validate()
        .expect("750 reverbs at once hold 120 s");

    // A clip ending where the next starts holds state with it, and so does one ending within the
    // margin before; one ending earlier does not.
    let pair = |period| reverb_clips(2, start, period).audio_effect_load();
    assert_eq!(pair(120_000).effects, 2);
    assert_eq!(pair(120_000 + margin).effects, 2);
    assert_eq!(pair(120_000 + margin + 1).effects, 1);
    assert_eq!(
        pair(120_000 + margin).effects_at_ticks,
        start + 120_000,
        "the second clip's state begins as the first ends"
    );

    // A clip's effects count from its lookahead, its track's and its own, before the margin.
    let compressor = crate::AudioEffect::Compressor {
        threshold_db: -12.0,
        ratio: 4.0,
        attack_ms: 5.0,
        release_ms: 80.0,
        lookahead_ms: 50.0,
    };
    let mut looking = reverb_clips(2, start, 120_000 + margin + 1);
    assert_eq!(looking.audio_effect_load().effects, 1);
    looking.timeline.tracks[1].audio.effects = vec![compressor.clone()];
    assert_eq!(
        looking.audio_effect_load().effects,
        3,
        "the track's compressor and both reverbs"
    );
    looking.timeline.tracks[1].audio.effects.clear();
    looking.timeline.tracks[1].clips[1]
        .audio
        .effects
        .push(compressor);
    let load = looking.audio_effect_load();
    assert_eq!((load.effects, load.delay_ms()), (3, 160 + 160 + 50));

    // The effect count is bounded the same way: 513 clips of sixteen effects at once are over.
    let eq = crate::AudioEffect::Eq {
        bands: vec![crate::EqBand {
            kind: crate::EqBandKind::Peaking,
            frequency_hz: 1_000.0,
            q: 1.0,
            gain_db: 3.0,
        }],
    };
    let with_eqs = |period| {
        let mut project = reverb_clips(crate::MAX_AUDIO_EFFECTS / 16 + 1, start, period);
        for clip in &mut project.timeline.tracks[1].clips {
            clip.audio.effects = vec![eq.clone(); 16];
        }
        project
    };
    assert_eq!(
        with_eqs(0).validate(),
        Err(crate::ModelError::TooManyAudioEffects {
            found: crate::MAX_AUDIO_EFFECTS + 16,
            maximum: crate::MAX_AUDIO_EFFECTS,
            at_ticks: start - margin,
        })
    );
    with_eqs(3 * 120_000)
        .validate()
        .expect("sixteen effects at a time");

    // Master and track effects hold state throughout; before timeline zero nothing does.
    let mut early = reverb_clips(1, 0, 0);
    early.audio.effects = vec![eq.clone(); 2];
    let load = early.audio_effect_load();
    assert_eq!((load.effects, load.effects_at_ticks), (3, 0));
}

#[test]
fn keyframed_gain_requires_ascending_finite_points() {
    let mut project = audio_project();
    project.timeline.tracks[1].clips[0].audio.gain = crate::Animated::Keyframes {
        keyframes: vec![
            crate::Keyframe {
                time: MediaTime(120_000),
                value: 1.0,
                interpolation: crate::Interpolation::Linear,
            },
            crate::Keyframe {
                time: MediaTime(0),
                value: 0.0,
                interpolation: crate::Interpolation::Linear,
            },
        ],
    };
    assert!(project.validate().is_err());

    project.timeline.tracks[1].clips[0].audio.gain = crate::Animated::Keyframes {
        keyframes: vec![
            crate::Keyframe {
                time: MediaTime(0),
                value: 0.0,
                interpolation: crate::Interpolation::Linear,
            },
            crate::Keyframe {
                time: MediaTime(120_000),
                value: 1.0,
                interpolation: crate::Interpolation::Linear,
            },
        ],
    };
    project.validate().expect("ascending keyframes are valid");
}

#[test]
fn ducking_rejects_self_missing_and_cyclic_sidechains() {
    let ducking = |track: &str| crate::Ducking {
        sidechain_track: id(TrackId::new, track),
        threshold: 0.1,
        depth_db: -12.0,
        attack_ms: 5.0,
        release_ms: 120.0,
    };

    let mut project = audio_project();
    project.timeline.tracks[1].audio.ducking = Some(ducking("track-audio"));
    assert!(matches!(
        project.validate(),
        Err(crate::ModelError::InvalidDuckingSidechain { .. })
    ));

    let mut project = audio_project();
    project.timeline.tracks[1].audio.ducking = Some(ducking("absent-track"));
    assert!(matches!(
        project.validate(),
        Err(crate::ModelError::InvalidDuckingSidechain { .. })
    ));

    // Every track carries an audio bus, so a visual track may drive a sidechain.
    let mut project = audio_project();
    project.timeline.tracks[1].audio.ducking = Some(ducking("track-a"));
    project
        .validate()
        .expect("a visual track may drive a sidechain");

    let mut project = audio_project();
    let second = Track {
        id: id(TrackId::new, "track-voice"),
        name: "Voice".into(),
        kind: TrackKind::Audio,
        clips: Vec::new(),
        transitions: Vec::new(),
        audio: crate::TrackAudio {
            ducking: Some(ducking("track-audio")),
            ..crate::TrackAudio::default()
        },
        captions: None,
    };
    project.timeline.tracks.push(second);
    project.validate().expect("one-way ducking is valid");

    project.timeline.tracks[1].audio.ducking = Some(ducking("track-voice"));
    assert!(matches!(
        project.validate(),
        Err(crate::ModelError::InvalidDuckingSidechain { .. })
    ));
}

/// Audio tracks named `name(index)` for `count` indices, each ducked by the
/// track `sidechain(index)` names, if any.
fn ducking_tracks(
    count: usize,
    name: impl Fn(usize) -> String,
    sidechain: impl Fn(usize) -> Option<usize>,
) -> Vec<Track> {
    (0..count)
        .map(|index| Track {
            id: id(TrackId::new, &name(index)),
            name: "Duck".into(),
            kind: TrackKind::Audio,
            clips: Vec::new(),
            transitions: Vec::new(),
            audio: crate::TrackAudio {
                ducking: sidechain(index).map(|other| crate::Ducking {
                    sidechain_track: id(TrackId::new, &name(other)),
                    threshold: 0.1,
                    depth_db: -12.0,
                    attack_ms: 5.0,
                    release_ms: 120.0,
                }),
                ..crate::TrackAudio::default()
            },
            captions: None,
        })
        .collect()
}

#[test]
fn a_ducking_cycle_is_reported_on_its_first_track_however_it_is_reached() {
    let name = |index: usize| format!("track-{index}");
    let invalid_track = |project: &Project| match project.validate() {
        Err(crate::ModelError::InvalidDuckingSidechain { track, .. }) => track.to_string(),
        other => panic!("expected an invalid sidechain, got {other:?}"),
    };
    let mut project = project();
    // 0 -> 1 -> 2 -> 3 -> 2: the tracks before the cycle lead into it but are not on it.
    project.timeline.tracks = ducking_tracks(4, name, |index| Some([1, 2, 3, 2][index]));
    assert_eq!(invalid_track(&project), "track-2");
    // 0 -> 3 -> 0 and 1 -> 2 -> 1: the first track on a cycle, in document order.
    project.timeline.tracks = ducking_tracks(4, name, |index| Some([3, 2, 1, 0][index]));
    assert_eq!(invalid_track(&project), "track-0");
    // 0 -> 1 -> 2, 2 -> 2: a track ducked by itself, at the end of a chain.
    project.timeline.tracks = ducking_tracks(3, name, |index| Some((index + 1).min(2)));
    assert_eq!(invalid_track(&project), "track-2");
    // 0 -> 1 -> 2 -> (missing): the missing sidechain is reported on its own track.
    project.timeline.tracks = ducking_tracks(3, name, |index| Some(index + 1));
    assert_eq!(invalid_track(&project), "track-2");
    // 0 -> 1 -> 2 -> 3, 3 ducked by nothing: a chain is valid.
    project.timeline.tracks = ducking_tracks(4, name, |index| (index < 3).then_some(index + 1));
    project.validate().expect("a ducking chain is valid");
}

#[test]
fn validating_a_ducking_chain_takes_work_in_proportion_to_its_tracks() {
    // Long ids that differ only at their end, so comparing two reads their whole length.
    let name = |index: usize| format!("{}{index:010}", "t".repeat(1_000));
    let with_tracks = |count: usize| {
        let mut project = project();
        project.timeline.tracks = ducking_tracks(count, name, |index| {
            (index + 1 < count).then_some(index + 1)
        });
        project
    };
    alpha2_scaling_support::assert_linear_shared(
        "validating a ducking chain",
        crate::MAX_TRACKS / alpha2_scaling_support::GROWTH,
        with_tracks,
        |project| project.validate().expect("a ducking chain is valid"),
    );
}

#[test]
fn audio_tracks_may_overlay_while_visual_tracks_still_need_transitions() {
    let mut project = audio_project();
    let mut second = project.timeline.tracks[1].clips[0].clone();
    second.id = id(ClipId::new, "clip-music-2");
    second.timeline_range =
        TimeRange::new(MediaTime(120_000), MediaTime(360_000)).expect("ordered range");
    project.timeline.tracks[1].clips.push(second);
    project
        .validate()
        .expect("overlapping audio clips are a defined mix");

    let mut visual = project.clone();
    let mut second = visual.timeline.tracks[0].clips[0].clone();
    second.id = id(ClipId::new, "clip-a2");
    second.timeline_range =
        TimeRange::new(MediaTime(60_000), MediaTime(180_000)).expect("ordered range");
    visual.timeline.tracks[0].clips.push(second);
    assert!(matches!(
        visual.validate(),
        Err(crate::ModelError::OverlappingClips { .. })
    ));
}

#[test]
fn a_video_source_declares_whether_it_carries_audio() {
    let mut project = audio_project();
    assert!(!project.sources[0].is_audible(), "video without a stream");
    assert!(project.sources[1].is_audible(), "an audio source always is");

    project.sources[0].has_audio = true;
    assert!(project.sources[0].is_audible());
    project.validate().expect("declaring audio is always valid");

    let mut value = serde_json::to_value(&project).expect("serialize project");
    value["sources"][0]
        .as_object_mut()
        .expect("source object")
        .remove("has_audio");
    let decoded: Project = serde_json::from_value(value).expect("has_audio is optional");
    assert!(!decoded.sources[0].has_audio);
}
