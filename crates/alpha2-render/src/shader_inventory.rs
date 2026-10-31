//! All WGSL the renderer gives the GPU, for checking with compilers other than naga.
//!
//! naga skips the fragment-stage uniformity rule that Tint and `WebKit` enforce, so a shader
//! naga accepts can still fail in a browser.

use alpha2_shaders::{
    CANVAS_PRESENT_WGSL, COMPOSITE_WGSL, FULLSCREEN_TEXTURE_QUAD_WGSL, READBACK_PACK_WGSL,
    RGB_TO_YUV420_WGSL, RGBA_PRESENT_WGSL, YUV_REPACK_WGSL, YUV_TO_LINEAR_WGSL,
    YUV16_TO_LINEAR_WGSL,
};

use crate::EffectDef;
use crate::registry::build_shader;

/// The full module compiled for `definition`: generated preamble plus body.
#[must_use]
pub fn effect_wgsl(definition: &EffectDef) -> String {
    build_shader(definition)
}

/// Every labelled WGSL module the renderer can create: built-in pipelines plus each built-in
/// effect's and transition's generated module.
#[must_use]
pub fn builtin_wgsl_modules() -> Vec<(String, String)> {
    let mut modules = [
        // Runs only in a browser, so only browser compilers check it.
        ("canvas-present", CANVAS_PRESENT_WGSL),
        ("fullscreen-quad", FULLSCREEN_TEXTURE_QUAD_WGSL),
        ("composite", COMPOSITE_WGSL),
        ("readback-pack", READBACK_PACK_WGSL),
        ("rgba-present", RGBA_PRESENT_WGSL),
        ("rgb-to-yuv420", RGB_TO_YUV420_WGSL),
        ("yuv-repack", YUV_REPACK_WGSL),
        ("yuv-to-linear", YUV_TO_LINEAR_WGSL),
        ("yuv16-to-linear", YUV16_TO_LINEAR_WGSL),
    ]
    .map(|(label, source)| (label.to_owned(), source.to_owned()))
    .to_vec();
    modules.push(("vector".to_owned(), crate::vector::vector_wgsl()));
    let mut definitions = crate::builtins::internal_effect_definitions();
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
