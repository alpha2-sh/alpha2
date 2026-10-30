use crate::{ClipId, CompositionId, EffectResourceId, NodeId, TrackId};
use alpha2_media::SourceId;
use std::fmt;

/// What holds a shape or a layout a [`ModelError::InvalidShape`] or
/// [`ModelError::InvalidLayout`] names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShapeOwner {
    /// A generated source's shapes or layout generator.
    Source(SourceId),
    /// A clip's masks.
    Clip(ClipId),
    /// A composition node's shapes or layout generator, or its masks.
    Node {
        /// The composition.
        composition: CompositionId,
        /// The node.
        node: NodeId,
    },
}

impl fmt::Display for ShapeOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(source) => write!(formatter, "source {source}"),
            Self::Clip(clip) => write!(formatter, "clip {clip}"),
            Self::Node { composition, node } => {
                write!(formatter, "composition {composition} node {node}")
            }
        }
    }
}

/// A document invariant violation.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ModelError {
    /// This build cannot interpret the document version.
    UnsupportedSchemaVersion {
        /// Version found in the decoded document.
        found: u32,
    },
    /// At least one ID was empty.
    EmptyIdentifier,
    /// Two sources share one ID.
    DuplicateSourceId(SourceId),
    /// Two effect resources share one ID.
    DuplicateEffectResourceId(EffectResourceId),
    /// Two tracks share one ID.
    DuplicateTrackId(TrackId),
    /// Two clips share one ID.
    DuplicateClipId(ClipId),
    /// A source locator was empty.
    EmptyLocator {
        /// Source whose locator was empty.
        source: SourceId,
    },
    /// A still image source says it has audio, which no image holds.
    AudioOnImage {
        /// The image source.
        source: SourceId,
    },
    /// A source's provider does not fit it: another kind of source than the
    /// provider supplies, audio, or incoherent settings; or a generated
    /// source has one.
    InvalidProvider {
        /// The source.
        source: SourceId,
    },
    /// A source's fingerprint is malformed, or a generated source has one.
    InvalidFingerprint {
        /// The source.
        source: SourceId,
    },
    /// A source declares a color whose dynamic range its transfer
    /// contradicts: HDR is exactly a PQ or HLG transfer.
    ContradictoryColor {
        /// The source.
        source: SourceId,
    },
    /// A generated source has no valid generator, or has a locator, color
    /// metadata, or audio; or another source has a generator.
    InvalidGenerator {
        /// The source.
        source: SourceId,
    },
    /// A shapes generator's content tree, or a mask's outline, breaks a
    /// limit, has a value out of range, or has keyframes that cannot
    /// interpolate.
    InvalidShape {
        /// What holds the shape.
        owner: ShapeOwner,
        /// Where in it the problem lies, like `items[2].items[0].path` or
        /// `masks[1].shape.size`.
        at: String,
        /// What is wrong there.
        problem: crate::ShapeProblem,
    },
    /// A layout generator's tree breaks a limit or its structure rules, has
    /// a value out of range, or has keyframes that cannot interpolate.
    InvalidLayout {
        /// What holds the layout.
        owner: ShapeOwner,
        /// Where in it the problem lies, like `root.children[2].padding`.
        at: String,
        /// What is wrong there.
        problem: crate::LayoutProblem,
    },
    /// An effect resource locator was empty.
    EmptyEffectResourceLocator {
        /// Resource whose locator was empty.
        resource: EffectResourceId,
    },
    /// A clip effect references an absent effect resource.
    UnknownEffectResource {
        /// Clip containing the reference.
        clip: ClipId,
        /// Missing resource identity.
        resource: EffectResourceId,
    },
    /// A clip has an empty source or timeline range.
    EmptyClipRange {
        /// Clip with an empty range.
        clip: ClipId,
    },
    /// A clip starts before the project or source timeline origin.
    NegativeClipRange {
        /// Clip with a negative range start.
        clip: ClipId,
    },
    /// Two clips overlap without one transition covering their intersection.
    OverlappingClips {
        /// Track containing the overlap.
        track: TrackId,
        /// First overlapping clip.
        first: ClipId,
        /// Second overlapping clip.
        second: ClipId,
    },
    /// A clip refers to an absent source.
    UnknownClipSource {
        /// Clip containing the absent source reference.
        clip: ClipId,
        /// Referenced source that was not in the project.
        source: SourceId,
    },
    /// A source cannot be placed on the selected track kind.
    IncompatibleClipSource {
        /// Clip containing the incompatible reference.
        clip: ClipId,
        /// Track that cannot accept the source kind.
        track: TrackId,
        /// Source with the incompatible kind.
        source: SourceId,
    },
    /// A visual clip has non-finite values, invalid opacity, or unordered keyframes.
    InvalidVisualProperties {
        /// Clip containing the invalid presentation data.
        clip: ClipId,
    },
    /// A clip has an invalid gain, pan, fade, or audio effect parameter.
    InvalidAudioProperties {
        /// Clip containing the invalid audio data.
        clip: ClipId,
    },
    /// A track bus has an invalid gain, pan, effect, or ducking parameter.
    InvalidTrackAudio {
        /// Track containing the invalid bus data.
        track: TrackId,
    },
    /// The project master bus has an invalid gain, effect, or limiter parameter.
    InvalidMasterAudio,
    /// A ducking sidechain names a missing track, itself, or forms a cycle.
    InvalidDuckingSidechain {
        /// Track whose ducking reference is unusable.
        track: TrackId,
        /// Referenced sidechain track.
        sidechain: TrackId,
    },
    /// A clip names an absent composition.
    UnknownComposition {
        /// The clip.
        clip: ClipId,
        /// The composition it names.
        composition: CompositionId,
    },
    /// A clip's composition has a non-positive scale, reaches a composition
    /// time past [`alpha2_time::MAX_SERIALIZED_TICKS`] before the clip ends,
    /// or is on an audio clip.
    InvalidClipComposition {
        /// The clip.
        clip: ClipId,
    },
    /// Two compositions share one ID.
    DuplicateCompositionId(CompositionId),
    /// A node ID has a `/`, which joins the IDs of the group and media nodes
    /// a drawn node was inlined through.
    InvalidNodeId {
        /// The composition.
        composition: CompositionId,
        /// The node ID.
        node: NodeId,
    },
    /// Two nodes of one composition share an ID.
    DuplicateNodeId {
        /// The composition.
        composition: CompositionId,
        /// The repeated node ID.
        node: NodeId,
    },
    /// A composition has more nodes than the renderer will schedule.
    TooManyCompositionNodes {
        /// The composition.
        composition: CompositionId,
        /// Number supplied by the document.
        found: usize,
        /// Largest supported count.
        maximum: usize,
    },
    /// A node has an invalid setting: an effect identity, version,
    /// parameter, or value; a transform, progress, opacity, or mask out of
    /// range; too many merge layers or masks; or a bypass on a node without
    /// inputs.
    InvalidNode {
        /// The composition.
        composition: CompositionId,
        /// The node.
        node: NodeId,
    },
    /// A node input names a node its composition does not have.
    UnknownNodeInput {
        /// The composition.
        composition: CompositionId,
        /// The node whose input it is.
        node: NodeId,
        /// The input: `input`, `from`, `to`, `matte`, `layers[i]`, or
        /// `inputs[i]`.
        input: String,
        /// The node named.
        target: NodeId,
    },
    /// A node depends on its own output.
    CompositionCycle {
        /// The composition.
        composition: CompositionId,
        /// A node on the cycle.
        node: NodeId,
    },
    /// A composition's output names a node it does not have.
    UnknownCompositionOutput {
        /// The composition.
        composition: CompositionId,
        /// The node named.
        node: NodeId,
    },
    /// A media node names an absent source or one that is not visual.
    InvalidNodeSource {
        /// The composition.
        composition: CompositionId,
        /// The node.
        node: NodeId,
        /// The source named.
        source: SourceId,
    },
    /// A composition's group inputs are unnamed, repeated, or too many, or
    /// a control names no effect setting of its nodes or repeats one.
    InvalidCompositionPorts {
        /// The composition.
        composition: CompositionId,
    },
    /// A group node names an absent composition.
    UnknownGroupComposition {
        /// The composition holding the group node.
        composition: CompositionId,
        /// The group node.
        node: NodeId,
        /// The composition it names.
        target: CompositionId,
    },
    /// A composition uses itself through group nodes or composition
    /// sources.
    CompositionUsesItself {
        /// A composition on the loop.
        composition: CompositionId,
    },
    /// A composition draws compositions, through group nodes or
    /// composition sources, nested more than
    /// [`crate::MAX_COMPOSITION_DEPTH`] deep.
    CompositionNestedTooDeep {
        /// The first such composition, in document order.
        composition: CompositionId,
        /// Deepest supported nesting.
        maximum: usize,
    },
    /// The compositions draw more nodes than
    /// [`crate::MAX_INLINED_NODES`], each group node and composition-source
    /// media node counted as the nodes of the composition it draws.
    TooManyInlinedNodes {
        /// Largest supported count.
        maximum: usize,
    },
    /// The clips on the timeline at one instant draw more than
    /// [`crate::MAX_FRAME_INLINED_NODES`] composition nodes together.
    FrameDrawsTooManyNodes {
        /// The first clip, in document order, starting at the first such
        /// instant.
        clip: ClipId,
        /// The nodes the clips at that instant draw.
        found: u64,
        /// Largest supported count.
        maximum: usize,
    },
    /// A composition source names an absent composition, or has what only a
    /// host-supplied source has.
    InvalidCompositionSource {
        /// The source.
        source: SourceId,
    },
    /// A node's effect references an absent effect resource.
    UnknownNodeResource {
        /// The composition.
        composition: CompositionId,
        /// The node.
        node: NodeId,
        /// The resource named.
        resource: EffectResourceId,
    },
    /// A clip has more than [`crate::MAX_CLIP_MASKS`] masks, a mask with a
    /// negative or non-finite value, or masks on an audio track.
    InvalidClipMask {
        /// Clip carrying the invalid mask.
        clip: ClipId,
    },
    /// A caption track has no captions, or has clips or transitions, or its
    /// cues or style are invalid; or another track has captions.
    InvalidCaptions {
        /// The track.
        track: TrackId,
    },
    /// A caption cue names a style its track does not have.
    UnknownCaptionStyle {
        /// The track.
        track: TrackId,
        /// The cue's position among the track's cues.
        cue: usize,
        /// The style it names.
        name: String,
    },
    /// A caption cue's style, its track's with the cue's patch applied, is
    /// out of range: a font larger than a caption's, say.
    InvalidCaptionCueStyle {
        /// The track.
        track: TrackId,
        /// The cue's position among the track's cues.
        cue: usize,
    },
    /// A clip freezes on an instant outside its source range.
    InvalidTimeMap {
        /// The clip.
        clip: ClipId,
    },
    /// A clip's matte names no visual clip, itself, a clip with a matte of its
    /// own, or a clip in a transition; or an audio clip has a matte.
    InvalidMatte {
        /// Clip whose matte is invalid.
        clip: ClipId,
    },
    /// A transition is not contained in two distinct visual clips on its track.
    InvalidClipTransition {
        /// Track containing the invalid transition.
        track: TrackId,
    },
    /// Transitions active at one instant lead from clip to clip in a circle,
    /// which leaves no clip to show.
    TransitionCycle {
        /// Track containing the transitions.
        track: TrackId,
        /// An instant they are all active.
        at: alpha2_time::MediaTime,
    },
    /// Two clips have more than one transition between them, in either
    /// direction: each overlap shows exactly one.
    DuplicateTransition {
        /// Track containing the transitions.
        track: TrackId,
        /// The transition's first clip.
        first: ClipId,
        /// The transition's second clip.
        second: ClipId,
    },
    /// The project contains too many compositions.
    TooManyCompositions {
        /// Number supplied by the document.
        found: usize,
        /// Largest supported count.
        maximum: usize,
    },
    /// The source table exceeds the bounded project resource budget.
    TooManySources {
        /// Number supplied by the document.
        found: usize,
        /// Largest supported count.
        maximum: usize,
    },
    /// The timeline contains too many tracks.
    TooManyTracks {
        /// Number supplied by the document.
        found: usize,
        /// Largest supported count.
        maximum: usize,
    },
    /// The timeline contains too many clips in total.
    TooManyClips {
        /// Number supplied by the document.
        found: usize,
        /// Largest supported count.
        maximum: usize,
    },
    /// More of the project's audio effects hold state at once than
    /// [`crate::MAX_AUDIO_EFFECTS`] (see [`crate::Project::audio_effect_load`]).
    TooManyAudioEffects {
        /// The most effects holding state at once.
        found: usize,
        /// Largest supported count.
        maximum: usize,
        /// The earliest timeline position, in ticks, at which `found` hold
        /// state.
        at_ticks: i64,
    },
    /// The project's audio effects hold more than
    /// [`crate::MAX_AUDIO_DELAY_MS`] of delay at once (see
    /// [`crate::Project::audio_effect_load`]).
    TooMuchAudioDelay {
        /// The most milliseconds held at once, rounded up.
        found_ms: u64,
        /// Largest supported total.
        maximum_ms: u32,
        /// The earliest timeline position, in ticks, at which `found_ms` is
        /// held.
        at_ticks: i64,
    },
    /// One track contains too many clips.
    TooManyClipsOnTrack {
        /// Track containing the excessive clip list.
        track: TrackId,
        /// Number supplied by the document.
        found: usize,
        /// Largest supported count.
        maximum: usize,
    },
    /// A caption track holds more than [`crate::MAX_CLIPS_PER_TRACK`] cues.
    TooManyCaptionCues {
        /// The caption track.
        track: TrackId,
        /// Number supplied by the document.
        found: usize,
        /// Largest supported count.
        maximum: usize,
    },
    /// One track contains too many transitions.
    TooManyTransitions {
        /// Track containing the excessive transition list.
        track: TrackId,
        /// Number supplied by the document.
        found: usize,
        /// Largest supported count.
        maximum: usize,
    },
    /// Width or height was zero.
    ZeroCanvasDimension,
    /// A canvas edge exceeds the portable GPU-independent limit.
    CanvasDimensionTooLarge {
        /// Supplied width.
        width: u32,
        /// Supplied height.
        height: u32,
        /// Largest supported edge.
        maximum: u32,
    },
    /// The canvas area exceeds the bounded pixel budget.
    CanvasPixelBudgetExceeded {
        /// Supplied pixel area.
        pixels: u64,
        /// Largest supported pixel area.
        maximum: u64,
    },
    /// A background component is non-finite or outside `0..=1`.
    InvalidBackground,
    /// The color pipeline's HDR peak is outside
    /// [`crate::HDR_PEAK_NITS_RANGE`].
    InvalidColorPipeline,
    /// The project sample rate is outside the supported DSP interval.
    UnsupportedSampleRate {
        /// Supplied rate.
        found: u32,
        /// Smallest supported rate.
        minimum: u32,
        /// Largest supported rate.
        maximum: u32,
    },
}

