// Second-brain graph, drawn as a lattice sphere: memory files are points, shared distinctive terms are
// lines (from /agent/memory/graph), a faint lattice of latitude and longitude rings holds them, and a warm
// core sits at the centre. Imported only when the graph opens. Uses the vendored three.js (offline, same
// origin) when WebGL works; otherwise a flat SVG with the same lattice and core.
//
// Motion, all of it calm: a slow idle turn, a breathing glow, a core that brightens and pulses while an
// engine works (busy(true)), a ripple that runs outward through the points when a reply lands (ripple()),
// a burst when a run finishes (burst()), and a point that lifts when pointed at. reduced, or pause(true),
// holds the sphere still. The loop stops while the tab is hidden or the graph is off screen, and the pixel
// ratio is capped at 1.5.

// Any CSS colour the host uses (hex, rgb, oklch...) to [r, g, b] in 0..1.
function rgb(css) {
  const c = document.createElement('canvas');
  c.width = c.height = 1;
  const x = c.getContext('2d', { willReadFrequently: true }); // CPU canvas: no GPU readback
  x.fillStyle = '#808080';
  x.fillStyle = css;
  x.fillRect(0, 0, 1, 1);
  const d = x.getImageData(0, 0, 1, 1).data;
  return [d[0] / 255, d[1] / 255, d[2] / 255];
}
const hex = ([r, g, b]) => '#' + [r, g, b].map(v => Math.round(v * 255).toString(16).padStart(2, '0')).join('');

// Fruchterman-Reingold with a fixed seed: the same memory always draws the same picture.
export function layout(nodes, edges, dims = 3, iterations = 240) {
  const n = nodes.length, pos = new Float32Array(n * 3), disp = new Float32Array(n * 3);
  let seed = 7;
  const rnd = () => (seed = (seed * 16807) % 2147483647) / 2147483647 - 0.5;
  for (let i = 0; i < n * 3; i++) pos[i] = dims === 3 || i % 3 !== 2 ? rnd() * 2 : 0;
  const k = Math.cbrt(8 / Math.max(n, 1));
  let t = 0.25;
  for (let it = 0; it < iterations; it++) {
    disp.fill(0);
    for (let i = 0; i < n; i++) for (let j = i + 1; j < n; j++) {
      const dx = pos[i * 3] - pos[j * 3], dy = pos[i * 3 + 1] - pos[j * 3 + 1], dz = pos[i * 3 + 2] - pos[j * 3 + 2];
      const f = (k * k) / (dx * dx + dy * dy + dz * dz + 1e-6);
      disp[i * 3] += dx * f; disp[i * 3 + 1] += dy * f; disp[i * 3 + 2] += dz * f;
      disp[j * 3] -= dx * f; disp[j * 3 + 1] -= dy * f; disp[j * 3 + 2] -= dz * f;
    }
    for (const e of edges) {
      const a = e.a * 3, b = e.b * 3;
      const dx = pos[a] - pos[b], dy = pos[a + 1] - pos[b + 1], dz = pos[a + 2] - pos[b + 2];
      const f = (Math.sqrt(dx * dx + dy * dy + dz * dz) / k) * (1 + 3 * Math.max(0, (e.w ?? 0.17) - 0.17));
      disp[a] -= dx * f; disp[a + 1] -= dy * f; disp[a + 2] -= dz * f;
      disp[b] += dx * f; disp[b + 1] += dy * f; disp[b + 2] += dz * f;
    }
    for (let i = 0; i < n * 3; i++) disp[i] -= pos[i] * 0.08 * n * k; // gentle pull to the centre
    for (let i = 0; i < n; i++) {
      const dx = disp[i * 3], dy = disp[i * 3 + 1], dz = disp[i * 3 + 2];
      const len = Math.sqrt(dx * dx + dy * dy + dz * dz) + 1e-9, s = Math.min(len, t) / len;
      pos[i * 3] += dx * s; pos[i * 3 + 1] += dy * s; if (dims === 3) pos[i * 3 + 2] += dz * s;
    }
    t *= 0.985;
  }
  let r = 1e-9;
  for (let i = 0; i < n; i++) r = Math.max(r, Math.hypot(pos[i * 3], pos[i * 3 + 1], pos[i * 3 + 2]));
  for (let i = 0; i < n * 3; i++) pos[i] /= r;
  return pos;
}

export function webglOk() {
  try {
    const c = document.createElement('canvas');
    return !!(c.getContext('webgl2') || c.getContext('webgl'));
  } catch { return false; }
}

