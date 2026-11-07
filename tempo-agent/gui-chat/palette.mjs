// SLASH: the menu that opens when you type "/" in the chat box, the suggestion cards in an empty chat,
// the ghost text for slash commands, and the places where help lives (guides, keyboard map, "what does
// this mean?"). The first half is plain functions with no page in them (so node can test it); the
// second half, attach(), wires them to the chat. chat.mjs calls attach() once and touches nothing else.
// Every visible string is in prompts.json.

// ---------------------------------------------------------------- ranking
const SEP = /[\s_\-/:.]/;

// Fuzzy match: the letters of q in order inside t. Prefixes, word starts and runs score higher.
// Returns { score, idx } (idx: where each letter landed, for highlighting) or null.
export function fuzzy(q, t) {
  q = q.toLowerCase(); const lt = t.toLowerCase();
  if (!q) return { score: 0, idx: [] };
  const n = lt.length, m = q.length;
  if (m > n) return null;
  const NEG = -1e9;
  const dp = Array.from({ length: m }, () => new Array(n).fill(NEG));
  const from = Array.from({ length: m }, () => new Array(n).fill(-1));
  for (let i = 0; i < m; i++) {
    for (let j = i; j < n; j++) {
      if (lt[j] !== q[i]) continue;
      const here = 10 + (j === 0 ? 20 : SEP.test(lt[j - 1]) ? 12 : 0);
      if (i === 0) { dp[0][j] = here - j * 0.5; continue; }
      for (let k = i - 1; k < j; k++) {
        if (dp[i - 1][k] === NEG) continue;
        const s = dp[i - 1][k] + here + (k === j - 1 ? 9 : -Math.min(j - k - 1, 6));
        if (s > dp[i][j]) { dp[i][j] = s; from[i][j] = k; }
      }
    }
  }
  let bj = -1, best = NEG;
  for (let j = 0; j < n; j++) if (dp[m - 1][j] > best) { best = dp[m - 1][j]; bj = j; }
  if (bj < 0) return null;
  const idx = [];
  for (let i = m - 1, j = bj; i >= 0; j = from[i][j], i--) idx.unshift(j);
  let score = best - n * 0.2;
  if (lt === q) score += 100; else if (lt.startsWith(q)) score += 40;
  return { score, idx };
}

// How well an item answers a query: its name first, its description a little.
export function scoreItem(it, q) {
  const name = (it.search ?? it.label).replace(/^[/!@]/, '');
  const a = fuzzy(q, name);
  let s = a ? a.score : -Infinity;
  const d = it.desc && it.desc.toLowerCase().includes(q.toLowerCase()) ? 8 : -Infinity;
  if (d > s) s = d;
  return { score: s, idx: a && s === a.score ? a.idx : [] };
}

// Items that match, best first. An empty query keeps the order they came in.
export function rank(items, q) {
  q = q.trim();
  if (!q) return items.map(it => ({ it, idx: [], score: 0 }));
  return items.map((it, n) => ({ it, n, ...scoreItem(it, q) }))
    .filter(r => r.score > -Infinity)
    .sort((a, b) => b.score - a.score || a.n - b.n);
}

// ---------------------------------------------------------------- templates
// A blank is {words}. Tab jumps from one to the next.
const BLANK = /\{([^{}\n]{1,60})\}/g;
export const blanks = text => [...text.matchAll(BLANK)].map(m => ({ start: m.index, end: m.index + m[0].length, name: m[1] }));

// The next blank at or after `from` (dir 1), or the last one that ends at or before it (dir -1); wraps round.
export function nextBlank(text, from, dir = 1) {
  const all = blanks(text);
  if (!all.length) return null;
  if (dir > 0) return all.find(b => b.start >= from) || all[0];
  return [...all].reverse().find(b => b.end <= from) || all[all.length - 1];
}

// Fill the blanks we already know (the band and flow names) and leave the rest.
export const subst = (text, ctx = {}) => text.replace(BLANK, (m, k) => (ctx[k] != null && ctx[k] !== '' ? ctx[k] : m));

