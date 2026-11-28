use super::*;

#[test]
fn a_caption_track_holds_as_many_cues_as_a_track_holds_clips() {
    use crate::{CaptionCue, Captions, TextStyle};

    let cue = |start: i64, end: i64| CaptionCue {
        timeline_range: TimeRange::new(MediaTime(start), MediaTime(end)).expect("range"),
        text: "Hello".into(),
        speaker: None,
        style: None,
    };
    let with_cues = |cues| {
        let mut project = project();
        let mut track = Track::new(
            id(TrackId::new, "track-captions"),
            "Captions",
            TrackKind::Caption,
        );
        track.captions = Some(Captions {
            from_words: None,
            style: TextStyle::default(),
            styles: std::collections::BTreeMap::new(),
            cues,
        });
        project.timeline.tracks.push(track);
        project
    };
    let sequential = |count: usize| {
        (0..i64::try_from(count).expect("count"))
            .map(|index| cue(index * 10, index * 10 + 10))
            .collect::<Vec<_>>()
    };
    with_cues(sequential(crate::MAX_CLIPS_PER_TRACK))
        .validate()
        .expect("a full caption track");
    let error = with_cues(sequential(crate::MAX_CLIPS_PER_TRACK + 1))
        .validate()
        .expect_err("one cue too many");
    assert_eq!(
        error.to_string(),
        "track track-captions has 4097 caption cues; the maximum is 4096"
    );
}

#[test]
fn caption_tracks_hold_ordered_cues_and_nothing_else() {
    use crate::{CaptionCue, Captions, TextStyle};

    let cue = |start: i64, end: i64| CaptionCue {
        timeline_range: TimeRange::new(MediaTime(start), MediaTime(end)).expect("range"),
        text: "Hello".into(),
        speaker: None,
        style: None,
    };
    let with_captions = |captions: Option<Captions>, kind: TrackKind| {
        let mut project = project();
        let mut track = Track::new(id(TrackId::new, "track-captions"), "Captions", kind);
        track.captions = captions;
        project.timeline.tracks.push(track);
        project
    };
    let invalid = |project: &crate::Project| {
        matches!(
            project.validate(),
            Err(crate::ModelError::InvalidCaptions { .. })
        )
    };
    let captions = |cues| {
        Some(Captions {
            from_words: None,
            style: TextStyle::default(),
            styles: std::collections::BTreeMap::new(),
            cues,
        })
    };
    with_captions(
        captions(vec![cue(0, 100), cue(100, 200)]),
        TrackKind::Caption,
    )
    .validate()
    .expect("cues that meet");
    // Overlapping, out of order, empty text, a caption track without captions, captions on a video
    // track.
    assert!(invalid(&with_captions(
        captions(vec![cue(0, 150), cue(100, 200)]),
        TrackKind::Caption
    )));
    assert!(invalid(&with_captions(
        captions(vec![cue(100, 200), cue(0, 50)]),
        TrackKind::Caption
    )));
    let mut empty = cue(0, 10);
    empty.text = crate::TextContent::default();
    assert!(invalid(&with_captions(
        captions(vec![empty]),
        TrackKind::Caption
    )));
    assert!(invalid(&with_captions(None, TrackKind::Caption)));
    assert!(invalid(&with_captions(
        captions(Vec::new()),
        TrackKind::Video
    )));
    let mut style = TextStyle {
        font_size: 0.6,
        ..TextStyle::default()
    };
    let mut project = with_captions(captions(Vec::new()), TrackKind::Caption);
    project.timeline.tracks[1]
        .captions
        .as_mut()
        .expect("captions")
        .style = style.clone();
    assert!(invalid(&project));
    style.font_size = 0.05;
    for family in [String::new(), "a".repeat(crate::MAX_FONT_FAMILY_BYTES + 1)] {
        style.font_family = Some(family);
        project.timeline.tracks[1]
            .captions
            .as_mut()
            .expect("captions")
            .style = style.clone();
        assert!(invalid(&project));
    }
    style.font_family = Some("Noto Sans KR".into());
    project.timeline.tracks[1]
        .captions
        .as_mut()
        .expect("captions")
        .style = style;
    assert!(!invalid(&project));
}

