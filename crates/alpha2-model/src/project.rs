use crate::{
    Composition, EffectResource, EffectResourceId, Generator, MasterAudio, MediaKind, MediaSource,
    ModelError, ProjectId, ProjectLoadError, Timeline, TrackKind,
};
use alpha2_time::{FrameRate, MediaTime};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use std::collections::{HashMap, HashSet};
use std::fmt;

/// The newest project-document schema understood by this build.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;
/// Largest project document accepted by [`Project::from_json`].
pub const MAX_PROJECT_JSON_BYTES: usize = 64 * 1024 * 1024;
/// Largest canvas edge accepted by the portable project model.
pub const MAX_CANVAS_DIMENSION: u32 = alpha2_media::MAX_FRAME_DIMENSION;
/// Largest total canvas area accepted before quality scaling.
pub const MAX_CANVAS_PIXELS: u64 = alpha2_media::MAX_FRAME_PIXELS;
/// Supported project audio sample-rate interval.
///
/// 96 kHz is the highest rate an export codec here takes (AAC-LC; Opus stops
/// at 48 kHz), and staying below the 120 kHz tick rate keeps every sample on a
/// distinct tick. A source may be sampled faster; hosts resample it.
pub const SAMPLE_RATE_RANGE: std::ops::RangeInclusive<u32> = 8_000..=96_000;
/// Maximum number of tracks in one project.
pub const MAX_TRACKS: usize = 256;
/// Maximum number of clips across one project, and of sources in it.
pub const MAX_CLIPS: usize = 100_000;
/// Maximum number of clips on one track.
pub const MAX_CLIPS_PER_TRACK: usize = 4_096;
/// Maximum number of transitions on one track.
pub const MAX_TRANSITIONS_PER_TRACK: usize = 4_096;

/// A complete serializable project document.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Project {
    /// Version used to serialize this document.
    pub schema_version: u32,
    /// Stable identity of this project.
    pub id: ProjectId,
    /// Human-readable project name. It has no semantic meaning.
    pub name: String,
    /// Output-wide timeline settings.
    pub settings: ProjectSettings,
    /// Media resources referenced by clips.
    pub sources: Vec<MediaSource>,
    /// Non-media resources referenced by effect invocations.
    #[serde(default)]
    pub effect_resources: Vec<EffectResource>,
    /// Composition graphs that clips show.
    #[serde(default)]
    pub compositions: Vec<Composition>,
    /// Ordered visual and audio tracks.
    pub timeline: Timeline,
    /// Master bus applied to the summed track audio.
    #[serde(default)]
    pub audio: MasterAudio,
}

/// The deserialized shape of a [`Project`], before its invariants are checked.
///
/// This is the wire contract, and the published JSON Schema is derived from
/// it: a document the schema accepts has the right shape, and
/// [`Project::validate`] then decides whether it means anything.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "Project"))]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectWire {
    /// Document version. This build reads and writes only
    /// [`CURRENT_SCHEMA_VERSION`].
    #[cfg_attr(feature = "schema", schemars(extend("const" = CURRENT_SCHEMA_VERSION)))]
    pub(crate) schema_version: u32,
    /// Stable identity of this project.
    pub(crate) id: ProjectId,
    /// Human-readable project name. It has no semantic meaning.
    pub(crate) name: String,
    /// Output-wide timeline settings.
    pub(crate) settings: ProjectSettings,
    /// Media resources referenced by clips.
    pub(crate) sources: Vec<MediaSource>,
    /// Non-media resources referenced by effect invocations.
    #[serde(default)]
    pub(crate) effect_resources: Vec<EffectResource>,
    /// Composition graphs that clips show.
    #[serde(default)]
    pub(crate) compositions: Vec<Composition>,
    /// Ordered visual and audio tracks.
    pub(crate) timeline: Timeline,
    /// Master bus applied to the summed track audio.
    #[serde(default)]
    pub(crate) audio: MasterAudio,
}

impl TryFrom<ProjectWire> for Project {
    type Error = ModelError;

    fn try_from(wire: ProjectWire) -> Result<Self, Self::Error> {
        let project = Self {
            schema_version: wire.schema_version,
            id: wire.id,
            name: wire.name,
            settings: wire.settings,
            sources: wire.sources,
            effect_resources: wire.effect_resources,
            compositions: wire.compositions,
            timeline: wire.timeline,
            audio: wire.audio,
        };
        project.validate()?;
        Ok(project)
    }
}

impl<'de> Deserialize<'de> for Project {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ProjectWire::deserialize(deserializer)?;
        Self::try_from(wire).map_err(D::Error::custom)
    }
}

