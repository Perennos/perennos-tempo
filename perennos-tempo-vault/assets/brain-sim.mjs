// The pure half of the second brain: no DOM, no three.js, so it runs under node:test.
//   normalize   raw agent or vault graph -> nodes and links keyed by source path
//   diffGraph   what a new picture of memory added, removed and changed
//   ForceSim    3D layout: charge, link springs, a pull to the core and a pull to the cluster's anchor
//   pickLabels  which labels fit on screen, by priority, without touching each other
//   latticePoints, bezier helpers, search matching, camera framing and routes for the light trails
// brain-world.mjs draws what these compute.

export const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
export const smooth = t => { t = clamp(t, 0, 1); return t * t * (3 - 2 * t); };
// Frame-rate independent easing: move `lambda` per second of the way to the target.
export const damp = (cur, target, lambda, dt) => cur + (target - cur) * (1 - Math.exp(-lambda * dt));

// A small seeded generator (mulberry32): the same memory always draws the same picture.
export function rng(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
export function hashStr(s) {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 16777619); }
  return h >>> 0;
}

// ---- data -----------------------------------------------------------------------------------------------

const dirOf = p => { const i = p.lastIndexOf('/'); return i > 0 ? p.slice(0, i) : ''; };
const absolute = p => p.startsWith('/') || /^[A-Za-z]:[\\/]/.test(p);

// The longest directory every absolute path shares: paths are shown relative to it.
export function commonRoot(paths) {
  const abs = paths.filter(absolute).map(p => p.replace(/\\/g, '/'));
  if (abs.length < 2) return '';
  let parts = dirOf(abs[0]).split('/');
  for (const p of abs.slice(1)) {
    const q = dirOf(p).split('/');
    let i = 0;
    while (i < parts.length && i < q.length && parts[i] === q[i]) i++;
    parts = parts.slice(0, i);
  }
  return parts.join('/');
}

// raw: {nodes: [{src|id, name, kind, chunks, used, mtime}], edges: [{a, b, w}]} with a and b as indexes into
// nodes (what both the agent and the vault send). opts.source: 'agent' or 'vault', which says what an edge
// that carries no type means: shared terms (similar) or a written link.
export function normalize(raw, opts = {}) {
  const src = (raw && raw.nodes) || [];
  const now = opts.now ?? Date.now() / 1000;
  const root = commonRoot(src.map(n => String(n.src ?? n.path ?? n.id ?? '')));
  const maxW = src.reduce((m, n) => Math.max(m, n.chunks || 1), 1);
  const ids = new Map(), nodes = [], byIndex = [];
  src.forEach((n, i) => {
    const id = String(n.src ?? n.path ?? n.id ?? i);
    if (ids.has(id)) { byIndex[i] = ids.get(id); return; }
    const path = id.replace(/\\/g, '/');
    const rel = root && path.startsWith(root + '/') ? path.slice(root.length + 1) : path;
    const mtime = Number.isFinite(n.mtime) ? n.mtime : 0;
    const imp = Math.sqrt((n.chunks || 1) / maxW);
    const rec = mtime ? Math.exp(-Math.max(0, now - mtime) / (30 * 86400)) : imp;
    const kind = String(n.kind || 'note');
    const folder = dirOf(rel);
    const node = {
      id, title: String(n.name ?? n.title ?? rel.split('/').pop() ?? id), kind, path: rel, folder,
      cluster: folder || kind, mtime, chunks: n.chunks || 1, used: !!n.used, size: 0.25 + 0.75 * (0.6 * imp + 0.4 * rec),
    };
    ids.set(id, nodes.length);
    byIndex[i] = nodes.length;
    nodes.push(node);
  });
  const seen = new Set(), links = [];
  for (const e of (raw && raw.edges) || []) {
    const a = byIndex[e.a], b = byIndex[e.b];
    if (a == null || b == null || a === b) continue;
    const key = a < b ? a + '|' + b : b + '|' + a;
    if (seen.has(key)) continue;
    seen.add(key);
    const na = nodes[a], nb = nodes[b];
    const type = e.type || (na.kind === 'tag' || nb.kind === 'tag' ? 'tag' : opts.source === 'vault' ? 'link' : 'similar');
    links.push({ a: na.id, b: nb.id, w: e.w ?? 0.2, type });
  }
  return { nodes, links };
}

const nodeSig = n => `${n.kind}|${n.size.toFixed(2)}|${n.used ? 1 : 0}|${n.mtime}|${n.title}`;
const linkKey = l => (l.a < l.b ? l.a + '\u0001' + l.b : l.b + '\u0001' + l.a) + '\u0001' + l.type;

