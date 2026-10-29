//! Decode planning: cursors, backward runs, frame selection, windows, shards, and budgets.

use super::*;

/// One export frame's needs: `(clip, source time)` pairs on `video-source`.
fn frame_needs(index: u64, needs: &[(&str, i64)]) -> ExportFrameNeeds {
    let source = SourceId::new("video-source").expect("source ID");
    ExportFrameNeeds {
        index,
        time: MediaTime::from_ticks(i64::try_from(index).expect("index fits") * 4_000),
        needs: needs
            .iter()
            .map(|(clip, ticks)| FrameNeed {
                reader: None,
                clip_id: ClipId::new(*clip).expect("clip ID"),
                source_id: source.clone(),
                media_time: MediaTime::from_ticks(*ticks),
            })
            .collect(),
    }
}

#[test]
fn a_backward_run_lasts_while_its_cursor_does_not_move_forward() {
    let frames = vec![
        frame_needs(5, &[("a", 20_000), ("b", 0)]),
        frame_needs(6, &[("a", 16_000), ("b", 4_000)]),
        frame_needs(7, &[("a", 12_000)]),
        // A slow reverse asks for one time twice; it is one target.
        frame_needs(8, &[("a", 12_000)]),
        frame_needs(9, &[("a", 8_000)]),
        // A ping-pong turn ends the run.
        frame_needs(10, &[("a", 12_000)]),
    ];
    assert_eq!(
        backward_run(&frames, 5, &cursor_of("a")),
        BackwardRun {
            times: [20_000, 16_000, 12_000, 8_000]
                .map(MediaTime::from_ticks)
                .to_vec(),
            until: 9,
        }
    );
    assert_eq!(
        ticks(&backward_run(&frames, 7, &cursor_of("a")).times),
        [12_000, 8_000]
    );
    // A cursor a frame does not ask for has stopped: its clip ended.
    assert_eq!(backward_run(&frames, 6, &cursor_of("b")).until, 6);
}

#[test]
fn a_decoded_frame_set_from_another_generation_is_refused() {
    let project: Project = serde_json::from_str(include_str!(
        "../../../../tests/cases/host-video-export-project.json"
    ))
    .expect("video export project");
    let set = |generation, index| crate::decode::DecodedFrameSet {
        generation,
        index,
        time: MediaTime::ZERO,
        frames: Vec::new(),
        stills: Vec::new(),
        bytes: 0,
        decode_time: Duration::ZERO,
        stage_timings: alpha2_native::DecodeStageTimings::default(),
        reverse_cache_hits: 0,
        reverse_windows_built: 0,
        reverse_cache_bytes: 0,
        software_decoded_frames: 0,
        hardware_decoded_frames: 0,
        hardware_fallbacks: HardwareFallbackCounts::default(),
        decoder_session_reopens: 0,
    };
    let mut scheduler = DecodeScheduler::new(2, &project);
    scheduler.generation = 3;
    scheduler.outstanding = 2;
    let stale = scheduler
        .accept(&set(1, 0), 0)
        .expect_err("generation 1 is stale");
    assert!(stale.contains("generation 1"), "{stale}");
    assert!(scheduler.accept(&set(3, 0), 0).is_ok());
    assert!(
        scheduler.accept(&set(3, 2), 1).is_err(),
        "a frame out of order"
    );
}

/// The decode pool asks for a still each time it comes into the
/// picture, once however many clips show it, and not while the frame before
/// showed it too: the host holds exactly the previous frame's stills.
#[test]
fn a_still_is_decoded_each_time_it_comes_into_the_picture() {
    let kinds = HashMap::from([
        (SourceId::new("a").expect("id"), MediaKind::Image),
        (SourceId::new("b").expect("id"), MediaKind::Image),
        (SourceId::new("v").expect("id"), MediaKind::Video),
    ]);
    let need = |source: &str, clip: &str| alpha2_eval::FrameNeed {
        source_id: SourceId::new(source).expect("id"),
        clip_id: ClipId::new(clip).expect("id"),
        media_time: MediaTime::ZERO,
        reader: None,
    };
    let names = |needs: &[alpha2_eval::FrameNeed]| {
        needs
            .iter()
            .map(|need| format!("{}/{}", need.source_id.as_str(), need.clip_id.as_str()))
            .collect::<Vec<_>>()
    };
    let ids = |stills: &[SourceId]| {
        stills
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect::<Vec<_>>()
    };
    let frame = vec![
        need("b", "1"),
        need("a", "2"),
        need("a", "3"),
        need("v", "4"),
    ];
    let (stills, asked) = crate::decode::still_needs_for_job(frame.clone(), &kinds, None);
    assert_eq!(ids(&stills), ["a", "b"]);
    assert_eq!(names(&asked), ["b/1", "a/2", "v/4"]);
    let held = [SourceId::new("a").expect("id")];
    let (stills, asked) = crate::decode::still_needs_for_job(frame, &kinds, Some(&held));
    assert_eq!(ids(&stills), ["a", "b"]);
    assert_eq!(names(&asked), ["b/1", "v/4"]);
}

#[test]
fn backward_runs_are_planned_once_and_single_steps_are_left_to_seeks() {
    let project: Project = serde_json::from_str(include_str!(
        "../../../../tests/cases/host-video-export-project.json"
    ))
    .expect("video export project");
    // Clip `a` plays reversed from frame 0; clip `b` wraps a loop at frame 2.
    let horizon = (0..6)
        .zip([8_000, 12_000, 0, 4_000, 8_000, 12_000])
        .map(|(index, loop_time)| {
            frame_needs(
                index,
                &[
                    (
                        "a",
                        40_000 - i64::try_from(index).expect("index fits") * 4_000,
                    ),
                    ("b", loop_time),
                ],
            )
        })
        .collect::<Vec<_>>();
    let mut scheduler = DecodeScheduler::new(2, &project);
    let mut evaluations = 0;
    let mut plan = |scheduler: &mut DecodeScheduler,
                    position: usize,
                    horizon_cache: &mut Option<Vec<ExportFrameNeeds>>| {
        let runs = scheduler
            .plan_backward_runs(&horizon[..], position, horizon_cache, || {
                evaluations += 1;
                Ok(horizon.clone())
            })
            .expect("plans");
        for need in &horizon[position].needs {
            scheduler
                .last_cursor_times
                .insert(crate::decode::decode_cursor_key(need), need.media_time);
        }
        runs
    };
    let mut cache = None;
    let first = plan(&mut scheduler, 0, &mut cache);
    assert_eq!(
        ticks(&first[&cursor_of("a")]),
        [40_000, 36_000, 32_000, 28_000, 24_000, 20_000]
    );
    assert!(!first.contains_key(&cursor_of("b")));
    assert_eq!(scheduler.planned_runs[&cursor_of("a")], 5);
    assert!(plan(&mut scheduler, 1, &mut cache).is_empty());
    // A loop's wrap is one step back and then forward again, from the pass's last frame as from the
    // wrap: a seek, not a run.
    assert!(plan(&mut scheduler, 2, &mut cache).is_empty());
    assert!(!scheduler.planned_runs.contains_key(&cursor_of("b")));
    assert_eq!(evaluations, 1, "the horizon is evaluated once per refill");
}