impl Project {
    /// Parses and validates a serialized project document.
    ///
    /// The document's `schema_version` is read first: a document of another
    /// version reports [`ModelError::UnsupportedSchemaVersion`] rather than
    /// whatever shape difference the versions have.
    ///
    /// # Errors
    /// Returns [`ProjectLoadError`] for malformed, unsupported, or structurally
    /// invalid documents.
    pub fn from_json(json: &str) -> Result<Self, ProjectLoadError> {
        // Only the version, and every other field skipped unparsed. A probe that fails leaves the
        // error to the full parse below.
        #[derive(Deserialize)]
        struct VersionProbe {
            schema_version: u32,
        }

        if json.len() > MAX_PROJECT_JSON_BYTES {
            return Err(ProjectLoadError::TooLarge {
                bytes: json.len(),
                maximum: MAX_PROJECT_JSON_BYTES,
            });
        }
        if let Ok(VersionProbe { schema_version }) = serde_json::from_str(json)
            && schema_version != CURRENT_SCHEMA_VERSION
        {
            return Err(ProjectLoadError::InvalidProject(
                ModelError::UnsupportedSchemaVersion {
                    found: schema_version,
                },
            ));
        }
        let wire = serde_json::from_str::<ProjectWire>(json).map_err(ProjectLoadError::Json)?;
        Self::try_from(wire).map_err(ProjectLoadError::InvalidProject)
    }

    /// The sources a render of this project can read, in source order: each
    /// clip's own, and those the media nodes its compositions draw read.
    ///
    /// A clip's compositions are the one it names and, for a composition
    /// source, the source's when the one it names draws the clip
    /// ([`crate::CompositionReads::draws_clip`]). From a composition's output
    /// the walk follows the inputs drawing a node reads
    /// ([`crate::CompositionReads::needed_inputs`], as the renderer plans
    /// them), into the composition a group node draws and the one a
    /// composition source a media node reads shows. So an unconnected node, a
    /// composition nothing shows, the inputs a bypassed node passes over, and
    /// the group ports a group's composition never reads read nothing. An
    /// unconnected input the output needs fails those frames; what the
    /// connected inputs read is still used.
    #[must_use]
    pub fn used_sources(&self) -> Vec<&MediaSource> {
        let sources = self
            .sources
            .iter()
            .map(|source| (&source.id, source))
            .collect::<HashMap<_, _>>();
        let compositions = self
            .compositions
            .iter()
            .map(|composition| (&composition.id, composition))
            .collect::<HashMap<_, _>>();
        let reads = crate::CompositionReads::new(&compositions);
        let mut used = HashSet::new();
        let mut walked = HashSet::new();
        let mut pending = Vec::new();
        for clip in self.timeline.tracks.iter().flat_map(|track| &track.clips) {
            used.insert(&clip.source_id);
            // A composition source is the clip's picture, drawn only where the clip's own
            // composition draws the clip.
            let drawn = clip
                .composition
                .as_ref()
                .is_none_or(|reference| reads.draws_clip(&reference.id));
            pending.extend(
                sources
                    .get(&clip.source_id)
                    .and_then(|source| source.composition.as_ref())
                    .filter(|_| drawn),
            );
            pending.extend(clip.composition.as_ref().map(|reference| &reference.id));
        }
        // Validation rules out cycles; `walked` also keeps a composition many
        // clips or groups draw from being walked again.
        while let Some(id) = pending.pop() {
            if !walked.insert(id) {
                continue;
            }
            let Some(composition) = compositions.get(id) else {
                continue;
            };
            let nodes = composition
                .nodes
                .iter()
                .map(|node| (&node.id, node))
                .collect::<HashMap<_, _>>();
            let mut reached = HashSet::new();
            let mut frontier = composition.output.iter().collect::<Vec<_>>();
            while let Some(node) = frontier.pop() {
                let Some(node) = nodes.get(node).filter(|_| reached.insert(node)) else {
                    continue;
                };
                match &node.op {
                    crate::NodeOp::Media { source_id, .. } => {
                        used.insert(source_id);
                        pending.extend(
                            sources
                                .get(source_id)
                                .and_then(|source| source.composition.as_ref()),
                        );
                    }
                    crate::NodeOp::Group { composition, .. } => pending.push(composition),
                    _ => {}
                }
                frontier.extend(
                    reads
                        .needed_inputs(node)
                        .into_iter()
                        .filter_map(crate::NodeInput::node),
                );
            }
        }
        self.sources
            .iter()
            .filter(|source| used.contains(&source.id))
            .collect()
    }

    /// Returns the HDR sources, which the project tone-maps or refuses as its
    /// [`ColorPipeline`] says: those whose color is PQ or HLG, or declared
    /// HDR, as the renderer decides ([`alpha2_media::ColorMetadata::is_hdr`]).
    #[must_use]
    pub fn hdr_sources(&self) -> Vec<&alpha2_media::SourceId> {
        self.sources
            .iter()
            .filter(|source| source.effective_color_metadata().is_hdr())
            .map(|source| &source.id)
            .collect()
    }

    /// Whether anything in the project may draw text: a caption track, or a
    /// text or layout generator in a source or a composition. A host whose
    /// project draws none needs no fonts and prepares no text. A layout is
    /// resolved by the text host, with or without text.
    #[must_use]
    pub fn draws_text(&self) -> bool {
        let is_text = |generator: &crate::Generator| {
            matches!(
                generator,
                crate::Generator::Text { .. } | crate::Generator::Layout { .. }
            )
        };
        self.timeline
            .tracks
            .iter()
            .any(|track| track.captions.is_some())
            || self
                .sources
                .iter()
                .any(|source| source.generator.as_ref().is_some_and(is_text))
            || self.compositions.iter().any(|composition| {
                composition.nodes.iter().any(|node| {
                    matches!(&node.op, crate::NodeOp::Generator { generator } if is_text(generator))
                })
            })
    }

