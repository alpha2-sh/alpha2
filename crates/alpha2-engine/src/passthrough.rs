//! Whether an export range is its sources' pictures, untouched.
//!
//! Copying sources' compressed packets into the output is only correct when
//! rendering would reproduce exactly those pictures: opaque clips that tile
//! the whole range one after another, each shown at its own size and speed,
//! with nothing drawn over, under, between, or through them. This decides that
//! from the project alone, before any file is opened. What the files must also
//! satisfy (codec, frame grid, keyframes, color tags, and a bitstream
//! contract shared across the joins) is the native host's to check; a
//! project-level yes here is necessary, not sufficient.
//!
//! The answer is conservative. Anything that could change a pixel or a
//! frame's timing, even an identity-valued keyframe track, refuses, and the
//! refusal names what it saw, so a caller learns why the export will render
//! instead. The audio never refuses: it is classified, and the host copies or
//! encodes it.
//!
//! What the render draws and hears is decided by the same predicates the
//! renderer uses: [`alpha2_model::Timeline::matte_sources`] for the clips it
//! never draws, [`alpha2_model::TrackAudio::is_silenced`] and
//! [`alpha2_model::AudioProperties::is_audible`] for what reaches the mix.

use alpha2_media::SourceId;
use alpha2_model::{
    Animated, Clip, ClipBlendMode, ClipId, MediaKind, Project, TimeMap, Track, TrackKind, Vec2,
};
use alpha2_time::{MediaTime, TimeRange};

/// What an export range copies: its segments in timeline order, and its audio.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PassthroughPlan {
    /// The clips that tile the range, one after another.
    pub segments: Vec<PassthroughSegment>,
    /// What the export's audio is over the range.
    pub audio: PassthroughAudio,
}

/// One clip's share of a copied range.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PassthroughSegment {
    /// The clip whose source is copied.
    pub clip_id: ClipId,
    /// The source copied.
    pub source_id: SourceId,
    /// The part of the export range this clip shows.
    pub timeline_range: TimeRange,
    /// The source interval shown there.
    pub source_range: TimeRange,
}

impl PassthroughSegment {
    /// The source time shown at timeline time `time`, inside this segment.
    #[must_use]
    pub fn source_time(&self, time: MediaTime) -> MediaTime {
        MediaTime::from_ticks(
            self.source_range.start().ticks()
                + (time.ticks() - self.timeline_range.start().ticks()),
        )
    }
}

/// What an export's audio is over a copied range. The picture decides
/// whether a range can be copied; the audio only decides whether it is
/// copied too or encoded from the mix beside the copied video.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PassthroughAudio {
    /// The export carries no audio.
    Absent,
    /// One copied clip's own audio, with nothing processing it and nothing
    /// else heard: the source's audio packets are the mix.
    Untouched,
    /// A mix the engine renders: several clips, processing (gain, pan,
    /// fades, effects, ducking, the master limiter), or silence, where
    /// nothing in the range is heard. The reason says which.
    Mixed(String),
}

/// Whether `range` of `project` shows its sources untouched.
///
/// `range` is the export's effective range, already on the frame grid.
/// `include_audio` is whether the export carries audio.
///
/// # Errors
/// Names the first thing in the project that rendering would change.
pub fn passthrough_plan(
    project: &Project,
    range: TimeRange,
    include_audio: bool,
) -> Result<PassthroughPlan, String> {
    let clips = tiling_clips(project, range)?;
    let mattes = project.timeline.matte_sources();
    let mut segments = Vec::with_capacity(clips.len());
    for &(_, clip) in &clips {
        let source = project
            .sources
            .iter()
            .find(|source| source.id == clip.source_id)
            .ok_or_else(|| format!("clip {} has no source", clip.id))?;
        shows_source_untouched(clip, source)?;
        if mattes.contains(&clip.id) {
            return Err(format!(
                "clip {} is another clip's matte, which is never drawn itself",
                clip.id
            ));
        }
        let timeline_range = TimeRange::new(
            clip.timeline_range.start().max(range.start()),
            clip.timeline_range.end().min(range.end()),
        )
        .map_err(|error| {
            format!(
                "clip {}'s share of the range is invalid: {error:?}",
                clip.id
            )
        })?;
        let offset = timeline_range.start().ticks() - clip.timeline_range.start().ticks();
        let source_start = clip.source_range.start().ticks().saturating_add(offset);
        let source_range = TimeRange::new(
            MediaTime::from_ticks(source_start),
            MediaTime::from_ticks(
                source_start.saturating_add(timeline_range.duration().map_or(0, MediaTime::ticks)),
            ),
        )
        .map_err(|error| format!("the copied source interval is invalid: {error:?}"))?;
        segments.push(PassthroughSegment {
            clip_id: clip.id.clone(),
            source_id: clip.source_id.clone(),
            timeline_range,
            source_range,
        });
    }
    let audio = if include_audio {
        mix_over(project, range, &clips)
    } else {
        PassthroughAudio::Absent
    };
    Ok(PassthroughPlan { segments, audio })
}

