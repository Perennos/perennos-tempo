// Perennos Tempo Vault, the app. Plain ES module, no framework, no build step. It talks only to
// /vault/* on this origin; every user string goes into the page as text, and only the server's
// escaped Markdown rendering is set as HTML.

const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];
const enc = encodeURIComponent;
function h(tag, attrs = {}, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k === 'class') e.className = v;
    else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v === true ? '' : v);
  }
  for (const k of kids.flat()) if (k !== null && k !== undefined && k !== false) e.append(k instanceof Node ? k : document.createTextNode(String(k)));
  return e;
}
const store = {
  get(k, d) { try { const v = localStorage.getItem('tempo-vault:' + k); return v === null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem('tempo-vault:' + k, JSON.stringify(v)); } catch { /* private window: fine */ } },
};
// Tempo's key, for the Tempo tab only: from #key=... when Perennos Tempo opens the vault (kept for
// this tab only and cleared from the address bar). The vault server passes it on and never reads
// Tempo's key file itself, so without it nothing here can reach the agent.
const KEY_SLOT = 'tempo-agent-key';
let agentKeyMem = null;
(function takeKey() {
  const m = location.hash.match(/(?:^#|[&?])key=([0-9a-fA-F]{16,128})(?=&|$)/);
  if (!m) return;
  agentKeyMem = m[1];
  try { sessionStorage.setItem(KEY_SLOT, m[1]); } catch { /* private window: kept in memory */ }
  let rest = location.hash.replace(/([&?])key=[0-9a-fA-F]+/, '').replace(/^#key=[0-9a-fA-F]+&?/, '#');
  if (rest === '#') rest = '';
  history.replaceState(null, '', location.pathname + location.search + rest);
})();
const agentKey = () => { try { return sessionStorage.getItem(KEY_SLOT) || agentKeyMem; } catch { return agentKeyMem; } };
async function api(path, body) {
  const headers = body === undefined ? {} : { 'Content-Type': 'application/json' };
  if (path.startsWith('agent/') && agentKey()) headers['X-Tempo-Key'] = agentKey();
  const opt = body === undefined ? { headers } : { method: 'POST', headers, body: JSON.stringify(body) };
  const r = await fetch('/vault/' + path, opt);
  let j = null;
  try { j = await r.json(); } catch { /* not JSON */ }
  if (!r.ok) { const e = new Error((j && j.error) || `HTTP ${r.status}`); e.status = r.status; e.data = j; throw e; }
  return j;
}
const debounce = (f, ms) => { let t = 0; return (...a) => { clearTimeout(t); t = setTimeout(() => f(...a), ms); }; };
const stem = p => p.split('/').pop().replace(/\.(md|base|canvas)$/i, '');
// .base and .canvas files open in the app like notes
const isDoc = p => /\.(base|canvas)$/i.test(p);
// a context line without its Markdown: [[a|b]] -> b, [[a]] -> a, **x** -> x
const plain = t => String(t).replace(/!?\[\[([^\]|]*\|)?([^\]]*)\]\]/g, '$2').replace(/\*\*|__|`/g, '').replace(/^\s*(#+|>|[-*+])\s+/, '');
const clock = d => d.toTimeString().slice(0, 5);
const ago = secs => {
  const s = Date.now() / 1000 - secs;
  if (s < 90) return 'just now';
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 86400) return `${Math.round(s / 3600)} h ago`;
  return new Date(secs * 1000).toISOString().slice(0, 10);
};
const reduced = () => matchMedia('(prefers-reduced-motion: reduce)').matches;

const S = {
  health: null, notes: new Map(), files: [], current: null, mode: 'read', dirty: false, saving: null,
  view: 'files', gen: 0, ro: false, recent: new Set(), collapsed: new Set(store.get('collapsed', [])), graph: null, mini: null,
  gopts: { tags: false, attachments: true, unresolved: false, orphans: true }, tab: store.get('tab', 'links'),
};

// ------------------------------------------------------------------ status line
function status(mid, save) {
  if (mid !== undefined) $('#st-mid').textContent = mid;
  if (save !== undefined) { const el = $('#st-save'); el.textContent = save; el.classList.toggle('dirty', S.dirty); }
}
function live(on, text) {
  $('#dot').classList.toggle('live', on);
  $('#st-live').textContent = text;
  $('#livebox').classList.toggle('off', !on);
  $('#livestate').textContent = on ? 'SSE' : 'OFF';
}

// ------------------------------------------------------------------ tree and side panel
async function loadTree() {
  const t = await api('tree');
  S.notes = new Map(t.notes.map(n => [n.path, n]));
  S.files = t.files;
  // the dot marks the few notes touched most recently (within a day)
  const now = Date.now() / 1000;
  S.recent = new Set(t.notes.filter(n => now - n.mtime < 86400).sort((a, b) => b.mtime - a.mtime).slice(0, 10).map(n => n.path));
  $('#vcount').textContent = S.notes.size;
  $('#n-files').textContent = S.notes.size || '';
  const open = t.notes.reduce((a, n) => a + n.open_tasks, 0);
  $('#n-tasks').textContent = open || '';
  const learned = t.notes.filter(n => n.path.startsWith('Tempo/Conversations/')).length;
  $('#n-learned').textContent = learned || '';
  const st = streak();
  status(`${S.notes.size} notes · ${S.files.length} files · ${open} open tasks${st > 1 ? ` · ${st}-day streak` : ''}`);
  if (S.view === 'files') renderSide();
}

function buildTree() {
  const root = { dirs: new Map(), items: [] };
  const add = (path, item) => {
    const parts = path.split('/');
    let node = root;
    for (const p of parts.slice(0, -1)) {
      if (!node.dirs.has(p)) node.dirs.set(p, { dirs: new Map(), items: [] });
      node = node.dirs.get(p);
    }
    node.items.push(item);
  };
  for (const n of S.notes.values()) add(n.path, { kind: 'note', ...n });
  if (store.get('showFiles', true)) for (const f of S.files) add(f.path, { kind: 'file', ...f, title: f.path.split('/').pop() });
  return root;
}

function treeList(node, prefix, filter) {
  const ul = h('ul', { class: 'tree' });
  const dirs = [...node.dirs.keys()].sort((a, b) => a.localeCompare(b, undefined, { sensitivity: 'base' }));
  for (const d of dirs) {
    const path = prefix ? `${prefix}/${d}` : d;
    const sub = treeList(node.dirs.get(d), path, filter);
    if (filter && !sub.children.length) continue;
    const closed = !filter && S.collapsed.has(path);
    const count = countNotes(node.dirs.get(d));
    const btn = h('button', { class: 'ti dir', type: 'button', 'aria-expanded': String(!closed), onclick: () => { S.collapsed.has(path) ? S.collapsed.delete(path) : S.collapsed.add(path); store.set('collapsed', [...S.collapsed]); renderSide(); } },
      h('span', { class: 'tw' }, closed ? '+' : '–'), h('span', { class: 'nm' }, d), h('span', { class: 'ct' }, count));
    const li = h('li', {}, btn);
    if (!closed) li.append(sub);
    ul.append(li);
  }
  const items = node.items.slice().sort((a, b) => (a.kind === b.kind ? 0 : a.kind === 'note' ? -1 : 1) || a.title.localeCompare(b.title, undefined, { sensitivity: 'base' }));
  for (const it of items) {
    if (filter && !(`${it.path} ${it.title}`.toLowerCase().includes(filter))) continue;
    const cur = S.current && S.current.path === it.path;
    const cls = `ti ${it.kind}${it.kind === 'note' && S.recent.has(it.path) ? ' recent' : ''}`;
    const doc = it.kind === 'file' && isDoc(it.path);
    const btn = it.kind === 'note' || doc
      ? h('button', { class: doc ? `ti vdoc v-${it.path.split('.').pop().toLowerCase()}` : cls, type: 'button', title: it.path, 'aria-current': cur ? 'true' : null, onclick: () => go(noteHash(it.path)) }, h('span', { class: 'tw' }, doc ? (/\.canvas$/i.test(it.path) ? '◫' : '▦') : ''), h('span', { class: 'nm' }, doc ? stem(it.path) : it.title))
      : h('a', { class: cls, href: `/vault/file?path=${enc(it.path)}`, target: '_blank', rel: 'noopener', title: it.path }, h('span', { class: 'tw' }, '·'), h('span', { class: 'nm' }, it.title));
    ul.append(h('li', {}, btn));
  }
  return ul;
}
const countNotes = n => n.items.filter(i => i.kind === 'note').length + [...n.dirs.values()].reduce((a, d) => a + countNotes(d), 0);

async function renderSide() {
  const body = $('#side-body');
  const f = $('#filter').value.trim();
  const title = { files: 'Files', search: 'Search', tasks: 'Open tasks', tags: 'Tags', health: 'Check', calendar: 'Calendar' }[S.view] || 'Files';
  $('#side-title').textContent = title;
  $('#filter').placeholder = S.view === 'search' ? 'Search: words, "phrase", tag:x, -not' : 'Filter';
  if (S.view === 'search') return runSearch(f);
  if (S.view === 'tasks') return renderTasks(f);
  if (S.view === 'tags') return renderTags(f);
  if (S.view === 'health') return renderHealth();
  if (S.view === 'calendar') return renderCalendar();
  body.replaceChildren();
  if (!S.notes.size) { body.append(h('p', { class: 'empty' }, 'No notes yet. Press N to write the first one.')); return; }
  body.append(treeList(buildTree(), '', f.toLowerCase()));
}

function highlight(text, ranges) {
  const cps = Array.from(text);
  const out = [];
  let at = 0;
  for (const [a, b] of (ranges || []).slice().sort((x, y) => x[0] - y[0])) {
    if (a < at) continue;
    out.push(cps.slice(at, a).join(''), h('mark', {}, cps.slice(a, b).join('')));
    at = b;
  }
  out.push(cps.slice(at).join(''));
  return out;
}

const runSearch = debounce(async q => {
  const body = $('#side-body');
  if (!q) { body.replaceChildren(h('p', { class: 'empty' }, 'Type to search every note.'), h('p', { class: 'hint' }, '"exact phrase" · -exclude · a OR b · word* · tag:#x · path:folder · file:name · task:text · [property:value]')); return; }
  try {
    const r = await api(`search?q=${enc(q)}&limit=80`);
    if ($('#filter').value.trim() !== q) return;
    body.replaceChildren(...(r.hits.length ? r.hits.map(x => h('button', { class: 'hit', type: 'button', onclick: () => go(noteHash(x.path, null, x.line)) },
      h('b', {}, x.title), h('span', { class: 'p' }, `${x.path}${x.line ? ' · line ' + x.line : ''}`), h('span', { class: 's' }, highlight(x.snippet, x.ranges)))) : [h('p', { class: 'empty' }, 'Nothing matches.')]));
  } catch (e) { body.replaceChildren(h('p', { class: 'empty' }, e.message)); }
}, 160);

async function renderTasks(f) {
  const r = await api(`tasks?rules=${enc('not done' + (f ? '\ntext includes ' + f : ''))}`);
  const body = $('#side-body');
  const groups = { Late: [], Today: [], Upcoming: [], 'No date': [] };
  for (const t of r.tasks) (t.due ? (t.due < r.today ? groups.Late : t.due === r.today ? groups.Today : groups.Upcoming) : groups['No date']).push(t);
  body.replaceChildren();
  for (const [g, list] of Object.entries(groups)) {
    if (!list.length) continue;
    body.append(h('div', { class: 'sec lbl' }, h('span', {}, g), h('span', { class: 'n' }, list.length)));
    for (const t of list) {
      const box = h('input', { type: 'checkbox', 'aria-label': 'done', onchange: async () => { await toggleTask(t.path, t.line, true); renderSide(); } });
      body.append(h('div', { class: 'hit' }, h('span', { class: 's' }, box, ' ', t.text), h('button', { class: 'bl', type: 'button', onclick: () => go(noteHash(t.path, null, t.line)) }, h('span', {}, `${t.path} · line ${t.line}${t.due ? ' · due ' + t.due : ''}`))));
    }
  }
  if (!r.tasks.length) body.append(h('p', { class: 'empty' }, 'No open tasks. Write one with - [ ] anywhere.'));
}

async function renderTags(f) {
  const r = await api('tags');
  $('#n-tags').textContent = r.tags.length || '';
  const body = $('#side-body');
  const list = r.tags.filter(t => !f || t.tag.includes(f.toLowerCase().replace(/^#/, '')));
  body.replaceChildren(...(list.length ? list.map(t => {
    const depth = t.tag.split('/').length - 1;
    return h('button', { class: 'ti', type: 'button', onclick: () => { setView('search'); $('#filter').value = `tag:#${t.tag}`; renderSide(); } },
      h('span', { class: 'tw' }, depth ? '└' : '#'), h('span', { class: 'nm' }, depth ? t.tag.split('/').pop() : t.tag), h('span', { class: 'ct' }, t.count));
  }) : [h('p', { class: 'empty' }, 'No tags yet. Add #tags in a note or tags: in its properties.')]));
}

