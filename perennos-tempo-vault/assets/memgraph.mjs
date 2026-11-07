// Second-brain graph: memory files as nodes, shared distinctive terms as edges (from /agent/memory/graph).
// Imported only when the graph opens, so the chat path carries none of it. Uses the vendored three.js
// (offline, same origin) when WebGL works; otherwise a flat SVG. Both draw one static layout computed once.
// Tempo Vault copy: a graph may bring its own positions (`pos3`, `pos2`, computed in Rust), and the
// palette knows the vault's node kinds (note, daily, current, tag, attachment, unresolved).

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

function palette(host) {
  const s = getComputedStyle(host), v = (n, f) => s.getPropertyValue(n).trim() || f;
  return {
    core: rgb(v('--t-ink', '#ECEEF2')), current: rgb(v('--t-ink', '#ECEEF2')), memory: rgb(v('--t-muted', '#8B93A1')),
    pack: rgb(v('--t-faint', '#6B7280')), used: rgb(v('--t-signal', '#F2A33A')), edge: rgb(v('--t-rule-hi', '#343B47')),
    note: rgb(v('--t-muted', '#8B93A1')), daily: rgb(v('--t-daily', '#B9A27A')), tag: rgb(v('--t-tag', '#7FA7C9')),
    attachment: rgb(v('--t-faint', '#6B7280')), unresolved: rgb(v('--t-rule-hi', '#4A5260')),
  };
}

const sizeOf = (node, maxChunks, used) => 5 + 11 * Math.sqrt((node.chunks || 1) / maxChunks) + (used ? 4 : 0);

// opts: {used: Set(src), onPick(src), reduced: bool, flatLabel: string}. Returns {close(), mode}.
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

async function threeView(host, g, opts, used, pal, maxChunks, tip) {
  const T = await import(new URL('./vendor/three.module.min.js', import.meta.url).href);
  const pos = g.pos3 ? Float32Array.from(g.pos3) : layout(g.nodes, g.edges, 3);
  const n = g.nodes.length;
  const canvas = document.createElement('canvas');
  canvas.className = 'mg-canvas';
  host.prepend(canvas);
  const renderer = new T.WebGLRenderer({ canvas, antialias: true, alpha: true, powerPreference: 'low-power' });
  const ratio = Math.min(window.devicePixelRatio || 1, 1.5);
  renderer.setPixelRatio(ratio);
  const scene = new T.Scene(), cam = new T.PerspectiveCamera(50, 1, 0.01, 100), group = new T.Group();
  cam.position.set(0, 0, 2.35);
  scene.add(group);

  const col = new Float32Array(n * 3), size = new Float32Array(n);
  g.nodes.forEach((node, i) => {
    const u = used.has(node.src) || !!node.used, c = u ? pal.used : (pal[node.kind] || pal.memory);
    col.set(c, i * 3);
    size[i] = sizeOf(node, maxChunks, u) * ratio;
  });
  const geo = new T.BufferGeometry();
  geo.setAttribute('position', new T.BufferAttribute(pos, 3));
  geo.setAttribute('acolor', new T.BufferAttribute(col, 3));
  geo.setAttribute('asize', new T.BufferAttribute(size, 1));
  const mat = new T.ShaderMaterial({
    transparent: true, depthWrite: false,
    vertexShader: 'attribute vec3 acolor; attribute float asize; varying vec3 vc;' +
      'void main(){ vc = acolor; vec4 mv = modelViewMatrix * vec4(position, 1.0);' +
      ' gl_PointSize = asize * (3.1 / -mv.z); gl_Position = projectionMatrix * mv; }',
    fragmentShader: 'varying vec3 vc; void main(){ float d = length(gl_PointCoord - 0.5);' +
      ' if (d > 0.5) discard; gl_FragColor = vec4(vc, smoothstep(0.5, 0.36, d)); }',
  });
  group.add(new T.Points(geo, mat));
  const ep = new Float32Array(g.edges.length * 6);
  g.edges.forEach((e, i) => { ep.set(pos.subarray(e.a * 3, e.a * 3 + 3), i * 6); ep.set(pos.subarray(e.b * 3, e.b * 3 + 3), i * 6 + 3); });
  const egeo = new T.BufferGeometry();
  egeo.setAttribute('position', new T.BufferAttribute(ep, 3));
  const emat = new T.LineBasicMaterial({ color: new T.Color(...pal.edge), transparent: true, opacity: 0.55 });
  group.add(new T.LineSegments(egeo, emat));

  // Labels for the files used this turn, placed in screen space each frame.
  const labels = g.nodes.map((node, i) => [node, i]).filter(([node]) => used.has(node.src) || node.used).map(([node, i]) => {
    const el = document.createElement('span');
    el.className = 'mg-label';
    el.textContent = node.name;
    host.append(el);
    return [el, i];
  });
  const v = new T.Vector3();
  const screen = i => {
    v.set(pos[i * 3], pos[i * 3 + 1], pos[i * 3 + 2]).applyMatrix4(group.matrixWorld).project(cam);
    return [(v.x + 1) / 2 * host.clientWidth, (1 - v.y) / 2 * host.clientHeight, v.z];
  };
  const place = () => labels.forEach(([el, i]) => { const [x, y] = screen(i); el.style.transform = `translate(${x + 8}px, ${y - 8}px)`; });

  let raf = 0, last = 0, dirty = true, visible = true, closed = false;
  const auto = !opts.reduced;
  function frame(t) {
    raf = 0;
    if (closed || !visible) return;
    if (auto && t - last >= 33) { group.rotation.y += 0.002; dirty = true; last = t; } // at most ~30 fps
    if (dirty) { renderer.render(scene, cam); place(); dirty = false; }
    if (auto) raf = requestAnimationFrame(frame);
  }
  const kick = () => { dirty = true; if (!raf && !closed) raf = requestAnimationFrame(frame); };
  const resize = () => {
    const w = host.clientWidth || 1, h = host.clientHeight || 1;
    renderer.setSize(w, h, false);
    cam.aspect = w / h;
    cam.updateProjectionMatrix();
    kick();
  };
  const ro = new ResizeObserver(resize);
  ro.observe(host);
  const io = new IntersectionObserver(es => { visible = es.some(e => e.isIntersecting); if (visible) kick(); });
  io.observe(host);
  const onVis = () => { visible = !document.hidden; if (visible) kick(); };
  document.addEventListener('visibilitychange', onVis);

  const pick = (px, py) => {
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
  let drag = null;
  canvas.addEventListener('pointerdown', e => { drag = { x: e.clientX, y: e.clientY, moved: false }; canvas.setPointerCapture(e.pointerId); });
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
    tip.hidden = i < 0;
    canvas.style.cursor = i < 0 ? '' : 'pointer';
    if (i >= 0) { tip.textContent = g.nodes[i].name; tip.style.transform = `translate(${e.clientX - r.left + 12}px, ${e.clientY - r.top + 12}px)`; }
  });
  canvas.addEventListener('pointerup', e => {
    const r = canvas.getBoundingClientRect(), wasDrag = drag?.moved;
    drag = null;
    if (wasDrag) return;
    const i = pick(e.clientX - r.left, e.clientY - r.top);
    if (i >= 0) opts.onPick?.(g.nodes[i].src);
  });
  canvas.addEventListener('wheel', e => {
    e.preventDefault();
    cam.position.z = Math.max(1.2, Math.min(8, cam.position.z * (1 + e.deltaY * 0.001)));
    kick();
  }, { passive: false });
  resize();

  return {
    mode: '3d',
    close() {
      closed = true;
      if (raf) cancelAnimationFrame(raf);
      ro.disconnect(); io.disconnect();
      document.removeEventListener('visibilitychange', onVis);
      geo.dispose(); mat.dispose(); egeo.dispose(); emat.dispose();
      renderer.dispose(); renderer.forceContextLoss();
      host.replaceChildren();
    },
  };
}

