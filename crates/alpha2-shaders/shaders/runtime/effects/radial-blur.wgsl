// Taps along the line to `center` (zoom: `amount` of the distance) or along the arc around it
// (spin: `angle` wide), centred on the pixel so the picture does not drift.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let taps = u32(clamp(max_filter_samples(), 1.0, 64.0));
    let aspect = builtin_aspect();
    let offset = (uv - params.center) * aspect;
    let spin = radians(params.angle);
    var sum = vec4f(0.0);
    for (var i = 0u; i < taps; i++) {
        let t = (f32(i) + 0.5) / f32(taps) - 0.5;
        var p = offset * (1.0 + params.amount * t);
        if params.mode == 1u {
            let c = cos(spin * t);
            let s = sin(spin * t);
            p = vec2f(c * offset.x - s * offset.y, s * offset.x + c * offset.y);
        }
        sum += sample_src(params.center + p / aspect);
    }
    return sum / f32(taps);
}
