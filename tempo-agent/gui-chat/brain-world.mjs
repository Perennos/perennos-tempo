// The second brain as a three.js world: a lattice sphere of grain, a warm core, and a node for every source
// Tempo knows, laid out and kept up to date by itself. Same bytes in the chat surface and in Vault.
//
//   shell      4000-8000 GPU points in rings of latitude: soft round grain, a slow breathing wave, depth fade
//   nodes      one point sprite per source, sized by importance or recency, coloured from the page's tokens
//   links      glowing arcs: written links, shared tags, similar content (QuadraticBezier, tessellated)
//   trails     light travelling from the core along the links to the nodes used this turn, while thinking
//   life       new nodes grow in, removed ones dissolve; nodes used this turn are "playing" and pulse warm; a reply sends
//              the downbeat (a ripple) through the lattice, the score; a finished run a burst
//   post       UnrealBloom on the core and the warm nodes, a vignette and film grain (skipped when too slow)
//
// brain-sim.mjs holds the pure parts (layout, labels, diffing); this file is the drawing and the input.
// Reduced motion, or pause(true): no auto-turn, no pulses, no trails, the layout settled before the first
// frame and every change applied at once; frames are drawn only when something is asked for.
import {
  normalize, diffGraph, ForceSim, pickLabels, clusterLeaders, matchNodes, fitSphere, latticePoints, adjacency,
  shortestPath, sampleRoute, routeAt, clamp, damp, smooth, ago, hashStr, rng,
} from './brain-sim.mjs';

const here = u => new URL(u, import.meta.url).href;
const MAX_SHELL = 8000, GLOW_SLOTS = 8, TRAIL_SLOTS = 224, SPARK_SLOTS = 128, ARC = 12, FOV = 40;
// Quality steps the frame-time watchdog moves between: shell points, device pixel ratio, bloom.
const TIERS = [
  { points: 8000, dpr: 1.5, bloom: true }, { points: 6000, dpr: 1.25, bloom: true },
  { points: 4000, dpr: 1, bloom: true }, { points: 4000, dpr: 1, bloom: false },
];
const COPY = {
  open: 'Open', close: 'Close', links: 'Linked to', changed: 'Last changed', kind: 'Kind', search: 'Search the score',
  noMatch: 'Nothing by that name', loadError: "The 3D view didn't load. Here's the flat one instead.", nodes: 'sources',
};

export function webglOk() {
  try {
    const c = document.createElement('canvas');
    return !!c.getContext('webgl2');
  } catch { return false; }
}

// Any CSS colour the page uses (hex, rgb, oklch...) as [r, g, b] in 0..1 sRGB.
function cssRgb(css, fb) {
  const c = document.createElement('canvas');
  c.width = c.height = 1;
  const x = c.getContext('2d', { willReadFrequently: true });
  x.fillStyle = fb;
  x.fillStyle = css || fb;
  x.fillRect(0, 0, 1, 1);
  const d = x.getImageData(0, 0, 1, 1).data;
  return [d[0] / 255, d[1] / 255, d[2] / 255];
}

const KIND_TOKEN = {
  memory: 'ink', note: 'ink', core: 'frost', current: 'frost', daily: 'sand', pack: 'sand', tag: 'tag',
  conversation: 'silver', chat: 'silver', band: 'warm2', conductor: 'warm2', attachment: 'mute', unresolved: 'dim',
};

// ---- shaders ----------------------------------------------------------------------------------------------
// Colours are linear and may exceed 1 (that is what the bloom picks up). With no composer the fragment
// converts to sRGB itself (uSrgb = 1).
const OUT = 'gl_FragColor.rgb = mix(gl_FragColor.rgb, pow(max(gl_FragColor.rgb, 0.0), vec3(1.0 / 2.2)), uSrgb);';
const HASH = 'float hash(vec2 p){ return fract(sin(dot(p, vec2(12.9898, 78.233))) * 43758.5453); }';

const SHELL_VS = `
attribute float aSeed;
uniform float uTime, uPx, uBreath, uRipT, uBurst, uDensity, uLight;
uniform vec3 uRipDir, uInk, uWarm;
uniform vec4 uGlow[${GLOW_SLOTS}];
varying vec3 vC; varying float vA;
void main(){
  vec3 p = position;
  float swell = 0.6 * sin(uTime * 0.9 - p.y * 4.2) + 0.4 * sin(uTime * 0.37 + p.x * 2.1 + p.z * 1.7);
  // a reply: one front sweeps over the body from the side that faces the viewer
  float ang = acos(clamp(dot(p, uRipDir), -1.0, 1.0));
  float rip = uRipT >= 0.0 ? exp(-pow((ang - uRipT * 1.9) * 3.0, 2.0)) * (1.0 - smoothstep(1.3, 1.9, uRipT)) : 0.0;
  // a finished run: the whole body swells outward and settles
  float bst = uBurst >= 0.0 ? exp(-uBurst * 1.7) * smoothstep(0.0, 0.07, uBurst) : 0.0;
  p *= 1.0 + 0.011 * swell * uBreath + 0.03 * rip + 0.07 * bst * (0.55 + 0.45 * aSeed);
  float glow = 0.0;
  for (int i = 0; i < ${GLOW_SLOTS}; i++) glow += uGlow[i].w * (1.0 - smoothstep(0.0, 0.7, distance(p, uGlow[i].xyz)));
  glow = min(glow, 1.0);
  vec4 mv = modelViewMatrix * vec4(p, 1.0);
  vec4 c0 = modelViewMatrix * vec4(0.0, 0.0, 0.0, 1.0);
  float front = clamp(0.5 + 0.5 * (mv.z - c0.z) / 1.0, 0.0, 1.0);
  vec3 nv = normalize((modelViewMatrix * vec4(normalize(p), 0.0)).xyz);
  float rim = 1.0 - abs(nv.z);
  float tw = 0.62 + 0.38 * fract(sin(aSeed * 91.7 + floor(uTime * 4.0) * 7.13) * 43758.5453);
  float near = smoothstep(0.3, 0.95, -mv.z);
  float a = tw * mix(0.26, 1.0, front) * mix(1.0, 0.5, pow(rim, 3.0)) * near * (0.82 + 0.35 * glow + 0.5 * rip + 0.6 * bst);
  float size = 0.88 * (1.0 + 0.25 * uLight) * uDensity * (1.0 + 0.34 * glow + 0.9 * rip + 0.7 * bst) * (1.0 + 0.1 * uBreath * sin(uTime * 0.7 + aSeed * 6.2831));
  gl_PointSize = clamp(uPx * size * (9.5 / max(-mv.z, 0.2)), 0.0, 9.0 * uPx);
  gl_Position = projectionMatrix * mv;
  vC = mix(uInk, uWarm, clamp(glow * 0.8 + rip * 0.3, 0.0, 1.0)) * (1.0 + 0.5 * bst);
  vA = a * mix(1.0, 1.75, uLight);
}`;
const SHELL_FS = `
uniform float uTime, uSrgb; varying vec3 vC; varying float vA; ${HASH}
void main(){
  float d = length(gl_PointCoord - 0.5); if (d > 0.5) discard;
  float grain = 0.86 + 0.14 * hash(gl_FragCoord.xy + floor(uTime * 12.0));
  gl_FragColor = vec4(vC, smoothstep(0.5, 0.14, d) * vA * grain);
  ${OUT}
}`;

const NODE_VS = `
attribute vec3 aColor; attribute float aSize; attribute vec3 aFx;
uniform float uPx; varying vec3 vC; varying float vH, vD, vL;
void main(){
  vec4 mv = modelViewMatrix * vec4(position, 1.0);
  vec4 c0 = modelViewMatrix * vec4(0.0, 0.0, 0.0, 1.0);
  float front = clamp(0.5 + 0.5 * (mv.z - c0.z) / 1.0, 0.0, 1.0);
  gl_PointSize = clamp(uPx * aSize * (3.1 / max(-mv.z, 0.25)), 0.0, 90.0 * uPx);
  gl_Position = projectionMatrix * mv;
  vC = aColor; vH = aFx.x; vL = aFx.y; vD = mix(0.5, 1.0, front) * aFx.z;
}`;
const NODE_FS = `
uniform float uSrgb; varying vec3 vC; varying float vH, vD, vL;
void main(){
  float d = length(gl_PointCoord - 0.5) * 2.0; if (d > 1.0) discard;
  float dot_ = smoothstep(0.46, 0.34, d);
  float halo = pow(1.0 - d, 2.4) * (0.5 + 0.9 * vH);
  float ring = vH * smoothstep(0.07, 0.0, abs(d - 0.8));
  float a = clamp((dot_ + halo * 0.6 + ring) * vL * vD, 0.0, 1.0);
  gl_FragColor = vec4(vC * (1.0 + 0.5 * vH), a);
  ${OUT}
}`;

