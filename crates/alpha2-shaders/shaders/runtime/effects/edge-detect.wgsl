// Sobel gradients of the premultiplied color, `width` apart: their luminance, or each
// channel's when colored. Inverted, dark lines on white cover the whole picture.
fn effect(uv: vec2f, src: vec4f) -> vec4f {
    let d = vec2f(params.width) / builtin_aspect();
    let tl = sample_src(uv + vec2f(-d.x, -d.y));
    let tc = sample_src(uv + vec2f(0.0, -d.y));
    let tr = sample_src(uv + vec2f(d.x, -d.y));
    let ml = sample_src(uv + vec2f(-d.x, 0.0));
    let mr = sample_src(uv + vec2f(d.x, 0.0));
    let bl = sample_src(uv + vec2f(-d.x, d.y));
    let bc = sample_src(uv + vec2f(0.0, d.y));
    let br = sample_src(uv + vec2f(d.x, d.y));
    let gx = (tr + 2.0 * mr + br) - (tl + 2.0 * ml + bl);
    let gy = (bl + 2.0 * bc + br) - (tl + 2.0 * tc + tr);
    let magnitude = sqrt(gx.rgb * gx.rgb + gy.rgb * gy.rgb) * params.strength;
    let edge = select(vec3f(builtin_luminance(magnitude)), magnitude, params.colored != 0u);
    let inverted = params.invert != 0u;
    return builtin_output(select(edge, vec3f(1.0) - edge, inverted), select(src.a, 1.0, inverted));
}
