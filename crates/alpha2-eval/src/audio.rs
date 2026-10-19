use crate::{ActiveIndex, EvalError, IndexedClip, collect_overlapping, count_overlapping};
use alpha2_media::SourceId;
use alpha2_model::{AudioProperties, ClipId, Project, TimeMap, TrackId};
use alpha2_time::{MediaTime, TimeRange};

/// One audio clip overlapping an evaluated timeline span.
///
/// Identities and authored properties are borrowed from the
/// [`crate::ActiveIndex`] that produced the span.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActiveAudioClip<'a> {
    /// Active clip identity.
    pub clip_id: &'a ClipId,
    /// Media source needed for this clip.
    pub source_id: &'a SourceId,
    /// Track containing the clip.
    pub track_id: &'a TrackId,
    /// Position of the containing track in project order.
    pub track_order: usize,
    /// Position of the clip in its track's clip list.
    pub clip_order: usize,
    /// Whether the containing track composites this clip visually as well.
    ///
    /// A video clip with an audio stream is reported here and by
    /// [`crate::evaluate`], so a host mixes its sound without a second clip.
    pub visual_track: bool,
    /// Whole timeline interval occupied by the clip.
    pub timeline_range: TimeRange,
    /// Whole source interval consumed by the clip.
    pub source_range: TimeRange,
    /// How the clip's timeline time maps onto its source range.
    pub time_map: TimeMap,
    /// Part of the evaluated span this clip covers.
    pub active_range: TimeRange,
    /// Authored presentation of the immutable project snapshot.
    pub audio: &'a AudioProperties,
}

impl ActiveAudioClip<'_> {
    /// Returns the rate at which the clip consumes its source, forward or
    /// backward: the source-to-timeline duration ratio, one for a loop, and
    /// zero for a frozen clip.
    ///
    /// One means untouched playback. Two consumes source twice as fast, which
    /// raises pitch by an octave because the mixer resamples rather than
    /// time-stretches.
    #[must_use]
    pub fn speed(&self) -> f64 {
        match self.time_map {
            TimeMap::Loop { .. } => return 1.0,
            TimeMap::Freeze { .. } => return 0.0,
            TimeMap::Forward | TimeMap::Reverse => {}
        }
        let timeline = self.timeline_range.duration().map_or(0, MediaTime::ticks);
        let source = self.source_range.duration().map_or(0, MediaTime::ticks);
        if timeline <= 0 || source <= 0 {
            return 1.0;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            source as f64 / timeline as f64
        }
    }

    /// Maps a timeline timestamp inside [`Self::timeline_range`] to source time.
    #[must_use]
    pub fn source_time_at(&self, time: MediaTime) -> Option<MediaTime> {
        crate::map_source_time(
            self.timeline_range,
            self.source_range,
            self.time_map,
            crate::Mirror::TICK,
            time,
        )
    }
}

/// Audio activity over one half-open timeline span.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioSpanDesc<'a> {
    /// Span evaluated on the project timeline.
    pub range: TimeRange,
    /// Clips overlapping the span, ordered by track, start time, and then
    /// position in the track.
    pub clips: Vec<ActiveAudioClip<'a>>,
}

/// Evaluates every audible clip overlapping a half-open timeline span.
///
/// Clips on audio tracks always qualify. A clip on a visual track qualifies
/// when its source declares an audio stream, so one clip carries both picture
/// and sound. Overlapping clips are all reported; the mixer sums them.
///
/// The span is the mixer's block, so clips are reported with their whole
/// authored ranges plus the part of the span they cover. Gain, pan, and fade
/// curves stay authored rather than sampled here; a mixer evaluates them per
/// sample so that automation is independent of the host block size.
///
/// The span borrows identities and properties from `index`. It allocates its
/// working list and its clip list, each sized exactly; a caller evaluating
/// span after span uses [`evaluate_audio_in`] to allocate nothing.
///
/// # Errors
/// Returns [`EvalError::IndexProjectMismatch`] when the index was built for another
/// project, and [`EvalError::UnmappableClip`] for unrepresentable retiming.
pub fn evaluate_audio<'a>(
    project: &Project,
    index: &'a ActiveIndex,
    range: TimeRange,
) -> Result<AudioSpanDesc<'a>, EvalError> {
    crate::ensure_index_matches(project, index)?;
    let capacity = index.root.map_or(0, |root| {
        count_overlapping(&index.nodes, root, range, &|entry| entry.source_is_audible)
    });
    let mut scratch = AudioSpanScratch {
        nodes: Vec::with_capacity(capacity),
    };
    let span = fill_span(index, range, &mut scratch)?;
    Ok(AudioSpanDesc {
        range,
        clips: span.iter().collect(),
    })
}