    /// Creates an empty project using the current schema version.
    #[must_use]
    pub fn new(id: ProjectId, name: impl Into<String>, settings: ProjectSettings) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            id,
            name: name.into(),
            settings,
            sources: Vec::new(),
            effect_resources: Vec::new(),
            compositions: Vec::new(),
            timeline: Timeline { tracks: Vec::new() },
            audio: MasterAudio::default(),
        }
    }

    /// Checks structural invariants without performing media I/O.
    ///
    /// # Errors
    ///
    /// Returns the first invalid document invariant encountered.
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(ModelError::UnsupportedSchemaVersion {
                found: self.schema_version,
            });
        }
        self.settings.validate()?;
        ensure_id(&self.id)?;
        if !self.audio.validate() {
            return Err(ModelError::InvalidMasterAudio);
        }
        if self.sources.len() > MAX_CLIPS {
            return Err(ModelError::TooManySources {
                found: self.sources.len(),
                maximum: MAX_CLIPS,
            });
        }
        if self.timeline.tracks.len() > MAX_TRACKS {
            return Err(ModelError::TooManyTracks {
                found: self.timeline.tracks.len(),
                maximum: MAX_TRACKS,
            });
        }

        let sources = self.validate_sources()?;
        let effect_resource_ids = self.validate_effect_resources()?;
        let inlined = crate::composition::validate_compositions(
            &self.compositions,
            &self.sources,
            &sources,
            &effect_resource_ids,
            (self.settings.width, self.settings.height),
        )?;
        let index = DocumentIndex {
            sources,
            compositions: self
                .compositions
                .iter()
                .map(|composition| &composition.id)
                .collect(),
        };

        let mut track_ids = HashSet::new();
        let mut clip_ids = HashSet::new();
        let mut clip_count = 0_usize;
        for track in &self.timeline.tracks {
            ensure_id(&track.id)?;
            if !track_ids.insert(&track.id) {
                return Err(ModelError::DuplicateTrackId(track.id.clone()));
            }
            if track.clips.len() > MAX_CLIPS_PER_TRACK {
                return Err(ModelError::TooManyClipsOnTrack {
                    track: track.id.clone(),
                    found: track.clips.len(),
                    maximum: MAX_CLIPS_PER_TRACK,
                });
            }
            validate_captions(track)?;
            if track.transitions.len() > MAX_TRANSITIONS_PER_TRACK {
                return Err(ModelError::TooManyTransitions {
                    track: track.id.clone(),
                    found: track.transitions.len(),
                    maximum: MAX_TRANSITIONS_PER_TRACK,
                });
            }
            clip_count =
                clip_count
                    .checked_add(track.clips.len())
                    .ok_or(ModelError::TooManyClips {
                        found: usize::MAX,
                        maximum: MAX_CLIPS,
                    })?;
            if clip_count > MAX_CLIPS {
                return Err(ModelError::TooManyClips {
                    found: clip_count,
                    maximum: MAX_CLIPS,
                });
            }
            for clip in &track.clips {
                validate_clip(&index, track, clip, &mut clip_ids)?;
            }
            for transition in &track.transitions {
                ensure_effect_resources_exist(
                    &transition.effect,
                    &effect_resource_ids,
                    &transition.to_clip,
                )?;
            }
            ensure_transitions_are_supported(track)?;
            if !track.audio.validate() {
                return Err(ModelError::InvalidTrackAudio {
                    track: track.id.clone(),
                });
            }
            ensure_overlaps_have_transitions(track)?;
        }
        self.ensure_audio_effects_fit()?;
        ensure_ducking_is_acyclic(&self.timeline)?;
        ensure_mattes_are_usable(&self.timeline)?;
        ensure_frames_fit(&self.timeline, &index.sources, &inlined)?;
        Ok(())
    }

    /// Checks each source, and returns them by ID.
    fn validate_sources(
        &self,
    ) -> Result<HashMap<&alpha2_media::SourceId, &MediaSource>, ModelError> {
        let mut sources = HashMap::with_capacity(self.sources.len());
        for source in &self.sources {
            ensure_id(&source.id)?;
            validate_source_content(source, (self.settings.width, self.settings.height))?;
            if sources.insert(&source.id, source).is_some() {
                return Err(ModelError::DuplicateSourceId(source.id.clone()));
            }
        }
        Ok(sources)
    }

    /// Bounds the audio effect state the project holds at once (see
    /// [`Project::audio_effect_load`]): [`crate::MAX_AUDIO_EFFECTS`] effects
    /// and [`crate::MAX_AUDIO_DELAY_MS`] of delay.
    fn ensure_audio_effects_fit(&self) -> Result<(), ModelError> {
        let load = self.audio_effect_load();
        if load.effects > crate::MAX_AUDIO_EFFECTS {
            return Err(ModelError::TooManyAudioEffects {
                found: load.effects,
                maximum: crate::MAX_AUDIO_EFFECTS,
                at_ticks: load.effects_at_ticks,
            });
        }
        if load.delay_ms() > u64::from(crate::MAX_AUDIO_DELAY_MS) {
            return Err(ModelError::TooMuchAudioDelay {
                found_ms: load.delay_ms(),
                maximum_ms: crate::MAX_AUDIO_DELAY_MS,
                at_ticks: load.delay_at_ticks,
            });
        }
        Ok(())
    }

    fn validate_effect_resources(&self) -> Result<HashSet<&EffectResourceId>, ModelError> {
        let mut ids = HashSet::new();
        for resource in &self.effect_resources {
            ensure_id(&resource.id)?;
            if resource.locator.is_empty() {
                return Err(ModelError::EmptyEffectResourceLocator {
                    resource: resource.id.clone(),
                });
            }
            if !ids.insert(&resource.id) {
                return Err(ModelError::DuplicateEffectResourceId(resource.id.clone()));
            }
        }
        Ok(ids)
    }
}