async function renderHealth() {
  const [u, o, act] = await Promise.all([api('unresolved'), api('orphans'), api('activity').catch(() => null)]);
  $('#n-health').textContent = u.unresolved.length || '';
  const body = $('#side-body');
  body.replaceChildren();
  if (act) body.append(heatmap(act));
  body.append(h('div', { class: 'sec lbl' }, h('span', {}, 'Wanted notes'), h('span', { class: 'n' }, u.unresolved.length)));
  if (!u.unresolved.length) body.append(h('p', { class: 'hint' }, 'Every link points at a note.'));
  for (const w of u.unresolved.slice(0, 200)) {
    body.append(h('button', { class: 'bl missing', type: 'button', title: 'Create it', onclick: () => createNote(w.target, true) }, h('b', {}, `[[${w.target}]]`), h('span', {}, `wanted by ${w.count}: ${w.from.slice(0, 3).map(x => stem(x.source)).join(', ')}`)));
  }
  body.append(h('div', { class: 'sec lbl' }, h('span', {}, 'Orphans'), h('span', { class: 'n' }, o.orphans.length)));
  for (const p of o.orphans.slice(0, 300)) body.append(h('button', { class: 'bl', type: 'button', onclick: () => go(noteHash(p)) }, h('b', {}, stem(p)), h('span', {}, p)));
}

// A year of writing, a square a day (darker is busier), like a contribution graph.
function heatmap(act) {
  const day = s => new Date(`${s}T12:00:00`);
  const key = d => `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
  const end = day(act.today);
  const start = new Date(end);
  start.setDate(start.getDate() - 7 * 26 - start.getDay());
  const max = Math.max(1, ...Object.values(act.days || {}));
  const grid = h('div', { class: 'heat', role: 'img' });
  let total = 0, active = 0, streak = 0, best = 0;
  for (let d = new Date(start); d <= end; d.setDate(d.getDate() + 1)) {
    const n = (act.days || {})[key(d)] || 0;
    total += n;
    if (n) { active++; streak++; best = Math.max(best, streak); } else streak = 0;
    const lvl = n ? Math.min(4, 1 + Math.floor((n / max) * 3.99)) : 0;
    const k = key(d);
    grid.append(h('button', { class: `hd l${lvl}`, type: 'button', title: `${k}: ${n} edit${n === 1 ? '' : 's'}`, onclick: () => { setView('search'); $('#filter').value = k; renderSide(); } }));
  }
  grid.setAttribute('aria-label', `${active} active days in the last 26 weeks`);
  return h('div', { class: 'heat-box' }, h('div', { class: 'sec lbl' }, h('span', {}, 'Your half year in notes'), h('span', { class: 'n' }, total)), grid,
    h('p', { class: 'hint' }, `${active} active days · longest run ${best} day${best === 1 ? '' : 's'} · ${streak ? `${streak} in a row now` : 'write today to start a run'}`));
}

// A month of daily notes: a dot where one exists; a day opens its note (or starts it).
async function renderCalendar() {
  const body = $('#side-body');
  const now = new Date();
  S.cal ||= { y: now.getFullYear(), m: now.getMonth() };
  const { y, m } = S.cal;
  const key = d => `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
  const days = new Map();
  for (const n of S.notes.values()) { const d = dayOf(n.path); if (d) days.set(d, n.path); }
  const edited = new Set([...S.notes.values()].map(n => key(new Date(n.mtime * 1000))));
  const first = new Date(y, m, 1);
  const lead = (first.getDay() + 6) % 7; // weeks start on Monday
  const grid = h('div', { class: 'cal' }, ...['M', 'T', 'W', 'T', 'F', 'S', 'S'].map(d => h('span', { class: 'cal-dow' }, d)));
  for (let i = 0; i < lead; i++) grid.append(h('span', {}));
  const last = new Date(y, m + 1, 0).getDate();
  for (let d = 1; d <= last; d++) {
    const k = key(new Date(y, m, d));
    const path = days.get(k);
    const today = k === key(now);
    grid.append(h('button', { class: `cal-d${path ? ' has' : ''}${edited.has(k) ? ' edited' : ''}${today ? ' today' : ''}`, type: 'button', title: path ? `open ${k}` : `start ${k}`,
      onclick: async () => { if (path) return go(noteHash(path)); if (S.ro) return flashStatus('read-only vault'); try { const r = await api('daily', { date: k }); await loadTree(); go(noteHash(r.path)); } catch (e) { flashStatus(e.message); } } }, String(d)));
  }
  const shift = k => { const t = new Date(y, m + k, 1); S.cal = { y: t.getFullYear(), m: t.getMonth() }; renderCalendar(); };
  const month = first.toLocaleString(undefined, { month: 'long', year: 'numeric' });
  const inMonth = [...days.keys()].filter(k => k.startsWith(`${y}-${String(m + 1).padStart(2, '0')}`)).length;
  body.replaceChildren(
    h('div', { class: 'cal-head' }, h('button', { class: 'btn sm', type: 'button', onclick: () => shift(-1), 'aria-label': 'previous month' }, '‹'), h('b', {}, month), h('button', { class: 'btn sm', type: 'button', onclick: () => shift(1), 'aria-label': 'next month' }, '›')),
    grid,
    h('p', { class: 'hint' }, `${inMonth} daily note${inMonth === 1 ? '' : 's'} this month · a dot is a daily note, a line is a day you edited notes`),
    h('button', { class: 'btn sm', type: 'button', onclick: () => { S.cal = { y: now.getFullYear(), m: now.getMonth() }; renderCalendar(); } }, 'This month'));
}

// ------------------------------------------------------------------ hover previews
// Rest the pointer on a link to a note and the note shows beside it (Esc or moving away hides it).
const PV = { t: 0, el: null, cache: new Map() };
function previewSoon(a) {
  clearTimeout(PV.t);
  PV.t = setTimeout(async () => {
    const path = a.dataset.path;
    if (!path || !/\.md$/i.test(path) || !a.isConnected) return;
    let html = PV.cache.get(path);
    if (html === undefined) {
      try { html = (await api(`render?path=${enc(path)}`)).html; } catch { html = null; }
      PV.cache.set(path, html);
    }
    if (!html || !a.matches(':hover')) return;
    hidePreview();
    const art = h('article', { class: 'md' });
    art.innerHTML = html; // escaped by the server's renderer
    const card = h('div', { class: 'pv', role: 'tooltip' }, h('div', { class: 'pv-head' }, h('span', {}, (S.notes.get(path) || {}).title || stem(path)), h('i', {}, path)), art);
    document.body.append(card);
    const r = a.getBoundingClientRect(), w = Math.min(460, innerWidth - 24), tall = Math.min(380, innerHeight * 0.55);
    card.style.width = `${w}px`;
    card.style.maxHeight = `${tall}px`;
    card.style.left = `${Math.max(12, Math.min(r.left, innerWidth - w - 12))}px`;
    card.style.top = `${r.bottom + 8 + tall > innerHeight ? Math.max(12, r.top - tall - 8) : r.bottom + 8}px`;
    card.addEventListener('mouseleave', () => setTimeout(() => { if (PV.el === card && !card.matches(':hover')) hidePreview(); }, 200));
    card.addEventListener('click', e => { const l = e.target.closest('a.internal[data-path]'); if (l) { e.preventDefault(); hidePreview(); go(noteHash(l.dataset.path, l.dataset.anchor)); } });
    PV.el = card;
  }, 450);
}
function hidePreview() { clearTimeout(PV.t); if (PV.el) { PV.el.remove(); PV.el = null; } }

// ------------------------------------------------------------------ present a note as slides
// Slides split at --- lines (or at each heading when there are none); arrows, space, Esc.
async function present() {
  const n = S.current;
  if (!n || n.kind) return flashStatus('open a note to present it');
  const r = await api(`render?path=${enc(n.path)}`);
  const tmp = h('div');
  tmp.innerHTML = r.html; // escaped by the server's renderer
  const kids = [...tmp.children].filter(el => !el.classList.contains('dup-title'));
  let slides = [[]];
  for (const el of kids) { if (el.tagName === 'HR') slides.push([]); else slides[slides.length - 1].push(el); }
  slides = slides.filter(s => s.length);
  if (slides.length <= 1) {
    slides = [];
    for (const el of kids) { if (/^H[1-3]$/.test(el.tagName) || !slides.length) slides.push([]); slides[slides.length - 1].push(el); }
  }
  if (!slides.length) return flashStatus('nothing to present');
  let i = 0;
  const stage = h('div', { class: 'slide md' });
  const count = h('div', { class: 'slide-n' });
  const bar = h('div', { class: 'slide-bar' });
  const deck = h('div', { class: 'deck', tabindex: '0', role: 'dialog', 'aria-label': `${n.title}, as slides` }, stage, bar, count, h('div', { class: 'slide-hint' }, '← → space · F full screen · Esc to leave'));
  const show = k => {
    i = Math.max(0, Math.min(slides.length - 1, k));
    stage.replaceChildren(...slides[i].map(x => x.cloneNode(true)));
    for (const box of stage.querySelectorAll('input')) box.disabled = true;
    count.textContent = `${i + 1} / ${slides.length}`;
    bar.style.width = `${((i + 1) / slides.length) * 100}%`;
  };
  const end = () => { deck.remove(); document.removeEventListener('keydown', keys, true); if (document.fullscreenElement) document.exitFullscreen().catch(() => {}); };
  const keys = e => {
    const k = e.key;
    if (['ArrowRight', 'ArrowDown', 'PageDown', ' ', 'Enter'].includes(k)) show(i + 1);
    else if (['ArrowLeft', 'ArrowUp', 'PageUp', 'Backspace'].includes(k)) show(i - 1);
    else if (k === 'Home') show(0);
    else if (k === 'End') show(slides.length - 1);
    else if (k === 'Escape') end();
    else if (k === 'f' || k === 'F') { if (document.fullscreenElement) document.exitFullscreen().catch(() => {}); else deck.requestFullscreen?.().catch(() => {}); }
    else return;
    e.preventDefault();
    e.stopPropagation();
  };
  document.addEventListener('keydown', keys, true);
  deck.addEventListener('click', e => { if (!e.target.closest('a')) show(i + (e.clientX > innerWidth / 3 ? 1 : -1)); });
  document.body.append(deck);
  show(0);
  deck.focus();
}

// Obsidian opens the same note (obsidian://open, when Obsidian is installed and knows this vault).
function openInObsidian() {
  if (!S.current) return;
  location.href = `obsidian://open?vault=${enc(S.health && S.health.name || '')}&file=${enc(S.current.path.replace(/\.md$/i, ''))}`;
}