const TOOL_TEMPLATES = {
  list_dir: 'Use list_dir to see what is in {folder}.',
  read_file: 'Use read_file to read {file}, then tell me {what to look for}.',
  write_file: 'Use write_file to save {what to write} to {file}.',
  edit_file: 'Use edit_file in {file} to change {the old text} to {the new text}.',
  apply_patch: 'Use apply_patch to {describe the change} across {the files}.',
  grep: 'Use grep to find {text} in {folder}.',
  glob: 'Use glob to find {a pattern, like **/*.md} in {folder}.',
  run_command: 'Use run_command to run {command} in {folder}.',
  command_output: 'Use command_output to check on {bg-1}.',
  command_stop: 'Use command_stop to end {bg-1}.',
  trash: 'Use trash to move {a file or folder} to the trash.',
  fetch_url: 'Use fetch_url to read {an address}, then give me the gist.',
  search_memory: 'Use search_memory to look through my notes for {a topic}.',
  workflow_update: 'Use workflow_update to write down the steps for {the job}.',
  delegate: 'Use delegate to ask {a machine} to {do the job}.',
  comfy_image: 'Use comfy_image to make an image of {what you picture}.',
};
export const toolTemplate = name => TOOL_TEMPLATES[name] || `Use ${name} to {what you want done}.`;

// A command: fills `/cd {folder}` when it needs an argument, `/mode ` when the argument is optional.
export function commandFill(c) {
  const a = (c.args || '').trim();
  if (c.name === '!') return '!{command}';
  if (c.name.startsWith('@')) return '@{' + c.name.slice(1) + '}';
  const req = a.match(/^<([^>]+)>/);
  return req ? `${c.name} {${req[1]}}` : `${c.name} `;
}
// Enter runs a command that needs nothing, and fills one that must be told something.
export const commandRuns = c => c.kind === 'command' && !/^</.test((c.args || '').trim());

// ---------------------------------------------------------------- the menu's rows
export const GROUPS = ['command', 'custom', 'tool', 'mcp', 'fleet', 'band', 'app', 'guide'];

export function buildItems(cat, P) {
  const out = [];
  const add = (group, o) => out.push({ group, approval: false, ...o });
  for (const c of cat.commands || []) {
    const label = c.kind === 'syntax' ? (c.name === '!' ? '!' + (c.args || '') : c.name) : c.name;
    add('command', { id: 'c:' + c.name, label, search: c.name, args: c.kind === 'syntax' ? '' : c.args, desc: c.help,
      kind: commandRuns(c) ? 'run' : 'fill', text: commandRuns(c) ? c.name : commandFill(c), fill: commandFill(c) });
  }
  add('command', { id: 'c:/guide', label: '/guide', args: '[topic]', desc: 'Open the guides: short lessons with examples to try.', kind: 'guide', text: '', fill: '/guide ' });
  add('command', { id: 'c:/keys', label: '/keys', args: '', desc: 'Show the keyboard map.', kind: 'keys', text: '', fill: '/keys ' });
  for (const c of cat.custom || []) add('custom', { id: 'u:' + c.name, label: c.name, desc: c.description || 'Your own command.', kind: 'fill', text: c.name + ' ', fill: c.name + ' ' });
  for (const t of cat.tools || []) add('tool', { id: 't:' + t.name, label: t.name, desc: t.description, approval: !!t.approval, kind: 'fill', text: toolTemplate(t.name), fill: toolTemplate(t.name) });
  for (const t of cat.mcp || []) {
    const fleet = /fleet/i.test(t.server || '');
    add(fleet ? 'fleet' : 'mcp', { id: 'm:' + t.name, label: t.name, desc: t.description || `From ${t.server}.`, approval: !!t.approval, kind: 'fill',
      text: toolTemplate(t.name), fill: toolTemplate(t.name) });
  }
  for (const b of cat.bands || []) {
    for (const f of b.flows || []) {
      const text = `Run the ${f.name} flow of the ${b.name} band, and tell me where the notes landed.`;
      add('band', { id: `b:${b.name}:${f.name}`, label: `${b.name} ${f.name}`, search: `${b.name} ${f.name}`,
        desc: f.goal || b.about || 'A flow in your band.', kind: 'fill', text, fill: text });
    }
  }
  for (const a of P.apps || []) {
    add('app', { id: 'a:' + a.id, label: 'Open ' + a.name, search: a.name + ' ' + a.id, desc: a.desc, kind: 'fill', text: a.prompt, fill: a.prompt });
  }
  for (const g of P.guides || []) {
    add('guide', { id: 'g:' + g.id, label: g.label, search: g.label, desc: g.lead, kind: 'guide', guide: g.id, text: '', fill: '/guide ' + g.id });
  }
  return out;
}

