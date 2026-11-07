// Tests for the pure parts of the second brain (layout, labels, diffing, feed). No browser: node --test.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  normalize, diffGraph, ForceSim, pickLabels, clusterLeaders, matchNodes, fitSphere, latticePoints,
  bezierControl, bezierAt, sampleRoute, routeAt, shortestPath, adjacency, commonRoot, ago, damp,
} from './brain-sim.mjs';
import { createFeed, graphSignature } from './brain-feed.mjs';

// A synthetic memory: three folders of notes, links mostly inside a folder.
function memory(n = 120) {
  const nodes = [], edges = [];
  for (let i = 0; i < n; i++) nodes.push({ src: `/data/${['notes', 'packs', 'chats'][i % 3]}/f${i}.md`, name: `f${i}.md`, kind: ['memory', 'pack', 'core'][i % 3], chunks: 1 + (i * 7) % 9, used: i % 17 === 0, mtime: 1e9 + i * 1000 });
  for (let i = 0; i < n; i++) for (const d of [3, 6, 1]) if (i + d < n && (d !== 1 || i % 4 === 0)) edges.push({ a: i, b: i + d, w: d === 1 ? 0.1 : 0.3 });
  return { nodes, edges };
}
const norm = raw => normalize(raw, { source: 'agent', now: 1.1e9 });

test('normalize keys nodes by source, makes paths relative to the shared root and sizes by importance', () => {
  const g = norm(memory(30));
  assert.equal(g.nodes.length, 30);
  assert.equal(g.nodes[0].id, '/data/notes/f0.md');
  assert.equal(g.nodes[0].path, 'notes/f0.md');
  assert.equal(g.nodes[0].folder, 'notes');
  assert.ok(g.nodes.every(n => n.size >= 0.25 && n.size <= 1));
  assert.ok(g.links.every(l => l.type === 'similar'));
  assert.equal(commonRoot(['/a/b/c.md', '/a/b/d/e.md']), '/a/b');
  assert.equal(commonRoot(['x.md', 'y/z.md']), '');
});

test('normalize drops self links, repeats and links to nowhere; tags become tag links; vault links stay links', () => {
  const raw = { nodes: [{ src: 'a.md', name: 'A', kind: 'note' }, { src: 'b.md', name: 'B', kind: 'note' }, { src: '#t', name: '#t', kind: 'tag' }], edges: [{ a: 0, b: 0 }, { a: 0, b: 1 }, { a: 1, b: 0 }, { a: 0, b: 9 }, { a: 0, b: 2 }] };
  const g = normalize(raw, { source: 'vault' });
  assert.deepEqual(g.links.map(l => l.type), ['link', 'tag']);
});

test('diffGraph finds added, removed and changed nodes and says when nothing moved', () => {
  const a = norm(memory(20));
  assert.ok(diffGraph(a, norm(memory(20))).empty);
  const raw = memory(20);
  raw.nodes.pop(); raw.edges = raw.edges.filter(e => e.a < 19 && e.b < 19);
  raw.nodes.push({ src: '/data/notes/new.md', name: 'new.md', kind: 'memory', chunks: 3 });
  raw.nodes[2].used = true;
  const d = diffGraph(a, norm(raw));
  assert.deepEqual(d.added.map(n => n.id), ['/data/notes/new.md']);
  assert.deepEqual(d.removed.map(n => n.id), [a.nodes[19].id]);
  assert.ok(d.changed.some(c => c.next.id === a.nodes[2].id && c.next.used));
  assert.equal(d.linksChanged, true);
  assert.equal(diffGraph(null, a).added.length, 20);
});

test('the force layout settles, stays inside the shell, and puts linked nodes nearer than strangers', () => {
  const g = norm(memory(120)), sim = new ForceSim();
  sim.setGraph(g.nodes, g.links, 1);
  const steps = sim.settle(900);
  assert.ok(sim.settled, 'cooled');
  assert.ok(steps < 900, 'in a few hundred ticks: ' + steps);
  assert.ok(sim.maxSpeed() < 0.01);
  const r = [...Array(120).keys()].map(i => Math.hypot(sim.pos[i * 3], sim.pos[i * 3 + 1], sim.pos[i * 3 + 2]));
  assert.ok(Math.max(...r) < 0.95, 'inside the shell: ' + Math.max(...r));
  const dist = (a, b) => Math.hypot(sim.pos[a * 3] - sim.pos[b * 3], sim.pos[a * 3 + 1] - sim.pos[b * 3 + 1], sim.pos[a * 3 + 2] - sim.pos[b * 3 + 2]);
  let linked = 0, ln = 0, other = 0, on = 0;
  const has = new Set(sim.links.map(l => l.a + '|' + l.b));
  for (let i = 0; i < 120; i++) for (let j = i + 1; j < 120; j++) { if (has.has(i + '|' + j)) { linked += dist(i, j); ln++; } else { other += dist(i, j); on++; } }
  assert.ok(linked / ln < 0.8 * (other / on), `linked ${linked / ln} vs others ${other / on}`);
  // nothing sits on top of anything else
  let min = Infinity;
  for (let i = 0; i < 120; i++) for (let j = i + 1; j < 120; j++) min = Math.min(min, dist(i, j));
  assert.ok(min > 0.01, 'minimum gap ' + min);
});