fn ensure_effect_resources_exist(
    effect: &crate::ClipEffect,
    known: &HashSet<&EffectResourceId>,
    clip: &crate::ClipId,
) -> Result<(), ModelError> {
    for resource in effect.resources.values() {
        if !known.contains(resource) {
            return Err(ModelError::UnknownEffectResource {
                clip: clip.clone(),
                resource: resource.clone(),
            });
        }
    }
    Ok(())
}

/// Checks one clip's identity, ranges, source compatibility, and authored
/// visual and audio parameters.
fn validate_clip<'a>(
    index: &DocumentIndex<'a>,
    track: &'a crate::Track,
    clip: &'a crate::Clip,
    clip_ids: &mut HashSet<&'a crate::ClipId>,
) -> Result<(), ModelError> {
    ensure_id(&clip.id)?;
    if clip.timeline_range.is_empty() || clip.source_range.is_empty() {
        return Err(ModelError::EmptyClipRange {
            clip: clip.id.clone(),
        });
    }
    if clip.timeline_range.start().ticks() < 0 || clip.source_range.start().ticks() < 0 {
        return Err(ModelError::NegativeClipRange {
            clip: clip.id.clone(),
        });
    }
    if !clip_ids.insert(&clip.id) {
        return Err(ModelError::DuplicateClipId(clip.id.clone()));
    }
    let Some(source) = index.sources.get(&clip.source_id) else {
        return Err(ModelError::UnknownClipSource {
            clip: clip.id.clone(),
            source: clip.source_id.clone(),
        });
    };
    if !track.kind.accepts(source.kind) {
        return Err(ModelError::IncompatibleClipSource {
            clip: clip.id.clone(),
            track: track.id.clone(),
            source: source.id.clone(),
        });
    }
    if !clip.visual.validate() {
        return Err(ModelError::InvalidVisualProperties {
            clip: clip.id.clone(),
        });
    }
    if let Some(composition) = &clip.composition {
        // The reference's own terms first: what follows computes with them. The composition time
        // grows with the timeline time, so the clip's last instant has the largest; every time a
        // document holds fits in a serialized tick.
        let held = || {
            let last = MediaTime::from_ticks(clip.timeline_range.end().ticks().saturating_sub(1));
            composition
                .local_time(clip.timeline_range.start(), last)
                .is_some_and(|time| time.ticks() <= alpha2_time::MAX_SERIALIZED_TICKS)
        };
        if track.kind != TrackKind::Video || !composition.scale.is_valid() || !held() {
            return Err(ModelError::InvalidClipComposition {
                clip: clip.id.clone(),
            });
        }
        if !index.compositions.contains(&composition.id) {
            return Err(ModelError::UnknownComposition {
                clip: clip.id.clone(),
                composition: composition.id.clone(),
            });
        }
    }
    if !clip.audio.validate() {
        return Err(ModelError::InvalidAudioProperties {
            clip: clip.id.clone(),
        });
    }
    let visual = track.kind == TrackKind::Video;
    if clip.masks.len() > crate::MAX_CLIP_MASKS || (!visual && !clip.masks.is_empty()) {
        return Err(ModelError::InvalidClipMask {
            clip: clip.id.clone(),
        });
    }
    crate::mask::validate_masks(&clip.masks).map_err(|issue| ModelError::InvalidShape {
        owner: crate::ShapeOwner::Clip(clip.id.clone()),
        at: issue.at,
        problem: issue.problem,
    })?;
    if !visual && clip.matte.is_some() {
        return Err(ModelError::InvalidMatte {
            clip: clip.id.clone(),
        });
    }
    if let crate::TimeMap::Freeze { source_time } = clip.time_map
        && !clip.source_range.contains(source_time)
    {
        return Err(ModelError::InvalidTimeMap {
            clip: clip.id.clone(),
        });
    }
    Ok(())
}

/// The document's sources and compositions by ID, so that validation looks
/// each reference up once rather than searching the lists.
struct DocumentIndex<'a> {
    sources: HashMap<&'a alpha2_media::SourceId, &'a MediaSource>,
    compositions: HashSet<&'a crate::CompositionId>,
}

