//! The per-snapshot interval index that frame and audio evaluation query.
//!
//! The evaluator has no GPU, decoder, filesystem, clock, or global state. A
//! host builds an [`ActiveIndex`] once for an immutable [`Project`] and may then
//! evaluate arbitrary timestamps on any thread.

use crate::{EvalError, IndexedCaptions, IndexedClip, IndexedTransition, IntervalNode, build_tree};
use alpha2_media::SourceId;
use alpha2_model::{CompositionId, Project, ProjectId};
use alpha2_time::TimeRange;

/// A precomputed interval index for a validated project snapshot.
#[derive(Clone, Debug)]
pub struct ActiveIndex {
    pub(crate) project_id: ProjectId,
    pub(crate) nodes: Vec<IntervalNode>,
    pub(crate) root: Option<usize>,
    pub(crate) transitions: Vec<IndexedTransition>,
    /// Caption tracks in track order.
    pub(crate) captions: Vec<IndexedCaptions>,
    /// The most clips active at any one instant.
    pub(crate) max_active: usize,
}

impl ActiveIndex {
    /// Builds an immutable interval tree from a structurally valid project.
    ///
    /// # Errors
    ///
    /// Returns [`EvalError::InvalidProject`] when the model is invalid.
    #[allow(clippy::too_many_lines)]
    pub fn build(project: &Project) -> Result<Self, EvalError> {
        project.validate().map_err(EvalError::InvalidProject)?;
        let mut entries = Vec::new();
        let mut transitions = Vec::new();
        let matte_sources = project.timeline.matte_sources();
        let lookup = crate::composition::Lookup::new(
            &project.compositions,
            &project.sources,
            (project.settings.width, project.settings.height),
        );
        let (plans, source_plans) = compile_plans(project, &lookup);
        let sources = project
            .sources
            .iter()
            .map(|source| (&source.id, source))
            .collect::<std::collections::HashMap<_, _>>();
        let generators = generator_plans(project);
        for (track_order, track) in project.timeline.tracks.iter().enumerate() {
            for (clip_order, clip) in track.clips.iter().enumerate() {
                // Validation guarantees the source exists.
                let source = sources.get(&clip.source_id).copied();
                let source_is_audible = source.is_some_and(alpha2_model::MediaSource::is_audible);
                let generator = source.and_then(|source| source.generator.clone());
                // A composition source is the clip's picture: it is drawn, and so planned, read,
                // and required, only where the clip's own composition draws the clip.
                let drawn = clip
                    .composition
                    .as_ref()
                    .is_none_or(|reference| lookup.reads().draws_clip(&reference.id));
                let source_composition = source
                    .filter(|_| drawn)
                    .and_then(|source| source_composition_of(source, &source_plans));
                entries.push(IndexedClip {
                    clip_id: clip.id.clone(),
                    source_id: clip.source_id.clone(),
                    track_id: track.id.clone(),
                    track_kind: track.kind,
                    timeline_range: clip.timeline_range,
                    source_range: clip.source_range,
                    time_map: clip.time_map,
                    visual: clip.visual.clone(),
                    // Validation guarantees the composition exists.
                    composition: clip.composition.as_ref().and_then(|reference| {
                        plans
                            .get(&reference.id)
                            .map(|plan| crate::IndexedComposition {
                                plan: std::sync::Arc::clone(plan),
                                reference: reference.clone(),
                            })
                    }),
                    blend_mode: clip.blend_mode,
                    cache: clip.cache,
                    masks: clip.masks.clone(),
                    matte: clip.matte.clone(),
                    is_matte: matte_sources.contains(&clip.id),
                    audio: clip.audio.clone(),
                    source_is_audible,
                    generator,
                    plan: generators.get(&clip.source_id).cloned(),
                    source_composition,
                    track_order,
                    clip_order,
                });
            }
            transitions.extend(
                track
                    .transitions
                    .iter()
                    .map(|transition| IndexedTransition {
                        from_clip: transition.from_clip.clone(),
                        to_clip: transition.to_clip.clone(),
                        timeline_range: transition.timeline_range,
                        easing: transition.easing,
                        effect: transition.effect.clone(),
                        track_order,
                    }),
            );
        }
        let captions = project
            .timeline
            .tracks
            .iter()
            .enumerate()
            .filter_map(|(track_order, track)| {
                track.captions.as_ref().map(|captions| IndexedCaptions {
                    track_id: track.id.clone(),
                    track_order,
                    captions: captions.clone(),
                })
            })
            .collect();
        entries.sort_unstable_by_key(|entry| {
            (
                entry.timeline_range.start(),
                entry.track_order,
                entry.clip_order,
            )
        });
        let max_active = deepest_overlap(&entries);
        let (nodes, root) = build_tree(entries);
        transitions.sort_unstable_by_key(|transition| {
            (transition.timeline_range.start(), transition.track_order)
        });
        Ok(Self {
            project_id: project.id.clone(),
            nodes,
            root,
            transitions,
            captions,
            max_active,
        })
    }

