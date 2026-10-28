// Port of gl-transitions/transitions/Drop_Zone_Flicker.glsl
// Author: bread
// License: MIT
// alpha2: PCG hashes of the frame and cell indices with one salt per use replace upstream's
// float hash; the alpha comes from the taps, and the washes, scanlines and exposure stay
// premultiplied. `frame_rate` sets how many steps the 24-step choreography takes.

// A value in [0, 1) for two integer-valued seeds.
fn drop_zone_hash(a: f32, b: f32, salt: u32) -> f32 {
    return builtin_unit(builtin_hash3(builtin_cell(a), builtin_cell(b), salt));
}

fn drop_zone_safe(uv: vec2f) -> vec2f {
    return clamp(uv, vec2f(0.001), vec2f(0.999));
}

// How far each step has revealed the incoming picture. Not monotonic on purpose: it flashes
// back to the outgoing one.
fn drop_zone_reveal(f: f32) -> f32 {
    if f < 0.5 { return 0.00; }
    if f < 1.5 { return 0.28; }
    if f < 2.5 { return 0.43; }
    if f < 3.5 { return 0.46; }
    if f < 4.5 { return 0.38; }
    if f < 5.5 { return 0.48; }
    if f < 6.5 { return 0.26; }
    if f < 7.5 { return 0.10; }
    if f < 8.5 { return 0.00; }
    if f < 9.5 { return 0.00; }
    if f < 10.5 { return 0.30; }
    if f < 11.5 { return 0.58; }
    if f < 12.5 { return 1.00; }
    if f < 13.5 { return 0.70; }
    if f < 14.5 { return 0.42; }
    if f < 15.5 { return 0.55; }
    if f < 16.5 { return 0.72; }
    if f < 17.5 { return 0.88; }
    if f < 18.5 { return 0.34; }
    if f < 19.5 { return 0.48; }
    if f < 20.5 { return 0.56; }
    if f < 21.5 { return 0.76; }
    if f < 22.5 { return 0.93; }
    return 1.00;
}

fn drop_zone_glitch(f: f32) -> f32 {
    if f < 0.5 { return 0.00; }
    if f < 6.5 { return 0.92; }
    if f < 10.5 { return 0.38; }
    if f < 11.5 { return 0.78; }
    if f < 12.5 { return 0.12; }
    if f < 17.5 { return 0.74; }
    if f < 22.5 { return 0.88; }
    if f < 23.5 { return 0.28; }
    return 0.00;
}

// The channels from three taps across `amount`; the alpha covers all three.
fn drop_zone_chroma_from(uv: vec2f, amount: f32) -> vec4f {
    let o = vec2f(amount, 0.0);
    let red = gl_from_color(drop_zone_safe(uv + o));
    let green = gl_from_color(drop_zone_safe(uv));
    let blue = gl_from_color(drop_zone_safe(uv - o));
    return vec4f(red.r, green.g, blue.b, max(green.a, max(red.a, blue.a)));
}

fn drop_zone_chroma_to(uv: vec2f, amount: f32) -> vec4f {
    let o = vec2f(amount, 0.0);
    let red = gl_to_color(drop_zone_safe(uv - o));
    let green = gl_to_color(drop_zone_safe(uv));
    let blue = gl_to_color(drop_zone_safe(uv + o));
    return vec4f(red.r, green.g, blue.b, max(green.a, max(red.a, blue.a)));
}

fn drop_zone_blocks(uv: vec2f, f: f32) -> f32 {
    let big = floor(uv * vec2f(4.0, 2.0));
    let small = floor(uv * vec2f(8.0, 4.0));
    let wide = step(0.48, drop_zone_hash(big.x, f, 1u));
    let chunks = step(0.56, drop_zone_hash(small.x + small.y * 8.0, f, 2u));
    let cut = builtin_smoothstep(-0.035, 0.035, uv.x - mix(0.18, 0.78, drop_zone_hash(f, 0.0, 3u)));
    return clamp(mix(wide, chunks, 0.38) * 0.72 + cut * 0.28, 0.0, 1.0);
}

fn gl_transition(uv: vec2f) -> vec4f {
    let f = floor(progress() * params.frame_rate);
    let reveal = drop_zone_reveal(f);
    let glitch = drop_zone_glitch(f);
    let jitter = vec2f(
        (drop_zone_hash(f, 0.0, 4u) - 0.5) * 0.042,
        (drop_zone_hash(f, 0.0, 5u) - 0.5) * 0.010,
    ) * glitch;
    let from_uv = drop_zone_safe(uv + jitter);
    let to_uv = drop_zone_safe(uv - jitter * 0.55);
    let block = drop_zone_blocks(uv, f);
    var local_reveal = clamp(
        reveal + (block - 0.5) * params.block_amount * glitch
            + (drop_zone_hash(f, floor(uv.y * 9.0), 6u) - 0.5) * 0.18 * glitch,
        0.0,
        1.0,
    );
    local_reveal = builtin_smoothstep(0.22, 0.78, local_reveal);
    let old_clip = drop_zone_chroma_from(from_uv, params.rgb_offset * glitch);
    let new_clip = drop_zone_chroma_to(to_uv, params.rgb_offset * glitch);
    var color = mix(old_clip, new_clip, local_reveal);
    let left_wash = (1.0 - builtin_smoothstep(0.10, 0.78, uv.x)) * glitch;
    let cyan_wash = builtin_smoothstep(0.08, 0.62, uv.x) * (1.0 - builtin_smoothstep(0.86, 1.0, uv.x)) * glitch;
    let red_ghost = vec4f(old_clip.rgb * vec3f(1.34, 0.42, 0.38), old_clip.a);
    let cyan_ghost = vec4f(new_clip.rgb * vec3f(0.48, 1.12, 1.24), new_clip.a);
    color = mix(color, red_ghost, left_wash * params.red_cyan * 0.42);
    color = mix(color, cyan_ghost, cyan_wash * params.red_cyan * 0.30);
    // The outgoing picture returns as a translucent flash, mostly after the first full hit.
    var old_return = glitch * (1.0 - builtin_smoothstep(0.94, 1.0, reveal));
    old_return *= 0.18 + 0.52 * (1.0 - abs(local_reveal - 0.5) * 2.0);
    color = mix(color, old_clip, old_return * params.ghost_amount);
    let lines = sin((uv.y + f * 0.017) * 1080.0);
    let exposure = (drop_zone_hash(f, 0.0, 7u) - 0.35) * 0.10 * glitch;
    let rgb = color.rgb + vec3f((lines * params.scanline * glitch + exposure) * color.a);
    return builtin_valid(vec4f(rgb, color.a));
}
