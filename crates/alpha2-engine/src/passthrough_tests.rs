use super::*;
use alpha2_time::MediaTime;
use serde_json::json;

const SECOND: i64 = alpha2_time::TICKS_PER_SECOND;

fn range(start: i64, end: i64) -> TimeRange {
    TimeRange::new(MediaTime::from_ticks(start), MediaTime::from_ticks(end)).expect("range")
}

/// One video clip at `[1 s, 5 s)` showing source `[2 s, 6 s)`, and
/// whatever `edit` changes on the JSON before it is parsed.
fn project(edit: impl FnOnce(&mut serde_json::Value)) -> Project {
    let mut value = json!({
        "schema_version": 1, "id": "passthrough", "name": "Passthrough",
        "settings": {"width": 64, "height": 48, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
        "sources": [{"id": "video", "locator": "video.mp4", "kind": "video", "has_audio": true}],
        "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [{
            "id": "clip", "source_id": "video",
            "timeline_range": {"start": SECOND, "end": 5 * SECOND},
            "source_range": {"start": 2 * SECOND, "end": 6 * SECOND}
        }]}]}
    });
    edit(&mut value);
    Project::from_json(&value.to_string()).expect("valid project")
}

fn clip(value: &mut serde_json::Value) -> &mut serde_json::Value {
    &mut value["timeline"]["tracks"][0]["clips"][0]
}

#[test]
fn an_untouched_clip_copies_the_source_interval_under_the_range() {
    let plan =
        passthrough_plan(&project(|_| {}), range(2 * SECOND, 4 * SECOND), false).expect("eligible");
    let [segment] = &plan.segments[..] else {
        panic!("one clip, one segment: {plan:?}");
    };
    assert_eq!(segment.source_id.as_str(), "video");
    assert_eq!(segment.timeline_range, range(2 * SECOND, 4 * SECOND));
    assert_eq!(segment.source_range, range(3 * SECOND, 5 * SECOND));
    assert_eq!(plan.audio, PassthroughAudio::Absent);
}