#[test]
fn shared_source_clips_keep_independent_decoder_cursors() {
    let project: Project = serde_json::from_str(include_str!(
        "../../../../tests/cases/host-video-export-project.json"
    ))
    .expect("video export project");
    let source = SourceId::new("video-source").expect("source ID");
    let first_clip = ClipId::new("clip-a").expect("clip ID");
    let second_clip = ClipId::new("clip-b").expect("clip ID");
    let assets = HashMap::from([(source.as_str().to_owned(), corpus("long-gop.mp4"))]);
    let mut state = ExportDecodeState::new(&project, &assets).expect("decode state");
    let cancel = CancelToken::default();

    for (index, (first_time, second_time)) in [
        (MediaTime::ZERO, MediaTime::from_ticks(60_000)),
        (MediaTime::from_ticks(4_000), MediaTime::from_ticks(64_000)),
    ]
    .into_iter()
    .enumerate()
    {
        let decoded = state
            .decode_job(
                DecodeJob {
                    generation: 0,
                    index: u64::try_from(index).expect("index fits"),
                    time: first_time,
                    needs: vec![
                        FrameNeed {
                            reader: None,
                            clip_id: first_clip.clone(),
                            source_id: source.clone(),
                            media_time: first_time,
                        },
                        FrameNeed {
                            reader: None,
                            clip_id: second_clip.clone(),
                            source_id: source.clone(),
                            media_time: second_time,
                        },
                    ],
                    reverse_runs: HashMap::new(),
                },
                &cancel,
            )
            .expect("both clip cursors decode");
        assert_eq!(decoded.frames.len(), 2);
        for frame in &decoded.frames {
            let clip = if frame.requested == first_time {
                &first_clip
            } else {
                &second_clip
            };
            let cached = &state
                .decoded
                .get(&(source.clone(), clip.clone(), None))
                .expect("cursor retains decoded frame")
                .frame;
            assert!(Arc::ptr_eq(&frame.frame, cached));
        }
        let backing = Arc::clone(&decoded.frames[0].frame);
        let aliases = [
            DecodedSourceFrame {
                source: source.clone(),
                clip: first_clip.clone(),
                requested: first_time,
                kind: MediaKind::Video,
                frame: Arc::clone(&backing),
            },
            DecodedSourceFrame {
                source: source.clone(),
                clip: first_clip.clone(),
                requested: first_time,
                kind: MediaKind::Video,
                frame: Arc::clone(&backing),
            },
        ];
        assert_eq!(
            unique_decoded_frame_bytes(&aliases),
            backing.retained_bytes()
        );
        assert_eq!(state.decoders.len(), 2);
        assert_eq!(state.decoded.len(), 2);
    }
    assert_eq!(
        state
            .decoded
            .get(&(source.clone(), first_clip, None))
            .expect("first cursor")
            .requested,
        MediaTime::from_ticks(4_000)
    );
    assert_eq!(
        state
            .decoded
            .get(&(source, second_clip, None))
            .expect("second cursor")
            .requested,
        MediaTime::from_ticks(64_000)
    );
}

#[test]
fn repeated_requests_retain_one_physical_decoded_frame() {
    let project: Project = serde_json::from_str(include_str!(
        "../../../../tests/cases/host-video-export-project.json"
    ))
    .expect("video export project");
    let source = SourceId::new("video-source").expect("source ID");
    let clip = ClipId::new("clip-a").expect("clip ID");
    let assets = HashMap::from([(source.as_str().to_owned(), corpus("long-gop.mp4"))]);
    let mut state = ExportDecodeState::new(&project, &assets).expect("decode state");
    let cancel = CancelToken::default();
    let first_request = MediaTime::from_ticks(2_000);
    let first = state
        .decode_need(
            &FrameNeed {
                reader: None,
                clip_id: clip.clone(),
                source_id: source.clone(),
                media_time: first_request,
            },
            &cancel,
        )
        .expect("first request decodes")
        .expect("video frame exists");
    // Between two 30 fps frames: the one presented there is the one before.
    assert_eq!(first.frame.pts(), MediaTime::ZERO);

    // A later request the same frame is still presented at.
    let alias = state
        .decode_need(
            &FrameNeed {
                reader: None,
                clip_id: clip,
                source_id: source,
                media_time: MediaTime::from_ticks(3_000),
            },
            &cancel,
        )
        .expect("alias request decodes")
        .expect("aliased video frame exists");
    assert_ne!(first.requested, alias.requested);
    assert_eq!(first.frame.pts(), alias.frame.pts());
    assert!(Arc::ptr_eq(&first.frame, &alias.frame));
}