test('the layout is deterministic, and a new node arrives beside its neighbour without moving the rest much', () => {
  const g = norm(memory(60)), a = new ForceSim(), b = new ForceSim();
  a.setGraph(g.nodes, g.links, 1); b.setGraph(g.nodes, g.links, 1);
  a.settle(); b.settle();
  assert.deepEqual([...a.pos], [...b.pos]);
  const raw = memory(60);
  raw.nodes.push({ src: '/data/notes/extra.md', name: 'extra.md', kind: 'memory', chunks: 2 });
  raw.edges.push({ a: 60, b: 3, w: 0.4 });
  const g2 = norm(raw), before = Float32Array.from(a.pos);
  a.setGraph(g2.nodes, g2.links, 0.3);
  const start = a.at('/data/notes/extra.md'), nb = a.at(g.nodes[3].id);
  assert.ok(Math.hypot(start[0] - nb[0], start[1] - nb[1], start[2] - nb[2]) < 0.25, 'starts beside its neighbour');
  a.settle();
  assert.ok(a.settled);
  let moved = 0;
  for (let i = 0; i < 60; i++) moved += Math.hypot(a.pos[i * 3] - before[i * 3], a.pos[i * 3 + 1] - before[i * 3 + 1], a.pos[i * 3 + 2] - before[i * 3 + 2]);
  assert.ok(moved / 60 < 0.25, 'the rest stays put: ' + moved / 60);
});

test('an empty or single-node graph does not break the layout', () => {
  const s = new ForceSim();
  s.setGraph([], []); assert.ok(s.settle() > 0);
  s.setGraph([{ id: 'x', cluster: 'k' }], []); s.settle();
  assert.ok(Math.hypot(...s.at('x')) < 0.2);
});

test('pickLabels hides by priority and never lets two labels touch', () => {
  const box = (id, x, y, priority) => ({ id, x, y, w: 80, h: 14, priority });
  const shown = pickLabels([box('low', 10, 10, 1), box('high', 40, 12, 9), box('mid', 200, 10, 5), box('off', 590, 10, 8)], { w: 640, h: 400 });
  assert.deepEqual(shown, ['high', 'mid']);
  // no pair of shown boxes overlaps, over a crowd
  const crowd = Array.from({ length: 200 }, (_, i) => box('n' + i, (i * 37) % 560, (i * 53) % 380, (i * 7) % 11));
  const ids = pickLabels(crowd, { w: 640, h: 400, pad: 4 }), by = new Map(crowd.map(c => [c.id, c]));
  for (let i = 0; i < ids.length; i++) for (let j = i + 1; j < ids.length; j++) {
    const p = by.get(ids[i]), q = by.get(ids[j]);
    assert.ok(p.x + p.w + 4 <= q.x || q.x + q.w + 4 <= p.x || p.y + p.h + 4 <= q.y || q.y + q.h + 4 <= p.y, `${ids[i]} touches ${ids[j]}`);
  }
  assert.equal(pickLabels(crowd, { max: 5 }).length, 5);
  // the label shown last time keeps its place against an equal rival
  assert.deepEqual(pickLabels([box('a', 0, 0, 1), box('b', 20, 0, 1)], { prev: new Set(['b']) }), ['b']);
});

test('labels go first to the largest node of the biggest clusters; search matches title, path and kind', () => {
  const g = norm(memory(30));
  const lead = clusterLeaders(g.nodes, 2);
  assert.equal(lead.length, 2);
  assert.ok(g.nodes.filter(n => n.cluster === g.nodes.find(x => x.id === lead[0]).cluster).every(n => n.size <= g.nodes.find(x => x.id === lead[0]).size));
  assert.equal(matchNodes(g.nodes, 'f7.md')[0], g.nodes[7].id);
  assert.ok(matchNodes(g.nodes, 'chats f1').length > 0);
  assert.deepEqual(matchNodes(g.nodes, ''), []);
  assert.deepEqual(matchNodes(g.nodes, 'zzzz'), []);
});