#[test]
fn anything_rendering_would_change_refuses_and_says_what() {
    let refused = |edit: &dyn Fn(&mut serde_json::Value), within: TimeRange, needle: &str| {
        let reason = passthrough_plan(&project(edit), within, false).expect_err("must refuse");
        assert!(reason.contains(needle), "{needle:?} not in {reason:?}");
    };
    let whole = range(SECOND, 5 * SECOND);
    refused(&|_| {}, range(0, 2 * SECOND), "background would show");
    refused(&|_| {}, range(6 * SECOND, 7 * SECOND), "no visual clip");
    refused(
        &|value| {
            clip(value)["visual"] = json!({
                "position": {"mode": "constant", "value": {"x": 0.5, "y": 0.5}},
                "anchor": {"x": 0.5, "y": 0.5},
                "scale": {"mode": "constant", "value": {"x": 1.0, "y": 1.0}},
                "rotation_degrees": {"mode": "constant", "value": 0.0},
                "opacity": {"mode": "constant", "value": 0.5}
            });
        },
        whole,
        "faded",
    );
    refused(
        &|value| clip(value)["time_map"] = json!({"type": "reverse"}),
        whole,
        "retimed",
    );
    refused(
        &|value| clip(value)["source_range"] = json!({"start": 0, "end": 8 * SECOND}),
        whole,
        "retimed",
    );
    refused(
        &|value| clip(value)["blend_mode"] = json!("multiply"),
        whole,
        "blends",
    );
    refused(
        &|value| {
            value["timeline"]["tracks"].as_array_mut().expect("tracks").push(json!({
                "id": "c", "name": "C", "kind": "caption",
                "captions": {"cues": [{"timeline_range": {"start": 2 * SECOND, "end": 3 * SECOND}, "text": "hi"}]}
            }));
        },
        whole,
        "caption",
    );
    refused(
        &|value| {
            value["sources"].as_array_mut().expect("sources").push(json!({
                "id": "wash", "kind": "generated",
                "generator": {"type": "fill", "paint": {"type": "solid", "color": {"red": 1, "green": 1, "blue": 1, "alpha": 1}}}
            }));
            value["timeline"]["tracks"]
                .as_array_mut()
                .expect("tracks")
                .push(json!({
                    "id": "v2", "name": "V2", "kind": "video", "clips": [{
                        "id": "over", "source_id": "wash",
                        "timeline_range": {"start": 4 * SECOND, "end": 6 * SECOND},
                        "source_range": {"start": 0, "end": 2 * SECOND}
                    }]
                }));
        },
        whole,
        "both visible at once",
    );
    // A clip that is another clip's matte is never drawn itself, even where that clip is
    // not.
    refused(
        &|value| {
            value["timeline"]["tracks"]
                .as_array_mut()
                .expect("tracks")
                .push(json!({
                    "id": "v2", "name": "V2", "kind": "video", "clips": [{
                        "id": "matted", "source_id": "video",
                        "timeline_range": {"start": 5 * SECOND, "end": 6 * SECOND},
                        "source_range": {"start": 0, "end": SECOND},
                        "matte": {"source": "clip", "mode": "luma"}
                    }]
                }));
        },
        whole,
        "matte",
    );
    passthrough_plan(
        &project(|value| {
            value["sources"].as_array_mut().expect("sources").push(json!({
                "id": "wash", "kind": "generated",
                "generator": {"type": "fill", "paint": {"type": "solid", "color": {"red": 1, "green": 1, "blue": 1, "alpha": 1}}}
            }));
            value["timeline"]["tracks"].as_array_mut().expect("tracks").push(json!({
                "id": "v2", "name": "V2", "kind": "video", "clips": [{
                    "id": "over", "source_id": "wash",
                    "timeline_range": {"start": 5 * SECOND, "end": 6 * SECOND},
                    "source_range": {"start": 0, "end": SECOND}
                }]
            }));
        }),
        whole,
        false,
    )
    .expect("a clip after the range does not matter");
}

