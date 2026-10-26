//! A renderer that draws render graphs on the CPU, with no GPU and no
//! graphics driver: native code for every built-in pass, run over bands of
//! rows on a thread pool.
//!
//! It executes the same [`crate::RenderGraph`] the GPU renderer does, stage
//! for stage, and keeps the same bits between stages: premultiplied,
//! sRGB-encoded RGBA stored as half floats rounded toward zero. Its output
//! is the same for any thread count and on every target.

mod blend;
mod composite;
mod curves;
mod execute;
mod half;
mod image;
mod kernels;
mod pack;
mod paint;
mod renderer;
mod source;
mod vector;
mod workers;

pub use image::CpuImage;
pub use renderer::{CpuRenderStats, CpuRenderer};
pub use source::{CpuSource, RgbaPixels, YuvPlanes};
