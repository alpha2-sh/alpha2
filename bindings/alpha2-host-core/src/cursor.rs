//! Which frames an export cursor reads next, and when a decoder keeps decoding instead of seeking.
//!
//! A cursor is one reader of one source: a clip, or a composition step inside it, so two clips of
//! one file decode independently.

use std::collections::HashMap;

use alpha2_engine::ExportFrameNeeds;
use alpha2_eval::FrameNeed;
use alpha2_media::SourceId;
use alpha2_model::{ClipId, NodeId};
use alpha2_time::{MediaTime, TimeRange};

#[derive(Debug, Eq, PartialEq)]
/// The times a cursor asks for while its source time does not increase.
pub struct BackwardRun {
    /// Distinct, non-increasing, in the order the cursor asks for them.
    pub times: Vec<MediaTime>,
    /// The last export frame index of the run.
    pub until: u64,
}

/// Times while they do not increase and every frame asks for the cursor.
#[must_use]
pub fn backward_run(
    frames: &[ExportFrameNeeds],
    start: u64,
    cursor: &DecodeCursorKey,
) -> BackwardRun {
    let mut times = Vec::<MediaTime>::new();
    let mut until = start;
    for frame in frames.iter().skip_while(|frame| frame.index < start) {
        let Some(time) = cursor_time(frame, cursor) else {
            break;
        };
        match times.last() {
            Some(last) if time > *last => break,
            Some(last) if time == *last => {}
            _ => times.push(time),
        }
        until = frame.index;
    }
    BackwardRun { times, until }
}

/// The source time `frame` asks `cursor` for, if it reads it.
#[must_use]
pub fn cursor_time(frame: &ExportFrameNeeds, cursor: &DecodeCursorKey) -> Option<MediaTime> {
    frame
        .needs
        .iter()
        .find(|need| decode_cursor_key(need) == *cursor)
        .map(|need| need.media_time)
}

/// Two seconds, as the browser's video adapters use. Further ahead, a seek from the preceding
/// keyframe decodes less than every frame between.
pub const MAX_SEQUENTIAL_DECODE_GAP: MediaTime =
    MediaTime::from_ticks(2 * alpha2_time::TICKS_PER_SECOND);

/// Not backward, and at most [`MAX_SEQUENTIAL_DECODE_GAP`] ahead. Either way the frame presented at
/// `requested` is the one answered.
#[must_use]
pub fn decodes_on(previous: MediaTime, requested: MediaTime) -> bool {
    requested >= previous
        && requested.ticks().saturating_sub(previous.ticks()) <= MAX_SEQUENTIAL_DECODE_GAP.ticks()
}

/// The source, the clip, and the composition step reading it (none for the clip's own source).
pub type DecodeCursorKey = (SourceId, ClipId, Option<NodeId>);

/// The cursor a need reads through.
#[must_use]
pub fn decode_cursor_key(need: &FrameNeed) -> DecodeCursorKey {
    (
        need.source_id.clone(),
        need.clip_id.clone(),
        need.reader.clone(),
    )
}

/// The peak number of decoder cursors reading at once
/// ([`alpha2_eval::ActiveIndex::peak_source_reads`]) over `export_range`, at least 1, and when each
/// reads. Provider sources and stills open no decoder.
#[must_use]
pub fn visual_decode_reads(
    project: &alpha2_model::Project,
    export_range: TimeRange,
) -> (usize, CursorReads) {
    let decoded = project
        .sources
        .iter()
        .filter(|source| source.kind == alpha2_model::MediaKind::Video && source.provider.is_none())
        .map(|source| &source.id)
        .collect::<std::collections::HashSet<_>>();
    // An export validated the project already; an index it cannot build reads nothing.
    let Ok(index) = alpha2_eval::ActiveIndex::build(project) else {
        return (1, CursorReads::default());
    };
    let counted = |source: &SourceId| decoded.contains(source);
    (
        index.peak_source_reads(export_range, counted).max(1),
        CursorReads::new(index.source_reads(export_range, counted)),
    )
}

