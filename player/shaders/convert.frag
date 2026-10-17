#version 450
// Turns one decoded NV12 frame (full-size luma, half-size chroma) into an
// sRGB picture. Runs once per video frame; the eye views then only sample it.
layout(location = 0) in vec2 ndc;
layout(location = 0) out vec4 color;
layout(set = 0, binding = 0) uniform sampler2D luma;
layout(set = 0, binding = 1) uniform sampler2D chroma;

layout(push_constant) uniform Params {
    vec4 params; // x: 1 = BT.601 matrix (else BT.709), y: 1 = full range (else limited)
} p;

void main() {
    vec2 st = ndc * 0.5 + 0.5;
    float y = texture(luma, st).r;
    vec2 c = texture(chroma, st).rg - 128.0 / 255.0;
    if (p.params.y < 0.5) {
        y = (y - 16.0 / 255.0) * (255.0 / 219.0);
        c *= 255.0 / 224.0;
    }
    vec3 rgb = p.params.x > 0.5
        ? vec3(y + 1.402 * c.y, y - 0.344136 * c.x - 0.714136 * c.y, y + 1.772 * c.x)
        : vec3(y + 1.5748 * c.y, y - 0.187324 * c.x - 0.468124 * c.y, y + 1.8556 * c.x);
    rgb = clamp(rgb, 0.0, 1.0);
    // Video values are already gamma encoded; the sRGB target expects linear
    // light and encodes it again, so this stores them unchanged.
    color = vec4(mix(rgb / 12.92, pow((rgb + 0.055) / 1.055, vec3(2.4)), step(0.04045, rgb)), 1.0);
}
