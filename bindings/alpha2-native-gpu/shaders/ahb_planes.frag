#version 450
// Copies one plane of a YCbCr image sampled through an RGB_IDENTITY conversion (R = Cr, G = Y,
// B = Cb). 10-bit samples arrive normalized to 1023 and are stored as P010's code << 6.
layout(constant_id = 0) const bool CHROMA = false;
layout(constant_id = 1) const bool TEN_BIT = false;
layout(set = 0, binding = 0) uniform sampler2D picture;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 plane;
void main() {
    vec4 color = texture(picture, uv);
    vec2 samples = CHROMA ? color.br : vec2(color.g, 0.0);
    plane = vec4(TEN_BIT ? round(samples * 1023.0) * (64.0 / 65535.0) : samples, 0.0, 1.0);
}
