// CHAT surface: the local agent loop (Ollama engine, MCP tools, memory RAG, delegate, workflow file).
// Talks only to the same origin at /agent/*. Every visible string comes from copy.json.
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
const api = (p, b) => fetch('/agent/' + p, b ? { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(b) } : {}).then(r => r.json());

export async function mount(el, ctx) {
  const c = ctx.copy;
  if (!c?.title) return { unmount() {} };
  const link = h('link', { rel: 'stylesheet', href: new URL('./chat.css', import.meta.url).href });
  const loaded = new Promise(r => { link.onload = link.onerror = r; });
  document.head.append(link);
  await loaded;

  const saved = ctx.view ? (ctx.view.chat ??= {}) : {};
  let conv = saved.conv || null, since = 0, busy = false, dead = false, timer = 0;

  const log = h('div', { class: 'ch-log', role: 'log', 'aria-live': 'polite' });
  const empty = h('p', { class: 'ch-empty', text: c.empty });
  log.append(empty);
  const strip = h('div', { class: 'ch-strip' });
  const wf = h('pre', { class: 'ch-wf', text: c.noTasks });
  const list = h('div', { class: 'ch-convs' });
  const input = h('textarea', { class: 'ch-in', rows: '2', placeholder: c.placeholder, 'aria-label': c.title });
  const sendBtn = h('button', { class: 'ch-btn ch-go', type: 'button', text: c.send });
  const newBtn = h('button', { class: 'ch-btn', type: 'button', text: c.newChat });

  const lab = t => h('span', { class: 't-label', text: t });
  function row(cls, who, text) {
    empty.remove();
    const d = h('div', { class: 'ch-m ' + cls }, who ? lab(who) : null, h('div', { class: 'ch-t', text }));
    log.append(d);
    log.scrollTop = log.scrollHeight;
    return d;
  }
  let live = null;
  function render(e) {
    if (e.type === 'delta') { if (!live) live = row('ch-bot', c.tempo, ''); live.querySelector('.ch-t').textContent += e.text; log.scrollTop = log.scrollHeight; return; }
    if (live && e.type !== 'status') { live.remove(); live = null; }
    if (e.type === 'user') row('ch-user', c.you, e.text);
    else if (e.type === 'assistant') row('ch-bot', c.tempo + (e.model ? ' / ' + e.model : ''), e.text);
    else if (e.type === 'assistant_partial') row('ch-bot', c.tempo, e.text);
    else if (e.type === 'tool_call') row('ch-tool', null, '> ' + e.name + ' ' + (e.args || ''));
    else if (e.type === 'tool_result') row('ch-tool', null, '< ' + (e.text || '').slice(0, 700));
    else if (e.type === 'status') row('ch-status', null, e.text);
    else if (e.type === 'error') row('ch-err', 'Error', e.text);
    else if (e.type === 'confirm') {
      const d = row('ch-confirm', c.needsYou, e.what);
      d.append(h('code', { text: e.detail }));
      const done = ok => { api('confirm', { conv, id: e.id, ok }); d.remove(); };
      d.append(h('button', { class: 'ch-btn ch-go', type: 'button', text: c.approve, onclick: () => done(true) }),
        h('button', { class: 'ch-btn', type: 'button', text: c.deny, onclick: () => done(false) }));
    }
  }
  async function poll() {
    if (dead || !conv) return;
    try {
      const r = await api(`events?conv=${conv}&since=${since}`);
      r.events.forEach(render); since += r.events.length; busy = r.busy;
      sendBtn.disabled = busy; sendBtn.textContent = busy ? c.busy : c.send;
      if (r.events.length || busy) api('workflow?conv=' + conv).then(w => { wf.textContent = (w.text.split('## Log')[0] || '').trim() || c.noTasks; });
    } catch { /* agent not running: health strip says so */ }
  }
  async function open_(id) {
    conv = id; saved.conv = id; since = 0; log.replaceChildren(empty);
    await poll(); convs();
  }
  async function convs() {
    try {
      const l = await api('convs');
      list.replaceChildren(...l.slice(0, 8).map(id => h('button', { class: 'ch-conv', type: 'button', 'aria-current': id === conv ? 'true' : null, text: id, onclick: () => open_(id) })));
    } catch { list.replaceChildren(h('p', { class: 'ch-empty', text: c.failed })); }
  }
  async function send() {
    const t = input.value.trim();
    if (!t || busy) return;
    if (!conv) await open_((await api('new', {})).conv);
    input.value = '';
    await api('send', { conv, text: t });
    poll();
  }
  async function health() {
    try {
      const x = await api('health');
      const up = Object.entries(x.hosts).filter(([, v]) => v === 'up').map(([k]) => k);
      strip.replaceChildren(
        h('span', { class: 'ch-pair' }, lab(c.engine), h('span', { class: 'ch-v', text: x.models[0] })),
        h('span', { class: 'ch-pair' }, lab(c.net), h('span', { class: 'ch-v', text: x.net ? c.netUp : c.netDown })),
        h('span', { class: 'ch-pair' }, lab(c.hosts), h('span', { class: 'ch-v', text: up.join(', ') || c.none })));
    } catch { strip.replaceChildren(h('span', { class: 'ch-v', text: c.failed })); }
  }

  sendBtn.addEventListener('click', send);
  input.addEventListener('keydown', e => { if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); send(); } });
  newBtn.addEventListener('click', async () => open_((await api('new', {})).conv));

  const root = h('div', { class: 'ch' },
    strip,
    h('div', { class: 'ch-body' },
      h('aside', { class: 'ch-side' }, lab(c.chats), newBtn, list, lab(c.tasks), wf),
      h('section', { class: 'ch-main' }, log, h('div', { class: 'ch-comp' }, input, sendBtn))));
  el.append(root);
  health(); convs();
  if (conv) open_(conv);
  timer = setInterval(poll, 1200);
  const ht = setInterval(health, 30000);
  return { unmount() { dead = true; clearInterval(timer); clearInterval(ht); link.remove(); } };
}