// ------------------------------------------------------------------ routing
const noteHash = (p, anchor, line) => `#/n/${p.split('/').map(enc).join('/')}${anchor ? '?h=' + enc(anchor) : line ? '?l=' + line : ''}`;
function go(hash) { if (location.hash === hash) route(); else location.hash = hash; }
function parseHash() {
  const m = location.hash.match(/^#\/(n|s|v)\/([^?]*)(?:\?(.*))?$/);
  if (!m) return null;
  const q = new URLSearchParams(m[3] || '');
  return { kind: m[1], arg: m[2].split('/').map(decodeURIComponent).join('/'), anchor: q.get('h'), line: Number(q.get('l')) || 0 };
}
async function route() {
  const r = parseHash();
  if (!r) return;
  if (r.kind === 'n') { await openNote(r.arg, r.anchor, r.line); if (S.view === 'graph' && !S.both) setView('files'); else applyLayout(S.view); }
  else if (r.kind === 's') { setView('search'); $('#filter').value = r.arg; renderSide(); }
  else if (r.kind === 'v') setView(r.arg);
}

function setView(v) {
  if (['daily', 'learned', 'new', 'theme'].includes(v)) return action(v);
  const was = S.view;
  S.view = v;
  for (const b of $$('.rail button[data-view]')) {
    if (b.dataset.view === v) b.setAttribute('aria-current', 'page'); else b.removeAttribute('aria-current');
  }
  const graph = v === 'graph';
  applyLayout(was);
  if (graph) { document.body.classList.remove('side-on'); return; }
  if (v !== was) document.body.classList.add('side-on');
  renderSide();
}

// Shows the graph, the note or both, from the current view and the Both switch.
function applyLayout(was) {
  const graph = S.view === 'graph', showGraph = graph || S.both;
  document.body.classList.toggle('lay-both', S.both);
  $('#graph').hidden = !showGraph;
  $('#doc').hidden = graph && !S.both;
  syncLayout();
  moveBar();
  if (showGraph) { if (!S.graph || (graph && was !== 'graph')) openGraphView(); } else closeGraphView();
}

// Brain (the graph, full), Dashboard (notes and details) or Both (side by side; stacked on a phone).
function setLayout(l) {
  if (!['brain', 'dashboard', 'both'].includes(l)) l = 'dashboard';
  store.set('layout', l);
  S.both = l === 'both';
  if (l === 'brain') { if (S.view === 'graph') setView('graph'); else go('#/v/graph'); }
  else if (S.view === 'graph' && l === 'dashboard') go('#/v/files');
  else setView(S.view);
}
function syncLayout() {
  const l = S.both ? 'both' : S.view === 'graph' ? 'brain' : 'dashboard';
  for (const b of $$('#layout button')) b.setAttribute('aria-pressed', String(b.dataset.layout === l));
}
// The orange bar slides to the current rail item (transform only).
function moveBar() {
  const bar = $('#rail-bar'), cur = $('.rail button[aria-current=page]');
  if (!bar || !cur) return;
  bar.style.height = `${cur.offsetHeight}px`;
  bar.style.transform = `translateY(${cur.offsetTop}px)`;
}

async function action(v) {
  if (v === 'daily') {
    try { const r = await api('daily', {}); await loadTree(); go(noteHash(r.path)); } catch (e) { flashStatus(e.message); }
  } else if (v === 'learned') {
    const p = 'Tempo/Tempo Learned.md';
    if (S.notes.has(p)) return go(noteHash(p));
    const st = await api('learn').catch(() => ({}));
    sheet('Tempo learns every day', [
      h('p', { class: 'hint' }, 'Each conversation with Tempo becomes a note here: what was asked, what was decided, what is left to do, linked to the notes it talks about. Nothing has been learned in this vault yet.'),
      h('p', { class: 'hint' }, 'Run it once:'), h('pre', {}, 'tempo-vault learn --vault <this vault> --agent-data <tempo agent data folder>'),
      h('p', { class: 'hint' }, 'Then let it run every hour (hidden, no windows):'), h('pre', {}, 'tempo-vault schedule --install --vault <this vault>'),
      st.last_run ? h('p', { class: 'hint' }, `Last run ${st.last_run}.`) : null,
    ], [['Close', closeSheet]]);
  } else if (v === 'new') createDialog();
  else if (v === 'theme') {
    const order = ['system', 'light', 'dark'];
    setTheme(order[(order.indexOf(document.documentElement.dataset.theme) + 1) % order.length], true);
    flashStatus(`theme: ${document.documentElement.dataset.theme}`);
  }
}

// Light, dark or the system's; a change fades (a View Transition, or a short colour fade) and is remembered.
function paintTheme(t) {
  document.documentElement.dataset.theme = t;
  for (const b of $$('#themesw button')) b.setAttribute('aria-pressed', String(b.dataset.themePick === t));
}
function setTheme(t, animate = false) {
  if (t === 'charcoal' || t === 'ink') t = t === 'ink' ? 'dark' : 'system'; // the old names
  if (t === 'paper') t = 'light';
  if (!['system', 'light', 'dark'].includes(t)) t = 'system';
  store.set('theme', t);
  if (animate && !reduced() && document.startViewTransition) document.startViewTransition(() => paintTheme(t));
  else if (animate && !reduced()) {
    document.documentElement.classList.add('tt-fading');
    paintTheme(t);
    setTimeout(() => document.documentElement.classList.remove('tt-fading'), 400);
  } else paintTheme(t);
  if (S.graph) openGraphView(); // the points take the new colours
}

// ------------------------------------------------------------------ the note
async function openNote(path, anchor, line) {
  if (S.dirty) await save();
  await closeCanvas();
  let n;
  try { n = await api(`note?path=${enc(path)}`); } catch (e) {
    if (e.status === 404) { renderMissing(path); return; }
    throw e;
  }
  S.current = n;
  S.missing = null;
  store.set('last', n.path);
  document.body.classList.remove('side-on');
  $('#banner').hidden = true;
  const parts = n.path.split('/');
  $('#crumbs').replaceChildren(...parts.slice(0, -1).map(p => h('span', {}, p)), h('span', {}, parts[parts.length - 1]));
  $('#title').textContent = n.title;
  document.title = `${n.title} · Perennos Tempo Vault`;
  $('#meta').replaceChildren(...[
    ...(n.kind ? [h('b', {}, n.kind === 'base' ? 'Base' : 'Canvas'), ' · '] : [h('b', {}, n.words), ' words', ...(n.words > 200 ? [' · ', h('b', {}, Math.max(1, Math.round(n.words / 230))), ' min read'] : []), ' · ']),
    h('b', {}, n.backlinks.length), ' linked from · edited ', h('b', {}, ago(n.mtime)),
    ...(n.history ? [' · ', h('b', {}, n.history), ' earlier versions'] : [])]);
  $('#tools').hidden = false;
  for (const id of ['#mode', '#rename', '#trash']) $(id).hidden = S.ro;
  if (S.mode === 'edit' && !S.ro) { $('#editor').value = n.text; S.dirty = false; }
  await renderBody();
  renderInsp();
  if (S.view === 'files') renderSide();
  if (anchor) scrollToAnchor(anchor);
  else if (line) scrollToLine(line);
  else $('#body').scrollTop = 0;
}

function renderMissing(path) {
  S.current = null;
  S.missing = path;
  $('#crumbs').replaceChildren(h('span', {}, path));
  $('#title').textContent = stem(path);
  $('#meta').textContent = 'this note does not exist yet';
  $('#tools').hidden = true;
  $('#body').replaceChildren(h('div', { class: 'md' }, h('p', {}, 'Nothing is written here yet.'), S.ro ? null : h('button', { class: 'btn go', type: 'button', onclick: () => createNote(path, true) }, 'Create this note')));
  $('#insp-body').replaceChildren();
}

let cvMod = null;
async function closeCanvas() { const c = S.canvas; S.canvas = null; if (c) await c.close(); }
async function openCanvasView(n) {
  await closeCanvas();
  document.body.classList.add('canvas-on');
  const host = h('div', { class: 'cvx-host' });
  $('#body').replaceChildren(host);
  try {
    const m = await (cvMod ||= import('/canvas.mjs'));
    if (S.current !== n) return;
    S.canvas = await m.openCanvas(host, n.path, {
      api, h, go, noteHash, stem, ro: S.ro, flash: flashStatus, files: () => S.files,
      status: t => status(undefined, t),
      saved: (mtime, text) => { if (S.current && S.current.path === n.path) { S.current.mtime = mtime; S.current.text = text; } status(undefined, `saved ${clock(new Date())}`); },
      conflict: d => {
        const b = $('#banner');
        b.hidden = false;
        b.replaceChildren(h('span', { class: 'grow' }, 'This canvas changed on disk while you were working on it (Obsidian, another window or Tempo). Both versions are kept in history.'),
          h('button', { class: 'btn sm', type: 'button', onclick: async () => { b.hidden = true; const c = S.canvas; S.canvas = null; if (c) c.drop(); await openNote(n.path); } }, 'Use theirs'),
          h('button', { class: 'btn sm go', type: 'button', onclick: async () => { b.hidden = true; if (S.canvas) await S.canvas.keepMine(); } }, 'Keep mine'));
        status(undefined, 'conflict: choose a version');
      },
    });
    const c = S.canvas.count();
    $('#meta').replaceChildren(h('b', {}, 'Canvas'), ' · ', h('b', {}, c.cards), ' cards · ', h('b', {}, c.arrows), ' arrows · ', h('b', {}, n.backlinks.length), ' linked from · edited ', h('b', {}, ago(n.mtime)));
    status(undefined, S.ro ? 'read-only vault' : 'saves as you go');
  } catch (e) { host.replaceChildren(h('p', { class: 'empty' }, `The canvas did not open: ${e.message}`)); }
}

async function renderBody() {
  const n = S.current;
  const editing = S.mode === 'edit' && !S.ro;
  if (editing) await closeCanvas();
  $('#editor').hidden = !editing;
  $('#body').hidden = editing;
  $('#mode').setAttribute('aria-pressed', String(editing));
  $('#mode').firstChild.textContent = editing ? 'Read ' : 'Edit ';
  if (editing) {
    status(undefined, S.dirty ? 'unsaved' : `editing · saves as you type`);
    const ed = $('#editor');
    if (document.activeElement !== ed && $('#pal').hidden && $('#sheet').hidden) { ed.focus(); ed.setSelectionRange(ed.value.length, ed.value.length); }
    return;
  }
  if (n.kind === 'canvas') return openCanvasView(n);
  document.body.classList.remove('canvas-on');
  const r = await api(`render?path=${enc(n.path)}`);
  const art = h('article', { class: 'md' });
  art.innerHTML = r.html; // escaped by the server's renderer
  const first = art.firstElementChild;
  if (first && first.tagName === 'H1' && first.textContent.trim() === n.title.trim()) first.classList.add('dup-title'); // the header already shows it
  if (n.kind) art.classList.add('wide', n.kind); // a base (or canvas) gets the whole width
  $('#body').replaceChildren(art);
  enhanceBases(art);
  status(undefined, S.ro ? 'read-only vault' : 'nothing leaves this machine');
}

function scrollToAnchor(a) {
  const body = $('#body');
  let el = null;
  if (a.startsWith('^')) el = body.querySelector(`[data-block="${CSS.escape(a.slice(1))}"]`);
  if (!el) el = $$('[data-heading]', body).find(x => x.dataset.heading.toLowerCase() === a.toLowerCase()) || body.querySelector(`#${CSS.escape(a)}`);
  if (el) { el.scrollIntoView({ block: 'start', behavior: reduced() ? 'auto' : 'smooth' }); el.classList.add('flash'); }
}
function scrollToLine(line) {
  const els = $$('[data-line]', $('#body')).filter(e => Number(e.dataset.line) <= line);
  const el = els[els.length - 1];
  if (el) { el.scrollIntoView({ block: 'center' }); el.classList.add('flash'); }
}

async function setMode(m) {
  if (S.ro || !S.current) return;
  if (m === S.mode) return;
  if (S.mode === 'edit' && S.dirty) await save();
  if (m === 'edit') await closeCanvas();
  S.mode = m;
  if (m === 'edit') { $('#editor').value = S.current.text; S.dirty = false; }
  await renderBody();
  if (m === 'edit') $('#editor').focus();
}

async function save(force) {
  const n = S.current;
  if (!n || !S.dirty) return;
  if (S.saving) return S.saving;
  const text = $('#editor').value;
  S.saving = (async () => {
    try {
      const r = await api('write', { path: n.path, text, mtime: force ? null : n.mtime });
      n.mtime = r.mtime; n.text = text;
      S.dirty = $('#editor').value !== text;
      status(undefined, `saved ${clock(new Date())}`);
      $('#banner').hidden = true;
    } catch (e) {
      if (e.data && e.data.conflict) conflict(e.data);
      else status(undefined, `not saved: ${e.message}`);
    } finally { S.saving = null; }
  })();
  return S.saving;
}
const autosave = debounce(() => save(), 900);

function conflict(d) {
  const b = $('#banner');
  b.hidden = false;
  b.replaceChildren(
    h('span', { class: 'grow' }, 'This note changed on disk while you were writing (another app, Obsidian or Tempo). Nothing is lost: both versions are kept in history.'),
    h('button', { class: 'btn sm', type: 'button', onclick: () => sheet('The version on disk', [h('pre', {}, d.text)], [['Close', closeSheet]]) }, 'See theirs'),
    h('button', { class: 'btn sm', type: 'button', onclick: async () => { S.dirty = false; await openNote(S.current.path); } }, 'Use theirs'),
    h('button', { class: 'btn sm go', type: 'button', onclick: async () => { S.dirty = true; await save(true); } }, 'Keep mine'));
  status(undefined, 'conflict: choose a version');
}

async function toggleTask(path, line, done) {
  try { await api('task', { path, line: Number(line), done }); } catch (e) { flashStatus(e.message); }
  if (S.current && S.current.path === path) { const keep = $('#body').scrollTop; await openNote(path); $('#body').scrollTop = keep; }
  loadTree();
}

async function createNote(name, edit) {
  if (S.ro) return;
  const path = /\.md$/i.test(name) ? name : `${name}.md`;
  try {
    const r = await api('create', { path, text: `# ${stem(path)}\n\n` });
    await loadTree();
    if (edit) S.mode = 'edit';
    closeSheet();
    go(noteHash(r.path));
  } catch (e) {
    if (e.status === 409) go(noteHash(path)); else flashStatus(e.message);
  }
}

// ------------------------------------------------------------------ inspector
function renderInsp() {
  for (const t of $$('.tab')) t.setAttribute('aria-selected', String(t.dataset.tab === S.tab));
  if (S.mini) { S.mini.close(); S.mini = null; }
  const n = S.current;
  const body = $('#insp-body');
  if (!n) { body.replaceChildren(); return; }
  if (S.tab === 'links') {
    const out = n.links.filter(l => !l.embed || l.to);
    const past = onThisDay(n.path);
    body.replaceChildren(
      past.length ? h('div', { class: 'sec lbl' }, h('span', {}, 'On this day'), h('span', { class: 'n' }, past.length)) : '',
      ...past.map(p => h('button', { class: 'bl', type: 'button', onclick: () => go(noteHash(p.path)) }, h('b', {}, p.title), h('span', {}, `${p.years} year${p.years > 1 ? 's' : ''} ago`))),
      h('div', { class: 'sec lbl' }, h('span', {}, 'Linked from'), h('span', { class: 'n' }, n.backlinks.length)),
      ...(n.backlinks.length ? n.backlinks.map(b => h('button', { class: 'bl', type: 'button', onclick: () => go(noteHash(b.source, null, b.lines[0].line)) }, h('b', {}, b.title || stem(b.source)), ...b.lines.slice(0, 3).map(l => h('span', {}, plain(l.text))))) : [h('p', { class: 'hint' }, 'Nothing links here yet.')]),
      h('div', { class: 'sec lbl' }, h('span', {}, 'Links out'), h('span', { class: 'n' }, out.length)),
      ...out.map(l => l.to
        ? h('button', { class: 'bl', type: 'button', onclick: () => l.to.endsWith('.md') ? go(noteHash(l.to, l.heading)) : window.open(`/vault/file?path=${enc(l.to)}`, '_blank', 'noopener') }, h('b', {}, l.alias || stem(l.to)), h('span', {}, l.to))
        : h('button', { class: 'bl missing', type: 'button', title: 'Create it', onclick: () => createNote(l.target, true) }, h('b', {}, `[[${l.target}]]`), h('span', {}, 'does not exist yet'))),
      h('div', { class: 'sec lbl' }, h('span', {}, 'Unlinked mentions'), h('button', { class: 'btn sm', type: 'button', onclick: e => loadMentions(e.target) }, 'Find')));
  } else if (S.tab === 'outline') {
    body.replaceChildren(...(n.headings.length ? n.headings.map(x => h('button', { class: 'ol', type: 'button', 'data-l': x.level, onclick: () => { if (S.mode === 'edit') setMode('read').then(() => scrollToAnchor(x.text)); else scrollToAnchor(x.text); } }, x.text)) : [h('p', { class: 'hint' }, 'No headings.')]));
  } else if (S.tab === 'props') {
    const dl = h('dl', { class: 'kv' });
    for (const p of n.props) {
      const vals = Array.isArray(p.value) ? p.value : [p.value];
      dl.append(h('dt', {}, p.key), h('dd', {}, ...vals.map(v => p.key.toLowerCase() === 'tags' ? h('button', { class: 'chip tag', type: 'button', onclick: () => go(`#/s/${enc('tag:#' + String(v).replace(/^#/, ''))}`) }, `#${String(v).replace(/^#/, '')}`) : h('span', { class: 'chip' }, String(v)))));
    }
    body.replaceChildren(
      n.props.length ? dl : h('p', { class: 'hint' }, 'No properties. Add them at the top of the note between --- lines.'),
      h('div', { class: 'sec lbl' }, h('span', {}, 'Tags')), h('div', {}, ...(n.tags.length ? n.tags.map(t => h('button', { class: 'chip tag', type: 'button', onclick: () => go(`#/s/${enc('tag:#' + t)}`) }, `#${t}`)) : [h('span', { class: 'hint' }, 'none')])),
      n.aliases.length ? [h('div', { class: 'sec lbl' }, h('span', {}, 'Also called')), h('div', {}, ...n.aliases.map(a => h('span', { class: 'chip' }, a)))] : null,
      h('div', { class: 'sec lbl' }, h('span', {}, 'File')), h('p', { class: 'hint' }, `${n.path} · ${(n.size / 1024).toFixed(1)} KB · edited ${new Date(n.mtime * 1000).toLocaleString()}`));
  } else if (S.tab === 'tempo') {
    renderAsk(body);
  } else if (S.tab === 'local') {
    const stage = h('div', { class: 'mini' });
    body.replaceChildren(h('p', { class: 'hint' }, 'This note and everything one link away. Drag to turn; click a dot to open it.'), stage);
    loadMini(stage, n.path);
  }
}

// ------------------------------------------------------------------ Ask Tempo (the agent on this computer)
const ASK = { conv: store.get('askConv', null), since: 0, el: null, log: null, input: null, cur: null, timer: 0, online: null, label: '', seen: new Set() };
const ASK_CHIPS = [['Summarize', 'Summarize this note in five lines.'], ['Links', 'Which notes in my vault should this note link to? Name them as [[links]].'], ['Tasks', 'Turn the open points in this note into a task list.'], ['Related', 'What else in my vault is related to this note, and how?']];
function renderAsk(body) {
  if (!ASK.el) {
    ASK.log = h('div', { class: 'ask-log', 'aria-live': 'polite' });
    ASK.input = h('textarea', { class: 'ask-in', rows: '3', placeholder: 'Ask Tempo about this note… (Enter sends, Shift+Enter is a new line)' });
    ASK.input.addEventListener('keydown', e => { if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); askSend(ASK.input.value); } e.stopPropagation(); });
    const chips = h('div', { class: 'ask-chips' }, ...ASK_CHIPS.map(([label, q]) => h('button', { class: 'chip', type: 'button', onclick: () => askSend(q) }, label)));
    ASK.state = h('span', { class: 'ask-state' }, '');
    ASK.el = h('div', { class: 'ask' },
      h('div', { class: 'sec lbl' }, h('span', {}, 'Ask Tempo'), ASK.state, h('button', { class: 'btn sm', type: 'button', title: 'start a new conversation', onclick: askReset }, 'New')),
      ASK.log, chips, ASK.input,
      h('p', { class: 'hint' }, 'Tempo runs on this computer. It reads the vault through its own tools and asks before it changes anything.'));
    askHealth();
    if (ASK.conv) askPoll(true);
  }
  body.replaceChildren(ASK.el);
}
const ASK_NO_KEY = 'To ask Tempo here, open the vault from Perennos Tempo (the Start menu, or "tempo-launch open vault"): that brings Tempo\'s key to this tab.';
const ASK_OLD_KEY = 'Tempo did not accept the key this tab has (it may have restarted with a new one). Open the vault again from Perennos Tempo to bring the current key.';
// The agent names its models only to a caller with its key, so a list means the key works.
function askLabel(hl) {
  const models = hl && Array.isArray(hl.models) ? hl.models : null;
  if (models && models.length) return `on · ${models.length} model${models.length === 1 ? '' : 's'}`;
  return !models && agentKey() ? 'on · key not accepted' : 'on';
}
async function askHealth() {
  try {
    const hl = await api('agent/health');
    ASK.online = true;
    ASK.label = askLabel(hl);
    ASK.state.textContent = ASK.label;
    if (!ASK.log.childElementCount) {
      if (!agentKey()) ASK.log.append(h('p', { class: 'hint' }, ASK_NO_KEY));
      else if (!(hl && Array.isArray(hl.models))) ASK.log.append(h('p', { class: 'hint' }, ASK_OLD_KEY));
    }
  }
  catch (e) {
    ASK.online = false;
    ASK.state.textContent = 'off';
    if (!ASK.log.childElementCount) ASK.log.append(h('p', { class: 'hint' }, e.status === 403 ? e.message : 'Tempo is not running. Start Perennos Tempo (or run "tempo-launch agent"), then ask again.'));
  }
}
function askReset() { ASK.conv = null; store.set('askConv', null); ASK.since = 0; ASK.cur = null; ASK.seen.clear(); ASK.log.replaceChildren(); askHealth(); }
async function askSend(text) {
  text = String(text || '').trim();
  if (!text) return;
  if (S.ro) return flashStatus('read-only vault');
  try {
    if (!ASK.conv) { const r = await api('agent/new', {}); ASK.conv = r.conv; store.set('askConv', r.conv); ASK.since = 0; }
    const n = S.current;
    const ctx = n ? `\n\n(I am looking at the vault note "${n.title}", ${n.path}. Read it with vault_read when it helps.)` : '';
    await api('agent/send', { conv: ASK.conv, text: text + ctx });
    ASK.input.value = '';
    ASK.state.textContent = 'thinking…';
    askPoll();
  } catch (e) {
    if (e.status === 409) flashStatus('Tempo is still answering');
    else if (e.status === 404) { askReset(); flashStatus('that conversation is gone; ask again'); }
    else if (e.status === 401) ASK.log.append(h('p', { class: 'hint ask-err' }, ASK_NO_KEY));
    else { ASK.state.textContent = 'off'; ASK.log.append(h('p', { class: 'hint ask-err' }, e.message)); }
  }
}
async function askPoll(once) {
  clearTimeout(ASK.timer);
  if (!ASK.conv) return;
  let r;
  try { r = await api(`agent/events?conv=${enc(ASK.conv)}&since=${ASK.since}`); } catch (e) { ASK.state.textContent = 'off'; return; }
  for (const ev of r.events || []) { ASK.since++; askEvent(ev); }
  document.documentElement.dataset.busy = r.busy ? 'true' : 'false'; // the insignia's circuit and the graph's core follow
  S.graph?.busy?.(!!r.busy);
  if (r.busy) { ASK.state.textContent = 'thinking…'; ASK.timer = setTimeout(askPoll, once ? 900 : 420); }
  else { ASK.state.textContent = ASK.online === false ? 'off' : ASK.label || 'on'; ASK.cur = null; }
  ASK.log.scrollTop = ASK.log.scrollHeight;
}
function askEvent(ev) {
  const log = ASK.log;
  const t = ev.type;
  if (t === 'user') {
    const shown = String(ev.text || '').replace(/\n\n\(I am looking at the vault note [\s\S]*$/, '');
    log.append(h('div', { class: 'ask-msg me' }, shown));
    ASK.cur = null;
  } else if (t === 'delta') {
    if (!ASK.cur) { ASK.cur = h('div', { class: 'ask-msg tempo md' }); log.append(ASK.cur); }
    ASK.cur.append(document.createTextNode(ev.text || ''));
  } else if (t === 'assistant') {
    const el = ASK.cur || h('div', { class: 'ask-msg tempo md' });
    if (!ASK.cur) log.append(el);
    ASK.cur = null;
    el.textContent = ev.text || '';
    // the answer as a note would show it: [[links]] open notes here
    api('preview', { path: (S.current && S.current.path) || 'Tempo.md', text: ev.text || '' }).then(r => { el.innerHTML = r.html; }).catch(() => {}); // escaped by the server's renderer
  } else if (t === 'tool_call') {
    log.append(h('div', { class: 'ask-tool' }, '→ ', h('b', {}, ev.name || 'tool'), ' ', String(ev.args || '').slice(0, 160)));
  } else if (t === 'tool_result') {
    log.append(h('details', { class: 'ask-tool' }, h('summary', {}, '← ', h('b', {}, ev.name || 'result')), h('pre', {}, String(ev.text || '').slice(0, 4000))));
  } else if (t === 'confirm') {
    if (ASK.seen.has(ev.id)) return;
    ASK.seen.add(ev.id);
    const verdict = h('span', { class: 'ask-verdict' }, '');
    const answer = async ok => {
      for (const b of card.querySelectorAll('button')) b.disabled = true;
      try { const r = await api('agent/confirm', { conv: ASK.conv, id: ev.id, ok }); verdict.textContent = r.ok ? (ok ? 'approved' : 'denied') : 'too late (it timed out)'; }
      catch (e) { verdict.textContent = e.message; }
      askPoll();
    };
    const card = h('div', { class: 'ask-confirm' }, h('div', { class: 'sec lbl' }, h('span', {}, 'Tempo asks first')), h('b', {}, ev.what || 'approve?'), h('pre', {}, ev.detail || ''),
      h('div', { class: 'row' }, h('button', { class: 'btn sm', type: 'button', onclick: () => answer(false) }, 'Deny'), h('button', { class: 'btn sm go', type: 'button', onclick: () => answer(true) }, 'Approve'), verdict));
    log.append(card);
  } else if (t === 'status' && ev.text) {
    log.append(h('div', { class: 'ask-status' }, ev.text));
  } else if (t === 'error' && ev.text) {
    log.append(h('p', { class: 'hint ask-err' }, ev.text));
  }
}

async function loadMentions(btn) {
  btn.disabled = true;
  const r = await api(`mentions?path=${enc(S.current.path)}`);
  const list = r.mentions.slice(0, 50).map(m => h('button', { class: 'bl', type: 'button', onclick: () => go(noteHash(m.source, null, m.line)) }, h('b', {}, stem(m.source)), h('span', {}, plain(m.text))));
  btn.replaceWith(h('span', { class: 'n' }, r.mentions.length));
  $('#insp-body').append(...(list.length ? list : [h('p', { class: 'hint' }, 'No plain-text mentions of this note.')]));
}

let mg = null;
const memgraph = () => (mg ||= import('/memgraph.mjs'));

async function loadMini(stage, path) {
  try {
    const [g, m] = await Promise.all([api(`graph?center=${enc(path)}&depth=1&tags=0`), memgraph()]);
    if (!stage.isConnected) return;
    S.mini = await m.openGraph(stage, g, { reduced: true, onPick: pick, flatLabel: 'Local graph' });
  } catch (e) { stage.replaceChildren(h('p', { class: 'hint' }, e.message)); }
}

function pick(src) {
  if (src.startsWith('#')) return go(`#/s/${enc('tag:' + src)}`);
  if (src.startsWith('?')) return createNote(src.slice(1), true);
  if (src.toLowerCase().endsWith('.md')) return go(noteHash(src));
  window.open(`/vault/file?path=${enc(src)}`, '_blank', 'noopener');
}

// ------------------------------------------------------------------ graph view
async function openGraphView() {
  closeGraphView();
  const stage = $('#gstage');
  stage.replaceChildren(h('p', { class: 'empty' }, 'Laying out the field…'));
  const o = S.gopts;
  const q = `graph?tags=${+o.tags}&attachments=${+o.attachments}&unresolved=${+o.unresolved}&orphans=${+o.orphans}&filter=${enc($('#gfilter').value.trim())}`;
  try {
    const [g, m] = await Promise.all([api(q), memgraph()]);
    if (!S.both && S.view !== 'graph') return;
    stage.replaceChildren();
    const kinds = {};
    for (const n of g.nodes) kinds[n.kind] = (kinds[n.kind] || 0) + 1;
    const used = g.nodes.filter(n => n.used).length;
    $('#legend').replaceChildren(...Object.entries(kinds).map(([k, c]) => h('span', { 'data-k': k }, `${k} ${c}`)), used ? h('span', { 'data-k': 'used' }, `changed today ${used}`) : null, h('span', { 'data-k': 'edges' }, `${g.edges.length} links`));
    if (!g.nodes.length) { stage.append(h('p', { class: 'empty' }, 'Nothing to draw yet.')); return; }
    S.graph = await m.openGraph(stage, g, { reduced: reduced(), onPick: pick, flatLabel: 'Vault graph' });
  } catch (e) { stage.replaceChildren(h('p', { class: 'empty' }, e.message)); }
}
function closeGraphView() { if (S.graph) { S.graph.close(); S.graph = null; } }

// ------------------------------------------------------------------ bases: view tabs, boards, cells
// A base with several views gets tabs (the server lists every view, for pages without scripts).
function enhanceBases(root) {
  $$('.base', root).forEach((base, idx) => {
    const views = $$(':scope > .base-view', base);
    if (views.length > 1) {
      const key = `baseView:${base.dataset.file || (S.current && S.current.path) || ''}#${base.dataset.file ? 0 : idx}`;
      const tabs = h('div', { class: 'base-tabs', role: 'tablist' });
      const show = k => {
        store.set(key, k);
        views.forEach((v, i) => { v.hidden = i !== k; });
        $$('button', tabs).forEach((b, i) => b.setAttribute('aria-selected', String(i === k)));
      };
      views.forEach((v, i) => tabs.append(h('button', { class: 'base-tab', type: 'button', role: 'tab', 'data-kind': v.dataset.kind, onclick: () => show(i) }, v.dataset.name || `View ${i + 1}`)));
      $('.base-head', base).append(tabs);
      for (const v of views) { const n = $(':scope > .base-view-name', v); if (n) n.hidden = true; }
      show(Math.min(Number(store.get(key, 0)) || 0, views.length - 1));
    } else for (const v of views) { const n = $(':scope > .base-view-name', v); if (n && !base.dataset.file) n.hidden = true; }
  });
  if (!S.ro) for (const board of $$('.base-board[data-prop]', root)) wireBoard(board);
}

// Drag a card to another lane (or focus it and press Alt+← / Alt+→): the note's property follows.
function wireBoard(board) {
  const prop = board.dataset.prop;
  let dragging = null;
  const lanes = () => $$('.base-lane', board);
  const recount = () => { for (const l of lanes()) $('.base-lane-head .count', l).textContent = $$('.base-card', l).length; };
  const move = async (card, lane) => {
    const from = card.closest('.base-lane');
    if (!lane || lane === from) return;
    $('.base-lane-body', lane).append(card);
    recount();
    const value = lane.dataset.lane;
    try {
      await api('prop', { path: card.dataset.path, key: prop, value: value === '' ? [] : value });
      flashStatus(`${stem(card.dataset.path)} · ${prop}: ${value || 'no value'}`);
    } catch (err) { flashStatus(err.message); $('.base-lane-body', from).append(card); recount(); }
  };
  for (const c of $$('.base-card[draggable]', board)) c.tabIndex = 0;
  board.addEventListener('dragstart', e => {
    const card = e.target.closest('.base-card[draggable]');
    if (!card) return;
    dragging = card;
    card.classList.add('dragging');
    e.dataTransfer.effectAllowed = 'move';
    e.dataTransfer.setData('text/plain', `[[${stem(card.dataset.path)}]]`);
  });
  board.addEventListener('dragend', () => {
    if (dragging) dragging.classList.remove('dragging');
    dragging = null;
    for (const l of lanes()) l.classList.remove('over');
  });
  board.addEventListener('dragover', e => {
    const lane = e.target.closest('.base-lane');
    if (!lane || !dragging) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = 'move';
    for (const l of lanes()) l.classList.toggle('over', l === lane);
  });
  board.addEventListener('drop', e => {
    const lane = e.target.closest('.base-lane');
    if (!lane || !dragging) return;
    e.preventDefault();
    lane.classList.remove('over');
    move(dragging, lane);
  });
  board.addEventListener('keydown', e => {
    const card = e.target.closest('.base-card[draggable]');
    if (!card || !e.altKey || (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight')) return;
    e.preventDefault();
    const all = lanes(), at = all.indexOf(card.closest('.base-lane'));
    move(card, all[at + (e.key === 'ArrowRight' ? 1 : -1)]).then(() => card.focus());
  });
}

// Double-click a property cell to change it in the note itself; Enter saves, Esc leaves it.
function editCell(td) {
  if (S.ro || td.querySelector('input')) return;
  let raw = null;
  try { raw = JSON.parse(td.dataset.raw || 'null'); } catch { /* shown as text */ }
  const list = Array.isArray(raw);
  const before = list ? raw.join(', ') : raw === null ? '' : String(raw);
  const input = h('input', { type: 'text', class: 'cell-in', value: before, 'aria-label': td.dataset.prop, spellcheck: 'false' });
  const keep = [...td.childNodes];
  td.replaceChildren(input);
  input.focus();
  input.select();
  let done = false;
  const finish = ok => {
    if (done) return;
    done = true;
    const v = input.value.trim();
    if (!ok || v === before.trim()) { td.replaceChildren(...keep); return; }
    setCell(td, list ? v.split(',').map(x => x.trim()).filter(Boolean) : v, keep);
  };
  input.addEventListener('keydown', e => {
    if (e.key === 'Enter') { e.preventDefault(); finish(true); }
    else if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); finish(false); }
  });
  input.addEventListener('blur', () => finish(true));
}

async function setCell(td, value, keep) {
  const path = td.closest('tr[data-path]').dataset.path, key = td.dataset.prop;
  const shown = Array.isArray(value) ? value.join(', ') : typeof value === 'boolean' ? (value ? '✓' : '–') : value;
  td.replaceChildren(typeof value === 'boolean' ? h('span', { class: `base-bool${value ? ' on' : ''}` }, shown) : shown);
  td.dataset.raw = JSON.stringify(typeof value === 'boolean' ? String(value) : value);
  try {
    await api('prop', { path, key, value: typeof value === 'boolean' ? String(value) : value === '' ? [] : value });
    flashStatus(`${stem(path)} · ${key} saved`);
  } catch (err) { if (keep) td.replaceChildren(...keep); flashStatus(err.message); }
}

// ------------------------------------------------------------------ palette
const COMMANDS = [
  ['New note', () => action('new')], ['New canvas', () => createDialog('canvas')], ['New base (table, cards, board)', () => createDialog('base')], ['Map this note on a canvas', () => mapNote()], ['Present this note as slides', () => present()], ['Calendar of daily notes', () => go('#/v/calendar')], ['Open this note in Obsidian', () => openInObsidian()], ["Today's note", () => action('daily')], ['Graph', () => go('#/v/graph')], ['Search', () => go('#/v/search')],
  ['Open tasks', () => go('#/v/tasks')], ['Tags', () => go('#/v/tags')], ['Check the vault', () => go('#/v/health')], ['What Tempo learned', () => action('learned')],
  ['Read / edit', () => setMode(S.mode === 'edit' ? 'read' : 'edit')], ['Rename or move this note', () => renameDialog()], ['Earlier versions', () => historyDialog()],
  ['Move this note to trash', () => trashDialog()], ['Copy a link to this note', () => copyLink()], ['Theme', () => action('theme')], ['Brain: the graph, full', () => setLayout('brain')], ['Dashboard: notes and details', () => setLayout('dashboard')], ['Both: the note and the graph', () => setLayout('both')],
  ['Random note', () => randomNote()], ['Focus mode', () => document.body.classList.toggle('zen')], ['Keyboard shortcuts', () => helpSheet()],
  ['Show or hide attachments in Files', () => { store.set('showFiles', !store.get('showFiles', true)); renderSide(); }], ['Reload from disk', async () => { await api('refresh', {}); await loadTree(); if (S.current) openNote(S.current.path); }],
];
let palItems = [], palSel = 0, palQuery = null;
function openPal(prefix = '') {
  $('#pal').hidden = false;
  const i = $('#pal-in');
  i.value = prefix;
  palItems = []; palQuery = null;
  $('#pal-list').replaceChildren();
  i.focus();
  palRefresh();
}
function closePal() { $('#pal').hidden = true; }
async function palRefresh() {
  const q = $('#pal-in').value;
  const list = $('#pal-list');
  if (q.startsWith('>')) {
    const f = q.slice(1).trim().toLowerCase();
    palItems = COMMANDS.filter(([n]) => n.toLowerCase().includes(f)).map(([n, f2]) => ({ cmd: true, title: n, run: f2 }));
  } else {
    const r = await api(`suggest?q=${enc(q)}&limit=30`).catch(() => ({ items: [] }));
    palItems = r.items.map(x => ({ title: x.title, path: x.path, run: () => go(noteHash(x.path)) }));
    if (q.trim() && !S.ro && !palItems.some(x => x.title.toLowerCase() === q.trim().toLowerCase())) palItems.push({ cmd: true, title: `Create "${q.trim()}"`, run: () => createNote(q.trim(), true) });
  }
  if ($('#pal-in').value !== q) return; // a newer query is on its way
  palSel = 0;
  palQuery = q;
  list.replaceChildren(...palItems.map((it, k) => h('button', { class: `pi${it.cmd ? ' cmd' : ''}`, type: 'button', role: 'option', 'aria-selected': String(k === palSel), onclick: () => { closePal(); it.run(); } }, h('b', {}, it.title), it.path ? h('i', {}, it.path) : null)));
}
const palUpdate = debounce(palRefresh, 70);
async function palKey(e) {
  if (e.key === 'Escape') { closePal(); return; }
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    palSel = (palSel + (e.key === 'ArrowDown' ? 1 : -1) + palItems.length) % Math.max(palItems.length, 1);
    $$('.pi').forEach((b, k) => { b.setAttribute('aria-selected', String(k === palSel)); if (k === palSel) b.scrollIntoView({ block: 'nearest' }); });
  }
  if (e.key === 'Enter') {
    e.preventDefault();
    if (palQuery !== $('#pal-in').value) await palRefresh(); // Enter right after typing: use what was typed
    const q = $('#pal-in').value.trim();
    closePal();
    if (e.shiftKey && q && !q.startsWith('>')) return createNote(q, true);
    const it = palItems[palSel];
    if (it) it.run();
  }
}

