// Port of gl-transitions/transitions/DoomScreenTransition.glsl
// Author: Zeh Fernando
// License: MIT
// alpha2: bars is rounded to a whole count, bar speeds come from a PCG hash, and the falling
// edge is antialiased (bar borders stay hard).
fn melt_wave(bar: f32, bars: f32) -> f32 {
    let x = bar * params.frequency * 0.1 * bars;
    return cos(x * 0.5) * cos(x * 0.13) * sin((x + 10.0) * 0.3) / 2.0 + 0.5;
}

fn gl_transition(uv: vec2f) -> vec4f {
    // The edge is horizontal within a bar, so only uv.y's footprint counts.
    let width = max(fwidthFine(uv.y), 0.0000001);
    let bars = max(round(params.bars), 2.0);
    let bar = floor(uv.x * bars);
    let jitter = builtin_unit(builtin_hash(u32(max(bar, 0.0))));
    let drip = sin(bar / (bars - 1.0) * 3.141592) * params.drip_scale;
    let scale = 1.0 + (mix(melt_wave(bar, bars), jitter, params.noise) + drip) * params.amplitude;
    let phase = progress() * scale;
    // Upstream's phase + uv.y < 1 shows the outgoing picture, moved down by phase.
    let landed = clamp(0.5 + (phase + uv.y - 1.0) / width, 0.0, 1.0);
    return mix(gl_from_color(vec2f(uv.x, uv.y + phase)), gl_to_color(uv), landed);
}
