#version 450
// One triangle that covers the whole eye view; the fragment shader casts a ray per pixel.
layout(location = 0) out vec2 ndc;
void main() {
    vec2 p = vec2((gl_VertexIndex << 1) & 2, gl_VertexIndex & 2);
    ndc = p * 2.0 - 1.0;
    gl_Position = vec4(ndc, 0.0, 1.0);
}
