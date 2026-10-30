//! Versioned, host-independent project model for alpha2.
//!
//! This crate describes intent only. It contains no decoder handles, GPU
//! resources, filesystem paths, or application-specific types.

mod animation;
mod audio;
mod audio_load;
mod caption;
mod caption_import;
mod checks;
mod composition;
mod easing;
mod effect;
mod error;
mod fingerprint;
mod generator;
mod ids;
mod layout;
mod layout_values;
mod lint;
mod mask;
mod project;
mod provider;
mod resource;
#[cfg(feature = "schema")]
mod schema;
mod shapes;
mod source;
#[cfg(test)]
pub(crate) mod tests;
mod text;
mod text_animation;
mod time_map;
mod timeline;

pub use animation::{
    Animated, AnimatedPoint, Interpolation, Keyframe, PointKeyframe, Vec2, VisualProperties,
};
pub use audio::{
    AUDIO_EFFECT_STATE_MARGIN_MS, AudioEffect, AudioProperties, Ducking, EQ_FREQUENCY_RANGE_HZ,
    EQ_Q_RANGE, EqBand, EqBandKind, FadeShape, Limiter, MAX_AUDIO_DELAY_MS, MAX_AUDIO_EFFECTS,
    MAX_CHAIN_EFFECTS, MAX_COMPRESSOR_RATIO, MAX_ENVELOPE_MS, MAX_EQ_BANDS, MAX_EQ_GAIN_DB,
    MAX_GAIN, MAX_LOOKAHEAD_MS, MIN_LEVEL_DB, MasterAudio, REVERB_DELAY_MS, TrackAudio,
};
pub use audio_load::AudioEffectLoad;
pub use caption::{
    CaptionCue, Captions, MAX_CAPTION_FONT_SIZE, MAX_CAPTION_NAME_BYTES, MAX_CAPTION_STYLES,
    TextStylePatch,
};
pub use caption_import::{
    CaptionImportError, ImportNotes, ImportedCaptions, MAX_CAPTION_FILE_BYTES,
    MAX_CAPTION_FILE_CUES, caption_rows, parse_srt, parse_webvtt,
};
pub use composition::{
    ClipComposition, Composition, CompositionControl, CompositionNode, CompositionReads,
    MAX_COMPOSITION_CONTROLS, MAX_COMPOSITION_DEPTH, MAX_COMPOSITION_NODES, MAX_COMPOSITIONS,
    MAX_EFFECT_INPUTS, MAX_FRAME_INLINED_NODES, MAX_GROUP_INPUTS, MAX_INLINED_NODES,
    MAX_MERGE_LAYERS, MergeLayer, NodeInput, NodeOp, TimeScale,
};
pub use easing::{CubicBezier, Easing};
pub use effect::{ClipBlendMode, ClipEffect, EffectValue};
pub use error::{ModelError, ProjectLoadError, ShapeOwner};
pub use fingerprint::{AudioStreamShape, MediaFingerprint, StreamTopology, VideoStreamShape};
pub use generator::{Generator, GradientStop, MAX_GRADIENT_STOPS, Paint, Rgba, Spread};
pub use ids::{ClipId, CompositionId, EffectResourceId, NodeId, ProjectId, TrackId};
pub use layout::{
    AlignContent, AlignItems, Display, FlexDirection, FlexWrap, GeneratorIssue, GridAutoFlow,
    JustifyContent, LAYOUT_TEXT_DEFAULTS, LayoutBorder, LayoutInset, LayoutIssue, LayoutNode,
    LayoutProblem, LayoutShape, LayoutTextStyle, MAX_GRID_TRACKS, MAX_LAYOUT_DEPTH,
    MAX_LAYOUT_FONT_SIZE, MAX_LAYOUT_ISOLATED, MAX_LAYOUT_NODES, MAX_LAYOUT_TEXTS, Overflow,
    Position,
};
pub use layout_values::{
    GridEdge, GridPlacement, Insets, LayoutLength, LayoutOffset, LayoutSize, RepeatCount,
    TrackBreadth, TrackComponent, TrackList, TrackSize,
};
pub use lint::{LintEdge, LintIssue, LintOwner, ShapeLintOwner};
pub use mask::{MAX_CLIP_MASKS, MaskMode, Matte, MatteMode, ShapeMask};
pub use project::{
    BlendSpace, CURRENT_SCHEMA_VERSION, ColorPipeline, HDR_PEAK_NITS_RANGE, HdrSources,
    MAX_CANVAS_DIMENSION, MAX_CANVAS_PIXELS, MAX_CLIPS, MAX_CLIPS_PER_TRACK,
    MAX_PROJECT_JSON_BYTES, MAX_TRACKS, MAX_TRANSITIONS_PER_TRACK, Project, ProjectSettings, Rgb,
    SAMPLE_RATE_RANGE,
};
pub use provider::{
    LinearPrimaries, MAX_LOTTIE_DOCUMENT_BYTES, MAX_PROVIDER_DOCUMENT_BYTES, MAX_SEQUENCE_FRAMES,
    MAX_SVG_DOCUMENT_BYTES, MissingFrames, ProviderSource,
};
pub use resource::{EffectResource, EffectResourceKind};
#[cfg(feature = "schema")]
pub use schema::{finish_json_schema, project_json_schema, project_limits_json};
pub use shapes::{
    Dash, EllipseGeometry, FillRule, LineCap, LineJoin, MAX_DASH_ENTRIES, MAX_PATH_SUBPATHS,
    MAX_PATH_VERTICES, MAX_POLYSTAR_POINTS, MAX_SHAPE_DEPTH, MAX_SHAPE_ITEMS, MIN_POLYSTAR_POINTS,
    Path, PathGeometry, PathVertex, PolystarGeometry, PolystarKind, RectangleGeometry,
    ShapeGeometry, ShapeIssue, ShapeItem, ShapeProblem, ShapeTransform, StrokeItem, Subpath,
    TrimItem, TrimMode, validate_shape_items,
};
pub use source::{MediaKind, MediaSource};
pub use text::{
    FONT_WEIGHT_RANGE, LETTER_SPACING_RANGE, LINE_HEIGHT_RANGE, MAX_FONT_FAMILY_BYTES,
    MAX_TEXT_CONTENT_BYTES, MAX_TEXT_GENERATOR_FONT_SIZE, MAX_TEXT_PADDING, MAX_TEXT_SHADOW_BLUR,
    MAX_TEXT_SHADOW_OFFSET, MAX_TEXT_SHADOWS, MAX_TEXT_STROKE_EXTENT, MAX_TEXT_STROKES, TextAlign,
    TextBackground, TextShadow, TextStroke, TextStyle,
};
pub use text_animation::{
    ANIMATOR_SCALE_RANGE, MAX_CLASS_NAME_BYTES, MAX_RANGE_INDEX, MAX_RUN_CLASSES,
    MAX_TEXT_ANIMATORS, MAX_TEXT_RUNS, RANGE_PERCENT, RangeShape, RangeUnits, RunState, RunTime,
    TextAnimator, TextAnimatorProperties, TextContent, TextRun, TextSelector, TextUnit,
    valid_class_name,
};
pub use time_map::{LOOP_FADE_SECONDS, LoopMode, TimeMap};
pub use timeline::{CacheIntent, Clip, ClipTransition, Timeline, Track, TrackKind};
