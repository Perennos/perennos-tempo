// Perennos Tempo Room: the app. No framework, no build step, no inline script or style, and no
// innerHTML with anything a person typed: every message is built from text nodes.
const $ = (id) => document.getElementById(id);
const el = (tag, cls, text) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
};
const enc = encodeURIComponent;
const load = (k) => { try { return localStorage.getItem('tempo-room:' + k); } catch { return null; } };
const save = (k, v) => { try { localStorage.setItem('tempo-room:' + k, v); } catch { /* private mode */ } };
const wide = () => window.matchMedia('(min-width: 860px)').matches;
const coarse = () => window.matchMedia('(pointer: coarse)').matches;

const S = {
  me: null,
  rooms: [],
  room: null,
  msgs: new Map(),
  people: new Map(),
  seq: 0,
  es: null,
  replyTo: null,
  unread: new Map(),
  thinking: new Map(),
  thinkers: new Map(),
  sugg: { items: [], i: 0, at: -1 },
  notes: 0,
};

async function api(path, body) {
  const opt = { credentials: 'same-origin' };
  if (body !== undefined) {
    opt.method = 'POST';
    opt.headers = { 'Content-Type': 'application/json' };
    opt.body = JSON.stringify(body);
  }
  const r = await fetch('/room/' + path, opt);
  let j = {};
  try { j = await r.json(); } catch { /* not JSON */ }
  if (!r.ok) {
    const e = new Error(j.error || 'Something went wrong (' + r.status + ').');
    e.status = r.status;
    throw e;
  }
  return j;
}

// ---------------------------------------------------------------- small helpers

let toastTimer = 0;
function toast(text, ms = 4200) {
  const t = $('toast');
  t.textContent = text;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.hidden = true; }, ms);
}

const pad2 = (n) => String(n).padStart(2, '0');
function clock(t) {
  const d = new Date(t * 1000);
  return pad2(d.getHours()) + ':' + pad2(d.getMinutes());
}
function dayLabel(t) {
  const d = new Date(t * 1000);
  const today = new Date();
  const y = new Date(); y.setDate(today.getDate() - 1);
  if (d.toDateString() === today.toDateString()) return 'Today';
  if (d.toDateString() === y.toDateString()) return 'Yesterday';
  return d.toLocaleDateString(undefined, { weekday: 'short', day: 'numeric', month: 'short' });
}
function rel(t) {
  const s = Date.now() / 1000 - t;
  if (s < 60) return 'now';
  if (s < 3600) return Math.floor(s / 60) + ' min';
  const d = new Date(t * 1000);
  if (d.toDateString() === new Date().toDateString()) return clock(t);
  if (s < 6 * 86400) return d.toLocaleDateString(undefined, { weekday: 'short' });
  return d.toLocaleDateString(undefined, { day: 'numeric', month: 'short' });
}
function roomOf(id) { return S.rooms.find((r) => r.id === id); }
function isMine(m) {
  if (!S.me || m.kind !== 'user') return false;
  return S.me.role === 'owner' ? m.from === 'owner' : m.from === S.me.id;
}
async function copy(text, input) {
  try {
    await navigator.clipboard.writeText(text);
    toast('Copied.');
  } catch {
    if (input) { input.focus(); input.select(); }
    toast('Selected: copy it with your keyboard or a long press.');
  }
}

// ---------------------------------------------------------------- theme and greeting

const THEMES = ['charcoal', 'ink', 'paper'];
function applyTheme(t) {
  if (!THEMES.includes(t)) t = 'charcoal';
  document.documentElement.dataset.theme = t;
  const meta = document.querySelector('meta[name=theme-color]');
  if (meta) meta.content = t === 'paper' ? '#efe9dc' : t === 'ink' ? '#070708' : '#1b1a1b';
  $('theme').firstElementChild.textContent = t;
  save('theme', t);
}

function greet() {
  const h = new Date().getHours();
  const n = S.me.name || 'friend';
  const box = document.querySelector('.hello');
  let g;
  let sub;
  box.classList.toggle('birthday', !!S.me.birthday);
  if (S.me.birthday) {
    g = S.me.role === 'owner' ? 'Happy birthday, ' + n + '.' : 'It is ' + S.me.owner_name + "'s birthday today.";
    sub = S.me.role === 'owner' ? 'Tempo knows. Your first answer from it today comes with a toast.' : 'Say something nice in the room.';
  } else if (h < 5 || h >= 22) {
    g = 'Still up, ' + n + '?';
    sub = 'The room keeps you company. Tempo answers when you write @tempo.';
  } else if (h < 12) {
    g = 'Good morning, ' + n + '.';
    sub = 'Coffee first. Then mention @tempo for anything.';
  } else if (h < 17) {
    g = 'Good afternoon, ' + n + '.';
    sub = 'Ask @tempo for a tl;dr if you missed the morning.';
  } else {
    g = 'Good evening, ' + n + '.';
    sub = 'Mention @tempo in any message and it answers in the thread.';
  }
  $('greeting').textContent = g;
  $('greeting-sub').textContent = sub;
  $('today').textContent = new Date().toLocaleDateString(undefined, { weekday: 'long', day: 'numeric', month: 'long' });
}

function agentState(state) {
  const dot = $('agent-dot');
  dot.classList.toggle('up', state === 'up');
  dot.classList.toggle('down', state === 'down');
  $('agent-state').textContent = state === 'up' ? 'tempo is awake' : state === 'down' ? 'tempo is resting' : 'tempo';
  hintComposer(state);
}

function hintComposer(state) {
  const h = $('composer-hint');
  if (state === 'down') h.textContent = "Tempo's agent is resting right now; the room still works.";
  else h.textContent = coarse() ? 'Write @tempo to ask Perennos Tempo.' : 'Enter sends · Shift+Enter for a new line · @tempo to ask Perennos Tempo';
}

// ---------------------------------------------------------------- start

function parseHash() {
  const h = location.hash.slice(1);
  const out = {};
  for (const kv of h.split('&')) {
    const [k, v] = kv.split('=');
    if (k && v) out[k] = decodeURIComponent(v);
  }
  return out;
}