/// Where each cursor may read ([`alpha2_eval::ActiveIndex::source_reads`]), which tells a shard
/// which session it can close and which one is read last. An unknown cursor is taken to be read
/// now, so it is never closed early and is evicted only by recency.
#[derive(Debug, Default)]
pub struct CursorReads {
    /// By start.
    spans: HashMap<DecodeCursorKey, Vec<TimeRange>>,
}

impl CursorReads {
    /// The spans every cursor reads, from the active index.
    #[must_use]
    pub fn new(reads: Vec<alpha2_eval::SourceRead>) -> Self {
        let mut spans = HashMap::<DecodeCursorKey, Vec<TimeRange>>::new();
        for read in reads {
            spans
                .entry((read.source_id, read.clip_id, read.reader))
                .or_default()
                .push(read.span);
        }
        for cursor_spans in spans.values_mut() {
            cursor_spans.sort_unstable_by_key(|span| span.start());
        }
        Self { spans }
    }

    /// Whether `cursor` has spans.
    #[must_use]
    pub fn knows(&self, cursor: &DecodeCursorKey) -> bool {
        self.spans.contains_key(cursor)
    }

    /// `time` itself inside a span, else the next span's start; `None` when nothing is read from
    /// `time` on or the cursor is unknown.
    #[must_use]
    pub fn next_read(&self, cursor: &DecodeCursorKey, time: MediaTime) -> Option<MediaTime> {
        self.spans
            .get(cursor)?
            .iter()
            .find(|span| span.end() > time)
            .map(|span| span.start().max(time))
    }

    /// Whether `cursor` reads nothing from `time` on.
    #[must_use]
    pub fn finished(&self, cursor: &DecodeCursorKey, time: MediaTime) -> bool {
        self.knows(cursor) && self.next_read(cursor, time).is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: i64, end: i64) -> TimeRange {
        TimeRange::new(MediaTime::from_ticks(start), MediaTime::from_ticks(end)).expect("range")
    }

    #[test]
    fn decode_parallelism_counts_what_compositions_read() {
        use serde_json::json;
        let span = |start, end| json!({"start": start, "end": end});
        let video = |id: &str| json!({"id": id, "kind": "video", "locator": format!("{id}.mp4")});
        let media = |id: &str| json!({"id": id, "op": {"type": "media", "source_id": id}});
        let quad = json!({"id": "quad", "nodes": [
            media("a"), media("b"), media("c"), media("d"),
            {"id": "mix", "op": {"type": "merge", "layers": [
                {"input": "a"}, {"input": "b"}, {"input": "c"}, {"input": "d"}
            ]}}
        ], "output": "mix"});
        let project = |clip: serde_json::Value| {
            alpha2_model::Project::from_json(
                &json!({
                    "schema_version": 1, "id": "p", "name": "p",
                    "settings": {"width": 64, "height": 36, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48_000},
                    "sources": [video("a"), video("b"), video("c"), video("d"),
                        {"id": "split", "kind": "composition", "composition": "quad"}],
                    "compositions": [quad],
                    "timeline": {"tracks": [{"id": "t", "name": "V", "kind": "video", "clips": [clip]}]}
                })
                .to_string(),
            )
            .expect("valid")
        };
        let composed = project(json!({
            "id": "c", "source_id": "a", "composition": {"id": "quad"},
            "timeline_range": span(0, 120_000), "source_range": span(0, 120_000)
        }));
        assert_eq!(visual_decode_reads(&composed, range(0, 120_000)).0, 4);
        let source = project(json!({
            "id": "c", "source_id": "split",
            "timeline_range": span(0, 120_000), "source_range": span(0, 120_000)
        }));
        assert_eq!(visual_decode_reads(&source, range(0, 120_000)).0, 4);
    }
}