/// What an export that carries audio has over `range`, whose picture
/// `tiling` copies.
fn mix_over(project: &Project, range: TimeRange, tiling: &[(&Track, &Clip)]) -> PassthroughAudio {
    let heard = heard_clips(project, range);
    let copied = |clip: &Clip| tiling.iter().any(|(_, tiled)| tiled.id == clip.id);
    if heard.is_empty() {
        return PassthroughAudio::Mixed(
            "nothing in the range is heard, so the mix is silence".to_owned(),
        );
    }
    if let Some((track, clip)) = heard.iter().find(|(_, clip)| !copied(clip)) {
        return PassthroughAudio::Mixed(format!(
            "clip {} on track {} is heard in the range too",
            clip.id, track.id
        ));
    }
    match (&heard[..], tiling) {
        ([(track, clip)], [_]) => heard_untouched(project, track, clip)
            .map_or_else(PassthroughAudio::Mixed, |()| PassthroughAudio::Untouched),
        _ => PassthroughAudio::Mixed("the range's audio joins more than one clip".to_owned()),
    }
}

/// The clips whose sound reaches the mix over `range`, as the renderer
/// decides it: an audible clip of an audible source, on a bus that is not
/// silenced, in the range, or before it on a bus whose effects can ring on
/// after the clip ends.
fn heard_clips(project: &Project, range: TimeRange) -> Vec<(&Track, &Clip)> {
    let any_solo = project.timeline.any_solo();
    let audible_source = |clip: &Clip| {
        project
            .sources
            .iter()
            .any(|source| source.id == clip.source_id && source.is_audible())
    };
    project
        .timeline
        .tracks
        .iter()
        .filter(|track| !track.audio.is_silenced(any_solo))
        .flat_map(|track| track.clips.iter().map(move |clip| (track, clip)))
        .filter(|(track, clip)| {
            let reaches = clip.timeline_range.start() < range.end()
                && (range.start() < clip.timeline_range.end() || !track.audio.effects.is_empty());
            reaches && clip.audio.is_audible() && audible_source(clip)
        })
        .collect()
}

/// The visual clips that tile `range`, in timeline order, with their tracks.
fn tiling_clips(project: &Project, range: TimeRange) -> Result<Vec<(&Track, &Clip)>, String> {
    let overlaps = |other: TimeRange| other.start() < range.end() && range.start() < other.end();
    let mut visual = Vec::new();
    for track in &project.timeline.tracks {
        match track.kind {
            TrackKind::Caption => {
                if track.captions.as_ref().is_some_and(|captions| {
                    captions.cues.iter().any(|cue| overlaps(cue.timeline_range))
                }) {
                    return Err(format!(
                        "caption track {} shows text in the range",
                        track.id
                    ));
                }
            }
            TrackKind::Video => {
                visual.extend(
                    track
                        .clips
                        .iter()
                        .filter(|clip| overlaps(clip.timeline_range))
                        .map(|clip| (track, clip)),
                );
            }
            TrackKind::Audio => {}
            _ => {
                return Err(format!(
                    "track {} is of a kind this check does not know",
                    track.id
                ));
            }
        }
    }
    visual.sort_by_key(|(_, clip)| clip.timeline_range.start());
    let first = visual
        .first()
        .ok_or_else(|| "no visual clip is in the range".to_owned())?;
    if first.1.timeline_range.start() > range.start() {
        return Err(format!(
            "clip {} starts after the range does, so the background would show",
            first.1.id
        ));
    }
    for pair in visual.windows(2) {
        let (before, after) = (pair[0].1, pair[1].1);
        if after.timeline_range.start() < before.timeline_range.end() {
            return Err(format!(
                "clips {} and {} are both visible at once, so the frame is a composite",
                before.id, after.id
            ));
        }
        if after.timeline_range.start() > before.timeline_range.end() {
            return Err(format!(
                "the background shows between clips {} and {}",
                before.id, after.id
            ));
        }
    }
    let last = visual.last().map_or(first.1, |(_, clip)| clip);
    if last.timeline_range.end() < range.end() {
        return Err(format!(
            "clip {} ends before the range does, so the background would show",
            last.id
        ));
    }
    for &(track, clip) in &visual {
        if track.transitions.iter().any(|transition| {
            (transition.from_clip == clip.id || transition.to_clip == clip.id)
                && overlaps(transition.timeline_range)
        }) {
            return Err(format!("clip {} is in a transition in the range", clip.id));
        }
    }
    Ok(visual)
}

