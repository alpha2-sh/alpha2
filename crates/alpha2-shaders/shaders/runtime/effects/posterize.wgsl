// Each encoded channel rounded to `levels` evenly spaced values.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    let steps = max(round(params.levels), 2.0) - 1.0;
    let rgb = floor(builtin_straight(src) * steps + vec3f(0.5)) / steps;
    return builtin_output(rgb, src.a);
}
