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
//!
//! # A minimal extractor
//!
//! The mean brightness of each timeline frame, as a `scalars` data set:
//!
//! ```
//! use alpha2_engine::extract::{
//!     AudioInput, Descriptor, Emitter, ExtractRequest, ExtractSession, Extractor,
//!     ExtractorError, Need, PassInfo, RunContext, SuppliedAudio, SuppliedFrame, Supplier,
//!     VideoInput, delivered_size, drive,
//! };
//! use serde_json::json;
//!
//! struct Brightness(Descriptor);
//!
//! impl Extractor for Brightness {
//!     fn describe(&self) -> &Descriptor {
//!         &self.0
//!     }
//!     fn begin(&mut self, _context: &RunContext) -> Result<(), ExtractorError> {
//!         Ok(())
//!     }
//!     fn pass(&mut self, _pass: &PassInfo) -> Result<(), ExtractorError> {
//!         Ok(())
//!     }
//!     fn video(&mut self, frame: &VideoInput, out: &mut Emitter) -> Result<(), ExtractorError> {
//!         let sum: u64 = frame.bytes.iter().map(|&value| u64::from(value)).sum();
//!         let mean = sum as f32 / frame.bytes.len() as f32 / 255.0;
//!         out.sample("level", frame.time, &[Some(mean)]);
//!         Ok(())
//!     }
//!     fn audio(&mut self, _block: &AudioInput, _out: &mut Emitter) -> Result<(), ExtractorError> {
//!         Ok(())
//!     }
//!     fn end(&mut self, _out: &mut Emitter) -> Result<(), ExtractorError> {
//!         Ok(())
//!     }
//! }
//!
//! /// Grey frames; a host supplies decoded or rendered ones.
//! struct Grey;
//!
//! impl Supplier for Grey {
//!     fn frame(&mut self, need: &Need) -> Result<Option<SuppliedFrame>, String> {
//!         let Need::TimelineFrame { time, size, .. } = need else {
//!             return Err("timeline frames only".into());
//!         };
//!         let (width, height) = delivered_size((64, 36), *size);
//!         let rgba = vec![128; (width * height * 4) as usize];
//!         Ok(Some(SuppliedFrame { pts: *time, width, height, rgba }))
//!     }
//!     fn audio(&mut self, _need: &Need) -> Result<SuppliedAudio, String> {
//!         Err("no audio".into())
//!     }
//! }
//!
//! let project = alpha2_model::Project::from_json(&json!({
//!     "schema_version": 1, "id": "p", "name": "P",
//!     "settings": { "width": 64, "height": 36, "frame_rate": { "num": 30, "den": 1 },
//!                   "sample_rate": 48_000 },
//!     "sources": [], "timeline": { "tracks": [] }
//! }).to_string())?;
//! let descriptor: Descriptor = serde_json::from_value(json!({
//!     "id": "com.example.brightness", "version": 1,
//!     "input": { "media": "video", "from": ["timeline"], "format": "luma8" },
//!     "outputs": [{ "name": "level", "kind": "scalars", "channels": ["y"] }]
//! }))?;
//! let request: ExtractRequest = serde_json::from_value(json!({
//!     "extractor": "com.example.brightness", "version": 1,
//!     "input": { "from": "timeline", "range": { "start": 0, "end": 12_000 }, "size": 32 },
//!     "outputs": { "level": "brightness" }
//! }))?;
//! let session = ExtractSession::new(&project, &descriptor, request)?;
//! let sets = drive(session, &mut Brightness(descriptor), &mut Grey)?;
//! assert_eq!(sets[0].id.as_str(), "brightness");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Limits
//!
//! - Outputs per descriptor: [`MAX_EXTRACTOR_OUTPUTS`]; passes per request:
//!   [`MAX_EXTRACTOR_PASSES`].
//! - A delivered frame's long edge when the descriptor names none: [`DEFAULT_MAX_SIZE`].
//! - Frames per audio block: [`BLOCK_FRAMES_RANGE`], [`DEFAULT_BLOCK_FRAMES`] when unnamed.
//! - Plugin manifest size: [`MAX_MANIFEST_BYTES`]; artifacts and resources per manifest:
//!   [`MAX_PLUGIN_ARTIFACTS`], [`MAX_PLUGIN_RESOURCES`].
//! - Data sets per project: [`alpha2_model::MAX_DATA_SETS`]; channels per output:
//!   [`alpha2_model::MAX_DATA_CHANNELS`].
//! - Values, words and intervals per data set: [`alpha2_model::MAX_DATA_VALUES`],
//!   [`alpha2_model::MAX_DATA_WORDS`], [`alpha2_model::MAX_DATA_INTERVALS`].
//! - Data set ids, and labels, texts, speakers and channel names:
//!   [`alpha2_model::MAX_DATA_ID_BYTES`], [`alpha2_model::MAX_DATA_LABEL_BYTES`].
//! - Emitted values: [`alpha2_model::MAX_DATA_COORDINATE`] for points and similarity
//!   translation, [`alpha2_model::MAX_DATA_SCALAR`] for scalars and interval scores,
//!   [`alpha2_model::MAX_DATA_ROTATION_DEGREES`] and [`alpha2_model::MAX_DATA_SCALE`] for
//!   similarity.

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
