// Port of gl-transitions/transitions/StripDatamoshGlitch.glsl
// Author: bread
// License: MIT
// alpha2: PCG hashes of the same integer seeds (frame, row, column, cell) with one salt per
// use replace upstream's two `fract(sin(...))` hashes; the alpha comes from the taps, and
// the added light is scaled by it so the result stays premultiplied.

// A value in [0, 1) for two integer-valued seeds.
fn strip_hash(a: f32, b: f32, salt: u32) -> f32 {
    return builtin_unit(builtin_hash3(builtin_cell(a), builtin_cell(b), salt));
}

fn strip_y(uv: vec2f, density: f32, seed: f32, min_width: f32, max_width: f32) -> f32 {
    let y = uv.y * density + seed * 0.137;
    let id = floor(y);
    let f = fract(y);
    let c = strip_hash(id, seed, 1u);
    let w = mix(min_width, max_width, strip_hash(id, seed, 2u));
    return 1.0 - builtin_smoothstep(w, w + 0.018, abs(f - c));
}

fn strip_x(uv: vec2f, density: f32, seed: f32, min_width: f32, max_width: f32) -> f32 {
    let x = uv.x * density + seed * 0.091;
    let id = floor(x);
    let f = fract(x);
    let c = strip_hash(id, seed, 3u);
    let w = mix(min_width, max_width, strip_hash(id, seed, 4u));
    return 1.0 - builtin_smoothstep(w, w + 0.012, abs(f - c));
}

fn strip_broken_gate(uv: vec2f, row: f32, row_salt: u32, frame: f32) -> f32 {
    let segments = mix(1.0, 9.0, strip_hash(row, frame, 5u));
    let segment = floor(uv.x * segments);
    return step(0.16, strip_hash(segment, row + frame * 3.0, row_salt));
}

fn strip_horizontal_mask(uv: vec2f, frame: f32) -> f32 {
    let bars = params.horizontal_bars;
    let r1 = floor((uv.y + strip_hash(frame, 0.0, 6u) * 0.031) * bars * 0.38);
    let r2 = floor((uv.y + strip_hash(frame, 0.0, 7u) * 0.013) * bars);
    let r3 = floor((uv.y + strip_hash(frame, 0.0, 8u) * 0.006) * bars * 3.4);
    var thick = strip_y(uv, bars * 0.38, frame + 1.0, 0.035, 0.22);
    var mid = strip_y(uv, bars, frame + 4.0, 0.014, 0.11);
    var hair = strip_y(uv, bars * 3.4, frame + 9.0, 0.004, 0.035);
    thick *= step(0.42, strip_hash(r1, frame, 10u));
    mid *= step(0.48, strip_hash(r2, frame, 11u));
    hair *= step(0.62, strip_hash(r3, frame, 12u));
    thick *= strip_broken_gate(uv, r1, builtin_hash3(builtin_cell(r1), builtin_cell(frame), 13u), frame);
    mid *= strip_broken_gate(uv, r2, builtin_hash3(builtin_cell(r2), builtin_cell(frame), 14u), frame + 3.0);
    return clamp(max(thick, max(mid, hair)), 0.0, 1.0);
}

fn strip_vertical_mask(uv: vec2f, frame: f32) -> f32 {
    let column = floor((uv.x + strip_hash(frame, 0.0, 15u) * 0.017) * params.vertical_slits);
    let slit = strip_x(uv, params.vertical_slits, frame + 13.0, 0.01, 0.075);
    return clamp(slit * step(0.66, strip_hash(column, frame, 16u)), 0.0, 1.0);
}

// The channels from three taps across `s`; the alpha covers all three.
fn strip_chroma_from(uv: vec2f, s: vec2f) -> vec4f {
    let p = clamp(uv, vec2f(0.0), vec2f(1.0));
    let red = gl_from_color(clamp(p + s, vec2f(0.0), vec2f(1.0)));
    let green = gl_from_color(p);
    let blue = gl_from_color(clamp(p - s, vec2f(0.0), vec2f(1.0)));
    return vec4f(red.r, green.g, blue.b, max(green.a, max(red.a, blue.a)));
}

