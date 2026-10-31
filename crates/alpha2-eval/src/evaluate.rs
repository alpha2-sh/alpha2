use crate::{
    ActiveCaption, ActiveClip, ActiveComposition, ActiveEffect, ActiveIndex, ActiveTransition,
    EvalError, FrameDesc, FrameNeed, collect_active, map_source_time, sample_visual,
};
use alpha2_model::{Project, TrackKind};
use alpha2_time::{FrameRate, MediaTime, TICKS_PER_SECOND, TimeRange};

/// Evaluates visual and audio clip descriptions at one timeline timestamp.
///
/// The result borrows identities and effects from `index`. Vectors are sized exactly, so an
/// evaluation allocates one scratch list plus one list per non-empty field.
///
/// # Errors
/// Returns an error for mismatched indexes or unrepresentable retiming.
pub fn evaluate<'a>(
    project: &Project,
    index: &'a ActiveIndex,
    time: MediaTime,
) -> Result<FrameDesc<'a>, EvalError> {
    ensure_index_matches(project, index)?;
    let aspect = f64::from(project.settings.width) / f64::from(project.settings.height.max(1));
    // No instant has more clips active than the index's deepest overlap.
    let mut active = Vec::with_capacity(index.max_active);
    if let Some(root) = index.root {
        collect_active(&index.nodes, root, time, &mut active);
    }
    active.sort_unstable_by_key(|entry| (entry.track_order, entry.clip_order));

    let visual_count = active
        .iter()
        .filter(|entry| entry.track_kind == TrackKind::Video)
        .count();
    let audio_count = active
        .iter()
        .filter(|entry| entry.track_kind == TrackKind::Audio)
        .count();
    // A caption lies over the clips of the tracks before it.
    let captions = index
        .captions
        .iter()
        .filter_map(|track| {
            let cues = &track.captions.cues;
            // Cues are ordered and disjoint, so only the last one starting at or before `time` can
            // be active.
            let candidate = cues.partition_point(|cue| cue.timeline_range.start() <= time);
            let index = candidate.checked_sub(1)?;
            let cue = &cues[index];
            cue.timeline_range.contains(time).then(|| ActiveCaption {
                track_id: &track.track_id,
                text: &cue.text,
                local_time: MediaTime(time.ticks() - cue.timeline_range.start().ticks()),
                style: &track.styles[track.cue_styles[index]],
                layers_below: active
                    .iter()
                    .filter(|entry| {
                        entry.track_kind == TrackKind::Video
                            && entry.track_order < track.track_order
                    })
                    .count(),
            })
        })
        .collect();
    let mut visual = Vec::with_capacity(visual_count);
    let mut audio = Vec::with_capacity(audio_count);
    let frame_mirror = crate::Mirror::frame(project.settings.frame_rate);
    for entry in active {
        let item = active_clip(entry, frame_mirror, time, aspect)?;
        match entry.track_kind {
            TrackKind::Video => visual.push(item),
            TrackKind::Audio => audio.push(item),
            _ => {}
        }
    }
    let frame_rate = project.settings.frame_rate;
    let active_transitions = || {
        index
            .transitions
            .iter()
            .take_while(move |transition| transition.timeline_range.start() <= time)
            .filter(move |transition| transition.timeline_range.contains(time))
    };
    let mut transitions = Vec::with_capacity(active_transitions().count());
    transitions.extend(active_transitions().map(|transition| {
        let linear_progress = centred_progress(transition.timeline_range, frame_rate, time);
        // Transition shaders are defined on 0..=1; clamp overshooting curves.
        #[allow(clippy::cast_possible_truncation)]
        let progress = transition.easing.apply(linear_progress).clamp(0.0, 1.0) as f32;
        ActiveTransition {
            from_clip: &transition.from_clip,
            to_clip: &transition.to_clip,
            effect: ActiveEffect::new(&transition.effect, time),
            progress,
        }
    }));
    Ok(FrameDesc {
        time,
        background: Some(project.settings.background),
        blend_space: project.settings.blend_space,
        visual,
        audio,
        transitions,
        captions,
    })
}

fn active_clip(
    entry: &crate::IndexedClip,
    frame_mirror: crate::Mirror,
    time: MediaTime,
    aspect: f64,
) -> Result<ActiveClip<'_>, EvalError> {
    let source_time = map_source_time(
        entry.timeline_range,
        entry.source_range,
        entry.time_map,
        frame_mirror,
        time,
    )
    .ok_or_else(|| EvalError::UnmappableClip {
        clip: entry.clip_id.clone(),
    })?;
    Ok(ActiveClip {
        clip_id: &entry.clip_id,
        source_id: &entry.source_id,
        track_id: &entry.track_id,
        source_time,
        visual: (entry.track_kind == TrackKind::Video)
            .then(|| sample_visual(&entry.visual, time, aspect)),
        composition: active_composition(entry, time)?,
        blend_mode: entry.blend_mode,
        cache: entry.cache,
        masks: &entry.masks,
        matte: entry.matte.as_ref(),
        is_matte: entry.is_matte,
        generator: entry.generator.as_ref(),
        plan: entry.plan.as_deref(),
        source_composition: active_source_composition(entry, source_time)?,
    })
}

fn active_source_composition(
    entry: &crate::IndexedClip,
    source_time: MediaTime,
) -> Result<Option<ActiveComposition<'_>>, EvalError> {
    entry
        .source_composition
        .as_ref()
        .filter(|_| entry.track_kind == TrackKind::Video)
        .map(|(id, plan)| {
            plan.as_ref()
                .as_ref()
                .map(|plan| ActiveComposition::new(plan, source_time))
                .map_err(|reason| EvalError::IncompleteComposition {
                    clip: entry.clip_id.clone(),
                    composition: id.clone(),
                    reason: reason.clone(),
                })
        })
        .transpose()
}

