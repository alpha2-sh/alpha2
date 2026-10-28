//! Bodies of the built-in effects in the "Stylize" catalog group.

/// Built-in vignette body.
pub const VIGNETTE: &str = include_str!("../../shaders/runtime/effects/vignette.wgsl");
/// Glow's first pass: the straight color above its threshold.
pub const GLOW_THRESHOLD: &str = include_str!("../../shaders/runtime/effects/glow-threshold.wgsl");
/// Glow's last pass: the source with its blurred bloom added.
pub const GLOW_COMBINE: &str = include_str!("../../shaders/runtime/effects/glow-combine.wgsl");
/// Built-in procedural film-grain body.
pub const FILM_GRAIN: &str = include_str!("../../shaders/runtime/effects/film-grain.wgsl");
/// Built-in rgb-split body: red and blue sampled apart.
pub const RGB_SPLIT: &str = include_str!("../../shaders/runtime/effects/rgb-split.wgsl");
/// Built-in glitch effect body: rows of blocks jumping sideways over time.
pub const GLITCH_EFFECT: &str = include_str!("../../shaders/runtime/effects/glitch.wgsl");
/// Built-in crt body: a curved screen with scanlines and an aperture mask.
pub const CRT: &str = include_str!("../../shaders/runtime/effects/crt.wgsl");
/// Built-in vhs body: wobbling rows, a tracking band, chroma bleed and grain.
pub const VHS: &str = include_str!("../../shaders/runtime/effects/vhs.wgsl");
/// Built-in halftone body: a rotated grid of dots.
pub const HALFTONE: &str = include_str!("../../shaders/runtime/effects/halftone.wgsl");
/// Built-in emboss body: relief from a luminance difference.
pub const EMBOSS: &str = include_str!("../../shaders/runtime/effects/emboss.wgsl");
/// Built-in edge-detect body: Sobel gradients.
pub const EDGE_DETECT: &str = include_str!("../../shaders/runtime/effects/edge-detect.wgsl");
/// Drop shadow's first pass: the source's alpha moved, in the shadow color.
pub const SHADOW_OFFSET: &str = include_str!("../../shaders/runtime/effects/shadow-offset.wgsl");
/// Drop shadow's last pass: the source over its softened shadow.
pub const SHADOW_COMBINE: &str = include_str!("../../shaders/runtime/effects/shadow-combine.wgsl");
/// Built-in outline body: an outer stroke from ring taps.
pub const OUTLINE: &str = include_str!("../../shaders/runtime/effects/outline.wgsl");
/// Built-in light-leak body: drifting blobs of light screened over the picture.
pub const LIGHT_LEAK: &str = include_str!("../../shaders/runtime/effects/light-leak.wgsl");
/// Built-in letterbox body: opaque bars around a window of a given aspect.
pub const LETTERBOX: &str = include_str!("../../shaders/runtime/effects/letterbox.wgsl");
/// Built-in oil-paint body: a Kuwahara filter, its quadrants weighted.
pub const OIL_PAINT: &str = include_str!("../../shaders/runtime/effects/oil-paint.wgsl");