function flatView(host, g, opts, used, pal, maxChunks, tip) {
  const NS = 'http://www.w3.org/2000/svg', mk = (t, a) => { const e = document.createElementNS(NS, t); for (const k in a) e.setAttribute(k, a[k]); return e; };
  const pos = g.pos2 ? Float32Array.from(g.pos2) : layout(g.nodes, g.edges, 2);
  let span = 2.4;
  const svg = mk('svg', { class: 'mg-svg', viewBox: `${-span / 2} ${-span / 2} ${span} ${span}`, role: 'img', 'aria-label': opts.flatLabel || '' });
  const lines = mk('g', { stroke: hex(pal.edge), 'stroke-opacity': '0.6' });
  for (const e of g.edges) {
    lines.append(mk('line', { x1: pos[e.a * 3], y1: pos[e.a * 3 + 1], x2: pos[e.b * 3], y2: pos[e.b * 3 + 1], 'vector-effect': 'non-scaling-stroke' }));
  }
  svg.append(lines);
  const unit = span / Math.max(host.clientWidth || 600, 1);
  g.nodes.forEach((node, i) => {
    const u = used.has(node.src) || !!node.used;
    const c = mk('circle', { cx: pos[i * 3], cy: pos[i * 3 + 1], r: (sizeOf(node, maxChunks, u) / 2) * unit, fill: hex(u ? pal.used : (pal[node.kind] || pal.memory)), tabindex: '0', role: 'button', 'aria-label': node.name });
    const t = mk('title', {});
    t.textContent = node.name;
    c.append(t);
    c.addEventListener('click', () => opts.onPick?.(node.src));
    c.addEventListener('keydown', e => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); opts.onPick?.(node.src); } });
    svg.append(c);
  });
  svg.addEventListener('wheel', e => {
    e.preventDefault();
    span = Math.max(0.4, Math.min(6, span * (1 + e.deltaY * 0.001)));
    svg.setAttribute('viewBox', `${-span / 2} ${-span / 2} ${span} ${span}`);
  }, { passive: false });
  host.prepend(svg);
  return { mode: 'flat', close() { host.replaceChildren(); } };
}