// ------------------------------------------------------------------ sheets: new, rename, history, trash
function sheet(title, content, buttons) {
  $('#sheet').hidden = false;
  const box = $('#sheet-box');
  box.replaceChildren(h('h2', {}, title), ...[content].flat(), h('div', { class: 'row' }, ...buttons.map(([t, f, cls]) => h('button', { class: `btn ${cls || ''}`, type: 'button', onclick: f }, t))));
  const first = box.querySelector('input');
  (first || box.querySelector('button')).focus();
}
function closeSheet() { $('#sheet').hidden = true; }

// A canvas of the open note: its links to the right, what links to it to the left, then open it.
async function mapNote() {
  const n = S.current;
  if (!n || n.kind || S.ro) return flashStatus(S.ro ? 'read-only vault' : 'open a note first');
  const out = [...new Set(n.links.filter(l => l.to && /\.md$/i.test(l.to) && l.to !== n.path).map(l => l.to))].slice(0, 24);
  const back = [...new Set(n.backlinks.map(b => b.source).filter(s => /\.md$/i.test(s)))].filter(p => !out.includes(p)).slice(0, 24);
  const nodes = [{ id: 'note', type: 'file', file: n.path, x: -170, y: -130, width: 340, height: 260, color: '2' }];
  const edges = [];
  const arc = (list, mid, into) => {
    const r = 460 + list.length * 18, span = Math.min(Math.PI * 0.9, 0.32 * list.length);
    list.forEach((p, i) => {
      const a = mid + (list.length === 1 ? 0 : span * (i / (list.length - 1) - 0.5));
      const id = `n${nodes.length}`;
      nodes.push({ id, type: 'file', file: p, x: Math.round(Math.cos(a) * r - 150), y: Math.round(Math.sin(a) * r - 100), width: 300, height: 200 });
      edges.push(into ? { id: `e${edges.length}`, fromNode: id, toNode: 'note' } : { id: `e${edges.length}`, fromNode: 'note', toNode: id });
    });
  };
  arc(out, 0, false);
  arc(back, Math.PI, true);
  if (out.length + back.length === 0) nodes.push({ id: 'hint', type: 'text', text: 'Nothing links here yet. Drag a dot to start a map.', x: 220, y: -40, width: 280, height: 80 });
  const base = n.path.replace(/\.md$/i, ' map');
  for (let k = 1; k < 50; k++) {
    const path = `${base}${k > 1 ? ' ' + k : ''}.canvas`;
    try {
      const r = await api('create', { path, text: JSON.stringify({ nodes, edges }, null, '\t') });
      await loadTree();
      S.mode = 'read';
      return go(noteHash(r.path));
    } catch (e) { if (e.status !== 409) return flashStatus(e.message); }
  }
}