async function refreshMe() {
  S.me = await api('me');
  $('who').textContent = S.me.role === 'owner' ? S.me.name + ' · owner' : S.me.role === 'member' ? S.me.name : '';
  $('logout').hidden = S.me.role === 'guest';
  $('new-room').hidden = S.me.role !== 'owner';
  agentState(S.me.agent);
}

async function boot() {
  applyTheme(load('theme') || 'charcoal');
  wire();
  const h = parseHash();
  if (h.owner) {
    try { await api('owner', { token: h.owner }); } catch (e) { toast(e.message, 8000); }
    history.replaceState(null, '', '/');
  }
  if (h.signin) {
    // A sign-in link from the owner: this device becomes the same person, no name needed.
    history.replaceState(null, '', '/');
    try {
      const j = await api('join', { code: h.signin });
      if (j.room) save('room', j.room);
      toast('This device is signed in as ' + j.name + '.');
    } catch (e) { toast(e.message, 8000); }
  }
  await refreshMe();
  if (S.me.role === 'guest') return showJoin(h.join || '');
  if (h.join) {
    history.replaceState(null, '', '/');
    try {
      const j = await api('join', { code: h.join, name: S.me.name });
      await refreshMe();
      save('room', j.room);
      toast('You are in #' + j.room + ' now.');
    } catch (e) { toast(e.message, 8000); }
  }
  greet();
  await loadRooms();
  connect();
  const want = load('room');
  if (want && roomOf(want) && wide()) await openRoom(want, false);
  setInterval(tick, 1000);
  setInterval(() => { greet(); renderRooms(); }, 30000);
  setInterval(health, 45000);
}

function showJoin(code) {
  document.body.classList.add('guest');
  $('join').hidden = false;
  $('chat').hidden = true;
  $('join-code').value = code;
  $('owner-hint').hidden = !S.me.loopback;
  (code ? $('join-name') : $('join-code')).focus();
}

async function join(e) {
  e.preventDefault();
  const code = $('join-code').value.trim();
  const name = $('join-name').value.trim();
  if (!code) { $('join-err').textContent = 'Please type the code you were given.'; return; }
  try {
    // A sign-in code from the owner needs no name; an invite does (the server says so).
    const j = await api('join', name ? { code, name } : { code });
    if (j.room) save('room', j.room);
    history.replaceState(null, '', '/');
    location.reload();
  } catch (err) { $('join-err').textContent = err.message; }
}

async function health() {
  try {
    const h = await api('health');
    agentState(h.agent);
  } catch { agentState('unknown'); }
}

// ---------------------------------------------------------------- rooms

async function loadRooms() {
  const j = await api('rooms');
  S.rooms = j.rooms;
  if (!S.seq) S.seq = j.seq || 0;
  for (const r of S.rooms) {
    if (r.thinking && !S.thinking.has(r.id)) S.thinking.set(r.id, Date.now());
    if (r.thinking) S.thinkers.set(r.id, r.thinking_who || []);
  }
  renderRooms();
  if (S.room) {
    const r = roomOf(S.room);
    if (r) roomHeader(r); else closeRoom();
  } else {
    $('empty-title').textContent = S.rooms.length ? 'Pick a room.' : S.me.role === 'owner' ? 'Make your first room.' : 'No rooms yet.';
  }
}

let roomsSoon = 0;
function renderRoomsSoon() {
  clearTimeout(roomsSoon);
  roomsSoon = setTimeout(renderRooms, 60);
}
let refreshSoon = 0;
function loadRoomsSoon() {
  clearTimeout(refreshSoon);
  refreshSoon = setTimeout(() => { loadRooms().catch(() => {}); }, 120);
}

function renderRooms() {
  const box = $('rooms');
  box.replaceChildren();
  if (!S.rooms.length) {
    box.append(el('p', 'empty', S.me.role === 'owner'
      ? 'No rooms yet. Make the first one: family, team, book club.'
      : 'You are not in any room yet. Ask for an invite.'));
    return;
  }
  const sorted = [...S.rooms].sort((a, b) => (b.last ? b.last.t : 0) - (a.last ? a.last.t : 0));
  for (const r of sorted) {
    const b = el('button', 'room');
    b.type = 'button';
    b.setAttribute('role', 'listitem');
    if (S.room === r.id) b.setAttribute('aria-current', 'true');
    b.append(el('b', null, r.name), el('span', 'when', r.last ? rel(r.last.t) : ''));
    let prev = 'Quiet so far.';
    if (r.last) {
      const who = r.last.kind === 'tempo' ? 'Tempo: ' : r.last.kind === 'system' ? '' : r.last.name + ': ';
      prev = who + r.last.text.replace(/\s+/g, ' ');
    }
    b.append(el('span', 'prev', prev));
    const tags = el('span', 'tags');
    tags.append(el('span', 'lbl', r.mode === 'always' ? 'answers all' : '@tempo'), el('span', 'lbl', r.personality));
    if (S.thinking.has(r.id)) tags.append(el('span', 'lbl think', 'thinking'));
    const u = S.unread.get(r.id);
    if (u) tags.append(el('span', 'unread', String(u)));
    b.append(tags);
    b.addEventListener('click', () => openRoom(r.id, true));
    box.append(b);
  }
}

// The teammates in a room: name and kind when the people list is loaded, else just the handle.
function teamHere(r) {
  const known = (S.people.get(r.id) || {}).team || [];
  return (r.team || []).map((h) => known.find((t) => t.handle === h) || { handle: h, name: h, kind: 'agent' });
}

function roomHeader(r) {
  $('chat-name').textContent = r.name;
  const people = r.members + 1;
  const team = teamHere(r);
  const dm = r.mode === 'always' && team.length === 1;
  const how = dm
    ? 'Chat with ' + team[0].name
    : r.mode === 'always' ? (wide() ? 'Tempo answers every message' : 'answers all') : (wide() ? 'Mention @tempo to ask' : '@tempo to ask');
  const tail = !dm && team.length ? ' · with ' + team.map((t) => t.name).join(', ') : '';
  $('chat-meta').textContent = how + ' · ' + (dm ? '' : r.personality + ' · ') + people + (people === 1 ? ' person' : ' people') + tail;
  $('room-title').textContent = '#' + r.id;
  $('room-sep').hidden = false;
  $('room-crumb').hidden = false;
  $('text').placeholder = 'Message #' + r.id;
}