const LINK_VS = `
attribute vec4 aCol; uniform float uPx; varying vec4 vCol;
void main(){
  vec4 mv = modelViewMatrix * vec4(position, 1.0);
  vec4 c0 = modelViewMatrix * vec4(0.0, 0.0, 0.0, 1.0);
  float front = clamp(0.5 + 0.5 * (mv.z - c0.z) / 1.0, 0.0, 1.0);
  vCol = vec4(aCol.rgb, aCol.a * mix(0.4, 1.0, front));
  gl_Position = projectionMatrix * mv;
}`;
const LINK_FS = `uniform float uSrgb; varying vec4 vCol; void main(){ gl_FragColor = vCol; ${OUT} }`;

const SPARK_VS = `
attribute float aA; attribute float aS; uniform float uPx; varying float vA;
void main(){
  vec4 mv = modelViewMatrix * vec4(position, 1.0);
  gl_PointSize = clamp(uPx * aS * (3.1 / max(-mv.z, 0.25)), 0.0, 40.0 * uPx);
  gl_Position = projectionMatrix * mv; vA = aA;
}`;
const SPARK_FS = `
uniform vec3 uColor; uniform float uSrgb; varying float vA;
void main(){
  float d = length(gl_PointCoord - 0.5) * 2.0; if (d > 1.0) discard;
  gl_FragColor = vec4(uColor, pow(1.0 - d, 1.6) * vA);
  ${OUT}
}`;

const CORE_VS = `
attribute float aSeed; uniform float uTime, uPx, uPulse, uBreath, uLight; uniform vec3 uColor; varying float vA; varying vec3 vC;
void main(){
  vec3 p = position * (1.0 + 0.06 * uBreath * sin(uTime * 1.3 + aSeed * 6.28) + 0.16 * uPulse);
  p += 0.004 * vec3(sin(uTime * 1.7 + aSeed * 41.0), cos(uTime * 1.3 + aSeed * 37.0), sin(uTime * 1.1 + aSeed * 29.0)) * uBreath;
  vec4 mv = modelViewMatrix * vec4(p, 1.0);
  gl_PointSize = clamp(uPx * (1.6 + 1.4 * aSeed) * (1.0 + 0.5 * uLight) * (1.0 + 0.5 * uPulse) * (3.1 / max(-mv.z, 0.25)), 0.0, 30.0 * uPx);
  gl_Position = projectionMatrix * mv;
  vA = 0.55 + 0.45 * aSeed; vC = uColor;
}`;
const CORE_FS = `
uniform float uSrgb; varying float vA; varying vec3 vC;
void main(){
  float d = length(gl_PointCoord - 0.5) * 2.0; if (d > 1.0) discard;
  gl_FragColor = vec4(vC, pow(1.0 - d, 1.4) * vA);
  ${OUT}
}`;

const FINISH = {
  uniforms: { tDiffuse: { value: null }, uTime: { value: 0 }, uVig: { value: 0.3 }, uGrain: { value: 0.035 }, uTint: { value: null }, uAspect: { value: 1 } },
  vertexShader: 'varying vec2 vUv; void main(){ vUv = uv; gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0); }',
  fragmentShader: `uniform sampler2D tDiffuse; uniform float uTime, uVig, uGrain, uAspect; uniform vec3 uTint; varying vec2 vUv; ${HASH}
    void main(){
      vec4 c = texture2D(tDiffuse, vUv);
      vec2 q = (vUv - 0.5) * vec2(uAspect, 1.0);
      float r = length(q);
      c.rgb *= 1.0 - uVig * smoothstep(0.32, 0.95, r);
      c.rgb += uTint * (1.0 - smoothstep(0.0, 0.5, r)) * 0.5;
      c.rgb += (hash(vUv * 977.0 + fract(uTime * 7.0)) - 0.5) * uGrain;
      gl_FragColor = vec4(c.rgb, 1.0);
    }`,
};

// ---- the world -------------------------------------------------------------------------------------------

// opts: {data (raw graph), source 'agent'|'vault', reduced, copy, onOpen(node), describe(node) -> Promise<{mtime}>,
//        quality 0..3 (pins the quality step and turns the watchdog off), flatLabel, fallback(host, raw, opts)}
// Returns {mode, setData(raw), setThinking(b), busy(b), setUsed(ids), ripple(), burst(), search(q), focus(id),
//          retheme(), pause(b), resize(), close(), nodeCount()}.
export async function mountBrain(host, opts = {}) {
  const copy = { ...COPY, ...(opts.copy || {}) };
  const note = msg => {
    let n = host.querySelector(':scope > .bw-note');
    if (!n) { n = document.createElement('p'); n.className = 'bw-note'; host.append(n); }
    n.textContent = msg; n.hidden = !msg;
  };
  const flat = async raw => {
    const { openGraph } = await import(here('./memgraph.mjs'));
    const used = new Set((raw?.nodes || []).filter(n => n.used).map(n => n.src));
    let g = await openGraph(host, raw || { nodes: [], edges: [] }, { used, reduced: true, flatLabel: opts.flatLabel, onPick: src => opts.onOpen?.({ id: src, path: src, title: src }) });
    // The flat picture is redrawn from scratch when the data changes.
    return {
      mode: 'flat', setData: async r => { g.close(); g = await openGraph(host, r, { used: new Set((r.nodes || []).filter(n => n.used).map(n => n.src)), reduced: true, flatLabel: opts.flatLabel, onPick: src => opts.onOpen?.({ id: src, path: src, title: src }) }); },
      setThinking: b => g.busy?.(b), busy: b => g.busy?.(b), setUsed() {}, ripple: () => g.ripple?.(), burst: () => g.burst?.(), search() {}, focus() {},
      retheme: () => g.retheme?.(), pause: b => g.pause?.(b), resize: () => g.resize?.(), close: () => g.close(), nodeCount: () => (raw?.nodes || []).length,
    };
  };
  if (!webglOk()) return flat(opts.data);
  let T, post;
  try {
    T = await import(here('./vendor/three.module.min.js'));
  } catch {
    note(copy.loadError);
    return flat(opts.data).catch(() => ({ mode: 'error', setData() {}, setThinking() {}, busy() {}, setUsed() {}, ripple() {}, burst() {}, search() {}, focus() {}, retheme() {}, pause() {}, resize() {}, close() { host.replaceChildren(); }, nodeCount: () => 0 }));
  }
  try { post = await import(here('./vendor/three-post.js')); } catch { post = null; }
  try {
    return await build(T, post, host, opts, copy, note);
  } catch (e) {
    note(copy.loadError);
    console.warn('brain world failed', e);
    return flat(opts.data);
  }
}

