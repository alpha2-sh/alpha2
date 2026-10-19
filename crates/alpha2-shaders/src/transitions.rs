//! File-backed WGSL bodies for the built-in transition catalog.
//! Each transition is a `.wgsl` asset under `shaders/runtime/transitions` so
//! shader authors can edit, review, and attribute shader-language source
//! without embedding it in Rust. The bodies are not standalone modules: the
//! effect registry concatenates them onto a generated per-effect preamble.
//!
//! Each body is a port of a gl-transitions shader and keeps its coordinates:
//! it defines `gl_transition` over a bottom-left uv and samples with
//! `gl_from_color` and `gl_to_color`. [`TRANSITION_HELPERS`] adapts that to
//! alpha2's top-left uv, so a direction or position parameter means what it
//! means upstream, and "up" is up.

/// Alpha and boundary helpers shared by the built-in transition bodies,
/// and the entry point that runs them in gl-transitions' bottom-left uv.
pub const TRANSITION_HELPERS: &str = include_str!("../shaders/runtime/transitions/helpers.wgsl");

/// Cross-fade transition body.
pub const FADE: &str = include_str!("../shaders/runtime/transitions/fade.wgsl");
/// Directional slide transition body.
pub const DIRECTIONAL: &str = include_str!("../shaders/runtime/transitions/directional.wgsl");
/// Circular opening or closing transition body.
pub const CIRCLE_OPEN: &str = include_str!("../shaders/runtime/transitions/circle-open.wgsl");
/// Angular sweep transition body.
pub const ANGULAR: &str = include_str!("../shaders/runtime/transitions/angular.wgsl");
/// Pixelated cross-fade transition body.
pub const PIXELIZE: &str = include_str!("../shaders/runtime/transitions/pixelize.wgsl");
/// Soft directional wipe transition body.
pub const DIRECTIONAL_WIPE: &str =
    include_str!("../shaders/runtime/transitions/directional-wipe.wgsl");
/// Circle-through-background transition body.
pub const CIRCLE: &str = include_str!("../shaders/runtime/transitions/circle.wgsl");
/// Cross-warp transition body.
pub const CROSS_WARP: &str = include_str!("../shaders/runtime/transitions/cross-warp.wgsl");
/// Dreamy vertical-warp transition body.
pub const DREAMY: &str = include_str!("../shaders/runtime/transitions/dreamy.wgsl");
/// Radial sweep transition body.
pub const RADIAL: &str = include_str!("../shaders/runtime/transitions/radial.wgsl");
/// Bottom-to-top hard wipe transition body.
pub const WIPE_UP: &str = include_str!("../shaders/runtime/transitions/wipe-up.wgsl");
/// Top-to-bottom hard wipe transition body.
pub const WIPE_DOWN: &str = include_str!("../shaders/runtime/transitions/wipe-down.wgsl");
/// Right-to-left hard wipe transition body.
pub const WIPE_LEFT: &str = include_str!("../shaders/runtime/transitions/wipe-left.wgsl");
/// Left-to-right hard wipe transition body.
pub const WIPE_RIGHT: &str = include_str!("../shaders/runtime/transitions/wipe-right.wgsl");
/// Alternating window-blind transition body.
pub const WINDOW_BLINDS: &str = include_str!("../shaders/runtime/transitions/window-blinds.wgsl");
/// Sliced-window transition body.
pub const WINDOW_SLICE: &str = include_str!("../shaders/runtime/transitions/window-slice.wgsl");
/// Noisy wind wipe transition body.
pub const WIND: &str = include_str!("../shaders/runtime/transitions/wind.wgsl");
/// Rotating pinwheel transition body.
pub const PINWHEEL: &str = include_str!("../shaders/runtime/transitions/pinwheel.wgsl");
/// Heart-shaped reveal transition body.
pub const HEART: &str = include_str!("../shaders/runtime/transitions/heart.wgsl");
/// Per-channel phased color transition body.
pub const COLOR_PHASE: &str = include_str!("../shaders/runtime/transitions/color-phase.wgsl");
/// Fade-through-color transition body.
pub const FADE_COLOR: &str = include_str!("../shaders/runtime/transitions/fade-color.wgsl");
/// Fade-through-grayscale transition body.
pub const FADE_GRAYSCALE: &str = include_str!("../shaders/runtime/transitions/fade-grayscale.wgsl");
/// Color-driven morph transition body.
pub const MORPH: &str = include_str!("../shaders/runtime/transitions/morph.wgsl");
/// Radial ripple transition body.
pub const RIPPLE: &str = include_str!("../shaders/runtime/transitions/ripple.wgsl");
/// Vertical squeeze transition body.
pub const SQUEEZE: &str = include_str!("../shaders/runtime/transitions/squeeze.wgsl");
/// Center zoom transition body.
pub const SIMPLE_ZOOM: &str = include_str!("../shaders/runtime/transitions/simple-zoom.wgsl");
/// Aspect-correct circular crop transition body.
pub const CIRCLE_CROP: &str = include_str!("../shaders/runtime/transitions/circle-crop.wgsl");
/// Color-distance reveal transition body.
pub const COLOUR_DISTANCE: &str =
    include_str!("../shaders/runtime/transitions/colour-distance.wgsl");
/// Polka-dot curtain transition body.
pub const POLKA_DOTS_CURTAIN: &str =
    include_str!("../shaders/runtime/transitions/polka-dots-curtain.wgsl");
/// Directional warp transition body.
pub const DIRECTIONAL_WARP: &str =
    include_str!("../shaders/runtime/transitions/directional-warp.wgsl");
