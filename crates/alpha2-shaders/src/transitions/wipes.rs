//! Bodies of the built-in transitions in the "Wipes" catalog group.

/// Bottom-to-top hard wipe transition body.
pub const WIPE_UP: &str = include_str!("../../shaders/runtime/transitions/wipe-up.wgsl");
/// Top-to-bottom hard wipe transition body.
pub const WIPE_DOWN: &str = include_str!("../../shaders/runtime/transitions/wipe-down.wgsl");
/// Right-to-left hard wipe transition body.
pub const WIPE_LEFT: &str = include_str!("../../shaders/runtime/transitions/wipe-left.wgsl");
/// Left-to-right hard wipe transition body.
pub const WIPE_RIGHT: &str = include_str!("../../shaders/runtime/transitions/wipe-right.wgsl");
/// Alternating window-blind transition body.
pub const WINDOW_BLINDS: &str =
    include_str!("../../shaders/runtime/transitions/window-blinds.wgsl");
/// Sliced-window transition body.
pub const WINDOW_SLICE: &str = include_str!("../../shaders/runtime/transitions/window-slice.wgsl");
/// Noisy wind wipe transition body.
pub const WIND: &str = include_str!("../../shaders/runtime/transitions/wind.wgsl");
/// Rotating pinwheel transition body.
pub const PINWHEEL: &str = include_str!("../../shaders/runtime/transitions/pinwheel.wgsl");
/// Heart-shaped reveal transition body.
pub const HEART: &str = include_str!("../../shaders/runtime/transitions/heart.wgsl");
/// Barn doors opening horizontally transition body.
pub const HORIZONTAL_OPEN: &str =
    include_str!("../../shaders/runtime/transitions/horizontal-open.wgsl");
/// Barn doors closing horizontally transition body.
pub const HORIZONTAL_CLOSE: &str =
    include_str!("../../shaders/runtime/transitions/horizontal-close.wgsl");
/// Barn doors opening vertically transition body.
pub const VERTICAL_OPEN: &str =
    include_str!("../../shaders/runtime/transitions/vertical-open.wgsl");
/// Barn doors closing vertically transition body.
pub const VERTICAL_CLOSE: &str =
    include_str!("../../shaders/runtime/transitions/vertical-close.wgsl");
/// Columns melting down the screen transition body.
pub const SCREEN_MELT: &str = include_str!("../../shaders/runtime/transitions/screen-melt.wgsl");
/// Directional grid-of-squares wipe transition body.
pub const SQUARES_WIRE: &str = include_str!("../../shaders/runtime/transitions/squares-wire.wgsl");
/// Wipe behind a band of static transition body.
pub const STATIC_WIPE: &str = include_str!("../../shaders/runtime/transitions/static-wipe.wgsl");
/// Zoom punch, then a leftward wipe, transition body.
pub const ZOOM_LEFT_WIPE: &str =
    include_str!("../../shaders/runtime/transitions/zoom-left-wipe.wgsl");
/// Zoom punch, then a rightward wipe, transition body.
pub const ZOOM_RIGHT_WIPE: &str =
    include_str!("../../shaders/runtime/transitions/zoom-right-wipe.wgsl");
/// Wipe along a procedural gradient transition body.
pub const GRADIENT_WIPE: &str =
    include_str!("../../shaders/runtime/transitions/gradient-wipe.wgsl");