#[test]
fn installed_frames_are_found_by_a_borrowed_source_and_kept_only_while_asked_for() {
    let video = SourceId::new("video").expect("source ID");
    let other = SourceId::new("other").expect("source ID");
    let at = MediaTime::from_ticks;
    let mut frames = crate::frames::RequestedFrames::default();
    frames.insert(&video, at(0), "video 0");
    frames.insert(&video, at(4_000), "video 4000");
    frames.insert(&other, at(0), "other 0");
    frames.insert(&video, at(0), "video 0 again");
    assert_eq!(frames.get(&video, at(0)), Some(&"video 0 again"));
    assert_eq!(frames.get(&video, at(4_000)), Some(&"video 4000"));
    assert_eq!(frames.get(&other, at(4_000)), None);
    assert!(frames.contains(&other, at(0)));
    assert_eq!(frames.len(), 3);

    frames.retain_needed(&HashSet::from([(&video, at(4_000))]));
    assert_eq!(frames.len(), 1);
    assert_eq!(frames.get(&video, at(4_000)), Some(&"video 4000"));
    assert!(!frames.contains(&video, at(0)));
    assert!(!frames.contains(&other, at(0)));
    frames.retain_needed(&HashSet::new());
    assert_eq!(frames.len(), 0);
    frames.insert(&other, at(0), "other back");
    assert_eq!(frames.get(&other, at(0)), Some(&"other back"));
    frames.clear();
    assert_eq!(frames.len(), 0);
}

#[test]
fn sequential_requests_get_the_frame_presented_at_each_time() {
    // vfr.mp4 presents frames at 0, 4000, 12000, 24000 and 40000 ticks.
    let project: Project = serde_json::from_str(include_str!(
        "../../../../tests/cases/host-video-export-project.json"
    ))
    .expect("video export project");
    let source = SourceId::new("video-source").expect("source ID");
    let clip = ClipId::new("clip-a").expect("clip ID");
    let assets = HashMap::from([(source.as_str().to_owned(), corpus("vfr.mp4"))]);
    let mut state = ExportDecodeState::new(&project, &assets).expect("decode state");
    let cancel = CancelToken::default();
    let presented: Vec<i64> = [2_000, 6_000, 12_000, 20_000, 30_000, 43_000]
        .into_iter()
        .map(|ticks| {
            state
                .decode_need(
                    &FrameNeed {
                        reader: None,
                        clip_id: clip.clone(),
                        source_id: source.clone(),
                        media_time: MediaTime::from_ticks(ticks),
                    },
                    &cancel,
                )
                .expect("decodes")
                .expect("a video frame")
                .frame
                .pts()
                .ticks()
        })
        .collect();
    assert_eq!(presented, [0, 4_000, 12_000, 12_000, 24_000, 40_000]);
}

/// The frame times a linear decode of `name` presents.
fn linear_frame_ticks(name: &str) -> Vec<i64> {
    let cancel = CancelToken::default();
    let mut decoder = alpha2_native::NativeDecoder::open(corpus(name)).expect("linear decoder");
    let mut presented = Vec::new();
    loop {
        match decoder.decode_next(&cancel).expect("linear decode") {
            alpha2_media::DecodeEvent::Video(frame) => presented.push(frame.pts.ticks()),
            alpha2_media::DecodeEvent::EndOfStream => break,
            event => panic!("unexpected {event:?}"),
        }
    }
    presented
}

/// The source frame (its index in a linear decode, `frames`) the export
/// decode shows at each of `times`, decoding forward from the first, and
/// seeking to each from the last back, which must agree.
fn shown_source_frames(name: &str, frames: &[i64], times: &[MediaTime]) -> Vec<usize> {
    let project: Project = serde_json::from_str(include_str!(
        "../../../../tests/cases/host-video-export-project.json"
    ))
    .expect("video export project");
    let assets = HashMap::from([("video-source".to_owned(), corpus(name))]);
    let shown = |state: &mut ExportDecodeState, time: MediaTime| {
        let need = FrameNeed {
            reader: None,
            clip_id: ClipId::new("clip-a").expect("clip ID"),
            source_id: SourceId::new("video-source").expect("source ID"),
            media_time: time,
        };
        let pts = state
            .decode_need(&need, &CancelToken::default())
            .expect("decodes")
            .expect("a video frame")
            .frame
            .pts()
            .ticks();
        frames
            .iter()
            .position(|&frame| frame == pts)
            .expect("a source frame")
    };
    let mut sequential = ExportDecodeState::new(&project, &assets).expect("decode state");
    let in_order = times
        .iter()
        .map(|&time| shown(&mut sequential, time))
        .collect::<Vec<_>>();
    let mut backward = ExportDecodeState::new(&project, &assets).expect("decode state");
    let mut sought = times
        .iter()
        .rev()
        .map(|&time| shown(&mut backward, time))
        .collect::<Vec<_>>();
    sought.reverse();
    assert_eq!(in_order, sought, "{name}: decoded in order and sought");
    in_order
}

/// A source whose frame times fall between ticks, or that its container rounds
/// to milliseconds, placed one to one at its own rate shows each frame once.
/// The frame a project frame asks for at its floored start may carry a
/// timestamp a few ticks after it, and without slack some frames show twice and
/// others are skipped. So may one whose time base is finer than the rounding
/// its times went through (an MP4 that kept a Matroska file's milliseconds in
/// 1/16000: 22 of 60 frames shown twice) or coarser than a millisecond (1/600,
/// 1/100): the slack comes from the time base alone, at most 61 ticks. Checked
/// for sequential export decode and for a seek to each frame.
#[test]
fn a_source_at_the_project_rate_shows_each_of_its_frames_once() {
    for (name, rate, count) in [
        ("frame-index-90fps.mov", (90, 1), 30),
        ("frame-index-2997.mkv", (30_000, 1_001), 30),
        ("frame-rounding-2997.mkv", (30_000, 1_001), 60),
        ("frame-rounding-2997-remux.mp4", (30_000, 1_001), 60),
        ("frame-rounding-2997-600.mp4", (30_000, 1_001), 60),
        ("frame-rounding-2997-100.mp4", (30_000, 1_001), 60),
        ("frame-rounding-30-600.mp4", (30, 1), 60),
        ("frame-rounding-30-gop32.mkv", (30, 1), 64),
    ] {
        let rate = alpha2_time::FrameRate::new(rate.0, rate.1).expect("rate");
        let frames = linear_frame_ticks(name);
        assert_eq!(frames.len(), count, "{name}");
        let times = (0..i64::try_from(count).expect("count"))
            .map(|index| rate.frame_start(index).expect("frame start"))
            .collect::<Vec<_>>();
        assert_eq!(
            shown_source_frames(name, &frames, &times),
            (0..count).collect::<Vec<_>>(),
            "{name}"
        );
    }
}