    /// Returns the project identity used to build this index.
    #[must_use]
    pub fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    /// Returns the number of clips indexed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Returns whether the index has no clips.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Every effect invocation the project can draw: the steps of each
    /// composition a clip shows or draws as its source (those that compile),
    /// and each transition. A compiled composition is shared by every clip
    /// that shows it, so its steps come once however many clips show it, in
    /// index order of its first use: the walk is linear in the clips and the
    /// distinct compositions, never in their product.
    pub fn effects(&self) -> impl Iterator<Item = &alpha2_model::ClipEffect> {
        let mut seen = std::collections::HashSet::new();
        let plans = self
            .nodes
            .iter()
            .flat_map(|node| clip_plans(node.entry()))
            .flatten()
            .filter(|slot| seen.insert(std::sync::Arc::as_ptr(slot)))
            .filter_map(|slot| slot.as_ref().as_ref().ok())
            .collect::<Vec<_>>();
        plans
            .into_iter()
            .flat_map(crate::CompositionPlan::effects)
            .chain(self.transitions.iter().map(|transition| &transition.effect))
    }

    /// The most sequential reads of host-supplied sources that `counted`
    /// selects at any one instant of `range`, or zero when none is read
    /// there. A clip reads its own source while it draws it
    /// ([`crate::ActiveClip::needs_source`]), and the source of each media
    /// step of its compositions; [`crate::frame_needs`] asks for each of these
    /// frames through its own decoder cursor, so this says how many cursors
    /// decode at once.
    ///
    /// Each compiled composition's reads are counted once however many clips
    /// show it, so this is linear in the clips and the distinct
    /// compositions' steps, never in their product.
    #[must_use]
    pub fn peak_source_reads(
        &self,
        range: TimeRange,
        mut counted: impl FnMut(&SourceId) -> bool,
    ) -> usize {
        // The reads of each compiled composition, by its shared allocation.
        let mut plan_reads = std::collections::HashMap::new();
        let mut events = Vec::new();
        for entry in self.nodes.iter().map(IntervalNode::entry) {
            let Some(span) = entry.timeline_range.intersection(range) else {
                continue;
            };
            let [composition, source_composition] = clip_plans(entry);
            let mut reads = usize::from(draws_own_source(entry) && counted(&entry.source_id));
            for slot in [composition, source_composition].into_iter().flatten() {
                reads += *plan_reads
                    .entry(std::sync::Arc::as_ptr(slot))
                    .or_insert_with(|| {
                        slot.as_ref().as_ref().map_or(0, |plan| {
                            plan.steps
                                .iter()
                                .filter(|step| match &step.op {
                                    crate::StepOp::Media { source_id, .. } => counted(source_id),
                                    _ => false,
                                })
                                .count()
                        })
                    });
            }
            if reads > 0 {
                let delta = isize::try_from(reads).unwrap_or(isize::MAX);
                events.extend([(span.start(), delta), (span.end(), -delta)]);
            }
        }
        // Ranges are half-open: at one instant, the reads ending leave before those starting enter.
        events.sort_unstable_by_key(|&(time, delta)| (time, delta.signum()));
        let mut active = 0_usize;
        let mut peak = 0;
        for (_, delta) in events {
            active = active.saturating_add_signed(delta);
            peak = peak.max(active);
        }
        peak
    }

    /// Every sequential read of a host-supplied source that `counted`
    /// selects over `range`: the reader, named as [`crate::frame_needs`]
    /// names it (the clip, and the composition step reading for it, `/`
    /// before a step of its composition source), and the part of `range` it
    /// may read in, the clip's span there. The reads
    /// [`Self::peak_source_reads`] counts, one entry each, in index order.
    ///
    /// A reader reads only inside its span: a host can close the decoder of
    /// a reader whose span has ended, and tell which open decoder it reads
    /// last. Within its span a reader may still skip frames (a frame another
    /// reader of the same source frame asks for, or a matte no clip names).
    #[must_use]
    pub fn source_reads(
        &self,
        range: TimeRange,
        mut counted: impl FnMut(&SourceId) -> bool,
    ) -> Vec<SourceRead> {
        let mut reads = Vec::new();
        for entry in self.nodes.iter().map(IntervalNode::entry) {
            let Some(span) = entry.timeline_range.intersection(range) else {
                continue;
            };
            let read = |source_id: &SourceId, reader| SourceRead {
                source_id: source_id.clone(),
                clip_id: entry.clip_id.clone(),
                reader,
                span,
            };
            if draws_own_source(entry) && counted(&entry.source_id) {
                reads.push(read(&entry.source_id, None));
            }
            let [composition, source_composition] = clip_plans(entry);
            for (slot, of_source) in [(composition, false), (source_composition, true)] {
                let Some(Ok(plan)) = slot.map(|slot| slot.as_ref().as_ref()) else {
                    continue;
                };
                for step in &plan.steps {
                    let crate::StepOp::Media { source_id, .. } = &step.op else {
                        continue;
                    };
                    if !counted(source_id) {
                        continue;
                    }
                    let reader = if of_source {
                        alpha2_model::NodeId::new(format!("/{}", step.node)).ok()
                    } else {
                        Some(step.node.clone())
                    };
                    reads.push(read(source_id, reader));
                }
            }
        }
        reads
    }
}

