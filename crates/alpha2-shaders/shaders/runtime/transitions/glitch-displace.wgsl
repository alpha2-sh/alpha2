// Port of gl-transitions/transitions/GlitchDisplace.glsl
// Author: Matt DesLauriers
// License: MIT
// alpha2: upstream's unused voronoi and its hash are dropped; the gray flash takes its alpha
// from the taps and its gray from their premultiplied colors, clamped to that alpha, so the
// result stays premultiplied (a straight gray would divide by the alpha at the taps' edges).

fn glitch_displace_offset(tex: vec4f, uv: vec2f, dot_depth: f32, texture_depth: f32, strength: f32) -> vec2f {
    var dis = tex * dot_depth + 1.0 - tex * texture_depth;
    dis.x = dis.x - 1.0 + texture_depth * dot_depth;
    dis.y = dis.y - 1.0 + texture_depth * dot_depth;
    return uv + vec2f(dis.x, dis.y) * strength;
}

fn glitch_displace_ease1(t: f32) -> f32 {
    if t == 0.0 || t == 1.0 { return t; }
    if t < 0.5 { return 0.5 * exp2(20.0 * t - 10.0); }
    return -0.5 * exp2(10.0 - t * 20.0) + 1.0;
}

fn glitch_displace_ease2(t: f32) -> f32 {
    if t == 1.0 { return t; }
    return 1.0 - exp2(-10.0 * t);
}

fn gl_transition(uv: vec2f) -> vec4f {
    var color1 = gl_from_color(uv);
    var color2 = gl_to_color(uv);
    let val = glitch_displace_ease1(progress());
    let disp = glitch_displace_offset(color1, uv, 0.33, 0.7, 1.0 - val);
    let disp2 = glitch_displace_offset(color2, uv, 0.33, 0.5, glitch_displace_ease2(progress()));
    let displaced1 = gl_to_color(disp);
    let displaced2 = gl_from_color(disp2);
    let gray = dot(min(displaced2, displaced1).rgb, vec3f(0.299, 0.587, 0.114));
    let alpha = max(displaced1.a, displaced2.a);
    let flash = vec4f(vec3f(min(gray * 2.0, alpha)), alpha);
    color1 = mix(color1, flash, builtin_smoothstep(0.0, 0.5, progress()));
    color2 = mix(color2, displaced1, builtin_smoothstep(1.0, 0.5, progress()));
    return builtin_valid(mix(color1, color2, val));
}
