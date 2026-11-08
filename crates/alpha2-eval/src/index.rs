//! The per-snapshot interval index that frame and audio evaluation query.
//!
//! The evaluator has no GPU, decoder, filesystem, clock or global state. A host builds an
//! [`ActiveIndex`] once per immutable [`Project`] and may evaluate any timestamp on any thread.

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
    /// The project's data sets and clips by id, for data bindings.
    pub(crate) data: crate::DataIndex,
}

impl ActiveIndex {
    /// The data index for sampling `project`'s timeline-time values; `project` must be the one
    /// this index was built from.
    #[must_use]
    pub fn sample_context<'a>(
        &'a self,
        project: &'a alpha2_model::Project,
    ) -> crate::SampleContext<'a> {
        crate::SampleContext::timeline(project, &self.data)
    }

    /// The project's data sets and clips by id.
    #[must_use]
    pub const fn data(&self) -> &crate::DataIndex {
        &self.data
    }

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
                // A composition source is planned only if the clip's own composition draws the clip.
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
                track.captions.as_ref().map(|captions| {
                    let mut shown = captions.clone();
                    if shown.from_words.is_some() {
                        shown.cues = crate::caption_cues(project, captions);
                    }
                    IndexedCaptions::new(track.id.clone(), track_order, shown)
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
            data: crate::DataIndex::new(project),
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

    /// Every effect invocation the project can draw: steps of each compiled composition (once
    /// each, in order of first use) and each transition.
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

    /// Peak number of concurrent host-source reads selected by `counted` in `range`, or 0.
    ///
    /// Each read (a clip's own source per [`crate::ActiveClip::needs_source`], or a media step of
    /// its compositions) gets its own decoder cursor in [`crate::frame_needs`], so this is the
    /// number of decoders active at once.
    #[must_use]
    pub fn peak_source_reads(
        &self,
        range: TimeRange,
        mut counted: impl FnMut(&SourceId) -> bool,
    ) -> usize {
        // Keyed by the shared plan allocation so each composition is counted once.
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
        // Half-open ranges: ends sort before starts at the same instant.
        events.sort_unstable_by_key(|&(time, delta)| (time, delta.signum()));
        let mut active = 0_usize;
        let mut peak = 0;
        for (_, delta) in events {
            active = active.saturating_add_signed(delta);
            peak = peak.max(active);
        }
        peak
    }

    /// Every host-source read selected by `counted` over `range`, in index order, with readers
    /// named as in [`crate::frame_needs`]. These are the reads [`Self::peak_source_reads`] counts.
    ///
    /// A reader reads only inside its span, so a host can close its decoder once the span ends.
    /// Within the span it may still skip frames (deduplicated needs, unused mattes).
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
    /// The clip that reads it or shows the reading composition.
    pub clip_id: alpha2_model::ClipId,
    /// The reading composition step as in [`crate::FrameNeed::reader`]; `None` for the clip.
    pub reader: Option<alpha2_model::NodeId>,
    /// Where on the timeline it may read.
    pub span: TimeRange,
}

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

/// The compiled compositions `entry` draws: its own, then its composition source's.
fn clip_plans(entry: &IndexedClip) -> [Option<&PlanSlot>; 2] {
    [
        entry
            .composition
            .as_ref()
            .map(|composition| &composition.plan),
        entry.source_composition.as_ref().map(|(_, plan)| plan),
    ]
}

type Plans<'a> = std::collections::HashMap<&'a CompositionId, PlanSlot>;

/// Compiles each used composition once. Together they hold at most [`crate::MAX_PLAN_BYTES`];
/// past that none draws, so the outcome does not depend on compile order.
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

fn source_composition_of(
    source: &alpha2_model::MediaSource,
    plans: &Plans<'_>,
) -> Option<(CompositionId, PlanSlot)> {
    let id = source.composition.as_ref()?;
    Some((id.clone(), std::sync::Arc::clone(plans.get(id)?)))
}

/// A compiled plan shared by every clip showing its composition, or the reason it cannot draw.
pub(crate) type PlanSlot = std::sync::Arc<Result<crate::CompositionPlan, crate::PlanError>>;

fn deepest_overlap(entries: &[IndexedClip]) -> usize {
    // Half-open intervals: ends (-1) sort before starts (+1) at the same instant.
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

/// Plans each shapes or layout source once, however many clips show it.
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