async function openRoom(id, push) {
  const r = roomOf(id);
  if (!r) return;
  if (S.room && S.room !== id) saveDraft();
  S.room = id;
  save('room', id);
  S.unread.delete(id);
  S.replyTo = null;
  updateReply();
  document.body.classList.add('in-room');
  $('chat').hidden = false;
  $('back').hidden = false;
  $('invite').hidden = S.me.role !== 'owner';
  roomHeader(r);
  if (push && !wide()) history.pushState({ room: id }, '');
  $('text').value = load('draft:' + id) || '';
  autosize();
  const j = await api('messages?room=' + enc(id) + '&limit=60');
  S.msgs.set(id, j.messages);
  if (j.thinking) {
    if (!S.thinking.has(id)) S.thinking.set(id, Date.now());
    S.thinkers.set(id, j.thinking_who || []);
  } else { S.thinking.delete(id); S.thinkers.delete(id); }
  renderMsgs(true);
  renderThinking();
  renderRooms();
  updateTitle();
  loadPeople(id);
  if (!coarse()) $('text').focus();
}

function closeRoom() {
  saveDraft();
  S.room = null;
  save('room', '');
  document.body.classList.remove('in-room');
  $('chat').hidden = true;
  $('back').hidden = true;
  $('room-sep').hidden = true;
  $('room-crumb').hidden = true;
  closeSuggest();
  renderRooms();
}

async function loadPeople(id) {
  try {
    const j = await api('members?room=' + enc(id));
    S.people.set(id, j);
    const r = roomOf(id);
    if (r && S.room === id) roomHeader(r);
  } catch { /* the list is a convenience */ }
}

// ---------------------------------------------------------------- messages