/// Rejects transitions on audio tracks, any transition not fully covered by
/// both of its clips, and a second transition between two clips.
fn ensure_transitions_are_supported(track: &crate::Track) -> Result<(), ModelError> {
    let clips = track
        .clips
        .iter()
        .map(|clip| (&clip.id, clip))
        .collect::<HashMap<_, _>>();
    let mut pairs = HashSet::with_capacity(track.transitions.len());
    for transition in &track.transitions {
        let covers = |id: &crate::ClipId| {
            clips.get(id).is_some_and(|clip| {
                clip.timeline_range.start() <= transition.timeline_range.start()
                    && clip.timeline_range.end() >= transition.timeline_range.end()
            })
        };
        let valid = track.kind == TrackKind::Video
            && transition.from_clip != transition.to_clip
            && !transition.timeline_range.is_empty()
            && transition.easing.is_valid()
            && transition.effect.is_valid()
            && covers(&transition.from_clip)
            && covers(&transition.to_clip);
        if !valid {
            return Err(ModelError::InvalidClipTransition {
                track: track.id.clone(),
            });
        }
        // The engine folds one transition over another: the same pair twice mixes twice, and both
        // ways hides both clips.
        let pair = if transition.from_clip < transition.to_clip {
            (&transition.from_clip, &transition.to_clip)
        } else {
            (&transition.to_clip, &transition.from_clip)
        };
        if !pairs.insert(pair) {
            return Err(ModelError::DuplicateTransition {
                track: track.id.clone(),
                first: transition.from_clip.clone(),
                second: transition.to_clip.clone(),
            });
        }
    }
    Ok(())
}

/// Rejects sidechain references to missing tracks, to the referencing track
/// itself, and to any cycle between ducked tracks.
///
/// Every track carries an audio bus, so either kind may duck or be ducked.
/// The track reported is the first, in document order, whose sidechain is
/// unusable. Each track ID is looked up once, so the check takes time in
/// proportion to the tracks and the length of their IDs, however long the
/// chains between them.
fn ensure_ducking_is_acyclic(timeline: &Timeline) -> Result<(), ModelError> {
    fn sidechain_of(track: &crate::Track) -> Option<&crate::TrackId> {
        track
            .audio
            .ducking
            .as_ref()
            .map(|ducking| &ducking.sidechain_track)
    }
    // Track IDs are unique by now, so each names one position.
    let positions = timeline
        .tracks
        .iter()
        .enumerate()
        .map(|(position, track)| (&track.id, position))
        .collect::<HashMap<_, _>>();
    let next = timeline
        .tracks
        .iter()
        .map(|track| sidechain_of(track).and_then(|sidechain| positions.get(sidechain).copied()))
        .collect::<Vec<_>>();
    let cyclic = tracks_on_cycles(&next);
    for ((track, next), on_cycle) in timeline.tracks.iter().zip(next).zip(cyclic) {
        let Some(sidechain) = sidechain_of(track) else {
            continue;
        };
        // A missing sidechain, or one that leads back to this track, itself included.
        if next.is_none() || on_cycle {
            return Err(ModelError::InvalidDuckingSidechain {
                track: track.id.clone(),
                sidechain: sidechain.clone(),
            });
        }
    }
    Ok(())
}

/// Which tracks lie on a cycle of sidechains, given for each track the
/// position of the track its sidechain names, if any.
///
/// Each track names at most one sidechain, so the walk from any track ends,
/// or enters the one cycle its path reaches; a track is on a cycle exactly
/// when its own walk returns to it. Each walk stops at the first track an
/// earlier walk reached, so every track is visited once over all of them.
fn tracks_on_cycles(next: &[Option<usize>]) -> Vec<bool> {
    const UNVISITED: usize = usize::MAX;
    let mut cyclic = vec![false; next.len()];
    // The walk that first reached each track: a walk that meets its own mark has closed a cycle.
    let mut walk_of = vec![UNVISITED; next.len()];
    for start in 0..next.len() {
        let mut cursor = Some(start);
        while let Some(position) = cursor {
            match walk_of[position] {
                UNVISITED => {
                    walk_of[position] = start;
                    cursor = next[position];
                }
                walk if walk == start => {
                    // Mark the cycle by going round it once from where the walk closed it.
                    let mut member = position;
                    loop {
                        cyclic[member] = true;
                        match next[member] {
                            Some(following) if following != position => member = following,
                            _ => break,
                        }
                    }
                    break;
                }
                _ => break,
            }
        }
    }
    cyclic
}

/// Rejects a matte that names no visual clip, the clip itself, a clip with a
/// matte of its own, or a clip in a transition.
///
/// A matte clip is drawn only into the mattes that name it. A chain of mattes
/// would need an order of evaluation the document does not give, and a clip
/// in a transition is drawn by the transition.
fn ensure_mattes_are_usable(timeline: &Timeline) -> Result<(), ModelError> {
    let visual = timeline
        .tracks
        .iter()
        .filter(|track| track.kind == TrackKind::Video)
        .flat_map(|track| track.clips.iter().map(|clip| (&clip.id, clip)))
        .collect::<HashMap<_, _>>();
    let in_transition = timeline
        .tracks
        .iter()
        .flat_map(|track| &track.transitions)
        .flat_map(|transition| [&transition.from_clip, &transition.to_clip])
        .collect::<HashSet<_>>();
    // In document order, so the clip reported is the first, on every platform.
    let clips = timeline
        .tracks
        .iter()
        .filter(|track| track.kind == TrackKind::Video)
        .flat_map(|track| &track.clips);
    for clip in clips {
        let Some(matte) = &clip.matte else {
            continue;
        };
        let usable = matte.source != clip.id
            && visual
                .get(&matte.source)
                .is_some_and(|source| source.matte.is_none())
            && !in_transition.contains(&matte.source);
        if !usable {
            return Err(ModelError::InvalidMatte {
                clip: clip.id.clone(),
            });
        }
    }
    Ok(())
}

