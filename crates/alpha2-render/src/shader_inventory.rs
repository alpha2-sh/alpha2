//! The complete WGSL the renderer hands to the GPU, for checking it with
//! compilers other than the one this process links.
//!
//! wgpu validates WGSL with naga, which skips the fragment-stage uniformity
//! rule that Tint (Chrome) and `WebKit` (Safari) enforce. A shader naga accepts
//! can therefore fail to compile in a browser; these modules are what a
//! browser-side check compiles.

use alpha2_shaders::{
    CANVAS_PRESENT_WGSL, COMPOSITE_WGSL, FULLSCREEN_TEXTURE_QUAD_WGSL, READBACK_PACK_WGSL,
    RGB_TO_YUV420_WGSL, RGBA_PRESENT_WGSL, YUV_REPACK_WGSL, YUV_TO_LINEAR_WGSL,
    YUV16_TO_LINEAR_WGSL,
};

use crate::EffectDef;
use crate::registry::build_shader;

/// Returns the complete module the renderer compiles for `definition`: the
/// generated preamble for its parameters and resources, followed by its body.
#[must_use]
pub fn effect_wgsl(definition: &EffectDef) -> String {
    build_shader(definition)
}

/// Returns every WGSL module the renderer can create, labelled: the built-in
/// pipeline modules and the generated module of every built-in effect and
/// transition.
#[must_use]
pub fn builtin_wgsl_modules() -> Vec<(String, String)> {
    let mut modules = [
        // The browser binding's canvas presenter: it runs only in a browser, so a browser
        // compiler is all that checks it.
        ("canvas-present", CANVAS_PRESENT_WGSL),
        ("fullscreen-quad", FULLSCREEN_TEXTURE_QUAD_WGSL),
        ("composite", COMPOSITE_WGSL),
        ("readback-pack", READBACK_PACK_WGSL),
        ("rgba-present", RGBA_PRESENT_WGSL),
        ("rgb-to-yuv420", RGB_TO_YUV420_WGSL),
        ("yuv-repack", YUV_REPACK_WGSL),
        ("yuv-to-linear", YUV_TO_LINEAR_WGSL),
        // The same module built for 16-bit containers uploaded as integers.
        ("yuv16-to-linear", YUV16_TO_LINEAR_WGSL),
    ]
    .map(|(label, source)| (label.to_owned(), source.to_owned()))
    .to_vec();
    let mut definitions = crate::builtins::internal_effect_definitions().to_vec();
    definitions.extend(crate::builtin_effect_definitions());
    definitions.extend(crate::builtin_transition_definitions());
    modules.extend(
        definitions
            .iter()
            .map(|definition| (definition.id.clone(), effect_wgsl(definition))),
    );
    modules
}

#[cfg(test)]
#[path = "shader_inventory_tests.rs"]
mod tests;
