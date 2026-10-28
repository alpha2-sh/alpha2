// Port of gl-transitions/transitions/Swirl.glsl
// Author: Sergey Kosarevsky
// License: MIT
// Ported by gre from https://gist.github.com/corporateshark/cacfedb8cca0f5ce3f7c
fn gl_transition(uv: vec2f) -> vec4f {
    let t = progress();
    var p = uv - 0.5;
    let dist = length(p);
    // A radius of 1 covers the picture; the swirl fades out to it.
    if (dist < 1.0) {
        let percent = 1.0 - dist;
        let amount = select(1.0 - (t - 0.5) / 0.5, t / 0.5, t <= 0.5);
        let theta = percent * percent * amount * 8.0 * 3.14159;
        let s = sin(theta);
        let c = cos(theta);
        p = vec2f(dot(p, vec2f(c, -s)), dot(p, vec2f(s, c)));
    }
    p += 0.5;
    return mix(gl_from_color(p), gl_to_color(p), t);
}
