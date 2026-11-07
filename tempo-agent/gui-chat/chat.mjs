// CHAT surface: the local agent loop (Ollama engine, MCP tools, memory RAG, delegate, workflow file).
// Talks only to the same origin at /agent/*. Every visible string comes from copy.json.
// The second brain (brain-world.mjs, three.js) is imported only when the Brain or Both view opens. It is kept
// current by brain-feed.mjs (polls /agent/memory/graph and /agent/memory); memgraph.mjs is its flat fallback.
// One look (tempo-theme.css): Brain, Dashboard or Both, and Light, Dark or System, are remembered
// in localStorage (wrapped in try/catch). Keys: B brain, D dashboard, Shift+B both, C W M panels, P pause motion, comma settings.
// Events arrive over /agent/stream (server-sent events, read with fetch so the key can go along)
// when /agent/health says the server has it (the Rust server does), else by polling.
// The agent's key comes from #key=... when Perennos Tempo opens this page; it is kept for this tab
// only (sessionStorage), cleared from the address bar, and sent as X-Tempo-Key on every call.
const h = (tag, a = {}, ...kids) => {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(a)) {
    if (v == null || v === false) continue;
    if (k === 'class') e.className = v;
    else if (k === 'text') e.textContent = v;
    else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v === true ? '' : v);
  }
  e.append(...kids.flat().filter(k => k != null && k !== false));
  return e;
};
const KEY_SLOT = 'tempo-agent-key';
let memKey = null;
(function takeKey() {
  const m = location.hash.match(/(?:^#|&)key=([0-9a-fA-F]{16,128})(?=&|$)/);
  if (!m) return;
  memKey = m[1];
  try { sessionStorage.setItem(KEY_SLOT, m[1]); } catch { /* private window: kept in memory */ }
  const rest = location.hash.slice(1).split('&').filter(x => !x.startsWith('key=')).join('&');
  history.replaceState(null, '', location.pathname + location.search + (rest ? '#' + rest : ''));
})();
const key = () => { try { return sessionStorage.getItem(KEY_SLOT) || memKey; } catch { return memKey; } };
const keyHeaders = () => { const k = key(); return k ? { 'X-Tempo-Key': k } : {}; };
// Every call but health needs the key: a 401 says how to get it (open the page from Perennos Tempo).
const api = (p, b) => fetch('/agent/' + p, b
  ? { method: 'POST', headers: { 'Content-Type': 'application/json', ...keyHeaders() }, body: JSON.stringify(b) }
  : { headers: keyHeaders() }).then(async r => {
  const j = await r.json().catch(() => ({}));
  if (r.status === 401) { const e = new Error(j.hint || 'needs the key'); e.status = 401; throw e; }
  return j;
});
const KINDS = ['current', 'core', 'memory', 'pack'];
const reducedMotion = () => window.matchMedia?.('(prefers-reduced-motion: reduce)').matches;
const store = {
  get(k, d) { try { const v = localStorage.getItem('tempo-chat:' + k); return v === null ? d : v; } catch { return d; } },
  set(k, v) { try { localStorage.setItem('tempo-chat:' + k, v); } catch { /* private window: fine */ } },
};
const VIEWS = ['brain', 'dashboard', 'both'], THEMES = ['system', 'light', 'dark'];
// Strings the newer views need; the host's copy.json overrides them.
const DEFAULT_COPY = {
  wordmark: 'perennos tempo', brain: 'Brain', dashboard: 'Dashboard', both: 'Both', view: 'View', panels: 'Panels', settings: 'Settings',
  theme: 'Theme', system: 'System', light: 'Light', dark: 'Dark', pause: 'Pause motion', motionNote: 'Your system asks for less motion, so the sphere holds still.',
  brainOpen: 'Open', brainClose: 'Close', brainLinks: 'Linked to', brainChanged: 'Last changed', brainSearch: 'Search the score', brainNoMatch: 'Nothing by that name', brainLoadError: "The 3D view didn't load. Here's the flat one instead.", brainNodes: 'sources',
  working: 'Working', idle: 'Idle', machinesUp: 'Machines up', memoryFiles: 'Memory files', chatCount: 'Chats', brainTitle: 'The second brain, as a field.', about: 'About', madeIn: 'Built and maintained in the Cayman Islands',
};
// Count a number up to its value (a few hundred ms); straight to it when motion is off.
function countTo(el, to, calm) {
  const from = Number(el.dataset.shown || 0);
  el.dataset.shown = String(to);
  cancelAnimationFrame(el._raf || 0);
  if (calm || from === to) { el.textContent = String(to); return; }
  const t0 = performance.now(), dur = 700;
  const step = now => {
    const k = Math.min(1, (now - t0) / dur), e = 1 - Math.pow(1 - k, 3);
    el.textContent = String(Math.round(from + (to - from) * e));
    if (k < 1) el._raf = requestAnimationFrame(step);
  };
  el._raf = requestAnimationFrame(step);
}

export async function mount(el, ctx) {
  if (!ctx.copy?.title) return { unmount() {} };
  const c = { ...DEFAULT_COPY, ...ctx.copy };
  const links = ['./tempo-theme.css', './chat.css'].map(f => h('link', { rel: 'stylesheet', href: new URL(f, import.meta.url).href }));
  await Promise.all(links.map(l => new Promise(r => { l.onload = l.onerror = r; document.head.append(l); })));

  const saved = ctx.view ? (ctx.view.chat ??= {}) : {};
  let conv = saved.conv || null, since = 0, busy = false, dead = false, timer = 0;
  let heavyModel = '', lane = null, lastTool = null, memUsed = new Set(), memFiles = [], graph = null, feed = null;
  let sse = false, es = null;

  const lab = t => h('span', { class: 'tt-lbl', text: t });
  const log = h('div', { class: 'ch-log', role: 'log', 'aria-live': 'polite' });
  const empty = h('p', { class: 'ch-empty', text: c.empty });
  log.append(empty);
  const strip = h('div', { class: 'ch-strip' });
  const laneV = h('span', { class: 'ch-v ch-lane', 'data-lane': 'none', text: '-' });
  const wf = h('pre', { class: 'ch-wf', text: c.noTasks });
  const list = h('div', { class: 'ch-convs' });
  const input = h('textarea', { class: 'ch-in', rows: '2', placeholder: c.placeholder, 'aria-label': c.title });
  const sendBtn = h('button', { class: 'ch-btn ch-go', type: 'button', text: c.send });
  const newBtn = h('button', { class: 'ch-btn', type: 'button', text: c.newChat });

  // ---- side panel: Chats / Workflow / Memory
  const memSearch = h('input', { class: 'ch-in ch-search', type: 'search', placeholder: c.search, 'aria-label': c.searchMemory });
  const memList = h('div', { class: 'ch-mem' });
  const memRead = h('div', { class: 'ch-read', hidden: true });
  const graphBtn = h('button', { class: 'ch-btn', type: 'button', text: c.brain });
  const panes = {
    chats: h('div', { class: 'ch-pane' }, newBtn, list),
    tasks: h('div', { class: 'ch-pane' }, wf),
    memory: h('div', { class: 'ch-pane' }, h('div', { class: 'ch-row' }, memSearch, graphBtn), memList, memRead),
  };
  function row(cls, who, text) {
    empty.remove();
    const d = h('div', { class: 'ch-m ' + cls }, who ? lab(who) : null, h('div', { class: 'ch-t', text }));
    log.append(d);
    log.scrollTop = log.scrollHeight;
    return d;
  }
  function setLane(l, model) {
    lane = l;
    laneV.dataset.lane = l;
    laneV.textContent = (l === 'heavy' ? c.heavy : c.fast) + (model ? ' · ' + model : '');
  }
  let live = null;
  function render(e) {
    if (e.type === 'delta') {
      graph?.setThinking(true);
      if (!live) { live = row('ch-bot ch-live', c.tempo, ''); }
      live.querySelector('.ch-t').textContent += e.text;
      log.scrollTop = log.scrollHeight;
      return;
    }
    if (live && e.type !== 'status') { live.remove(); live = null; }
    if (e.type === 'user') row('ch-user', c.you, e.text);
    else if (e.type === 'assistant') { graph?.ripple(); row('ch-bot', c.tempo + (e.model ? ' / ' + e.model : ''), e.text); }
    else if (e.type === 'assistant_partial') row('ch-bot', c.tempo, e.text);
    else if (e.type === 'tool_call') {
      empty.remove();
      lastTool = h('details', { class: 'ch-m ch-tool', 'data-state': 'run' },
        h('summary', { class: 'ch-t', text: e.name + ' ' + (e.args || '') }));
      log.append(lastTool);
      log.scrollTop = log.scrollHeight;
    } else if (e.type === 'tool_result') {
      const text = (e.text || '').slice(0, 700);
      if (lastTool) {
        lastTool.dataset.state = /^(ERROR|refused|denied|unknown tool)/.test(text) ? 'err' : 'done';
        lastTool.append(h('pre', { class: 'ch-res', text }));
        lastTool = null;
      } else row('ch-tool', null, '< ' + text);
    } else if (e.type === 'status') {
      const m = /^lane: (fast|heavy) \(([^)]*)\)/.exec(e.text || '');
      if (m) setLane(m[1], m[2]);
      else if (/^fast lane handed/.test(e.text || '')) setLane('heavy', heavyModel);
      if (/^memory: /.test(e.text || '')) refreshUsed();
      row('ch-status', null, e.text);
    } else if (e.type === 'error') row('ch-err', 'Error', e.text);
    else if (e.type === 'confirm') {
      const d = row('ch-confirm', c.needsYou, e.what);
      d.append(h('code', { text: e.detail }));
      const btns = [h('button', { class: 'ch-btn ch-go', type: 'button', text: c.approve, onclick: () => done(true) }),
        h('button', { class: 'ch-btn', type: 'button', text: c.deny, onclick: () => done(false) })];
      // The card stays in the log as a record of the decision.
      const done = ok => { api('confirm', { conv, id: e.id, ok }); btns.forEach(b => b.remove()); d.classList.add('ch-decided'); d.append(lab(ok ? c.approved : c.denied)); };
      d.append(...btns);
    }
  }
  const setBusy = b => {
    const was = busy;
    busy = b;
    sendBtn.disabled = busy; sendBtn.textContent = busy ? c.busy : c.send; sendBtn.classList.toggle('working', busy);
    root.dataset.busy = String(!!b); // the insignia's circuit runs faster and brighter while an engine works
    dot.className = 'tt-dot ' + (b ? 'run' : 'idle'); whereText.textContent = b ? c.working : c.idle;
    graph?.setThinking(b);
    feed?.setFast(b); // nodes used in the turn show up within a couple of seconds
    if (was && !b) { graph?.burst(); feed?.poke(); } // a run finished
  };
  const refreshWf = () => api('workflow?conv=' + conv).then(w => { wf.textContent = (w.text.split('## Log')[0] || '').trim() || c.noTasks; }).catch(() => {});
  let polling = false;
  async function poll() {
    if (dead || !conv || es || polling) return; // one poll at a time, or rows repeat and `since` skips ahead
    polling = true;
    const asked = conv;
    try {
      const r = await api(`events?conv=${asked}&since=${since}`);
      if (asked !== conv) return;
      for (const e of r.events) { const n = e.n ?? since; if (n >= since) { render(e); since = n + 1; } }
      setBusy(r.busy);
      if (r.events.length || busy) refreshWf();
    } catch { /* agent not running: health strip says so */ } finally { polling = false; }
  }
  // Live stream: one event per message, plus a `state` event when busy changes. If it fails before
  // anything arrives, the surface falls back to polling.
  function stream() {
    if (!sse || !conv || dead) return;
    let heard = false;
    const ctl = new AbortController();
    const me = { close: () => ctl.abort() };
    es = me;
    const take = block => {
      let ev = 'message', data = '';
      for (const line of block.split('\n')) {
        if (line.startsWith('event:')) ev = line.slice(6).trim();
        else if (line.startsWith('data:')) data += line.slice(5).trim();
      }
      if (!data) return;
      heard = true;
      const e = JSON.parse(data);
      if (ev === 'state') return setBusy(e.busy);
      if (e.n >= since) { render(e); since = e.n + 1; }
      if (e.type === 'tool_result' || e.type === 'assistant') refreshWf();
    };
    fetch(`/agent/stream?conv=${encodeURIComponent(conv)}&since=${since}`, { headers: keyHeaders(), signal: ctl.signal })
      .then(async r => {
        if (!r.ok || !r.body) throw new Error(`stream ${r.status}`);
        const rd = r.body.getReader(), dec = new TextDecoder();
        let buf = '';
        for (;;) {
          const { value, done } = await rd.read();
          if (done) break;
          buf += dec.decode(value, { stream: true });
          let i;
          while ((i = buf.indexOf('\n\n')) >= 0) { take(buf.slice(0, i)); buf = buf.slice(i + 2); }
        }
        throw new Error('stream closed');
      })
      .catch(() => {
        if (es === me) es = null;
        if (dead || ctl.signal.aborted) return;
        // it worked before: pick up again where it stopped; it never worked: poll instead
        if (heard) setTimeout(() => { if (!es && !dead) stream(); }, 1500); else { sse = false; poll(); }
      });
  }
  async function open_(id) {
    await ready;
    es?.close(); es = null;
    conv = id; saved.conv = id; since = 0; lastTool = null; log.replaceChildren(empty);
    feed?.poke(); // the nodes used in this conversation
    if (sse) { stream(); refreshWf(); } else await poll();
    convs();
  }
  async function convs() {
    try {
      const l = await api('convs');
      setNum('chats', l.length);
      list.replaceChildren(...l.slice(0, 8).map(id => h('button', { class: 'ch-conv', type: 'button', 'aria-current': id === conv ? 'true' : null, text: id, onclick: () => open_(id) })));
    } catch (e) { list.replaceChildren(h('p', { class: 'ch-empty', text: e.status === 401 ? (c.signIn || e.message) : c.failed })); }
  }
  async function send() {
    const t = input.value.trim();
    if (!t || busy) return;
    if (!conv) await open_((await api('new', {})).conv);
    input.value = '';
    await api('send', { conv, text: t });
    if (es) setBusy(true); else poll();
  }
  // Numbers in the status strip count up when the dashboard first shows them (and ease between values after).
  const nums = {};
  const num = (key, label) => {
    const v = h('span', { class: 'ch-num', text: '0' });
    nums[key] = v;
    return h('span', { class: 'ch-pair' }, lab(label), v);
  };
  const pairs = h('span', { class: 'ch-pairs' });
  const kpis = h('span', { class: 'ch-kpis' }, num('up', c.machinesUp), num('mem', c.memoryFiles), num('chats', c.chatCount));
  strip.append(pairs, kpis);
  const setNum = (key, n) => {
    const el = nums[key];
    el.dataset.want = String(n);
    if (view !== 'brain') countTo(el, n, calm());
  };
  const revealNums = () => { for (const el of Object.values(nums)) if (el.dataset.want != null && el.dataset.shown !== el.dataset.want) countTo(el, Number(el.dataset.want), calm()); };

  async function health() {
    try {
      const x = await api('health');
      heavyModel = x.models[0];
      sse = x.stream === true && 'ReadableStream' in window;
      const up = Object.entries(x.hosts).filter(([, v]) => v === 'up').map(([k]) => k);
      setNum('up', up.length);
      pairs.replaceChildren(
        h('span', { class: 'ch-pair' }, lab(c.engine), h('span', { class: 'ch-v', text: x.models[0] })),
        h('span', { class: 'ch-pair' }, lab(c.lane), laneV),
        h('span', { class: 'ch-pair' }, lab(c.net), h('span', { class: 'ch-v', text: x.net ? c.netUp : c.netDown })),
        h('span', { class: 'ch-pair' }, lab(c.hosts), h('span', { class: 'ch-v', text: up.join(', ') || c.none })));
    } catch { pairs.replaceChildren(h('span', { class: 'ch-v', text: c.failed })); }
  }

  // ---- second brain: file list, excerpts
  const kindLabel = k => (c.kinds && c.kinds[k]) || k;
  function drawMemory() {
    const q = memSearch.value.trim().toLowerCase();
    const files = memFiles.filter(f => !q || f.name.toLowerCase().includes(q));
    if (!memFiles.length) { memList.replaceChildren(h('p', { class: 'ch-empty', text: c.noMemory })); return; }
    const item = f => h('button', { class: 'ch-file', type: 'button', 'data-used': memUsed.has(f.src) ? 'true' : null, title: f.src, onclick: () => readFile(f.src) },
      h('span', { class: 'ch-dot', 'aria-hidden': 'true' }), h('span', { class: 'ch-fname', text: f.name }), h('span', { class: 'ch-fmeta', text: String(f.chunks) }));
    const groups = [];
    const usedNow = files.filter(f => memUsed.has(f.src));
    if (usedNow.length) groups.push(lab(c.used), ...usedNow.map(item));
    for (const k of KINDS) {
      const fs = files.filter(f => f.kind === k);
      if (fs.length) groups.push(lab(kindLabel(k) + ' · ' + fs.length), ...fs.map(item));
    }
    memList.replaceChildren(...groups);
  }
  async function loadMemory() {
    try {
      const r = await api('memory' + (conv ? '?conv=' + encodeURIComponent(conv) : ''));
      memFiles = r.files; memUsed = new Set(r.used);
      setNum('mem', memFiles.length);
      drawMemory();
    } catch { memList.replaceChildren(h('p', { class: 'ch-empty', text: c.failed })); }
  }
  function refreshUsed() { if (saved.tab === 'memory') loadMemory(); feed?.poke(); }
  async function readFile(src) {
    showTab('memory');
    try {
      const r = await api('memory/file?src=' + encodeURIComponent(src));
      if (r.error) return;
      memRead.replaceChildren(
        h('div', { class: 'ch-row' }, h('button', { class: 'ch-btn', type: 'button', text: c.back, onclick: () => { memRead.hidden = true; memList.hidden = false; } }), lab(kindLabel(r.kind))),
        h('p', { class: 'ch-fname ch-title', text: r.name }),
        ...r.chunks.map(t => h('pre', { class: 'ch-wf ch-chunk', text: t })),
        r.total > r.chunks.length ? h('p', { class: 'ch-empty', text: `+${r.total - r.chunks.length} ${c.moreChunks}` }) : null);
      memList.hidden = true; memRead.hidden = false; memRead.scrollTop = 0;
      if (view === 'brain') setView('both');
    } catch { /* stale index: ignore */ }
  }

  // ---- the Brain: the lattice sphere (memgraph.mjs), in its own pane
  const stage = h('div', { class: 'mg-stage' });
  const legend = h('div', { class: 'mg-legend' });
  const hint = h('span', { class: 'mg-hint', text: c.graphHint });
  const brainNote = h('p', { class: 'mg-note', hidden: true });
  const brainPane = h('section', { class: 'ch-brain', 'aria-label': c.graphTitle },
    stage, h('div', { class: 'mg-hd' }, lab(c.graphTitle), h('h1', { text: c.brainTitle }), legend, hint), brainNote);
  let opening = false;
  // One look at the brain: the graph plus the file list (its mtimes), merged. Also what the feed polls.
  async function loadBrain() {
    const q = conv ? '?conv=' + encodeURIComponent(conv) : '';
    const [g, m] = await Promise.all([api('memory/graph' + q), api('memory' + q).catch(() => null)]);
    if (!g.nodes) throw new Error('no graph');
    if (m?.files) {
      const mt = new Map(m.files.map(f => [f.src, f.mtime]));
      for (const n of g.nodes) n.mtime = mt.get(n.src);
      memFiles = m.files; memUsed = new Set(m.used);
      setNum('mem', memFiles.length);
      if (saved.tab === 'memory') drawMemory();
    }
    return g;
  }
  async function openBrain() {
    if (graph || opening) return;
    opening = true;
    legend.replaceChildren(...['current', 'core', 'memory', 'pack', 'used'].map(k => h('span', { class: 'mg-key', 'data-kind': k, text: k === 'used' ? c.used : kindLabel(k) })));
    let g = { nodes: [], edges: [] }, why = '';
    try {
      g = await loadBrain();
      if (!g.nodes.length) why = c.graphEmpty;
    } catch (e) { why = e.status === 401 ? (c.signIn || e.message) : c.failed; }
    try {
      const { mountBrain } = await import(new URL('./brain-world.mjs', import.meta.url).href);
      if (view === 'dashboard' || dead) return;
      stage.replaceChildren();
      const copy = { open: c.brainOpen, close: c.brainClose, links: c.brainLinks, changed: c.brainChanged, search: c.brainSearch, noMatch: c.brainNoMatch, loadError: c.brainLoadError, nodes: c.brainNodes };
      graph = await mountBrain(stage, { data: g, source: 'agent', reduced: reducedMotion(), copy, flatLabel: c.graphTitle, onOpen: node => readFile(node.id) });
      graph.setThinking(busy); graph.pause(paused);
      hint.textContent = graph.mode === 'flat' ? c.graphHintFlat : c.graphHint;
      brainNote.hidden = !why;
      brainNote.textContent = why;
      const { createFeed } = await import(new URL('./brain-feed.mjs', import.meta.url).href);
      if (!graph || dead) return;
      feed = createFeed({
        load: loadBrain,
        onData: raw => { graph?.setData(raw); brainNote.hidden = raw.nodes.length > 0; if (!raw.nodes.length) { brainNote.hidden = false; brainNote.textContent = c.graphEmpty; } },
        interval: 8000, fast: 2500,
      });
      feed.prime(g); feed.setFast(busy); feed.start(false);
    } catch { brainNote.hidden = false; brainNote.textContent = c.failed; } finally { opening = false; }
  }
  function closeBrain() { feed?.stop(); feed = null; graph?.close(); graph = null; }

  // ---- view (Brain, Dashboard, Both), theme (System, Light, Dark), pause motion; all remembered
  let view = VIEWS.includes(store.get('view', 'dashboard')) ? store.get('view', 'dashboard') : 'dashboard';
  let theme = THEMES.includes(store.get('theme', 'system')) ? store.get('theme', 'system') : 'system';
  let paused = store.get('motion', '') === 'still';
  const calm = () => reducedMotion() || paused;
  // A View Transition where the browser has one (the panes slide and cross-fade); the panes' own rise
  // animation is the fallback. Never while motion is off.
  function swap(change) {
    if (document.startViewTransition && !calm()) {
      root.classList.add('vt');
      const t = document.startViewTransition(() => { change(); });
      t.finished.finally(() => root.classList.remove('vt'));
    } else change();
  }
  function setView(v) {
    if (!VIEWS.includes(v)) v = 'dashboard';
    view = v;
    store.set('view', v);
    swap(() => {
      root.dataset.view = v;
      for (const b of viewBtns) b.setAttribute('aria-pressed', String(b.dataset.view === v));
      moveBars();
    });
    if (v === 'dashboard') closeBrain(); else openBrain();
    if (v !== 'brain') revealNums();
  }
  function paintTheme() {
    root.dataset.theme = theme;
    for (const b of themeBtns) b.setAttribute('aria-pressed', String(b.dataset.theme === theme));
    graph?.retheme();
  }
  function setTheme(t) {
    if (!THEMES.includes(t)) t = 'system';
    theme = t;
    store.set('theme', t);
    if (document.startViewTransition && !calm()) document.startViewTransition(paintTheme);
    else if (!calm()) { root.classList.add('tt-fading'); paintTheme(); setTimeout(() => root.classList.remove('tt-fading'), 400); } else paintTheme();
  }
  function setPaused(p) {
    paused = p;
    store.set('motion', p ? 'still' : '');
    root.dataset.motion = p ? 'still' : '';
    pauseBtn.setAttribute('aria-pressed', String(p));
    graph?.pause(p);
  }
  function openTab(name) {
    const sameOpen = saved.tab === name && dash.dataset.side === 'open';
    showTab(name);
    if (view === 'brain') setView('dashboard');
    dash.dataset.side = sameOpen ? 'closed' : 'open';
  }
  function showTab(name) {
    saved.tab = name;
    for (const [k, p] of Object.entries(panes)) p.hidden = k !== name;
    for (const b of tabBtns) b.setAttribute('aria-current', String(b.dataset.tab === name));
    if (name === 'memory') loadMemory();
    moveBars();
  }
  // The orange bars slide to the current rail items (transform only).
  function moveBars() {
    for (const [bar, sel] of [[barView, '[data-view][aria-pressed=true]'], [barTab, '[data-tab][aria-current=true]']]) {
      const cur = rail.querySelector(sel);
      if (!cur) continue;
      bar.style.height = `${cur.offsetHeight}px`;
      bar.style.transform = `translateY(${cur.offsetTop}px)`;
    }
  }

  const railBtn = (attrs, kbd, text) => h('button', { type: 'button', ...attrs }, h('kbd', { text: kbd }), text);
  const viewBtns = [railBtn({ 'data-view': 'brain', title: c.brain }, 'B', c.brain), railBtn({ 'data-view': 'dashboard', title: c.dashboard }, 'D', c.dashboard), railBtn({ 'data-view': 'both', title: c.both }, '⇧B', c.both)];
  const tabBtns = [['chats', 'C', c.chats], ['tasks', 'W', c.tasks], ['memory', 'M', c.memory]].map(([k, key, t]) => railBtn({ 'data-tab': k }, key, t));
  const barView = h('i', { class: 'ch-bar', 'aria-hidden': 'true' }), barTab = h('i', { class: 'ch-bar', 'aria-hidden': 'true' });
  const setBtn = railBtn({ 'aria-haspopup': 'dialog', title: c.settings }, ',', c.settings);
  const rail = h('nav', { class: 'ch-rail', 'aria-label': c.view }, barView, barTab,
    h('div', { class: 'sub', text: c.view }), ...viewBtns, h('div', { class: 'sub', text: c.panels }), ...tabBtns, h('div', { class: 'gap' }), setBtn);
  for (const b of viewBtns) b.addEventListener('click', () => setView(b.dataset.view));
  for (const b of tabBtns) b.addEventListener('click', () => openTab(b.dataset.tab));

  // Settings and About: the theme, pause motion, and who made this
  const themeBtns = THEMES.map(t => h('button', { type: 'button', 'data-theme': t, text: c[t], onclick: () => setTheme(t) }));
  const pauseBtn = h('button', { class: 'ch-btn', type: 'button', 'aria-pressed': String(paused), text: c.pause, onclick: () => setPaused(!paused) });
  const pop = h('div', { class: 'ch-pop', role: 'dialog', 'aria-label': c.settings, hidden: true },
    h('h2', { text: c.settings }),
    lab(c.theme), h('div', { class: 'tt-seg', role: 'group', 'aria-label': c.theme }, ...themeBtns),
    pauseBtn,
    reducedMotion() ? h('p', { text: c.motionNote }) : null,
    lab(c.about), h('p', { text: c.madeIn }));
  const closePop = () => { pop.hidden = true; setBtn.removeAttribute('aria-expanded'); };
  setBtn.addEventListener('click', () => { pop.hidden = !pop.hidden; setBtn.setAttribute('aria-expanded', String(!pop.hidden)); });

  // ---- the thread's events drive the sphere and the insignia (see also render and setBusy)
  sendBtn.addEventListener('click', send);
  input.addEventListener('keydown', e => { if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); send(); } });
  newBtn.addEventListener('click', async () => open_((await api('new', {})).conv));
  memSearch.addEventListener('input', drawMemory);
  graphBtn.addEventListener('click', () => setView(view === 'dashboard' ? 'both' : 'brain'));
  const onKey = e => {
    if (e.key === 'Escape') { if (!pop.hidden) return closePop(); if (view === 'brain') return setView('dashboard'); return; }
    if (e.ctrlKey || e.metaKey || e.altKey || e.defaultPrevented) return;
    if (e.target?.closest?.('input, textarea, select, [contenteditable]')) return;
    const k = e.key;
    if (k === 'B') setView('both');
    else if (k === 'b') setView('brain');
    else if (k === 'd' || k === 'D') setView('dashboard');
    else if (k === 'c') openTab('chats');
    else if (k === 'w') openTab('tasks');
    else if (k === 'm') openTab('memory');
    else if (k === 'p') setPaused(!paused);
    else if (k === ',') setBtn.click();
    else return;
    e.preventDefault();
  };
  document.addEventListener('keydown', onKey);

  const dash = h('section', { class: 'ch-dash', 'data-side': 'closed', 'aria-label': c.dashboard },
    strip,
    h('div', { class: 'ch-body' },
      h('aside', { class: 'ch-side' }, ...Object.values(panes)),
      h('div', { class: 'ch-main' }, log, h('div', { class: 'ch-comp' }, input, sendBtn))));
  // The insignia is mounted in ONE element: replace this span to swap in a live version (see README).
  const brand = h('span', { class: 'tt-brand' },
    h('span', { class: 'tt-ins tilt', 'data-insignia': '' }, h('i', { class: 'tt-led' })),
    h('span', { class: 'tt-wm', role: 'img', 'aria-label': 'perennos' }), h('b', { text: 'tempo' }));
  const dot = h('i', { class: 'tt-dot idle' }), whereText = h('b', { text: c.idle });
  const root = h('div', { class: 'ch tempo', 'data-view': view, 'data-theme': theme, 'data-motion': paused ? 'still' : '', 'data-busy': 'false' },
    h('header', { class: 'ch-top' }, brand, h('span', { class: 'ch-where' }, dot, whereText)),
    rail, h('div', { class: 'ch-views' }, brainPane, dash), pop);
  el.append(root);
  showTab(saved.tab || 'chats');
  paintTheme();
  setView(view);
  requestAnimationFrame(moveBars);
  const ro = new ResizeObserver(moveBars);
  ro.observe(root);
  const ready = health();
  loadMemory();
  convs();
  if (conv) open_(conv);
  timer = setInterval(poll, 1200);
  const ht = setInterval(health, 30000);
  return {
    unmount() {
      dead = true; es?.close(); closeBrain(); ro.disconnect(); clearInterval(timer); clearInterval(ht);
      document.removeEventListener('keydown', onKey); links.forEach(l => l.remove()); root.remove();
    },
  };
}
