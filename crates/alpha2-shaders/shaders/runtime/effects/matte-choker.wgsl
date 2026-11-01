// Shrinks (choke > 0) or spreads (choke < 0) the matte using the min or max coverage of a tap
// ring `radius` out, alternate taps at half radius to catch thin structures.
fn effect(uv: vec2f, src: vec4f) -> vec4f {
    let taps = u32(clamp(max_filter_samples(), 8.0, 64.0));
    let reach = params.radius / builtin_aspect();
    var low = src.a;
    var high = src.a;
    var spread = vec4f(0.0);
    for (var i = 0u; i < taps; i++) {
        let angle = f32(i) * (BUILTIN_TAU / f32(taps));
        let ring = select(1.0, 0.5, (i & 1u) == 1u);
        let s = sample_src(uv + vec2f(cos(angle), sin(angle)) * reach * ring);
        low = min(low, s.a);
        high = max(high, s.a);
        // Each tap by how much more it covers than the pixel.
        spread += s * max(s.a - src.a, 0.0);
    }
    // Erosion keeps the pixel's color; dilation weights gained coverage by the tap colors, so
    // equal taps blend smoothly.
    let eroded = src * (low / max(src.a, 0.0001));
    let color = clamp(spread.rgb / max(spread.a, 1.0e-12), vec3f(0.0), vec3f(1.0));
    let dilated = vec4f(src.rgb + color * (high - src.a), high);
    let picked = select(dilated, eroded, params.choke >= 0.0);
    return mix(src, picked, abs(params.choke));
}