// Rows for a query, in groups: the group with the best match first.
export function grouped(items, q) {
  const hits = rank(items, q);
  const by = new Map();
  for (const h of hits) { if (!by.has(h.it.group)) by.set(h.it.group, []); by.get(h.it.group).push(h); }
  const order = [...by.keys()].sort((a, b) => (q.trim() ? by.get(b)[0].score - by.get(a)[0].score : 0) || GROUPS.indexOf(a) - GROUPS.indexOf(b));
  return order.map(g => ({ group: g, rows: by.get(g) }));
}

// Slash text the menu answers to: a "/" and the first word, nothing after.
export const slashQuery = text => { const m = /^\/([^\s]*)$/.exec(text); return m ? m[1] : null; };

// The rest of a slash command, as grey text after the cursor. Commands only, never prose.
export function ghostFor(text, names) {
  const q = slashQuery(text);
  if (q == null || q.length < 1) return '';
  const lt = text.toLowerCase();
  const hit = names.find(n => n.toLowerCase().startsWith(lt) && n.length > text.length);
  return hit ? hit.slice(text.length) : '';
}

// ---------------------------------------------------------------- suggestion cards
export function has(token, cat, health) {
  const [kind, ...rest] = token.split(':');
  const v = rest.join(':');
  if (kind === 'tool') return (cat.tools || []).some(t => t.name === v) || (cat.mcp || []).some(t => t.name === v);
  if (kind === 'command') return (cat.commands || []).some(c => c.name === v);
  if (kind === 'mcp') return (cat.mcp || []).some(t => (t.server || '').toLowerCase().includes(v));
  if (kind === 'no') return !has(v, cat, health);
  if (token === 'bands') return !!(cat.bands || []).some(b => (b.flows || []).length);
  if (token === 'comfy') return !!cat.comfy;
  if (token === 'fleet') return !!cat.fleet;
  if (token === 'net') return !!(health && health.net);
  return false;
}

// A small seeded shuffle, so a card set can be repeated in tests and rotates when the seed moves.
export function shuffle(list, seed) {
  const a = [...list];
  let s = (seed >>> 0) || 1;
  const rnd = () => { s = (s + 0x6d2b79f5) >>> 0; let t = s; t = Math.imul(t ^ (t >>> 15), t | 1); t ^= t + Math.imul(t ^ (t >>> 7), t | 61); return ((t ^ (t >>> 14)) >>> 0) / 4294967296; };
  for (let i = a.length - 1; i > 0; i--) { const j = Math.floor(rnd() * (i + 1)); [a[i], a[j]] = [a[j], a[i]]; }
  return a;
}

// Four to six cards that fit this computer: nothing whose tool is missing.
export function pickCards(P, cat, health, seed, n = 5) {
  const band = (cat.bands || []).find(b => (b.flows || []).length);
  const ctx = band ? { band: band.name, flow: band.flows[0].name } : {};
  const ok = (P.suggestions || []).filter(s => (s.needs || []).every(t => has(t, cat, health)));
  return shuffle(ok, seed).slice(0, Math.max(4, Math.min(6, n))).map(s => ({ ...s, prompt: subst(s.prompt, ctx), why: subst(s.why || '', ctx) }));
}

// The guide that answers an error, by what it says.
export function guideForError(text, P) {
  for (const e of P.errorGuides || []) if (new RegExp(e.match, 'i').test(text)) return e.guide;
  return 'ask';
}

// ---------------------------------------------------------------- the page half
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
const mem = {
  get(k, d) { try { const v = localStorage.getItem('tempo-chat:' + k); return v === null ? d : v; } catch { return d; } },
  set(k, v) { try { localStorage.setItem('tempo-chat:' + k, v); } catch { /* private window: fine */ } },
};