// Colours come from the page's own tokens (tempo-theme.css), so the graph follows light and dark.
function palette(host) {
  const s = getComputedStyle(host), v = (n, f) => s.getPropertyValue(n).trim() || f;
  const bg = rgb(v('--bg', '#1b1a1b'));
  return {
    paper: 0.2126 * bg[0] + 0.7152 * bg[1] + 0.0722 * bg[2] > 0.5,
    core: rgb(v('--frost', '#d9dde2')), current: rgb(v('--frost', '#d9dde2')), memory: rgb(v('--ink', '#efe9dc')),
    pack: rgb(v('--silver', '#a9aaad')), used: rgb(v('--warm', '#ff5a2a')), edge: rgb(v('--silver', '#a9aaad')),
    warm: rgb(v('--warm', '#ff5a2a')), ring: rgb(v('--line3', '#77787c')),
  };
}

const sizeOf = (node, maxChunks, used) => 5 + 11 * Math.sqrt((node.chunks || 1) / maxChunks) + (used ? 4 : 0);

// opts: {used: Set(src), onPick(src), reduced: bool, flatLabel: string}.
// Returns {mode, close(), resize(), retheme(), busy(bool), ripple(), burst(), pause(bool)}.
export async function openGraph(host, g, opts) {
  const used = opts.used || new Set();
  const pal = palette(host);
  const maxChunks = g.nodes.reduce((m, n) => Math.max(m, n.chunks || 1), 1);
  const tip = document.createElement('div');
  tip.className = 'mg-tip';
  tip.hidden = true;
  host.append(tip);
  if (webglOk()) {
    try { return await threeView(host, g, opts, used, pal, maxChunks, tip); } catch { /* fall through to the flat view */ }
  }
  return flatView(host, g, opts, used, pal, maxChunks, tip);
}

// Rings of latitude and longitude on the unit sphere, as line segments.
function latticeLines() {
  const out = [], seg = 72, ring = (f) => { for (let i = 0; i < seg; i++) out.push(...f(i / seg * 2 * Math.PI), ...f((i + 1) / seg * 2 * Math.PI)); };
  for (const lat of [-60, -30, 0, 30, 60].map(d => d * Math.PI / 180)) ring(a => [Math.cos(lat) * Math.cos(a), Math.sin(lat), Math.cos(lat) * Math.sin(a)]);
  for (let m = 0; m < 6; m++) { const ph = m * Math.PI / 6; ring(a => [Math.cos(a) * Math.cos(ph), Math.sin(a), Math.cos(a) * Math.sin(ph)]); }
  return new Float32Array(out);
}