/// Bounds what one frame draws: at no instant do the clips on the timeline
/// draw more than [`crate::MAX_FRAME_INLINED_NODES`] composition nodes
/// together, each clip counting the nodes its composition and its
/// composition source draw once inlined. A composition is compiled once
/// however many clips show it, but each clip on screen draws all of it.
///
/// The clip reported is the first, in document order, of those starting at
/// the first instant over the bound, and the count is all the clips there.
fn ensure_frames_fit(
    timeline: &Timeline,
    sources: &HashMap<&alpha2_media::SourceId, &MediaSource>,
    inlined: &crate::composition::InlinedNodes<'_>,
) -> Result<(), ModelError> {
    let nodes = |id: Option<&crate::CompositionId>| id.and_then(|id| inlined.get(id)).copied();
    // Each clip drawing a composition, with the nodes it draws, at its start (+) and its end (-).
    let mut edges = Vec::new();
    for clip in timeline.tracks.iter().flat_map(|track| &track.clips) {
        let source = sources
            .get(&clip.source_id)
            .and_then(|source| source.composition.as_ref());
        let drawn = nodes(clip.composition.as_ref().map(|reference| &reference.id))
            .unwrap_or(0)
            .saturating_add(nodes(source).unwrap_or(0));
        if drawn > 0 {
            edges.push((clip.timeline_range.start(), true, drawn, &clip.id));
            edges.push((clip.timeline_range.end(), false, drawn, &clip.id));
        }
    }
    // By instant, the clips ending there before those starting there (ranges are half-open), each
    // in document order: the sort is stable.
    edges.sort_by_key(|&(time, starts, ..)| (time, starts));
    let mut drawn = 0_u64;
    let mut edges = edges.into_iter().peekable();
    while let Some((time, starts, nodes, clip)) = edges.next() {
        if !starts {
            drawn -= nodes;
            continue;
        }
        drawn = drawn.saturating_add(nodes);
        // Every clip starting at this instant, so the count is the frame's.
        while let Some((_, _, nodes, _)) =
            edges.next_if(|&(next, next_starts, ..)| next == time && next_starts)
        {
            drawn = drawn.saturating_add(nodes);
        }
        if drawn > crate::MAX_FRAME_INLINED_NODES as u64 {
            return Err(ModelError::FrameDrawsTooManyNodes {
                clip: clip.clone(),
                found: drawn,
                maximum: crate::MAX_FRAME_INLINED_NODES,
            });
        }
    }
    Ok(())
}

/// Rejects visual overlaps that no transition covers.
///
/// Two visual clips occupying one timestamp have no defined composition unless
/// a transition says how to combine them. Audio has a defined answer (the
/// mixer sums overlapping clips into their track bus), so audio tracks may
/// overlay freely and shape the result with fades.
pub(crate) fn ensure_overlaps_have_transitions(track: &crate::Track) -> Result<(), ModelError> {
    if track.kind != TrackKind::Video {
        return Ok(());
    }
    let transitions = track
        .transitions
        .iter()
        .flat_map(|transition| {
            [
                (
                    &transition.from_clip,
                    &transition.to_clip,
                    transition.timeline_range,
                ),
                (
                    &transition.to_clip,
                    &transition.from_clip,
                    transition.timeline_range,
                ),
            ]
        })
        .collect::<HashSet<_>>();
    let directed = track
        .transitions
        .iter()
        .map(|transition| (&transition.from_clip, &transition.to_clip))
        .collect::<HashSet<_>>();
    let mut ordered = track.clips.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|clip| (clip.timeline_range.start(), clip.timeline_range.end()));
    let mut active = Vec::new();
    for second in ordered {
        active.retain(|first: &&crate::Clip| {
            first.timeline_range.end() > second.timeline_range.start()
        });
        for first in &active {
            let Some(intersection) = first.timeline_range.intersection(second.timeline_range)
            else {
                continue;
            };
            if !transitions.contains(&(&first.id, &second.id, intersection)) {
                return Err(ModelError::OverlappingClips {
                    track: track.id.clone(),
                    first: first.id.clone(),
                    second: second.id.clone(),
                });
            }
        }
        active.push(second);
        // Every clip active here covers this instant, and so does the transition between each
        // two: the engine folds them from clip to clip, which needs no cycle. A tournament has none
        // exactly when its clips lead to 0, 1, ..., k - 1 others. A set only grows at a start, so
        // the starts are every instant to check.
        if active.len() >= 3 {
            let mut leads = active
                .iter()
                .map(|from| {
                    active
                        .iter()
                        .filter(|to| directed.contains(&(&from.id, &to.id)))
                        .count()
                })
                .collect::<Vec<_>>();
            leads.sort_unstable();
            if leads.iter().enumerate().any(|(rank, lead)| rank != *lead) {
                return Err(ModelError::TransitionCycle {
                    track: track.id.clone(),
                    at: second.timeline_range.start(),
                });
            }
        }
    }
    Ok(())
}