const URL_RE = /\bhttps?:\/\/[^\s<>"'`]+[^\s<>"'`.,;:!?)\]]/g;
const AT_RE = /(^|[^\p{L}\p{N}_@.])(@[\p{L}\p{N}_]+)/gu;

function addMentions(node, s) {
  let last = 0;
  for (const m of s.matchAll(AT_RE)) {
    const at = m.index + m[1].length;
    node.append(document.createTextNode(s.slice(last, at)));
    node.append(el('span', 'at', m[2]));
    last = at + m[2].length;
  }
  node.append(document.createTextNode(s.slice(last)));
}

function fillText(node, text) {
  let last = 0;
  for (const m of text.matchAll(URL_RE)) {
    addMentions(node, text.slice(last, m.index));
    const a = el('a', null, m[0]);
    a.href = m[0];
    a.target = '_blank';
    a.rel = 'noopener noreferrer nofollow';
    node.append(a);
    last = m.index + m[0].length;
  }
  addMentions(node, text.slice(last));
}

function msgNode(m, prev) {
  const cls = ['msg'];
  if (m.kind === 'tempo') cls.push('tempo');
  if (m.kind === 'teammate') cls.push('team');
  if (m.kind === 'system') cls.push('system');
  if (isMine(m)) cls.push('mine');
  if (m.local) cls.push('local');
  const cont = prev && prev.kind !== 'system' && m.kind !== 'system' && prev.from === m.from && m.t - prev.t < 300 && !m.reply_name && !m.local;
  if (cont) cls.push('cont');
  const d = el('div', cls.join(' '));
  d.dataset.id = String(m.id);
  if (m.kind === 'system') {
    d.textContent = m.text;
    return d;
  }
  const hd = el('div', 'hd');
  if (m.kind === 'tempo') hd.append(el('span', 'mark', 'tempo'));
  else if (m.kind === 'teammate') {
    // a teammate is marked with its own name and handle, never with Tempo's mark
    hd.append(el('span', 'who team', m.name), el('span', 'via', '@' + String(m.from).replace(/^team:/, '')));
  } else hd.append(el('span', 'who', m.name));
  if (m.via === 'discord' || m.via === 'telegram' || m.via === 'whatsapp') hd.append(el('span', 'via', m.via));
  if (m.via === 'mcp' && m.kind === 'tempo') hd.append(el('span', 'via', 'via agent'));
  if (m.local) hd.append(el('span', 'via', 'only you'));
  const time = el('time', 'time', clock(m.t));
  time.title = new Date(m.t * 1000).toLocaleString();
  hd.append(time);
  d.append(hd);
  if (m.reply_name) {
    const q = el('button', 'quote');
    q.type = 'button';
    q.append(el('b', null, m.reply_name + ': '), document.createTextNode(m.reply_text || ''));
    q.addEventListener('click', (e) => { e.stopPropagation(); if (m.reply_to) jumpTo(m.reply_to); });
    d.append(q);
  }
  const t = el('div', 'text');
  fillText(t, m.text);
  d.append(t);
  if (!m.local) {
    const acts = el('div', 'acts');
    const reply = el('button', 'btn sm', 'Reply');
    reply.type = 'button';
    reply.addEventListener('click', (e) => { e.stopPropagation(); startReply(m); });
    const cp = el('button', 'btn sm', 'Copy');
    cp.type = 'button';
    cp.addEventListener('click', (e) => { e.stopPropagation(); copy(m.text); });
    acts.append(reply, cp);
    if (m.kind === 'user' && !isMine(m)) {
      const check = el('button', 'btn sm', 'Is this true?');
      check.type = 'button';
      check.title = 'Ask Tempo to fact-check this message';
      check.addEventListener('click', (e) => { e.stopPropagation(); startReply(m); $('text').value = '@tempo is this true?'; autosize(); });
      acts.append(check);
    }
    d.append(acts);
    d.addEventListener('click', (e) => {
      if (e.target.closest('a,button') || getSelection().toString()) return;
      const open = d.classList.contains('open');
      for (const o of document.querySelectorAll('.msg.open')) o.classList.remove('open');
      if (!open) d.classList.add('open');
    });
  }
  return d;
}

function emptyRoom() {
  const p = el('p', 'empty first');
  p.textContent = 'Quiet in here. Say hello, or ask @tempo anything: a recipe, a fact, a tl;dr.';
  return p;
}

const nearEnd = (box) => box.scrollHeight - box.scrollTop - box.clientHeight < 80;

function renderMsgs(stick) {
  const box = $('msgs');
  const list = S.msgs.get(S.room) || [];
  box.replaceChildren();
  if (list.length && list[0].id > 1) {
    const more = el('button', 'btn sm more', 'Earlier messages');
    more.type = 'button';
    more.addEventListener('click', loadEarlier);
    box.append(more);
  }
  if (!list.length) box.append(emptyRoom());
  let prev = null;
  let prevDay = '';
  for (const m of list) {
    const day = dayLabel(m.t);
    if (day !== prevDay) { box.append(el('div', 'day', day)); prevDay = day; prev = null; }
    box.append(msgNode(m, prev));
    prev = m;
  }
  if (box.firstElementChild) box.firstElementChild.classList.add('first');
  if (stick) box.scrollTop = box.scrollHeight;
  $('jump').hidden = true;
}

async function loadEarlier() {
  const list = S.msgs.get(S.room) || [];
  if (!list.length) return;
  const box = $('msgs');
  const before = box.scrollHeight - box.scrollTop;
  const j = await api('messages?room=' + enc(S.room) + '&before=' + list[0].id + '&limit=60');
  S.msgs.set(S.room, j.messages.concat(list));
  renderMsgs(false);
  box.scrollTop = box.scrollHeight - before;
}

function addMsg(m) {
  const list = S.msgs.get(m.room) || [];
  if (list.some((x) => x.id === m.id)) return;
  list.push(m);
  S.msgs.set(m.room, list);
  if (m.room !== S.room) return;
  const box = $('msgs');
  const end = nearEnd(box);
  const placeholder = box.querySelector('p.empty');
  if (placeholder) placeholder.remove();
  const prev = list.length > 1 ? list[list.length - 2] : null;
  const sameDay = prev && dayLabel(prev.t) === dayLabel(m.t);
  if (!sameDay) box.append(el('div', 'day', dayLabel(m.t)));
  box.append(msgNode(m, sameDay ? prev : null));
  if (end || isMine(m)) box.scrollTop = box.scrollHeight;
  else $('jump').hidden = false;
}

/// A message only this person sees (a /command's answer, the tap tempo).
function localNote(text) {
  if (!S.room) { toast(text, 8000); return; }
  S.notes += 1;
  const m = { id: 'local-' + S.notes, t: Date.now() / 1000, room: S.room, from: 'tempo', name: 'Tempo', kind: 'tempo', via: 'app', text, local: true };
  const box = $('msgs');
  box.append(msgNode(m, null));
  box.scrollTop = box.scrollHeight;
}

function jumpTo(id) {
  const n = $('msgs').querySelector('.msg[data-id="' + id + '"]');
  if (!n) { toast('That message is further back.'); return; }
  n.scrollIntoView({ block: 'center', behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' });
  n.classList.remove('flash');
  void n.offsetWidth;
  n.classList.add('flash');
}

// ---------------------------------------------------------------- live

function live(on) {
  $('livebox').classList.toggle('off', !on);
  $('livestate').textContent = on ? 'LIVE' : 'RECONNECTING';
  $('st-mid').textContent = on ? 'live' : 'reconnecting';
}

function connect() {
  if (S.es) S.es.close();
  const es = new EventSource('/room/events?since=' + S.seq);
  es.onopen = () => live(true);
  es.onerror = () => live(false);
  es.addEventListener('message', (e) => {
    S.seq = Math.max(S.seq, Number(e.lastEventId) || 0);
    const d = JSON.parse(e.data);
    onMessage(d.room, d.msg);
  });
  es.addEventListener('typing', (e) => {
    S.seq = Math.max(S.seq, Number(e.lastEventId) || 0);
    const d = JSON.parse(e.data);
    const who = S.thinkers.get(d.room) || [];
    if (d.on) {
      if (!S.thinking.has(d.room)) S.thinking.set(d.room, Date.now());
      if (d.who && !who.includes(d.who)) who.push(d.who);
      S.thinkers.set(d.room, who);
    } else {
      const left = who.filter((n) => n !== d.who);
      S.thinkers.set(d.room, left);
      if (!left.length) { S.thinking.delete(d.room); S.thinkers.delete(d.room); }
    }
    renderThinking();
    renderRoomsSoon();
  });
  es.addEventListener('room', (e) => {
    S.seq = Math.max(S.seq, Number(e.lastEventId) || 0);
    loadRoomsSoon();
    if (S.room) loadPeople(S.room);
  });
  es.addEventListener('reset', () => {
    loadRoomsSoon();
    if (S.room) openRoom(S.room, false);
  });
  S.es = es;
}

function onMessage(room, m) {
  const r = roomOf(room);
  if (!r) { loadRoomsSoon(); return; }
  r.last = m;
  r.count = Math.max(r.count, m.id);
  addMsg(m);
  const away = room !== S.room || document.hidden;
  if (away && !isMine(m) && m.kind !== 'system') S.unread.set(room, (S.unread.get(room) || 0) + 1);
  renderRoomsSoon();
  updateTitle();
}

function updateTitle() {
  let n = 0;
  for (const v of S.unread.values()) n += v;
  document.title = (n ? '(' + n + ') ' : '') + 'Perennos Tempo Room';
}

function renderThinking() {
  const since = S.room ? S.thinking.get(S.room) : 0;
  $('thinking').hidden = !since;
  if (since) {
    const s = Math.floor((Date.now() - since) / 1000);
    const names = S.thinkers.get(S.room) || [];
    // Tempo keeps its mark; teammates are named
    const only = names.length === 0 || (names.length === 1 && names[0] === 'Tempo');
    $('thinking-mark').textContent = only ? 'tempo' : names.map((n) => (n === 'Tempo' ? 'tempo' : n)).join(', ');
    const verb = names.length > 1 ? 'are thinking' : 'is thinking';
    $('thinking-text').textContent = s >= 10 ? verb + ' · ' + s + ' s' : verb;
  }
}

function tick() { renderThinking(); }

// ---------------------------------------------------------------- composer

function autosize() {
  const t = $('text');
  t.style.height = 'auto';
  t.style.height = Math.min(t.scrollHeight + 2, 160) + 'px';
}

let draftTimer = 0;
function saveDraft() {
  if (!S.room) return;
  save('draft:' + S.room, $('text').value);
}

function startReply(m) {
  S.replyTo = m;
  updateReply();
  for (const o of document.querySelectorAll('.msg.open')) o.classList.remove('open');
  $('text').focus();
}

function updateReply() {
  const r = S.replyTo;
  $('replying').hidden = !r;
  if (r) $('replying-text').textContent = (r.kind === 'tempo' ? 'Tempo' : r.name) + ': ' + r.text.replace(/\s+/g, ' ');
}

async function send() {
  const ta = $('text');
  const text = ta.value.trim();
  if (!text || !S.room) return;
  if (/^\/(bpm|tap)\b/i.test(text)) {
    ta.value = '';
    saveDraft();
    autosize();
    openTap();
    return;
  }
  const body = { room: S.room, text };
  if (S.replyTo && typeof S.replyTo.id === 'number') body.reply_to = S.replyTo.id;
  $('send').disabled = true;
  try {
    const j = await api('post', body);
    ta.value = '';
    saveDraft();
    S.replyTo = null;
    updateReply();
    autosize();
    if (j.msg) onMessage(j.msg.room, j.msg);
    if (j.note) localNote(j.note);
  } catch (e) {
    toast(e.message, 6000);
  } finally {
    $('send').disabled = false;
    if (!coarse()) ta.focus();
  }
}

// @ suggestions: tempo first, then the people in the room.
function suggestions(q) {
  const p = S.people.get(S.room);
  const names = ['tempo'];
  if (p) {
    for (const t of p.team || []) names.push(t.handle);
    if (p.owner_name) names.push(p.owner_name);
    for (const m of p.members) names.push(m.name);
  }
  const ql = q.toLowerCase();
  return [...new Set(names)].filter((n) => n.toLowerCase().startsWith(ql) && !(S.me && n === S.me.name && n !== 'tempo')).slice(0, 8);
}

function onInput() {
  autosize();
  clearTimeout(draftTimer);
  draftTimer = setTimeout(saveDraft, 400);
  const t = $('text');
  const before = t.value.slice(0, t.selectionStart);
  const m = before.match(/(^|\s)@([\p{L}\p{N}_]*)$/u);
  if (!m) { closeSuggest(); return; }
  const items = suggestions(m[2]);
  if (!items.length) { closeSuggest(); return; }
  S.sugg = { items, i: 0, at: before.length - m[2].length - 1 };
  renderSuggest();
}

function renderSuggest() {
  const box = $('suggest');
  box.replaceChildren();
  S.sugg.items.forEach((n, i) => {
    const b = el('button', 'opt');
    b.type = 'button';
    b.setAttribute('role', 'option');
    b.setAttribute('aria-selected', String(i === S.sugg.i));
    if (n === 'tempo') {
      b.append(el('span', 'mark', 'tempo'), el('i', null, 'ask Perennos Tempo'));
    } else {
      const t = ((S.people.get(S.room) || {}).team || []).find((x) => x.handle === n);
      b.append(el('span', null, '@' + n));
      if (t) b.append(el('i', null, t.name + ' · teammate'));
    }
    b.addEventListener('mousedown', (e) => { e.preventDefault(); pick(i); });
    box.append(b);
  });
  box.hidden = false;
}

function closeSuggest() {
  $('suggest').hidden = true;
  S.sugg = { items: [], i: 0, at: -1 };
}

function pick(i) {
  const t = $('text');
  const name = S.sugg.items[i];
  if (name == null) return;
  const caret = t.selectionStart;
  const ins = '@' + name + ' ';
  t.value = t.value.slice(0, S.sugg.at) + ins + t.value.slice(caret);
  const pos = S.sugg.at + ins.length;
  t.setSelectionRange(pos, pos);
  closeSuggest();
  autosize();
  t.focus();
}

function onKey(e) {
  const open = !$('suggest').hidden;
  if (open && (e.key === 'ArrowDown' || e.key === 'ArrowUp')) {
    e.preventDefault();
    const n = S.sugg.items.length;
    S.sugg.i = (S.sugg.i + (e.key === 'ArrowDown' ? 1 : n - 1)) % n;
    renderSuggest();
    return;
  }
  if (open && (e.key === 'Enter' || e.key === 'Tab')) {
    e.preventDefault();
    pick(S.sugg.i);
    return;
  }
  if (open && e.key === 'Escape') { closeSuggest(); return; }
  if (e.key === 'Escape' && S.replyTo) { S.replyTo = null; updateReply(); return; }
  if (e.key === 'Enter' && !e.shiftKey && !e.isComposing && !coarse()) {
    e.preventDefault();
    send();
  }
}

// ---------------------------------------------------------------- sheets

function sheet(build) {
  const box = $('sheet-box');
  box.replaceChildren();
  build(box);
  $('sheet').hidden = false;
  const f = box.querySelector('button,input');
  if (f && !coarse()) f.focus();
}
function closeSheet() {
  $('sheet').hidden = true;
  $('sheet-box').replaceChildren();
}
function closeButton() {
  const b = el('button', 'btn sm', 'Close');
  b.type = 'button';
  b.addEventListener('click', closeSheet);
  return b;
}

async function openPeople() {
  const r = roomOf(S.room);
  await loadPeople(S.room);
  const p = S.people.get(S.room) || { members: [], owner_name: '', team: [] };
  const owner = S.me.role === 'owner';
  let team = [];
  if (owner) {
    try { team = (await api('team')).team; } catch (e) { toast(e.message); }
  }
  sheet((box) => {
    box.append(el('h2', null, 'People in #' + r.id));
    box.append(el('p', 'sub', p.members.length ? 'Everyone here can read the room and ask @tempo' + ((p.team || []).length ? ' or a teammate.' : '.') : 'Just you and Tempo so far.'));
    const list = el('div');
    const row = (name, tag) => {
      const d = el('div', 'person');
      d.append(el('b', null, name), el('span', 'lbl', tag));
      return d;
    };
    list.append(row(p.owner_name, 'owner'));
    const t = row('', 'Perennos Tempo');
    t.firstChild.replaceWith(el('span', 'mark', 'tempo'));
    t.firstChild.classList.add('grow');
    list.append(t);
    if (!owner) {
      for (const t of p.team || []) {
        const d = row(t.name, '@' + t.handle + ' · teammate');
        list.append(d);
      }
    }
    for (const m of p.members) {
      const d = row(m.name, m.muted ? 'muted' : 'member');
      if (owner) {
        const mute = el('button', 'btn sm', m.muted ? 'Unmute' : 'Mute');
        mute.type = 'button';
        mute.addEventListener('click', async () => {
          try { await api('mute', { room: S.room, member: m.id, muted: !m.muted }); openPeople(); } catch (e) { toast(e.message); }
        });
        const rm = el('button', 'btn sm danger', 'Remove');
        rm.type = 'button';
        rm.addEventListener('click', async () => {
          if (!confirm('Remove ' + m.name + '? They leave every room; their record is kept in trash, and they would need a new invite.')) return;
          try { await api('remove', { member: m.id }); openPeople(); } catch (e) { toast(e.message); }
        });
        const dev = el('button', 'btn sm', 'New sign-in link');
        dev.type = 'button';
        dev.title = 'Adds another device for ' + m.name;
        dev.addEventListener('click', () => openSignIn(m));
        d.append(mute, dev, rm);
      }
      list.append(d);
    }
    box.append(list);
    if (owner) {
      teamSection(box, r, team);
      box.append(el('p', 'lbl sec', "Tempo's tone here"));
      const tone = el('div', 'seg-row');
      for (const v of ['plain', 'warm', 'playful']) {
        const b = el('button', 'btn sm', v);
        b.type = 'button';
        b.setAttribute('aria-pressed', String(r.personality === v));
        b.addEventListener('click', async () => { await api('update', { room: r.id, personality: v }); r.personality = v; openPeople(); roomHeader(r); });
        tone.append(b);
      }
      box.append(tone);
      box.append(el('p', 'lbl sec', 'When Tempo answers'));
      const mode = el('div', 'seg-row');
      for (const [v, label] of [['mention', 'When someone writes @tempo'], ['always', 'Every message']]) {
        const b = el('button', 'btn sm', label);
        b.type = 'button';
        b.setAttribute('aria-pressed', String(r.mode === v));
        b.addEventListener('click', async () => { await api('update', { room: r.id, mode: v }); r.mode = v; openPeople(); roomHeader(r); });
        mode.append(b);
      }
      box.append(mode);
      box.append(el('p', 'lbl sec', 'Your name in rooms'));
      const f = el('form', 'copybox');
      const inp = el('input', 'field');
      inp.value = S.me.name;
      inp.maxLength = 32;
      inp.setAttribute('aria-label', 'Your name');
      const ok = el('button', 'btn', 'Save');
      f.append(inp, ok);
      f.addEventListener('submit', async (e) => {
        e.preventDefault();
        try { await api('settings', { owner_name: inp.value }); await refreshMe(); greet(); toast('Saved.'); } catch (err) { toast(err.message); }
      });
      box.append(f);
    }
    const end = el('div', 'row');
    end.append(closeButton());
    box.append(end);
  });
}

// ---- Team: named AI members the owner sets up (owner only)

function teamSection(box, r, team) {
  box.append(el('p', 'lbl sec', 'Team'));
  box.append(el('p', 'sub', 'Teammates are named AI members you talk to like colleagues. Write @handle in a message to ask one. They never get tools: they cannot run commands or change files.'));
  const list = el('div');
  if (!team.length) list.append(el('p', 'hint', 'No teammates yet.'));
  for (const t of team) {
    const here = (t.rooms || []).includes(r.id);
    const d = el('div', 'person');
    d.append(el('b', null, t.name), el('span', 'lbl', '@' + t.handle + ' · ' + t.kind + (t.model ? ' · ' + t.model : '')));
    const tog = el('button', 'btn sm', here ? 'In this room' : 'Not in this room');
    tog.type = 'button';
    tog.setAttribute('aria-pressed', String(here));
    tog.title = here ? 'Take ' + t.name + ' out of #' + r.id : 'Put ' + t.name + ' in #' + r.id;
    tog.addEventListener('click', async () => {
      try { await api('team/room', { room: r.id, handle: t.handle, on: !here }); await loadRooms(); openPeople(); } catch (e) { toast(e.message); }
    });
    const edit = el('button', 'btn sm', 'Edit');
    edit.type = 'button';
    edit.addEventListener('click', () => openTeamForm(t));
    const rm = el('button', 'btn sm danger', 'Remove');
    rm.type = 'button';
    rm.addEventListener('click', async () => {
      if (!confirm('Remove ' + t.name + ' (@' + t.handle + ')? It leaves every room; its record is kept in trash.')) return;
      try { await api('team/remove', { handle: t.handle }); await loadRooms(); openPeople(); } catch (e) { toast(e.message); }
    });
    d.append(tog, edit, rm);
    list.append(d);
  }
  box.append(list);
  const add = el('button', 'btn sm', 'Add a teammate');
  add.type = 'button';
  add.addEventListener('click', () => openTeamForm(null));
  const row = el('div', 'row');
  row.append(add);
  box.append(row);
}

async function openTeamForm(t) {
  let models = [];
  let note = '';
  try {
    const j = await api('team/models');
    models = (j.models || []).map((m) => m.name);
    if (j.ollama !== 'up') note = j.note || 'Ollama is not answering right now.';
  } catch (e) { note = e.message; }
  const editing = !!t;
  sheet((box) => {
    box.append(el('h2', null, editing ? 'Edit ' + t.name : 'Add a teammate'));
    box.append(el('p', 'sub', 'A teammate has a name, a role (your instruction) and a kind. Write @handle in a room to ask it.'));
    const form = el('form', 'team-form');
    const field = (label, node, hint) => {
      const l = el('label', 'lbl', label);
      l.append(node);
      form.append(l);
      if (hint) form.append(el('p', 'hint small', hint));
      return node;
    };
    const handle = el('input', 'field');
    handle.maxLength = 24;
    handle.placeholder = 'sage';
    handle.autocomplete = 'off';
    handle.spellcheck = false;
    if (editing) { handle.value = t.handle; handle.disabled = true; }
    field('Handle (lowercase letters, digits, hyphens)', handle, editing ? 'The handle cannot change. Remove the teammate and add it again for a new one.' : '');
    const name = el('input', 'field');
    name.maxLength = 32;
    name.placeholder = 'Sage';
    name.value = editing ? t.name : '';
    field('Name', name);
    const kind = el('select', 'field');
    for (const [v, label] of [['agent', 'Agent (through your Perennos Tempo agent)'], ['engine', 'Engine (one model in Ollama)']]) {
      const o = el('option', null, label);
      o.value = v;
      kind.append(o);
    }
    kind.value = editing ? t.kind : 'agent';
    field('Kind', kind);
    const kindHint = el('p', 'hint small', '');
    form.append(kindHint);
    let model;
    if (models.length) {
      model = el('select', 'field');
      const none = el('option', null, "(none: the agent's own choice)");
      none.value = '';
      model.append(none);
      const names = [...models];
      if (editing && t.model && !names.includes(t.model)) names.push(t.model);
      for (const n of names) { const o = el('option', null, n); o.value = n; model.append(o); }
      model.value = editing && t.model ? t.model : '';
    } else {
      model = el('input', 'field');
      model.placeholder = 'qwen3:4b';
      model.maxLength = 100;
      model.value = editing && t.model ? t.model : '';
    }
    field('Model (from the models installed in Ollama)', model, note ? note + ' Type the model name instead.' : '');
    const role = el('textarea', 'field');
    role.rows = 5;
    role.maxLength = 2000;
    role.placeholder = 'You are a calm cooking coach. Keep answers short.';
    role.value = editing ? t.role : '';
    field('Role (your instruction for this teammate, 2,000 characters at most)', role);
    const sync = () => {
      kindHint.textContent = kind.value === 'engine'
        ? 'An engine chats directly with the one model above in Ollama. It needs a model, and has no tools at all.'
        : "An agent answers through your Perennos Tempo agent in guest mode, like @tempo, with no tools. An older agent uses the agent's own models and ignores the model above.";
    };
    kind.addEventListener('change', sync);
    sync();
    const err = el('p', 'err');
    err.setAttribute('role', 'alert');
    const row = el('div', 'row');
    const save = el('button', 'btn go', editing ? 'Save' : 'Add');
    save.type = 'submit';
    const cancel = el('button', 'btn sm', 'Back');
    cancel.type = 'button';
    cancel.addEventListener('click', () => { closeSheet(); openPeople(); });
    row.append(save, cancel);
    form.append(err, row);
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      err.textContent = '';
      const body = { handle: handle.value.trim().toLowerCase().replace(/^@/, ''), name: name.value, kind: kind.value, model: model.value.trim(), role: role.value };
      if (!editing && S.room) body.rooms = [S.room];
      save.disabled = true;
      try {
        const j = await api(editing ? 'team/edit' : 'team/add', body);
        if (j.note) toast(j.note, 9000);
        else toast(editing ? 'Saved.' : name.value.trim() + ' is in #' + S.room + '. Write @' + body.handle + ' to ask.');
        await loadRooms();
        closeSheet();
        openPeople();
      } catch (x) { err.textContent = x.message; } finally { save.disabled = false; }
    });
    box.append(form);
  });
}

async function openInvite() {
  const r = roomOf(S.room);
  let j;
  try { j = await api('invite', { room: S.room }); } catch (e) { toast(e.message); return; }
  linkSheet(j, {
    title: 'Share this room: #' + r.id,
    what: 'The code is the key to this room. It works once',
    label: 'Link to share',
    share: 'Join #' + r.id + ' on Perennos Tempo Room (code ' + j.code + ')',
    note: 'Anyone you give this code or link to can join #' + r.id + ' once and share the room, including its teammates. Share it only with people you trust.',
  });
}

// The owner adds a device for someone who is already in: a one-time sign-in link and code.
async function openSignIn(m) {
  let j;
  try { j = await api('invite', { member: m.id }); } catch (e) { toast(e.message); return; }
  linkSheet(j, {
    title: 'New sign-in link for ' + m.name,
    what: 'This sign-in link adds one more device for ' + m.name + '. It works once',
    label: 'Sign-in link',
    share: 'Sign in to Perennos Tempo Room (code ' + j.code + ')',
    note: m.name + ' keeps the same name and rooms, and the other devices stay signed in.',
  });
}

function linkSheet(j, o) {
  sheet((box) => {
    box.append(el('h2', null, o.title));
    const until = new Date(j.expires * 1000).toLocaleString(undefined, { weekday: 'short', day: 'numeric', month: 'short', hour: '2-digit', minute: '2-digit' });
    box.append(el('p', 'sub', o.what + ', until ' + until + '. Send the link, or read the code out loud.'));
    if (o.note) box.append(el('p', 'hint', o.note));
    box.append(el('p', 'code-big', j.code));
    const cb = el('div', 'copybox');
    const inp = el('input', 'field');
    inp.readOnly = true;
    inp.value = j.link;
    inp.setAttribute('aria-label', o.label);
    const c1 = el('button', 'btn go', 'Copy link');
    c1.type = 'button';
    c1.addEventListener('click', () => copy(j.link, inp));
    cb.append(inp, c1);
    box.append(cb);
    const row = el('div', 'row');
    const c2 = el('button', 'btn sm', 'Copy code');
    c2.type = 'button';
    c2.addEventListener('click', () => copy(j.code));
    row.append(c2);
    if (navigator.share) {
      const sh = el('button', 'btn sm', 'Share');
      sh.type = 'button';
      sh.addEventListener('click', () => navigator.share({ title: 'Perennos Tempo Room', text: o.share, url: j.link }).catch(() => {}));
      row.append(sh);
    }
    row.append(closeButton());
    if (j.local_only) {
      box.append(el('p', 'hint', 'This link only works on this machine. To invite family or a team, start the room on your tailnet or LAN: tempo-room serve --bind <tailnet address> --allow-host <machine name>. The README has the steps.'));
    }
    box.append(row);
  });
}

// ---------------------------------------------------------------- small things

const MARKINGS = [
  [40, 'Grave: slower than slow'], [60, 'Largo: broad and slow'], [66, 'Larghetto: gently slow'],
  [76, 'Adagio: at ease'], [108, 'Andante: a walking pace'], [120, 'Moderato: right in the middle'],
  [156, 'Allegro: bright and quick'], [176, 'Vivace: lively'], [200, 'Presto: very fast'],
];
function marking(bpm) {
  for (const [top, name] of MARKINGS) if (bpm < top) return name;
  return 'Prestissimo: as fast as it goes';
}

// /bpm: tap four times and Tempo says the tempo.
function openTap() {
  const taps = [];
  let said = false;
  sheet((box) => {
    box.append(el('h2', null, 'Tap tempo'));
    box.append(el('p', 'sub', 'Tap four times, steady as you like (or press T).'));
    const wrap = el('div', 'tap');
    const pad = el('button', 'pad', 'TAP');
    pad.type = 'button';
    const out = el('p', 'bpm', '');
    const what = el('p', 'lbl', '4 taps to go');
    wrap.append(pad, out, what);
    box.append(wrap);
    const hit = () => {
      const now = performance.now();
      if (taps.length && now - taps[taps.length - 1] > 2500) { taps.length = 0; said = false; }
      taps.push(now);
      if (taps.length > 8) taps.shift();
      pad.classList.add('hit');
      setTimeout(() => pad.classList.remove('hit'), 90);
      if (taps.length < 4) {
        what.textContent = (4 - taps.length) + (4 - taps.length === 1 ? ' tap to go' : ' taps to go');
        out.textContent = '·'.repeat(taps.length);
        return;
      }
      const bpm = Math.round(60000 / ((taps[taps.length - 1] - taps[0]) / (taps.length - 1)));
      out.textContent = bpm + ' BPM';
      what.textContent = marking(bpm);
      if (!said) {
        said = true;
        localNote("That's " + bpm + ' BPM. ' + marking(bpm) + '.');
      }
    };
    pad.addEventListener('pointerdown', (e) => { e.preventDefault(); hit(); });
    pad.addEventListener('keydown', (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); hit(); } });
    box.addEventListener('keydown', (e) => { if (e.key === 't' || e.key === 'T') hit(); });
    const row = el('div', 'row');
    row.append(closeButton());
    box.append(row);
  });
}