#[test]
fn a_caption_look_is_held_to_its_limits() {
    use crate::{CaptionCue, Captions, TextStyle};

    fn stroke(width: f32) -> crate::TextStroke {
        crate::TextStroke {
            color: crate::Rgba::new(0.0, 0.0, 0.0, 1.0),
            width,
        }
    }
    fn shadow(offset: crate::Vec2, blur: f32) -> crate::TextShadow {
        crate::TextShadow {
            color: crate::Rgba::new(0.0, 0.0, 0.0, 1.0),
            offset,
            blur,
        }
    }

    let mut project = project();
    let mut track = Track::new(
        id(TrackId::new, "track-captions"),
        "Captions",
        TrackKind::Caption,
    );
    track.captions = Some(Captions {
        from_words: None,
        style: TextStyle::default(),
        styles: std::collections::BTreeMap::new(),
        cues: vec![CaptionCue {
            timeline_range: TimeRange::new(MediaTime(0), MediaTime(100)).expect("range"),
            text: "Hello".into(),
            speaker: None,
            style: None,
        }],
    });
    project.timeline.tracks.push(track);
    let caption_track = project.timeline.tracks.len() - 1;
    let invalid = |project: &crate::Project| {
        matches!(
            project.validate(),
            Err(crate::ModelError::InvalidCaptions { .. })
        )
    };
    let style = TextStyle::default();
    let looks: [fn(&mut TextStyle); 6] = [
        |style| style.strokes = vec![stroke(0.0)],
        |style| style.strokes = vec![stroke(0.05); crate::MAX_TEXT_STROKES + 1],
        |style| style.strokes = vec![stroke(0.3), stroke(0.25)],
        |style| style.shadows = vec![shadow(crate::Vec2::default(), 0.1); 5],
        |style| style.shadows = vec![shadow(crate::Vec2::new(0.0, 1.5), 0.1)],
        |style| style.shadows = vec![shadow(crate::Vec2::default(), -0.1)],
    ];
    for look in looks {
        let mut invalid_style = style.clone();
        look(&mut invalid_style);
        project.timeline.tracks[caption_track]
            .captions
            .as_mut()
            .expect("captions")
            .style = invalid_style;
        assert!(invalid(&project));
    }
    let mut look = style.clone();
    look.strokes = vec![stroke(0.0625); crate::MAX_TEXT_STROKES];
    look.shadows = vec![shadow(crate::Vec2::new(-1.0, 1.0), 1.0); crate::MAX_TEXT_SHADOWS];
    project.timeline.tracks[caption_track]
        .captions
        .as_mut()
        .expect("captions")
        .style = look;
    assert!(!invalid(&project));
}

/// A project with one caption track of `cues` (`(start, end, speaker, style)`) and `styles`,
/// and its validation result.
fn styled_captions(
    styles: &[(&str, crate::TextStylePatch)],
    cues: &[(i64, i64, Option<&str>, Option<&str>)],
) -> (Project, crate::Captions) {
    let captions = crate::Captions {
        from_words: None,
        style: crate::TextStyle::default(),
        styles: styles
            .iter()
            .map(|(name, patch)| ((*name).to_owned(), patch.clone()))
            .collect(),
        cues: cues
            .iter()
            .map(|&(start, end, speaker, style)| crate::CaptionCue {
                timeline_range: TimeRange::new(MediaTime(start), MediaTime(end)).expect("range"),
                text: "Hello".into(),
                speaker: speaker.map(Into::into),
                style: style.map(Into::into),
            })
            .collect(),
    };
    let mut project = project();
    let mut track = Track::new(id(TrackId::new, "subs"), "Subtitles", TrackKind::Caption);
    track.captions = Some(captions.clone());
    project.timeline.tracks.push(track);
    (project, captions)
}

