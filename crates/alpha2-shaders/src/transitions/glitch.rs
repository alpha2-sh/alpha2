//! Bodies of the built-in transitions in the "Glitch" catalog group.

/// Datamosh strips and tears transition body.
pub const STRIP_DATAMOSH_GLITCH: &str =
    include_str!("../../shaders/runtime/transitions/strip-datamosh-glitch.wgsl");
/// Flickering block glitch transition body.
pub const DROP_ZONE_FLICKER: &str =
    include_str!("../../shaders/runtime/transitions/drop-zone-flicker.wgsl");
/// Color-driven displacement glitch transition body.
pub const GLITCH_DISPLACE: &str =
    include_str!("../../shaders/runtime/transitions/glitch-displace.wgsl");
/// Lost TV signal tracking lines transition body.
pub const OLD_TV_LOST_SIGNAL: &str =
    include_str!("../../shaders/runtime/transitions/old-tv-lost-signal.wgsl");
/// Cut through cells of static transition body.
pub const STATIC_FADE: &str = include_str!("../../shaders/runtime/transitions/static-fade.wgsl");
/// Cut through full-frame TV static transition body.
pub const TV_STATIC: &str = include_str!("../../shaders/runtime/transitions/tv-static.wgsl");
/// Color-driven spiral glitch transition body.
pub const PARAMETRIC_GLITCH: &str =
    include_str!("../../shaders/runtime/transitions/parametric-glitch.wgsl");
/// Row jumps and channel splits transition body.
pub const GLITCH: &str = include_str!("../../shaders/runtime/transitions/glitch.wgsl");