/// A caption track holds valid captions and nothing else; no other track
/// holds captions.
fn validate_captions(track: &crate::Track) -> Result<(), ModelError> {
    if let Some(captions) = &track.captions
        && captions.cues.len() > MAX_CLIPS_PER_TRACK
    {
        return Err(ModelError::TooManyCaptionCues {
            track: track.id.clone(),
            found: captions.cues.len(),
            maximum: MAX_CLIPS_PER_TRACK,
        });
    }
    let valid = match (&track.kind, &track.captions) {
        (TrackKind::Caption, Some(captions)) => {
            captions.is_valid() && track.clips.is_empty() && track.transitions.is_empty()
        }
        (TrackKind::Caption, None) => false,
        (_, captions) => captions.is_none(),
    };
    if valid {
        Ok(())
    } else {
        Err(ModelError::InvalidCaptions {
            track: track.id.clone(),
        })
    }
}

/// A generated source has a valid generator and nothing a host would supply;
/// any other source has a locator and no generator, and an image no audio.
/// A declared color is consistent: HDR exactly when its transfer is PQ or HLG.
/// Checks one source; `canvas` is the project's, which a layout's pixels are
/// pixels of.
fn validate_source_content(source: &MediaSource, canvas: (u32, u32)) -> Result<(), ModelError> {
    if let Some(fingerprint) = &source.fingerprint
        && (source.kind == MediaKind::Generated || !fingerprint.is_valid())
    {
        return Err(ModelError::InvalidFingerprint {
            source: source.id.clone(),
        });
    }
    if source
        .color_metadata
        .is_some_and(|color| !color.is_consistent())
    {
        return Err(ModelError::ContradictoryColor {
            source: source.id.clone(),
        });
    }
    if let Some(provider) = &source.provider
        && (provider.kind() != source.kind || source.has_audio || !provider.is_valid())
    {
        return Err(ModelError::InvalidProvider {
            source: source.id.clone(),
        });
    }
    if source.kind == MediaKind::Composition {
        // Whether the composition exists is checked with the compositions.
        let valid = source.composition.is_some()
            && source.generator.is_none()
            && source.locator.is_empty()
            && source.color_metadata.is_none()
            && source.fingerprint.is_none()
            && source.provider.is_none()
            && !source.has_audio;
        return if valid {
            Ok(())
        } else {
            Err(ModelError::InvalidCompositionSource {
                source: source.id.clone(),
            })
        };
    }
    if source.composition.is_some() {
        return Err(ModelError::InvalidCompositionSource {
            source: source.id.clone(),
        });
    }
    if source.kind == MediaKind::Generated {
        let owner = || crate::ShapeOwner::Source(source.id.clone());
        match source
            .generator
            .as_ref()
            .map(|generator| generator.validate_on(canvas.0, canvas.1))
        {
            Some(Err(crate::GeneratorIssue::Shape(issue))) => {
                return Err(ModelError::InvalidShape {
                    owner: owner(),
                    at: issue.at,
                    problem: issue.problem,
                });
            }
            Some(Err(crate::GeneratorIssue::Layout(issue))) => {
                return Err(ModelError::InvalidLayout {
                    owner: owner(),
                    at: issue.at,
                    problem: issue.problem,
                });
            }
            _ => {}
        }
        let valid = source.generator.as_ref().is_some_and(Generator::is_valid)
            && source.locator.is_empty()
            && source.color_metadata.is_none()
            && !source.has_audio;
        return if valid {
            Ok(())
        } else {
            Err(ModelError::InvalidGenerator {
                source: source.id.clone(),
            })
        };
    }
    if source.generator.is_some() {
        return Err(ModelError::InvalidGenerator {
            source: source.id.clone(),
        });
    }
    if source.locator.is_empty() {
        return Err(ModelError::EmptyLocator {
            source: source.id.clone(),
        });
    }
    if source.kind == MediaKind::Image && source.has_audio {
        return Err(ModelError::AudioOnImage {
            source: source.id.clone(),
        });
    }
    Ok(())
}

fn ensure_id<T: fmt::Display>(id: &T) -> Result<(), ModelError> {
    if id.to_string().is_empty() {
        Err(ModelError::EmptyIdentifier)
    } else {
        Ok(())
    }
}

/// Output-wide settings independent of a renderer or encoder.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ProjectSettings {
    /// Canvas width in output pixels, at most [`MAX_CANVAS_DIMENSION`].
    pub width: u32,
    /// Canvas height in output pixels, at most [`MAX_CANVAS_DIMENSION`]. The
    /// canvas area is at most [`MAX_CANVAS_PIXELS`].
    pub height: u32,
    /// Timeline frame rate.
    pub frame_rate: FrameRate,
    /// Internal audio sample rate, within [`SAMPLE_RATE_RANGE`].
    pub sample_rate: u32,
    /// Opaque color under every layer, in preview and export alike.
    ///
    /// Empty canvas, and whatever shows through a translucent layer, is this
    /// color rather than something a host or encoder decides.
    #[serde(default)]
    pub background: Rgb,
    /// The space in which clips' blend modes are evaluated, in preview and
    /// export alike.
    #[serde(default)]
    pub blend_space: BlendSpace,
    /// How sources' colors come into the working space.
    #[serde(default)]
    pub color: ColorPipeline,
}

