use super::*;

fn run(text: &str, class: &[&str]) -> TextRun {
    TextRun {
        text: text.into(),
        class: class.iter().map(|&name| name.into()).collect(),
        time: None,
    }
}

fn range(start_ms: i64, end_ms: i64) -> TimeRange {
    TimeRange::new(
        MediaTime::from_ticks(start_ms * 120),
        MediaTime::from_ticks(end_ms * 120),
    )
    .expect("range")
}

#[test]
fn srt_reads_multiline_cues_strips_markup_and_skips_bad_blocks() {
    let text = "\u{feff}1\r\n00:00:01,000 --> 00:00:02,500\r\nHello <i>there</i>\r\n{\\an8}General Kenobi\r\n\r\n\
                    2\n00:00:03,000 --> 00:00:02,000\nEnds before it starts\n\n\
                    not a cue at all\n\n\
                    3\n01:02:03.004 --> 01:02:04.000\n<font color=\"red\">Late</font>\n\n\
                    4\n00:00:05,000 --> 00:00:06,000\n\n";
    let imported = parse_srt(text).expect("an SRT file");
    assert_eq!(
        imported.cues,
        [
            // Italics become a run's class; the override block is dropped.
            CaptionCue {
                timeline_range: range(1_000, 2_500),
                text: TextContent::Runs(vec![
                    run("Hello ", &[]),
                    run("there", &["i"]),
                    run("\nGeneral Kenobi", &[]),
                ]),
                speaker: None,
                style: None,
            },
            CaptionCue {
                timeline_range: range(3_723_004, 3_724_000),
                text: "Late".into(),
                speaker: None,
                style: None,
            },
        ]
    );
    // The backwards cue, the stray text, and the cue with no text.
    assert_eq!(imported.skipped, 3);
}

#[test]
fn webvtt_reads_cues_with_settings_references_and_identifiers() {
    let text = "WEBVTT - a title\n\nNOTE a comment\nspanning lines\n\nSTYLE\n::cue { color: red }\n\n\
                    intro\n00:01.000 --> 00:02.000 align:start line:0\n<v Roger>Fish &amp; chips</v>\n\n\
                    00:00:03.500 --> 00:00:04.000\n<c.loud>A &lt;b&gt; tag</c> &#x41;&#66; &bogus;\n\n\
                    00:05.000 -> 00:06.000\nbroken arrow\n";
    let imported = parse_webvtt(text).expect("a WebVTT file");
    assert_eq!(
        imported.cues,
        [
            // A voice names the speaker; a class span becomes a run's class.
            CaptionCue {
                timeline_range: range(1_000, 2_000),
                text: TextContent::Runs(vec![run("Fish & chips", &["voice"])]),
                speaker: Some("Roger".into()),
                style: None,
            },
            CaptionCue {
                timeline_range: range(3_500, 4_000),
                text: TextContent::Runs(
                    vec![run("A <b> tag", &["loud"]), run(" AB &bogus;", &[]),]
                ),
                speaker: None,
                style: None,
            },
        ]
    );
    assert_eq!(imported.skipped, 1);
    assert_eq!(
        parse_webvtt("1\n00:01.000 --> 00:02.000\nno header\n"),
        Err(CaptionImportError::MissingWebVttHeader)
    );
    assert!(parse_webvtt("WEBVTTX\n").is_err());
}

