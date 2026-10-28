//! The built-in passes ported to native code: one kernel per built-in
//! effect, transition and internal pass, keyed by the identity and version
//! of the [`EffectDef`] whose WGSL it ports.
//!
//! A kernel shades rows. [`KernelContext`] gives it what the WGSL prelude
//! gives a shader (`registry.rs` `build_shader`): its inputs sampled
//! bilinearly with the edge repeated, its parameters with their defaults,
//! and the runtime values.

use crate::cpu::image::{CpuImage, PixelBox};
use crate::{EffectDef, EffectParams, EffectRuntime, ParamValue, RenderSize};

mod effects;
mod generator;
mod internal;
mod prelude;
mod transitions;

#[allow(unused_imports)]
pub(crate) use prelude::*;

/// A 3D lookup table as the effect samples it: the values stored as the GPU
/// texture stores them (half floats rounded to nearest).
#[derive(Clone, Debug)]
pub(crate) struct CpuLut {
    pub(crate) edge: u32,
    pub(crate) domain_min: [f32; 3],
    pub(crate) domain_max: [f32; 3],
    pub(crate) values: Vec<[f32; 3]>,
}

/// What one pass reads: the WGSL prelude's bindings.
pub(crate) struct KernelContext<'a> {
    pub(crate) size: RenderSize,
    pub(crate) definition: &'a EffectDef,
    pub(crate) params: &'a EffectParams<'a>,
    pub(crate) runtime: EffectRuntime,
    pub(crate) max_filter_samples: u32,
    /// The effect's source, or a transition's `from`; transparent for a
    /// generator.
    pub(crate) source_a: &'a CpuImage,
    /// A transition's `to`; the source again for anything else.
    pub(crate) source_b: &'a CpuImage,
    /// Named inputs, by the names the definition declares.
    pub(crate) inputs: &'a [(&'a str, &'a CpuImage)],
    /// Resources, by the names the definition declares.
    pub(crate) resources: &'a [(&'a str, &'a CpuLut)],
}

impl KernelContext<'_> {
    /// The value of parameter `name`: as supplied, else the definition's
    /// default.
    pub(crate) fn param(&self, name: &str) -> ParamValue {
        if let Some(value) = self.params.get(name) {
            return value.clone();
        }
        self.definition
            .params
            .iter()
            .find(|spec| spec.name == name)
            .map_or(ParamValue::Float(0.0), |spec| spec.default.clone())
    }

    pub(crate) fn float(&self, name: &str) -> f32 {
        match self.param(name) {
            ParamValue::Float(value) => value,
            _ => 0.0,
        }
    }

    pub(crate) fn vec2(&self, name: &str) -> [f32; 2] {
        match self.param(name) {
            ParamValue::Vec2(value) => value,
            _ => [0.0; 2],
        }
    }

    /// A `vec4` or color parameter.
    pub(crate) fn vec4(&self, name: &str) -> [f32; 4] {
        match self.param(name) {
            ParamValue::Vec4(value) | ParamValue::Color(value) => value,
            _ => [0.0; 4],
        }
    }

    pub(crate) fn choice(&self, name: &str) -> u32 {
        match self.param(name) {
            ParamValue::Enum(value) => value,
            _ => 0,
        }
    }

    pub(crate) fn boolean(&self, name: &str) -> bool {
        matches!(self.param(name), ParamValue::Bool(true))
    }

    /// `resolution()`: the output size in pixels.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn resolution(&self) -> [f32; 2] {
        [self.size.width as f32, self.size.height as f32]
    }

    /// `time_ticks()`: the time's ticks as two words, low first.
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn time_ticks(&self) -> [u32; 2] {
        let ticks = self.runtime.time.ticks().cast_unsigned();
        [ticks as u32, (ticks >> 32) as u32]
    }

    /// `time()`: the time in seconds, rounded once from the exact quotient
    /// as the runtime uniform holds it.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    pub(crate) fn time(&self) -> f32 {
        (self.runtime.time.ticks() as f64 / alpha2_time::TICKS_PER_SECOND as f64) as f32
    }

    pub(crate) const fn progress(&self) -> f32 {
        self.runtime.progress
    }

    pub(crate) const fn ratio(&self) -> f32 {
        self.runtime.ratio
    }

    /// `max_filter_samples()`.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) const fn max_filter_samples(&self) -> f32 {
        self.max_filter_samples as f32
    }

    /// `sample_src(uv)`.
    #[inline]
    pub(crate) fn sample_src(&self, uv: [f32; 2]) -> [f32; 4] {
        self.source_a.sample(uv[0], uv[1])
    }

    /// `from_color(uv)`.
    #[inline]
    #[allow(clippy::wrong_self_convention)]
    pub(crate) fn from_color(&self, uv: [f32; 2]) -> [f32; 4] {
        self.source_a.sample(uv[0], uv[1])
    }

    /// `to_color(uv)`.
    #[inline]
    pub(crate) fn to_color(&self, uv: [f32; 2]) -> [f32; 4] {
        self.source_b.sample(uv[0], uv[1])
    }

    /// The lookup table bound as `name`.
    pub(crate) fn lut(&self, name: &str) -> Option<&CpuLut> {
        self.resources
            .iter()
            .find(|(resource, _)| *resource == name)
            .map(|(_, lut)| *lut)
    }
}

/// One built-in pass in native code.
pub(crate) trait CpuKernel: Sync + Send {
    /// The pixels the pass can make non-transparent; outside them it writes
    /// transparency. The whole picture unless a kernel knows better.
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        PixelBox::whole(context.size)
    }

    /// Shades the pixels `x0..x0 + out.len()` of row `y` into `out`, as
    /// the shader returns them, before storage.
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]);
}

/// The kernel that draws the built-in `id` at `version`, if there is one.
pub(crate) fn kernel(id: &str, version: u32) -> Option<&'static dyn CpuKernel> {
    if version != 1 {
        return None;
    }
    effects::kernel(id)
        .or_else(|| transitions::kernel(id))
        .or_else(|| internal::kernel(id))
        .or_else(|| generator::kernel(id))
}