// A new base looks at the folder it is made in: every note there, as a table and a board.
function starterBase(path) {
  const folder = path.includes('/') ? path.replace(/\/[^/]+$/, '') : '';
  return [
    'filters:', '  and:', folder ? `    - file.inFolder(${JSON.stringify(folder)})` : '    - file.ext == "md"',
    'views:', '  - type: table', '    name: Table', '    order: [file.name, status, file.mtime]', '    sort:', '      - property: file.mtime', '        direction: DESC',
    '  - type: board', '    name: Board', '    groupBy: status', '    lanes: [Todo, Doing, Done]', '',
  ].join('\n');
}

async function createDialog(kind) {
  if (S.ro) return flashStatus('read-only vault');
  const t = await api('templates').catch(() => ({ templates: [] }));
  const input = h('input', { type: 'text', placeholder: 'Folder/Name', value: S.current && S.current.path.includes('/') ? S.current.path.replace(/[^/]+$/, '') : '' });
  const sel = h('select', { class: 'filter' }, h('option', { value: '' }, 'Blank note'), h('option', { value: '::canvas' }, 'Canvas: cards and arrows on a board'), h('option', { value: '::base' }, 'Base: a table and a board over notes'),
    ...t.templates.map(p => h('option', { value: stem(p) }, `Template: ${stem(p)}`)));
  if (kind === 'base') sel.value = '::base';
  if (kind === 'canvas') sel.value = '::canvas';
  const go2 = async () => {
    const name = input.value.trim();
    if (!name) return;
    try {
      let r;
      if (sel.value === '::canvas') {
        const path = /\.canvas$/i.test(name) ? name : `${name.replace(/\.md$/i, '')}.canvas`;
        r = await api('create', { path, text: '{\n\t"nodes":[],\n\t"edges":[]\n}\n' });
        S.mode = 'read';
      } else if (sel.value === '::base') {
        const path = /\.base$/i.test(name) ? name : `${name.replace(/\.md$/i, '')}.base`;
        r = await api('create', { path, text: starterBase(path) });
        S.mode = 'read';
      } else {
        const path = /\.md$/i.test(name) ? name : `${name}.md`;
        r = sel.value ? await api('create', { path, template: sel.value }) : await api('create', { path, text: `# ${stem(path)}\n\n` });
        S.mode = 'edit';
      }
      closeSheet(); await loadTree(); go(noteHash(r.path));
    } catch (e) { flashStatus(e.message); }
  };
  input.addEventListener('keydown', e => { if (e.key === 'Enter') go2(); });
  sheet(kind === 'base' ? 'A new base' : kind === 'canvas' ? 'A new canvas' : 'A new note', [input, h('p', {}, sel)], [['Cancel', closeSheet], ['Create', go2, 'go']]);
  input.setSelectionRange(input.value.length, input.value.length);
}