/// A time is written to a project as at most `MAX_SERIALIZED_TICKS`, the most a document
/// reads: a cue past it is a block that does not parse, skipped and counted, and the cues that
/// are left make a document that loads.
#[test]
fn a_cue_later_than_a_project_holds_is_skipped() {
    // The last millisecond a document holds is 20849998:16:29.508.
    let last = alpha2_time::MAX_SERIALIZED_TICKS / TICKS_PER_MILLISECOND;
    assert_eq!(last, 75_059_993_789_508);
    let srt = "1\n00:00:01,000 --> 00:00:02,000\nfine\n\n\
                   2\n20849998:16:29,507 --> 20849998:16:29,508\nlast\n\n\
                   3\n20849998:16:29,508 --> 20849998:16:29,509\none past\n\n\
                   4\n99999999:00:00,000 --> 99999999:00:01,000\nfar\n";
    let webvtt = "WEBVTT\n\n00:01.000 --> 00:02.000\nfine\n\n\
                      20849998:16:29.507 --> 20849998:16:29.508\nlast\n\n\
                      20849998:16:29.508 --> 20849998:16:29.509\none past\n\n\
                      99999999:00:00.000 --> 99999999:00:01.000\nfar\n";
    for (format, imported) in [
        ("srt", parse_srt(srt).expect("an SRT file")),
        ("webvtt", parse_webvtt(webvtt).expect("a WebVTT file")),
    ] {
        assert_eq!(
            imported.cues,
            [
                CaptionCue {
                    timeline_range: range(1_000, 2_000),
                    text: "fine".into(),
                    speaker: None,
                    style: None,
                },
                CaptionCue {
                    timeline_range: range(last - 1, last),
                    text: "last".into(),
                    speaker: None,
                    style: None,
                },
            ],
            "{format}"
        );
        assert_eq!(imported.skipped, 2, "{format}");
        let mut project = crate::tests::project();
        for (index, row) in caption_rows(&imported.cues)
            .expect("rows")
            .into_iter()
            .enumerate()
        {
            let mut track = crate::Track::new(
                crate::TrackId::new(format!("captions-{index}")).expect("an id"),
                "Captions",
                crate::TrackKind::Caption,
            );
            track.captions = Some(crate::Captions {
                style: crate::TextStyle::default(),
                styles: std::collections::BTreeMap::new(),
                cues: row,
            });
            project.timeline.tracks.push(track);
        }
        let json = serde_json::to_string(&project).expect("JSON");
        assert!(
            crate::Project::from_json(&json).is_ok(),
            "{format}: {:?}",
            crate::Project::from_json(&json)
        );
    }
}

#[test]
fn a_webvtt_cue_right_under_the_header_is_read() {
    let texts = |text: &str| {
        let imported = parse_webvtt(text).expect("a WebVTT file");
        let texts: Vec<String> = imported
            .cues
            .into_iter()
            .map(|cue| cue.text.text().into_owned())
            .collect();
        (texts, imported.skipped)
    };
    // As the WebVTT parser does, the header ends at the first timing line.
    assert_eq!(
        texts("WEBVTT\n00:00.000 --> 00:01.000\nFirst\n\n00:01.000 --> 00:02.000\nSecond\n"),
        (vec!["First".to_owned(), "Second".to_owned()], 0)
    );
    assert_eq!(
        texts("WEBVTT\r\nKind: captions\r\nLanguage: en\r\n00:00.000 --> 00:01.000\r\nFirst\r\n"),
        (vec!["First".to_owned()], 0)
    );
}

#[test]
fn every_line_terminator_ends_a_line() {
    let cues = |text: &str| {
        let imported = parse_webvtt(text).expect("a WebVTT file");
        let texts: Vec<String> = imported
            .cues
            .into_iter()
            .map(|cue| cue.text.text().into_owned())
            .collect();
        (texts, imported.skipped)
    };
    let lines = [
        "WEBVTT",
        "",
        "00:00.000 --> 00:01.000",
        "Hello",
        "World",
        "",
        "NOTE\ta comment",
        "",
        "STYLE\t",
        "::cue { color: red }",
        "",
        "00:01.000 --> 00:02.000",
        "Again",
    ];
    let expected = (vec!["Hello\nWorld".to_owned(), "Again".to_owned()], 0);
    // LF, CRLF, a lone CR, and the CRCRLF of a file converted twice.
    for terminator in ["\n", "\r\n", "\r", "\r\r\n"] {
        assert_eq!(cues(&lines.join(terminator)), expected, "{terminator:?}");
    }
    let srt = ["1", "00:00:01,000 --> 00:00:02,000", "Hello", "", "2"];
    let srt = [&srt[..], &["00:00:03,000 --> 00:00:04,000", "Again", ""]].concat();
    for terminator in ["\n", "\r\n", "\r", "\r\r\n"] {
        let imported = parse_srt(&srt.join(terminator)).expect("an SRT file");
        let texts: Vec<String> = imported
            .cues
            .into_iter()
            .map(|cue| cue.text.text().into_owned())
            .collect();
        assert_eq!(
            (texts, imported.skipped),
            (vec!["Hello".to_owned(), "Again".to_owned()], 0),
            "{terminator:?}"
        );
    }
}