fn active_composition(
    entry: &crate::IndexedClip,
    time: MediaTime,
) -> Result<Option<ActiveComposition<'_>>, EvalError> {
    let Some(indexed) = entry
        .composition
        .as_ref()
        .filter(|_| entry.track_kind == TrackKind::Video)
    else {
        return Ok(None);
    };
    let plan =
        indexed
            .plan
            .as_ref()
            .as_ref()
            .map_err(|reason| EvalError::IncompleteComposition {
                clip: entry.clip_id.clone(),
                composition: indexed.reference.id.clone(),
                reason: reason.clone(),
            })?;
    let local = indexed
        .reference
        .local_time(entry.timeline_range.start(), time)
        .ok_or_else(|| EvalError::UnmappableClip {
            clip: entry.clip_id.clone(),
        })?;
    Ok(Some(ActiveComposition::new(plan, local)))
}

/// A transition's linear progress, `clamp((t - start + F/2) / D, 0, 1)`.
///
/// `F` is one project frame and `D` the duration. Sampling at frame centres makes frame `i` of
/// `N` show `(i + 0.5) / N`, so every frame mixes both clips and reversed playback shows `1 - p`.
fn centred_progress(range: TimeRange, frame_rate: FrameRate, time: MediaTime) -> f64 {
    let elapsed = i128::from(time.ticks()) - i128::from(range.start().ticks());
    let duration = i128::from(range.end().ticks()) - i128::from(range.start().ticks());
    // F / 2 = TICKS_PER_SECOND * den / (2 * num); scale by 2 * num to stay exact until the division.
    let num = i128::from(frame_rate.numerator());
    let den = i128::from(frame_rate.denominator());
    let numerator = 2 * num * elapsed + i128::from(TICKS_PER_SECOND) * den;
    let denominator = 2 * num * duration;
    #[allow(clippy::cast_precision_loss)]
    let progress = (numerator as f64 / denominator as f64).clamp(0.0, 1.0);
    progress
}

/// Describes all source frames needed at a timestamp.
///
/// The list is in evaluation order with no duplicate `(source, timestamp)` pairs. Generated
/// sources need no frames. A visual clip needs its own source only when it draws it
/// ([`ActiveClip::needs_source`]) plus every source its composition reads. A matte clip needs
/// frames only while an active clip names it ([`FrameDesc::draws`]).
///
/// # Errors
///
/// Returns any error emitted by [`evaluate`].
pub fn frame_needs(
    project: &Project,
    index: &ActiveIndex,
    time: MediaTime,
) -> Result<Vec<FrameNeed>, EvalError> {
    let frame = evaluate(project, index, time)?;
    let mut needs = Needs::default();
    for item in frame
        .visual
        .iter()
        .filter(|item| frame.draws(item))
        .chain(&frame.audio)
    {
        if item.needs_source() {
            needs.ask(item.clip_id, item.source_id, item.source_time, || None);
        }
        let plans = item
            .composition
            .iter()
            .map(|composition| (composition, false))
            .chain(
                item.source_composition
                    .iter()
                    .map(|composition| (composition, true)),
            );
        for (composition, of_source) in plans {
            let steps = &composition.plan().steps;
            for (step, source_id, time) in composition.media() {
                let node = &steps[step].node;
                // Prefix source-composition steps with `/`, which no own-composition path starts with.
                needs.ask(item.clip_id, source_id, time, || {
                    if of_source {
                        alpha2_model::NodeId::new(format!("/{node}")).ok()
                    } else {
                        Some(node.clone())
                    }
                });
            }
        }
    }
    Ok(needs.list)
}

/// Needs deduplicated by linear scan before switching to a hash set. Frames usually need only a
/// few sources, but a composition may read tens of thousands.
pub(crate) const SCANNED_NEEDS: usize = 32;

#[cfg(test)]
thread_local! {
    /// Comparisons made by [`Needs::ask`] on this thread.
    pub(crate) static COMPARED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Default)]
struct Needs<'a> {
    list: Vec<FrameNeed>,
    /// Needs past the first [`SCANNED_NEEDS`], borrowing IDs so lookups clone nothing.
    hashed: std::collections::HashSet<(&'a alpha2_media::SourceId, MediaTime)>,
}

impl<'a> Needs<'a> {
    /// Records `source_id` at `media_time` unless already recorded. `reader` runs only for a new
    /// need.
    fn ask(
        &mut self,
        clip_id: &alpha2_model::ClipId,
        source_id: &'a alpha2_media::SourceId,
        media_time: MediaTime,
        reader: impl FnOnce() -> Option<alpha2_model::NodeId>,
    ) {
        let scanned = &self.list[..self.list.len().min(SCANNED_NEEDS)];
        #[cfg(test)]
        COMPARED.with(|compared| compared.set(compared.get() + scanned.len()));
        let repeat = scanned
            .iter()
            .any(|need| need.media_time == media_time && need.source_id == *source_id)
            || (self.list.len() > SCANNED_NEEDS && self.hashed.contains(&(source_id, media_time)));
        if repeat {
            return;
        }
        if self.list.len() >= SCANNED_NEEDS {
            self.hashed.insert((source_id, media_time));
        }
        self.list.push(FrameNeed {
            clip_id: clip_id.clone(),
            reader: reader(),
            source_id: source_id.clone(),
            media_time,
        });
    }
}

pub(crate) fn ensure_index_matches(
    project: &Project,
    index: &ActiveIndex,
) -> Result<(), EvalError> {
    if project.id == index.project_id {
        Ok(())
    } else {
        Err(EvalError::IndexProjectMismatch {
            index_project: index.project_id.clone(),
            evaluated_project: project.id.clone(),
        })
    }
}
