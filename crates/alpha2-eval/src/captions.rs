//! Caption cues built from a `words` data set.

use alpha2_model::{
    CaptionCue, Captions, DataAnchor, DataValues, DataWord, Project, TimeMap, group_words,
};
use alpha2_time::{MediaTime, TimeRange};

/// The cues a caption track shows, its own or its `from_words` words grouped in timeline time.
///
/// A source-anchored word set contributes through each clip of the named track showing its
/// source with a forward or loop time map (reversed sound is unintelligible and a freeze is
/// silent). A word is mapped through the clip's inverse time map, clipped to the clip, and
/// dropped when its midpoint lies outside the clip's source range. Words are grouped per clip
/// and loop pass, so a cue never spans a cut; where cues of neighbouring clips overlap, each ends
/// where the next starts.
#[must_use]
pub fn caption_cues(project: &Project, captions: &Captions) -> Vec<CaptionCue> {
    let Some(from) = &captions.from_words else {
        return captions.cues.clone();
    };
    let Some(set) = project.data.iter().find(|set| set.id == from.data) else {
        return Vec::new();
    };
    let DataValues::Words(words) = &set.values else {
        return Vec::new();
    };
    let source = match &set.anchor {
        DataAnchor::Timeline => return group_words(words, from.max_chars),
        DataAnchor::Source { source_id } => source_id,
    };
    let Some(track) = project
        .timeline
        .tracks
        .iter()
        .find(|track| Some(&track.id) == from.track.as_ref())
    else {
        return Vec::new();
    };
    let mut clips = track
        .clips
        .iter()
        .filter(|clip| {
            &clip.source_id == source
                && matches!(clip.time_map, TimeMap::Forward | TimeMap::Loop { .. })
        })
        .collect::<Vec<_>>();
    clips.sort_by_key(|clip| clip.timeline_range.start());
    let rate = project.settings.frame_rate;
    let mut cues: Vec<CaptionCue> = Vec::new();
    for clip in clips {
        // One group of mapped words per loop pass (a forward clip has one).
        let mut passes: Vec<(i64, Vec<DataWord>)> = Vec::new();
        for word in words {
            let middle =
                MediaTime(word.start.ticks() + (word.end.ticks() - word.start.ticks()) / 2);
            if !clip.source_range.contains(middle) {
                continue;
            }
            let Some(ranges) = crate::source_range_to_timeline(
                clip.timeline_range,
                clip.source_range,
                clip.time_map,
                rate,
                word.start,
                word.end,
            ) else {
                continue;
            };
            for range in ranges {
                let pass = pass_of(clip.timeline_range, clip.source_range, clip.time_map, range);
                let mapped = DataWord {
                    start: range.start(),
                    end: range.end(),
                    ..word.clone()
                };
                match passes.iter_mut().find(|(index, _)| *index == pass) {
                    Some((_, held)) => held.push(mapped),
                    None => passes.push((pass, vec![mapped])),
                }
            }
        }
        passes.sort_by_key(|(pass, _)| *pass);
        for (_, mut held) in passes {
            held.sort_by_key(|word| (word.start, word.end));
            cues.extend(group_words(&held, from.max_chars));
        }
    }
    cues.sort_by_key(|cue| cue.timeline_range.start());
    for index in 1..cues.len() {
        let next = cues[index].timeline_range.start();
        let previous = &mut cues[index - 1];
        if previous.timeline_range.end() > next {
            match TimeRange::new(previous.timeline_range.start(), next) {
                Ok(range) if !range.is_empty() => {
                    previous.timeline_range = range;
                    alpha2_model::clip_runs(previous);
                }
                _ => {
                    previous.timeline_range =
                        TimeRange::new(next, next).unwrap_or(previous.timeline_range);
                }
            }
        }
    }
    cues.retain(|cue| !cue.timeline_range.is_empty());
    cues
}

/// Which loop pass of a clip `range` lies in; zero for other time maps.
fn pass_of(timeline: TimeRange, source: TimeRange, map: TimeMap, range: TimeRange) -> i64 {
    match (map, source.duration()) {
        (TimeMap::Loop { .. }, Some(length)) if length.ticks() > 0 => {
            (range.start().ticks() - timeline.start().ticks()) / length.ticks()
        }
        _ => 0,
    }
}