fn strip_chroma_to(uv: vec2f, s: vec2f) -> vec4f {
    let p = clamp(uv, vec2f(0.0), vec2f(1.0));
    let red = gl_to_color(clamp(p - s, vec2f(0.0), vec2f(1.0)));
    let green = gl_to_color(p);
    let blue = gl_to_color(clamp(p + s, vec2f(0.0), vec2f(1.0)));
    return vec4f(red.r, green.g, blue.b, max(green.a, max(red.a, blue.a)));
}

fn gl_transition(uv: vec2f) -> vec4f {
    let b = pow(max(0.0, sin(progress() * 3.141592653589793)), 0.42) * params.strength;
    let frame = floor(progress() * 30.0);
    let h = strip_horizontal_mask(uv, frame);
    let v = strip_vertical_mask(uv, frame);
    let glitch = clamp(max(h, v * 0.75), 0.0, 1.0);
    let row = floor(uv.y * params.horizontal_bars);
    let column = floor(uv.x * params.vertical_slits);
    let row_random = strip_hash(row, frame, 17u);
    let band_delay = (row_random - 0.5) * 0.30 * h;
    let reveal = builtin_smoothstep(0.18, 0.84, progress() + band_delay);
    let split = vec2f(params.chroma * b * (1.0 + 1.7 * glitch), params.chroma * 0.22 * b * v);
    // Upstream's `distortUv`, once for both pictures: they tear in opposite directions.
    let tear_random = strip_hash(row, frame, 18u);
    let drag_random = strip_hash(column, frame, 19u);
    let x_tear = (tear_random - 0.5) * 2.0 * params.tear * b * h + sin(uv.y * 120.0 + progress() * 95.0) * 0.006 * b;
    let y_drag = (drag_random - 0.5) * 0.13 * b * v;
    let micro = (strip_hash(row, column + frame, 20u) - 0.5) * 0.018 * b * max(h, v);
    let from_uv = uv + vec2f(x_tear + micro, y_drag);
    let to_uv = uv + vec2f(-x_tear + micro, y_drag);
    var color = mix(strip_chroma_from(from_uv, split), strip_chroma_to(to_uv, split), reveal);
    // Slice residue: old and new pictures dragged through uneven scan bands.
    var smear_uv = uv;
    smear_uv.x += (row_random - 0.5) * 0.46 * b * h;
    smear_uv.y += (strip_hash(row, frame, 21u) - 0.5) * 0.045 * b * h;
    let slice_reveal = builtin_smoothstep(0.28, 0.78, progress() + (row_random - 0.5) * 0.22);
    let slice_color = mix(
        strip_chroma_from(smear_uv, split * 1.65),
        strip_chroma_to(smear_uv - vec2f((row_random - 0.5) * 0.18 * b, 0.0), split * 1.65),
        slice_reveal,
    );
    color = mix(color, slice_color, h * b * params.residue);
    // Thin scan sparks and broken bright lines.
    let hair_line = strip_y(uv, 190.0, frame + 55.0, 0.002, 0.012) * step(0.70, strip_hash(floor(uv.y * 190.0), frame, 22u));
    var rgb = color.rgb + vec3f(0.72, 0.90, 1.0) * (hair_line * b * 0.28 * color.a);
    let scan = 0.5 + 0.5 * sin(uv.y * 980.0 + progress() * 130.0);
    rgb *= 1.0 - params.scan_amount * b * scan;
    let cell = floor(uv * vec2f(360.0 * ratio(), 210.0));
    let n = strip_hash(cell.x + frame * 7.0, cell.y + frame * 13.0, 23u);
    rgb += vec3f((n - 0.5) * params.noise_amount * b * (0.55 + glitch) * color.a);
    // Slight desaturation at the damage peak, like broadcast signal corruption.
    let luma = dot(rgb, vec3f(0.299, 0.587, 0.114));
    rgb = mix(rgb, vec3f(luma), 0.18 * b * glitch);
    let strobe = step(0.78, strip_hash(frame, 0.0, 24u)) * pow(b, 1.65);
    rgb += vec3f(strobe * params.flash_amount * color.a);
    return builtin_valid(vec4f(rgb, color.a));
}
