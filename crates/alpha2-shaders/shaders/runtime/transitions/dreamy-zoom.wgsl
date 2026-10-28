// Port of gl-transitions/transitions/DreamyZoom.glsl
// Author: Zeh Fernando
// License: MIT
// alpha2: `rotation` is in degrees; upstream scaled its value by pi / 80, so the default 13.5
// is upstream's 6. The flash is clamped into premultiplied range.
fn gl_transition(uv: vec2f) -> vec4f {
    let first = progress() < 0.5;
    let phase = select((progress() - 0.5) * 2.0, progress() * 2.0, first);
    let turn = radians(params.rotation);
    let angle_offset = select(mix(-turn, 0.0, phase), mix(0.0, turn, phase), first);
    let zoom = select(mix(params.scale, 1.0, phase), mix(1.0, params.scale, phase), first);
    let p = (uv - vec2f(0.5)) / zoom * vec2f(ratio(), 1.0);
    let angle = atan2(p.y, p.x) + angle_offset;
    let dist = length(p);
    let q = vec2f(cos(angle) * dist / ratio() + 0.5, sin(angle) * dist + 0.5);
    let c = select(gl_to_color(q), gl_from_color(q), first);
    let flash = select(mix(1.0, 0.0, phase), mix(0.0, 1.0, phase), first);
    return builtin_valid(c + vec4f(flash));
}