async function threeView(host, g, opts, used, pal, maxChunks, tip) {
  const T = await import(new URL('./vendor/three.module.min.js', import.meta.url).href);
  const n = g.nodes.length;
  const pos = n ? layout(g.nodes, g.edges, 3) : new Float32Array(0);
  const canvas = document.createElement('canvas');
  canvas.className = 'mg-canvas';
  host.prepend(canvas);
  const renderer = new T.WebGLRenderer({ canvas, antialias: true, alpha: true, powerPreference: 'low-power' });
  const ratio = Math.min(window.devicePixelRatio || 1, 1.5);
  renderer.setPixelRatio(ratio);
  const scene = new T.Scene(), cam = new T.PerspectiveCamera(50, 1, 0.01, 100), group = new T.Group();
  cam.position.set(0, 0, 2.6);
  scene.add(group);
  group.rotation.x = 0.3; // tipped a little, so the rings read as rings
  const col3 = c => new T.Vector3(c[0], c[1], c[2]);
  const U = {
    uTime: { value: 0 }, uRip: { value: -100 }, uBst: { value: -100 }, uHov: { value: 0 }, uHovPos: { value: new T.Vector3(9, 9, 9) },
    uBreath: { value: 1 }, uCore: { value: 0 }, uWarm: { value: col3(pal.warm) }, uRatio: { value: ratio },
  };
  const blending = () => (pal.paper ? T.NormalBlending : T.AdditiveBlending);

  // lattice rings and the lines between memories
  const lgeo = new T.BufferGeometry();
  lgeo.setAttribute('position', new T.BufferAttribute(latticeLines(), 3));
  const lmat = new T.LineBasicMaterial({ color: new T.Color(...pal.ring), transparent: true, opacity: 0.2 });
  group.add(new T.LineSegments(lgeo, lmat));
  let egeo = null, emat = null, geo = null, mat = null, col = null, size = null;
  if (n) {
    col = new Float32Array(n * 3); size = new Float32Array(n);
    const seed = new Float32Array(n);
    g.nodes.forEach((node, i) => { size[i] = sizeOf(node, maxChunks, used.has(node.src) || !!node.used) * ratio; seed[i] = (i * 0.6180339887) % 1; });
    const paint = () => g.nodes.forEach((node, i) => { const u = used.has(node.src) || !!node.used; col.set(u ? pal.used : (pal[node.kind] || pal.memory), i * 3); });
    paint();
    geo = new T.BufferGeometry();
    geo.setAttribute('position', new T.BufferAttribute(pos, 3));
    geo.setAttribute('acolor', new T.BufferAttribute(col, 3));
    geo.setAttribute('asize', new T.BufferAttribute(size, 1));
    geo.setAttribute('aseed', new T.BufferAttribute(seed, 1));
    mat = new T.ShaderMaterial({
      transparent: true, depthWrite: false, blending: blending(), uniforms: U,
      vertexShader: 'attribute vec3 acolor; attribute float asize; attribute float aseed;' +
        'uniform float uTime, uRip, uHov, uBreath; uniform vec3 uHovPos, uWarm; varying vec3 vc; varying float vw;' +
        'void main(){' +
        ' float r = length(position); float since = uTime - uRip;' +
        ' float w = since > 0.0 && since < 2.6 ? exp(-pow((r - since * 1.15) / 0.17, 2.0)) * (1.0 - since / 2.6) : 0.0;' +
        ' float hov = distance(position, uHovPos) < 0.0005 ? uHov : 0.0;' +
        ' float br = 1.0 + 0.07 * uBreath * sin(uTime * 1.3 + aseed * 6.2832);' +
        ' vc = mix(acolor, uWarm, min(1.0, w * 0.8 + hov * 0.35)); vw = w;' +
        ' vec4 mv = modelViewMatrix * vec4(position, 1.0);' +
        ' gl_PointSize = asize * br * (1.0 + w * 1.2 + hov * 0.75) * (3.1 / -mv.z); gl_Position = projectionMatrix * mv; }',
      fragmentShader: 'varying vec3 vc; varying float vw; void main(){ float d = length(gl_PointCoord - 0.5);' +
        ' if (d > 0.5) discard; gl_FragColor = vec4(vc, smoothstep(0.5, 0.34, d) * (0.9 + 0.1 * vw)); }',
    });
    group.add(new T.Points(geo, mat));
    const ep = new Float32Array(g.edges.length * 6);
    g.edges.forEach((e, i) => { ep.set(pos.subarray(e.a * 3, e.a * 3 + 3), i * 6); ep.set(pos.subarray(e.b * 3, e.b * 3 + 3), i * 6 + 3); });
    egeo = new T.BufferGeometry();
    egeo.setAttribute('position', new T.BufferAttribute(ep, 3));
    emat = new T.LineBasicMaterial({ color: new T.Color(...pal.edge), transparent: true, opacity: pal.paper ? 0.4 : 0.5 });
    group.add(new T.LineSegments(egeo, emat));
    var repaint = () => { paint(); geo.attributes.acolor.needsUpdate = true; };
  }

  // the warm core: a soft glow that brightens and pulses while an engine works
  const cgeo = new T.BufferGeometry();
  cgeo.setAttribute('position', new T.BufferAttribute(new Float32Array([0, 0, 0]), 3));
  const cmat = new T.ShaderMaterial({
    transparent: true, depthWrite: false, depthTest: false, blending: blending(), uniforms: U,
    vertexShader: 'uniform float uTime, uCore, uBreath, uRatio, uBst; void main(){ vec4 mv = modelViewMatrix * vec4(position, 1.0);' +
      ' float since = uTime - uBst; float kick = since > 0.0 && since < 0.9 ? (1.0 - since / 0.9) : 0.0;' +
      ' float pulse = 1.0 + (0.05 * uBreath + 0.16 * uCore) * sin(uTime * (1.4 + uCore * 3.2)) + kick * 0.5;' +
      ' gl_PointSize = (64.0 + 54.0 * uCore) * pulse * uRatio * (2.6 / -mv.z); gl_Position = projectionMatrix * mv; }',
    fragmentShader: 'uniform vec3 uWarm; uniform float uCore, uBst, uTime; void main(){ float d = length(gl_PointCoord - 0.5) * 2.0;' +
      ' if (d > 1.0) discard; float since = uTime - uBst; float kick = since > 0.0 && since < 0.9 ? (1.0 - since / 0.9) : 0.0;' +
      ' float glow = exp(-d * d * 7.0); float core = smoothstep(0.2, 0.0, d);' +
      ' gl_FragColor = vec4(mix(uWarm, vec3(1.0), core * 0.55), (glow * (0.5 + 0.4 * uCore + 0.4 * kick) + core * 0.6)); }',
  });
  group.add(new T.Points(cgeo, cmat));

  // a burst: a shell of particles that runs outward and fades, when a run or flow finishes
  const bn = 140, bdir = new Float32Array(bn * 3);
  for (let i = 0; i < bn; i++) { const y = 1 - 2 * (i + 0.5) / bn, r = Math.sqrt(1 - y * y), a = i * 2.399963; bdir.set([r * Math.cos(a), y, r * Math.sin(a)], i * 3); }
  const bgeo = new T.BufferGeometry();
  bgeo.setAttribute('position', new T.BufferAttribute(bdir, 3));
  const bmat = new T.ShaderMaterial({
    transparent: true, depthWrite: false, blending: blending(), uniforms: U,
    vertexShader: 'uniform float uTime, uBst, uRatio; varying float va; void main(){ float t = (uTime - uBst) / 1.3; va = t > 0.0 && t < 1.0 ? pow(1.0 - t, 2.0) : 0.0;' +
      ' vec3 p = position * (0.05 + 1.25 * (1.0 - pow(1.0 - clamp(t, 0.0, 1.0), 3.0))); vec4 mv = modelViewMatrix * vec4(p, 1.0);' +
      ' gl_PointSize = 5.0 * uRatio * (2.6 / -mv.z); gl_Position = va > 0.0 ? projectionMatrix * mv : vec4(2.0, 2.0, 2.0, 1.0); }',
    fragmentShader: 'uniform vec3 uWarm; varying float va; void main(){ float d = length(gl_PointCoord - 0.5); if (d > 0.5) discard; gl_FragColor = vec4(uWarm, va * smoothstep(0.5, 0.2, d)); }',
  });
  group.add(new T.Points(bgeo, bmat));

  // labels for the files used this turn, placed in screen space each frame
  const nodeLabels = new Map();
  g.nodes.forEach((node, i) => {
    if (!(used.has(node.src) || node.used) || nodeLabels.size >= 24) return;
    const el = document.createElement('span');
    el.className = 'mg-label';
    el.textContent = node.name;
    host.append(el);
    nodeLabels.set(i, el);
  });
  const v = new T.Vector3();
  const screen = i => {
    v.set(pos[i * 3], pos[i * 3 + 1], pos[i * 3 + 2]).applyMatrix4(group.matrixWorld).project(cam);
    return [(v.x + 1) / 2 * host.clientWidth, (1 - v.y) / 2 * host.clientHeight, v.z];
  };
  const place = () => nodeLabels.forEach((el, i) => { const [x, y] = screen(i); el.style.transform = `translate(${x + 9}px, ${y - 8}px)`; });

  // ---- the loop: one rAF while there is motion to show, none while hidden, off screen or held still
  let raf = 0, last = 0, dirty = true, visible = true, closed = false, paused = false, busy = false, dragging = false;
  let hover = -1, hoverAmt = 0, coreAmt = 0, clock = 0;
  const still = () => !!opts.reduced || paused;
  const ease = (a, b, dt, k) => a + (b - a) * (1 - Math.exp(-dt * k));
  function frame(t) {
    raf = 0;
    if (closed || !visible || document.hidden) return;
    const dt = Math.min(0.05, last ? (t - last) / 1000 : 0.016);
    last = t;
    const live = !still();
    if (live) {
      clock += dt;
      if (!dragging) group.rotation.y += dt * 0.1;
      U.uBreath.value = 1;
      lmat.opacity = 0.2 + 0.07 * Math.sin(clock * 0.9);
    } else { U.uBreath.value = 0; U.uRip.value = -100; U.uBst.value = -100; }
    U.uTime.value = clock;
    coreAmt = live ? ease(coreAmt, busy ? 1 : 0, dt, 3.5) : (busy ? 0.6 : 0);
    U.uCore.value = coreAmt;
    const target = hover >= 0 ? 1 : 0;
    hoverAmt = live ? ease(hoverAmt, target, dt, 12) : target;
    U.uHov.value = hoverAmt;
    renderer.render(scene, cam);
    place();
    dirty = false;
    if (live) raf = requestAnimationFrame(frame);
    else last = 0;
  }
  const kick = () => { dirty = true; if (!raf && !closed) raf = requestAnimationFrame(frame); };
  const resize = () => {
    const w = host.clientWidth || 1, h = host.clientHeight || 1;
    renderer.setSize(w, h, false);
    cam.aspect = w / h;
    // keep the whole sphere in view on a tall or narrow stage
    cam.position.z = Math.max(2.1, Math.min(8, 2.6 * Math.max(1, 1 / cam.aspect) * 0.92)) * (zoom || 1);
    cam.updateProjectionMatrix();
    kick();
  };
  let zoom = 1;
  const ro = new ResizeObserver(resize);
  ro.observe(host);
  const io = new IntersectionObserver(es => { visible = es.some(e => e.isIntersecting); if (visible) { last = 0; kick(); } });
  io.observe(host);
  const onVis = () => { visible = !document.hidden; if (visible) { last = 0; kick(); } };
  document.addEventListener('visibilitychange', onVis);

  const pick = (px, py) => {
    if (!n) return -1;
    group.updateMatrixWorld();
    let best = -1, bd = 14 * 14;
    for (let i = 0; i < n; i++) {
      const [x, y, z] = screen(i);
      if (z > 1) continue;
      const d = (x - px) ** 2 + (y - py) ** 2;
      if (d < bd) { bd = d; best = i; }
    }
    return best;
  };
  const setHover = i => {
    if (i === hover) return;
    if (hover >= 0) nodeLabels.get(hover)?.classList.remove('lift');
    hover = i;
    if (i >= 0) { U.uHovPos.value.set(pos[i * 3], pos[i * 3 + 1], pos[i * 3 + 2]); nodeLabels.get(i)?.classList.add('lift'); hoverAmt = still() ? 1 : 0; }
    kick();
  };
  let drag = null;
  canvas.addEventListener('pointerdown', e => { drag = { x: e.clientX, y: e.clientY, moved: false }; dragging = true; canvas.setPointerCapture(e.pointerId); });
  canvas.addEventListener('pointermove', e => {
    const r = canvas.getBoundingClientRect();
    if (drag) {
      const dx = e.clientX - drag.x, dy = e.clientY - drag.y;
      if (Math.abs(dx) + Math.abs(dy) > 2) drag.moved = true;
      group.rotation.y += dx * 0.006;
      group.rotation.x = Math.max(-1.4, Math.min(1.4, group.rotation.x + dy * 0.006));
      drag.x = e.clientX; drag.y = e.clientY;
      tip.hidden = true;
      kick();
      return;
    }
    const i = pick(e.clientX - r.left, e.clientY - r.top);
    setHover(i);
    tip.hidden = i < 0;
    canvas.style.cursor = i < 0 ? '' : 'pointer';
    if (i >= 0) { tip.textContent = g.nodes[i].name; tip.style.transform = `translate(${e.clientX - r.left + 12}px, ${e.clientY - r.top + 12}px)`; }
  });
  canvas.addEventListener('pointerleave', () => { setHover(-1); tip.hidden = true; });
  canvas.addEventListener('pointerup', e => {
    const r = canvas.getBoundingClientRect(), wasDrag = drag?.moved;
    drag = null; dragging = false;
    if (wasDrag) return;
    const i = pick(e.clientX - r.left, e.clientY - r.top);
    if (i >= 0) opts.onPick?.(g.nodes[i].src);
  });
  canvas.addEventListener('wheel', e => {
    e.preventDefault();
    zoom = Math.max(0.5, Math.min(3, zoom * (1 + e.deltaY * 0.001)));
    resize();
  }, { passive: false });
  resize();

  return {
    mode: '3d',
    resize,
    // the page's colours changed (light, dark): take them
    retheme() {
      Object.assign(pal, palette(host));
      lmat.color.setRGB(...pal.ring);
      if (emat) { emat.color.setRGB(...pal.edge); emat.opacity = pal.paper ? 0.4 : 0.5; }
      U.uWarm.value.set(...pal.warm);
      for (const m of [mat, cmat, bmat]) if (m) { m.blending = blending(); m.needsUpdate = true; }
      if (n) repaint();
      kick();
    },
    busy(b) { busy = !!b; kick(); },
    ripple() { if (still()) return; U.uRip.value = clock; kick(); },
    burst() { if (still()) return; U.uBst.value = clock; kick(); },
    pause(p) { paused = !!p; last = 0; kick(); },
    close() {
      closed = true;
      if (raf) cancelAnimationFrame(raf);
      ro.disconnect(); io.disconnect();
      document.removeEventListener('visibilitychange', onVis);
      for (const x of [geo, mat, egeo, emat, lgeo, lmat, cgeo, cmat, bgeo, bmat]) x?.dispose();
      renderer.dispose(); renderer.forceContextLoss();
      host.replaceChildren();
    },
  };
}