/// One reader of a source over an export range ([`ActiveIndex::source_reads`]).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceRead {
    /// The source read.
    pub source_id: SourceId,
    /// The clip that reads it, or shows the composition that does.
    pub clip_id: alpha2_model::ClipId,
    /// The composition step that reads, as [`crate::FrameNeed::reader`]
    /// names it; `None` for the clip's own source.
    pub reader: Option<alpha2_model::NodeId>,
    /// Where on the timeline it may read.
    pub span: TimeRange,
}

/// Whether `entry` draws its own source picture: it has no generator or
/// composition source, and any composition it shows uses its clip.
fn draws_own_source(entry: &IndexedClip) -> bool {
    let [composition, source_composition] = clip_plans(entry);
    entry.generator.is_none()
        && source_composition.is_none()
        && composition.is_none_or(|slot| {
            slot.as_ref()
                .as_ref()
                .is_ok_and(crate::CompositionPlan::uses_clip)
        })
}

/// The compiled compositions `entry` draws: its own, then its composition
/// source's.
fn clip_plans(entry: &IndexedClip) -> [Option<&PlanSlot>; 2] {
    [
        entry
            .composition
            .as_ref()
            .map(|composition| &composition.plan),
        entry.source_composition.as_ref().map(|(_, plan)| plan),
    ]
}

/// The compiled compositions of a project: each a clip shows, and each a
/// composition source shows.
type Plans<'a> = std::collections::HashMap<&'a CompositionId, PlanSlot>;

/// Each composition a clip shows, compiled once however many clips show it,
/// and each a composition source shows, compiled once to draw it. They hold
/// at most [`crate::MAX_PLAN_BYTES`] together: past it none is drawn, so
/// whether a composition draws does not depend on which others were
/// compiled first.
fn compile_plans<'a>(
    project: &'a Project,
    lookup: &crate::composition::Lookup<'a>,
) -> (Plans<'a>, Plans<'a>) {
    let shown = project
        .timeline
        .tracks
        .iter()
        .flat_map(|track| &track.clips)
        .filter_map(|clip| clip.composition.as_ref().map(|reference| &reference.id))
        .collect::<std::collections::HashSet<_>>();
    // The composition sources a clip draws, or a media node may read.
    let sourced = project
        .sources
        .iter()
        .filter_map(|source| source.composition.as_ref())
        .collect::<std::collections::HashSet<_>>();
    let mut budget = crate::composition::PlanBudget::new(crate::MAX_PLAN_BYTES);
    let mut plans = Plans::new();
    let mut source_plans = Plans::new();
    for composition in &project.compositions {
        if shown.contains(&composition.id) {
            let plan =
                crate::composition::plan_composition_within(composition, lookup, &mut budget);
            plans.insert(&composition.id, std::sync::Arc::new(plan));
        }
        if sourced.contains(&composition.id) {
            let plan = crate::composition::plan_source_composition_within(
                composition,
                lookup,
                &mut budget,
            );
            source_plans.insert(&composition.id, std::sync::Arc::new(plan));
        }
    }
    if budget.exceeded() {
        let refused = std::sync::Arc::new(Err(crate::PlanError::TooLarge));
        for plan in plans.values_mut().chain(source_plans.values_mut()) {
            *plan = std::sync::Arc::clone(&refused);
        }
    }
    (plans, source_plans)
}

/// A composition source's composition and its plan.
fn source_composition_of(
    source: &alpha2_model::MediaSource,
    plans: &Plans<'_>,
) -> Option<(CompositionId, PlanSlot)> {
    let id = source.composition.as_ref()?;
    Some((id.clone(), std::sync::Arc::clone(plans.get(id)?)))
}

/// A compiled plan shared by every clip showing its composition, or the reason it cannot draw.
pub(crate) type PlanSlot = std::sync::Arc<Result<crate::CompositionPlan, crate::PlanError>>;

/// The most half-open clip intervals that contain one instant.
fn deepest_overlap(entries: &[IndexedClip]) -> usize {
    // An interval ending where another starts does not overlap it, so at one instant ends (-1)
    // sort before starts (+1).
    let mut edges: Vec<_> = entries
        .iter()
        .flat_map(|entry| {
            [
                (entry.timeline_range.start(), 1_i8),
                (entry.timeline_range.end(), -1),
            ]
        })
        .collect();
    edges.sort_unstable();
    let mut depth = 0_usize;
    let mut deepest = 0;
    for (_, step) in edges {
        depth = depth.saturating_add_signed(isize::from(step));
        deepest = deepest.max(depth);
    }
    deepest
}

/// Each shapes or layout source's tree, planned once however many clips
/// show it.
fn generator_plans(
    project: &Project,
) -> std::collections::HashMap<&SourceId, std::sync::Arc<crate::GeneratorPlan>> {
    project
        .sources
        .iter()
        .filter_map(|source| {
            let plan = crate::GeneratorPlan::new(
                source.generator.as_ref()?,
                (project.settings.width, project.settings.height),
            )?;
            Some((&source.id, std::sync::Arc::new(plan)))
        })
        .collect()
}