/// Whether `clip` shows `source`'s decoded pictures at their own size and
/// speed, with nothing applied to them.
fn shows_source_untouched(clip: &Clip, source: &alpha2_model::MediaSource) -> Result<(), String> {
    if source.kind != MediaKind::Video || source.provider.is_some() {
        return Err(format!(
            "clip {} shows a {} source, not a decoded video file",
            clip.id,
            media_kind(source.kind)
        ));
    }
    // A declared color wins over the stream's tags in a render; a copy keeps the stream's
    // tags.
    if source
        .color_metadata
        .is_some_and(|declared| declared != alpha2_media::ColorMetadata::EXPORT)
    {
        return Err(format!(
            "clip {}'s source declares a color other than the output's, which a render converts",
            clip.id
        ));
    }
    if clip.composition.is_some() {
        return Err(format!(
            "clip {} is shown through a composition graph",
            clip.id
        ));
    }
    if !clip.masks.is_empty() || clip.matte.is_some() {
        return Err(format!("clip {} is masked or matted", clip.id));
    }
    if clip.blend_mode != ClipBlendMode::Normal {
        return Err(format!(
            "clip {} blends with a mode other than normal",
            clip.id
        ));
    }
    let visual_identity = constant(&clip.visual.position) == Some(Vec2::new(0.5, 0.5))
        && clip.visual.anchor == Vec2::new(0.5, 0.5)
        && constant(&clip.visual.scale) == Some(Vec2::new(1.0, 1.0))
        && constant(&clip.visual.rotation_degrees) == Some(0.0)
        && constant(&clip.visual.opacity) == Some(1.0);
    if !visual_identity {
        return Err(format!(
            "clip {} is moved, scaled, rotated, or faded; only a constant identity transform is copied",
            clip.id
        ));
    }
    if clip.time_map != TimeMap::Forward
        || clip.source_range.duration() != clip.timeline_range.duration()
    {
        return Err(format!(
            "clip {} is retimed, reversed, frozen, or looped",
            clip.id
        ));
    }
    Ok(())
}

/// Whether the mix is `clip`'s audio alone, with no gain, pan, fade, effect,
/// ducking, or limiter anywhere between it and the output. `clip` is the only
/// one heard, so its bus is not silenced.
fn heard_untouched(project: &Project, track: &Track, clip: &Clip) -> Result<(), String> {
    let clip_audio = &clip.audio;
    if constant(&clip_audio.gain) != Some(1.0)
        || constant(&clip_audio.pan) != Some(0.0)
        || clip_audio.fade_in.ticks() != 0
        || clip_audio.fade_out.ticks() != 0
        || !clip_audio.effects.is_empty()
    {
        return Err(format!("clip {}'s audio is processed", clip.id));
    }
    if constant(&track.audio.gain) != Some(1.0)
        || constant(&track.audio.pan) != Some(0.0)
        || !track.audio.effects.is_empty()
        || track.audio.ducking.is_some()
    {
        return Err(format!("track {}'s audio bus processes the clip", track.id));
    }
    let master = &project.audio;
    if constant(&master.gain) != Some(1.0) || !master.effects.is_empty() || master.limiter.is_some()
    {
        return Err("the master bus processes the mix (gain, effects, or its limiter)".to_owned());
    }
    Ok(())
}

fn constant<T: Copy>(animated: &Animated<T>) -> Option<T> {
    match animated {
        Animated::Constant { value } => Some(*value),
        Animated::Keyframes { .. } => None,
    }
}

const fn media_kind(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
        MediaKind::Image => "image",
        MediaKind::Generated => "generated",
        MediaKind::Composition => "composition",
    }
}

#[cfg(test)]
#[path = "passthrough_tests.rs"]
mod tests;