// No WebGL: the same lattice, points and core as a flat SVG. Motion is CSS (transform and opacity only).
function flatView(host, g, opts, used, pal, maxChunks, tip) {
  const NS = 'http://www.w3.org/2000/svg', mk = (t, a) => { const e = document.createElementNS(NS, t); for (const k in a) e.setAttribute(k, a[k]); return e; };
  const pos = layout(g.nodes, g.edges, 2);
  let span = 2.6;
  const svg = mk('svg', { class: 'mg-svg', viewBox: `${-span / 2} ${-span / 2} ${span} ${span}`, role: 'img', 'aria-label': opts.flatLabel || '' });
  const ring = mk('g', { class: 'mg-ring', fill: 'none', stroke: hex(pal.ring), 'stroke-opacity': '0.5', 'stroke-width': '1', 'vector-effect': 'non-scaling-stroke' });
  ring.append(mk('circle', { cx: 0, cy: 0, r: 1, 'vector-effect': 'non-scaling-stroke' }));
  for (const f of [0.5, 0.87]) ring.append(mk('ellipse', { cx: 0, cy: 0, rx: 1, ry: f * 0.3, 'vector-effect': 'non-scaling-stroke' }), mk('ellipse', { cx: 0, cy: 0, rx: f, ry: 1, 'vector-effect': 'non-scaling-stroke' }));
  svg.append(ring);
  const lines = mk('g', { stroke: hex(pal.edge), 'stroke-opacity': '0.5' });
  for (const e of g.edges) {
    lines.append(mk('line', { x1: pos[e.a * 3], y1: pos[e.a * 3 + 1], x2: pos[e.b * 3], y2: pos[e.b * 3 + 1], 'vector-effect': 'non-scaling-stroke' }));
  }
  svg.append(lines);
  const unit = span / Math.max(host.clientWidth || 600, 1);
  const dots = [];
  g.nodes.forEach((node, i) => {
    const u = used.has(node.src) || !!node.used;
    const c = mk('circle', { class: 'mg-pt', cx: pos[i * 3], cy: pos[i * 3 + 1], r: (sizeOf(node, maxChunks, u) / 2) * unit, fill: hex(u ? pal.used : (pal[node.kind] || pal.memory)), tabindex: '0', role: 'button', 'aria-label': node.name });
    c.style.setProperty('--d', (Math.hypot(pos[i * 3], pos[i * 3 + 1]) * 0.9).toFixed(2));
    const t = mk('title', {});
    t.textContent = node.name;
    c.append(t);
    c.addEventListener('click', () => opts.onPick?.(node.src));
    c.addEventListener('keydown', e => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); opts.onPick?.(node.src); } });
    dots.push(c);
    svg.append(c);
  });
  svg.append(mk('circle', { class: 'mg-aura', cx: 0, cy: 0, r: 0.12, fill: hex(pal.warm) }), mk('circle', { class: 'mg-core', cx: 0, cy: 0, r: 0.045, fill: hex(pal.warm) }));
  svg.addEventListener('wheel', e => {
    e.preventDefault();
    span = Math.max(0.4, Math.min(6, span * (1 + e.deltaY * 0.001)));
    svg.setAttribute('viewBox', `${-span / 2} ${-span / 2} ${span} ${span}`);
  }, { passive: false });
  host.prepend(svg);
  const replay = cls => { svg.classList.remove(cls); void svg.getBoundingClientRect(); svg.classList.add(cls); setTimeout(() => svg.classList.remove(cls), 1800); };
  return {
    mode: 'flat',
    resize() {},
    retheme() {},
    busy(b) { svg.classList.toggle('busy', !!b); },
    ripple() { replay('rip'); },
    burst() { replay('bst'); },
    pause(p) { svg.classList.toggle('held', !!p); },
    close() { host.replaceChildren(); },
  };
}
