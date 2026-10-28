// Port of gl-transitions/transitions/fragment.glsl
// Author: lbl
// License: MIT
// alpha2: the shards' sites and velocities are a table precomputed from PCG hashes
// (builtin_hash3(shard, k, 0)) instead of upstream's fract(sin) random, so the loops call no
// function; the nearest site compares squared distances.
fn gl_transition(uv: vec2f) -> vec4f {
    var sites = array<vec2f, 10>(
        vec2f(0.49947667, 0.79509515), vec2f(0.78598905, 0.8087231),
        vec2f(0.7057787, 0.054352045), vec2f(0.15288764, 0.93668187),
        vec2f(0.144001, 0.630305), vec2f(0.7376622, 0.4944077),
        vec2f(0.96819806, 0.06137538), vec2f(0.052491367, 0.2105034),
        vec2f(0.84342873, 0.49684906), vec2f(0.5713028, 0.6560287),
    );
    // Upstream's normalize(direction) * speed.
    var velocities = array<vec2f, 10>(
        vec2f(0.16108474, 0.18276972), vec2f(0.008172598, 0.19987547),
        vec2f(0.27351242, 0.006472239), vec2f(0.19284527, 0.22853737),
        vec2f(0.18874688, 0.09974405), vec2f(0.091339566, 0.20896637),
        vec2f(0.0029833287, 0.26016384), vec2f(0.26136217, 0.11754496),
        vec2f(0.18530324, 0.21598965), vec2f(0.15319453, 0.20363313),
    );
    let duration = 8.0;
    let travel = clamp(progress() * duration - 0.5, 0.0, duration);
    var color = gl_to_color(uv);
    for (var i = 0u; i < 10u; i++) {
        // Shard `i` is site i's Voronoi cell of the outgoing picture, moved along its velocity.
        let shifted = uv - velocities[i] * travel;
        if shifted.x < 0.0 || shifted.x > 1.0 || shifted.y < 0.0 || shifted.y > 1.0 {
            continue;
        }
        let own = shifted - sites[i];
        let own_distance = dot(own, own);
        var closest = true;
        for (var j = 0u; j < 10u; j++) {
            let other = shifted - sites[j];
            if dot(other, other) < own_distance {
                closest = false;
                break;
            }
        }
        if closest {
            color = from_color(vec2f(shifted.x, 1.0 - shifted.y));
            break;
        }
    }
    return color;
}
