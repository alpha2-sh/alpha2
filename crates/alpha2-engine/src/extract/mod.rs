//! The extractor contract: how code outside the engine is supplied frames or audio and returns
//! primitives that a project records as data sets.
//!
//! An extractor runs ahead of time over a range, never while a frame renders. The host drives
//! an [`ExtractSession`]: it supplies each [`Need`] from its own decoders or renderer, hands each
//! [`Delivery`] to the extractor, and passes the extractor's [`Emit`]s back, which the session
//! validates. [`ExtractSession::finish`] returns sorted, validated data sets. Rust extractors
//! implement [`Extractor`] and run through [`drive`]; other surfaces wrap the same step machine.
//!
//! Source frames are delivered display-oriented, contain-fit to at most `size` on the long edge
//! ([`delivered_size`]), as straight-alpha sRGB `rgba8` or BT.709 `luma8` ([`luma8_from_rgba8`]).
//! The module uses plain data only; the render path never calls it.

mod descriptor;
mod drive;
#[cfg(any(test, feature = "extract-fixtures"))]
pub mod fixtures;
mod manifest;
mod pixels;
mod session;
#[cfg(test)]
mod tests;
mod wire;

pub use descriptor::{
    AudioChannels, AudioFormat, BLOCK_FRAMES_RANGE, DEFAULT_BLOCK_FRAMES, DEFAULT_MAX_SIZE,
    Descriptor, Direction, ExtractRequest, InputFrom, InputSpec, MAX_EXTRACTOR_OUTPUTS,
    MAX_EXTRACTOR_PASSES, OutputSpec, Pass, RequestInput, Suggest, VideoFormat,
};
pub use drive::{DriveError, Emitter, Extractor, ExtractorError, PassInfo, Supplier, drive};
#[cfg(feature = "schema")]
pub use manifest::plugin_manifest_json_schema;
pub use manifest::{
    ArtifactKind, MAX_MANIFEST_BYTES, MAX_PLUGIN_ARTIFACTS, MAX_PLUGIN_RESOURCES, ManifestError,
    PLUGIN_MANIFEST_SCHEMA_ID, PluginArtifact, PluginKind, PluginManifest, PluginResource,
    WASM_TARGET,
};
pub use pixels::{
    delivered_size, displayed_size, luma8_from_rgba8, mono_downmix, present_for_extraction,
};
pub use session::{
    AudioInput, Delivery, Emit, ExtractSession, FrameFrom, Need, PassProgress, RequestError,
    RunContext, SessionError, Step, SuppliedAudio, SuppliedFrame, VideoInput,
};
pub use wire::{client_context_json, emit_from_json};
