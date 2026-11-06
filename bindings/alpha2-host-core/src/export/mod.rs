//! Exports driven on the host's own threads: frame needs, decoded sources, the store the
//! renderer reads, and the encoder sink.

pub mod frame_store;
mod prepare;
mod run;

pub use crate::encode::PlaneBuffer;
pub use prepare::{GpuSource, prepare_cpu, prepare_gpu};
pub use run::{
    ExportError, ExportRenderer, MAX_EXPORT_REFILLS, SourceFrames, SourceStream, StreamSources,
    VideoSink, export_video,
};