// What changed between two normalized graphs. changed holds {prev, next} pairs for nodes present in both.
export function diffGraph(prev, next) {
  const pm = new Map((prev?.nodes || []).map(n => [n.id, n])), nm = new Map(next.nodes.map(n => [n.id, n]));
  const added = next.nodes.filter(n => !pm.has(n.id));
  const removed = (prev?.nodes || []).filter(n => !nm.has(n.id));
  const changed = [];
  for (const n of next.nodes) { const p = pm.get(n.id); if (p && nodeSig(p) !== nodeSig(n)) changed.push({ prev: p, next: n }); }
  const pl = new Set((prev?.links || []).map(linkKey)), nl = new Set(next.links.map(linkKey));
  let linksChanged = pl.size !== nl.size;
  if (!linksChanged) for (const k of nl) if (!pl.has(k)) { linksChanged = true; break; }
  return { added, removed, changed, linksChanged, empty: !added.length && !removed.length && !changed.length && !linksChanged };
}

export const adjacency = links => {
  const adj = new Map();
  for (const l of links) {
    if (!adj.has(l.a)) adj.set(l.a, []);
    if (!adj.has(l.b)) adj.set(l.b, []);
    adj.get(l.a).push({ id: l.b, w: l.w });
    adj.get(l.b).push({ id: l.a, w: l.w });
  }
  return adj;
};

// Cheapest route over links (a stronger link is cheaper), as a list of ids, or null when none connects.
export function shortestPath(adj, from, to) {
  if (from === to) return [from];
  const dist = new Map([[from, 0]]), prev = new Map(), open = new Set([from]);
  while (open.size) {
    let u = null, best = Infinity;
    for (const id of open) { const d = dist.get(id); if (d < best) { best = d; u = id; } }
    open.delete(u);
    if (u === to) break;
    for (const { id, w } of adj.get(u) || []) {
      const d = best + 1 / (0.05 + w);
      if (d < (dist.get(id) ?? Infinity)) { dist.set(id, d); prev.set(id, u); open.add(id); }
    }
  }
  if (!prev.has(to)) return null;
  const out = [to];
  while (out[0] !== from) out.unshift(prev.get(out[0]));
  return out;
}

// ---- layout ---------------------------------------------------------------------------------------------

// Anchors for clusters: spread on a sphere (Fibonacci spiral) at `radius`, in sorted key order.
export function clusterAnchors(keys, radius = 0.5) {
  const sorted = [...new Set(keys)].sort(), m = sorted.length, out = new Map();
  sorted.forEach((k, i) => {
    if (m === 1) { out.set(k, [0, 0, 0]); return; }
    const y = 1 - (2 * (i + 0.5)) / m, r = Math.sqrt(1 - y * y), a = i * 2.399963229728653;
    out.set(k, [Math.cos(a) * r * radius, y * radius, Math.sin(a) * r * radius]);
  });
  return out;
}

