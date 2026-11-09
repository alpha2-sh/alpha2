//! Where data can be bound. A value's type decides the kinds it can follow, and the clock its
//! keyframes are on decides the anchors.

use crate::animation::{Keyed, PlaceType, visit_keyed};
use crate::{
    BindingProblem, Clip, ClipId, Composition, CompositionId, DataAnchor, DataBinding, DataKind,
    DataOwner, DataSet, DataSetId, DataSpace, DataVia, EffectParamType, Generator, MediaKind,
    MediaSource, ModelError, NodeOp, Project, TrackKind, VisualProperties,
};
use alpha2_media::SourceId;
use std::collections::HashMap;

/// The clock a value's keyframes are on.
#[derive(Copy, Clone)]
enum Clock<'a> {
    /// Timeline time.
    Timeline,
    /// Composition time in this composition.
    Composition(&'a Composition),
    /// A generator's picture time, or a text's own time from its holder's start.
    Picture,
}

/// The type of the value a binding sits on.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Slot {
    Point,
    Vec2,
    Scalar,
    /// An effect parameter of a known type, or of a host effect whose type the model lacks.
    Param(Option<EffectParamType>),
    Other,
}

/// A canvas-space binding: `follower`'s value depends on `via`'s visual.
struct Follow<'a> {
    follower: &'a ClipId,
    via: &'a ClipId,
    at: String,
}

struct Places<'a> {
    data: HashMap<&'a DataSetId, &'a DataSet>,
    clips: HashMap<&'a ClipId, (&'a Clip, TrackKind)>,
    compositions: HashMap<&'a CompositionId, &'a Composition>,
    follows: Vec<Follow<'a>>,
}

/// Checks every data set, then every binding, correction and caption words reference.
pub(crate) fn validate_data(
    project: &Project,
    sources: &HashMap<&SourceId, &MediaSource>,
) -> Result<(), ModelError> {
    if project.data.len() > crate::MAX_DATA_SETS {
        return Err(ModelError::TooManyDataSets {
            found: project.data.len(),
            maximum: crate::MAX_DATA_SETS,
        });
    }
    let mut data = HashMap::with_capacity(project.data.len());
    for set in &project.data {
        if data.insert(&set.id, set).is_some() {
            return Err(ModelError::DuplicateDataSetId(set.id.clone()));
        }
        let invalid = |problem| ModelError::InvalidDataSet {
            data: set.id.clone(),
            problem,
        };
        let duration = match &set.anchor {
            DataAnchor::Source { source_id } => {
                let source = sources
                    .get(source_id)
                    .ok_or_else(|| invalid(crate::DataProblem::UnknownSource(source_id.clone())))?;
                if !matches!(
                    source.kind,
                    MediaKind::Video | MediaKind::Audio | MediaKind::Image
                ) {
                    return Err(invalid(crate::DataProblem::SourceWithoutMedia(
                        source_id.clone(),
                    )));
                }
                source
                    .fingerprint
                    .as_ref()
                    .and_then(|fingerprint| fingerprint.duration_ticks)
            }
            DataAnchor::Timeline => None,
        };
        set.validate(duration).map_err(invalid)?;
    }
    let mut places = Places {
        data,
        clips: project
            .timeline
            .tracks
            .iter()
            .flat_map(|track| {
                track
                    .clips
                    .iter()
                    .map(move |clip| (&clip.id, (clip, track.kind)))
            })
            .collect(),
        compositions: project
            .compositions
            .iter()
            .map(|composition| (&composition.id, composition))
            .collect(),
        follows: Vec::new(),
    };
    places.walk_timeline(project)?;
    places.walk_compositions(project)?;
    places.walk_sources(project)?;
    places.ensure_follows_are_acyclic()
}

/// The slot of a visited value; effect parameters are resolved by the caller.
const fn slot_of(place: PlaceType) -> Slot {
    match place {
        PlaceType::Point => Slot::Point,
        PlaceType::Vec2 => Slot::Vec2,
        PlaceType::Scalar => Slot::Scalar,
        PlaceType::Effect => Slot::Param(None),
        PlaceType::Other => Slot::Other,
    }
}

