// An iris wipe through one of six shapes, opening from `center` or closing onto it.
// The star's edges are linear in angle, so its sides bow a little; it reads as a star iris.

// The shape's own distance: its boundary at 1 for size 1, growing outward.
fn shape_metric(p: vec2f) -> f32 {
    let a = max(params.aspect, 0.1);
    if params.shape == 1u { return length(p / vec2f(a, 1.0)); }
    if params.shape == 2u { return abs(p.x) / a + abs(p.y); }
    if params.shape == 3u { return max(abs(p.x) / a, abs(p.y)); }
    if params.shape == 4u {
        // Five points, one up: the radius falls linearly in angle from 1 at a tip to 0.45
        // between two. The atan2 seam points down, between two tips, where this is continuous.
        let turn = atan2(p.x, -p.y) / 6.28318531 * 5.0;
        let tip = abs(fract(turn + 0.5) * 2.0 - 1.0);
        return length(p) / mix(1.0, 0.45, tip);
    }
    if params.shape == 5u {
        return min(max(abs(p.x) * 3.0, abs(p.y)), max(abs(p.x), abs(p.y) * 3.0));
    }
    return length(p);
}

// `uv` in the shape's frame: height units about the center, turned counter-clockwise on screen.
fn shape_point(uv: vec2f) -> vec2f {
    let p = (uv - params.center) * vec2f(resolution().x / resolution().y, 1.0);
    let angle = params.rotation * 3.14159265 / 180.0;
    let c = cos(angle);
    let s = sin(angle);
    return vec2f(c * p.x - s * p.y, s * p.x + c * p.y);
}

// The size at which the shape covers the frame. A convex shape's largest metric over the frame
// is at a corner; the star and the cross are bounded instead: the star's radius is at least
// 0.45 and the cross lies within each of its two bars.
fn shape_reach() -> f32 {
    let c0 = shape_point(vec2f(0.0, 0.0));
    let c1 = shape_point(vec2f(1.0, 0.0));
    let c2 = shape_point(vec2f(0.0, 1.0));
    let c3 = shape_point(vec2f(1.0, 1.0));
    if params.shape == 4u {
        return max(max(length(c0), length(c1)), max(length(c2), length(c3))) / 0.45;
    }
    if params.shape == 5u {
        let a = abs(vec4f(c0.x, c1.x, c2.x, c3.x));
        let b = abs(vec4f(c0.y, c1.y, c2.y, c3.y));
        let across = max(a * 3.0, b);
        let along = max(a, b * 3.0);
        return min(
            max(max(across.x, across.y), max(across.z, across.w)),
            max(max(along.x, along.y), max(along.z, along.w)),
        );
    }
    return max(max(shape_metric(c0), shape_metric(c1)), max(shape_metric(c2), shape_metric(c3)));
}

fn transition(uv: vec2f) -> vec4f {
    let m = shape_metric(shape_point(uv));
    let footprint = fwidthFine(m);
    let closing = params.closing != 0u;
    let amount = select(progress(), 1.0 - progress(), closing);
    let edge = amount * (shape_reach() + params.softness);
    let outside = builtin_aa_ramp(edge - params.softness, edge, m, footprint);
    return mix(from_color(uv), to_color(uv), select(1.0 - outside, outside, closing));
}
