// Both pictures blurred toward `center`, most at the middle, where one becomes the other.
fn transition(uv: vec2f) -> vec4f {
    let taps = u32(clamp(max_filter_samples(), 1.0, 64.0));
    let reach = params.strength * sin(3.14159265 * progress());
    let blend = builtin_smoothstep(0.35, 0.65, progress());
    let toward = params.center - uv;
    var sum = vec4f(0.0);
    for (var i = 0u; i < taps; i++) {
        let p = uv + toward * ((f32(i) + 0.5) / f32(taps) * reach);
        sum += mix(from_color(p), to_color(p), blend);
    }
    return sum / f32(taps);
}
