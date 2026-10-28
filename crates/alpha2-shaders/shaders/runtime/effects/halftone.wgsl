// A grid of dots `cells` across, turned by `angle`, each dot sized by its cell's centre:
// black ink on white paper, or red, green and blue dots on black. The source's alpha stays.
fn effect(uv: vec2f, src: vec4f) -> vec4f {
    let aspect = builtin_aspect();
    let size = aspect.x / params.cells;
    let angle = radians(params.angle);
    let p = builtin_rotate((uv - 0.5) * aspect, angle) / size;
    let cell = floor(p) + 0.5;
    let center_uv = builtin_rotate(cell * size, -angle) / aspect + 0.5;
    let from_center = length(p - cell);
    let width = max(fwidthFine(from_center), 0.0000001);
    let rgb = builtin_straight(sample_src(center_uv));
    let radius = sqrt(1.0 - builtin_gray(rgb)) * 0.7071;
    let ink = 1.0 - clamp(0.5 + (from_center - radius) / width, 0.0, 1.0);
    let radii = sqrt(rgb) * 0.7071;
    let dots = clamp(vec3f(0.5) - (vec3f(from_center) - radii) / width, vec3f(0.0), vec3f(1.0));
    return builtin_output(select(vec3f(1.0 - ink), dots, params.mode == 1u), src.a);
}
