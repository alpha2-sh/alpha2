//! A renderer that draws render graphs on the CPU, with no GPU and no
//! graphics driver: native code for every built-in pass, run over bands of
//! rows on a thread pool.
//!
//! It executes the same [`crate::RenderGraph`] the GPU renderer does, stage
//! for stage, and keeps the same bits between stages: premultiplied,
//! sRGB-encoded RGBA stored as half floats rounded toward zero, as a GPU's
//! half-float targets store them. The one stage it models rather than
//! matches is the fixed-function premultiplied `over` blend of vector and
//! quad passes: computed in `f32`, then stored, which is exact on lavapipe,
//! llvmpipe, WARP and Metal and within 16 half-float steps (under a quarter of
//! a code) on NVIDIA, RADV and an Intel integrated GPU. Its output is the
//! same for any thread count and on every target.

mod blend;
mod composite;
mod curves;
mod execute;
pub(crate) mod half;

/// The value a stored half float holds.
pub(crate) fn half_value(bits: u16) -> f32 {
    half::load(bits)
}

/// The value an 8-bit code samples as.
pub(crate) fn unorm8_value(code: u8) -> f32 {
    half::unorm8_value(code)
}
mod image;
mod kernels;
mod pack;
mod paint;
mod renderer;
mod source;
mod vector;
mod workers;

#[cfg(test)]
pub(crate) use vector::over as premultiplied_over;

pub use image::CpuImage;
pub use pack::pack_cpu_image;
pub use renderer::{CpuRenderStats, CpuRenderer};
pub use source::{CpuSource, RgbaPixels, YuvPlanes};
