// CHAT surface: the local agent loop (Ollama engine, MCP tools, memory RAG, delegate, workflow file).
// Talks only to the same origin at /agent/*. Every visible string comes from copy.json.
// The second-brain graph (memgraph.mjs, three.js) is imported only when it is opened.
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

export async function mount(el, ctx) {
  const c = ctx.copy;
  if (!c?.title) return { unmount() {} };
  const link = h('link', { rel: 'stylesheet', href: new URL('./chat.css', import.meta.url).href });
  const loaded = new Promise(r => { link.onload = link.onerror = r; });
  document.head.append(link);
  await loaded;

  const saved = ctx.view ? (ctx.view.chat ??= {}) : {};
  let conv = saved.conv || null, since = 0, busy = false, dead = false, timer = 0;
  let heavyModel = '', lane = null, lastTool = null, memUsed = new Set(), memFiles = [], graph = null;
  let sse = false, es = null;

  const lab = t => h('span', { class: 't-label', text: t });
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
  const graphBtn = h('button', { class: 'ch-btn', type: 'button', text: c.graph });
  const panes = {
    chats: h('div', { class: 'ch-pane' }, newBtn, list),
    tasks: h('div', { class: 'ch-pane' }, wf),
    memory: h('div', { class: 'ch-pane' }, h('div', { class: 'ch-row' }, memSearch, graphBtn), memList, memRead),
  };
  const tabs = h('div', { class: 'ch-tabs', role: 'tablist' });
  function showTab(name) {
    saved.tab = name;
    for (const [k, p] of Object.entries(panes)) p.hidden = k !== name;
    for (const b of tabs.children) b.setAttribute('aria-selected', String(b.dataset.tab === name));
    if (name === 'memory') loadMemory();
  }
  for (const [k, t] of [['chats', c.chats], ['tasks', c.tasks], ['memory', c.memory]]) {
    tabs.append(h('button', { class: 'ch-tab', type: 'button', role: 'tab', 'data-tab': k, text: t, onclick: () => showTab(k) }));
  }

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
      if (!live) { live = row('ch-bot ch-live', c.tempo, ''); }
      live.querySelector('.ch-t').textContent += e.text;
      log.scrollTop = log.scrollHeight;
      return;
    }
    if (live && e.type !== 'status') { live.remove(); live = null; }
    if (e.type === 'user') row('ch-user', c.you, e.text);
    else if (e.type === 'assistant') row('ch-bot', c.tempo + (e.model ? ' / ' + e.model : ''), e.text);
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
  const setBusy = b => { busy = b; sendBtn.disabled = busy; sendBtn.textContent = busy ? c.busy : c.send; };
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
    if (sse) { stream(); refreshWf(); } else await poll();
    convs();
  }
  async function convs() {
    try {
      const l = await api('convs');
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
  async function health() {
    try {
      const x = await api('health');
      heavyModel = x.models[0];
      sse = x.stream === true && 'ReadableStream' in window;
      const up = Object.entries(x.hosts).filter(([, v]) => v === 'up').map(([k]) => k);
      strip.replaceChildren(
        h('span', { class: 'ch-pair' }, lab(c.engine), h('span', { class: 'ch-v', text: x.models[0] })),
        h('span', { class: 'ch-pair' }, lab(c.lane), laneV),
        h('span', { class: 'ch-pair' }, lab(c.net), h('span', { class: 'ch-v', text: x.net ? c.netUp : c.netDown })),
        h('span', { class: 'ch-pair' }, lab(c.hosts), h('span', { class: 'ch-v', text: up.join(', ') || c.none })));
    } catch { strip.replaceChildren(h('span', { class: 'ch-v', text: c.failed })); }
  }

  // ---- second brain: file list, excerpts, graph
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
      drawMemory();
    } catch { memList.replaceChildren(h('p', { class: 'ch-empty', text: c.failed })); }
  }
  function refreshUsed() { if (saved.tab === 'memory' || graph) loadMemory().then(() => { if (graph) openGraph(); }); }
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
    } catch { /* stale index: ignore */ }
  }
  const stage = h('div', { class: 'mg-stage' });
  const legend = h('div', { class: 'mg-legend' });
  const hint = h('span', { class: 'mg-hint', text: c.graphHint });
  const overlay = h('div', { class: 'mg', hidden: true, role: 'dialog', 'aria-label': c.graphTitle },
    h('div', { class: 'mg-head' }, lab(c.graphTitle), hint, legend,
      h('button', { class: 'ch-btn', type: 'button', text: c.close, onclick: () => closeGraph() })),
    stage);
  async function openGraph() {
    if (graph) graph.close();
    overlay.hidden = false;
    legend.replaceChildren(...['current', 'core', 'memory', 'pack', 'used'].map(k => h('span', { class: 'mg-key', 'data-kind': k, text: k === 'used' ? c.used : kindLabel(k) })));
    try {
      if (!memFiles.length) await loadMemory();
      const g = await api('memory/graph' + (conv ? '?conv=' + encodeURIComponent(conv) : ''));
      if (!g.nodes.length) { stage.replaceChildren(h('p', { class: 'ch-empty', text: c.graphEmpty })); return; }
      const { openGraph: draw } = await import(new URL('./memgraph.mjs', import.meta.url).href);
      graph = await draw(stage, g, { used: memUsed, reduced: reducedMotion(), flatLabel: c.graphTitle, onPick: src => readFile(src) });
      overlay.dataset.mode = graph.mode;
      hint.textContent = graph.mode === 'flat' ? c.graphHintFlat : c.graphHint;
    } catch { stage.replaceChildren(h('p', { class: 'ch-empty', text: c.failed })); }
  }
  function closeGraph() { graph?.close(); graph = null; overlay.hidden = true; }
  graphBtn.addEventListener('click', openGraph);
  memSearch.addEventListener('input', drawMemory);

  sendBtn.addEventListener('click', send);
  input.addEventListener('keydown', e => { if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); send(); } });
  newBtn.addEventListener('click', async () => open_((await api('new', {})).conv));
  const onKey = e => { if (e.key === 'Escape' && graph) closeGraph(); };
  document.addEventListener('keydown', onKey);

  const root = h('div', { class: 'ch' },
    strip,
    h('div', { class: 'ch-body' },
      h('aside', { class: 'ch-side' }, tabs, ...Object.values(panes)),
      h('section', { class: 'ch-main' }, log, h('div', { class: 'ch-comp' }, input, sendBtn), overlay)));
  el.append(root);
  showTab(saved.tab || 'chats');
  const ready = health();
  convs();
  if (conv) open_(conv);
  timer = setInterval(poll, 1200);
  const ht = setInterval(health, 30000);
  return { unmount() { dead = true; es?.close(); closeGraph(); clearInterval(timer); clearInterval(ht); document.removeEventListener('keydown', onKey); link.remove(); } };
}
