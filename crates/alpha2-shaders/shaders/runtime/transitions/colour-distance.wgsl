// Port of gl-transitions/transitions/ColourDistance.glsl
// Author: P-Seebauer
// License: MIT
// Softened over a fixed width of colour distance rather than a pixel footprint, which
// would be the image's own derivatives that GPUs measure differently.
const COLOUR_DISTANCE_SOFTNESS: f32 = 0.02;

fn gl_transition(uv: vec2f) -> vec4f {
    let source_color = gl_from_color(uv);
    let target_color = gl_to_color(uv);
    let colour_distance = distance(builtin_straight(source_color), builtin_straight(target_color));
    let mask = builtin_smoothstep(
        colour_distance - COLOUR_DISTANCE_SOFTNESS,
        colour_distance + COLOUR_DISTANCE_SOFTNESS,
        progress(),
    );
    return mix(mix(source_color, target_color, mask), target_color, pow(progress(), params.power));
}