#[test]
fn srt_keeps_a_less_than_sign_that_opens_no_tag() {
    let text = |line: &str| {
        let imported =
            parse_srt(&format!("1\n00:00:01,000 --> 00:00:02,000\n{line}\n")).expect("an SRT file");
        imported
            .cues
            .into_iter()
            .map(|cue| cue.text)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        text("I <3 you, x < y and more"),
        ["I <3 you, x < y and more"]
    );
    assert_eq!(text("<< Previously >>"), ["<< Previously >>"]);
    assert_eq!(text("<"), ["<"]);
    assert_eq!(text("a <i unclosed"), ["a <i unclosed"]);
    assert_eq!(text("x <a <b>bold</b>"), ["x <a bold"]);
    assert_eq!(text("<i>it</i> <font color=\"red\">red</font>"), ["it red"]);
}

#[test]
fn overlapping_cues_spread_over_rows() {
    let cue = |start, end| CaptionCue {
        timeline_range: range(start, end),
        text: "x".into(),
        speaker: None,
        style: None,
    };
    let rows = caption_rows(&[cue(0, 1_000), cue(500, 1_500), cue(1_000, 2_000)]);
    assert_eq!(
        rows,
        Ok(vec![
            vec![cue(0, 1_000), cue(1_000, 2_000)],
            vec![cue(500, 1_500)]
        ])
    );
}

#[test]
fn a_row_holds_no_more_cues_than_a_caption_track() {
    let cue = |index: i64| CaptionCue {
        timeline_range: range(index * 2_000, index * 2_000 + 1_500),
        text: format!("cue {index}").into(),
        speaker: None,
        style: None,
    };
    let count = 2 * MAX_CLIPS_PER_TRACK + 10;
    let cues: Vec<_> = (0..i64::try_from(count).unwrap()).map(cue).collect();
    let rows = caption_rows(&cues).unwrap();
    assert_eq!(
        rows.iter().map(Vec::len).collect::<Vec<_>>(),
        [MAX_CLIPS_PER_TRACK, MAX_CLIPS_PER_TRACK, 10]
    );
    assert_eq!(rows.concat(), cues);
    for (index, row) in rows.into_iter().enumerate() {
        let mut project = crate::tests::project();
        let mut track = crate::Track::new(
            crate::TrackId::new(format!("captions-{index}")).unwrap(),
            "Captions",
            crate::TrackKind::Caption,
        );
        track.captions = Some(crate::Captions {
            style: crate::TextStyle::default(),
            styles: std::collections::BTreeMap::new(),
            cues: row,
        });
        project.timeline.tracks.push(track);
        assert_eq!(project.validate(), Ok(()), "row {index}");
    }
}

#[test]
fn cues_that_need_more_tracks_than_a_project_holds_are_refused() {
    let cue = CaptionCue {
        timeline_range: range(0, 1_000),
        text: "all at once".into(),
        speaker: None,
        style: None,
    };
    assert_eq!(
        caption_rows(&vec![cue.clone(); MAX_TRACKS]).map(|rows| rows.len()),
        Ok(MAX_TRACKS)
    );
    let error = caption_rows(&vec![cue; MAX_TRACKS + 1]).unwrap_err();
    assert_eq!(
        error,
        CaptionImportError::TooManyRows {
            rows: MAX_TRACKS + 1,
            maximum: MAX_TRACKS
        }
    );
    assert_eq!(
        error.to_string(),
        "the captions need 257 caption tracks; a project holds at most 256 tracks"
    );
}

/// An SRT file of `count` cues, each `spacing` ms after the last and
/// `length` ms long.
fn srt(count: usize, spacing: i64, length: i64) -> String {
    use std::fmt::Write as _;
    let stamp = |ms: i64| {
        format!(
            "{:02}:{:02}:{:02},{:03}",
            ms / 3_600_000,
            ms / 60_000 % 60,
            ms / 1_000 % 60,
            ms % 1_000
        )
    };
    let mut file = String::new();
    for index in 0..count {
        let start = i64::try_from(index).expect("small") * spacing;
        let (number, from, to) = (index + 1, stamp(start), stamp(start + length));
        writeln!(file, "{number}\n{from} --> {to}\nCue {number}\n").expect("a string takes it");
    }
    file
}

