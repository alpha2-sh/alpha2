// Port of gl-transitions/transitions/doorway.glsl
// Author: gre
// License: MIT
// alpha2: the opaque black floor is `back_color`, the reflection is composited over it
// premultiplied, and the door and frame edges are antialiased.

fn doorway_project(p: vec2f) -> vec2f {
    return p * vec2f(1.0, -1.2) + vec2f(0.0, -0.02);
}

fn gl_transition(p: vec2f) -> vec4f {
    let middle_slit = 2.0 * abs(p.x - 0.5) - progress();
    // Upstream sets the door's point only where the slit is open; both doors are
    // computed everywhere here so their derivatives are.
    var door = p + select(1.0, -1.0, p.x > 0.5) * vec2f(0.5 * progress(), 0.0);
    let d = 1.0 / (1.0 + params.perspective * progress() * (1.0 - middle_slit));
    door.y = (door.y - d / 2.0) * d + d / 2.0;
    let size = mix(1.0, params.depth, 1.0 - progress());
    let next = (p + vec2f(-0.5, -0.5)) * vec2f(size, size) + vec2f(0.5, 0.5);
    let floor_point = doorway_project(next);
    let door_w = fwidthFine(door);
    let next_w = fwidthFine(next);
    let door_in = builtin_aa_step(0.0, middle_slit) * builtin_interval_coverage(door.x, door_w.x)
        * builtin_interval_coverage(door.y, door_w.y);
    let next_in = builtin_interval_coverage(next.x, next_w.x) * builtin_interval_coverage(next.y, next_w.y);
    let floor_in = builtin_interval_coverage(floor_point.x, next_w.x)
        * builtin_interval_coverage(floor_point.y, next_w.y * 1.2);
    let reflected = gl_to_color(floor_point) * (params.reflection * (1.0 - floor_point.y) * floor_in);
    let floor_color = builtin_over(builtin_valid(reflected), builtin_premultiply(params.back_color));
    let doors = mix(mix(floor_color, gl_to_color(next), next_in), gl_from_color(door), door_in);
    return builtin_valid(doors);
}