/// Calls `visit` with each data binding of a visual, by property path.
fn visual_bindings<'v>(
    prefix: &str,
    visual: &'v VisualProperties,
    visit: &mut dyn FnMut(&str, &'v DataBinding, PlaceType),
) {
    if let Some(binding) = visual.position.data() {
        visit(&format!("{prefix}.position"), binding, PlaceType::Point);
    }
    if let Some(binding) = visual.scale.data() {
        visit(&format!("{prefix}.scale"), binding, PlaceType::Vec2);
    }
    if let Some(binding) = visual.rotation_degrees.data() {
        visit(
            &format!("{prefix}.rotation_degrees"),
            binding,
            PlaceType::Scalar,
        );
    }
    if let Some(binding) = visual.opacity.data() {
        visit(&format!("{prefix}.opacity"), binding, PlaceType::Scalar);
    }
}

/// The declared type of a built-in effect's parameter, or `None` for a host effect.
fn param_type(effect: &crate::ClipEffect, name: &str) -> Option<EffectParamType> {
    crate::builtin_effect(&effect.id)
        .and_then(|spec| spec.param(name))
        .map(|param| param.param_type)
}

/// Collects `(path, binding, place)` of every non-constant value a walker reports.
fn collect(
    walk: impl FnOnce(&mut dyn FnMut(&str, Keyed<'_>)),
) -> Vec<(String, DataBinding, PlaceType)> {
    let mut found = Vec::new();
    walk(&mut |at, keyed| {
        if let Keyed::Data(binding, place) = keyed {
            found.push((at.to_owned(), binding.clone(), place));
        }
    });
    found
}

impl<'a> Places<'a> {
    #[allow(clippy::too_many_lines)]
    fn walk_timeline(&mut self, project: &'a Project) -> Result<(), ModelError> {
        for track in &project.timeline.tracks {
            for clip in &track.clips {
                let owner = DataOwner::Clip(clip.id.clone());
                let mut found = Vec::new();
                visual_bindings("visual", &clip.visual, &mut |at, binding, place| {
                    found.push((at.to_owned(), binding.clone(), place));
                });
                for (index, mask) in clip.masks.iter().enumerate() {
                    found.extend(collect(|visit| {
                        mask.shape.for_each_keyed(
                            &format!("masks[{index}].shape"),
                            &mut |at, keyed| {
                                visit(at, keyed);
                            },
                        );
                        visit_keyed(
                            &format!("masks[{index}].feather"),
                            &mask.feather,
                            &mut |at, keyed| {
                                visit(at, keyed);
                            },
                        );
                    }));
                }
                found.extend(collect(|visit| {
                    visit_keyed("audio.gain", &clip.audio.gain, &mut |at, keyed| {
                        visit(at, keyed);
                    });
                    visit_keyed("audio.pan", &clip.audio.pan, &mut |at, keyed| {
                        visit(at, keyed);
                    });
                }));
                for (at, binding, place) in &found {
                    self.check(
                        &owner,
                        at,
                        Clock::Timeline,
                        slot_of(*place),
                        binding,
                        Some(&clip.id),
                    )?;
                }
                if let Some(correction) = &clip.visual.correction {
                    self.check_correction(&owner, clip, &correction.data)?;
                }
            }
            for transition in &track.transitions {
                let owner = DataOwner::Transition {
                    track: track.id.clone(),
                    from: transition.from_clip.clone(),
                    to: transition.to_clip.clone(),
                };
                for (name, value) in &transition.effect.params {
                    if let Some(binding) = value.data() {
                        let slot = Slot::Param(param_type(&transition.effect, name));
                        let at = format!("effect.params.{name}");
                        self.check(&owner, &at, Clock::Timeline, slot, binding, None)?;
                    }
                }
            }
            let owner = DataOwner::Track(track.id.clone());
            for (at, value) in [
                ("audio.gain", &track.audio.gain),
                ("audio.pan", &track.audio.pan),
            ] {
                if let Some(binding) = value.data() {
                    self.check(&owner, at, Clock::Timeline, Slot::Scalar, binding, None)?;
                }
            }
            if let Some(captions) = &track.captions {
                let mut found = collect(|visit| {
                    crate::text_animation::for_each_keyed_animator(
                        &captions.style.animators,
                        "captions.style.animators",
                        &mut |at, keyed| visit(at, keyed),
                    );
                    for (name, patch) in &captions.styles {
                        if let Some(animators) = &patch.animators {
                            crate::text_animation::for_each_keyed_animator(
                                animators,
                                &format!("captions.styles.{name}.animators"),
                                &mut |at, keyed| visit(at, keyed),
                            );
                        }
                    }
                });
                for (at, binding, place) in found.drain(..) {
                    self.check(&owner, &at, Clock::Picture, slot_of(place), &binding, None)?;
                }
                if let Some(words) = &captions.from_words {
                    self.check_words(project, &owner, words)?;
                }
            }
        }
        if let Some(binding) = project.audio.gain.data() {
            self.check(
                &DataOwner::Master,
                "audio.gain",
                Clock::Timeline,
                Slot::Scalar,
                binding,
                None,
            )?;
        }
        Ok(())
    }

    fn walk_compositions(&mut self, project: &'a Project) -> Result<(), ModelError> {
        for composition in &project.compositions {
            for node in &composition.nodes {
                let owner = DataOwner::Node {
                    composition: composition.id.clone(),
                    node: node.id.clone(),
                };
                if let NodeOp::Transform { transform, .. } = &node.op
                    && transform.correction.is_some()
                {
                    return Err(ModelError::InvalidDataBinding {
                        owner,
                        at: "transform.correction".to_owned(),
                        problem: BindingProblem::FieldNotApplicable("correction"),
                    });
                }
                let found = collect(|visit| {
                    crate::lint::node_keyed(&node.op, &mut |at, keyed| visit(at, keyed));
                });
                for (at, binding, place) in found {
                    let clock = if at.starts_with("generator.") {
                        Clock::Picture
                    } else {
                        Clock::Composition(composition)
                    };
                    let slot = match place {
                        PlaceType::Effect => Slot::Param(self.node_param_type(&node.op, &at)),
                        other => slot_of(other),
                    };
                    self.check(&owner, &at, clock, slot, &binding, None)?;
                }
            }
        }
        Ok(())
    }

    fn walk_sources(&mut self, project: &'a Project) -> Result<(), ModelError> {
        for source in &project.sources {
            let Some(generator) = &source.generator else {
                continue;
            };
            let owner = DataOwner::Source(source.id.clone());
            let found = collect(|visit| match generator {
                Generator::Shapes { items } => {
                    crate::shapes::for_each_keyed_item(items, "items", &mut |at, keyed| {
                        visit(at, keyed);
                    });
                }
                Generator::Layout { root } => {
                    root.for_each_keyed("root", &mut |at, keyed| visit(at, keyed));
                }
                Generator::Text { style, .. } => crate::text_animation::for_each_keyed_animator(
                    &style.animators,
                    "style.animators",
                    &mut |at, keyed| visit(at, keyed),
                ),
                Generator::Fill { .. } => {}
            });
            for (at, binding, place) in found {
                self.check(&owner, &at, Clock::Picture, slot_of(place), &binding, None)?;
            }
        }
        Ok(())
    }

    /// The type of an effect node's parameter or a group node's control at path `at`.
    fn node_param_type(&self, op: &NodeOp, at: &str) -> Option<EffectParamType> {
        match op {
            NodeOp::Effect { effect, .. } | NodeOp::Transition { effect, .. } => {
                param_type(effect, at.strip_prefix("effect.params.")?)
            }
            NodeOp::Group { composition, .. } => {
                let name = at.strip_prefix("controls.")?;
                let group = self.compositions.get(composition)?;
                let control = group.controls.iter().find(|control| control.name == name)?;
                let target = group.nodes.iter().find(|node| node.id == control.node)?;
                match &target.op {
                    NodeOp::Effect { effect, .. } | NodeOp::Transition { effect, .. } => {
                        param_type(effect, &control.param)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn data_set(
        &self,
        owner: &DataOwner,
        at: &str,
        id: &DataSetId,
    ) -> Result<&'a DataSet, ModelError> {
        self.data
            .get(id)
            .copied()
            .ok_or_else(|| ModelError::InvalidDataBinding {
                owner: owner.clone(),
                at: at.to_owned(),
                problem: BindingProblem::UnknownData,
            })
    }

    /// Checks one binding at `at`; `follower` owns the value, for the canvas dependency graph.
    #[allow(clippy::too_many_lines)]
    fn check(
        &mut self,
        owner: &DataOwner,
        at: &str,
        clock: Clock<'a>,
        slot: Slot,
        binding: &DataBinding,
        follower: Option<&'a ClipId>,
    ) -> Result<(), ModelError> {
        let refuse = |problem| ModelError::InvalidDataBinding {
            owner: owner.clone(),
            at: at.to_owned(),
            problem,
        };
        let set = self.data_set(owner, at, &binding.data)?;
        let kind = set.kind();
        let accepted: &[DataKind] = match slot {
            Slot::Point | Slot::Vec2 | Slot::Param(Some(EffectParamType::Vec2) | None) => {
                &[DataKind::Points, DataKind::Scalars]
            }
            Slot::Scalar | Slot::Param(Some(EffectParamType::Float)) => &[DataKind::Scalars],
            Slot::Param(Some(_)) | Slot::Other => {
                return Err(refuse(BindingProblem::TypeRefusesData));
            }
        };
        if !accepted.contains(&kind) {
            return Err(refuse(BindingProblem::WrongKind(kind)));
        }
        let samples = set
            .values
            .samples()
            .expect("points and scalars are sampled");
        if samples.channel(&binding.channel).is_none() {
            return Err(refuse(BindingProblem::UnknownChannel));
        }
        if !binding.fields_valid() {
            return Err(refuse(BindingProblem::InvalidRemap));
        }
        let points = kind == DataKind::Points;
        if points && binding.remap.is_some() {
            return Err(refuse(BindingProblem::FieldNotApplicable("remap")));
        }
        if !points && binding.offset.is_some() {
            return Err(refuse(BindingProblem::FieldNotApplicable("offset")));
        }
        if !points && binding.space.is_some() {
            return Err(refuse(BindingProblem::FieldNotApplicable("space")));
        }
        let source = match (&set.anchor, clock) {
            (_, Clock::Picture) | (DataAnchor::Timeline, Clock::Composition(_)) => {
                return Err(refuse(BindingProblem::ClockRefusesAnchor));
            }
            (DataAnchor::Timeline, Clock::Timeline) => {
                if binding.via.is_some() {
                    return Err(refuse(BindingProblem::ViaForbidden));
                }
                if binding.space.is_some() {
                    return Err(refuse(BindingProblem::FieldNotApplicable("space")));
                }
                return Ok(());
            }
            (DataAnchor::Source { source_id }, _) => source_id,
        };
        let via = binding
            .via
            .as_ref()
            .ok_or_else(|| refuse(BindingProblem::ViaRequired))?;
        if points && binding.space.is_none() {
            return Err(refuse(BindingProblem::SpaceRequired));
        }
        match (via, clock) {
            (DataVia::Clip(clip), Clock::Timeline) => {
                let (clip, kind) = self
                    .clips
                    .get(clip)
                    .copied()
                    .ok_or_else(|| refuse(BindingProblem::UnknownViaClip))?;
                if &clip.source_id != source {
                    return Err(refuse(BindingProblem::ViaShowsOtherSource));
                }
                if binding.space == Some(DataSpace::Canvas) {
                    if kind != TrackKind::Video {
                        return Err(refuse(BindingProblem::ViaWrongReader));
                    }
                    match follower {
                        Some(follower) if follower == &clip.id => {
                            return Err(refuse(BindingProblem::Cycle));
                        }
                        Some(follower) => self.follows.push(Follow {
                            follower,
                            via: &clip.id,
                            at: at.to_owned(),
                        }),
                        None => {}
                    }
                }
                Ok(())
            }
            (DataVia::Node(node), Clock::Composition(composition)) => {
                let reads = composition
                    .nodes
                    .iter()
                    .find(|candidate| &candidate.id == node);
                let Some(NodeOp::Media { source_id, .. }) = reads.map(|node| &node.op) else {
                    return Err(refuse(BindingProblem::InvalidViaNode));
                };
                if source_id != source {
                    return Err(refuse(BindingProblem::ViaShowsOtherSource));
                }
                if binding.space == Some(DataSpace::Canvas) {
                    return Err(refuse(BindingProblem::FieldNotApplicable("space")));
                }
                Ok(())
            }
            _ => Err(refuse(BindingProblem::ViaWrongReader)),
        }
    }

    fn check_correction(
        &self,
        owner: &DataOwner,
        clip: &Clip,
        id: &DataSetId,
    ) -> Result<(), ModelError> {
        let at = "visual.correction";
        let set = self.data_set(owner, at, id)?;
        let refuse = |problem| ModelError::InvalidDataBinding {
            owner: owner.clone(),
            at: at.to_owned(),
            problem,
        };
        if set.kind() != DataKind::Similarity {
            return Err(refuse(BindingProblem::WrongKind(set.kind())));
        }
        if set.anchor.source() != Some(&clip.source_id) {
            return Err(refuse(BindingProblem::NotOwnSource));
        }
        Ok(())
    }

    fn check_words(
        &self,
        project: &Project,
        owner: &DataOwner,
        words: &crate::CaptionsFromWords,
    ) -> Result<(), ModelError> {
        let at = "captions.from_words";
        let set = self.data_set(owner, at, &words.data)?;
        let refuse = |problem| ModelError::InvalidDataBinding {
            owner: owner.clone(),
            at: at.to_owned(),
            problem,
        };
        if set.kind() != DataKind::Words {
            return Err(refuse(BindingProblem::WrongKind(set.kind())));
        }
        let track_ok = match (&set.anchor, &words.track) {
            (DataAnchor::Source { .. }, Some(track)) => project
                .timeline
                .tracks
                .iter()
                .any(|candidate| &candidate.id == track && candidate.kind != TrackKind::Caption),
            (DataAnchor::Timeline, None) => true,
            _ => false,
        };
        if !track_ok || words.max_chars == 0 {
            return Err(refuse(BindingProblem::InvalidCaptionTrack));
        }
        Ok(())
    }

    /// Refuses a cycle of canvas-space follows, naming the first follow on it in document order.
    fn ensure_follows_are_acyclic(&self) -> Result<(), ModelError> {
        let mut next: HashMap<&ClipId, Vec<&ClipId>> = HashMap::new();
        for follow in &self.follows {
            next.entry(follow.follower).or_default().push(follow.via);
        }
        // Colors: absent unvisited, false on the walk, true finished.
        let mut state: HashMap<&ClipId, bool> = HashMap::new();
        let mut on_cycle = std::collections::HashSet::new();
        for follow in &self.follows {
            let mut stack = vec![(follow.follower, 0_usize)];
            if state.contains_key(follow.follower) {
                continue;
            }
            state.insert(follow.follower, false);
            while let Some((clip, index)) = stack.pop() {
                let targets = next.get(clip).map_or(&[][..], Vec::as_slice);
                if let Some(target) = targets.get(index) {
                    stack.push((clip, index + 1));
                    match state.get(target) {
                        None => {
                            state.insert(target, false);
                            stack.push((target, 0));
                        }
                        Some(false) => {
                            // Every clip on the walk from `target` to here is on a cycle.
                            on_cycle.insert(*target);
                            for (member, _) in stack.iter().rev() {
                                on_cycle.insert(*member);
                                if member == target {
                                    break;
                                }
                            }
                        }
                        Some(true) => {}
                    }
                } else {
                    state.insert(clip, true);
                }
            }
        }
        if let Some(follow) = self
            .follows
            .iter()
            .find(|follow| on_cycle.contains(follow.follower) && on_cycle.contains(follow.via))
        {
            return Err(ModelError::InvalidDataBinding {
                owner: DataOwner::Clip(follow.follower.clone()),
                at: follow.at.clone(),
                problem: BindingProblem::Cycle,
            });
        }
        Ok(())
    }
}
