// Drop shadow's last pass: the source (`from`) over its softened shadow (`to`), or the shadow
// alone. Progress is ignored.
fn transition(uv: vec2f) -> vec4f {
    let src = from_color(uv);
    let shadow = to_color(uv);
    if params.shadow_only != 0u { return shadow; }
    return src + shadow * (1.0 - src.a);
}
