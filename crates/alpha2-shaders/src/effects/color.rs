//! Bodies of the built-in effects in the "Color" catalog group.

/// Author body for the built-in resource-backed 3D LUT effect.
pub const LUT_3D_BODY: &str = include_str!("../../shaders/runtime/effects/lut-3d.wgsl");
/// Built-in exposure, contrast, saturation, temperature, and tint body.
pub const COLOR_CONTROLS: &str = include_str!("../../shaders/runtime/effects/color-controls.wgsl");
/// Built-in invert body.
pub const INVERT: &str = include_str!("../../shaders/runtime/effects/invert.wgsl");
/// Built-in black-and-white channel mixer body.
pub const BLACK_AND_WHITE: &str =
    include_str!("../../shaders/runtime/effects/black-and-white.wgsl");
/// Built-in sepia body.
pub const SEPIA: &str = include_str!("../../shaders/runtime/effects/sepia.wgsl");
/// Built-in two-color duotone body.
pub const DUOTONE: &str = include_str!("../../shaders/runtime/effects/duotone.wgsl");
/// Built-in three-color gradient map body.
pub const GRADIENT_MAP: &str = include_str!("../../shaders/runtime/effects/gradient-map.wgsl");
/// Built-in posterize body.
pub const POSTERIZE: &str = include_str!("../../shaders/runtime/effects/posterize.wgsl");
/// Built-in black-and-white threshold body.
pub const THRESHOLD: &str = include_str!("../../shaders/runtime/effects/threshold.wgsl");
/// Built-in levels body.
pub const LEVELS: &str = include_str!("../../shaders/runtime/effects/levels.wgsl");
/// Built-in hue, saturation, and lightness body.
pub const HUE_SATURATION: &str = include_str!("../../shaders/runtime/effects/hue-saturation.wgsl");
/// Built-in vibrance body.
pub const VIBRANCE: &str = include_str!("../../shaders/runtime/effects/vibrance.wgsl");
/// Built-in channel mixer body.
pub const CHANNEL_MIXER: &str = include_str!("../../shaders/runtime/effects/channel-mixer.wgsl");
/// Built-in lift, gamma, and gain body.
pub const LIFT_GAMMA_GAIN: &str =
    include_str!("../../shaders/runtime/effects/lift-gamma-gain.wgsl");
/// Built-in color isolate (one hue kept) body.
pub const COLOR_ISOLATE: &str = include_str!("../../shaders/runtime/effects/color-isolate.wgsl");
/// Built-in solarize body.
pub const SOLARIZE: &str = include_str!("../../shaders/runtime/effects/solarize.wgsl");
/// Built-in color fill body.
pub const FILL: &str = include_str!("../../shaders/runtime/effects/fill.wgsl");