function renameDialog() {
  if (!S.current || S.ro) return;
  const input = h('input', { type: 'text', value: S.current.path });
  const run = async () => {
    const to = input.value.trim();
    if (!to || to === S.current.path) return closeSheet();
    try {
      if (S.dirty) await save();
      const r = await api('rename', { from: S.current.path, to });
      closeSheet(); await loadTree(); go(noteHash(r.path));
      flashStatus(r.changed.length ? `links rewritten in ${r.changed.length} note${r.changed.length > 1 ? 's' : ''}` : 'renamed');
    } catch (e) { flashStatus(e.message); }
  };
  input.addEventListener('keydown', e => { if (e.key === 'Enter') run(); });
  sheet('Rename or move', [input, h('p', { class: 'hint' }, 'Every link to this note, in every note, is rewritten to the new name. Each changed note keeps its old text in history.')], [['Cancel', closeSheet], ['Rename', run, 'go']]);
  const base = S.current.path.lastIndexOf('/') + 1;
  const dot = S.current.path.lastIndexOf('.');
  input.setSelectionRange(base, dot > base ? dot : S.current.path.length);
}

async function historyDialog() {
  if (!S.current) return;
  const path = S.current.path;
  const r = await api(`history?path=${enc(path)}`);
  const preview = h('pre', {}, r.versions.length ? 'Choose a version to see it.' : 'No earlier versions yet: one is kept before every change.');
  let chosen = null;
  const list = r.versions.slice(0, 60).map(v => h('div', { class: 'ver' }, h('span', {}, v.time || v.id), h('button', { class: 'btn sm', type: 'button', onclick: async () => { const t = await api(`history/text?path=${enc(path)}&id=${enc(v.id)}`); preview.textContent = t.text; chosen = v.id; } }, 'Show')));
  const restore = async () => {
    if (!chosen) return flashStatus('choose a version first');
    try { await api('restore', { path, id: chosen }); closeSheet(); await openNote(path); flashStatus('restored; the text before is kept too'); } catch (e) { flashStatus(e.message); }
  };
  sheet(`Earlier versions of ${stem(path)}`, [h('div', {}, ...list), preview], S.ro ? [['Close', closeSheet]] : [['Close', closeSheet], ['Restore this version', restore, 'go']]);
}

function trashDialog() {
  if (!S.current || S.ro) return;
  const path = S.current.path;
  sheet('Move to trash?', [h('p', { class: 'hint' }, `${path} moves to the vault's .trash folder. Nothing is deleted; links to it will show as wanted.`)], [['Cancel', closeSheet], ['Move to trash', async () => {
    try { await api('trash', { path }); closeSheet(); S.dirty = false; S.current = null; await loadTree(); const first = [...S.notes.keys()][0]; if (first) go(noteHash(first)); else renderMissing('Welcome.md'); } catch (e) { flashStatus(e.message); }
  }, 'danger']]);
}