/// How late each `frame-rounding-*` source's frames may be: the rounding its
/// first timestamps show, else its time base's half unit (at most 61 ticks).
#[test]
fn a_source_is_given_the_slack_its_timestamps_show() {
    for (name, slack) in [
        ("frame-rounding-2997.mkv", 61),
        ("frame-rounding-2997-remux.mp4", 61),
        ("frame-rounding-2997-600.mp4", 101),
        ("frame-rounding-2997-100.mp4", 601),
        // Exact frames: 30 fps in 1/600 and 60 and 30 fps mixed in 1/90000 keep the time base's
        // slack, so a frame that starts later than a time is not shown at it.
        ("frame-rounding-30-600.mp4", 61),
        ("frame-rounding-vfr-90000.mp4", 2),
        ("frame-rounding-vfr-ms.mkv", 61),
        // Only the first 64 frames are read, and they are exact.
        ("frame-rounding-joined.mp4", 2),
        ("frame-rounding-30-gop32.mkv", 61),
    ] {
        let decoder = alpha2_native::NativeDecoder::open(corpus(name)).expect("decoder");
        assert_eq!(
            decoder.presentation_deadline(MediaTime::ZERO),
            MediaTime::from_ticks(slack),
            "{name}"
        );
    }
}

/// The source frames the export decode shows for each `frame-rounding-*`
/// source at five project rates: `tests/golden/frame-selection-corpus.json`, which
/// the browser package's decoders run (`packages/alpha2/test/frame-selection.test.mjs`)
/// so they pick the frames a native export does. `ALPHA2_UPDATE_GOLDENS=1`
/// rewrites it.
#[test]
fn the_frame_selection_table_is_the_native_exports() {
    const RATES: [(u32, u32); 5] = [(24, 1), (25, 1), (30_000, 1_001), (30, 1), (60, 1)];
    let sources = [
        "frame-rounding-2997.mkv",
        "frame-rounding-2997-remux.mp4",
        "frame-rounding-2997-600.mp4",
        "frame-rounding-2997-100.mp4",
        "frame-rounding-30-600.mp4",
        "frame-rounding-vfr-90000.mp4",
        "frame-rounding-vfr-ms.mkv",
        "frame-rounding-joined.mp4",
        "frame-rounding-30-gop32.mkv",
        "frame-rounding-23976.mkv",
        "frame-rounding-23976-remux.mp4",
        "frame-rounding-23976.ts",
        "frame-rounding-60.webm",
    ]
    .into_iter()
    .map(|name| {
        let frames = linear_frame_ticks(name);
        let last = *frames.last().expect("frames");
        let slack = alpha2_native::NativeDecoder::open(corpus(name))
            .expect("decoder")
            .presentation_deadline(MediaTime::ZERO)
            .ticks();
        let rates = RATES
            .into_iter()
            .map(|(num, den)| {
                let rate = alpha2_time::FrameRate::new(num, den).expect("rate");
                let mut times = Vec::new();
                while let Some(time) = rate
                    .frame_start(i64::try_from(times.len()).expect("frames"))
                    .filter(|time| time.ticks() <= last)
                {
                    times.push(time);
                }
                let shown = shown_source_frames(name, &frames, &times);
                for (time, &index) in times.iter().zip(&shown) {
                    let expected = frames
                        .iter()
                        .rposition(|&frame| frame <= time.ticks() + slack)
                        .unwrap_or(0);
                    assert_eq!(index, expected, "{name} at {num}/{den}, {time}");
                }
                json!({ "rate": { "num": num, "den": den }, "frames": shown })
            })
            .collect::<Vec<_>>();
        json!({ "file": name, "slackTicks": slack, "frameTicks": frames, "rates": rates })
    })
    .collect::<Vec<_>>();
    let table = json!({
        "description": "The source frame (an index into frameTicks, a linear decode's frame times) a native export shows at each project frame of five rates, for sources in tests/corpus whose time bases round their frames or hold them exactly, and the presentation slack each is given. Generated by bindings/alpha2-host/src/tests.rs (ALPHA2_UPDATE_GOLDENS=1 rewrites it); packages/alpha2/test/frame-selection.test.mjs runs the browser decoders against it.",
        "sources": sources,
    });
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(&table).expect("serializes")
    );
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/frame-selection-corpus.json");
    if env::var_os("ALPHA2_UPDATE_GOLDENS").as_deref() == Some(std::ffi::OsStr::new("1")) {
        std::fs::write(&path, &text).expect("writes the table");
        return;
    }
    let pinned = std::fs::read_to_string(&path).expect("tests/golden/frame-selection-corpus.json");
    assert!(
        pinned == text,
        "tests/golden/frame-selection-corpus.json differs from the native export's frames; rerun with ALPHA2_UPDATE_GOLDENS=1 after an intended change"
    );
}

/// A cursor that jumps more than two seconds ahead seeks to the keyframe before
/// its target instead of decoding every frame between, as the browser's
/// adapters do; otherwise a sped-up clip decodes its source's every frame. The
/// frame presented is the same either way.
#[test]
fn a_long_jump_forward_seeks_instead_of_decoding_every_frame_between() {
    // vfr-rising.mp4: 30 fps with a keyframe each second for 6 s, then 60 fps.
    let project: Project = serde_json::from_str(include_str!(
        "../../../../tests/cases/host-video-export-project.json"
    ))
    .expect("video export project");
    let assets = HashMap::from([("video-source".to_owned(), corpus("vfr-rising.mp4"))]);
    let mut state = ExportDecodeState::new(&project, &assets).expect("decode state");
    let cancel = CancelToken::default();
    let decode = |state: &mut ExportDecodeState, ticks: i64| {
        let before = state.software_decoded_frames;
        let frame = state
            .decode_need(
                &FrameNeed {
                    reader: None,
                    clip_id: ClipId::new("clip-a").expect("clip ID"),
                    source_id: SourceId::new("video-source").expect("source ID"),
                    media_time: MediaTime::from_ticks(ticks),
                },
                &cancel,
            )
            .expect("decodes")
            .expect("a video frame");
        (
            frame.frame.pts().ticks(),
            state.software_decoded_frames - before,
        )
    };
    decode(&mut state, 0);
    assert_eq!(decode(&mut state, 240_000), (240_000, 60));
    // Three seconds on (5.1 s) seeks to the keyframe at 5 s.
    let (pts, decoded) = decode(&mut state, 612_000);
    assert_eq!(pts, 612_000);
    assert!(decoded <= 4, "{decoded} frames decoded for a 3 s jump");
    assert!(crate::decode::decodes_on(
        MediaTime::ZERO,
        crate::decode::MAX_SEQUENTIAL_DECODE_GAP
    ));
    assert!(!crate::decode::decodes_on(
        MediaTime::ZERO,
        MediaTime::from_ticks(crate::decode::MAX_SEQUENTIAL_DECODE_GAP.ticks() + 1)
    ));
}