export class ForceSim {
  // Everything is in sphere units: the lattice shell has radius 1, nodes stay inside `limit`.
  constructor(o = {}) {
    this.limit = o.limit ?? 0.8;
    this.charge = o.charge ?? 1;
    this.linkK = o.linkK ?? 0.14;
    this.centerK = o.centerK ?? 0.4;
    this.clusterK = o.clusterK ?? 1.5;
    this.damping = o.damping ?? 0.55;
    this.alphaDecay = o.alphaDecay ?? 1 - Math.pow(0.001, 1 / 280);
    this.alphaMin = o.alphaMin ?? 0.0035;
    this.alpha = 1;
    this.ids = []; this.pos = new Float32Array(0); this.vel = new Float32Array(0);
    this.links = []; this.cluster = []; this.anchors = new Map(); this.index = new Map();
    this.k = 0.3;
  }
  get settled() { return this.alpha < this.alphaMin; }
  // nodes: [{id, cluster}], links: [{a, b, w}] (by id). Nodes already present keep their place; new ones
  // start beside a neighbour that is placed, else on a seeded point inside the shell.
  setGraph(nodes, links, reheat = 0.6) {
    const old = new Map(this.ids.map((id, i) => [id, i]));
    const n = nodes.length, pos = new Float32Array(n * 3), vel = new Float32Array(n * 3);
    const nb = new Map();
    for (const l of links) {
      if (!nb.has(l.a)) nb.set(l.a, []);
      if (!nb.has(l.b)) nb.set(l.b, []);
      nb.get(l.a).push(l.b); nb.get(l.b).push(l.a);
    }
    const anchors = clusterAnchors(nodes.map(x => x.cluster), 0.55);
    nodes.forEach((node, i) => {
      const o = old.get(node.id);
      if (o != null) { pos.set(this.pos.subarray(o * 3, o * 3 + 3), i * 3); vel.set(this.vel.subarray(o * 3, o * 3 + 3), i * 3); return; }
      const r = rng(hashStr(node.id));
      const near = (nb.get(node.id) || []).find(id => old.has(id));
      if (near != null) {
        const o2 = old.get(near);
        for (let d = 0; d < 3; d++) pos[i * 3 + d] = this.pos[o2 * 3 + d] + (r() - 0.5) * 0.12;
      } else {
        const a = anchors.get(node.cluster) || [0, 0, 0];
        for (let d = 0; d < 3; d++) pos[i * 3 + d] = a[d] + (r() - 0.5) * 0.5;
      }
    });
    this.ids = nodes.map(x => x.id);
    this.index = new Map(this.ids.map((id, i) => [id, i]));
    this.cluster = nodes.map(x => x.cluster);
    this.anchors = anchors;
    this.pos = pos; this.vel = vel;
    this.links = links.filter(l => this.index.has(l.a) && this.index.has(l.b)).map(l => ({ a: this.index.get(l.a), b: this.index.get(l.b), w: l.w ?? 0.2 }));
    // spacing a uniform fill of the interior would have; springs and charge both scale with it
    this.k = this.limit * 0.9 / Math.cbrt(Math.max(n, 1));
    this.alpha = Math.max(this.alpha, reheat);
  }
  reheat(a = 0.4) { this.alpha = Math.max(this.alpha, a); }
  // One tick. Returns true while the layout is still moving.
  step() {
    if (this.settled) return false;
    const { pos, vel, links, k } = this, n = this.ids.length, a = this.alpha;
    const cut = k * 3.2, cut2 = cut * cut, q = this.charge * k * k * 0.9 * a;
    for (let i = 0; i < n; i++) {
      const ix = i * 3;
      for (let j = i + 1; j < n; j++) {
        const jx = j * 3, dx = pos[ix] - pos[jx], dy = pos[ix + 1] - pos[jx + 1], dz = pos[ix + 2] - pos[jx + 2];
        const d2 = dx * dx + dy * dy + dz * dz;
        if (d2 > cut2) continue;
        const f = q / (d2 + k * k * 0.04);
        vel[ix] += dx * f; vel[ix + 1] += dy * f; vel[ix + 2] += dz * f;
        vel[jx] -= dx * f; vel[jx + 1] -= dy * f; vel[jx + 2] -= dz * f;
      }
    }
    for (const l of links) {
      const ax = l.a * 3, bx = l.b * 3, dx = pos[bx] - pos[ax], dy = pos[bx + 1] - pos[ax + 1], dz = pos[bx + 2] - pos[ax + 2];
      const d = Math.sqrt(dx * dx + dy * dy + dz * dz) + 1e-6, s = clamp(l.w * 3, 0.15, 1);
      const rest = k * (1.5 - 0.8 * s), f = (d - rest) / d * this.linkK * (0.4 + s) * a;
      vel[ax] += dx * f; vel[ax + 1] += dy * f; vel[ax + 2] += dz * f;
      vel[bx] -= dx * f; vel[bx + 1] -= dy * f; vel[bx + 2] -= dz * f;
    }
    for (let i = 0; i < n; i++) {
      const ix = i * 3, an = this.anchors.get(this.cluster[i]) || [0, 0, 0];
      for (let d = 0; d < 3; d++) {
        vel[ix + d] += (-pos[ix + d] * this.centerK * 0.25 + (an[d] - pos[ix + d]) * this.clusterK * 0.25) * a;
      }
      const r = Math.hypot(pos[ix], pos[ix + 1], pos[ix + 2]);
      if (r > this.limit) { const push = (r - this.limit) / r * 0.8; vel[ix] -= pos[ix] * push; vel[ix + 1] -= pos[ix + 1] * push; vel[ix + 2] -= pos[ix + 2] * push; }
      for (let d = 0; d < 3; d++) { vel[ix + d] *= this.damping; pos[ix + d] += vel[ix + d]; }
    }
    this.alpha += (0 - this.alpha) * this.alphaDecay;
    return !this.settled;
  }
  // Runs ticks until it settles (or `max`), for still layouts and reduced motion.
  settle(max = 400) { let i = 0; while (i++ < max && this.step()); return i; }
  maxSpeed() { let m = 0; for (let i = 0; i < this.ids.length; i++) m = Math.max(m, Math.hypot(this.vel[i * 3], this.vel[i * 3 + 1], this.vel[i * 3 + 2])); return m; }
  at(id, out = [0, 0, 0]) { const i = this.index.get(id); if (i == null) return null; out[0] = this.pos[i * 3]; out[1] = this.pos[i * 3 + 1]; out[2] = this.pos[i * 3 + 2]; return out; }
}

