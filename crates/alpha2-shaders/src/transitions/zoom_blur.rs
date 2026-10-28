//! Bodies of the built-in transitions in the "Zoom & Blur" catalog group.

/// Defocus-blur cross-fade transition body.
pub const DEFOCUS_BLUR: &str = include_str!("../../shaders/runtime/transitions/defocus-blur.wgsl");
/// Rotating zoom through a white flash transition body.
pub const DREAMY_ZOOM: &str = include_str!("../../shaders/runtime/transitions/dreamy-zoom.wgsl");
/// Spin-zoom with motion blur transition body.
pub const REVOLVE: &str = include_str!("../../shaders/runtime/transitions/revolve.wgsl");
/// Box-blur cross-fade transition body.
pub const LINEAR_BLUR: &str = include_str!("../../shaders/runtime/transitions/linear-blur.wgsl");
/// Zoom over a wall of turned tiles transition body.
pub const MOSAIC_TILES: &str = include_str!("../../shaders/runtime/transitions/mosaic-tiles.wgsl");
/// Spin and scale through the frame's edge transition body.
pub const ROTATE_SCALE_FADE: &str =
    include_str!("../../shaders/runtime/transitions/rotate-scale-fade.wgsl");
/// Spin and shrink into the next picture transition body.
pub const ROTATE_SCALE_VANISH: &str =
    include_str!("../../shaders/runtime/transitions/rotate-scale-vanish.wgsl");
/// Zoom-in cross-fade transition body.
pub const SCALE_IN: &str = include_str!("../../shaders/runtime/transitions/scale-in.wgsl");
/// Zoom-out reveal transition body.
pub const SIMPLE_ZOOM_OUT: &str =
    include_str!("../../shaders/runtime/transitions/simple-zoom-out.wgsl");
/// Concentric ring zoom transition body.
pub const ZOOM_IN_CIRCLES: &str =
    include_str!("../../shaders/runtime/transitions/zoom-in-circles.wgsl");
/// Zoom-through cross-fade transition body.
pub const ZOOM_IN_OUT: &str = include_str!("../../shaders/runtime/transitions/zoom-in-out.wgsl");
/// Radial zoom-blur cross-fade transition body.
pub const ZOOM_BLUR: &str = include_str!("../../shaders/runtime/transitions/zoom-blur.wgsl");
/// Motion-blurred push transition body.
pub const WHIP_PAN: &str = include_str!("../../shaders/runtime/transitions/whip-pan.wgsl");