/// A decode state for long-gop.mp4 (33 frames at 30 fps, keyframes at 0 and
/// 30) whose reverse cache holds `frames` of its decoded frames.
fn long_gop_state(frames: u64) -> ExportDecodeState {
    let project: Project = serde_json::from_str(include_str!(
        "../../../../tests/cases/host-video-export-project.json"
    ))
    .expect("video export project");
    let assets = HashMap::from([("video-source".to_owned(), corpus("long-gop.mp4"))]);
    let mut state = ExportDecodeState::new(&project, &assets).expect("decode state");
    presented(&mut state, "probe", 0);
    let frame_bytes = state.decoded[&cursor_of("probe")].frame.retained_bytes();
    state.max_reverse_cache_bytes = frames * frame_bytes;
    state
}

/// The job that starts a backward run of `clip` over `times`.
fn run_job(clip: &str, times: &[i64]) -> DecodeJob {
    let (source, clip_id, _) = cursor_of(clip);
    DecodeJob {
        generation: 0,
        index: 0,
        time: MediaTime::ZERO,
        needs: vec![FrameNeed {
            reader: None,
            clip_id,
            source_id: source,
            media_time: MediaTime::from_ticks(times[0]),
        }],
        reverse_runs: HashMap::from([(
            cursor_of(clip),
            times.iter().copied().map(MediaTime::from_ticks).collect(),
        )]),
    }
}

#[test]
fn a_backward_run_is_decoded_a_window_at_a_time() {
    let mut state = long_gop_state(10);
    let times = (0..29).rev().map(|frame| frame * 4_000).collect::<Vec<_>>();
    let first = state
        .decode_job(run_job("reverse", &times), &CancelToken::default())
        .expect("the run's first frame decodes");
    assert_eq!(first.frames[0].frame.pts().ticks(), times[0]);
    assert_eq!(first.reverse_windows_built, 1);
    for &time in &times[1..] {
        assert_eq!(presented(&mut state, "reverse", time), time);
        assert!(state.reverse_cache_bytes <= state.max_reverse_cache_bytes);
    }
    // 29 frames through a 10-frame cache: three windows, each decoded forward once, not a window
    // rebuilt by a seek per frame.
    assert_eq!(state.reverse_windows_built, 3);
    assert_eq!(state.reverse_cache_hits, 29);
}

#[test]
fn a_forward_turn_reuses_the_window_and_then_releases_it() {
    let mut state = long_gop_state(10);
    let times = (0..12).rev().map(|frame| frame * 4_000).collect::<Vec<_>>();
    state
        .decode_job(run_job("pingpong", &times), &CancelToken::default())
        .expect("the run starts");
    for &time in &times[1..] {
        presented(&mut state, "pingpong", time);
    }
    let hits = state.reverse_cache_hits;
    assert_eq!(presented(&mut state, "pingpong", 4_000), 4_000);
    assert_eq!(state.reverse_cache_hits, hits + 1);
    assert_eq!(presented(&mut state, "pingpong", 48_000), 48_000);
    assert_eq!(state.reverse_cache_bytes, 0);
    assert!(state.reverse_windows.is_empty());
    assert!(state.reverse_plans.is_empty());
}

#[test]
fn past_the_last_frame_the_last_frame_is_presented() {
    // long-gop.mp4 ends at 1.1 s (last frame at 128 000 ticks). A clip running past the media holds
    // that picture instead of failing the export.
    let mut state = long_gop_state(10);
    for ticks in [128_000, 131_999, 132_000, 600_000] {
        assert_eq!(presented(&mut state, "forward", ticks), 128_000);
    }
    let times = [240_000, 200_000, 132_000, 128_000, 124_000];
    state
        .decode_job(run_job("reverse", &times), &CancelToken::default())
        .expect("a run past the end decodes");
    for (&time, expected) in times[1..].iter().zip([128_000, 128_000, 128_000, 124_000]) {
        assert_eq!(presented(&mut state, "reverse", time), expected);
    }
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwdecode"
))]
#[test]
fn a_run_that_requires_hardware_is_windowed_to_the_surface_cap() {
    let times = (0..6).rev().map(|frame| frame * 4_000).collect::<Vec<_>>();
    let mut state = long_gop_state(10);
    state.hardware_required = true;
    state.reverse_hardware_surfaces = 2;
    state
        .decode_job(run_job("reverse", &times), &CancelToken::default())
        .expect("a windowed run decodes");
    for &time in &times[1..] {
        assert_eq!(presented(&mut state, "reverse", time), time);
    }
    assert_eq!(state.reverse_windows_built, 3, "six frames, two a window");

    // With no surfaces a strict run has no window to use, and says so rather than decoding in
    // software.
    let mut state = long_gop_state(10);
    state.hardware_required = true;
    state.reverse_hardware_surfaces = 0;
    let error = state
        .decode_job(run_job("reverse", &times), &CancelToken::default())
        .err()
        .expect("a strict run without surfaces fails");
    assert!(error.contains("ALPHA2_REVERSE_DECODE_SURFACES"), "{error}");
}

#[test]
fn a_reverse_window_falls_back_to_software_instead_of_failing_the_export() {
    // An unimportable hardware surface in a reverse window must degrade like the forward path: a
    // frame set is produced, the reason recorded, and the source pinned to software.
    let mut state = long_gop_state(10);
    state.reverse_window_unsupported_after = Some(1);
    let decoded = state
        .decode_job(
            run_job("reverse", &[8_000, 4_000, 0]),
            &CancelToken::default(),
        )
        .expect("an unimportable surface degrades instead of aborting");

    assert_eq!(decoded.frames[0].frame.pts().ticks(), 8_000);
    assert_eq!(decoded.reverse_windows_built, 1);
    assert_eq!(
        decoded
            .hardware_fallbacks
            .entries()
            .find(|(reason, _)| *reason == "unsupportedSurfaceFormat")
            .map(|(_, count)| count),
        Some(1),
        "the typed reason is recorded, not swallowed"
    );
    assert!(
        state
            .software_only
            .contains(&SourceId::new("video-source").expect("source ID")),
        "the source is pinned to software so later cursors do not retry it"
    );
    assert_eq!(presented(&mut state, "reverse", 4_000), 4_000);
    assert_eq!(state.reverse_cache_hits, 2);
}

