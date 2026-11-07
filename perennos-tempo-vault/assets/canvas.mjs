// Perennos Tempo Vault: the canvas editor, for JSON Canvas 1.0 (Obsidian's .canvas files).
// Loaded only when a canvas opens. The file's own JSON is edited in place, so fields this editor
// does not know are kept; every change is saved through the vault's safe write path (history,
// conflict check). Positions are set through the CSS object model, never inline style markup,
// so the page's strict Content Security Policy holds. Card text arrives escaped from the server.

const SNAP = 10;
const PRESETS = ['1', '2', '3', '4', '5', '6'];
const okColor = c => typeof c === 'string' && (PRESETS.includes(c) || /^#[0-9a-f]{3}([0-9a-f]{3})?$/i.test(c));
const okUrl = u => /^https?:\/\/[^\s"<>]+$/i.test(u || '');
const IMG = /\.(png|jpe?g|gif|webp|svg|bmp|avif)$/i;
const newId = () => [...crypto.getRandomValues(new Uint8Array(8))].map(b => b.toString(16).padStart(2, '0')).join('');
const snap = v => Math.round(v / SNAP) * SNAP;

function anchor(n, side, toward) {
  let s = side;
  if (!['top', 'right', 'bottom', 'left'].includes(s)) {
    const dx = (toward.x + toward.width / 2) - (n.x + n.width / 2), dy = (toward.y + toward.height / 2) - (n.y + n.height / 2);
    s = Math.abs(dx) * n.height >= Math.abs(dy) * n.width ? (dx >= 0 ? 'right' : 'left') : (dy >= 0 ? 'bottom' : 'top');
  }
  const p = { top: [n.x + n.width / 2, n.y], bottom: [n.x + n.width / 2, n.y + n.height], left: [n.x, n.y + n.height / 2], right: [n.x + n.width, n.y + n.height / 2] }[s];
  return [p[0], p[1], s];
}
function curve(a, b) {
  const len = Math.min(160, Math.max(30, Math.hypot(b[0] - a[0], b[1] - a[1]) / 2.5));
  const pull = ([x, y, s]) => s === 'top' ? [x, y - len] : s === 'bottom' ? [x, y + len] : s === 'left' ? [x - len, y] : [x + len, y];
  return [[a[0], a[1]], pull(a), pull(b), [b[0], b[1]]];
}
const pathOf = p => `M${p[0][0]} ${p[0][1]} C${p[1][0]} ${p[1][1]} ${p[2][0]} ${p[2][1]} ${p[3][0]} ${p[3][1]}`;
function head(from, tip) {
  const dx = tip[0] - from[0], dy = tip[1] - from[1], l = Math.hypot(dx, dy) || 1, ux = dx / l, uy = dy / l;
  const bx = tip[0] - ux * 14, by = tip[1] - uy * 14;
  return `${tip[0]},${tip[1]} ${bx - uy * 7},${by + ux * 7} ${bx + uy * 7},${by - ux * 7}`;
}
const SVG = 'http://www.w3.org/2000/svg';
const svg = (tag, attrs = {}) => { const e = document.createElementNS(SVG, tag); for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, v); return e; };

export async function openCanvas(host, path, ctx) {
  const { api, h, go, noteHash, stem, flash, ro } = ctx;
  const data = await api(`canvas?path=${encodeURIComponent(path)}`);
  let doc;
  try { doc = JSON.parse(data.text || '{}'); } catch { doc = null; }
  if (!doc || typeof doc !== 'object' || Array.isArray(doc) || data.problem) {
    host.replaceChildren(h('div', { class: 'md' }, h('p', { class: 'base-problem' }, `This canvas cannot be shown: ${data.problem || 'it is not a JSON object'}. Press E to fix its JSON; nothing is changed until it reads.`)));
    return { close() {} };
  }
  doc.nodes = Array.isArray(doc.nodes) ? doc.nodes : [];
  doc.edges = Array.isArray(doc.edges) ? doc.edges : [];
  const html = new Map(Object.entries(data.html || {}));
  const shownText = new Map(doc.nodes.filter(n => n.type === 'text').map(n => [n.id, n.text || '']));
  let mtime = data.mtime;
  const cam = { x: 0, y: 0, z: 1 };
  const sel = new Set();
  const undo = [], redo = [];
  let saveT = 0, closed = false, saving = null;

  // ------------------------------------------------------------ DOM
  const world = h('div', { class: 'cvx-world' });
  const edgesSvg = svg('svg', { class: 'cvx-edges', 'aria-hidden': 'true' });
  world.append(edgesSvg);
  const stage = h('div', { class: 'cvx-stage' }, world);
  const zoomLbl = h('span', { class: 'cvx-zoom' }, '100%');
  const btn = (label, title, f, cls = '') => h('button', { class: `cvx-btn ${cls}`, type: 'button', title, onclick: f }, label);
  const swatches = h('span', { class: 'cvx-swatches' }, ...PRESETS.map(c => h('button', { class: `cvx-sw cv-c${c}`, type: 'button', title: `colour ${c}`, onclick: () => paint(c) })), h('button', { class: 'cvx-sw none', type: 'button', title: 'no colour', onclick: () => paint(null) }, '×'));
  const bar = h('div', { class: 'cvx-bar' },
    ...(ro ? [] : [btn('+ Text', 'a text card (or double-click the board)', () => addText(center())), btn('+ Note', 'a note or file from the vault', () => pickNote(center())), btn('+ Link', 'a web link', () => addLink(center())), btn('Group', 'group the selected cards', () => group()), swatches]),
    h('span', { class: 'cvx-grow' }),
    btn('−', 'zoom out (-)', () => zoomBy(1 / 1.25)), zoomLbl, btn('+', 'zoom in (+)', () => zoomBy(1.25)), btn('Fit', 'show everything (F)', () => fit()),
    ...(ro ? [] : [btn('↶', 'undo (Ctrl+Z)', () => doUndo()), btn('↷', 'redo (Ctrl+Shift+Z)', () => doRedo())]));
  const hint = h('div', { class: 'cvx-hint' }, ro ? 'Drag to look around · Ctrl+scroll to zoom · F to fit' : 'Double-click to add a card · drag a dot to connect · Delete removes · Ctrl+scroll zooms · F fits');
  const root = h('div', { class: 'cvx', tabindex: '0', 'aria-label': `Canvas ${stem(path)}` }, bar, stage, hint);
  // the toolbar never takes the keys away from the board (Delete, Ctrl+Z after a colour)
  bar.addEventListener('mousedown', e => { if (e.target.closest('button')) e.preventDefault(); });
  host.replaceChildren(root);
  const els = new Map(); // id -> element

  // ------------------------------------------------------------ helpers
  const byId = id => doc.nodes.find(n => n.id === id);
  const toWorld = (cx, cy) => { const r = stage.getBoundingClientRect(); return [(cx - r.left) / cam.z + cam.x, (cy - r.top) / cam.z + cam.y]; };
  const center = () => { const r = stage.getBoundingClientRect(); return toWorld(r.left + r.width / 2, r.top + r.height / 2); };
  function applyCam() {
    world.style.transform = `translate(${-cam.x * cam.z}px, ${-cam.y * cam.z}px) scale(${cam.z})`;
    zoomLbl.textContent = `${Math.round(cam.z * 100)}%`;
    stage.style.setProperty('--grid', `${Math.max(8, 20 * cam.z)}px`);
    stage.style.backgroundPosition = `${-cam.x * cam.z}px ${-cam.y * cam.z}px`;
    try { sessionStorage.setItem(`tempo-vault:cam:${path}`, JSON.stringify(cam)); } catch { /* fine */ }
  }
  function zoomAt(cx, cy, f) {
    const [wx, wy] = toWorld(cx, cy);
    cam.z = Math.min(4, Math.max(0.1, cam.z * f));
    const r = stage.getBoundingClientRect();
    cam.x = wx - (cx - r.left) / cam.z; cam.y = wy - (cy - r.top) / cam.z;
    applyCam();
  }
  function zoomBy(f) { const r = stage.getBoundingClientRect(); zoomAt(r.left + r.width / 2, r.top + r.height / 2, f); }
  function fit() {
    const r = stage.getBoundingClientRect();
    if (!doc.nodes.length) { cam.x = -r.width / 2; cam.y = -r.height / 2; cam.z = 1; return applyCam(); }
    const x0 = Math.min(...doc.nodes.map(n => n.x)), y0 = Math.min(...doc.nodes.map(n => n.y));
    const x1 = Math.max(...doc.nodes.map(n => n.x + n.width)), y1 = Math.max(...doc.nodes.map(n => n.y + n.height));
    const pad = 60;
    cam.z = Math.min(1.5, Math.max(0.1, Math.min(r.width / (x1 - x0 + pad * 2), r.height / (y1 - y0 + pad * 2))));
    cam.x = (x0 + x1) / 2 - r.width / 2 / cam.z; cam.y = (y0 + y1) / 2 - r.height / 2 / cam.z;
    applyCam();
  }
  function color(el, c, prop = '--cc') {
    for (const p of PRESETS) el.classList.remove(`cv-c${p}`);
    el.style.removeProperty(prop);
    if (!okColor(c)) return;
    if (PRESETS.includes(c)) el.classList.add(`cv-c${c}`); else el.style.setProperty(prop, c);
  }

  // ------------------------------------------------------------ changes, undo, saving
  function change(f) {
    if (ro) return;
    undo.push(JSON.stringify(doc));
    if (undo.length > 200) undo.shift();
    redo.length = 0;
    f();
    draw();
    queueSave();
  }
  function restore(snapshot) {
    doc = JSON.parse(snapshot);
    for (const id of [...sel]) if (!byId(id) && !doc.edges.some(e => e.id === id)) sel.delete(id);
    draw();
    refreshTexts();
    queueSave();
  }
  function doUndo() { if (ro || !undo.length) return; redo.push(JSON.stringify(doc)); restore(undo.pop()); }
  function doRedo() { if (ro || !redo.length) return; undo.push(JSON.stringify(doc)); restore(redo.pop()); }
  function queueSave() { clearTimeout(saveT); saveT = setTimeout(save, 600); ctx.status('unsaved'); }
  async function save(force) {
    clearTimeout(saveT);
    saveT = 0;
    if (saving) await saving;
    const text = JSON.stringify(doc, null, '\t');
    saving = (async () => {
      try {
        const r = await api('write', { path, text, mtime: force ? null : mtime });
        mtime = r.mtime;
        ctx.saved(mtime, text);
      } catch (e) {
        if (e.data && e.data.conflict) ctx.conflict(e.data);
        else flash(`canvas not saved: ${e.message}`);
      } finally { saving = null; }
    })();
    return saving;
  }
  async function refreshTexts() {
    for (const n of doc.nodes) {
      if (n.type === 'text' && shownText.get(n.id) !== (n.text || '')) await renderText(n);
    }
  }
  async function renderText(n) {
    try {
      const r = await api('preview', { path, text: n.text || '' });
      html.set(n.id, r.html);
      shownText.set(n.id, n.text || '');
      const el = els.get(n.id);
      if (el) { const b = el.querySelector('.cvx-body'); if (b && !b.querySelector('textarea')) b.innerHTML = r.html; } // escaped by the server's renderer
    } catch (e) { flash(e.message); }
  }
  async function renderFile(n) {
    if (!n.file || IMG.test(n.file)) return;
    try {
      const r = await api(`render?path=${encodeURIComponent(n.file)}`);
      html.set(n.id, r.html);
      draw(true);
    } catch { /* missing note: the card says so */ }
  }

  // ------------------------------------------------------------ drawing
  function nodeEl(n) {
    let el = els.get(n.id);
    const kind = n.type;
    if (!el || el.dataset.type !== kind) {
      if (el) el.remove();
      el = h('div', { class: `cvx-node cvx-${kind}`, 'data-id': n.id, 'data-type': kind });
      if (kind === 'group') el.append(h('div', { class: 'cvx-glabel' }));
      else el.append(h('div', { class: 'cvx-body' }));
      if (!ro) {
        el.append(h('div', { class: 'cvx-resize', title: 'resize' }));
        for (const s of ['top', 'right', 'bottom', 'left']) el.append(h('div', { class: `cvx-port ${s}`, 'data-side': s, title: 'drag to another card to connect' }));
      }
      if (kind === 'group') world.insertBefore(el, edgesSvg); else world.append(el);
      els.set(n.id, el);
      el.dataset.drawn = '';
    }
    return el;
  }
  function fillBody(el, n) {
    const key = `${n.type}|${n.text ?? ''}|${n.file ?? ''}|${n.subpath ?? ''}|${n.url ?? ''}|${n.label ?? ''}|${html.has(n.id)}`;
    if (el.dataset.drawn === key) return;
    el.dataset.drawn = key;
    if (n.type === 'group') { el.querySelector('.cvx-glabel').textContent = n.label || ''; return; }
    const body = el.querySelector('.cvx-body');
    if (body.querySelector('textarea')) return;
    if (n.type === 'text') { body.innerHTML = html.get(n.id) || ''; if (!n.text) body.append(h('p', { class: 'cvx-empty' }, 'Double-click to write')); return; } // escaped by the server
    if (n.type === 'link') {
      // the card itself selects and drags; the button opens the page
      const open = okUrl(n.url) ? h('button', { class: 'cvx-open', type: 'button', title: 'open in a new tab', onclick: () => window.open(n.url, '_blank', 'noopener,noreferrer') }, '↗ open') : null;
      let host = '';
      try { host = new URL(n.url).host; } catch { /* shown as written */ }
      body.replaceChildren(h('div', { class: 'cvx-lhost' }, host || 'link'), h('div', { class: 'cvx-url' }, n.url || '(no address)'), open);
      return;
    }
    if (n.type === 'file') {
      const title = h('button', { class: 'cvx-ftitle', type: 'button', title: 'open it', onclick: () => go(noteHash(n.file, n.subpath ? n.subpath.replace(/^#/, '') : null)) }, `${stem(n.file)}${n.subpath || ''}`);
      if (IMG.test(n.file)) body.replaceChildren(h('img', { class: 'cvx-img', src: `/vault/file?path=${encodeURIComponent(n.file)}`, alt: n.file, draggable: 'false' }));
      else if (html.has(n.id)) { const inner = h('div', { class: 'cvx-note' }); inner.innerHTML = html.get(n.id); body.replaceChildren(title, inner); } // escaped by the server
      else body.replaceChildren(title, h('p', { class: 'cvx-empty' }, /\.md$/i.test(n.file) ? 'not in the vault (yet)' : n.file));
    }
  }
  function draw(bodiesOnly) {
    const live = new Set();
    for (const n of doc.nodes) {
      if (!Number.isFinite(n.x) || !Number.isFinite(n.y)) continue;
      const el = nodeEl(n);
      live.add(n.id);
      el.style.left = `${n.x}px`; el.style.top = `${n.y}px`;
      el.style.width = `${Math.max(40, n.width)}px`; el.style.height = `${Math.max(30, n.height)}px`;
      color(el, n.color);
      el.classList.toggle('sel', sel.has(n.id));
      fillBody(el, n);
    }
    for (const [id, el] of els) if (!live.has(id)) { el.remove(); els.delete(id); }
    if (bodiesOnly) return;
    drawEdges();
    hint.hidden = doc.nodes.length > 0;
  }
  function drawEdges(temp) {
    edgesSvg.replaceChildren();
    for (const e of doc.edges) {
      const a = byId(e.fromNode), b = byId(e.toNode);
      if (!a || !b) continue;
      const pa = anchor(a, e.fromSide, b), pb = anchor(b, e.toSide, a), pts = curve(pa, pb);
      const g = svg('g', { class: `cvx-edge${sel.has(e.id) ? ' sel' : ''}`, 'data-edge': e.id });
      const d = pathOf(pts);
      g.append(svg('path', { d, class: 'hit' }), svg('path', { d, class: 'line' }));
      if (e.toEnd !== 'none') g.append(svg('polygon', { points: head(pts[2], pts[3]) }));
      if (e.fromEnd === 'arrow') g.append(svg('polygon', { points: head(pts[1], pts[0]) }));
      if (e.label) {
        const t = svg('text', { x: (pts[0][0] + 3 * pts[1][0] + 3 * pts[2][0] + pts[3][0]) / 8, y: (pts[0][1] + 3 * pts[1][1] + 3 * pts[2][1] + pts[3][1]) / 8, 'text-anchor': 'middle' });
        t.textContent = e.label;
        g.append(t);
      }
      if (okColor(e.color)) { if (PRESETS.includes(e.color)) g.classList.add(`cv-c${e.color}`); else g.style.setProperty('--cc', e.color); }
      edgesSvg.append(g);
    }
    if (temp) edgesSvg.append(svg('path', { d: pathOf(curve(temp[0], temp[1])), class: 'cvx-temp' }));
  }

  // ------------------------------------------------------------ adding
  function addNode(n, edit) {
    change(() => { doc.nodes.push(n); sel.clear(); sel.add(n.id); });
    if (edit) editText(n.id);
    return n;
  }
  function addText([x, y], edit = true) {
    return addNode({ id: newId(), type: 'text', text: '', x: snap(x - 130), y: snap(y - 40), width: 260, height: 100 }, edit);
  }
  function addFile(file, [x, y]) {
    const img = IMG.test(file);
    const n = addNode({ id: newId(), type: 'file', file, x: snap(x - 150), y: snap(y - 100), width: img ? 300 : 320, height: img ? 220 : 260 });
    renderFile(n);
    return n;
  }
  function inlineInput(placeholder, done, value = '') {
    const inp = h('input', { class: 'cvx-input', type: 'text', placeholder, value, spellcheck: 'false' });
    const box = h('div', { class: 'cvx-pop' }, inp);
    root.append(box);
    inp.focus();
    inp.select();
    let over = false;
    const end = ok => { if (over) return; over = true; box.remove(); root.focus(); if (ok) done(inp.value.trim()); };
    inp.addEventListener('keydown', e => { e.stopPropagation(); if (e.key === 'Enter') end(true); else if (e.key === 'Escape') end(false); });
    inp.addEventListener('blur', () => end(false));
    return { inp, box, end };
  }
  function addLink(at) {
    inlineInput('https://…', url => {
      if (!url) return;
      if (!okUrl(url)) return flash('a link card takes an http(s) address');
      addNode({ id: newId(), type: 'link', url, x: snap(at[0] - 160), y: snap(at[1] - 40), width: 320, height: 90 });
    });
  }
  function pickNote(at) {
    const list = h('div', { class: 'cvx-picks' });
    const { inp, box, end } = inlineInput('A note or file…', () => {});
    box.append(list);
    let items = [], pick = 0;
    const show = () => list.replaceChildren(...items.map((it, k) => h('button', { class: `cvx-pick${k === pick ? ' on' : ''}`, type: 'button', onmousedown: ev => { ev.preventDefault(); choose(k); } }, h('b', {}, it.title), h('i', {}, it.path))));
    const choose = k => { const it = items[k]; if (!it) return; end(false); addFile(it.path, at); };
    const look = async () => {
      const q = inp.value;
      const r = await api(`suggest?q=${encodeURIComponent(q)}&limit=12`).catch(() => ({ items: [] }));
      const files = (ctx.files() || []).filter(f => !q || f.path.toLowerCase().includes(q.toLowerCase())).slice(0, 6).map(f => ({ title: f.path.split('/').pop(), path: f.path }));
      if (inp.value !== q) return;
      items = [...r.items, ...files.filter(f => !/\.(canvas|base)$/i.test(f.path))];
      pick = 0;
      show();
    };
    inp.addEventListener('input', look);
    inp.addEventListener('keydown', e => {
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') { e.preventDefault(); pick = (pick + (e.key === 'ArrowDown' ? 1 : -1) + items.length) % Math.max(items.length, 1); show(); }
      if (e.key === 'Enter') { e.preventDefault(); choose(pick); }
    });
    look();
  }
  function group() {
    const nodes = [...sel].map(byId).filter(n => n && n.type !== 'group');
    if (!nodes.length) {
      const [x, y] = center();
      return addNode({ id: newId(), type: 'group', label: 'Group', x: snap(x - 200), y: snap(y - 150), width: 400, height: 300 });
    }
    const pad = 30;
    const x0 = Math.min(...nodes.map(n => n.x)) - pad, y0 = Math.min(...nodes.map(n => n.y)) - pad;
    const x1 = Math.max(...nodes.map(n => n.x + n.width)) + pad, y1 = Math.max(...nodes.map(n => n.y + n.height)) + pad;
    const g = { id: newId(), type: 'group', label: 'Group', x: snap(x0), y: snap(y0), width: snap(x1 - x0), height: snap(y1 - y0) };
    change(() => { doc.nodes.unshift(g); sel.clear(); sel.add(g.id); });
    editLabel(g.id);
  }
  function paint(c) {
    if (!sel.size) return flash('select a card or an arrow first');
    change(() => {
      for (const id of sel) {
        const t = byId(id) || doc.edges.find(e => e.id === id);
        if (!t) continue;
        if (c) t.color = c; else delete t.color;
      }
    });
  }
  function removeSelected() {
    if (!sel.size) return;
    change(() => {
      const gone = new Set(sel);
      doc.nodes = doc.nodes.filter(n => !gone.has(n.id));
      doc.edges = doc.edges.filter(e => !gone.has(e.id) && !gone.has(e.fromNode) && !gone.has(e.toNode));
      sel.clear();
    });
    flash('removed from the canvas · Ctrl+Z brings it back (the file keeps every version)');
  }
  function duplicate() {
    const nodes = [...sel].map(byId).filter(Boolean);
    if (!nodes.length) return;
    change(() => {
      sel.clear();
      for (const n of nodes) { const c = { ...JSON.parse(JSON.stringify(n)), id: newId(), x: n.x + 40, y: n.y + 40 }; doc.nodes.push(c); sel.add(c.id); if (html.has(n.id)) html.set(c.id, html.get(n.id)); if (c.type === 'text') shownText.set(c.id, c.text || ''); }
    });
  }

  // ------------------------------------------------------------ editing text and labels
  function editText(id) {
    const n = byId(id), el = els.get(id);
    if (ro || !n || !el || n.type !== 'text') return;
    const body = el.querySelector('.cvx-body');
    const ta = h('textarea', { class: 'cvx-edit', spellcheck: 'true', 'aria-label': 'card text' });
    ta.value = n.text || '';
    body.replaceChildren(ta);
    el.classList.add('editing');
    ta.focus();
    let over = false;
    const end = () => {
      if (over) return; over = true;
      el.classList.remove('editing');
      el.dataset.drawn = '';
      const v = ta.value;
      body.replaceChildren(); // the rendered text comes back in place of the box
      if (v !== (n.text || '')) { change(() => { byId(id).text = v; }); renderText(byId(id)); }
      else draw(true);
      root.focus();
    };
    ta.addEventListener('keydown', e => { e.stopPropagation(); if (e.key === 'Escape' || (e.key === 'Enter' && (e.ctrlKey || e.metaKey))) { e.preventDefault(); end(); } });
    ta.addEventListener('blur', end);
    ta.addEventListener('pointerdown', e => e.stopPropagation());
    ta.addEventListener('wheel', e => e.stopPropagation(), { passive: true });
  }
  function editLabel(id) {
    const n = byId(id);
    if (ro || !n) return;
    inlineInput('Group name', v => change(() => { byId(id).label = v; }), n.label || '');
  }
  function editEdgeLabel(id) {
    const e = doc.edges.find(x => x.id === id);
    if (ro || !e) return;
    inlineInput('Arrow label', v => change(() => { const t = doc.edges.find(x => x.id === id); if (v) t.label = v; else delete t.label; }), e.label || '');
  }

  // ------------------------------------------------------------ pointer: pan, select, move, resize, connect
  let drag = null;
  stage.addEventListener('pointerdown', e => {
    if (e.button !== 0 && e.button !== 1) return;
    root.focus({ preventScroll: true });
    const t = e.target;
    if (t.closest('a, textarea, .cvx-ftitle, .cvx-open, input')) return;
    const nodeEl = t.closest('.cvx-node');
    const edgeEl = t.closest('.cvx-edge');
    const [wx, wy] = toWorld(e.clientX, e.clientY);
    if (!ro && t.classList.contains('cvx-port') && nodeEl) {
      const n = byId(nodeEl.dataset.id);
      drag = { kind: 'link', from: n, side: t.dataset.side, cx: e.clientX, cy: e.clientY, moved: false };
    } else if (!ro && t.classList.contains('cvx-resize') && nodeEl) {
      const n = byId(nodeEl.dataset.id);
      drag = { kind: 'size', n, w: n.width, h: n.height, sx: wx, sy: wy, before: JSON.stringify(doc) };
    } else if (nodeEl && e.button === 0) {
      const id = nodeEl.dataset.id;
      if (e.shiftKey || e.metaKey || e.ctrlKey) { sel.has(id) ? sel.delete(id) : sel.add(id); }
      else if (!sel.has(id)) { sel.clear(); sel.add(id); }
      // a group carries the cards inside it
      const moving = new Set(sel);
      for (const id2 of sel) {
        const g = byId(id2);
        if (g && g.type === 'group') for (const n of doc.nodes) if (n.id !== g.id && n.x >= g.x && n.y >= g.y && n.x + n.width <= g.x + g.width && n.y + n.height <= g.y + g.height) moving.add(n.id);
      }
      drag = { kind: 'move', sx: wx, sy: wy, start: [...moving].map(i => { const n = byId(i); return n && [n, n.x, n.y]; }).filter(Boolean), moved: false, before: JSON.stringify(doc) };
      draw();
    } else if (edgeEl && e.button === 0) {
      const id = edgeEl.dataset.edge;
      if (!(e.shiftKey || e.metaKey || e.ctrlKey)) sel.clear();
      sel.add(id);
      draw();
      return;
    } else if (e.shiftKey && !ro) {
      drag = { kind: 'box', sx: wx, sy: wy, box: h('div', { class: 'cvx-box' }) };
      world.append(drag.box);
    } else {
      drag = { kind: 'pan', cx: e.clientX, cy: e.clientY, x: cam.x, y: cam.y, moved: false };
      stage.classList.add('panning');
    }
    stage.setPointerCapture(e.pointerId);
    e.preventDefault();
  });
  stage.addEventListener('pointermove', e => {
    if (!drag) return;
    const [wx, wy] = toWorld(e.clientX, e.clientY);
    if (drag.kind === 'pan') {
      cam.x = drag.x - (e.clientX - drag.cx) / cam.z; cam.y = drag.y - (e.clientY - drag.cy) / cam.z;
      drag.moved ||= Math.hypot(e.clientX - drag.cx, e.clientY - drag.cy) > 3;
      applyCam();
    } else if (drag.kind === 'move') {
      const dx = wx - drag.sx, dy = wy - drag.sy;
      if (!drag.moved && Math.hypot(dx, dy) * cam.z < 3) return;
      drag.moved = true;
      for (const [n, x, y] of drag.start) { n.x = e.altKey ? x + dx : snap(x + dx); n.y = e.altKey ? y + dy : snap(y + dy); }
      draw();
    } else if (drag.kind === 'size') {
      drag.n.width = Math.max(60, snap(drag.w + wx - drag.sx)); drag.n.height = Math.max(40, snap(drag.h + wy - drag.sy));
      draw();
    } else if (drag.kind === 'link') {
      drag.moved ||= Math.hypot(e.clientX - drag.cx, e.clientY - drag.cy) > 8;
      const a = anchor(drag.from, drag.side, drag.from);
      const over = document.elementFromPoint(e.clientX, e.clientY)?.closest('.cvx-node');
      for (const el of world.querySelectorAll('.cvx-node.target')) el.classList.remove('target');
      if (over && over.dataset.id !== drag.from.id) over.classList.add('target');
      const back = { top: 'bottom', bottom: 'top', left: 'right', right: 'left' }[a[2]];
      drawEdges([a, [wx, wy, back]]);
    } else if (drag.kind === 'box') {
      const x = Math.min(wx, drag.sx), y = Math.min(wy, drag.sy), w = Math.abs(wx - drag.sx), hh = Math.abs(wy - drag.sy);
      Object.assign(drag.box.style, { left: `${x}px`, top: `${y}px`, width: `${w}px`, height: `${hh}px` });
      drag.rect = [x, y, w, hh];
    }
  });
  const finish = e => {
    if (!drag) return;
    const d = drag;
    drag = null;
    stage.classList.remove('panning');
    if (d.kind === 'pan' && !d.moved && e.type === 'pointerup') { sel.clear(); draw(); }
    if ((d.kind === 'move' && d.moved) || d.kind === 'size') {
      if (d.before !== JSON.stringify(doc)) { undo.push(d.before); redo.length = 0; queueSave(); }
    }
    if (d.kind === 'box') {
      d.box.remove();
      if (d.rect) { const [x, y, w, hh] = d.rect; for (const n of doc.nodes) if (n.x >= x && n.y >= y && n.x + n.width <= x + w && n.y + n.height <= y + hh) sel.add(n.id); }
      draw();
    }
    if (d.kind === 'link' && d.moved && e.type === 'pointerup') {
      for (const el of world.querySelectorAll('.cvx-node.target')) el.classList.remove('target');
      const over = document.elementFromPoint(e.clientX, e.clientY)?.closest('.cvx-node');
      const [wx, wy] = toWorld(e.clientX, e.clientY);
      let to = over && byId(over.dataset.id);
      if (to && to.id === d.from.id) to = null;
      change(() => {
        if (!to) {
          // let go on the board: a new card there, joined to this one (as Obsidian does)
          to = { id: newId(), type: 'text', text: '', x: snap(wx - 130), y: snap(wy - 50), width: 260, height: 100 };
          doc.nodes.push(to);
        }
        const toSide = anchor(to, null, d.from)[2];
        doc.edges.push({ id: newId(), fromNode: d.from.id, fromSide: d.side, toNode: to.id, toSide });
      });
      if (to && !over) editText(to.id);
    }
    if (d.kind === 'link') drawEdges();
  };
  stage.addEventListener('pointerup', finish);
  stage.addEventListener('pointercancel', finish);
  stage.addEventListener('dblclick', e => {
    if (ro) return;
    const nodeEl = e.target.closest('.cvx-node'), edgeEl = e.target.closest('.cvx-edge');
    if (nodeEl) {
      const n = byId(nodeEl.dataset.id);
      if (n && n.type === 'text') editText(n.id);
      else if (n && n.type === 'group') editLabel(n.id);
      else if (n && n.type === 'link') inlineInput('https://…', url => { if (okUrl(url)) change(() => { byId(n.id).url = url; }); else if (url) flash('a link card takes an http(s) address'); }, n.url || '');
      return;
    }
    if (edgeEl) return editEdgeLabel(edgeEl.dataset.edge);
    addText(toWorld(e.clientX, e.clientY));
  });
  stage.addEventListener('wheel', e => {
    const body = e.target.closest('.cvx-body, .cvx-note');
    if (body && !e.ctrlKey && !e.metaKey && body.scrollHeight > body.clientHeight + 2) return; // let the card scroll
    e.preventDefault();
    if (e.ctrlKey || e.metaKey) zoomAt(e.clientX, e.clientY, Math.exp(-e.deltaY * 0.0025));
    else { cam.x += e.deltaX / cam.z; cam.y += e.deltaY / cam.z; applyCam(); }
  }, { passive: false });
  // drop notes ([[links]] from boards and lists) and files (pictures become attachments)
  stage.addEventListener('dragover', e => { if (!ro) { e.preventDefault(); stage.classList.add('drop'); } });
  stage.addEventListener('dragleave', () => stage.classList.remove('drop'));
  stage.addEventListener('drop', async e => {
    stage.classList.remove('drop');
    if (ro) return;
    e.preventDefault();
    const at = toWorld(e.clientX, e.clientY);
    const files = [...(e.dataTransfer?.files || [])];
    for (const f of files) {
      try {
        const r = await fetch(`/vault/upload?name=${encodeURIComponent(f.name || 'Pasted file')}&note=${encodeURIComponent(path)}`, { method: 'POST', headers: { 'Content-Type': 'application/octet-stream' }, body: f });
        const j = await r.json();
        if (!r.ok) throw new Error(j.error || `HTTP ${r.status}`);
        addFile(j.path, at);
        at[0] += 40; at[1] += 40;
      } catch (err) { flash(`could not add ${f.name}: ${err.message}`); }
    }
    if (files.length) return;
    const t = (e.dataTransfer?.getData('text/plain') || '').trim();
    const m = t.match(/^\[\[([^\]|#]+)/);
    if (m) {
      const r = await api(`suggest?q=${encodeURIComponent(m[1])}&limit=1`).catch(() => ({ items: [] }));
      if (r.items[0]) addFile(r.items[0].path, at);
    } else if (okUrl(t)) addNode({ id: newId(), type: 'link', url: t, x: snap(at[0] - 160), y: snap(at[1] - 40), width: 320, height: 90 });
    else if (t) { const n = addText(at, false); change(() => { byId(n.id).text = t; }); renderText(byId(n.id)); }
  });

  // ------------------------------------------------------------ keys (only while the canvas has focus)
  root.addEventListener('keydown', e => {
    if (e.target.matches('input, textarea, button')) return;
    const mod = e.ctrlKey || e.metaKey;
    const k = e.key;
    let used = true;
    if (!ro && (k === 'Delete' || k === 'Backspace')) removeSelected();
    else if (!ro && mod && k.toLowerCase() === 'z') e.shiftKey ? doRedo() : doUndo();
    else if (!ro && mod && k.toLowerCase() === 'y') doRedo();
    else if (mod && k.toLowerCase() === 'a') { sel.clear(); for (const n of doc.nodes) sel.add(n.id); draw(); }
    else if (!ro && mod && k.toLowerCase() === 'd') duplicate();
    else if (k === 'Escape' && sel.size) { sel.clear(); draw(); }
    else if (!mod && (k === 'f' || k === 'F')) fit();
    else if (!mod && (k === '+' || k === '=')) zoomBy(1.25);
    else if (!mod && (k === '-' || k === '_')) zoomBy(1 / 1.25);
    else if (!mod && k === '0') { cam.z = 1; applyCam(); }
    else if (!ro && !mod && k === 'Enter' && sel.size === 1) { const n = byId([...sel][0]); if (n && n.type === 'text') editText(n.id); else if (n && n.type === 'file') go(noteHash(n.file)); }
    else if (!ro && !mod && k.startsWith('Arrow') && sel.size) {
      const step = e.shiftKey ? 50 : SNAP, d = { ArrowLeft: [-step, 0], ArrowRight: [step, 0], ArrowUp: [0, -step], ArrowDown: [0, step] }[k];
      change(() => { for (const id of sel) { const n = byId(id); if (n) { n.x += d[0]; n.y += d[1]; } } });
    } else used = false;
    if (used) { e.preventDefault(); e.stopPropagation(); }
  });

  // ------------------------------------------------------------ start
  const ro2 = new ResizeObserver(() => applyCam());
  ro2.observe(stage);
  draw();
  let had = null;
  try { had = JSON.parse(sessionStorage.getItem(`tempo-vault:cam:${path}`) || 'null'); } catch { /* fine */ }
  if (had && Number.isFinite(had.z)) { Object.assign(cam, had); applyCam(); } else fit();
  root.focus({ preventScroll: true });
  return {
    // save what is waiting before the canvas goes away
    async close() { if (closed) return; closed = true; ro2.disconnect(); if (saveT) await save(); else if (saving) await saving; },
    get pending() { return !!saveT || !!saving; },
    keepMine: () => save(true),
    // leave without saving (the version on disk wins)
    drop() { closed = true; clearTimeout(saveT); saveT = 0; ro2.disconnect(); },
    fit,
    count: () => ({ cards: doc.nodes.length, arrows: doc.edges.length }),
  };
}