#[test]
#[allow(clippy::too_many_lines)]
fn every_change_rendering_makes_is_refused_and_named() {
    type Edit = Box<dyn Fn(&mut serde_json::Value)>;
    let whole = range(SECOND, 5 * SECOND);
    let visual = |field: &str, value: serde_json::Value| {
        let mut visual = json!({
            "position": {"mode": "constant", "value": {"x": 0.5, "y": 0.5}},
            "anchor": {"x": 0.5, "y": 0.5},
            "scale": {"mode": "constant", "value": {"x": 1.0, "y": 1.0}},
            "rotation_degrees": {"mode": "constant", "value": 0.0},
            "opacity": {"mode": "constant", "value": 1.0}
        });
        visual[field] = value;
        visual
    };
    let add_source = |value: &mut serde_json::Value, source: serde_json::Value| {
        value["sources"]
            .as_array_mut()
            .expect("sources")
            .push(source);
    };
    let add_track = |value: &mut serde_json::Value, track: serde_json::Value| {
        value["timeline"]["tracks"]
            .as_array_mut()
            .expect("tracks")
            .push(track);
    };
    let picture: Vec<(&str, Edit, TimeRange, &str)> = vec![
        (
            "an image",
            Box::new(move |value| {
                add_source(
                    value,
                    json!({"id": "still", "locator": "still.png", "kind": "image"}),
                );
                clip(value)["source_id"] = json!("still");
            }),
            whole,
            "not a decoded video file",
        ),
        (
            "a provider's video",
            Box::new(move |value| {
                add_source(
                    value,
                    json!({"id": "gif", "locator": "a.gif", "kind": "video", "provider": {"format": "animated_image"}}),
                );
                clip(value)["source_id"] = json!("gif");
            }),
            whole,
            "not a decoded video file",
        ),
        (
            "a composition",
            Box::new(|value| {
                value["compositions"] = json!([{"id": "look", "nodes": [{"id": "clip", "op": {"type": "clip"}}], "output": "clip"}]);
                clip(value)["composition"] = json!({"id": "look"});
            }),
            whole,
            "composition graph",
        ),
        (
            "a mask",
            Box::new(|value| {
                clip(value)["masks"] = json!([{"shape": {"type": "ellipse"},
                    "center": {"mode": "constant", "value": {"x": 0.5, "y": 0.5}},
                    "size": {"mode": "constant", "value": {"x": 0.8, "y": 0.7}},
                    "feather": {"mode": "constant", "value": 0.05}}]);
            }),
            whole,
            "masked or matted",
        ),
        (
            "a matte from a clip outside the range",
            Box::new(move |value| {
                add_track(
                    value,
                    json!({"id": "m", "name": "M", "kind": "video", "clips": [{
                        "id": "matte", "source_id": "video",
                        "timeline_range": {"start": 6 * SECOND, "end": 7 * SECOND},
                        "source_range": {"start": 0, "end": SECOND}
                    }]}),
                );
                clip(value)["matte"] = json!({"source": "matte", "mode": "alpha"});
            }),
            whole,
            "masked or matted",
        ),
        (
            "an anchor",
            Box::new(move |value| {
                clip(value)["visual"] = visual("anchor", json!({"x": 0.25, "y": 0.5}));
            }),
            whole,
            "moved, scaled, rotated",
        ),
        (
            "a scale",
            Box::new(move |value| {
                clip(value)["visual"] = visual(
                    "scale",
                    json!({"mode": "constant", "value": {"x": 2.0, "y": 2.0}}),
                );
            }),
            whole,
            "moved, scaled, rotated",
        ),
        (
            "a rotation",
            Box::new(move |value| {
                clip(value)["visual"] = visual(
                    "rotation_degrees",
                    json!({"mode": "constant", "value": 10.0}),
                );
            }),
            whole,
            "moved, scaled, rotated",
        ),
        (
            "an identity held by keyframes",
            Box::new(move |value| {
                clip(value)["visual"] = visual(
                    "position",
                    json!({"mode": "keyframes", "keyframes": [
                    {"time": 0, "value": {"x": 0.5, "y": 0.5}, "interpolation": {"type": "linear"}},
                    {"time": SECOND, "value": {"x": 0.5, "y": 0.5}, "interpolation": {"type": "hold"}}]}),
                );
            }),
            whole,
            "moved, scaled, rotated",
        ),
        (
            "a range past the clip's end",
            Box::new(|_| {}),
            range(SECOND, 6 * SECOND),
            "ends before the range does",
        ),
    ];
    let mut wrong = Vec::new();
    for (name, edit, within, needle) in &picture {
        match passthrough_plan(&project(edit), *within, false) {
            Err(reason) if reason.contains(needle) => {}
            other => wrong.push(format!(
                "{name}: {other:?}, not a refusal naming {needle:?}"
            )),
        }
    }
    // Anything processing the copied clip's sound makes the export's audio the render's mix,
    // and the reason says what.
    let sound: Vec<(&str, Edit, &str)> = vec![
        (
            "a clip pan",
            Box::new(|value| {
                clip(value)["audio"] = json!({"pan": {"mode": "constant", "value": 0.5}});
            }),
            "audio is processed",
        ),
        (
            "a clip fade",
            Box::new(|value| clip(value)["audio"] = json!({"fade_in": SECOND / 2})),
            "audio is processed",
        ),
        (
            "a clip effect",
            Box::new(
                |value| clip(value)["audio"] = json!({"effects": [{"type": "reverb", "room_size": 0.5, "damping": 0.5, "mix": 0.3}]}),
            ),
            "audio is processed",
        ),
        (
            "a track gain",
            Box::new(|value| {
                value["timeline"]["tracks"][0]["audio"] =
                    json!({"gain": {"mode": "constant", "value": 0.5}});
            }),
            "bus processes",
        ),
        (
            "a track pan",
            Box::new(|value| {
                value["timeline"]["tracks"][0]["audio"] =
                    json!({"pan": {"mode": "constant", "value": -0.5}});
            }),
            "bus processes",
        ),
        (
            "a track effect",
            Box::new(|value| {
                value["timeline"]["tracks"][0]["audio"] = json!({"effects": [{"type": "reverb", "room_size": 0.5, "damping": 0.5, "mix": 0.3}]});
            }),
            "bus processes",
        ),
        (
            "a master gain",
            Box::new(|value| {
                value["audio"] =
                    json!({"limiter": null, "gain": {"mode": "constant", "value": 0.5}});
            }),
            "master bus",
        ),
        (
            "a master effect",
            Box::new(|value| {
                value["audio"] = json!({"limiter": null, "effects": [{"type": "reverb", "room_size": 0.5, "damping": 0.5, "mix": 0.3}]});
            }),
            "master bus",
        ),
        (
            "another audio track's clip in the range",
            Box::new(move |value| {
                add_source(
                    value,
                    json!({"id": "music", "locator": "music.wav", "kind": "audio"}),
                );
                add_track(
                    value,
                    json!({"id": "a", "name": "A", "kind": "audio", "clips": [{
                        "id": "music-clip", "source_id": "music",
                        "timeline_range": {"start": 2 * SECOND, "end": 3 * SECOND},
                        "source_range": {"start": 0, "end": SECOND}
                    }]}),
                );
            }),
            "heard in the range too",
        ),
    ];
    for (name, edit, needle) in &sound {
        let project = project(|value| {
            if value.get("audio").is_none() {
                value["audio"] = json!({"limiter": null});
            }
            edit(value);
            if value["audio"].get("limiter").is_none() {
                value["audio"]["limiter"] = json!(null);
            }
        });
        match passthrough_plan(&project, whole, true).map(|plan| plan.audio) {
            Ok(PassthroughAudio::Mixed(reason)) if reason.contains(needle) => {}
            other => wrong.push(format!("{name}: {other:?}, not a mix naming {needle:?}")),
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[test]
fn a_source_declared_in_another_color_than_the_outputs_refuses() {
    let whole = range(SECOND, 5 * SECOND);
    // A color the project declares wins over the stream's tags in a render; a copy would keep
    // the tags.
    let reason = passthrough_plan(
        &project(|value| {
            value["sources"][0]["color_metadata"] = json!({
                "primaries": "bt2020", "transfer": "pq", "matrix": "bt2020_ncl",
                "range": "limited", "dynamic_range": "hdr"
            });
        }),
        whole,
        false,
    )
    .expect_err("must refuse");
    assert!(reason.contains("declares a color"), "{reason}");
    // Declared as the output is tagged, the stream's own tags decide, which the file check
    // reads.
    passthrough_plan(
        &project(|value| {
            value["sources"][0]["color_metadata"] =
                serde_json::to_value(alpha2_media::ColorMetadata::EXPORT).expect("color");
        }),
        whole,
        false,
    )
    .expect("a declaration of the output's color");
}

#[test]
fn audio_never_refuses_a_copy_and_says_whether_it_is_the_sources_own() {
    let whole = range(SECOND, 5 * SECOND);
    let audio = |project: &Project, include: bool| {
        passthrough_plan(project, whole, include)
            .expect("the picture is untouched")
            .audio
    };
    // Every project has a master limiter unless it removes it, so the mix is rendered.
    let limited = audio(&project(|_| {}), true);
    assert!(
        matches!(&limited, PassthroughAudio::Mixed(reason) if reason.contains("limiter")),
        "{limited:?}"
    );
    let unlimited = |edit: &dyn Fn(&mut serde_json::Value)| {
        project(|value| {
            value["audio"] = json!({"limiter": null});
            edit(value);
        })
    };
    assert_eq!(
        audio(&unlimited(&|_| {}), true),
        PassthroughAudio::Untouched
    );
    assert_eq!(audio(&project(|_| {}), false), PassthroughAudio::Absent);
    let gained = audio(
        &unlimited(&|value| {
            clip(value)["audio"] = json!({"gain": {"mode": "constant", "value": 0.5}});
        }),
        true,
    );
    assert!(
        matches!(&gained, PassthroughAudio::Mixed(reason) if reason.contains("audio is processed")),
        "{gained:?}"
    );
    // A muted clip is heard as silence, which the export still carries.
    let muted = audio(
        &unlimited(&|value| clip(value)["audio"] = json!({"muted": true})),
        true,
    );
    assert!(
        matches!(&muted, PassthroughAudio::Mixed(reason) if reason.contains("nothing in the range is heard")),
        "{muted:?}"
    );
}

/// The copied clip's source, as the render's provider reads it.
const COPIED: &str = "video";

/// Every source at one constant level per channel: `copied` for the
/// copied clip's, `others` for the rest.
struct Levels(Vec<(SourceId, alpha2_media::AudioBlock)>);

impl Levels {
    fn new(project: &Project, copied: f32, others: f32) -> Self {
        let frames = 10 * 48_000;
        Self(
            project
                .sources
                .iter()
                .map(|source| {
                    let level = if source.id.as_str() == COPIED {
                        copied
                    } else {
                        others
                    };
                    (
                        source.id.clone(),
                        alpha2_media::AudioBlock {
                            start_sample: 0,
                            sample_rate: 48_000,
                            speakers: alpha2_media::SpeakerLayout::STEREO,
                            channels: vec![vec![level; frames]; 2],
                        },
                    )
                })
                .collect(),
        )
    }
}

impl alpha2_media::AudioBlockProvider for Levels {
    fn block(
        &self,
        source: &SourceId,
        _start_sample: i64,
        _frames: usize,
    ) -> Option<alpha2_media::AudioSpan<'_>> {
        self.0
            .iter()
            .find(|(id, _)| id == source)
            .map(|(_, block)| alpha2_media::AudioSpan::whole(block))
    }
}

/// What an export of `within` mixes, both channels, from a fresh renderer.
fn render_mix(project: &Project, within: TimeRange, levels: &Levels) -> Vec<f32> {
    let snapshot = crate::EngineProject::new(project.clone()).expect("valid project");
    let mut renderer = crate::AudioRenderer::new(&snapshot, 2, 1_024).expect("renderer");
    let samples = |time: MediaTime| time.ticks() * 48_000 / SECOND;
    let (mut position, end) = (samples(within.start()), samples(within.end()));
    let mut mix = Vec::new();
    while position < end {
        let frames = usize::try_from((end - position).min(1_024)).expect("small");
        let status = renderer
            .render(
                &snapshot,
                position,
                frames,
                levels,
                alpha2_audio::TransportState::Playing,
            )
            .expect("render");
        assert!(status.missing_sources.is_empty(), "{status:?}");
        mix.extend_from_slice(&renderer.channel(0)[..frames]);
        mix.extend_from_slice(&renderer.channel(1)[..frames]);
        position += i64::try_from(frames).expect("small");
    }
    mix
}

/// Whether the render of `within` is the copied clip's audio alone:
/// with every other source sounding too, the mix is the copied source's
/// level, passed through.
fn render_is_the_copied_audio(project: &Project, within: TimeRange) -> bool {
    render_mix(project, within, &Levels::new(project, 0.25, 0.5))
        .iter()
        .all(|sample| (sample - 0.25).abs() < 1e-6)
}

/// Copied audio is the render's mix exactly where the render mixes the
/// copied clip alone, untouched; a heard range that is not is encoded,
/// as the render encodes it, silence included.
#[test]
#[allow(clippy::too_many_lines)]
fn the_audio_is_the_sources_own_exactly_where_the_render_mixes_it_alone() {
    let whole = range(SECOND, 5 * SECOND);
    let unlimited = |edit: &dyn Fn(&mut serde_json::Value)| {
        project(|value| {
            value["audio"] = json!({"limiter": null});
            edit(value);
        })
    };
    let add_track = |value: &mut serde_json::Value, track: serde_json::Value| {
        value["timeline"]["tracks"]
            .as_array_mut()
            .expect("tracks")
            .push(track);
    };
    let add_music = |value: &mut serde_json::Value| {
        value["sources"]
            .as_array_mut()
            .expect("sources")
            .push(json!({"id": "music", "locator": "music.wav", "kind": "audio"}));
    };
    let music_clip = |start: i64, end: i64| {
        json!({
            "id": "music-clip", "source_id": "music",
            "timeline_range": {"start": start, "end": end},
            "source_range": {"start": 0, "end": end - start}
        })
    };
    let cases: Vec<(&str, Project)> = vec![
        ("untouched", unlimited(&|_| {})),
        (
            "another track solos, empty",
            unlimited(&|value| {
                add_track(
                    value,
                    json!({"id": "a", "name": "A", "kind": "audio", "audio": {"solo": true}}),
                );
            }),
        ),
        (
            "another track solos, its clips outside the range",
            unlimited(&|value| {
                add_music(value);
                add_track(
                    value,
                    json!({"id": "a", "name": "A", "kind": "audio", "audio": {"solo": true},
                           "clips": [music_clip(6 * SECOND, 7 * SECOND)]}),
                );
            }),
        ),
        (
            "the copied clip's track solos",
            unlimited(&|value| {
                value["timeline"]["tracks"][0]["audio"] = json!({"solo": true});
                add_music(value);
                add_track(
                    value,
                    json!({"id": "a", "name": "A", "kind": "audio",
                           "clips": [music_clip(2 * SECOND, 3 * SECOND)]}),
                );
            }),
        ),
        (
            "a muted track's clip in the range",
            unlimited(&|value| {
                add_music(value);
                add_track(
                    value,
                    json!({"id": "a", "name": "A", "kind": "audio", "audio": {"muted": true},
                           "clips": [music_clip(2 * SECOND, 3 * SECOND)]}),
                );
            }),
        ),
        (
            "the copied clip's track is ducked",
            unlimited(&|value| {
                add_music(value);
                add_track(
                    value,
                    json!({"id": "a", "name": "A", "kind": "audio",
                           "clips": [music_clip(0, SECOND)]}),
                );
                value["timeline"]["tracks"][0]["audio"] = json!({"ducking": {
                    "sidechain_track": "a", "threshold": 0.01, "depth_db": -24.0,
                    "attack_ms": 5.0, "release_ms": 2000.0
                }});
            }),
        ),
        (
            "a reverb's tail rings into the range",
            unlimited(&|value| {
                add_music(value);
                add_track(
                    value,
                    json!({"id": "a", "name": "A", "kind": "audio",
                           "audio": {"effects": [{"type": "reverb", "room_size": 0.9, "damping": 0.1, "mix": 1.0}]},
                           "clips": [music_clip(0, SECOND)]}),
                );
            }),
        ),
        (
            "the copied clip is muted",
            unlimited(&|value| clip(value)["audio"] = json!({"muted": true})),
        ),
        (
            "the copied source has no audio",
            unlimited(&|value| value["sources"][0]["has_audio"] = json!(false)),
        ),
    ];
    let mut wrong = Vec::new();
    for (name, project) in &cases {
        let audio = passthrough_plan(project, whole, true)
            .expect("the picture is untouched")
            .audio;
        let alone = render_is_the_copied_audio(project, whole);
        // An export that carries audio carries a stream: what is not copied is the render's
        // mix, encoded.
        let agrees = (audio == PassthroughAudio::Untouched) == alone
            && matches!(
                audio,
                PassthroughAudio::Untouched | PassthroughAudio::Mixed(_)
            );
        if !agrees {
            wrong.push(format!(
                "{name}: the plan says {audio:?}, the render is the copied audio alone: {alone}"
            ));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// A second clip after the first on the same track, showing `source`
/// from `source_start` for two seconds.
fn follow_with(value: &mut serde_json::Value, source: &str, source_start: i64) {
    value["timeline"]["tracks"][0]["clips"]
        .as_array_mut()
        .expect("clips")
        .push(json!({
            "id": "next", "source_id": source,
            "timeline_range": {"start": 5 * SECOND, "end": 7 * SECOND},
            "source_range": {"start": source_start, "end": source_start + 2 * SECOND}
        }));
}

#[test]
fn clips_that_tile_the_range_copy_as_segments_in_order() {
    let plan = passthrough_plan(
        &project(|value| follow_with(value, "video", 0)),
        range(2 * SECOND, 6 * SECOND),
        false,
    )
    .expect("a cut between two untouched clips");
    let spans = plan
        .segments
        .iter()
        .map(|segment| {
            (
                segment.clip_id.as_str(),
                segment.timeline_range,
                segment.source_range,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        spans,
        [
            (
                "clip",
                range(2 * SECOND, 5 * SECOND),
                range(3 * SECOND, 6 * SECOND)
            ),
            ("next", range(5 * SECOND, 6 * SECOND), range(0, SECOND)),
        ]
    );
    assert_eq!(
        plan.segments[1]
            .source_time(MediaTime::from_ticks(5 * SECOND + 4_000))
            .ticks(),
        4_000
    );
    // Two untouched clips' audio is still a join of two sources, which is mixed and
    // encoded.
    let joined = passthrough_plan(
        &project(|value| {
            value["audio"] = json!({"limiter": null});
            follow_with(value, "video", 0);
        }),
        range(2 * SECOND, 6 * SECOND),
        true,
    )
    .expect("eligible");
    assert!(
        matches!(&joined.audio, PassthroughAudio::Mixed(reason) if reason.contains("more than one clip")),
        "{joined:?}"
    );
}

#[test]
fn a_gap_an_overlap_or_a_touched_clip_in_a_tiling_refuses() {
    let refused = |edit: &dyn Fn(&mut serde_json::Value), needle: &str| {
        let reason = passthrough_plan(&project(edit), range(2 * SECOND, 6 * SECOND), false)
            .expect_err("must refuse");
        assert!(reason.contains(needle), "{needle:?} not in {reason:?}");
    };
    refused(
        &|value| {
            follow_with(value, "video", 0);
            value["timeline"]["tracks"][0]["clips"][1]["timeline_range"] =
                json!({"start": 5 * SECOND + 4_000, "end": 7 * SECOND});
            value["timeline"]["tracks"][0]["clips"][1]["source_range"] =
                json!({"start": 0, "end": 2 * SECOND - 4_000});
        },
        "background shows between",
    );
    refused(
        &|value| {
            follow_with(value, "video", 0);
            value["timeline"]["tracks"][0]["clips"][1]["visual"] = json!({
                "position": {"mode": "constant", "value": {"x": 0.25, "y": 0.5}},
                "anchor": {"x": 0.5, "y": 0.5},
                "scale": {"mode": "constant", "value": {"x": 1.0, "y": 1.0}},
                "rotation_degrees": {"mode": "constant", "value": 0.0},
                "opacity": {"mode": "constant", "value": 1.0}
            });
        },
        "moved",
    );
    // Overlapping clips (on two tracks, or in a transition, which needs them to overlap) are a
    // composite.
    refused(
        &|value| {
            value["timeline"]["tracks"]
                .as_array_mut()
                .expect("tracks")
                .push(
                    json!({"id": "v2", "name": "V2", "kind": "video", "clips": [{
                        "id": "next", "source_id": "video",
                        "timeline_range": {"start": 4 * SECOND, "end": 7 * SECOND},
                        "source_range": {"start": 0, "end": 3 * SECOND}
                    }]}),
                );
        },
        "both visible at once",
    );
}