// ---- labels, search, camera ----------------------------------------------------------------------------

// cands: [{id, x, y, w, h, priority}] in screen pixels (x, y the top left). Highest priority first; a label
// that would touch one already placed, or leave the box {w, h}, is hidden. `prev` (a Set of ids shown last
// time) gets a small bonus so labels do not trade places while the picture turns.
export function pickLabels(cands, o = {}) {
  const pad = o.pad ?? 6, max = o.max ?? 24, bias = o.stickyBias ?? 0.5, prev = o.prev;
  const order = cands.map(c => ({ c, p: c.priority + (prev && prev.has(c.id) ? bias : 0) }))
    .sort((u, v) => v.p - u.p || (u.c.id < v.c.id ? -1 : 1));
  const placed = [], out = [];
  for (const { c } of order) {
    if (out.length >= max) break;
    if (o.w != null && (c.x < 0 || c.y < 0 || c.x + c.w > o.w || c.y + c.h > o.h)) continue;
    let hit = false;
    for (const r of placed) {
      if (c.x < r.x + r.w + pad && c.x + c.w + pad > r.x && c.y < r.y + r.h + pad && c.y + c.h + pad > r.y) { hit = true; break; }
    }
    if (hit) continue;
    placed.push(c); out.push(c.id);
  }
  return out;
}

// The largest node of each of the biggest clusters: they carry the labels when nothing else is lit.
export function clusterLeaders(nodes, clusters = 8) {
  const by = new Map();
  for (const n of nodes) {
    const g = by.get(n.cluster) || { n: 0, lead: null };
    g.n++;
    if (!g.lead || n.size > g.lead.size || (n.size === g.lead.size && n.id < g.lead.id)) g.lead = n;
    by.set(n.cluster, g);
  }
  return [...by.entries()].sort((a, b) => b[1].n - a[1].n || (a[0] < b[0] ? -1 : 1)).slice(0, clusters).map(([, g]) => g.lead.id);
}

// Nodes whose title, path or kind holds every word of the query; best matches first.
export function matchNodes(nodes, query) {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return [];
  const out = [];
  for (const n of nodes) {
    const t = n.title.toLowerCase(), p = n.path.toLowerCase(), all = t + ' ' + p + ' ' + n.kind.toLowerCase();
    if (!words.every(w => all.includes(w))) continue;
    let s = 0;
    for (const w of words) s += (t.startsWith(w) ? 3 : t.includes(w) ? 2 : 1);
    out.push({ id: n.id, score: s + n.size * 0.1 });
  }
  return out.sort((a, b) => b.score - a.score || (a.id < b.id ? -1 : 1)).map(m => m.id);
}

// Centre and radius of a box round the points, for framing a set of nodes with the camera.
export function fitSphere(points) {
  if (!points.length) return { c: [0, 0, 0], r: 1 };
  const lo = [Infinity, Infinity, Infinity], hi = [-Infinity, -Infinity, -Infinity];
  for (const p of points) for (let d = 0; d < 3; d++) { lo[d] = Math.min(lo[d], p[d]); hi[d] = Math.max(hi[d], p[d]); }
  const c = lo.map((v, d) => (v + hi[d]) / 2);
  let r = 0;
  for (const p of points) r = Math.max(r, Math.hypot(p[0] - c[0], p[1] - c[1], p[2] - c[2]));
  return { c, r };
}

// "3 days ago" style, for the card; empty when the time is unknown.
export function ago(mtime, now = Date.now() / 1000) {
  if (!mtime) return '';
  const s = Math.max(0, now - mtime);
  const u = [[60, 'second'], [3600, 'minute'], [86400, 'hour'], [86400 * 30, 'day'], [86400 * 365, 'month'], [Infinity, 'year']];
  const div = [1, 60, 3600, 86400, 86400 * 30, 86400 * 365];
  for (let i = 0; i < u.length; i++) {
    if (s < u[i][0]) { const v = Math.floor(s / div[i]); return v < 1 && i === 0 ? 'just now' : `${v} ${u[i][1]}${v === 1 ? '' : 's'} ago`; }
  }
  return '';
}