// ctx: { root, input, log, empty, api(path), send() }
export async function attach(ctx) {
  const here = f => new URL(f, import.meta.url).href;
  const css = h('link', { rel: 'stylesheet', href: here('./palette.css') });
  document.head.append(css);
  let P;
  try { P = await (await fetch(here('./prompts.json'))).json(); } catch { css.remove(); return { detach() {} }; }
  const U = P.ui;
  const { root, input, log, empty } = ctx;
  const host = input.parentElement;
  host.classList.add('pm-host');

  let cat = { commands: [], tools: [], mcp: [], custom: [], bands: [], comfy: false, fleet: false };
  let health = {}, items = [], loaded = 0;
  async function load() {
    try { cat = await ctx.api('catalog'); if (!cat.commands) throw new Error('no catalog'); } catch { /* not signed in, or the agent is down: the guides still work */ }
    try { health = await ctx.api('health'); } catch { /* the strip says so */ }
    items = buildItems(cat, P);
    loaded = Date.now();
    paintCards();
    if (open) paintMenu();
  }
  items = buildItems(cat, P);

  // ---- put text in the box and select its first blank
  function setText(t) {
    input.value = t;
    input.focus();
    const b = nextBlank(t, 0);
    if (b) input.setSelectionRange(b.start, b.end); else input.setSelectionRange(t.length, t.length);
    update();
  }

  // ---- the menu
  const menu = h('div', { class: 'pm', role: 'listbox', 'aria-label': U.menuLabel, hidden: true });
  const foot = h('div', { class: 'pm-foot', text: U.foot });
  host.append(menu);
  let open = false, rows = [], sel = 0;
  const gname = g => U.groups[g] || g;
  function hl(label, idx) {
    if (!idx.length) return [label];
    const off = /^[/!@]/.test(label) ? 1 : 0, out = [];
    let last = 0;
    for (const i of idx.map(i => i + off)) { if (i > last) out.push(label.slice(last, i)); out.push(h('mark', { text: label[i] })); last = i + 1; }
    out.push(label.slice(last));
    return out;
  }
  function paintMenu() {
    const q = slashQuery(input.value);
    if (q == null) return closeMenu();
    const groups = grouped(items, q);
    rows = groups.flatMap(g => g.rows);
    if (sel >= rows.length) sel = 0;
    const parts = [];
    let n = 0;
    for (const g of groups) {
      parts.push(h('div', { class: 'pm-grp', role: 'presentation', text: gname(g.group) }));
      for (const r of g.rows) {
        const k = n++, it = r.it;
        parts.push(h('div', { class: 'pm-row', role: 'option', id: 'pm-' + k, 'aria-selected': String(k === sel), 'data-group': it.group, onmousedown: e => { e.preventDefault(); choose(k, false); }, onmousemove: () => { if (sel !== k) { sel = k; mark(); } } },
          h('span', { class: 'pm-name' }, ...hl(it.label, r.idx)),
          it.args ? h('span', { class: 'pm-args', text: it.args }) : null,
          h('span', { class: 'pm-desc', text: it.desc }),
          it.approval ? h('span', { class: 'pm-ask', text: U.approval }) : null,
          h('kbd', { class: 'pm-kbd', text: it.kind === 'run' ? '↵ run' : it.kind === 'fill' ? '↵ fill' : '↵ open' })));
      }
    }
    if (!rows.length) parts.push(h('p', { class: 'pm-none', text: U.none }));
    menu.replaceChildren(...parts, foot);
    menu.hidden = false;
    open = true;
    input.setAttribute('aria-expanded', 'true');
    mark();
  }
  function mark() {
    menu.querySelectorAll('.pm-row').forEach((r, k) => {
      r.setAttribute('aria-selected', String(k === sel));
      if (k === sel) { r.scrollIntoView({ block: 'nearest' }); input.setAttribute('aria-activedescendant', r.id); }
    });
    ghost();
  }
  function closeMenu() { menu.hidden = true; open = false; sel = 0; input.removeAttribute('aria-expanded'); input.removeAttribute('aria-activedescendant'); }

  // Enter (tab=false) runs or fills; Tab (tab=true) always just fills.
  function choose(k, tab) {
    const it = rows[k]?.it;
    if (!it) return;
    closeMenu();
    if (it.kind === 'guide') { input.value = ''; openGuide(it.guide); return; }
    if (it.kind === 'keys') { input.value = ''; openKeys(); return; }
    if (it.kind === 'run' && !tab) { input.value = it.text; ctx.send(); return; }
    setText(it.fill);
  }

  // ---- ghost text: the rest of a slash command, grey, after what you typed
  const ghostEl = h('div', { class: 'pm-ghost', 'aria-hidden': 'true' });
  host.append(ghostEl);
  const names = () => items.filter(i => i.group === 'command' || i.group === 'custom').map(i => i.search ?? i.label);
  let ghostText = '';
  function ghost() {
    const t = input.value;
    let g = ghostFor(t, names());
    const cur = open ? rows[sel]?.it : null;
    if (cur && t.length > 1 && (cur.group === 'command' || cur.group === 'custom')) {
      const n = cur.search ?? cur.label;
      if (n.toLowerCase().startsWith(t.toLowerCase()) && n.length > t.length) g = n.slice(t.length);
    }
    ghostText = g;
    if (!g) { ghostEl.hidden = true; return; }
    const cs = getComputedStyle(input);
    Object.assign(ghostEl.style, { left: input.offsetLeft + 'px', top: input.offsetTop + 'px', width: input.offsetWidth + 'px', height: input.offsetHeight + 'px',
      font: cs.font, letterSpacing: cs.letterSpacing, padding: cs.padding, borderWidth: cs.borderWidth, borderRadius: cs.borderRadius });
    ghostEl.replaceChildren(h('span', { text: t }), h('span', { class: 'pm-g', text: g }), h('span', { class: 'pm-gk', text: U.ghostHint }));
    ghostEl.hidden = false;
  }
  function update() {
    if (slashQuery(input.value) == null) { closeMenu(); ghost(); return; }
    if (!open) sel = 0;
    if (Date.now() - loaded > 30000) load();
    paintMenu();
  }

  // Runs before chat.mjs's own handler (capture), and stops it only for the keys it takes.
  input.addEventListener('keydown', e => {
    if (e.isComposing) return;
    const take = () => { e.preventDefault(); e.stopImmediatePropagation(); };
    if (open && rows.length) {
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') { take(); sel = (sel + (e.key === 'ArrowDown' ? 1 : -1) + rows.length) % rows.length; return mark(); }
      if (e.key === 'Enter' && !e.shiftKey) { take(); return choose(sel, false); }
      if (e.key === 'Tab' && !e.shiftKey) { take(); return choose(sel, true); }
    }
    if (open && e.key === 'Escape') { take(); closeMenu(); return ghost(); }
    if (e.key === 'Tab' && !e.shiftKey && !open && ghostText) { take(); input.value += ghostText + ' '; return update(); }
    if (e.key === 'Tab' && blanks(input.value).length) {
      const b = nextBlank(input.value, e.shiftKey ? input.selectionStart : input.selectionEnd, e.shiftKey ? -1 : 1);
      if (b) { take(); input.setSelectionRange(b.start, b.end); }
      return;
    }
    if (e.key === 'Enter' && !e.shiftKey) {
      const m = /^\/(guide|keys)(?:\s+(\S+))?\s*$/.exec(input.value.trim());
      if (m) { take(); input.value = ''; update(); return m[1] === 'keys' ? openKeys() : openGuide(m[2]); }
    }
  }, true);
  input.addEventListener('input', update);
  input.addEventListener('blur', () => setTimeout(() => { if (document.activeElement !== input && !menu.contains(document.activeElement)) { closeMenu(); } }, 120));
  input.setAttribute('role', 'combobox');
  input.setAttribute('aria-controls', 'pm-list');
  input.setAttribute('aria-autocomplete', 'list');
  menu.id = 'pm-list';

  // ---- suggestion cards in an empty chat
  const cards = h('section', { class: 'pm-cards', 'aria-label': U.cardsTitle });
  let rot = Number(mem.get('cards', '0')) || 0, shown = false;
  function paintCards() {
    const set = pickCards(P, cat, health, rot * 7919 + 13);
    if (!set.length) { cards.replaceChildren(); return; }
    cards.replaceChildren(
      h('div', { class: 'pm-cards-hd' }, h('span', { class: 'tt-lbl', text: U.cardsTitle }), h('button', { class: 'pm-link', type: 'button', text: U.more, onclick: () => { rot++; mem.set('cards', String(rot)); paintCards(); } })),
      h('p', { class: 'pm-lead', text: U.cardsLead }),
      h('div', { class: 'pm-grid' }, ...set.map(s => h('button', { class: 'pm-card', type: 'button', onclick: () => setText(s.prompt) },
        h('b', { text: s.title }), h('span', { text: s.why })))));
  }
  function syncCards() {
    if (empty.isConnected && !cards.isConnected) { if (!shown) { shown = true; rot++; mem.set('cards', String(rot)); } empty.after(cards); paintCards(); }
    else if (!empty.isConnected && cards.isConnected) { cards.remove(); shown = false; }
  }

  // ---- "What does this mean?" under an error
  function decorate() {
    for (const e of log.querySelectorAll('.ch-err:not([data-pm]), .ch-tool[data-state=err]:not([data-pm])')) {
      e.dataset.pm = '1';
      const text = e.textContent || '';
      e.append(h('button', { class: 'pm-what', type: 'button', text: U.whatMeans, onclick: ev => { ev.preventDefault(); openGuide(guideForError(text, P)); } }));
    }
  }
  const mo = new MutationObserver(() => { syncCards(); decorate(); });
  mo.observe(log, { childList: true, subtree: true, attributes: true, attributeFilter: ['data-state'] });

  // ---- the Guide panel and the keyboard map
  let layer = null, back = null;
  const closeLayer = () => { layer?.remove(); layer = null; if (back) { back.focus?.(); back = null; } };
  function show(kind, ...kids) {
    back = back || document.activeElement;
    layer?.remove();
    layer = h('div', { class: 'pg', 'data-kind': kind, onmousedown: e => { if (e.target === layer) closeLayer(); } },
      h('div', { class: 'pg-box', role: 'dialog', 'aria-modal': 'true', 'aria-label': kind === 'keys' ? U.keysTitle : U.guideTitle, tabindex: '-1' }, ...kids));
    root.append(layer);
    layer.querySelector('.pg-box').focus();
  }
  const fillFrom = t => { closeLayer(); setText(t); };
  function openGuide(id) {
    const g = (P.guides || []).find(x => x.id === id || x.label.toLowerCase() === String(id).toLowerCase()) || null;
    const list = h('nav', { class: 'pg-nav', 'aria-label': U.guideTitle }, ...P.guides.map((x, i) =>
      h('button', { type: 'button', 'aria-current': String(!!g && x.id === g.id), onclick: () => openGuide(x.id) }, h('i', { text: String(i + 1).padStart(2, '0') }), x.label)));
    const body = g
      ? h('article', { class: 'pg-body' },
          h('span', { class: 'tt-lbl', text: U.guideTitle }),
          h('h2', { text: g.title }),
          h('p', { class: 'pg-lead', text: g.lead }),
          h('ul', {}, ...g.body.map(t => h('li', { text: t }))),
          h('span', { class: 'tt-lbl', text: U.guideTry }),
          h('div', { class: 'pg-try' }, ...g.examples.map(x => h('button', { type: 'button', onclick: () => fillFrom(x), text: x }))))
      : h('article', { class: 'pg-body' },
          h('span', { class: 'tt-lbl', text: '02 — ' + U.guideTitle }),
          h('h2', { text: U.guideLead }),
          h('div', { class: 'pg-all' }, ...P.guides.map(x => h('button', { type: 'button', onclick: () => openGuide(x.id) }, h('b', { text: x.label }), h('span', { text: x.lead })))));
    show('guide', h('button', { class: 'pg-x', type: 'button', 'aria-label': U.guideClose, onclick: closeLayer, text: '×' }), list, body);
  }
  function openKeys() {
    show('keys',
      h('button', { class: 'pg-x', type: 'button', 'aria-label': U.guideClose, onclick: closeLayer, text: '×' }),
      h('article', { class: 'pg-body' },
        h('span', { class: 'tt-lbl', text: U.keysTitle }),
        h('h2', { text: U.keysLead }),
        h('dl', { class: 'pg-keys' }, ...P.keys.flatMap(([k, d]) => [h('dt', {}, ...k.split('  ').map(x => h('kbd', { text: x }))), h('dd', { text: d })])),
        h('button', { class: 'pm-link', type: 'button', text: U.keysGuides + ' →', onclick: () => openGuide() })));
  }

  // G and ? from anywhere that is not a text box; Esc closes whatever is open
  const onKey = e => {
    if (e.key === 'Escape' && layer) { e.preventDefault(); e.stopPropagation(); return closeLayer(); }
    if (e.ctrlKey || e.metaKey || e.altKey || e.defaultPrevented) return;
    if (e.target?.closest?.('input, textarea, select, [contenteditable]')) return;
    if (e.key === 'g' || e.key === 'G') { e.preventDefault(); layer ? closeLayer() : openGuide(); }
    else if (e.key === '?') { e.preventDefault(); layer ? closeLayer() : openKeys(); }
  };
  document.addEventListener('keydown', onKey, true);
  const top = root.querySelector('.ch-top');
  const btns = h('span', { class: 'pm-top' },
    h('button', { class: 'pm-tb', type: 'button', title: U.guideTitle, onclick: () => openGuide() }, h('kbd', { text: 'G' }), 'Guide'),
    h('button', { class: 'pm-tb', type: 'button', title: U.keysTitle, onclick: () => openKeys() }, h('kbd', { text: '?' }), 'Keys'));
  top?.append(btns);
  input.setAttribute('aria-description', U.slashHint);

  syncCards();
  load();
  const t = setInterval(load, 60000);
  return {
    openGuide, openKeys,
    detach() { clearInterval(t); mo.disconnect(); document.removeEventListener('keydown', onKey, true); menu.remove(); ghostEl.remove(); cards.remove(); btns.remove(); closeLayer(); css.remove(); },
  };
}
