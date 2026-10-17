#version 450
// Maps each pixel's view ray onto the video: an equirectangular sphere (360),
// YouTube's equi-angular cubemap (360 cube), a hemisphere (180), or a flat
// screen in front of the viewer.
layout(location = 0) in vec2 ndc;
layout(location = 0) out vec4 color;
// Two decoded frames as NV12, each full-size luma and half-size chroma
// (Cb, Cr): the frame on screen and the next one, for blending.
layout(set = 0, binding = 0) uniform sampler2D luma0;
layout(set = 0, binding = 1) uniform sampler2D chroma0;
layout(set = 0, binding = 2) uniform sampler2D luma1;
layout(set = 0, binding = 3) uniform sampler2D chroma1;

layout(push_constant) uniform Eye {
    vec4 rot0;   // rows of the eye's rotation (view -> world)
    vec4 rot1;
    vec4 rot2;
    vec4 tans;   // tan(left), tan(right), tan(up), tan(down)
    vec4 pos;    // eye position in world space
    vec4 rect;   // this eye's part of the video: x, y, w, h in 0..1
    vec4 params; // mode (0 = 360, 1 = 180, 2 = flat, 3 = 360 cube), screen width, screen height, screen distance
    vec4 color;  // x: 1 = BT.601 matrix (else BT.709), y: 1 = full range (else limited),
                 // z: weight of the other frame, w: set holding the frame on screen
} eye;

const float PI = 3.14159265358979;
const vec4 BACKGROUND = vec4(0.0, 0.0, 0.0, 1.0);

// Equi-angular cubemap as YouTube stores it: faces left, front, right on the
// top row and down, back, up (rotated) on the bottom row, each with a 2 pixel
// border. Mirrors ffmpeg's v360 filter (xyz_to_eac), whose axes are x right,
// y down, z forward.
vec2 eac_uv(vec3 d, vec2 size) {
    vec3 f = vec3(d.x, -d.y, -d.z);
    float phi = atan(f.x, f.z);
    float theta = asin(clamp(f.y, -1.0, 1.0));
    const float Q = PI / 4.0;
    // 0 right, 1 left, 2 up, 3 down, 4 front, 5 back
    int face;
    float phi_norm;
    if (phi >= -Q && phi < Q) { face = 4; phi_norm = phi; }
    else if (phi >= -3.0 * Q && phi < -Q) { face = 1; phi_norm = phi + 2.0 * Q; }
    else if (phi >= Q && phi < 3.0 * Q) { face = 0; phi_norm = phi - 2.0 * Q; }
    else { face = 5; phi_norm = phi + (phi > 0.0 ? -PI : PI); }
    float limit = atan(cos(phi_norm));
    if (theta > limit) face = 3;
    else if (theta < -limit) face = 2;

    vec2 uv;
    vec2 cell;
    if (face == 0)      { uv = vec2(-f.z / f.x,  f.y / f.x); cell = vec2(2.0, 0.0); }
    else if (face == 1) { uv = vec2(-f.z / f.x, -f.y / f.x); cell = vec2(0.0, 0.0); }
    else if (face == 2) { uv = vec2(-f.x / f.y, -f.z / f.y); uv = vec2(uv.y, -uv.x); cell = vec2(2.0, 1.0); }
    else if (face == 3) { uv = vec2( f.x / f.y, -f.z / f.y); uv = vec2(uv.y, -uv.x); cell = vec2(0.0, 1.0); }
    else if (face == 4) { uv = vec2( f.x / f.z,  f.y / f.z); cell = vec2(1.0, 0.0); }
    else                { uv = vec2( f.x / f.z, -f.y / f.z); uv = vec2(-uv.y, uv.x); cell = vec2(1.0, 1.0); }

    uv = (2.0 / PI) * atan(uv) + 0.5;
    vec2 pad = 2.0 / size;
    return vec2((uv.x + cell.x) * (1.0 - 2.0 * pad.x) / 3.0 + pad.x,
                uv.y * (0.5 - 2.0 * pad.y) + pad.y + 0.5 * cell.y);
}

// One frame's colour at `st`, as linear light for the sRGB swapchain.
vec3 frame_rgb(int set, vec2 st) {
    float y = set == 0 ? texture(luma0, st).r : texture(luma1, st).r;
    vec2 c = (set == 0 ? texture(chroma0, st).rg : texture(chroma1, st).rg) - 128.0 / 255.0;
    if (eye.color.y < 0.5) {
        y = (y - 16.0 / 255.0) * (255.0 / 219.0);
        c *= 255.0 / 224.0;
    }
    vec3 rgb = eye.color.x > 0.5
        ? vec3(y + 1.402 * c.y, y - 0.344136 * c.x - 0.714136 * c.y, y + 1.772 * c.x)
        : vec3(y + 1.5748 * c.y, y - 0.187324 * c.x - 0.468124 * c.y, y + 1.8556 * c.x);
    rgb = clamp(rgb, 0.0, 1.0);
    return mix(rgb / 12.92, pow((rgb + 0.055) / 1.055, vec3(2.4)), step(0.04045, rgb));
}

vec4 sample_video(vec2 st) {
    int shown = int(eye.color.w + 0.5);
    vec3 rgb = frame_rgb(shown, st);
    if (eye.color.z > 0.0) {
        rgb = mix(rgb, frame_rgb(1 - shown, st), eye.color.z);
    }
    return vec4(rgb, 1.0);
}

void main() {
    vec2 t = ndc * 0.5 + 0.5; // Vulkan: t.y = 0 at the top
    vec3 v = vec3(mix(eye.tans.x, eye.tans.y, t.x), mix(eye.tans.z, eye.tans.w, t.y), -1.0);
    vec3 d = normalize(vec3(dot(eye.rot0.xyz, v), dot(eye.rot1.xyz, v), dot(eye.rot2.xyz, v)));

    int mode = int(eye.params.x + 0.5);
    vec2 uv;
    if (mode == 3) {
        uv = eac_uv(d, vec2(textureSize(luma0, 0)) * eye.rect.zw);
    } else if (mode == 2) {
        float dist = eye.params.w;
        if (d.z > -1e-4) { color = BACKGROUND; return; }
        vec3 hit = eye.pos.xyz + d * ((-dist - eye.pos.z) / d.z);
        uv = vec2(hit.x / eye.params.y + 0.5, 0.5 - hit.y / eye.params.z);
        if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) { color = BACKGROUND; return; }
    } else {
        float lon = atan(d.x, -d.z);            // 0 straight ahead, positive to the right
        float lat = asin(clamp(d.y, -1.0, 1.0));
        float span = mode == 0 ? 2.0 * PI : PI;
        uv = vec2(lon / span + 0.5, 0.5 - lat / PI);
        if (mode == 1 && (uv.x < 0.0 || uv.x > 1.0)) { color = BACKGROUND; return; }
        uv.x = fract(uv.x);
    }
    color = sample_video(eye.rect.xy + uv * eye.rect.zw);
}
