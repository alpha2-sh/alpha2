//! Bodies of the built-in effects in the "Keying" catalog group.

/// Author body for the built-in chroma-key effect.
pub const CHROMA_KEY_BODY: &str = include_str!("../../shaders/runtime/effects/chroma-key.wgsl");
/// Built-in luma-key body.
pub const LUMA_KEY: &str = include_str!("../../shaders/runtime/effects/luma-key.wgsl");
/// Built-in spill-suppression body.
pub const SPILL_SUPPRESS: &str = include_str!("../../shaders/runtime/effects/spill-suppress.wgsl");
/// Built-in alpha levels body.
pub const ALPHA_ADJUST: &str = include_str!("../../shaders/runtime/effects/alpha-adjust.wgsl");
/// Built-in matte erode and dilate body.
pub const MATTE_CHOKER: &str = include_str!("../../shaders/runtime/effects/matte-choker.wgsl");
/// Built-in body keeping the source where its `matte` input covers it.
pub const SET_MATTE: &str = include_str!("../../shaders/runtime/effects/set-matte.wgsl");
