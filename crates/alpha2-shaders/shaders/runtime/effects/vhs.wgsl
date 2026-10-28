// Worn tape: rows wobble sideways with slow noise, a tracking band rolls down the frame and
// tears the rows it crosses, chroma bleeds to the right and grain flickers. The bleed and the
// grain are in output pixels on purpose, as a tape's horizontal bandwidth is.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let seed = builtin_seed(params.seed);
    let t = time() * params.speed;
    let wobble = (builtin_value_noise(vec2f(uv.y * 40.0, t * 3.0), seed) - 0.5) * 0.004 * params.amount;
    let band_center = fract(t * 0.1);
    let band = 1.0 - builtin_smoothstep(0.0, 0.04, abs(uv.y - band_center));
    let tear = (builtin_value_noise(vec2f(uv.y * 400.0, t * 30.0), seed + 1u) - 0.5) * 0.03 * band * params.amount;
    let p = vec2f(uv.x + wobble + tear, uv.y);
    // Luma from the pixel, chroma from a smear to its left.
    let texel = 1.0 / resolution().x;
    let c0 = sample_src(p);
    let smear = (sample_src(p - vec2f(texel * 2.0, 0.0)) + sample_src(p - vec2f(texel * 4.0, 0.0))
        + sample_src(p - vec2f(texel * 6.0, 0.0))) / 3.0;
    let rgb0 = builtin_straight(c0);
    let rgb1 = builtin_straight(smear);
    let luma = builtin_luminance(rgb0);
    let chroma = mix(rgb0 - vec3f(luma), rgb1 - vec3f(builtin_luminance(rgb1)), params.bleed);
    let pixel = vec2u(max(floor(uv * resolution()), vec2f(0.0)));
    let frame = builtin_hash(u32(max(floor(t * 30.0), 0.0))) ^ seed;
    let grain = builtin_unit(builtin_hash3(pixel.x, pixel.y, frame)) - 0.5;
    let rgb = vec3f(luma) + chroma + vec3f(grain * params.noise + band * 0.15 * params.amount);
    return builtin_output(rgb, c0.a);
}
