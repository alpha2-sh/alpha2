// A curved screen with scanlines, an aperture mask and darkened corners. The mask's stripes
// are one output pixel wide on purpose, so a preview drawn at another size shows them at
// another density; the scanlines are capped at one per two output rows so they cannot alias.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let centered = uv * 2.0 - 1.0;
    let bent = centered * (1.0 + params.curvature * dot(centered, centered) * 0.25);
    let p = bent * 0.5 + 0.5;
    let width = fwidthFine(p);
    let picture = builtin_sample_edge(p, width, 0u);
    let lines = min(params.line_count, resolution().y * 0.5);
    let scan = 1.0 - params.scanlines * (0.5 - 0.5 * cos(p.y * lines * BUILTIN_TAU));
    let column = u32(floor(uv.x * resolution().x)) % 3u;
    let dim = 1.0 - params.mask;
    let phosphor = vec3f(select(dim, 1.0, column == 0u), select(dim, 1.0, column == 1u), select(dim, 1.0, column == 2u));
    let shade = 1.0 - params.vignette * dot(centered, centered) * 0.5;
    let rgb = builtin_straight(picture) * scan * phosphor * shade;
    return builtin_output(rgb, picture.a);
}
