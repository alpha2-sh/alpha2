#version 450
// One triangle over the target; uv runs 0 to 1 across it, so fragments sit at texel centers.
layout(location = 0) out vec2 uv;
void main() {
    vec2 corner = vec2(float((gl_VertexIndex << 1) & 2), float(gl_VertexIndex & 2));
    uv = corner;
    gl_Position = vec4(corner * 2.0 - 1.0, 0.0, 1.0);
}
