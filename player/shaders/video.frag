#version 450
// Maps each pixel's view ray onto the video: an equirectangular sphere (360),
// YouTube's equi-angular cubemap (360 cube), a hemisphere (180), or a flat
// screen in front of the viewer.
layout(location = 0) in vec2 ndc;
layout(location = 0) out vec4 color;
// The frame on screen and the next one (for blending), already in colour.
layout(set = 0, binding = 0) uniform sampler2D shown;
layout(set = 0, binding = 1) uniform sampler2D other;

layout(push_constant) uniform Eye {
    vec4 rot0;   // rows of the eye's rotation (view -> world)
    vec4 rot1;
    vec4 rot2;
    vec4 tans;   // tan(left), tan(right), tan(up), tan(down)
    vec4 pos;    // eye position in world space
    vec4 rect;   // this eye's part of the video: x, y, w, h in 0..1
    vec4 params; // mode (0 = 360, 1 = 180, 2 = flat, 3 = 360 cube), screen width, screen height, screen distance
    vec4 color;  // x: weight of the other frame
} eye;

const float PI = 3.14159265358979;
const vec4 BACKGROUND = vec4(0.0, 0.0, 0.0, 1.0);

// Equi-angular cubemap as YouTube stores it: faces left, front, right on the
// top row and down, back, up (rotated) on the bottom row, each with a 2 pixel
// border. Mirrors ffmpeg's v360 filter (xyz_to_eac), whose axes are x right,
// y down, z forward.
vec2 eac_uv(vec3 d, vec2 size) {
    vec3 f = vec3(d.x, -d.y, -d.z);
    vec3 a = abs(f);
    // The face is the one the ray's largest component points at.
    vec2 uv;
    vec2 cell;
    if (a.y >= a.x && a.y >= a.z) {
        if (f.y > 0.0) { uv = vec2( f.x / f.y, -f.z / f.y); cell = vec2(0.0, 1.0); } // down
        else           { uv = vec2(-f.x / f.y, -f.z / f.y); cell = vec2(2.0, 1.0); } // up
        uv = vec2(uv.y, -uv.x);
    } else if (a.x >= a.z) {
        if (f.x > 0.0) { uv = vec2(-f.z / f.x,  f.y / f.x); cell = vec2(2.0, 0.0); } // right
        else           { uv = vec2(-f.z / f.x, -f.y / f.x); cell = vec2(0.0, 0.0); } // left
    } else if (f.z > 0.0) {
        uv = vec2(f.x / f.z, f.y / f.z); cell = vec2(1.0, 0.0); // front
    } else {
        uv = vec2(f.x / f.z, -f.y / f.z); uv = vec2(-uv.y, uv.x); cell = vec2(1.0, 1.0); // back
    }

    uv = (2.0 / PI) * atan(uv) + 0.5;
    vec2 pad = 2.0 / size;
    return vec2((uv.x + cell.x) * (1.0 - 2.0 * pad.x) / 3.0 + pad.x,
                uv.y * (0.5 - 2.0 * pad.y) + pad.y + 0.5 * cell.y);
}

// Colour at `st` (linear light: the pictures are sRGB textures).
vec4 sample_video(vec2 st) {
    vec3 rgb = texture(shown, st).rgb;
    if (eye.color.x > 0.0) {
        rgb = mix(rgb, texture(other, st).rgb, eye.color.x);
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
        uv = eac_uv(d, vec2(textureSize(shown, 0)) * eye.rect.zw);
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