// The Konami code (outside the message box): Tempo's next answer here is a haiku.
const KONAMI = ['ArrowUp', 'ArrowUp', 'ArrowDown', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'ArrowLeft', 'ArrowRight', 'b', 'a'];
let konami = 0;
function onDocKey(e) {
  if (e.key === 'Escape' && !$('sheet').hidden) { closeSheet(); return; }
  const t = e.target;
  if (t instanceof Element && t.closest('input,textarea')) { konami = 0; return; }
  const k = e.key.length === 1 ? e.key.toLowerCase() : e.key;
  if (k === KONAMI[konami]) konami += 1;
  else konami = k === KONAMI[0] ? 1 : 0;
  if (konami === KONAMI.length) {
    konami = 0;
    haiku();
  }
}
async function haiku() {
  if (!S.room) { toast('Open a room first, then try that again.'); return; }
  try {
    const j = await api('haiku', { room: S.room });
    toast(j.note, 6000);
  } catch (e) { toast(e.message); }
}

// ---------------------------------------------------------------- wiring

function wire() {
  $('theme').addEventListener('click', () => {
    const cur = document.documentElement.dataset.theme;
    applyTheme(THEMES[(THEMES.indexOf(cur) + 1) % THEMES.length]);
  });
  $('join-form').addEventListener('submit', join);
  $('back').addEventListener('click', () => { if (history.state && history.state.room) history.back(); else closeRoom(); });
  window.addEventListener('popstate', () => { if (S.room && !wide()) closeRoom(); });
  $('new-room').addEventListener('click', () => {
    const f = $('new-room-form');
    f.hidden = !f.hidden;
    if (!f.hidden) $('new-room-name').focus();
  });
  $('new-room-form').addEventListener('submit', async (e) => {
    e.preventDefault();
    const name = $('new-room-name').value.trim();
    if (!name) return;
    try {
      const j = await api('create', { name });
      $('new-room-name').value = '';
      $('new-room-form').hidden = true;
      await loadRooms();
      await openRoom(j.room.id, true);
    } catch (err) { toast(err.message); }
  });
  $('logout').addEventListener('click', async () => {
    const msg = S.me.role === 'member'
      ? 'Sign out on this device only? Your other devices stay signed in. To use this device again you would need a new sign-in link or invite from the owner.'
      : 'Sign out on this device? You would need a new invite (or the owner link) to come back.';
    if (!confirm(msg)) return;
    await api('logout', {});
    location.reload();
  });
  $('people').addEventListener('click', openPeople);
  $('invite').addEventListener('click', openInvite);
  $('send').addEventListener('click', send);
  $('text').addEventListener('input', onInput);
  $('text').addEventListener('keydown', onKey);
  $('text').addEventListener('blur', () => setTimeout(closeSuggest, 150));
  $('replying-x').addEventListener('click', () => { S.replyTo = null; updateReply(); });
  $('jump').addEventListener('click', () => { const b = $('msgs'); b.scrollTop = b.scrollHeight; $('jump').hidden = true; });
  $('msgs').addEventListener('scroll', () => { if (nearEnd($('msgs'))) $('jump').hidden = true; });
  $('sheet').addEventListener('click', (e) => { if (e.target === $('sheet')) closeSheet(); });
  document.addEventListener('keydown', onDocKey);
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden && S.room) { S.unread.delete(S.room); updateTitle(); renderRoomsSoon(); }
  });
  hintComposer('unknown');
}

boot().catch((e) => toast(e.message || 'Could not start.', 10000));
