#![no_main]

use alpha2_model::{
    MAX_CLIPS_PER_TRACK, MAX_TEXT_CONTENT_BYTES, MAX_TRACKS, caption_rows, parse_srt, parse_webvtt,
};
use alpha2_time::{MAX_SERIALIZED_TICKS, MediaTime};
use libfuzzer_sys::fuzz_target;

// Any text reads as SRT or WebVTT cues or is refused, never panics, and runs in time linear in its
// size. Cues come out as a caption track holds them, and `caption_rows` splits them onto at most a
// project's tracks without overlaps or losses. The first byte picks the format: odd for WebVTT.
fuzz_target!(|data: &[u8]| {
    let Some((&format, text)) = data.split_first() else {
        return;
    };
    let Ok(text) = std::str::from_utf8(text) else {
        return;
    };
    let parsed = if format & 1 == 1 {
        parse_webvtt(text)
    } else {
        parse_srt(text)
    };
    let Ok(imported) = parsed else {
        return;
    };
    let cues = &imported.cues;
    for cue in cues {
        let range = cue.timeline_range;
        assert!(
            !range.is_empty() && range.start() >= MediaTime::ZERO,
            "{cue:?}"
        );
        assert!(range.end().ticks() <= MAX_SERIALIZED_TICKS, "{cue:?}");
        let text = cue.text.text();
        assert!(!text.trim().is_empty() && text.len() <= MAX_TEXT_CONTENT_BYTES);
    }
    assert!(cues.is_sorted_by_key(|cue| cue.timeline_range.start()));
    let Ok(rows) = caption_rows(cues) else {
        return;
    };
    assert!(rows.len() <= MAX_TRACKS);
    let mut placed = 0;
    for row in &rows {
        assert!(!row.is_empty() && row.len() <= MAX_CLIPS_PER_TRACK);
        for pair in row.windows(2) {
            assert!(
                pair[0].timeline_range.end() <= pair[1].timeline_range.start(),
                "{pair:?}"
            );
        }
        placed += row.len();
    }
    assert_eq!(placed, cues.len());
});
