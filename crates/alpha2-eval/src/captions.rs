//! Caption cues built from a `words` data set.

use alpha2_model::{
    CaptionCue, Captions, DataAnchor, DataValues, DataWord, MAX_DATA_WORDS, MAX_DERIVED_CUES,
    ModelError, Project, TimeMap, TrackId, end_cues_where_the_next_starts, group_words,
};
use alpha2_time::{MediaTime, TimeRange};
use std::collections::BTreeMap;

/// The cues a caption track shows, its own or its `from_words` words grouped in timeline time.
///
/// A source-anchored word set contributes through each clip of the named track showing its
/// source with a forward or loop time map (reversed sound is unintelligible and a freeze is
/// silent). A word is mapped through the clip's inverse time map, clipped to the clip, and
/// dropped when its midpoint lies outside the clip's source range. Words are grouped per clip
/// and loop pass, so a cue never spans a cut; where cues of neighbouring clips overlap, each ends
/// where the next starts.
///
/// Every cue passes the checks of an explicit cue, and the cues are ordered and disjoint.
///
/// # Errors
/// [`ModelError::TooManyDerivedCues`] naming `track` when the words map to more than
/// [`alpha2_model::MAX_DATA_WORDS`] instances or group into more than [`MAX_DERIVED_CUES`] cues.
pub fn caption_cues(
    project: &Project,
    track: &TrackId,
    captions: &Captions,
) -> Result<Vec<CaptionCue>, ModelError> {
    let too_many = || ModelError::TooManyDerivedCues {
        track: track.clone(),
    };
    let Some(from) = &captions.from_words else {
        return Ok(captions.cues.clone());
    };
    let Some(set) = project.data.iter().find(|set| set.id == from.data) else {
        return Ok(Vec::new());
    };
    let DataValues::Words(words) = &set.values else {
        return Ok(Vec::new());
    };
    let source = match &set.anchor {
        DataAnchor::Timeline => {
            let cues = group_words(words, from.max_chars);
            return if cues.len() > MAX_DERIVED_CUES {
                Err(too_many())
            } else {
                Ok(cues)
            };
        }
        DataAnchor::Source { source_id } => source_id,
    };
    let Some(words_track) = project
        .timeline
        .tracks
        .iter()
        .find(|candidate| Some(&candidate.id) == from.track.as_ref())
    else {
        return Ok(Vec::new());
    };
    let mut clips = words_track
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
    let mut mapped_words = 0_usize;
    for clip in clips {
        // One group of mapped words per loop pass (a forward clip has one).
        let mut passes: BTreeMap<i64, Vec<DataWord>> = BTreeMap::new();
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
            mapped_words += ranges.len();
            if mapped_words > MAX_DATA_WORDS {
                return Err(too_many());
            }
            for range in ranges {
                let pass = pass_of(clip.timeline_range, clip.source_range, clip.time_map, range);
                passes.entry(pass).or_default().push(DataWord {
                    start: range.start(),
                    end: range.end(),
                    ..word.clone()
                });
            }
        }
        for mut held in passes.into_values() {
            held.sort_by_key(|word| (word.start, word.end));
            cues.extend(group_words(&held, from.max_chars));
            if cues.len() > MAX_DERIVED_CUES {
                return Err(too_many());
            }
        }
    }
    cues.sort_by_key(|cue| cue.timeline_range.start());
    end_cues_where_the_next_starts(&mut cues);
    Ok(cues)
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
