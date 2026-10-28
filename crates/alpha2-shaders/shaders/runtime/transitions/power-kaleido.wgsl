// Port of gl-transitions/transitions/powerKaleido.glsl
// Author: Boundless
// License: MIT

fn rotate_about(p: vec2f, origin: vec2f, angle: f32) -> vec2f {
    let s = sin(angle);
    let c = cos(angle);
    let d = p - origin;
    return origin + vec2f(c * d.x + s * d.y, -s * d.x + c * d.y);
}

fn gl_transition(uv: vec2f) -> vec4f {
    let dist = params.scale / 10.0;
    var p = uv - 0.5;
    p.x *= ratio();
    p *= params.z;
    p = rotate_about(p, vec2f(0.0), progress() * params.speed);
    // alpha2: the three mirrors at 0, 120 and 240 degrees as constants (cos, sin, tan).
    // Upstream's float loop to 2 pi took a fourth step or not depending on rounding.
    var mirrors = array<vec3f, 3>(
        vec3f(1.0, 0.0, 0.0),
        vec3f(-0.5, 0.8660254, -1.7320508),
        vec3f(-0.5, -0.8660254, 1.7320508),
    );
    for (var iteration = 0; iteration < 10; iteration++) {
        for (var k = 0; k < 3; k++) {
            let mirror = mirrors[k];
            let lhs = p.y - dist * mirror.x;
            let rhs = mirror.z * (p.x + dist * mirror.y);
            if ((mirror.x > 0.0 && lhs > rhs) || (mirror.x <= 0.0 && lhs < rhs)) {
                // Reflected across the mirror's line through the origin.
                let q = vec2f(p.x + mirror.y * dist * 2.0, p.y - mirror.x * dist * 2.0);
                let n = mirror.xy;
                p = 2.0 * n * dot(q, n) - q;
            }
        }
    }
    p += 0.5;
    p = rotate_about(p, vec2f(0.5), progress() * -params.speed);
    p -= 0.5;
    p.x /= ratio();
    p += 0.5;
    p = 2.0 * abs(p / 2.0 - floor(p / 2.0 + 0.5));
    let uv_mix = mix(p, uv, cos(progress() * 6.2831855) / 2.0 + 0.5);
    return mix(gl_from_color(uv_mix), gl_to_color(uv_mix), cos((progress() - 1.0) * 3.1415927) / 2.0 + 0.5);
}