#[test]
fn a_line_of_ampersands_is_read_in_time_linear_in_its_length() {
    // Up to a megabyte of `&` with no reference in it, and with a `;` that closes none of them.
    let lines: [fn(usize) -> String; 3] = [
        |bytes| "&".repeat(bytes),
        |bytes| format!("{};", "&".repeat(bytes)),
        |bytes| format!("{};", "&x".repeat(bytes / 2)),
    ];
    for (shape, line) in lines.into_iter().enumerate() {
        let file = |bytes| format!("WEBVTT\n\n00:01.000 --> 00:02.000\n{}\n", line(bytes));
        let imported = parse_webvtt(&file(1 << 20)).expect("a WebVTT file");
        // Longer than a cue holds, so it is skipped.
        assert_eq!((imported.cues.len(), imported.skipped), (0, 1));
        alpha2_scaling_support::assert_linear_shared(
            &format!("a line of ampersands, shape {shape}"),
            (1 << 20) / alpha2_scaling_support::GROWTH,
            file,
            |file| {
                parse_webvtt(file).expect("a WebVTT file");
            },
        );
    }
    // References still decode, `+` and all, and anything else stays as written.
    for (written, read) in [
        ("&amp;&lt;&gt;&nbsp;&lrm;&rlm;", "&<>\u{a0}\u{200e}\u{200f}"),
        ("&#65;&#x42;&#X43;&#+68;&#x+45;&#0070;", "ABCDEF"),
        ("&amp;amp; &&amp; &amp&lt;", "&amp; && &amp<"),
        (
            "&; &#; &#x; &bogus; & amp; &am p;",
            "&; &#; &#x; &bogus; & amp; &am p;",
        ),
        (
            "&#1114112; &#xD800; &#99999999999;",
            "&#1114112; &#xD800; &#99999999999;",
        ),
        ("tail &", "tail &"),
        ("&ä; &amp", "&ä; &amp"),
    ] {
        assert_eq!(decode_references(written), read, "{written:?}");
    }
}

#[test]
fn a_file_past_the_import_limits_is_refused() {
    let large = format!("WEBVTT\n{}", " ".repeat(MAX_CAPTION_FILE_BYTES));
    let refused = CaptionImportError::FileTooLarge {
        bytes: large.len(),
        maximum: MAX_CAPTION_FILE_BYTES,
    };
    assert_eq!(parse_webvtt(&large), Err(refused));
    assert_eq!(parse_srt(&large), Err(refused));
    assert_eq!(
        refused.to_string(),
        format!(
            "the subtitle file is {} bytes; at most 67108864 are read",
            large.len()
        )
    );
    let most = srt(MAX_CAPTION_FILE_CUES, 1_000, 900);
    assert_eq!(
        parse_srt(&most).map(|imported| imported.cues.len()),
        Ok(MAX_CAPTION_FILE_CUES)
    );
    let refused = CaptionImportError::TooManyCues {
        maximum: MAX_CAPTION_FILE_CUES,
    };
    assert_eq!(
        parse_srt(&srt(MAX_CAPTION_FILE_CUES + 1, 1_000, 900)),
        Err(refused)
    );
    assert_eq!(
        parse_webvtt(&format!(
            "WEBVTT\n\n{}",
            srt(MAX_CAPTION_FILE_CUES + 1, 1_000, 900).replace(',', ".")
        )),
        Err(refused)
    );
    assert_eq!(
        refused.to_string(),
        "the subtitle file holds more than 65536 cues"
    );
}