function build(T, post, host, opts, copy, note) {
  const source = opts.source || 'agent';
  const still0 = !!opts.reduced;
  let paused = false, closed = false, thinking = false, visible = true;
  const still = () => still0 || paused;

  // ---- renderer, scene, camera
  const canvas = document.createElement('canvas');
  canvas.className = 'bw-canvas';
  host.prepend(canvas);
  const renderer = new T.WebGLRenderer({ canvas, antialias: false, alpha: false, powerPreference: 'high-performance' });
  renderer.autoClear = true;
  const scene = new T.Scene(), cam = new T.PerspectiveCamera(FOV, 1, 0.05, 40);
  // ?brainq=0..3 pins the quality step (and ?brainmaxdt=seconds lets slow software GL animate in wall time);
  // for checking a page on a machine without a GPU. It also exposes the controller as window.__brain.
  const qs = (() => { try { return new URLSearchParams(location.search); } catch { return new URLSearchParams(); } })();
  const q0 = opts.quality ?? (qs.has('brainq') ? Number(qs.get('brainq')) : null);
  let tier = q0 != null ? clamp(q0 | 0, 0, TIERS.length - 1) : 1, pinned = q0 != null;
  let ratio = Math.min(devicePixelRatio || 1, TIERS[tier].dpr, 1.5), W = 1, H = 1;
  renderer.setPixelRatio(ratio);

  // ---- theme: every colour comes from the page's own tokens
  let th, light = false;
  const col = new Map();
  const lin = rgb => new T.Color().setRGB(rgb[0], rgb[1], rgb[2], T.SRGBColorSpace);
  function readTheme() {
    const s = getComputedStyle(host), v = n => s.getPropertyValue(n).trim();
    const tok = (n, fb) => lin(cssRgb(v(n), fb));
    const bgRgb = cssRgb(v('--bg'), '#1b1a1b');
    light = 0.2126 * bgRgb[0] + 0.7152 * bgRgb[1] + 0.0722 * bgRgb[2] > 0.5;
    th = {
      bg: lin(bgRgb), ink: tok('--ink', '#e9e6df'), frost: tok('--frost', '#d4d1ca'), silver: tok('--silver', '#a9aaad'), mute: tok('--mute', '#8b949b'),
      dim: tok('--dim', '#4f5053'), warm: tok('--warm', '#ff5b1f'), warm2: tok('--warm2', '#ff8a5c'), sand: tok('--sand', '#c9b38a'), tag: tok('--tag', '#8fb3d1'),
      font: v('--label') || 'ui-monospace, monospace',
    };
    col.clear();
  }
  const kindColor = kind => { const t = KIND_TOKEN[kind] || 'ink'; return th[t]; };
  readTheme();

  const U = {
    uTime: { value: 0 }, uPx: { value: 1 }, uSrgb: { value: 0 }, uLight: { value: 0 }, uBreath: { value: 1 }, uDensity: { value: 1 },
    uRipT: { value: -1 }, uRipDir: { value: new T.Vector3(0, 0, 1) }, uBurst: { value: -1 },
    uInk: { value: new T.Color() }, uWarm: { value: new T.Color() },
    uGlow: { value: Array.from({ length: GLOW_SLOTS }, () => new T.Vector4(0, 0, 0, 0)) },
  };
  const blend = () => (light ? T.NormalBlending : T.AdditiveBlending);

  // shell
  let shell = null, shellGeo = null;
  const shellMat = new T.ShaderMaterial({ uniforms: U, vertexShader: SHELL_VS, fragmentShader: SHELL_FS, transparent: true, depthWrite: false, depthTest: false });
  function buildShell(count) {
    const l = latticePoints(count);
    const g = new T.BufferGeometry();
    g.setAttribute('position', new T.BufferAttribute(l.pos, 3));
    g.setAttribute('aSeed', new T.BufferAttribute(l.seed, 1));
    if (shell) { scene.remove(shell); shellGeo.dispose(); }
    shellGeo = g; shell = new T.Points(g, shellMat); shell.frustumCulled = false; shell.renderOrder = 1;
    scene.add(shell);
    U.uDensity.value = Math.sqrt(6000 / count) * 0.96;
  }

  // core: grain in a small ball, plus a soft sprite for the glow when nothing blooms
  const CORE_N = 520, coreU = { uTime: U.uTime, uPx: U.uPx, uSrgb: U.uSrgb, uBreath: U.uBreath, uLight: U.uLight, uPulse: { value: 0 }, uColor: { value: new T.Color() } };
  const coreGeo = new T.BufferGeometry();
  {
    const p = new Float32Array(CORE_N * 3), s = new Float32Array(CORE_N), r = rng(99);
    for (let i = 0; i < CORE_N; i++) {
      const u = r() * 2 - 1, a = r() * Math.PI * 2, rad = 0.1 * Math.cbrt(r()), q = Math.sqrt(1 - u * u);
      p[i * 3] = Math.cos(a) * q * rad; p[i * 3 + 1] = u * rad; p[i * 3 + 2] = Math.sin(a) * q * rad; s[i] = r();
    }
    coreGeo.setAttribute('position', new T.BufferAttribute(p, 3));
    coreGeo.setAttribute('aSeed', new T.BufferAttribute(s, 1));
  }
  const coreMat = new T.ShaderMaterial({ uniforms: coreU, vertexShader: CORE_VS, fragmentShader: CORE_FS, transparent: true, depthWrite: false, depthTest: false });
  const corePts = new T.Points(coreGeo, coreMat); corePts.frustumCulled = false; corePts.renderOrder = 3;
  const glowTex = (() => {
    const c = document.createElement('canvas'); c.width = c.height = 128;
    const x = c.getContext('2d'), g = x.createRadialGradient(64, 64, 0, 64, 64, 64);
    g.addColorStop(0, 'rgba(255,255,255,.9)'); g.addColorStop(0.18, 'rgba(255,255,255,.4)'); g.addColorStop(0.42, 'rgba(255,255,255,.12)'); g.addColorStop(0.72, 'rgba(255,255,255,.025)'); g.addColorStop(1, 'rgba(255,255,255,0)');
    x.fillStyle = g; x.fillRect(0, 0, 128, 128);
    const t = new T.CanvasTexture(c); t.colorSpace = T.NoColorSpace; return t;
  })();
  const glowMat = new T.SpriteMaterial({ map: glowTex, transparent: true, depthWrite: false, depthTest: false, color: 0xffffff });
  const glow = new T.Sprite(glowMat); glow.renderOrder = 2;
  scene.add(corePts, glow);

  // nodes: one Points object, arrays grown by doubling and reused
  const nodeU = { uPx: U.uPx, uSrgb: U.uSrgb };
  let cap = 0, nPos, nCol, nSize, nFx, nodeGeo = null, nodePts = null;
  const nodeMat = new T.ShaderMaterial({ uniforms: nodeU, vertexShader: NODE_VS, fragmentShader: NODE_FS, transparent: true, depthWrite: false, depthTest: false });
  function growNodes(n) {
    if (n <= cap) return;
    cap = Math.max(64, 1 << Math.ceil(Math.log2(n)));
    nPos = new Float32Array(cap * 3); nCol = new Float32Array(cap * 3); nSize = new Float32Array(cap); nFx = new Float32Array(cap * 3);
    const g = new T.BufferGeometry();
    for (const [k, a, s] of [['position', nPos, 3], ['aColor', nCol, 3], ['aSize', nSize, 1], ['aFx', nFx, 3]]) g.setAttribute(k, new T.BufferAttribute(a, s).setUsage(T.DynamicDrawUsage));
    if (nodePts) { scene.remove(nodePts); nodeGeo.dispose(); }
    nodeGeo = g; nodePts = new T.Points(g, nodeMat); nodePts.frustumCulled = false; nodePts.renderOrder = 4;
    scene.add(nodePts);
  }

  // links: arcs as indexed line segments, ARC pieces each
  const linkU = { uPx: U.uPx, uSrgb: U.uSrgb };
  let linkCap = 0, lPos, lCol, lIdx, linkGeo = null, linkLines = null, linkCount = 0;
  const linkMat = new T.ShaderMaterial({ uniforms: linkU, vertexShader: LINK_VS, fragmentShader: LINK_FS, transparent: true, depthWrite: false, depthTest: false });
  function growLinks(n) {
    if (n <= linkCap) return;
    linkCap = Math.max(128, 1 << Math.ceil(Math.log2(n)));
    lPos = new Float32Array(linkCap * (ARC + 1) * 3); lCol = new Float32Array(linkCap * (ARC + 1) * 4); lIdx = new Uint32Array(linkCap * ARC * 2);
    for (let i = 0; i < linkCap; i++) for (let s = 0; s < ARC; s++) { lIdx[(i * ARC + s) * 2] = i * (ARC + 1) + s; lIdx[(i * ARC + s) * 2 + 1] = i * (ARC + 1) + s + 1; }
    const g = new T.BufferGeometry();
    g.setAttribute('position', new T.BufferAttribute(lPos, 3).setUsage(T.DynamicDrawUsage));
    g.setAttribute('aCol', new T.BufferAttribute(lCol, 4).setUsage(T.DynamicDrawUsage));
    g.setIndex(new T.BufferAttribute(lIdx, 1));
    if (linkLines) { scene.remove(linkLines); linkGeo.dispose(); }
    linkGeo = g; linkLines = new T.LineSegments(g, linkMat); linkLines.frustumCulled = false; linkLines.renderOrder = 2.5;
    scene.add(linkLines);
  }

  // particles: light trails then sparks, one Points object
  const sparkU = { uPx: U.uPx, uSrgb: U.uSrgb, uColor: { value: new T.Color() } };
  const PN = TRAIL_SLOTS + SPARK_SLOTS, pPos = new Float32Array(PN * 3), pA = new Float32Array(PN), pS = new Float32Array(PN);
  const partGeo = new T.BufferGeometry();
  partGeo.setAttribute('position', new T.BufferAttribute(pPos, 3).setUsage(T.DynamicDrawUsage));
  partGeo.setAttribute('aA', new T.BufferAttribute(pA, 1).setUsage(T.DynamicDrawUsage));
  partGeo.setAttribute('aS', new T.BufferAttribute(pS, 1).setUsage(T.DynamicDrawUsage));
  const partMat = new T.ShaderMaterial({ uniforms: sparkU, vertexShader: SPARK_VS, fragmentShader: SPARK_FS, transparent: true, depthWrite: false, depthTest: false });
  const parts = new T.Points(partGeo, partMat); parts.frustumCulled = false; parts.renderOrder = 5;
  scene.add(parts);

  // ---- post: bloom on what is brighter than white, then vignette and grain
  let composer = null, bloom = null, finish = null;
  function buildPost() {
    if (!post || composer || !TIERS[tier].bloom) return;
    try {
      const rt = new T.WebGLRenderTarget(W * ratio, H * ratio, { type: T.HalfFloatType });
      composer = new post.EffectComposer(renderer, rt);
      composer.setPixelRatio(ratio); composer.setSize(W, H);
      composer.addPass(new post.RenderPass(scene, cam));
      bloom = new post.UnrealBloomPass(new T.Vector2(W, H), 0.6, 0.7, 1.0);
      composer.addPass(bloom);
      composer.addPass(new post.OutputPass());
      finish = new post.ShaderPass(FINISH);
      finish.uniforms.uTint.value = new T.Color();
      composer.addPass(finish);
      U.uSrgb.value = 0;
    } catch (e) { console.warn('brain: no post-processing', e); dropPost(); }
  }
  function dropPost() {
    if (composer) { composer.dispose?.(); composer = null; bloom = null; finish = null; }
    U.uSrgb.value = 1;
  }
  function applyTheme() {
    U.uInk.value.copy(th.ink); U.uWarm.value.copy(th.warm);
    U.uLight.value = light ? 1 : 0;
    const warmHdr = light ? 1 : 3;
    coreU.uColor.value.copy(th.warm).multiplyScalar(warmHdr);
    sparkU.uColor.value.copy(th.warm2).multiplyScalar(light ? 1 : 4);
    glowMat.color.copy(th.warm).multiplyScalar(light ? 1 : 1.4);
    glowMat.opacity = light ? 0.45 : 0.4;
    for (const m of [shellMat, nodeMat, linkMat, coreMat, partMat, glowMat]) { m.blending = blend(); m.needsUpdate = true; }
    // As the scene background, not the renderer's clear colour: that one is encoded for the screen even while
    // the composer draws into its linear float target, which would encode it twice.
    scene.background = th.bg.clone();
    if (bloom) { bloom.strength = light ? 0.2 : 0.62; bloom.radius = light ? 0.4 : 0.72; bloom.threshold = light ? 0.97 : 1.0; }
    if (finish) { finish.uniforms.uVig.value = light ? 0.12 : 0.34; finish.uniforms.uGrain.value = light ? 0.03 : 0.04; finish.uniforms.uTint.value.copy(th.warm).multiplyScalar(light ? 0.02 : 0.022); }
  }

  // ---- camera: orbit with damping, flights, a view shift that keeps a node clear of its card
  const view = { theta: 0.55, phi: 1.3, dist: 3.2, tx: 0, ty: 0, tz: 0, shift: 0 };
  const goal = { ...view };
  let fit = 3.2, flying = false, lastTouch = -1e9, dragging = false;
  const fitDist = () => { const a = W / Math.max(H, 1), r = 1.12; return (r / Math.sin(FOV * Math.PI / 360)) / Math.min(1, a * 1.15); };
  function placeCamera() {
    const sp = Math.sin(view.phi);
    cam.position.set(view.tx + view.dist * sp * Math.sin(view.theta), view.ty + view.dist * Math.cos(view.phi), view.tz + view.dist * sp * Math.cos(view.theta));
    cam.lookAt(view.tx, view.ty, view.tz);
    cam.updateMatrixWorld();
    if (view.shift) cam.setViewOffset(W, H, view.shift * W, 0, W, H); else cam.clearViewOffset();
  }
  function goHome() { Object.assign(goal, { dist: fit, tx: 0, ty: 0, tz: 0, shift: 0 }); flying = true; if (still()) snapCamera(); }
  function snapCamera() { Object.assign(view, goal); flying = false; }
  const wrapNear = (a, ref) => { while (a - ref > Math.PI) a -= 2 * Math.PI; while (a - ref < -Math.PI) a += 2 * Math.PI; return a; };

  // ---- data, items and layout
  let graph = { nodes: [], links: [] }, adj = new Map(), byId = new Map(), leaders = new Set();
  const items = new Map(), order = [];
  const sim = new ForceSim();
  let simPrimed = false, linksDirty = true, routesDirty = true, hubId = null, usedIds = [];
  let hovered = null, selected = null, matches = new Set(), matchList = [], searchQ = '';

  function setData(raw) {
    if (closed) return;
    const next = normalize(raw, { source });
    const d = diffGraph(graph, next);
    if (d.empty) return;
    const instant = still();
    for (const n of d.removed) { const it = items.get(n.id); if (!it) continue; sim.at(n.id, it.pos); if (instant) dropItem(it); else { it.dying = true; it.delay = 0; } }
    let k = 0;
    for (const n of next.nodes) {
      let it = items.get(n.id);
      if (!it) {
        it = { id: n.id, node: n, life: instant ? 1 : 0, delay: instant ? 0 : Math.min(0.9, k++ * 0.045), dying: false, pos: [0, 0, 0], hover: 0, warm: n.used ? 1 : 0, size: n.size, phase: (hashStr(n.id) % 628) / 100, match: 0, dim: 1 };
        items.set(n.id, it); order.push(it);
      } else { it.node = n; it.dying = false; }
    }
    graph = next; byId = new Map(next.nodes.map(n => [n.id, n])); adj = adjacency(next.links);
    hubId = next.nodes.reduce((b, n) => (!b || n.chunks > b.chunks ? n : b), null)?.id ?? null;
    leaders = new Set(clusterLeaders(next.nodes, 8));
    usedIds = next.nodes.filter(n => n.used).map(n => n.id);
    sim.setGraph(next.nodes, next.links, simPrimed ? 0.5 : 1);
    if (!simPrimed || instant) { sim.settle(instant ? 500 : 110); simPrimed = true; }
    if (selected && !byId.has(selected)) selectNode(null);
    linksDirty = routesDirty = true;
    rebuildTrails(); refreshMatches(); drawCountHint(); kick();
  }
  function dropItem(it) { items.delete(it.id); const i = order.indexOf(it); if (i >= 0) order.splice(i, 1); }
  function setUsed(ids) {
    const s = new Set(ids);
    setData({ ...rawOf(), nodes: rawOf().nodes.map(n => ({ ...n, used: s.has(n.src ?? n.id) })) });
  }
  function rawOf() {
    const idx = new Map(graph.nodes.map((n, i) => [n.id, i]));
    return { nodes: graph.nodes.map(n => ({ src: n.id, name: n.title, kind: n.kind, chunks: n.chunks, used: n.used, mtime: n.mtime })), edges: graph.links.map(l => ({ a: idx.get(l.a), b: idx.get(l.b), w: l.w, type: l.type })) };
  }

  // ---- trails and sparks
  let trails = [], sparks = [], gain = 0, burstSeed = 1;
  function rebuildTrails() {
    trails = [];
    if (still()) return;
    let targets = usedIds.slice(0, Math.floor(TRAIL_SLOTS / 7));
    if (!targets.length && thinking) { // nothing named yet: the agent is looking, so a few trails wander out
      const r = rng(burstSeed);
      const pool = graph.nodes.filter(n => n.id !== hubId);
      for (let i = 0; i < Math.min(5, pool.length); i++) targets.push(pool[Math.floor(r() * pool.length)].id);
    }
    targets.forEach((id, i) => trails.push({ id, phase: (i * 0.618) % 1, period: 2.1 + (i % 3) * 0.25, route: null }));
    routesDirty = true;
  }
  const posOf = id => { const it = items.get(id); return it ? it.pos : null; };
  function buildRoutes() {
    for (const tr of trails) {
      const tp = posOf(tr.id); if (!tp) { tr.route = null; continue; }
      const pts = [[0, 0, 0]];
      let path = hubId && hubId !== tr.id ? shortestPath(adj, hubId, tr.id) : null;
      if (path && path.length > 7) path = [path[0], ...path.slice(-6)];
      for (const id of path || [tr.id]) { const p = posOf(id); if (p) pts.push(p.slice()); }
      if (pts.length < 2) pts.push(tp.slice());
      tr.route = sampleRoute(pts, 10);
    }
    routesDirty = false;
  }
  function spawnSparks() {
    const r = rng(burstSeed++ * 7919);
    sparks = [];
    for (let i = 0; i < SPARK_SLOTS; i++) {
      const u = r() * 2 - 1, a = r() * Math.PI * 2, q = Math.sqrt(1 - u * u);
      sparks.push({ dx: Math.cos(a) * q, dy: u, dz: Math.sin(a) * q, speed: 0.55 + r() * 0.9, born: -r() * 0.06, life: 0.9 + r() * 0.7 });
    }
  }

  // ---- ripple and burst
  let ripT = -1, burstT = -1, flare = 0;
  function ripple() {
    if (still() || closed) return;
    U.uRipDir.value.copy(cam.position).sub(new T.Vector3(view.tx, view.ty, view.tz)).normalize();
    ripT = 0; kick();
  }
  function burst() {
    if (still() || closed) return;
    burstT = 0; flare = 1; spawnSparks(); kick();
  }

  // ---- selection, hover, search, card
  const labelsEl = document.createElement('div'); labelsEl.className = 'bw-labels'; labelsEl.setAttribute('aria-hidden', 'true');
  const card = document.createElement('aside'); card.className = 'bw-card'; card.hidden = true; card.setAttribute('role', 'dialog');
  const sbox = document.createElement('div'); sbox.className = 'bw-search'; sbox.hidden = true;
  const sin = document.createElement('input'); sin.type = 'search'; sin.placeholder = copy.search; sin.setAttribute('aria-label', copy.search); sin.autocomplete = 'off';
  const sCount = document.createElement('span'); sCount.className = 'bw-count';
  sbox.append(sin, sCount);
  const count = document.createElement('span'); count.className = 'bw-total';
  host.append(labelsEl, card, sbox, count);
  function drawCountHint() { count.textContent = graph.nodes.length ? `${graph.nodes.length} ${copy.nodes}` : ''; }

  function selectNode(id, fly = true) {
    selected = id;
    if (!id) {
      card.hidden = true;
      if (fly) goHome();
      goal.shift = 0;
      kick(); return;
    }
    const it = items.get(id); if (!it) return;
    flyToNode(it);
    fillCard(it.node);
    card.hidden = false;
    goal.shift = W > 720 ? -0.12 : 0; // the node sits clear of the card
    kick();
  }
  function flyToNode(it) {
    const p = it.pos, len = Math.hypot(p[0], p[1], p[2]);
    goal.tx = p[0]; goal.ty = p[1]; goal.tz = p[2];
    if (len > 0.12) { goal.theta = wrapNear(Math.atan2(p[0], p[2]), view.theta); goal.phi = clamp(Math.acos(clamp(p[1] / len, -1, 1)), 0.4, Math.PI - 0.4); }
    goal.dist = 1.5; flying = true; lastTouch = performance.now();
    if (still()) snapCamera();
  }
  function fillCard(n) {
    const el = (t, c, txt) => { const e = document.createElement(t); if (c) e.className = c; if (txt != null) e.textContent = txt; return e; };
    const close = el('button', 'bw-x', '×'); close.type = 'button'; close.setAttribute('aria-label', copy.close); close.onclick = () => selectNode(null);
    const open = el('button', 'bw-open', copy.open); open.type = 'button'; open.onclick = () => opts.onOpen?.(n);
    const when = el('div', 'bw-when');
    const setWhen = m => { when.textContent = m ? `${copy.changed}: ${ago(m)}` : ''; if (m) when.title = new Date(m * 1000).toLocaleString(); };
    setWhen(n.mtime);
    if (!n.mtime && opts.describe) Promise.resolve(opts.describe(n)).then(r => { if (r?.mtime && selected === n.id) { n.mtime = r.mtime; setWhen(r.mtime); } }).catch(() => {});
    const neigh = (adj.get(n.id) || []).slice().sort((a, b) => b.w - a.w).slice(0, 8);
    const list = el('div', 'bw-links');
    for (const { id } of neigh) { const m = byId.get(id); if (!m) continue; const b = el('button', 'bw-link', m.title); b.type = 'button'; b.onclick = () => selectNode(id); list.append(b); }
    card.replaceChildren(close, el('div', 'bw-kind', n.kind), el('h2', 'bw-title', n.title), el('div', 'bw-path', n.path), when,
      ...(neigh.length ? [el('div', 'bw-sub', `${copy.links} ${(adj.get(n.id) || []).length}`), list] : []), open);
  }

  function refreshMatches() {
    matchList = searchQ ? matchNodes(graph.nodes, searchQ) : [];
    matches = new Set(matchList);
    sCount.textContent = searchQ ? (matchList.length ? String(matchList.length) : copy.noMatch) : '';
  }
  function frameMatches() {
    if (!matchList.length) { if (!selected) goHome(); return; }
    const pts = matchList.map(id => posOf(id)).filter(Boolean);
    const f = fitSphere(pts);
    goal.tx = f.c[0] * 0.8; goal.ty = f.c[1] * 0.8; goal.tz = f.c[2] * 0.8;
    goal.dist = clamp(f.r / Math.sin(FOV * Math.PI / 360) * 1.35 + 1.0, 1.4, fit);
    flying = true; lastTouch = performance.now();
    if (still()) snapCamera();
    kick();
  }
  let searchTimer = 0;
  function search(q) {
    searchQ = (q || '').trim();
    refreshMatches();
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => { if (!closed) frameMatches(); }, 140);
    kick();
  }
  function openSearch(on) {
    sbox.hidden = !on;
    if (on) { sin.value = searchQ; sin.focus(); sin.select(); }
    else { sin.blur(); if (searchQ) { sin.value = ''; search(''); } }
  }
  sin.addEventListener('input', () => search(sin.value));
  sin.addEventListener('keydown', e => {
    e.stopPropagation();
    if (e.key === 'Escape') { openSearch(false); goHome(); }
    else if (e.key === 'Enter' && matchList.length) { selectNode(matchList[0]); sin.blur(); }
  });
  const hostVisible = () => host.isConnected && host.getClientRects().length > 0;
  const onKey = e => {
    if (closed || !hostVisible() || e.ctrlKey || e.metaKey || e.altKey) return;
    if (e.key === '/' && !e.target?.closest?.('input, textarea, select, [contenteditable]')) { e.preventDefault(); e.stopPropagation(); openSearch(true); }
    else if (e.key === 'Escape' && (selected || !sbox.hidden)) {
      e.stopPropagation();
      if (!sbox.hidden) openSearch(false);
      if (selected) selectNode(null); else goHome();
    }
  };
  window.addEventListener('keydown', onKey, true);

  // ---- pointer input
  const ptrs = new Map();
  let downAt = null, pinch = 0;
  const local = e => { const r = canvas.getBoundingClientRect(); return [e.clientX - r.left, e.clientY - r.top]; };
  canvas.addEventListener('pointerdown', e => {
    canvas.setPointerCapture(e.pointerId);
    ptrs.set(e.pointerId, [e.clientX, e.clientY]);
    downAt = { x: e.clientX, y: e.clientY, moved: false, id: e.pointerId };
    if (ptrs.size === 2) { const [a, b] = [...ptrs.values()]; pinch = Math.hypot(a[0] - b[0], a[1] - b[1]); }
    lastTouch = performance.now(); flying = false; dragging = true; kick();
  });
  canvas.addEventListener('pointermove', e => {
    const prev = ptrs.get(e.pointerId);
    if (prev) {
      ptrs.set(e.pointerId, [e.clientX, e.clientY]);
      if (ptrs.size === 2) {
        const [a, b] = [...ptrs.values()], d = Math.hypot(a[0] - b[0], a[1] - b[1]);
        if (pinch) goal.dist = clamp(goal.dist * pinch / d, 1.15, fit * 2.4);
        pinch = d; downAt && (downAt.moved = true);
      } else {
        const dx = e.clientX - prev[0], dy = e.clientY - prev[1];
        if (downAt && Math.hypot(e.clientX - downAt.x, e.clientY - downAt.y) > 4) downAt.moved = true;
        if (downAt?.moved) { goal.theta -= dx * 0.0062; goal.phi = clamp(goal.phi - dy * 0.0062, 0.12, Math.PI - 0.12); setHover(null); }
      }
      lastTouch = performance.now(); kick(); return;
    }
    const [x, y] = local(e);
    setHover(pick(x, y));
  });
  const up = e => {
    ptrs.delete(e.pointerId); pinch = 0;
    if (!ptrs.size) dragging = false;
    if (downAt && downAt.id === e.pointerId && !downAt.moved && e.type === 'pointerup') {
      const [x, y] = local(e), id = pick(x, y);
      if (id) selectNode(id); else if (selected) selectNode(null);
    }
    if (downAt?.id === e.pointerId) downAt = null;
    lastTouch = performance.now(); kick();
  };
  canvas.addEventListener('pointerup', up);
  canvas.addEventListener('pointercancel', up);
  canvas.addEventListener('pointerleave', () => { if (!dragging) setHover(null); });
  canvas.addEventListener('wheel', e => {
    e.preventDefault();
    goal.dist = clamp(goal.dist * Math.exp(e.deltaY * (e.ctrlKey ? 0.01 : 0.0013)), 1.15, fit * 2.4);
    lastTouch = performance.now(); flying = false; kick();
  }, { passive: false });
  canvas.setAttribute('tabindex', '0');
  canvas.addEventListener('keydown', e => {
    const k = { ArrowLeft: [-0.12, 0], ArrowRight: [0.12, 0], ArrowUp: [0, -0.1], ArrowDown: [0, 0.1] }[e.key];
    if (k) { e.preventDefault(); goal.theta += k[0]; goal.phi = clamp(goal.phi + k[1], 0.12, Math.PI - 0.12); lastTouch = performance.now(); kick(); }
    else if (e.key === '+' || e.key === '=') { goal.dist = clamp(goal.dist * 0.85, 1.15, fit * 2.4); kick(); }
    else if (e.key === '-') { goal.dist = clamp(goal.dist / 0.85, 1.15, fit * 2.4); kick(); }
  });
  function setHover(id) {
    if (id === hovered) return;
    hovered = id; canvas.style.cursor = id ? 'pointer' : '';
    kick();
  }

  // ---- projection: screen position and how much in front of the core each node is
  const sx = new Float32Array(0);
  let scr = { x: sx, y: sx, f: sx, ids: [] };
  const tmp = new T.Vector3();
  function project() {
    const n = order.length;
    if (scr.x.length < n) scr = { x: new Float32Array(n * 2), y: new Float32Array(n * 2), f: new Float32Array(n * 2), ids: [] };
    scr.ids.length = n;
    const cx = cam.position.length();
    for (let i = 0; i < n; i++) {
      const it = order[i], p = it.pos;
      tmp.set(p[0], p[1], p[2]);
      const dist = tmp.distanceTo(cam.position);
      tmp.project(cam);
      scr.x[i] = (tmp.x + 1) / 2 * W; scr.y[i] = (1 - tmp.y) / 2 * H;
      scr.f[i] = tmp.z > 1 ? 0 : clamp(0.5 + 0.5 * (cx - dist), 0, 1);
      scr.ids[i] = it.id;
    }
  }
  function pick(px, py) {
    let best = null, bd = 18 * 18;
    for (let i = 0; i < order.length; i++) {
      const it = order[i]; if (it.dying || it.life < 0.5 || scr.f[i] < 0.25) continue;
      const d = (scr.x[i] - px) ** 2 + (scr.y[i] - py) ** 2 - scr.f[i] * 30; // the nearer node wins a tie
      if (d < bd) { bd = d; best = it.id; }
    }
    return best;
  }

  // ---- labels: crisp HTML in the label font, shown by priority, never touching
  const labelEls = new Map(), widths = new Map(), shown = new Set();
  const measure = (() => { const c = document.createElement('canvas').getContext('2d'); return t => { c.font = `500 10px ${th.font}`; return c.measureText(t.toUpperCase()).width + t.length * 1.0 + 6; }; })();
  let labelTick = 0;
  function updateLabels(force) {
    if (!force && (labelTick++ & 1)) return;
    const cands = [];
    const lit = new Set([...usedIds, ...matchList.slice(0, 12)]);
    for (let i = 0; i < order.length; i++) {
      const it = order[i], n = it.node;
      if (it.dying || it.life < 0.7) continue;
      const id = it.id, isH = id === hovered, isS = id === selected, isU = lit.has(id) && !(searchQ && !matches.has(id) && !n.used), isL = leaders.has(id);
      if (!(isH || isS || isU || isL)) continue;
      if (scr.f[i] < (isU ? 0.2 : 0.42) && !isH && !isS) continue;
      let w = widths.get(id); if (w == null) { w = measure(n.title); widths.set(id, w); }
      const x = scr.x[i] + 11, y = scr.y[i] - 7;
      const pri = isH ? 1000 : isS ? 900 : matches.has(id) ? 700 : isU ? 500 + n.size * 10 : 100 + n.size * 10;
      cands.push({ id, x, y, w, h: 14, priority: pri + scr.f[i] });
    }
    const ids = pickLabels(cands, { w: W, h: H, pad: 8, max: 18, prev: shown });
    const by = new Map(cands.map(c => [c.id, c]));
    const next = new Set(ids);
    for (const id of ids) {
      let el = labelEls.get(id);
      if (!el) { el = document.createElement('span'); el.className = 'bw-label'; el.textContent = byId.get(id)?.title || ''; labelsEl.append(el); labelEls.set(id, el); }
      const c = by.get(id);
      el.style.transform = `translate(${c.x.toFixed(1)}px, ${c.y.toFixed(1)}px)`;
      el.classList.toggle('lift', id === hovered || id === selected);
      el.classList.toggle('used', !!byId.get(id)?.used);
      el.style.opacity = '1';
    }
    for (const [id, el] of labelEls) {
      if (next.has(id)) continue;
      el.style.opacity = '0';
      if (!byId.has(id)) { el.remove(); labelEls.delete(id); widths.delete(id); }
    }
    shown.clear(); for (const id of ids) shown.add(id);
  }

  // ---- per frame
  const maxDt = opts.maxDt ?? (qs.has('brainmaxdt') ? Number(qs.get('brainmaxdt')) : 0.05); // a long frame (a tab coming back) must not fling the animation forward
  let t = 0, last = 0, raf = 0, dirty = true, frames = 0, ema = 16, slow = 0, fast = 0, noUp = false;
  const lPosTmp = [0, 0, 0], lc = [0, 0, 0];
  function updateNodes(dt) {
    const calm = still();
    let moving = false;
    const warmHdr = light ? 1.15 : 2.3;
    let w = 0;
    const camV = cam.position;
    // dissolved nodes leave the list before it is drawn
    if (order.some(it => it.dying && it.life <= 0)) {
      for (const it of order.filter(x => x.dying && x.life <= 0)) items.delete(it.id);
      order.splice(0, order.length, ...order.filter(x => items.has(x.id)));
      linksDirty = true;
    }
    growNodes(order.length);
    for (let i = 0; i < order.length; i++) {
      const it = order[i], n = it.node, sp = sim.index.get(it.id);
      if (!it.dying && sp != null) { it.pos[0] = sim.pos[sp * 3]; it.pos[1] = sim.pos[sp * 3 + 1]; it.pos[2] = sim.pos[sp * 3 + 2]; }
      // appear / dissolve
      if (it.dying) { it.life = calm ? 0 : Math.max(0, it.life - dt / 0.9); moving = true; }
      else if (it.life < 1) { if (it.delay > 0) it.delay -= dt; else it.life = calm ? 1 : Math.min(1, it.life + dt / 1.1); moving = true; }
      const life = smooth(it.life);
      const hv = it.id === hovered || it.id === selected ? 1 : 0;
      it.hover = calm ? hv : damp(it.hover, hv, 12, dt);
      if (Math.abs(it.hover - hv) > 0.01) moving = true;
      const wm = n.used ? 1 : 0;
      it.warm = calm ? wm : damp(it.warm, wm, 4, dt);
      if (Math.abs(it.warm - wm) > 0.01) moving = true;
      const isMatch = matches.has(it.id) ? 1 : 0, dimT = searchQ && !isMatch ? 0.22 : 1;
      it.dim = calm ? dimT : damp(it.dim, dimT, 8, dt); if (Math.abs(it.dim - dimT) > 0.01) moving = true;
      it.match = calm ? isMatch : damp(it.match, isMatch, 8, dt);
      const pulse = calm ? 0 : it.warm * (0.5 + 0.5 * Math.sin(t * 2.6 + it.phase));
      let rip = 0;
      if (ripT >= 0) { const l = Math.hypot(it.pos[0], it.pos[1], it.pos[2]) || 1, c = (it.pos[0] * U.uRipDir.value.x + it.pos[1] * U.uRipDir.value.y + it.pos[2] * U.uRipDir.value.z) / l; rip = Math.exp(-(((Math.acos(clamp(c, -1, 1)) - ripT * 1.9) * 2.4) ** 2)) * (1 - smoothstep(1.3, 1.9, ripT)); }
      const bst = burstT >= 0 ? Math.exp(-burstT * 2.2) : 0;
      const base = 5.5 + 8 * it.size + 3.5 * it.warm;
      const size = base * life * (1 + 0.55 * it.hover + 0.14 * pulse + 0.5 * rip + 0.35 * bst + 0.4 * it.match);
      // lift: a pointed-at node comes toward the viewer a little
      let lx = it.pos[0], ly = it.pos[1], lz = it.pos[2];
      if (it.hover > 0.001) { const dx = camV.x - lx, dy = camV.y - ly, dz = camV.z - lz, dl = Math.hypot(dx, dy, dz) || 1; lx += dx / dl * 0.05 * it.hover; ly += dy / dl * 0.05 * it.hover; lz += dz / dl * 0.05 * it.hover; }
      if (it.dying) { const o = 1 - life; lx *= 1 + 0.12 * o; ly *= 1 + 0.12 * o; lz *= 1 + 0.12 * o; }
      nPos[w * 3] = lx; nPos[w * 3 + 1] = ly; nPos[w * 3 + 2] = lz;
      nSize[w] = size;
      const base3 = kindColor(n.kind), m = it.warm, hot = warmHdr * (1 + 0.35 * pulse), mt = it.match * 0.6;
      let r = base3.r + (th.warm.r * hot - base3.r) * m, g = base3.g + (th.warm.g * hot - base3.g) * m, b = base3.b + (th.warm.b * hot - base3.b) * m;
      if (mt) { const k = light ? 1 : 2.2; r += (th.warm2.r * k - r) * mt; g += (th.warm2.g * k - g) * mt; b += (th.warm2.b * k - b) * mt; }
      nCol[w * 3] = r; nCol[w * 3 + 1] = g; nCol[w * 3 + 2] = b;
      nFx[w * 3] = Math.max(it.hover, it.match * 0.6); nFx[w * 3 + 1] = life; nFx[w * 3 + 2] = it.dim;
      w++;
    }
    nodeGeo.setDrawRange(0, w);
    for (const k of ['position', 'aColor', 'aSize', 'aFx']) nodeGeo.attributes[k].needsUpdate = true;
    return moving;
  }
  const smoothstep = (a, b, x) => smooth((x - a) / (b - a));

  const lcol = new T.Color();
  function updateLinks(force) {
    if (!force && !linksDirty) return;
    linksDirty = false;
    const links = graph.links, n = links.length;
    growLinks(Math.max(n, 1));
    let w = 0;
    const hi = hovered || selected;
    const a0 = [0, 0, 0], b0 = [0, 0, 0], cc = [0, 0, 0], q = [0, 0, 0];
    for (let li = 0; li < n; li++) {
      const l = links[li], ia = items.get(l.a), ib = items.get(l.b);
      if (!ia || !ib) continue;
      const pa = ia.pos, pb = ib.pos;
      a0[0] = pa[0]; a0[1] = pa[1]; a0[2] = pa[2]; b0[0] = pb[0]; b0[1] = pb[1]; b0[2] = pb[2];
      const len = Math.hypot(b0[0] - a0[0], b0[1] - a0[1], b0[2] - a0[2]);
      const mx = (a0[0] + b0[0]) / 2, my = (a0[1] + b0[1]) / 2, mz = (a0[2] + b0[2]) / 2, ml = Math.hypot(mx, my, mz) || 1;
      cc[0] = mx + mx / ml * len * 0.2; cc[1] = my + my / ml * len * 0.2; cc[2] = mz + mz / ml * len * 0.2;
      const lifeMin = Math.min(smooth(ia.life), smooth(ib.life));
      const both = ia.node.used && ib.node.used, one = ia.node.used || ib.node.used;
      const isHi = hi && (l.a === hi || l.b === hi);
      const dimmed = searchQ && !(matches.has(l.a) || matches.has(l.b));
      const token = l.type === 'tag' ? th.sand : l.type === 'similar' ? th.frost : th.silver;
      let base = (l.type === 'tag' ? 0.06 : l.type === 'similar' ? 0.075 : 0.17) * (0.4 + clamp(l.w * 3, 0, 1) * 0.9);
      if (light) base *= 1.2;
      lcol.copy(token);
      let alpha = base * lifeMin * (dimmed ? 0.25 : 1);
      if (one) { const k = both ? 0.85 : 0.45; lcol.lerp(th.warm, k); lcol.multiplyScalar(light ? 1 : 1 + 0.9 * k); alpha *= 1 + 1.4 * k; }
      if (isHi) { lcol.lerp(light ? th.ink : th.warm2, 0.55); alpha = Math.max(alpha * 2.6, 0.5 * lifeMin); }
      for (let s = 0; s <= ARC; s++) {
        const tt = s / ARC, u = 1 - tt, w0 = u * u, w1 = 2 * u * tt, w2 = tt * tt;
        q[0] = w0 * a0[0] + w1 * cc[0] + w2 * b0[0]; q[1] = w0 * a0[1] + w1 * cc[1] + w2 * b0[1]; q[2] = w0 * a0[2] + w1 * cc[2] + w2 * b0[2];
        const vi = w * (ARC + 1) + s, env = 0.3 + 0.7 * Math.sin(Math.PI * tt);
        lPos[vi * 3] = q[0]; lPos[vi * 3 + 1] = q[1]; lPos[vi * 3 + 2] = q[2];
        lCol[vi * 4] = lcol.r; lCol[vi * 4 + 1] = lcol.g; lCol[vi * 4 + 2] = lcol.b; lCol[vi * 4 + 3] = Math.min(1, alpha * env);
      }
      w++;
    }
    linkCount = w;
    linkGeo.setDrawRange(0, w * ARC * 2);
    linkGeo.attributes.position.needsUpdate = true; linkGeo.attributes.aCol.needsUpdate = true;
  }

  const tp = [0, 0, 0];
  function updateParticles(dt) {
    let any = false;
    const target = thinking && !still() ? 1 : 0;
    gain = damp(gain, target, thinking ? 3 : 2.2, dt);
    if (gain < 0.004 && !target) gain = 0;
    let k = 0;
    if (gain > 0) {
      if (routesDirty) buildRoutes();
      for (const tr of trails) {
        if (!tr.route) continue;
        const ph = ((t / tr.period) + tr.phase) % 1;
        for (let s = 0; s < 7 && k < TRAIL_SLOTS; s++) {
          const tt = ph - s * 0.022;
          if (tt < 0) { pA[k] = 0; k++; continue; }
          routeAt(tr.route, tt, tp);
          pPos[k * 3] = tp[0]; pPos[k * 3 + 1] = tp[1]; pPos[k * 3 + 2] = tp[2];
          const arrive = ph > 0.92 ? (1 - ph) / 0.08 : 1;
          pA[k] = (s === 0 ? 1 : 0.8 * (1 - s / 7) ** 1.2) * gain * arrive;
          pS[k] = s === 0 ? 15 : 11 - s * 1.3;
          k++;
        }
      }
      any = true;
    }
    for (; k < TRAIL_SLOTS; k++) pA[k] = 0;
    // sparks from a finished run
    let j = TRAIL_SLOTS;
    if (burstT >= 0) {
      for (const s of sparks) {
        const age = burstT - s.born;
        if (j >= PN) break;
        if (age < 0 || age > s.life) { pA[j++] = 0; continue; }
        const d = s.speed * (1 - Math.exp(-age * 2.4)) / 2.4 * 1.6;
        pPos[j * 3] = s.dx * d; pPos[j * 3 + 1] = s.dy * d; pPos[j * 3 + 2] = s.dz * d;
        pA[j] = (1 - age / s.life) ** 1.5; pS[j] = 6; j++; any = true;
      }
    }
    for (; j < PN; j++) pA[j] = 0;
    partGeo.attributes.position.needsUpdate = true; partGeo.attributes.aA.needsUpdate = true; partGeo.attributes.aS.needsUpdate = true;
    return any;
  }

  function frame(now) {
    raf = 0;
    if (closed || !visible) return;
    const dt = last ? Math.min(maxDt, (now - last) / 1000) : 0.016, raw = last ? now - last : 16;
    last = now;
    const busy = tick(dt, now);
    render();
    if (frames > 24 && !still() && raw < 250) watchdog(raw);
    frames++;
    if (!still() || busy || dirty) { dirty = false; raf = requestAnimationFrame(frame); }
  }
  // Everything that moves, for one step of dt seconds. Returns whether anything is still moving.
  function tick(dt, now) {
    const calm = still();
    if (!calm) t += dt;
    U.uTime.value = t;
    U.uBreath.value = calm ? 0 : 1;

    // layout: a tick or two per frame until it cools
    let busy = false;
    if (!sim.settled) { sim.step(); if (graph.nodes.length < 160) sim.step(); busy = true; linksDirty = routesDirty = true; }

    // camera
    const idle = now - lastTouch > 4000 && !dragging && !selected && !searchQ && sbox.hidden;
    if (!calm && idle && !flying) goal.theta += 0.05 * dt;
    const lam = flying ? 3.4 : 9;
    for (const k of ['theta', 'phi', 'dist', 'tx', 'ty', 'tz', 'shift']) view[k] = calm ? goal[k] : damp(view[k], goal[k], lam, dt);
    const settledCam = Math.abs(goal.theta - view.theta) + Math.abs(goal.phi - view.phi) + Math.abs(goal.dist - view.dist) + Math.abs(goal.tx - view.tx) + Math.abs(goal.ty - view.ty) + Math.abs(goal.tz - view.tz) + Math.abs(goal.shift - view.shift) < 2e-3;
    if (flying && settledCam) flying = false;
    if (!settledCam) busy = true;
    if (ripT >= 0) { ripT += dt; busy = true; if (ripT > 2) ripT = -1; }
    if (burstT >= 0) { burstT += dt; busy = true; if (burstT > 2.4) burstT = -1; }
    flare = damp(flare, 0, 2.6, dt);
    placeCamera();

    // the core: brighter and beating while an engine works
    const beat = thinking && !calm ? 0.5 + 0.5 * Math.sin(t * 5.2) : 0;
    coreU.uPulse.value = damp(coreU.uPulse.value, thinking ? 0.35 + 0.65 * beat : 0, 5, dt) + flare * 1.3;
    const gs = 0.95 + 0.3 * coreU.uPulse.value + 1.1 * flare;
    glow.scale.set(gs, gs, 1);
    glowMat.opacity = (light ? 0.5 : 0.38) * (0.7 + 0.5 * coreU.uPulse.value) + flare * 0.3;
    if (thinking || flare > 0.01 || coreU.uPulse.value > 0.01) busy = true;
    if (ripT >= 0) U.uRipT.value = ripT; else U.uRipT.value = -1;
    U.uBurst.value = burstT;

    // warm bleed from the nodes in use onto the grain nearest to them
    const glowArr = U.uGlow.value;
    let gi = 0;
    for (const id of usedIds) { if (gi >= GLOW_SLOTS) break; const it = items.get(id); if (!it) continue; glowArr[gi++].set(it.pos[0], it.pos[1], it.pos[2], 0.45 * it.warm * smooth(it.life)); }
    for (; gi < GLOW_SLOTS; gi++) glowArr[gi].w = 0;

    if (updateNodes(dt)) busy = true;
    updateLinks(false);
    if (updateParticles(dt)) busy = true;
    if (linkDirtyFromAnim()) { linksDirty = true; }
    project();
    updateLabels(false);
    return busy;
  }
  // Links follow nodes that grow in or dissolve, and the highlight of the pointed-at node.
  let lastHi = null, lastQ = '';
  function linkDirtyFromAnim() {
    const hi = hovered || selected;
    const changed = hi !== lastHi || searchQ !== lastQ || order.some(it => it.life < 1 || it.dying) || order.some(it => Math.abs(it.warm - (it.node.used ? 1 : 0)) > 0.01);
    lastHi = hi; lastQ = searchQ;
    return changed;
  }
  function render() {
    if (composer) { finish.uniforms.uTime.value = t; composer.render(); } else renderer.render(scene, cam);
  }
  function kick() { dirty = true; if (!raf && !closed && visible) raf = requestAnimationFrame(frame); }

  // Frame-time watchdog: a slow device loses bloom, then grain, before it loses smoothness.
  let calmFrames = 0;
  function watchdog(ms) {
    ema += (ms - ema) * 0.08;
    if (pinned) return;
    if (ema > 21) { slow++; fast = 0; } else if (ema < 12) { fast++; slow = 0; } else { slow = fast = 0; }
    if (slow > 45 && tier < TIERS.length - 1) { setTier(tier + 1); noUp = true; slow = 0; }
    else if (fast > 360 && tier > 0 && !noUp) { setTier(tier - 1); fast = 0; }
    calmFrames++;
  }
  function setTier(i) {
    tier = i; const q = TIERS[i];
    ratio = Math.min(devicePixelRatio || 1, q.dpr, 1.5);
    renderer.setPixelRatio(ratio);
    buildShell(q.points);
    if (!q.bloom) dropPost(); else buildPost();
    applyTheme(); resize(); ema = 16;
  }

  function resize() {
    W = host.clientWidth || 1; H = host.clientHeight || 1;
    renderer.setSize(W, H, false);
    composer?.setSize(W, H);
    cam.aspect = W / H; cam.updateProjectionMatrix();
    const was = fit;
    fit = fitDist();
    if (Math.abs(goal.dist - was) < 1e-3) { goal.dist = fit; if (Math.abs(view.dist - was) < 1e-3) view.dist = fit; }
    else goal.dist = clamp(goal.dist, 1.15, fit * 2.4);
    U.uPx.value = ratio * Math.min(1.6, Math.max(0.6, H / 800));
    if (finish) finish.uniforms.uAspect.value = W / H;
    kick();
  }

  // ---- lifecycle
  const ro = new ResizeObserver(() => resize()); ro.observe(host);
  const io = new IntersectionObserver(es => { visible = es.some(e => e.isIntersecting) && !document.hidden; if (visible) { last = 0; kick(); } });
  io.observe(host);
  const onVis = () => { visible = !document.hidden; if (visible) { last = 0; kick(); } };
  document.addEventListener('visibilitychange', onVis);
  const onLost = e => { e.preventDefault(); note(copy.loadError); };
  canvas.addEventListener('webglcontextlost', onLost);

  buildShell(TIERS[tier].points);
  buildPost();
  if (!composer) U.uSrgb.value = 1;
  applyTheme();
  resize();
  goal.dist = view.dist = fit;
  Object.assign(goal, { theta: view.theta, phi: view.phi });
  growNodes(64); growLinks(128);
  if (opts.data) setData(opts.data);
  kick();

  const api = {
    mode: '3d',
    setData, setUsed, ripple, burst, search, resize,
    focus: id => { if (items.has(id)) selectNode(id); },
    // Where a node is on screen now, in host pixels (for tests, and for anything that points at a node).
    screenOf(id) { const i = order.findIndex(it => it.id === id); return i < 0 ? null : [scr.x[i], scr.y[i]]; },
    ids() { return order.filter(it => !it.dying).map(it => it.id); },
    setThinking(b) { b = !!b; if (b === thinking) return; thinking = b; if (b) { burstSeed++; } rebuildTrails(); kick(); },
    busy(b) { this.setThinking(b); },
    retheme() { readTheme(); labelEls.forEach(el => el.remove()); labelEls.clear(); widths.clear(); applyTheme(); linksDirty = true; kick(); },
    pause(b) { paused = !!b; if (paused) { trails = []; ripT = burstT = -1; snapCamera(); } else rebuildTrails(); kick(); },
    nodeCount: () => graph.nodes.length,
    // Runs the animation forward without waiting for frames (tests and screenshots on slow software GL).
    advance(sec) { const n = Math.ceil(sec * 30); for (let i = 0; i < n; i++) tick(1 / 30, performance.now()); render(); },
    stats: () => ({ tier, points: TIERS[tier].points, ratio, nodes: graph.nodes.length, links: linkCount, settled: sim.settled, bloom: !!composer, ema }),
    close() {
      closed = true; if (raf) cancelAnimationFrame(raf); clearTimeout(searchTimer);
      ro.disconnect(); io.disconnect(); document.removeEventListener('visibilitychange', onVis); window.removeEventListener('keydown', onKey, true);
      canvas.removeEventListener('webglcontextlost', onLost);
      for (const g of [shellGeo, coreGeo, nodeGeo, linkGeo, partGeo]) g?.dispose();
      for (const m of [shellMat, coreMat, nodeMat, linkMat, partMat, glowMat]) m.dispose();
      glowTex.dispose(); composer?.dispose?.(); renderer.dispose(); renderer.forceContextLoss();
      host.replaceChildren();
    },
  };
  if (qs.has('brainq')) window.__brain = api;
  return api;
}
