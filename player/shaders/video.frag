#version 450
// Maps each pixel's view ray onto the video: an equirectangular sphere (360),
// a hemisphere (180), or a flat screen in front of the viewer.
layout(location = 0) in vec2 ndc;
layout(location = 0) out vec4 color;
layout(set = 0, binding = 0) uniform sampler2D video;

layout(push_constant) uniform Eye {
    vec4 rot0;   // rows of the eye's rotation (view -> world)
    vec4 rot1;
    vec4 rot2;
    vec4 tans;   // tan(left), tan(right), tan(up), tan(down)
    vec4 pos;    // eye position in world space
    vec4 rect;   // this eye's part of the video: x, y, w, h in 0..1
    vec4 params; // mode (0 = 360, 1 = 180, 2 = flat), screen width, screen height, screen distance
} eye;

const float PI = 3.14159265358979;
const vec4 BACKGROUND = vec4(0.0, 0.0, 0.0, 1.0);

void main() {
    vec2 t = ndc * 0.5 + 0.5; // Vulkan: t.y = 0 at the top
    vec3 v = vec3(mix(eye.tans.x, eye.tans.y, t.x), mix(eye.tans.z, eye.tans.w, t.y), -1.0);
    vec3 d = normalize(vec3(dot(eye.rot0.xyz, v), dot(eye.rot1.xyz, v), dot(eye.rot2.xyz, v)));

    int mode = int(eye.params.x + 0.5);
    vec2 uv;
    if (mode == 2) {
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
    color = texture(video, eye.rect.xy + uv * eye.rect.zw);
}
