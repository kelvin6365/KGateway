// GLSL sources for the ambient silk background, theme-aware via u_light:
//   u_light = 0.0 — "Midnight Baroque": luminous blue folds on deep navy ink
//   u_light = 1.0 — "Porcelain Baroque": Delft-blue silk washed over ivory
// Written as GLSL ES 1.00 so the same strings compile under WebGL1 and WebGL2.
// Silk/ray tinting uses mix() toward the silk color (not additive blending) so
// folds lighten the dark theme and gently deepen the light one.

export const vertexSrc = /* glsl */ `
attribute vec2 a_position;
void main() {
  gl_Position = vec4(a_position, 0.0, 1.0);
}
`;

export const fragmentSrc = /* glsl */ `
precision highp float;

uniform float u_time;
uniform vec2 u_resolution;
uniform float u_light; // 0 = midnight, 1 = porcelain

// -- value noise ------------------------------------------------------------
float hash(vec2 p) {
  return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453123);
}

float noise(vec2 p) {
  vec2 i = floor(p);
  vec2 f = fract(p);
  vec2 u = f * f * (3.0 - 2.0 * f);
  return mix(
    mix(hash(i), hash(i + vec2(1.0, 0.0)), u.x),
    mix(hash(i + vec2(0.0, 1.0)), hash(i + vec2(1.0, 1.0)), u.x),
    u.y
  );
}

float fbm(vec2 p) {
  float v = 0.0;
  float a = 0.5;
  for (int i = 0; i < 5; i++) {
    v += a * noise(p);
    p = p * 2.03 + vec2(17.0, -9.0);
    a *= 0.5;
  }
  return v;
}

void main() {
  vec2 uv = gl_FragCoord.xy / u_resolution.xy;
  vec2 p = uv;
  p.x *= u_resolution.x / u_resolution.y;

  float t = u_time * 0.03;

  // Midnight palette (matches the .dark CSS tokens)
  vec3 inkDeep  = vec3(0.024, 0.039, 0.086);
  vec3 ink      = vec3(0.039, 0.063, 0.125); // #0a1020
  vec3 blueDim  = vec3(0.239, 0.494, 0.753); // #3d7fc0
  vec3 blueLume = vec3(0.561, 0.816, 1.0);   // #8fd0ff

  // Porcelain palette (matches the :root CSS tokens)
  vec3 porDeep   = vec3(0.898, 0.922, 0.949);
  vec3 por       = vec3(0.965, 0.973, 0.984); // #f6f8fb
  vec3 delftSoft = vec3(0.545, 0.710, 0.867); // ~#8bb5dd
  vec3 delft     = vec3(0.184, 0.435, 0.710); // #2f6fb5

  // Theme blend
  vec3 base0 = mix(inkDeep, porDeep, u_light);
  vec3 base1 = mix(ink, por, u_light);
  vec3 silkSoft = mix(blueDim, delftSoft, u_light);
  vec3 silkVivid = mix(blueLume, delft, u_light);

  // Base: vertical gradient
  vec3 col = mix(base0, base1, smoothstep(0.0, 1.0, uv.y));

  // Flowing silk: two-stage domain-warped fbm
  vec2 q = vec2(fbm(p * 1.4 + t), fbm(p * 1.4 - t * 0.7 + vec2(5.2, 1.3)));
  vec2 r = vec2(
    fbm(p * 1.4 + 2.2 * q + vec2(1.7, 9.2) + t * 0.6),
    fbm(p * 1.4 + 2.2 * q + vec2(8.3, 2.8) - t * 0.4)
  );
  float silk = fbm(p * 1.4 + 2.4 * r);

  // Sharpen the fold highlights so it reads as draped fabric, not fog.
  // Porcelain wants a fainter wash than midnight.
  float strength = mix(1.0, 0.55, u_light);
  float folds = smoothstep(0.42, 0.72, silk);
  col = mix(col, silkSoft, folds * 0.10 * strength);
  col = mix(col, silkVivid, smoothstep(0.62, 0.86, silk) * 0.08 * strength);

  // Diagonal light rays falling from the upper left
  vec2 rp = mat2(0.866, -0.5, 0.5, 0.866) * p; // rotate ~30deg
  float ray = sin(rp.x * 9.0 - t * 2.0) * 0.5 + 0.5;
  ray = pow(ray, 4.0);
  ray *= noise(vec2(rp.x * 3.0, t * 0.5)) * 0.8 + 0.2;   // shimmer
  ray *= smoothstep(1.15, 0.0, uv.y * 0.6 + rp.x * 0.25); // fade with depth
  col = mix(col, silkVivid, ray * 0.05 * strength);

  // Vignette toward the deep base tone
  float vig = distance(uv, vec2(0.5, 0.55));
  col = mix(col, base0, smoothstep(0.45, 1.05, vig) * mix(0.7, 0.35, u_light));

  gl_FragColor = vec4(col, 1.0);
}
`;