/// A shard past its share of sessions evicts the least recently used and
/// opens it again when it is read again, which answers the frame presented
/// at each time as before; a backward cursor evicted mid-run gives its
/// window's bytes back and builds the window again. No other test runs
/// this path.
#[test]
fn a_shard_past_its_sessions_evicts_the_least_recently_used_and_reopens_it() {
    let project: Project = serde_json::from_str(include_str!(
        "../../../../tests/cases/host-video-export-project.json"
    ))
    .expect("video export project");
    let assets = HashMap::from([("video-source".to_owned(), corpus("long-gop.mp4"))]);
    let mut state = ExportDecodeState::new_with_limits(&project, &assets, 1, 2, 128 << 20)
        .expect("decode state");
    // long-gop.mp4 presents a frame every 4 000 ticks; three cursors take turns on two sessions.
    for frame in 0..6 {
        for clip in ["a", "b", "c"] {
            let ticks = frame * 4_000;
            assert_eq!(
                presented(&mut state, clip, ticks),
                ticks,
                "{clip} at {ticks}"
            );
            assert!(state.decoders.len() <= 2);
            assert_eq!(state.session_use.len(), state.decoders.len());
            assert!(
                state
                    .decoded
                    .keys()
                    .all(|cursor| state.decoders.contains_key(cursor))
            );
        }
    }

    let mut state = long_gop_state(10);
    state.max_sessions = 2;
    let times = (0..20).rev().map(|frame| frame * 4_000).collect::<Vec<_>>();
    state
        .decode_job(run_job("reverse", &times), &CancelToken::default())
        .expect("the run's first frame decodes");
    assert!(state.reverse_cache_bytes > 0);
    presented(&mut state, "x", 0);
    presented(&mut state, "y", 0);
    assert!(!state.decoders.contains_key(&cursor_of("reverse")));
    assert_eq!(state.reverse_cache_bytes, 0);
    assert!(state.reverse_windows.is_empty());
    for &time in &times[1..] {
        assert_eq!(presented(&mut state, "reverse", time), time);
        assert!(state.decoders.len() <= 2);
        assert!(state.reverse_cache_bytes <= state.max_reverse_cache_bytes);
        let held = state
            .reverse_windows
            .values()
            .map(|window| window.bytes)
            .sum::<u64>();
        assert_eq!(state.reverse_cache_bytes, held);
    }
}

/// A decode state over the corpus's 61-frame H.264 source, which presents a
/// frame every 4 000 ticks, holding at most `sessions` sessions.
fn copy_source_state(sessions: usize) -> ExportDecodeState {
    let project: Project = serde_json::from_str(include_str!(
        "../../../../tests/cases/host-video-export-project.json"
    ))
    .expect("video export project");
    let assets = HashMap::from([("video-source".to_owned(), corpus("h264-bt709-copy.mp4"))]);
    ExportDecodeState::new_with_limits(&project, &assets, 1, sessions, 128 << 20)
        .expect("decode state")
}

/// The job of output frame `frame`, reading `clips` at the same time.
fn frame_job(frame: u64, clips: &[&str]) -> DecodeJob {
    let time = MediaTime::from_ticks(i64::try_from(frame).expect("frame") * 4_000);
    DecodeJob {
        generation: 0,
        index: frame,
        time,
        needs: clips
            .iter()
            .map(|clip| {
                let (source_id, clip_id, reader) = cursor_of(clip);
                FrameNeed {
                    reader,
                    clip_id,
                    source_id,
                    media_time: time,
                }
            })
            .collect(),
        reverse_runs: HashMap::new(),
    }
}

/// Each decoded picture's planes, by clip.
fn pictures(set: &crate::decode::DecodedFrameSet) -> Vec<(String, Vec<u8>)> {
    set.frames
        .iter()
        .map(|frame| {
            let alpha2_native::NativeVideoFrame::Cpu(picture) = &*frame.frame else {
                panic!("a software session decodes on the CPU");
            };
            let bytes = picture
                .planes
                .iter()
                .flat_map(|plane| plane.bytes.iter().copied())
                .collect();
            (frame.clip.as_str().to_owned(), bytes)
        })
        .collect()
}

/// One more cursor than sessions, read in one order every frame: the shard
/// closes the one read next the latest, so a frame reopens at most two
/// sessions (least recently used, it would reopen every one), and every
/// picture is the one a shard with room for all of them decodes.
#[test]
fn a_shard_short_of_one_session_reopens_at_most_two_a_frame_and_decodes_the_same_pictures() {
    const SESSIONS: usize = 3;
    let clips = ["a", "b", "c", "d"];
    let mut short = copy_source_state(SESSIONS);
    let mut roomy = copy_source_state(clips.len());
    let cancel = CancelToken::default();
    let mut reopened = Vec::new();
    for frame in 0..30 {
        let decoded = short
            .decode_job(frame_job(frame, &clips), &cancel)
            .expect("decodes short");
        let expected = roomy
            .decode_job(frame_job(frame, &clips), &cancel)
            .expect("decodes with room");
        assert_eq!(pictures(&decoded), pictures(&expected), "frame {frame}");
        assert!(short.decoders.len() <= SESSIONS);
        reopened.push(decoded.decoder_session_reopens);
        // With a session for every cursor, nothing opens twice.
        assert_eq!(expected.decoder_session_reopens, 0, "frame {frame}");
    }
    assert_eq!(reopened[0], 0, "the first frame opens each cursor once");
    assert!(
        reopened[1..].iter().all(|count| *count <= 2),
        "{reopened:?}"
    );
    assert!(reopened[1..].iter().any(|count| *count > 0), "{reopened:?}");
    assert_eq!(short.session_reopens, reopened.iter().sum::<u64>());
}

