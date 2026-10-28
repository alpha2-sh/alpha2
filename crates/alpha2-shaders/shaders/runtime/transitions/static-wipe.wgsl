// Port of gl-transitions/transitions/static_wipe.glsl
// Author: Ben Lucas
// License: MIT
// alpha2: the static is a PCG hash of the pixel and the progress instead of upstream's
// fract(sin) rnd; pow(x, 0.5) is sqrt; the wipe's edge is antialiased.
fn gl_transition(uv: vec2f) -> vec4f {
    let edge = select(uv.y, 1.0 - uv.y, params.up_to_down != 0u);
    let mix_ratio = 1.0 - builtin_aa_step(progress(), edge);
    let span = params.max_static_span * sqrt(max(sin(3.14159265359 * progress()), 0.0));
    let wiped = mix(gl_from_color(uv), gl_to_color(uv), mix_ratio);
    let envelope = builtin_smoothstep(progress() - span, progress(), edge)
        * (1.0 - builtin_smoothstep(progress(), progress() + span, edge));
    let pixel = uv * resolution();
    let static_value = builtin_unit(
        builtin_hash3(builtin_cell(pixel.x), builtin_cell(pixel.y), bitcast<u32>(progress())),
    );
    return mix(wiped, vec4f(vec3f(static_value), 1.0), envelope);
}