fn red() -> crate::TextStylePatch {
    crate::TextStylePatch {
        fill: Some(crate::Paint::Solid {
            color: crate::Rgba::new(1.0, 0.0, 0.0, 1.0),
        }),
        ..crate::TextStylePatch::default()
    }
}

#[test]
fn cue_style_resolves_by_name_then_speaker_then_base() {
    let ada = crate::TextStylePatch {
        position: Some(crate::Vec2::new(0.2, 0.9)),
        ..crate::TextStylePatch::default()
    };
    let (project, captions) = styled_captions(
        &[("red", red()), ("Ada", ada)],
        &[
            (0, 100, Some("Ada"), Some("red")),
            (100, 200, Some("Ada"), None),
            (200, 300, Some("Grace"), None),
            (300, 400, None, None),
        ],
    );
    project.validate().expect("valid");
    let style = |index: usize| captions.cue_style(&captions.cues[index]).into_owned();
    assert_eq!(style(0).fill, red().fill.expect("a fill"));
    assert_eq!(style(0).position, crate::TextStyle::default().position);
    assert_eq!(style(1).position, crate::Vec2::new(0.2, 0.9));
    assert_eq!(style(2), captions.style);
    assert_eq!(style(3), captions.style);
}

#[test]
fn a_patch_replaces_whole_lists() {
    let stroke = |width| crate::TextStroke {
        color: crate::Rgba::new(0.0, 0.0, 0.0, 1.0),
        width,
    };
    let base = crate::TextStyle {
        strokes: vec![stroke(0.05), stroke(0.1)],
        ..crate::TextStyle::default()
    };
    let patched = crate::TextStylePatch {
        strokes: Some(vec![stroke(0.2)]),
        background: Some(None),
        ..crate::TextStylePatch::default()
    }
    .apply(&crate::TextStyle {
        background: Some(crate::TextBackground {
            color: crate::Rgba::new(0.0, 0.0, 0.0, 0.5),
            padding: crate::TextInsets::all(0.2),
            radius: 0.0,
            border: None,
            extent: crate::TextBoxExtent::Line,
        }),
        ..base.clone()
    });
    assert_eq!(patched.strokes, vec![stroke(0.2)]);
    // `null` clears what the base has; a field left out keeps it.
    assert_eq!(patched.background, None);
    assert_eq!(patched.font_size, base.font_size);
    let read = |json: &str| serde_json::from_str::<crate::TextStylePatch>(json).expect("patch");
    assert_eq!(read("{}").font_family, None);
    assert_eq!(read(r#"{"font_family":null}"#).font_family, Some(None));
    assert_eq!(
        read(r#"{"font_family":"Inter"}"#).font_family,
        Some(Some("Inter".into()))
    );
    let written = serde_json::to_value(read(r#"{"font_family":null}"#)).expect("writes");
    assert_eq!(written, serde_json::json!({ "font_family": null }));
}

#[test]
fn an_unknown_cue_style_is_refused() {
    let (project, _) = styled_captions(&[("red", red())], &[(0, 100, None, Some("blue"))]);
    assert!(matches!(
        project.validate(),
        Err(crate::ModelError::UnknownCaptionStyle { cue: 0, ref name, .. }) if name == "blue"
    ));
}

#[test]
fn a_merged_style_over_the_caption_limit_is_refused() {
    let huge = crate::TextStylePatch {
        font_size: Some(0.5),
        ..crate::TextStylePatch::default()
    };
    // An unused patch is not checked as a style; a cue taking it is refused by name.
    let (project, _) = styled_captions(&[("huge", huge.clone())], &[(0, 100, None, None)]);
    project.validate().expect("an unused patch");
    let (project, _) = styled_captions(&[("huge", huge)], &[(0, 100, Some("huge"), None)]);
    assert!(matches!(
        project.validate(),
        Err(crate::ModelError::InvalidCaptionCueStyle { cue: 0, .. })
    ));
}

#[test]
fn run_times_lie_in_their_cue() {
    let (mut project, mut captions) = styled_captions(&[], &[(1_000, 2_000, None, None)]);
    let run = |start: i64, end: i64| crate::TextRun {
        text: "Hi".into(),
        class: Vec::new(),
        time: Some(crate::RunTime {
            start: MediaTime(start),
            end: MediaTime(end),
        }),
        style: None,
    };
    for (runs, valid) in [
        (vec![run(0, 500), run(500, 1_000)], true),
        (vec![run(0, 1_001)], false),
    ] {
        captions.cues[0].text = crate::TextContent::Runs(runs);
        let track = project
            .timeline
            .tracks
            .last_mut()
            .expect("the caption track");
        track.captions = Some(captions.clone());
        assert_eq!(project.validate().is_ok(), valid);
    }
}

#[cfg(feature = "schema")]
#[test]
fn style_patch_has_every_text_style_field() {
    // Guards that every text style field also exists on the patch.
    let properties = |schema: schemars::Schema| {
        let mut names = schema
            .get("properties")
            .and_then(|properties| properties.as_object())
            .expect("properties")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    assert_eq!(
        properties(schemars::schema_for!(crate::TextStylePatch)),
        properties(schemars::schema_for!(crate::TextStyle))
    );
}

/// Words 10,000 ticks apart, each 8,000 long, with the cue texts and run texts they group to.
fn grouped(words: &[&str], max_chars: u32) -> Vec<(String, Vec<String>)> {
    let words = words
        .iter()
        .zip(0..)
        .map(|(text, index): (&&str, i64)| crate::DataWord {
            start: MediaTime(index * 10_000),
            end: MediaTime(index * 10_000 + 8_000),
            text: (*text).to_owned(),
            speaker: None,
        })
        .collect::<Vec<_>>();
    crate::group_words(&words, max_chars)
        .into_iter()
        .map(|cue| {
            let crate::TextContent::Runs(runs) = &cue.text else {
                panic!("cues from words have runs");
            };
            let runs = runs.iter().map(|run| run.text.clone()).collect::<Vec<_>>();
            (runs.concat(), runs)
        })
        .collect()
}

#[test]
fn japanese_words_are_joined_without_spaces() {
    let cues = grouped(&["今日", "は", "いい", "天気", "です", "ね"], 42);
    assert_eq!(cues.len(), 1);
    assert_eq!(cues[0].0, "今日はいい天気ですね");
    assert_eq!(cues[0].1[0], "今日", "a run carries no separator");
}

#[test]
fn mixed_latin_and_japanese_words_are_spaced_only_between_latin_words() {
    let cues = grouped(&["Alpha2", "Studio", "で", "編集", "して", "OK"], 42);
    assert_eq!(cues[0].0, "Alpha2 Studio で編集して OK");
}

#[test]
fn thai_words_are_joined_without_spaces() {
    let cues = grouped(&["สวัสดี", "ครับ", "ทุก", "คน"], 42);
    assert_eq!(cues[0].0, "สวัสดีครับทุกคน");
}

#[test]
fn a_cue_of_unspaced_words_counts_no_separators() {
    // Ten two-character words fill 20 characters exactly; with spaces they would take 29.
    let words = ["日本"; 10];
    assert_eq!(grouped(&words, 20).len(), 1);
    assert_eq!(
        grouped(&["a1", "b2", "c3"], 7).len(),
        2,
        "Latin words still count a space"
    );
}

#[test]
fn fullwidth_terminators_end_a_sentence() {
    for end in ["！", "？", "．", "。"] {
        let first = format!("はい{end}");
        let cues = grouped(&[first.as_str(), "次", "です"], 42);
        assert_eq!(cues.len(), 2, "{end}");
        assert_eq!(cues[1].0, "次です");
    }
}

#[test]
fn latin_words_group_with_a_space_between_each_two() {
    let cues = grouped(&["Hello", "there.", "General", "Kenobi"], 42);
    assert_eq!(
        cues.iter().map(|cue| cue.0.as_str()).collect::<Vec<_>>(),
        ["Hello there.", "General Kenobi"]
    );
    assert_eq!(cues[1].1, ["General ", "Kenobi"]);
}

fn word(start: i64, end: i64, text: &str, speaker: Option<&str>) -> crate::DataWord {
    crate::DataWord {
        start: MediaTime(start),
        end: MediaTime(end),
        text: text.to_owned(),
        speaker: speaker.map(str::to_owned),
    }
}

/// `cues` as the explicit cues of a caption track, validated.
fn as_explicit_cues(cues: Vec<crate::CaptionCue>) -> Result<(), crate::ModelError> {
    let mut project = project();
    let mut track = Track::new(id(TrackId::new, "baked"), "Baked", TrackKind::Caption);
    track.captions = Some(crate::Captions {
        from_words: None,
        style: crate::TextStyle::default(),
        styles: std::collections::BTreeMap::new(),
        cues,
    });
    project.timeline.tracks.push(track);
    project.validate()
}

#[test]
fn derived_cues_end_before_the_text_limits() {
    // Speech without sentences and no character limit: the byte limit ends each cue.
    let long = (0..2_000_i64)
        .map(|at| word(at * 30_000, (at + 1) * 30_000, "abcdef", None))
        .collect::<Vec<_>>();
    // One-letter words: the run limit ends each cue first.
    let short = (0..2_000_i64)
        .map(|at| word(at * 30_000, (at + 1) * 30_000, "a", None))
        .collect::<Vec<_>>();
    for words in [long, short] {
        let cues = crate::group_words(&words, u32::MAX);
        assert!(cues.len() > 1);
        assert!(
            cues.iter()
                .all(|cue| cue.text.len() <= crate::MAX_TEXT_CONTENT_BYTES
                    && cue.text.runs().len() <= crate::MAX_TEXT_RUNS)
        );
        let words_kept = cues.iter().map(|cue| cue.text.runs().len()).sum::<usize>();
        assert_eq!(words_kept, words.len());
        as_explicit_cues(cues).expect("derived cues are valid explicit cues");
    }
}

#[test]
fn a_word_of_white_space_makes_no_run() {
    let words = [
        word(0, 30_000, "Hello.", None),
        word(30_000, 60_000, " ", None),
        word(60_000, 90_000, "Again", None),
    ];
    let cues = crate::group_words(&words, 42);
    assert_eq!(cues.len(), 2);
    assert!(cues.iter().all(|cue| {
        cue.text
            .runs()
            .iter()
            .all(|run| !run.text.trim().is_empty())
    }));
    as_explicit_cues(cues).expect("valid");
}

#[test]
fn of_two_cues_with_one_start_the_later_is_kept() {
    let words = [
        word(0, 60_000, "Hi", Some("B")),
        word(0, 120_000, "Hello", Some("A")),
        word(240_000, 300_000, "Later", Some("B")),
    ];
    let cues = crate::group_words(&words, 42);
    let shown = cues
        .iter()
        .map(|cue| (cue.speaker.clone(), cue.text.text().into_owned()))
        .collect::<Vec<_>>();
    assert_eq!(
        shown,
        [
            (Some("A".to_owned()), "Hello".to_owned()),
            (Some("B".to_owned()), "Later".to_owned())
        ]
    );
    assert!(cues[0].timeline_range.end() <= cues[1].timeline_range.start());
    as_explicit_cues(cues).expect("valid");
}

#[test]
fn words_group_in_time_linear_in_their_count() {
    let contiguous = |count: usize| {
        (0..i64::try_from(count).expect("count"))
            .map(|at| word(at * 10, at * 10 + 10, "a", None))
            .collect::<Vec<_>>()
    };
    alpha2_scaling_support::assert_linear_shared(
        "word grouping",
        200_000 / alpha2_scaling_support::GROWTH,
        contiguous,
        |words| {
            std::hint::black_box(crate::group_words(words, u32::MAX));
        },
    );
}