async function copyLink() {
  if (!S.current) return;
  const p = S.current.path;
  const link = isDoc(p) ? `![[${p.split('/').pop()}]]` : `[[${stem(p)}]]`;
  try { await navigator.clipboard.writeText(link); flashStatus(`copied ${link}`); } catch { flashStatus(link); }
}

let flashT = 0;
function flashStatus(msg) {
  const el = $('#st-save');
  const keep = el.textContent;
  el.textContent = msg;
  clearTimeout(flashT);
  flashT = setTimeout(() => { if (el.textContent === msg) el.textContent = S.dirty ? 'unsaved' : keep; }, 4000);
}

// ------------------------------------------------------------------ [[ autocomplete in the editor
const sug = { open: false, items: [], sel: 0, start: 0 };
function caretXY(ta) {
  const mirror = h('div');
  const cs = getComputedStyle(ta);
  for (const p of ['font', 'padding', 'border', 'boxSizing', 'whiteSpace', 'wordWrap', 'letterSpacing', 'lineHeight', 'tabSize']) mirror.style[p] = cs[p];
  mirror.style.position = 'absolute'; mirror.style.visibility = 'hidden'; mirror.style.whiteSpace = 'pre-wrap'; mirror.style.width = ta.clientWidth + 'px';
  mirror.textContent = ta.value.slice(0, ta.selectionStart);
  const mark = h('span', {}, '​');
  mirror.append(mark);
  document.body.append(mirror);
  const r = ta.getBoundingClientRect();
  const x = r.left + mark.offsetLeft - ta.scrollLeft, y = r.top + mark.offsetTop - ta.scrollTop + parseFloat(cs.lineHeight || '20');
  mirror.remove();
  return [x, y];
}
const suggestNow = debounce(async () => {
  const ta = $('#editor');
  const before = ta.value.slice(0, ta.selectionStart);
  const m = before.match(/\[\[([^\]\n|#^]*)$/);
  if (!m) return closeSuggest();
  sug.start = ta.selectionStart - m[1].length;
  const r = await api(`suggest?q=${enc(m[1])}&limit=12`).catch(() => ({ items: [] }));
  sug.items = r.items;
  sug.sel = 0;
  if (!sug.items.length) return closeSuggest();
  const box = $('#suggest');
  box.replaceChildren(...sug.items.map((x, k) => h('button', { class: 'pi', type: 'button', role: 'option', 'aria-selected': String(k === 0), onmousedown: e => { e.preventDefault(); sug.sel = k; acceptSuggest(); } }, h('b', {}, x.title), h('i', {}, x.path))));
  const [x, y] = caretXY(ta);
  box.style.left = Math.min(x, innerWidth - 280) + 'px';
  box.style.top = Math.min(y + 4, innerHeight - 200) + 'px';
  box.hidden = false;
  sug.open = true;
}, 90);
function closeSuggest() { $('#suggest').hidden = true; sug.open = false; }
function acceptSuggest() {
  const ta = $('#editor');
  const it = sug.items[sug.sel];
  if (!it) return closeSuggest();
  const name = stem(it.path);
  const unique = [...S.notes.keys()].filter(p => stem(p).toLowerCase() === name.toLowerCase()).length <= 1;
  const target = unique ? name : it.path.replace(/\.md$/i, '');
  const after = ta.value.slice(ta.selectionStart);
  const close = after.startsWith(']]') ? '' : ']]';
  ta.setRangeText(target + close, sug.start, ta.selectionStart, 'end');
  closeSuggest();
  ta.dispatchEvent(new Event('input'));
}

// ------------------------------------------------------------------ live updates
function connect() {
  let es;
  try { es = new EventSource(`/vault/events?since=${S.gen}`); } catch { live(false, 'offline'); return; }
  es.onopen = () => live(true, 'live');
  es.onerror = () => live(false, 'reconnecting');
  const refresh = debounce(async paths => {
    for (const p of paths) PV.cache.delete(p);
    await loadTree();
    if (S.view !== 'files' && S.view !== 'graph') renderSide();
    if (S.view === 'graph') openGraphView();
    const cur = S.current;
    if (!cur && S.missing && paths.includes(S.missing) && (S.notes.has(S.missing) || S.files.some(f => f.path === S.missing))) return openNote(S.missing);
    if (!cur || !paths.includes(cur.path)) return;
    if (S.saving) await S.saving;
    // our own save comes back as an event too: same mtime and text means nothing new
    const disk = await api(`note?path=${enc(cur.path)}`).catch(() => null);
    if (!disk || S.current !== cur) return;
    if (disk.mtime === cur.mtime && disk.text === cur.text) return;
    if (S.mode === 'edit' && S.dirty) {
      const b = $('#banner');
      b.hidden = false;
      b.replaceChildren(h('span', { class: 'grow' }, 'This note just changed on disk. Keep writing to overwrite it on save, or load the new version.'), h('button', { class: 'btn sm', type: 'button', onclick: async () => { S.dirty = false; await openNote(cur.path); } }, 'Load it'));
    } else {
      const keep = $('#body').scrollTop;
      await openNote(cur.path);
      $('#body').scrollTop = keep;
    }
  }, 300);
  es.addEventListener('change', e => {
    const d = JSON.parse(e.data);
    S.gen = d.gen;
    S.graph?.ripple?.(); // a note changed: a ripple runs through the points
    refresh(d.paths);
  });
}

// ------------------------------------------------------------------ wiring
function wire() {
  $('.rail').addEventListener('click', e => {
    const b = e.target.closest('button[data-view]');
    if (!b) return;
    const v = b.dataset.view;
    if (['daily', 'learned', 'new', 'theme'].includes(v)) return action(v);
    if (v === S.view && v !== 'graph') return document.body.classList.toggle('side-on');
    go(`#/v/${v}`);
  });
  $('#layout').addEventListener('click', e => { const b = e.target.closest('button[data-layout]'); if (b) setLayout(b.dataset.layout); });
  $('#themesw').addEventListener('click', e => { const b = e.target.closest('button[data-theme-pick]'); if (b) setTheme(b.dataset.themePick, true); });
  addEventListener('resize', debounce(moveBar, 100));
  $('#filter').addEventListener('input', debounce(() => renderSide(), 120));
  $('#palbtn').addEventListener('click', () => openPal());
  $('#pal-in').addEventListener('input', palUpdate);
  $('#pal-in').addEventListener('keydown', palKey);
  $('#pal').addEventListener('mousedown', e => { if (e.target.id === 'pal') closePal(); });
  $('#sheet').addEventListener('mousedown', e => { if (e.target.id === 'sheet') closeSheet(); });
  $('#mode').addEventListener('click', () => setMode(S.mode === 'edit' ? 'read' : 'edit'));
  $('#rename').addEventListener('click', renameDialog);
  $('#hist').addEventListener('click', historyDialog);
  $('#trash').addEventListener('click', trashDialog);
  $('#inspbtn').addEventListener('click', () => document.body.classList.toggle('insp-on'));
  for (const t of $$('.tab')) t.addEventListener('click', () => { S.tab = t.dataset.tab; store.set('tab', S.tab); renderInsp(); });
  for (const b of $$('[data-g]')) b.addEventListener('click', () => { const k = b.dataset.g; S.gopts[k] = !S.gopts[k]; b.setAttribute('aria-pressed', String(S.gopts[k])); openGraphView(); });
  $('#gfilter').addEventListener('input', debounce(openGraphView, 400));
  const ed = $('#editor');
  ed.addEventListener('input', () => { S.dirty = true; status(undefined, 'unsaved'); autosave(); suggestNow(); });
  ed.addEventListener('keydown', e => {
    if (sug.open) {
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') { e.preventDefault(); sug.sel = (sug.sel + (e.key === 'ArrowDown' ? 1 : -1) + sug.items.length) % sug.items.length; $$('#suggest .pi').forEach((b, k) => b.setAttribute('aria-selected', String(k === sug.sel))); return; }
      if (e.key === 'Enter' || e.key === 'Tab') { e.preventDefault(); acceptSuggest(); return; }
      if (e.key === 'Escape') { e.preventDefault(); closeSuggest(); return; }
    }
    if (e.key === 'Enter' && !e.shiftKey && !e.ctrlKey && !e.metaKey && !e.altKey && smartEnter(ed)) { e.preventDefault(); ed.dispatchEvent(new Event('input')); return; }
    if (e.key === 'Tab' && !e.ctrlKey && !e.metaKey) { e.preventDefault(); indentLines(ed, e.shiftKey); ed.dispatchEvent(new Event('input')); }
    if (e.key === 'Escape') { e.preventDefault(); setMode('read'); }
  });
  ed.addEventListener('paste', e => {
    const files = [...(e.clipboardData?.files || [])];
    if (files.length) { e.preventDefault(); uploadFiles(files); }
  });
  ed.addEventListener('dragover', e => { if ([...(e.dataTransfer?.types || [])].includes('Files')) { e.preventDefault(); ed.classList.add('drop'); } });
  ed.addEventListener('dragleave', () => ed.classList.remove('drop'));
  ed.addEventListener('drop', e => {
    ed.classList.remove('drop');
    const files = [...(e.dataTransfer?.files || [])];
    if (files.length) { e.preventDefault(); uploadFiles(files); }
  });
  ed.addEventListener('blur', () => { setTimeout(closeSuggest, 150); if (S.dirty) save(); });
  $('#body').addEventListener('dblclick', e => {
    const td = e.target.closest('.base-table td[data-prop]');
    if (td && !e.target.closest('a')) { e.preventDefault(); editCell(td); }
  });
  $('#body').addEventListener('click', async e => {
    const flag = e.target.closest('.base-table td[data-prop] .base-bool');
    if (flag && !S.ro) { const td = flag.closest('td'); return setCell(td, !flag.classList.contains('on')); }
    const box = e.target.closest('input.task-box');
    if (box) { e.preventDefault(); if (S.ro) return; return toggleTask(box.dataset.path, box.dataset.line, box.checked); }
    const a = e.target.closest('a, span.embed.unresolved');
    if (!a) return;
    if (a.matches('[data-new]')) { e.preventDefault(); if (S.ro) return flashStatus('read-only vault'); return sheet(`Create "${a.dataset.new}"?`, [h('p', { class: 'hint' }, 'This link points at a note that does not exist yet.')], [['Cancel', closeSheet], ['Create', () => createNote(a.dataset.new, true), 'go']]); }
    if (a.matches('a.tag[data-tag]')) { e.preventDefault(); return go(`#/s/${enc('tag:#' + a.dataset.tag)}`); }
    if (a.matches('a.internal[data-path]')) { e.preventDefault(); return go(noteHash(a.dataset.path, a.dataset.anchor)); }
    const href = a.getAttribute('href') || '';
    if (href.startsWith('#fn')) { e.preventDefault(); const t = document.getElementById(href.slice(1)); if (t) t.scrollIntoView({ block: 'center' }); }
  });
  document.addEventListener('mouseover', e => {
    const a = e.target.closest && e.target.closest('a.internal[data-path]');
    if (a && !a.closest('.pv')) previewSoon(a);
  });
  document.addEventListener('mouseout', e => {
    const a = e.target.closest && e.target.closest('a.internal[data-path]');
    if (a && !a.closest('.pv')) { clearTimeout(PV.t); setTimeout(() => { if (PV.el && !PV.el.matches(':hover')) hidePreview(); }, 250); }
  });
  addEventListener('scroll', hidePreview, true);
  addEventListener('hashchange', () => { hidePreview(); route(); });
  addEventListener('beforeunload', e => { if (S.dirty) { save(); e.preventDefault(); } });
  document.addEventListener('keydown', e => {
    const mod = e.ctrlKey || e.metaKey;
    const typing = e.target.matches('input, textarea, select');
    if (mod && (e.key === 'k' || e.key === 'p' || e.key === 'o')) { e.preventDefault(); return openPal(); }
    if (mod && e.key === 'e') { e.preventDefault(); return setMode(S.mode === 'edit' ? 'read' : 'edit'); }
    if (mod && e.key === 's') { e.preventDefault(); return save(); }
    if (mod && e.key === 'g') { e.preventDefault(); return go('#/v/graph'); }
    if (mod && e.shiftKey && (e.key === 'F' || e.key === 'f')) { e.preventDefault(); go('#/v/search'); return $('#filter').focus(); }
    if (e.key === 'F2') { e.preventDefault(); return renameDialog(); }
    if (e.key === 'Escape') {
      if (PV.el) return hidePreview();
      if (!$('#pal').hidden) return closePal();
      if (!$('#sheet').hidden) return closeSheet();
      if (S.mode === 'edit' && e.target.id !== 'editor') return setMode('read');
    }
    if (typing || mod || e.altKey || !$('#pal').hidden || !$('#sheet').hidden) return;
    if (e.shiftKey && e.key === 'B') { e.preventDefault(); return setLayout('both'); }
    if (e.shiftKey && e.key === 'G') { e.preventDefault(); return setLayout('brain'); }
    if (e.shiftKey && e.key === 'D') { e.preventDefault(); return setLayout('dashboard'); }
    const map = { f: 'files', s: 'search', g: 'graph', t: 'tasks', h: 'tags', c: 'health', m: 'calendar' };
    const k = e.key.toLowerCase();
    if (map[k]) { e.preventDefault(); go(`#/v/${map[k]}`); if (k === 's') $('#filter').focus(); }
    else if (k === 'd') { e.preventDefault(); action('daily'); }
    else if (k === 'l') { e.preventDefault(); action('learned'); }
    else if (k === 'n') { e.preventDefault(); action('new'); }
    else if (k === 'e') { e.preventDefault(); setMode(S.mode === 'edit' ? 'read' : 'edit'); }
    else if (k === ',') { e.preventDefault(); action('theme'); }
    else if (k === 'i') { e.preventDefault(); document.body.classList.toggle('insp-on'); }
    else if (k === 'r') { e.preventDefault(); randomNote(); }
    else if (k === 'p') { e.preventDefault(); present(); }
    else if (k === 'z') { e.preventDefault(); document.body.classList.toggle('zen'); flashStatus(document.body.classList.contains('zen') ? 'focus mode · Z to leave' : 'focus mode off'); }
    else if (e.key === '?') { e.preventDefault(); helpSheet(); }
    else if (k === 'b') { tap(); }
    else if (k === '/') { e.preventDefault(); go('#/v/search'); $('#filter').focus(); }
  });
}


// ------------------------------------------------------------------ small things
// Daily notes are the ones named YYYY-MM-DD; the streak counts back from today (or yesterday).
const dayOf = p => (stem(p).match(/^(\d{4})-(\d{2})-(\d{2})$/) || [])[0];
function streak() {
  const days = new Set([...S.notes.keys()].map(dayOf).filter(Boolean));
  const d = new Date();
  const key = x => `${x.getFullYear()}-${String(x.getMonth() + 1).padStart(2, '0')}-${String(x.getDate()).padStart(2, '0')}`;
  if (!days.has(key(d))) d.setDate(d.getDate() - 1);
  let n = 0;
  while (days.has(key(d))) { n++; d.setDate(d.getDate() - 1); }
  return n;
}
// Daily notes from the same day in earlier years.
function onThisDay(path) {
  const day = dayOf(path);
  if (!day) return [];
  const [y, md] = [Number(day.slice(0, 4)), day.slice(4)];
  return [...S.notes.values()].filter(n => { const d = dayOf(n.path); return d && d.slice(4) === md && Number(d.slice(0, 4)) < y; })
    .map(n => ({ path: n.path, title: n.title, years: y - Number(dayOf(n.path).slice(0, 4)) })).sort((a, b) => a.years - b.years);
}
function randomNote() {
  const all = [...S.notes.keys()].filter(p => !S.current || p !== S.current.path);
  if (!all.length) return flashStatus('nothing to wander to yet');
  const p = all[Math.floor(Math.random() * all.length)];
  go(noteHash(p));
  flashStatus('serendipity: a random note (R for another)');
}
// Enter in a list continues it ("- ", "- [ ] ", "1. " counts up); Enter on an empty item ends it.
function smartEnter(ed) {
  if (ed.selectionStart !== ed.selectionEnd) return false;
  const at = ed.selectionStart, v = ed.value, start = v.lastIndexOf('\n', at - 1) + 1;
  const line = v.slice(start, at);
  const m = line.match(/^(\s*)([-*+]|\d+[.)])(\s+)(\[[ xX/-]\]\s+)?/);
  if (!m) return false;
  if (line.trim() === m[0].trim()) { ed.setRangeText('', start, at, 'end'); return true; } // empty item: end the list
  let marker = m[2];
  const num = marker.match(/^(\d+)([.)])$/);
  if (num) marker = `${Number(num[1]) + 1}${num[2]}`;
  ed.setRangeText(`\n${m[1]}${marker}${m[3]}${m[4] ? '[ ] ' : ''}`, at, at, 'end');
  return true;
}
// Tab and Shift+Tab indent the selected lines (a tab, as Obsidian writes lists).
function indentLines(ed, out) {
  const v = ed.value, a = v.lastIndexOf('\n', ed.selectionStart - 1) + 1;
  const b = ed.selectionEnd > ed.selectionStart && v[ed.selectionEnd - 1] === '\n' ? ed.selectionEnd - 1 : (v.indexOf('\n', ed.selectionEnd) + 1 || v.length + 1) - 1;
  const lines = v.slice(a, b).split('\n');
  const done = lines.map(l => out ? l.replace(/^(\t| {1,4})/, '') : '\t' + l).join('\n');
  if (!out && lines.length === 1 && ed.selectionStart === ed.selectionEnd && !/^\s*([-*+]|\d+[.)])\s/.test(lines[0])) {
    ed.setRangeText('\t', ed.selectionStart, ed.selectionEnd, 'end');
    return;
  }
  ed.setRangeText(done, a, b, 'select');
}
// Pasted or dropped files become attachments, embedded where the cursor is.
async function uploadFiles(files) {
  if (S.ro || !S.current) return flashStatus('read-only vault');
  const ed = $('#editor');
  for (const f of files) {
    let name = f.name || 'Pasted file';
    if (/^image\.(png|jpe?g|gif|webp)$/i.test(name)) {
      const t = new Date(), p2 = x => String(x).padStart(2, '0');
      name = `Pasted image ${t.getFullYear()}${p2(t.getMonth() + 1)}${p2(t.getDate())}${p2(t.getHours())}${p2(t.getMinutes())}${p2(t.getSeconds())}.${name.split('.').pop()}`;
    }
    flashStatus(`adding ${name}…`);
    try {
      const r = await fetch(`/vault/upload?name=${enc(name)}&note=${enc(S.current.path)}`, { method: 'POST', headers: { 'Content-Type': 'application/octet-stream' }, body: f });
      const j = await r.json();
      if (!r.ok) throw new Error(j.error || `HTTP ${r.status}`);
      const at = ed.selectionStart, before = ed.value.slice(0, at);
      ed.setRangeText(`${before && !before.endsWith('\n') ? '\n' : ''}${j.embed}\n`, at, ed.selectionEnd, 'end');
      ed.dispatchEvent(new Event('input'));
      flashStatus(`added ${j.path}`);
    } catch (e) { flashStatus(`could not add ${name}: ${e.message}`); }
  }
}
function helpSheet() {
  const rows = [['Ctrl K', 'jump to a note, or > for commands'], ['Ctrl E or E', 'read / edit'], ['Ctrl S', 'save now (it saves as you type)'], ['[[', 'link a note while writing'],
    ['F S G T H', 'files, search, graph, tasks, tags'], ['Shift+G / D / B', 'Brain, Dashboard, Both'], ['D', "today's note"], ['M', 'calendar'], ['P', 'present the note as slides'], ['L', 'what Tempo learned'], ['C', 'check the vault'], ['N', 'new note'], ['R', 'a random note'],
    ['Z', 'focus mode'], ['I', 'links, outline, properties'], [',', 'theme'], ['F2', 'rename (links follow)'], ['Esc', 'back to reading'], ['?', 'this sheet'],
    ['On a canvas', 'double-click: a card · drag a dot: an arrow · Delete · Ctrl+Z · Ctrl+D · F fits · Ctrl+scroll zooms · Alt+drag: no snapping']];
  sheet('Keys', [h('div', { class: 'keys' }, ...rows.flatMap(([k, d]) => [h('kbd', {}, k), h('span', {}, d)])), h('p', { class: 'hint' }, 'Paste or drop images and files into a note to attach them. Enter continues a list; Tab indents it.')], [['Close', closeSheet]]);
}
// Tap B to the beat and Tempo tells you the tempo.
const taps = [];
function tap() {
  const now = performance.now();
  if (taps.length && now - taps[taps.length - 1] > 2000) taps.length = 0;
  taps.push(now);
  if (taps.length >= 4) {
    const gaps = taps.slice(1).map((t, i) => t - taps[i]).slice(-8);
    const bpm = Math.round(60000 / (gaps.reduce((a, b) => a + b, 0) / gaps.length));
    flashStatus(`♩ = ${bpm} bpm${bpm >= 118 && bpm <= 122 ? ' · right in tempo' : ''}`);
  }
}
// ↑ ↑ ↓ ↓ ← → ← → B A
function konami() {
  const code = ['ArrowUp', 'ArrowUp', 'ArrowDown', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'ArrowLeft', 'ArrowRight', 'b', 'a'];
  let i = 0;
  document.addEventListener('keydown', e => {
    if (e.target.matches('input, textarea, select')) return;
    i = e.key.toLowerCase() === code[i].toLowerCase() ? i + 1 : (e.key === code[0] ? 1 : 0);
    if (i === code.length) {
      i = 0;
      document.body.classList.add('konami');
      flashStatus('Perennos Tempo · keeping time since the first note');
      setTimeout(() => document.body.classList.remove('konami'), 6000);
    }
  });
}
function signature() {
  const h_ = new Date().getHours();
  const hello = h_ < 5 ? 'Up late' : h_ < 12 ? 'Good morning' : h_ < 18 ? 'Good afternoon' : 'Good evening';
  setTimeout(() => flashStatus(`${hello}. Press ? for the keys.`), 900);
  try { console.log('%ctempo%c vault · your notes never leave this machine · made with care in the Cayman Islands', 'font: italic 18px Georgia, serif; color: #ff5a2a', 'font: 11px monospace; color: #77787c'); } catch { /* no console */ }
}

async function boot() {
  S.both = store.get('layout', 'dashboard') === 'both';
  setTheme(store.get('theme', 'system'));
  matchMedia('(prefers-color-scheme: light)').addEventListener?.('change', () => { if (S.graph) openGraphView(); });
  wire();
  signature();
  konami();
  try {
    S.health = await api('health');
  } catch (e) {
    live(false, 'offline');
    $('#body').replaceChildren(h('p', { class: 'empty' }, `The vault server did not answer: ${e.message}`));
    return;
  }
  S.ro = !!S.health.read_only;
  S.gen = S.health.gen;
  $('#vname').textContent = S.health.name;
  if (S.ro) { for (const id of ['#mode', '#rename', '#trash']) $(id).hidden = true; status(undefined, 'read-only vault'); }
  await loadTree();
  connect();
  if (parseHash()) return route();
  if (store.get('layout', 'dashboard') === 'brain') return go('#/v/graph');
  const last = store.get('last', null);
  const first = (last && S.notes.has(last) && last) || (S.notes.has('Welcome.md') && 'Welcome.md') || [...S.notes.values()].sort((a, b) => b.mtime - a.mtime).map(n => n.path)[0];
  if (first) go(noteHash(first));
  else renderMissing('Welcome.md');
  applyLayout(S.view);
  api('unresolved').then(u => { $('#n-health').textContent = u.unresolved.length || ''; }).catch(() => {});
  api('tags').then(t => { $('#n-tags').textContent = t.tags.length || ''; }).catch(() => {});
}

boot();