/// A cursor the timeline reads no more is closed after the first frame that
/// does not read it, and its plan goes with it.
#[test]
fn a_cursor_past_its_last_read_is_closed() {
    let mut state = copy_source_state(8);
    let span =
        |end: i64| TimeRange::new(MediaTime::ZERO, MediaTime::from_ticks(end)).expect("span");
    let read = |clip: &str, end: i64| {
        let (source_id, clip_id, reader) = cursor_of(clip);
        alpha2_eval::SourceRead {
            source_id,
            clip_id,
            reader,
            span: span(end),
        }
    };
    state.reads = Arc::new(crate::decode::CursorReads::new(vec![
        read("short", 3 * 4_000),
        read("long", 60 * 4_000),
    ]));
    let cancel = CancelToken::default();
    for frame in 0..6 {
        let clips: &[&str] = if frame < 3 {
            &["long", "short"]
        } else {
            &["long"]
        };
        state
            .decode_job(frame_job(frame, clips), &cancel)
            .expect("decodes");
        assert_eq!(
            state.decoders.contains_key(&cursor_of("short")),
            frame < 3,
            "frame {frame}"
        );
        assert!(state.decoders.contains_key(&cursor_of("long")));
        assert_eq!(state.decoded.len(), state.decoders.len());
    }
    assert_eq!(state.session_reopens, 0);
}

/// A backward cursor whose session another cursor takes builds its window
/// again from what its run has left, at the same size: the cache gives the
/// bytes back when the window goes, and holds as much again after.
#[test]
fn an_evicted_backward_cursor_builds_its_window_again_from_the_rest_of_its_run() {
    let mut state = long_gop_state(10);
    state.max_sessions = 2;
    let cancel = CancelToken::default();
    let times = (0..20).rev().map(|frame| frame * 4_000).collect::<Vec<_>>();
    state
        .decode_job(run_job("reverse", &times), &cancel)
        .expect("the run's first frame decodes");
    let window_bytes = state.reverse_cache_bytes;
    assert!(window_bytes > 0);
    let mut others = frame_job(1, &["x", "y"]);
    for need in &mut others.needs {
        need.media_time = MediaTime::ZERO;
    }
    state.decode_job(others, &cancel).expect("decodes x and y");
    assert!(!state.decoders.contains_key(&cursor_of("reverse")));
    assert_eq!(state.reverse_cache_bytes, 0);
    assert!(state.reverse_plans.contains_key(&cursor_of("reverse")));
    let mut next = frame_job(2, &["reverse"]);
    next.needs[0].media_time = MediaTime::from_ticks(times[1]);
    let decoded = state.decode_job(next, &cancel).expect("decodes on");
    assert_eq!(decoded.frames[0].frame.pts().ticks(), times[1]);
    assert_eq!(decoded.decoder_session_reopens, 1);
    assert_eq!(state.reverse_cache_bytes, window_bytes);
    assert_eq!(
        state
            .reverse_windows
            .values()
            .map(|window| window.bytes)
            .sum::<u64>(),
        window_bytes
    );
}

/// The reverse cache a frame set reports is what every shard's windows hold, a
/// shard the frame asked nothing of included: one that built a window for an
/// earlier reversed clip still holds it, and it counts toward the sum and
/// `peakReverseCacheBytes`.
#[test]
fn the_reverse_cache_counts_every_shards_windows() {
    let worker = decode_worker_over(corpus("long-gop.mp4"), 2, |config| {
        config.max_sessions = 2;
        config.reverse_cache_bytes = 64 << 20;
    });
    let times = (0..8).rev().map(|frame| frame * 4_000).collect::<Vec<_>>();
    for (index, clip) in [(0, "ra"), (1, "rb")] {
        let mut job = run_job(clip, &times);
        job.index = index;
        worker.send(job).expect("the pool takes the job");
    }
    let started = Instant::now();
    let mut sets = Vec::new();
    while sets.len() < 2 {
        assert!(started.elapsed() < Duration::from_secs(30));
        if let Some(set) = worker
            .result_timeout(Duration::from_millis(10))
            .expect("decodes")
        {
            worker.release(set.bytes);
            sets.push(set.reverse_cache_bytes);
        }
    }
    assert!(sets[0] > 0);
    assert_eq!(sets[1], 2 * sets[0]);
}

/// The shard each of `clips` (cursors of `video-source`) is placed on, and
/// of `stills`, in one frame.
fn placed(
    placement: &mut crate::decode::ShardPlacement,
    clips: &[&str],
    stills: &[&str],
) -> Vec<usize> {
    let kinds = stills
        .iter()
        .map(|still| (SourceId::new(*still).expect("source ID"), MediaKind::Image))
        .chain([(
            SourceId::new("video-source").expect("source ID"),
            MediaKind::Video,
        )])
        .collect::<HashMap<_, _>>();
    let needs = clips
        .iter()
        .map(|clip| ("video-source", *clip))
        .chain(stills.iter().map(|still| (*still, "still-clip")))
        .map(|(source, clip)| FrameNeed {
            reader: None,
            clip_id: ClipId::new(clip).expect("clip ID"),
            source_id: SourceId::new(source).expect("source ID"),
            media_time: MediaTime::ZERO,
        })
        .collect::<Vec<_>>();
    placement.place(&needs, &[], &kinds).0
}

/// Cursors that read together are placed on different shards, and none holds
/// more than its share of sessions while another has room. Placed by a hash of
/// the cursor, the shared-source fixture's two clips could share one shard,
/// leaving the other idle, and a shard could get more cursors than its
/// sessions, reopening a decoder every frame.
#[test]
fn decode_shards_share_the_cursors_that_read_together() {
    use crate::decode::ShardPlacement;
    let mut placement = ShardPlacement::new(vec![4, 4]);
    let shards = placed(&mut placement, &["video-clip-a", "video-clip-b"], &[]);
    assert_ne!(shards[0], shards[1]);
    assert_eq!(placed(&mut placement, &["video-clip-b"], &[]), [shards[1]]);

    let clips = ["c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8"];
    for shares in [vec![4, 4], vec![3, 3, 2], vec![2, 2, 2, 2]] {
        let mut placement = ShardPlacement::new(shares.clone());
        let shards = placed(&mut placement, &clips, &[]);
        let mut per_shard = vec![0; shares.len()];
        for shard in shards {
            per_shard[shard] += 1;
        }
        assert_eq!(per_shard, shares);
    }

    let mut placement = ShardPlacement::new(vec![4, 4]);
    let shards = placed(&mut placement, &["c1", "c2", "c3"], &[]);
    let lighter = usize::from(shards.iter().filter(|&&shard| shard == 0).count() > 1);
    let with_still = placed(&mut placement, &["c1", "c2", "c3"], &["still"]);
    assert_eq!(with_still[3], lighter);
    assert_eq!(placement.shard_of(&cursor_of("c1")), Some(shards[0]));
}