/// How sources' colors come into the working space.
///
/// The working space is display-referred linear light with BT.709 primaries,
/// where `1.0` is SDR white (100 nits), and every export is SDR BT.709. SDR
/// sources decode through the sRGB EOTF; wide-gamut ones are converted into
/// BT.709 primaries. An HDR (PQ or HLG) source is either refused or brought
/// in: linearized to display light, converted into BT.709 primaries, and
/// tone-mapped from [`Self::hdr_peak_nits`] to SDR white by the ITU-R BT.2390
/// EETF on each pixel's largest component, which keeps its hue.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ColorPipeline {
    /// What an HDR source does.
    #[serde(default)]
    pub hdr: HdrSources,
    /// The luminance, in nits, HDR sources are taken to peak at: PQ above it
    /// is clipped to it, HLG is displayed at it, and the tone map brings it
    /// to SDR white. Within [`HDR_PEAK_NITS_RANGE`].
    #[serde(default = "default_hdr_peak")]
    pub hdr_peak_nits: u32,
}

/// Supported [`ColorPipeline::hdr_peak_nits`]: SDR white to PQ's ceiling.
pub const HDR_PEAK_NITS_RANGE: std::ops::RangeInclusive<u32> = 100..=10_000;

const fn default_hdr_peak() -> u32 {
    1_000
}

impl Default for ColorPipeline {
    fn default() -> Self {
        Self {
            hdr: HdrSources::default(),
            hdr_peak_nits: default_hdr_peak(),
        }
    }
}

/// What a project does with HDR sources.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum HdrSources {
    /// Tone-map them into the SDR working space.
    #[default]
    ToneMap,
    /// Refuse to render them.
    Reject,
}

/// The color space in which blend modes are evaluated.
///
/// Composition is always premultiplied; this chooses what the blend formulas
/// see. Design tools blend sRGB-encoded values, so overlay, soft-light, and
/// the other contrast modes look as they do there only in
/// [`BlendSpace::SrgbEncoded`].
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum BlendSpace {
    /// Linear light, where blending is physically meaningful.
    #[default]
    LinearLight,
    /// sRGB-encoded values, as design tools and CSS blend.
    SrgbEncoded,
}

/// An opaque color as a color picker shows it: straight, sRGB-encoded
/// components in the inclusive unit interval.
#[derive(Copy, Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Rgb {
    /// Red component.
    pub red: f32,
    /// Green component.
    pub green: f32,
    /// Blue component.
    pub blue: f32,
}

impl Rgb {
    /// Opaque black, the default background.
    pub const BLACK: Self = Self::new(0.0, 0.0, 0.0);

    /// Creates a color from sRGB-encoded components.
    #[must_use]
    pub const fn new(red: f32, green: f32, blue: f32) -> Self {
        Self { red, green, blue }
    }

    fn is_valid(self) -> bool {
        [self.red, self.green, self.blue]
            .into_iter()
            .all(|component| component.is_finite() && (0.0..=1.0).contains(&component))
    }
}

impl ProjectSettings {
    /// Creates validated output settings.
    ///
    /// # Errors
    /// Returns a model error when dimensions or sample rate exceed supported bounds.
    pub fn new(
        width: u32,
        height: u32,
        frame_rate: FrameRate,
        sample_rate: u32,
    ) -> Result<Self, ModelError> {
        let settings = Self {
            width,
            height,
            frame_rate,
            sample_rate,
            background: Rgb::BLACK,
            blend_space: BlendSpace::LinearLight,
            color: ColorPipeline::default(),
        };
        settings.validate()?;
        Ok(settings)
    }

    /// Replaces the background color.
    ///
    /// # Errors
    /// Returns [`ModelError::InvalidBackground`] for a non-finite or
    /// out-of-range component.
    pub fn with_background(mut self, background: Rgb) -> Result<Self, ModelError> {
        self.background = background;
        self.validate()?;
        Ok(self)
    }

    /// Checks that output settings are usable.
    fn validate(&self) -> Result<(), ModelError> {
        if !HDR_PEAK_NITS_RANGE.contains(&self.color.hdr_peak_nits) {
            return Err(ModelError::InvalidColorPipeline);
        }
        if self.width == 0 || self.height == 0 {
            return Err(ModelError::ZeroCanvasDimension);
        }
        if self.width > MAX_CANVAS_DIMENSION || self.height > MAX_CANVAS_DIMENSION {
            return Err(ModelError::CanvasDimensionTooLarge {
                width: self.width,
                height: self.height,
                maximum: MAX_CANVAS_DIMENSION,
            });
        }
        let pixels = u64::from(self.width) * u64::from(self.height);
        if pixels > MAX_CANVAS_PIXELS {
            return Err(ModelError::CanvasPixelBudgetExceeded {
                pixels,
                maximum: MAX_CANVAS_PIXELS,
            });
        }
        if !self.background.is_valid() {
            return Err(ModelError::InvalidBackground);
        }
        if !SAMPLE_RATE_RANGE.contains(&self.sample_rate) {
            return Err(ModelError::UnsupportedSampleRate {
                found: self.sample_rate,
                minimum: *SAMPLE_RATE_RANGE.start(),
                maximum: *SAMPLE_RATE_RANGE.end(),
            });
        }
        Ok(())
    }
}