impl fmt::Display for ModelError {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion { found } => {
                write!(formatter, "unsupported schema version {found}")
            }
            Self::EmptyIdentifier => formatter.write_str("document contains an empty identifier"),
            Self::DuplicateSourceId(id) => write!(formatter, "duplicate source ID {id}"),
            Self::DuplicateEffectResourceId(id) => {
                write!(formatter, "duplicate effect resource ID {id}")
            }
            Self::DuplicateTrackId(id) => write!(formatter, "duplicate track ID {id}"),
            Self::DuplicateClipId(id) => write!(formatter, "duplicate clip ID {id}"),
            Self::EmptyLocator { source } => {
                write!(formatter, "source {source} has an empty locator")
            }
            Self::AudioOnImage { source } => {
                write!(
                    formatter,
                    "source {source} is a still image and has no audio"
                )
            }
            Self::InvalidGenerator { source } => {
                write!(formatter, "source {source} has an invalid generator")
            }
            Self::InvalidShape { owner, at, problem } => {
                write!(formatter, "{owner} has an invalid shape: {at} {problem}")
            }
            Self::InvalidLayout { owner, at, problem } => {
                write!(formatter, "{owner} has an invalid layout: {at} {problem}")
            }
            Self::ContradictoryColor { source } => write!(
                formatter,
                "source {source} declares a dynamic range its transfer contradicts"
            ),
            Self::InvalidFingerprint { source } => {
                write!(formatter, "source {source} has an invalid fingerprint")
            }
            Self::InvalidProvider { source } => {
                write!(formatter, "source {source} has an invalid provider")
            }
            Self::EmptyEffectResourceLocator { resource } => {
                write!(formatter, "effect resource {resource} has an empty locator")
            }
            Self::UnknownEffectResource { clip, resource } => {
                write!(
                    formatter,
                    "clip {clip} references unknown effect resource {resource}"
                )
            }
            Self::EmptyClipRange { clip } => write!(formatter, "clip {clip} has an empty range"),
            Self::NegativeClipRange { clip } => {
                write!(formatter, "clip {clip} has a range before timeline origin")
            }
            Self::OverlappingClips {
                track,
                first,
                second,
            } => write!(
                formatter,
                "clips {first} and {second} overlap without a covering transition on track {track}"
            ),
            Self::UnknownClipSource { clip, source } => {
                write!(formatter, "clip {clip} references unknown source {source}")
            }
            Self::IncompatibleClipSource {
                clip,
                track,
                source,
            } => {
                write!(
                    formatter,
                    "clip {clip} cannot use source {source} on track {track}"
                )
            }
            Self::InvalidVisualProperties { clip } => {
                write!(formatter, "clip {clip} has invalid visual properties")
            }
            Self::InvalidAudioProperties { clip } => {
                write!(formatter, "clip {clip} has invalid audio properties")
            }
            Self::InvalidTrackAudio { track } => {
                write!(formatter, "track {track} has invalid audio bus settings")
            }
            Self::InvalidMasterAudio => {
                write!(formatter, "the project master bus is invalid")
            }
            Self::InvalidDuckingSidechain { track, sidechain } => write!(
                formatter,
                "track {track} cannot duck from sidechain track {sidechain}"
            ),
            Self::UnknownComposition { clip, composition } => {
                write!(
                    formatter,
                    "clip {clip} references unknown composition {composition}"
                )
            }
            Self::InvalidClipComposition { clip } => {
                write!(
                    formatter,
                    "clip {clip} has an invalid composition reference"
                )
            }
            Self::DuplicateCompositionId(id) => write!(formatter, "duplicate composition ID {id}"),
            Self::InvalidNodeId { composition, node } => {
                write!(
                    formatter,
                    "composition {composition} has node ID {node}, which has a `/`"
                )
            }
            Self::DuplicateNodeId { composition, node } => {
                write!(
                    formatter,
                    "composition {composition} has duplicate node ID {node}"
                )
            }
            Self::TooManyCompositionNodes {
                composition,
                found,
                maximum,
            } => write!(
                formatter,
                "composition {composition} has {found} nodes; the maximum is {maximum}"
            ),
            Self::InvalidNode { composition, node } => {
                write!(
                    formatter,
                    "node {node} of composition {composition} has an invalid setting"
                )
            }
            Self::UnknownNodeInput {
                composition,
                node,
                input,
                target,
            } => write!(
                formatter,
                "input {input} of node {node} in composition {composition} names unknown node {target}"
            ),
            Self::CompositionCycle { composition, node } => write!(
                formatter,
                "node {node} of composition {composition} depends on its own output"
            ),
            Self::UnknownCompositionOutput { composition, node } => write!(
                formatter,
                "composition {composition} outputs unknown node {node}"
            ),
            Self::InvalidNodeSource {
                composition,
                node,
                source,
            } => write!(
                formatter,
                "node {node} of composition {composition} cannot show source {source}"
            ),
            Self::InvalidCompositionPorts { composition } => write!(
                formatter,
                "composition {composition} has invalid group inputs or controls"
            ),
            Self::UnknownGroupComposition {
                composition,
                node,
                target,
            } => write!(
                formatter,
                "group node {node} of composition {composition} names unknown composition {target}"
            ),
            Self::CompositionUsesItself { composition } => {
                write!(formatter, "composition {composition} uses itself")
            }
            Self::CompositionNestedTooDeep {
                composition,
                maximum,
            } => write!(
                formatter,
                "composition {composition} nests compositions more than {maximum} deep"
            ),
            Self::TooManyInlinedNodes { maximum } => write!(
                formatter,
                "compositions draw more than {maximum} nodes, counting each group and composition source as the nodes it draws"
            ),
            Self::FrameDrawsTooManyNodes {
                clip,
                found,
                maximum,
            } => write!(
                formatter,
                "the clips on the timeline where clip {clip} starts draw {found} composition nodes; a frame draws at most {maximum}"
            ),
            Self::InvalidCompositionSource { source } => {
                write!(
                    formatter,
                    "source {source} is not a valid composition source"
                )
            }
            Self::UnknownNodeResource {
                composition,
                node,
                resource,
            } => write!(
                formatter,
                "node {node} of composition {composition} references unknown effect resource {resource}"
            ),
            Self::InvalidClipMask { clip } => write!(formatter, "clip {clip} has an invalid mask"),
            Self::InvalidMatte { clip } => write!(formatter, "clip {clip} has an invalid matte"),
            Self::InvalidCaptions { track } => {
                write!(formatter, "track {track} has invalid captions")
            }
            Self::UnknownCaptionStyle { track, cue, name } => write!(
                formatter,
                "cue {cue} of track {track} names style {name:?}, which the track does not have"
            ),
            Self::InvalidCaptionCueStyle { track, cue } => write!(
                formatter,
                "cue {cue} of track {track} has a style out of range once its patch is applied"
            ),
            Self::InvalidTimeMap { clip } => {
                write!(formatter, "clip {clip} freezes outside its source range")
            }
            Self::InvalidClipTransition { track } => {
                write!(formatter, "track {track} has an invalid clip transition")
            }
            Self::TransitionCycle { track, at } => write!(
                formatter,
                "transitions on track {track} active at {} ticks lead from clip to clip in a circle",
                at.ticks()
            ),
            Self::DuplicateTransition {
                track,
                first,
                second,
            } => write!(
                formatter,
                "clips {first} and {second} have more than one transition on track {track}"
            ),
            Self::TooManyCompositions { found, maximum } => write!(
                formatter,
                "project has {found} compositions; the maximum is {maximum}"
            ),
            Self::TooManySources { found, maximum } => {
                write!(
                    formatter,
                    "project has {found} sources; the maximum is {maximum}"
                )
            }
            Self::TooManyTracks { found, maximum } => {
                write!(
                    formatter,
                    "project has {found} tracks; the maximum is {maximum}"
                )
            }
            Self::TooManyClips { found, maximum } => {
                write!(
                    formatter,
                    "project has {found} clips; the maximum is {maximum}"
                )
            }
            Self::TooManyAudioEffects {
                found,
                maximum,
                at_ticks,
            } => write!(
                formatter,
                "project has {found} audio effects holding state at tick {at_ticks}; the maximum at once is {maximum}"
            ),
            Self::TooMuchAudioDelay {
                found_ms,
                maximum_ms,
                at_ticks,
            } => write!(
                formatter,
                "project audio effects hold {found_ms} ms of delay at tick {at_ticks}; the maximum at once is {maximum_ms} ms"
            ),
            Self::TooManyClipsOnTrack {
                track,
                found,
                maximum,
            } => write!(
                formatter,
                "track {track} has {found} clips; the maximum is {maximum}"
            ),
            Self::TooManyCaptionCues {
                track,
                found,
                maximum,
            } => write!(
                formatter,
                "track {track} has {found} caption cues; the maximum is {maximum}"
            ),
            Self::TooManyTransitions {
                track,
                found,
                maximum,
            } => write!(
                formatter,
                "track {track} has {found} transitions; the maximum is {maximum}"
            ),
            Self::ZeroCanvasDimension => formatter.write_str("canvas dimensions must be positive"),
            Self::CanvasDimensionTooLarge {
                width,
                height,
                maximum,
            } => write!(
                formatter,
                "canvas {width}x{height} exceeds the maximum edge {maximum}"
            ),
            Self::CanvasPixelBudgetExceeded { pixels, maximum } => write!(
                formatter,
                "canvas has {pixels} pixels; the maximum is {maximum}"
            ),
            Self::InvalidBackground => {
                formatter.write_str("background color components must be finite and within 0..=1")
            }
            Self::InvalidColorPipeline => {
                formatter.write_str("the HDR peak must be within 100..=10000 nits")
            }
            Self::UnsupportedSampleRate {
                found,
                minimum,
                maximum,
            } => write!(
                formatter,
                "sample rate {found} is outside {minimum}..={maximum}"
            ),
        }
    }
}

impl std::error::Error for ModelError {}

/// Failure while decoding a serialized project document.
#[derive(Debug)]
#[non_exhaustive]
pub enum ProjectLoadError {
    /// The JSON input exceeded the parser byte budget.
    TooLarge {
        /// Supplied UTF-8 byte length.
        bytes: usize,
        /// Largest accepted byte length.
        maximum: usize,
    },
    /// JSON syntax or shape was invalid.
    Json(serde_json::Error),
    /// The decoded document violated a model invariant.
    InvalidProject(ModelError),
}

impl fmt::Display for ProjectLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { bytes, maximum } => {
                write!(
                    formatter,
                    "project JSON is {bytes} bytes; the maximum is {maximum}"
                )
            }
            Self::Json(error) => write!(formatter, "invalid project JSON: {error}"),
            Self::InvalidProject(error) => write!(formatter, "invalid project: {error}"),
        }
    }
}

impl std::error::Error for ProjectLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TooLarge { .. } => None,
            Self::Json(error) => Some(error),
            Self::InvalidProject(error) => Some(error),
        }
    }
}
