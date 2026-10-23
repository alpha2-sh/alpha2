//! Pure, deterministic timeline evaluation.

mod animation;
mod audio;
mod composition;
mod error;
mod evaluate;
mod frame;
mod index;
mod interval_tree;
mod retime;
mod shapes;
#[cfg(test)]
mod tests;

pub use animation::{
    EffectParamValue, sample_effect_param, sample_paint, sample_path, sample_point, sample_scalar,
    sample_vec2, sample_visual,
};
pub use audio::{
    ActiveAudioClip, AudioSpan, AudioSpanDesc, AudioSpanScratch, evaluate_audio, evaluate_audio_in,
};
pub(crate) use composition::IndexedComposition;
pub use composition::{
    ActiveComposition, CompositionPlan, MAX_PLAN_BYTES, PlanError, Step, StepLayer, StepOp,
    plan_composition, plan_source_composition,
};
pub use error::EvalError;
pub(crate) use error::{IndexedCaptions, IndexedClip, IndexedTransition};
pub(crate) use evaluate::ensure_index_matches;
pub use evaluate::{evaluate, frame_needs};
pub use frame::{
    ActiveCaption, ActiveClip, ActiveEffect, ActiveTransition, EvaluatedVisual, FrameDesc,
    FrameNeed,
};
pub use index::ActiveIndex;
pub(crate) use interval_tree::{
    IntervalNode, build_tree, collect_active, collect_overlapping, count_overlapping,
};
pub use retime::picture_source_time;
pub(crate) use retime::{Mirror, map_source_time};
pub use shapes::{ShapesPlan, evaluate_geometry, evaluate_item, evaluate_items};
