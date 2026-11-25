precision highp float;

uniform float vTime;
uniform float vAlpha;
uniform float vBlobsAlpha;
uniform vec2 vScreenSize;
uniform float vSceneOffsetY;
uniform vec3 vColor[6];

float blob(vec2 p, float r, float t, float seed) {
    float a = atan(p.y, p.x);
    float edge = 1.0
        + 0.14 * sin(a * 3.0 + t * 0.8 + seed)
        + 0.08 * sin(a * 5.0 - t * 1.1 + seed * 2.3);
    float d = length(p) / (r * edge);
    return exp(-d * d * 3.0);
}

void main() {
    vec2 uv = gl_FragCoord.xy / vScreenSize;
    float aspect = vScreenSize.x / vScreenSize.y;
    vec2 p = vec2((uv.x - 0.5) * aspect, uv.y - 0.5 + vSceneOffsetY);
    float t = vTime;

    vec3 col = vec3(0.0);
    for (int i = 0; i < 3; i++) {
        float fi = float(i);
        float phase = fi * 2.094;
        vec2 c = 0.05 * vec2(sin(t * 0.31 + phase), cos(t * 0.23 + phase * 1.3));
        float ang = t * 0.12 + phase;
        mat2 rot = mat2(cos(ang), -sin(ang), sin(ang), cos(ang));
        vec2 q = rot * (p - c) * vec2(1.0, 1.25);
        float b = blob(q, 0.27 - fi * 0.03, t, fi * 1.7);
        vec3 tint = mix(vColor[i * 2 + 1], vColor[i * 2], smoothstep(0.2, 0.9, b));
        col += tint * b * (0.75 - fi * 0.12);
    }
    float glow = smoothstep(0.55, 0.0, uv.y) * 0.12;
    col += mix(vColor[1], vColor[3], uv.x) * glow;

    col = 1.0 - exp(-col * 1.6);
    gl_FragColor = vec4(col * vBlobsAlpha * vAlpha, 1.0);
}