// ---- lattice and curves -----------------------------------------------------------------------------------

// `count` points on the unit sphere in rings of latitude, one spacing apart, with every other ring staggered
// by half a step, so the body reads as rows of even grain. Also returns a seed in 0..1 per point.
export function latticePoints(count) {
  const d = Math.sqrt(4 * Math.PI / count), rings = Math.max(2, Math.round(Math.PI / d));
  const raw = [];
  let tot = 0;
  for (let k = 0; k < rings; k++) { const c = Math.max(1, 2 * Math.PI * Math.sin(Math.PI * (k + 0.5) / rings) / d); raw.push(c); tot += c; }
  const pos = new Float32Array(count * 3), seed = new Float32Array(count), r = rng(1234567);
  let cum = 0, start = 0, n = 0;
  for (let k = 0; k < rings; k++) {
    cum += raw[k];
    const end = k === rings - 1 ? count : Math.round(count * cum / tot), nk = end - start;
    const th = Math.PI * (k + 0.5) / rings, y = Math.cos(th), rr = Math.sin(th);
    for (let i = 0; i < nk; i++) {
      const ph = 2 * Math.PI * (i + 0.5 * (k & 1)) / nk;
      pos[n * 3] = Math.cos(ph) * rr; pos[n * 3 + 1] = y; pos[n * 3 + 2] = Math.sin(ph) * rr;
      seed[n] = r();
      n++;
    }
    start = end;
  }
  return { pos, seed, count: n };
}

// Control point of the arc between two nodes: the middle of the chord pushed away from the core, so links
// arch outward like fibres instead of cutting straight through the middle.
export function bezierControl(a, b, bulge = 0.3, out = [0, 0, 0]) {
  const mx = (a[0] + b[0]) / 2, my = (a[1] + b[1]) / 2, mz = (a[2] + b[2]) / 2;
  const len = Math.hypot(b[0] - a[0], b[1] - a[1], b[2] - a[2]), ml = Math.hypot(mx, my, mz);
  const ux = ml > 1e-4 ? mx / ml : 0, uy = ml > 1e-4 ? my / ml : 1, uz = ml > 1e-4 ? mz / ml : 0;
  out[0] = mx + ux * len * bulge; out[1] = my + uy * len * bulge; out[2] = mz + uz * len * bulge;
  return out;
}
export function bezierAt(a, c, b, t, out = [0, 0, 0]) {
  const u = 1 - t, w0 = u * u, w1 = 2 * u * t, w2 = t * t;
  out[0] = w0 * a[0] + w1 * c[0] + w2 * b[0]; out[1] = w0 * a[1] + w1 * c[1] + w2 * b[1]; out[2] = w0 * a[2] + w1 * c[2] + w2 * b[2];
  return out;
}

// A trail's path as a list of 3D points (a polyline sampled along the arcs), with its length, so a particle can
// be placed at any 0..1 along it. points: [[x,y,z], ...] node positions from the core to the target.
export function sampleRoute(points, per = 10, bulge = 0.3) {
  const out = [];
  for (let i = 0; i + 1 < points.length; i++) {
    const a = points[i], b = points[i + 1], c = bezierControl(a, b, bulge);
    for (let s = i ? 1 : 0; s <= per; s++) out.push(bezierAt(a, c, b, s / per));
  }
  const cum = [0];
  for (let i = 1; i < out.length; i++) cum.push(cum[i - 1] + Math.hypot(out[i][0] - out[i - 1][0], out[i][1] - out[i - 1][1], out[i][2] - out[i - 1][2]));
  return { pts: out, cum, length: cum[cum.length - 1] || 1e-6 };
}
export function routeAt(route, t, out = [0, 0, 0]) {
  const want = clamp(t, 0, 1) * route.length;
  let lo = 0, hi = route.cum.length - 1;
  while (hi - lo > 1) { const mid = (lo + hi) >> 1; if (route.cum[mid] < want) lo = mid; else hi = mid; }
  const span = route.cum[hi] - route.cum[lo] || 1e-9, f = clamp((want - route.cum[lo]) / span, 0, 1);
  for (let d = 0; d < 3; d++) out[d] = route.pts[lo][d] + (route.pts[hi][d] - route.pts[lo][d]) * f;
  return out;
}
