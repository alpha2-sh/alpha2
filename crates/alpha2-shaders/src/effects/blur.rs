//! Bodies of the built-in effects in the "Blur & Sharpen" catalog group.

/// Author body for one axis pass of the built-in Gaussian blur cascade.
pub const GAUSSIAN_BLUR: &str = include_str!("../../shaders/runtime/effects/gaussian-blur.wgsl");
/// Built-in sharpening body.
pub const SHARPEN: &str = include_str!("../../shaders/runtime/effects/sharpen.wgsl");
/// Built-in zoom and spin blur body.
pub const RADIAL_BLUR: &str = include_str!("../../shaders/runtime/effects/radial-blur.wgsl");
/// Built-in disc (bokeh) blur body.
pub const LENS_BLUR: &str = include_str!("../../shaders/runtime/effects/lens-blur.wgsl");
/// Built-in edge-preserving (bilateral) blur body.
pub const SURFACE_BLUR: &str = include_str!("../../shaders/runtime/effects/surface-blur.wgsl");
/// Unsharp mask's last pass: the source pushed away from its blur.
pub const UNSHARP_COMBINE: &str =
    include_str!("../../shaders/runtime/effects/unsharp-combine.wgsl");
/// Tilt-shift's last pass: the source and two blurs mixed by the distance from a band.
pub const FOCUS_COMBINE: &str = include_str!("../../shaders/runtime/effects/focus-combine.wgsl");