/// Reusable working memory for [`evaluate_audio_in`].
///
/// It holds positions in an index rather than borrows of one, so a mixer keeps
/// a single scratch for its whole life, across snapshots. Once it has held as
/// many clips as a span needs, evaluating another span allocates nothing.
#[derive(Clone, Debug, Default)]
pub struct AudioSpanScratch {
    nodes: Vec<usize>,
}

/// The clips of one span, as [`evaluate_audio`] reports them, held in an
/// [`AudioSpanScratch`] and read back without copying.
#[derive(Clone, Copy, Debug)]
pub struct AudioSpan<'a, 's> {
    index: &'a ActiveIndex,
    range: TimeRange,
    nodes: &'s [usize],
}

impl<'a> AudioSpan<'a, '_> {
    /// Span evaluated on the project timeline.
    #[must_use]
    pub const fn range(&self) -> TimeRange {
        self.range
    }

    /// Number of clips in the span.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether no audible clip overlaps the span.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The clips in mix order: by track, start time, and then position in
    /// the track.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = ActiveAudioClip<'a>> {
        let (index, range) = (self.index, self.range);
        self.nodes
            .iter()
            .map(move |&node| active_clip(index.nodes[node].entry(), range))
    }
}

/// Evaluates the same clips as [`evaluate_audio`] into reusable memory.
///
/// # Errors
/// Returns any error emitted by [`evaluate_audio`].
pub fn evaluate_audio_in<'a, 's>(
    project: &Project,
    index: &'a ActiveIndex,
    range: TimeRange,
    scratch: &'s mut AudioSpanScratch,
) -> Result<AudioSpan<'a, 's>, EvalError> {
    crate::ensure_index_matches(project, index)?;
    fill_span(index, range, scratch)
}

/// Collects the nodes of every audible, mappable clip overlapping `range`,
/// sorted into mix order.
fn fill_span<'a, 's>(
    index: &'a ActiveIndex,
    range: TimeRange,
    scratch: &'s mut AudioSpanScratch,
) -> Result<AudioSpan<'a, 's>, EvalError> {
    let nodes = &mut scratch.nodes;
    nodes.clear();
    let mix_order = |entry: &IndexedClip| {
        (
            entry.track_order,
            entry.timeline_range.start(),
            entry.clip_order,
        )
    };
    // The first clip in mix order that cannot be mapped, if any.
    let mut unmappable: Option<&IndexedClip> = None;
    if let Some(root) = index.root {
        collect_overlapping(&index.nodes, root, range, &mut |node, entry| {
            // Audio-track clips always qualify; a visual clip needs a source that declares an
            // audio stream.
            if !entry.source_is_audible {
                return;
            }
            let Some(active_range) = entry.timeline_range.intersection(range) else {
                return;
            };
            if active_range.is_empty() {
                return;
            }
            // Reject retiming the mixer could not map, matching visual evaluation.
            if crate::map_source_time(
                entry.timeline_range,
                entry.source_range,
                entry.time_map,
                crate::Mirror::TICK,
                active_range.start(),
            )
            .is_none()
            {
                if unmappable.is_none_or(|first| mix_order(entry) < mix_order(first)) {
                    unmappable = Some(entry);
                }
                return;
            }
            nodes.push(node);
        });
    }
    if let Some(entry) = unmappable {
        return Err(EvalError::UnmappableClip {
            clip: entry.clip_id.clone(),
        });
    }
    nodes.sort_unstable_by_key(|&node| mix_order(index.nodes[node].entry()));
    Ok(AudioSpan {
        index,
        range,
        nodes,
    })
}

/// Describes one clip that [`fill_span`] kept: it overlaps `range`.
fn active_clip(entry: &IndexedClip, range: TimeRange) -> ActiveAudioClip<'_> {
    ActiveAudioClip {
        clip_id: &entry.clip_id,
        source_id: &entry.source_id,
        track_id: &entry.track_id,
        track_order: entry.track_order,
        clip_order: entry.clip_order,
        visual_track: entry.track_kind == alpha2_model::TrackKind::Video,
        timeline_range: entry.timeline_range,
        source_range: entry.source_range,
        time_map: entry.time_map,
        active_range: entry.timeline_range.intersection(range).unwrap_or(range),
        audio: &entry.audio,
    }
}
