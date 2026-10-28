//! Bodies of the built-in transitions in the "Stylized" catalog group.

/// Per-channel phased color transition body.
pub const COLOR_PHASE: &str = include_str!("../../shaders/runtime/transitions/color-phase.wgsl");
/// Fade-through-color transition body.
pub const FADE_COLOR: &str = include_str!("../../shaders/runtime/transitions/fade-color.wgsl");
/// Fade-through-grayscale transition body.
pub const FADE_GRAYSCALE: &str =
    include_str!("../../shaders/runtime/transitions/fade-grayscale.wgsl");
/// Color-driven morph transition body.
pub const MORPH: &str = include_str!("../../shaders/runtime/transitions/morph.wgsl");
/// Radial ripple transition body.
pub const RIPPLE: &str = include_str!("../../shaders/runtime/transitions/ripple.wgsl");
/// Vertical squeeze transition body.
pub const SQUEEZE: &str = include_str!("../../shaders/runtime/transitions/squeeze.wgsl");
/// Center zoom transition body.
pub const SIMPLE_ZOOM: &str = include_str!("../../shaders/runtime/transitions/simple-zoom.wgsl");
/// Aspect-correct circular crop transition body.
pub const CIRCLE_CROP: &str = include_str!("../../shaders/runtime/transitions/circle-crop.wgsl");
/// Color-distance reveal transition body.
pub const COLOUR_DISTANCE: &str =
    include_str!("../../shaders/runtime/transitions/colour-distance.wgsl");
/// Polka-dot curtain transition body.
pub const POLKA_DOTS_CURTAIN: &str =
    include_str!("../../shaders/runtime/transitions/polka-dots-curtain.wgsl");
/// Directional warp transition body.
pub const DIRECTIONAL_WARP: &str =
    include_str!("../../shaders/runtime/transitions/directional-warp.wgsl");
/// Per-block random dissolve transition body.
pub const BLOCK_DISSOLVE: &str =
    include_str!("../../shaders/runtime/transitions/block-dissolve.wgsl");
/// Random-squares dissolve transition body.
pub const RANDOM_SQUARES: &str =
    include_str!("../../shaders/runtime/transitions/random-squares.wgsl");
/// Noise burn-away transition body.
pub const BURN_NOISE: &str = include_str!("../../shaders/runtime/transitions/burn-noise.wgsl");
/// Value-noise dissolve transition body.
pub const PERLIN: &str = include_str!("../../shaders/runtime/transitions/perlin.wgsl");
/// Film burn and light leak transition body.
pub const FILM_BURN: &str = include_str!("../../shaders/runtime/transitions/film-burn.wgsl");
/// Overexposed flash cross-fade transition body.
pub const OVEREXPOSURE: &str = include_str!("../../shaders/runtime/transitions/overexposure.wgsl");
/// Center-anchored growing mosaic transition body.
pub const ADVANCED_MOSAIC: &str =
    include_str!("../../shaders/runtime/transitions/advanced-mosaic.wgsl");
/// Hot flash cross-fade transition body.
pub const BURN: &str = include_str!("../../shaders/runtime/transitions/burn.wgsl");
/// Scratchy radial reveal transition body.
pub const CROSSHATCH: &str = include_str!("../../shaders/runtime/transitions/crosshatch.wgsl");
/// Chromatic butterfly-wave transition body.
pub const BUTTERFLY_WAVE_SCRAWLER: &str =
    include_str!("../../shaders/runtime/transitions/butterfly-wave-scrawler.wgsl");
/// Parametric-curve swirl transition body.
pub const CRAZY_PARAMETRIC_FUN: &str =
    include_str!("../../shaders/runtime/transitions/crazy-parametric-fun.wgsl");
/// Edge-detect flash transition body.
pub const EDGE: &str = include_str!("../../shaders/runtime/transitions/edge.wgsl");
/// Compound-eye lens transition body.
pub const FLY_EYE: &str = include_str!("../../shaders/runtime/transitions/fly-eye.wgsl");
/// Hexagonal pixelation transition body.
pub const HEXAGONALIZE: &str = include_str!("../../shaders/runtime/transitions/hexagonalize.wgsl");
/// Kaleidoscope cross-fade transition body.
pub const KALEIDOSCOPE: &str = include_str!("../../shaders/runtime/transitions/kaleidoscope.wgsl");
/// Luma-threshold burn transition body.
pub const LUMA_BURN: &str = include_str!("../../shaders/runtime/transitions/luma-burn.wgsl");
/// Luminance melt transition body.
pub const LUMINANCE_MELT: &str =
    include_str!("../../shaders/runtime/transitions/luminance-melt.wgsl");
/// Spinning kaleidoscope zoom transition body.
pub const POWER_KALEIDO: &str =
    include_str!("../../shaders/runtime/transitions/power-kaleido.wgsl");
/// Per-pixel random dissolve transition body.
pub const RANDOM_NOISE: &str = include_str!("../../shaders/runtime/transitions/random-noise.wgsl");
/// Swirl cross-fade transition body.
pub const SWIRL: &str = include_str!("../../shaders/runtime/transitions/swirl.wgsl");
/// Wavy burning ring transition body.
pub const UNDULATING_BURN_OUT: &str =
    include_str!("../../shaders/runtime/transitions/undulating-burn-out.wgsl");
/// Water-drop ripple transition body.
pub const WATER_DROP: &str = include_str!("../../shaders/runtime/transitions/water-drop.wgsl");