#[test]
fn cues_go_to_the_first_row_with_room_in_time_n_log_n() {
    // Every cue at once: a row each, found without looking through the rows before it.
    let all_at_once = vec![
        CaptionCue {
            timeline_range: range(0, 1_000),
            text: "x".into(),
            speaker: None,
            style: None,
        };
        MAX_CAPTION_FILE_CUES
    ];
    assert_eq!(
        caption_rows(&all_at_once),
        Err(CaptionImportError::TooManyRows {
            rows: MAX_CAPTION_FILE_CUES,
            maximum: MAX_TRACKS
        })
    );
    // Measured an eighth of the way up: towards the limit the rows and their cues outgrow a core's
    // cache, and the misses grow the work about 26 times in release for this linear step. A scan of
    // the rows per cue still dominates there.
    alpha2_scaling_support::assert_linear(
        "caption rows for cues all at once",
        MAX_CAPTION_FILE_CUES / alpha2_scaling_support::GROWTH.pow(2),
        |count| all_at_once[..count].to_vec(),
        |cues| {
            let _ = caption_rows(cues);
        },
    );
    // The rows the first-fit scan gives, for cues of every overlap.
    let first_fit = |cues: &[CaptionCue]| {
        let mut sorted = cues.to_vec();
        sorted.sort_by_key(|cue| cue.timeline_range.start());
        let mut rows: Vec<Vec<CaptionCue>> = Vec::new();
        for cue in sorted {
            let start = cue.timeline_range.start();
            match rows.iter_mut().find(|row| {
                row.len() < MAX_CLIPS_PER_TRACK
                    && row
                        .last()
                        .is_none_or(|last| last.timeline_range.end() <= start)
            }) {
                Some(row) => row.push(cue),
                None => rows.push(vec![cue]),
            }
        }
        rows
    };
    let mut seed = 0x9e37_79b9_u32;
    let mut next = move |below: u32| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        i64::from(seed % below)
    };
    for (count, span, longest) in [(50, 100, 30), (400, 2_000, 400), (9_000, 40_000, 9)] {
        let cues = (0..count)
            .map(|index| {
                let start = next(span);
                CaptionCue {
                    timeline_range: range(start, start + 1 + next(longest)),
                    text: format!("{index}").into(),
                    speaker: None,
                    style: None,
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(caption_rows(&cues), Ok(first_fit(&cues)), "{count} cues");
    }
}

#[test]
fn webvtt_voices_become_speakers() {
    let text = "WEBVTT\n\n00:01.000 --> 00:02.000\n<v.loud Ann>Hi</v> <v Bob>there</v>\n";
    let imported = parse_webvtt(text).expect("a WebVTT file");
    let cue = &imported.cues[0];
    assert_eq!(cue.speaker.as_deref(), Some("Ann"));
    assert_eq!(
        cue.text,
        TextContent::Runs(vec![
            run("Hi", &["voice", "loud"]),
            run(" ", &[]),
            run("there", &["voice"]),
        ])
    );
    assert_eq!(imported.notes.extra_voices, 1);
}

#[test]
fn webvtt_classes_and_bold_become_run_classes() {
    let text = "WEBVTT\n\n00:01.000 --> 00:02.000\n<c.a.b>one <b>two</b></c> <i>three</i> <c.bad!name.ok>four</c>\n";
    let imported = parse_webvtt(text).expect("a WebVTT file");
    assert_eq!(
        imported.cues[0].text,
        TextContent::Runs(vec![
            // Bold adds the class `b`, which the span already gives: one run.
            run("one two", &["a", "b"]),
            run(" ", &[]),
            run("three", &["i"]),
            run(" ", &[]),
            run("four", &["ok"]),
        ])
    );
    assert_eq!(imported.notes.dropped_classes, 1);
}

#[test]
fn webvtt_inline_timestamps_become_timed_runs() {
    let text = "WEBVTT\n\n00:01.000 --> 00:04.000\nOne <00:00:02.000>two <00:00:03.000>three\n";
    let imported = parse_webvtt(text).expect("a WebVTT file");
    let ms = |ms: i64| MediaTime::from_ticks(ms * 120);
    let timed = |text: &str, start: i64, end: i64| TextRun {
        text: text.into(),
        class: Vec::new(),
        time: Some(RunTime {
            start: ms(start),
            end: ms(end),
        }),
    };
    // Times are from the cue's start: the first run from it, the last to its end.
    assert_eq!(
        imported.cues[0].text,
        TextContent::Runs(vec![
            timed("One ", 0, 1_000),
            timed("two ", 1_000, 2_000),
            timed("three", 2_000, 3_000),
        ])
    );
}

#[test]
fn out_of_order_timestamps_are_dropped_and_counted() {
    let text =
        "WEBVTT\n\n00:01.000 --> 00:04.000\nA <00:00:03.000>b <00:00:02.000>c <00:00:09.000>d\n";
    let imported = parse_webvtt(text).expect("a WebVTT file");
    assert_eq!(imported.notes.dropped_timestamps, 2);
    let runs = imported.cues[0].text.runs();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].text, "b c d");
}

#[test]
fn plain_cues_stay_plain() {
    let imported =
        parse_webvtt("WEBVTT\n\n00:01.000 --> 00:02.000\n<ruby>漢<rt>kan</rt></ruby> &amp; text\n")
            .expect("a WebVTT file");
    assert_eq!(
        imported.cues[0].text,
        TextContent::Plain("漢kan & text".into())
    );
    assert_eq!(imported.cues[0].speaker, None);
    assert_eq!(imported.notes, ImportNotes::default());
}