test('the lattice has exactly the points asked for, all on the unit sphere', () => {
  for (const n of [4000, 6000, 8000]) {
    const l = latticePoints(n);
    assert.equal(l.count, n);
    for (let i = 0; i < n; i += 97) assert.ok(Math.abs(Math.hypot(l.pos[i * 3], l.pos[i * 3 + 1], l.pos[i * 3 + 2]) - 1) < 1e-5);
    assert.ok(l.seed.every(s => s >= 0 && s < 1));
  }
});

test('curves arch away from the core and routes follow the strongest links', () => {
  const a = [0.4, 0, 0], b = [0, 0.4, 0], c = bezierControl(a, b, 0.3), mid = bezierAt(a, c, b, 0.5);
  assert.ok(Math.hypot(...mid) > Math.hypot(0.2, 0.2, 0), 'bulges outward');
  assert.deepEqual(bezierAt(a, c, b, 0), a);
  const adj = adjacency([{ a: 'core', b: 'x', w: 0.3 }, { a: 'x', b: 'y', w: 0.3 }, { a: 'core', b: 'y', w: 0.01 }, { a: 'y', b: 'z', w: 0.3 }]);
  assert.deepEqual(shortestPath(adj, 'core', 'z'), ['core', 'x', 'y', 'z']);
  assert.equal(shortestPath(adj, 'core', 'nowhere'), null);
  const r = sampleRoute([[0, 0, 0], [0.3, 0, 0], [0.3, 0.3, 0]]);
  assert.deepEqual(routeAt(r, 0), [0, 0, 0]);
  const end = routeAt(r, 1);
  assert.ok(Math.abs(end[0] - 0.3) < 1e-6 && Math.abs(end[1] - 0.3) < 1e-6);
  assert.ok(r.length > 0.6);
  const f = fitSphere([[0, 0, 0], [2, 0, 0]]);
  assert.deepEqual(f.c, [1, 0, 0]); assert.equal(f.r, 1);
});

test('small helpers: ago and damp', () => {
  assert.equal(ago(0), '');
  assert.equal(ago(1000, 1030), '30 seconds ago');
  assert.equal(ago(1000, 1000 + 86400 * 3), '3 days ago');
  assert.equal(ago(1000, 1000 + 3600), '1 hour ago');
  assert.ok(damp(0, 1, 5, 0.1) > 0.3 && damp(0, 1, 5, 0.1) < 0.5);
});

test('the feed hands over data only when it changed, backs off on errors and joins a load already running', async () => {
  const q = []; let now = 0;
  const timers = { set: (f, ms) => { q.push({ f, ms }); return q.length; }, clear: () => {} };
  let raw = memory(10), loads = 0, fail = false; const seen = [], errs = [];
  const feed = createFeed({ load: async () => { loads++; if (fail) throw new Error('down'); return raw; }, onData: r => seen.push(r), onError: e => errs.push(e), interval: 1000, fast: 100, hidden: 9000, isHidden: () => now > 0, timers });
  feed.start();
  await new Promise(r => setImmediate(r));
  assert.equal(seen.length, 1);
  assert.equal(q.at(-1).ms, 1000);
  q.pop().f(); await new Promise(r => setImmediate(r));
  assert.equal(seen.length, 1, 'same picture: nothing handed over');
  raw = memory(11); q.pop().f(); await new Promise(r => setImmediate(r));
  assert.equal(seen.length, 2);
  feed.setFast(true); q.pop().f(); await new Promise(r => setImmediate(r));
  assert.equal(q.at(-1).ms, 100);
  fail = true; q.pop().f(); await new Promise(r => setImmediate(r));
  assert.equal(errs.length, 1); assert.equal(q.at(-1).ms, 2000);
  fail = false; now = 1; q.pop().f(); await new Promise(r => setImmediate(r));
  assert.equal(q.at(-1).ms, 9000, 'slow while hidden');
  feed.stop(); const n = loads; feed.poke(); await new Promise(r => setImmediate(r));
  assert.equal(loads, n);
  assert.notEqual(graphSignature(memory(10)), graphSignature(memory(11)));
});
