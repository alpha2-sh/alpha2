//! Bodies of the built-in transitions in the "Classic" catalog group.

/// Cross-fade transition body.
pub const FADE: &str = include_str!("../../shaders/runtime/transitions/fade.wgsl");
/// Directional slide transition body.
pub const DIRECTIONAL: &str = include_str!("../../shaders/runtime/transitions/directional.wgsl");
/// Circular opening or closing transition body.
pub const CIRCLE_OPEN: &str = include_str!("../../shaders/runtime/transitions/circle-open.wgsl");
/// Angular sweep transition body.
pub const ANGULAR: &str = include_str!("../../shaders/runtime/transitions/angular.wgsl");
/// Pixelated cross-fade transition body.
pub const PIXELIZE: &str = include_str!("../../shaders/runtime/transitions/pixelize.wgsl");
/// Soft directional wipe transition body.
pub const DIRECTIONAL_WIPE: &str =
    include_str!("../../shaders/runtime/transitions/directional-wipe.wgsl");
/// Circle-through-background transition body.
pub const CIRCLE: &str = include_str!("../../shaders/runtime/transitions/circle.wgsl");
/// Cross-warp transition body.
pub const CROSS_WARP: &str = include_str!("../../shaders/runtime/transitions/cross-warp.wgsl");
/// Dreamy vertical-warp transition body.
pub const DREAMY: &str = include_str!("../../shaders/runtime/transitions/dreamy.wgsl");
/// Radial sweep transition body.
pub const RADIAL: &str = include_str!("../../shaders/runtime/transitions/radial.wgsl");
/// Picture dropping away with bounces and a contact shadow transition body.
pub const BOUNCE: &str = include_str!("../../shaders/runtime/transitions/bounce.wgsl");
/// Directional push that shrinks both pictures mid-way transition body.
pub const DIRECTIONAL_SCALED: &str =
    include_str!("../../shaders/runtime/transitions/directional-scaled.wgsl");
/// Scaling slide from an edge, a corner or the centre transition body.
pub const SLIDES: &str = include_str!("../../shaders/runtime/transitions/slides.wgsl");
/// Squash push transition body.
pub const FOLD: &str = include_str!("../../shaders/runtime/transitions/fold.wgsl");
/// Eased directional push transition body.
pub const DIRECTIONAL_EASING: &str =
    include_str!("../../shaders/runtime/transitions/directional-easing.wgsl");
/// Cross-fade through hue, saturation and value transition body.
pub const HSV_FADE: &str = include_str!("../../shaders/runtime/transitions/hsv-fade.wgsl");
/// Slide in from the right over a receding picture transition body.
pub const LEFT_RIGHT: &str = include_str!("../../shaders/runtime/transitions/left-right.wgsl");
/// Slide in vertically over a receding picture transition body.
pub const TOP_BOTTOM: &str = include_str!("../../shaders/runtime/transitions/top-bottom.wgsl");
/// Cross-fade through the two pictures' product transition body.
pub const MULTIPLY_BLEND: &str =
    include_str!("../../shaders/runtime/transitions/multiply-blend.wgsl");
/// Picture rolling away around a corner transition body.
pub const ROLLS: &str = include_str!("../../shaders/runtime/transitions/rolls.wgsl");
/// Two halves sliding in horizontally transition body.
pub const SPLIT_SLIDE_IN_HORIZONTAL: &str =
    include_str!("../../shaders/runtime/transitions/split-slide-in-horizontal.wgsl");
/// Two halves sliding in vertically transition body.
pub const SPLIT_SLIDE_IN_VERTICAL: &str =
    include_str!("../../shaders/runtime/transitions/split-slide-in-vertical.wgsl");
/// Two halves sliding out horizontally transition body.
pub const SPLIT_SLIDE_OUT_HORIZONTAL: &str =
    include_str!("../../shaders/runtime/transitions/split-slide-out-horizontal.wgsl");
/// Two halves sliding out vertically transition body.
pub const SPLIT_SLIDE_OUT_VERTICAL: &str =
    include_str!("../../shaders/runtime/transitions/split-slide-out-vertical.wgsl");
/// Split horizontal push transition body.
pub const SPLIT_SLIDE_IN_OUT_HORIZONTAL: &str =
    include_str!("../../shaders/runtime/transitions/split-slide-in-out-horizontal.wgsl");
/// Split vertical push transition body.
pub const SPLIT_SLIDE_IN_OUT_VERTICAL: &str =
    include_str!("../../shaders/runtime/transitions/split-slide-in-out-vertical.wgsl");
/// Spinning, tiled cross-fade transition body.
pub const ROTATE: &str = include_str!("../../shaders/runtime/transitions/rotate.wgsl");
/// Sideways shift over a cross-fade transition body.
pub const X_AXIS_TRANSLATION: &str =
    include_str!("../../shaders/runtime/transitions/x-axis-translation.wgsl");
/// Incoming picture sliding over the outgoing one transition body.
pub const COVER: &str = include_str!("../../shaders/runtime/transitions/cover.wgsl");
/// Outgoing picture sliding off the incoming one transition body.
pub const UNCOVER: &str = include_str!("../../shaders/runtime/transitions/uncover.wgsl");
