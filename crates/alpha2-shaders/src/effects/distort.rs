//! Bodies of the built-in effects in the "Distort" catalog group.

/// Built-in pixelation body.
pub const PIXELATE_EFFECT: &str = include_str!("../../shaders/runtime/effects/pixelate.wgsl");
/// Built-in crop body.
pub const CROP: &str = include_str!("../../shaders/runtime/effects/crop.wgsl");
/// Built-in mirror body.
pub const MIRROR: &str = include_str!("../../shaders/runtime/effects/mirror.wgsl");
/// Built-in swirl body.
pub const SWIRL_EFFECT: &str = include_str!("../../shaders/runtime/effects/swirl.wgsl");
/// Built-in bulge body.
pub const BULGE: &str = include_str!("../../shaders/runtime/effects/bulge.wgsl");
/// Built-in wave warp body.
pub const WAVE_WARP: &str = include_str!("../../shaders/runtime/effects/wave-warp.wgsl");
/// Built-in ripple body.
pub const RIPPLE_EFFECT: &str = include_str!("../../shaders/runtime/effects/ripple.wgsl");
/// Built-in lens distortion body.
pub const LENS_DISTORTION: &str =
    include_str!("../../shaders/runtime/effects/lens-distortion.wgsl");
/// Built-in polar coordinates body.
pub const POLAR_COORDINATES: &str =
    include_str!("../../shaders/runtime/effects/polar-coordinates.wgsl");
/// Built-in offset body.
pub const OFFSET: &str = include_str!("../../shaders/runtime/effects/offset.wgsl");
/// Built-in corner pin body.
pub const CORNER_PIN: &str = include_str!("../../shaders/runtime/effects/corner-pin.wgsl");
/// Built-in shake body.
pub const SHAKE: &str = include_str!("../../shaders/runtime/effects/shake.wgsl");
/// Built-in turbulent displace body.
pub const TURBULENT_DISPLACE: &str =
    include_str!("../../shaders/runtime/effects/turbulent-displace.wgsl");
/// Built-in displacement map body.
pub const DISPLACEMENT_MAP: &str =
    include_str!("../../shaders/runtime/effects/displacement-map.wgsl");
/// Built-in kaleidoscope body.
pub const KALEIDOSCOPE_EFFECT: &str =
    include_str!("../../shaders/runtime/effects/kaleidoscope.wgsl");
/// Built-in tile body.
pub const TILE: &str = include_str!("../../shaders/runtime/effects/tile.wgsl");