/// A frame set that does not fit the decode queue budget waits until a release
/// makes room, and a cancellation wakes the wait with an error. The stall watch
/// does not see this wait: a wake that never came would hang the export
/// silently.
#[test]
fn a_full_decode_budget_waits_for_a_release_or_a_cancellation() {
    let budget = Arc::new(DecodeBudget {
        limit: 16.into(),
        state: Mutex::new(DecodeBudgetState::default()),
        available: Condvar::new(),
    });
    let cancel = CancelToken::default();
    budget.reserve(12, &cancel).expect("fits");
    let waiting = |bytes: u64, cancel: CancelToken| {
        let budget = Arc::clone(&budget);
        let (done, reserved) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(budget.reserve(bytes, &cancel));
        });
        reserved
    };
    let reserved = waiting(8, cancel.clone());
    assert!(
        reserved.recv_timeout(Duration::from_millis(100)).is_err(),
        "12 + 8 bytes do not fit 16"
    );
    budget.release(12);
    reserved
        .recv_timeout(Duration::from_secs(10))
        .expect("the release woke the wait")
        .expect("8 bytes fit once 12 are released");
    assert_eq!(budget.peak_bytes(), 12);

    let stopped = CancelToken::default();
    let reserved = waiting(9, stopped.clone());
    assert!(reserved.recv_timeout(Duration::from_millis(100)).is_err());
    stopped.cancel();
    budget.cancel();
    let error = reserved
        .recv_timeout(Duration::from_secs(10))
        .expect("the cancellation woke the wait")
        .expect_err("a cancelled wait reserves nothing");
    assert!(error.contains("cancelled"), "{error}");
    budget.release(8);
    assert_eq!(budget.state.lock().expect("budget").bytes, 0);
}

#[test]
fn decode_byte_budget_rejects_an_oversized_frame_set() {
    let budget = DecodeBudget {
        limit: 16.into(),
        state: Mutex::new(DecodeBudgetState::default()),
        available: Condvar::new(),
    };
    assert!(budget.reserve(17, &CancelToken::default()).is_err());
    budget
        .reserve(16, &CancelToken::default())
        .expect("exact budget fits");
    assert_eq!(budget.peak_bytes(), 16);
    budget.release(16);
}

/// One clip whose composition reads a source at two times keeps a cursor
/// per reading step, so neither makes the other seek.
#[test]
fn composition_readers_of_one_source_keep_their_own_cursors() {
    let project: Project = serde_json::from_str(include_str!(
        "../../../../tests/cases/host-video-export-project.json"
    ))
    .expect("video export project");
    let source = SourceId::new("video-source").expect("source ID");
    let clip = ClipId::new("clip-a").expect("clip ID");
    let reader = |id: &str| Some(alpha2_model::NodeId::new(id).expect("node ID"));
    let assets = HashMap::from([(source.as_str().to_owned(), corpus("long-gop.mp4"))]);
    let mut state = ExportDecodeState::new(&project, &assets).expect("decode state");
    let cancel = CancelToken::default();
    for (index, (early, late)) in [(0, 60_000), (4_000, 64_000)].into_iter().enumerate() {
        let need = |step: &str, ticks: i64| FrameNeed {
            clip_id: clip.clone(),
            reader: reader(step),
            source_id: source.clone(),
            media_time: MediaTime::from_ticks(ticks),
        };
        state
            .decode_job(
                DecodeJob {
                    generation: 0,
                    index: u64::try_from(index).expect("index fits"),
                    time: MediaTime::from_ticks(early),
                    needs: vec![need("early", early), need("late", late)],
                    reverse_runs: HashMap::new(),
                },
                &cancel,
            )
            .expect("both readers decode");
    }
    assert_eq!(state.decoders.len(), 2, "one decoder per reading step");
    for (step, ticks) in [("early", 4_000), ("late", 64_000)] {
        assert_eq!(
            state
                .decoded
                .get(&(source.clone(), clip.clone(), reader(step)))
                .expect("the step's cursor")
                .requested,
            MediaTime::from_ticks(ticks)
        );
    }
}

/// A software decode sharing the CPU with a software encoder is sized from the
/// largest video frame, which conform has just probed. The decode plan reads
/// conform's sizes rather than opening every used video file again, one after
/// another, before the first frame.
#[test]
fn the_decode_plan_sizes_software_decode_from_what_conform_probed() {
    let _gpu = gpu_test_lock();
    let mut host = create_host(VideoDecodePolicy::Software).expect("host");
    let directory = scratch_directory("decode-plan-size");
    let mut request = json!({
        "protocolVersion": 1, "requestId": "sized", "operation": "export",
        "project": conform_project(&json!({})),
        "assets": {"video": corpus("h264-bt709-copy.mp4")},
        "startTicks": 0, "endTicks": 120_000,
        "frameRate": {"num": 12, "den": 1},
        "videoCodec": "mpeg4-part2",
        "output": directory.join("out.mp4")
    });
    request["project"]["settings"]["width"] = json!(128);
    request["project"]["settings"]["height"] = json!(96);
    crate::host::SOURCE_REPROBES.with(|count| count.set(0));
    let result = host
        .handle(
            parse_request(&request.to_string()).expect("request"),
            &mut |_| Ok(()),
        )
        .expect("export");
    assert_eq!(result["videoEncoderHardware"], json!(false), "{result}");
    assert_eq!(
        result["performance"]["maxSourceFramePixels"],
        json!(128 * 96),
        "{}",
        result["performance"]
    );
    assert_eq!(
        crate::host::SOURCE_REPROBES.with(std::cell::Cell::get),
        0,
        "the decode plan opened a source conform had probed"
    );
    std::fs::remove_dir_all(&directory).ok();
}
