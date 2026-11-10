'use strict';
// Authority arrives once from the native launcher, stays out of requests/URLs, and survives refresh.
const fragment = new URLSearchParams(location.hash.slice(1));
const incoming = fragment.get('key');
history.replaceState(null, '', location.pathname);
if (incoming && /^[a-f0-9]{64}$/.test(incoming)) sessionStorage.setItem('fleet-owner', incoming);
const token = sessionStorage.getItem('fleet-owner') || '';
const el = id => document.getElementById(id);
let handle = '', sending = false, lastBusy = false, memberKey = '', candidateKey = '';

async function api(action, fields = {}) {
  const response = await fetch('/api', {
    method: 'POST', cache: 'no-store', headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
    body: JSON.stringify({ action, ...fields }), signal: AbortSignal.timeout(8000)
  });
  if (!response.ok) {
    if (response.status === 403) throw new Error('Open Fleet from Perennos Tempo on this computer to continue.');
    const body = await response.json(); throw new Error(body.error || 'The operation could not start.');
  }
  return response.json();
}
function text(tag, value) { const node = document.createElement(tag); node.textContent = value; return node; }
function disabled(busy) {
  for (const id of ['discover', 'prepare', 'approve', 'decline', 'check-update', 'automatic']) el(id).disabled = busy;
  el('target').disabled = busy;
  for (const button of el('candidates').querySelectorAll('button')) button.disabled = busy;
}
function show(state) {
  lastBusy = state.busy;
  disabled(sending || state.busy);
  el('heading').textContent = state.heading;
  el('stage').textContent = state.stage;
  el('stage').dataset.state = state.busy ? 'loading' : state.stage === 'Connected' ? 'success' : 'idle';
  el('error').textContent = state.error;
  const nextMembers = JSON.stringify(state.members);
  if (nextMembers !== memberKey) {
  memberKey = nextMembers;
  el('members').replaceChildren(...state.members.map(member => {
    const row = document.createElement('li');
    const identity = document.createElement('div'); identity.append(text('strong', member.name), text('span', member.address));
    row.append(identity, text('span', member.state)); return row;
  }));
  if (!state.members.length) el('members').append(text('li', 'This computer is ready. Add another when you need it.'));
  }
  const nextCandidates = JSON.stringify(state.candidates);
  if (nextCandidates !== candidateKey) {
  candidateKey = nextCandidates;
  el('candidates').replaceChildren(...state.candidates.map(candidate => {
    const row = document.createElement('li'); const identity = document.createElement('div');
    identity.append(text('strong', candidate.name || candidate.target), text('span', `${candidate.target} · ${candidate.state}`));
    const button = text('button', 'Review setup'); button.disabled = sending || state.busy;
    button.addEventListener('click', () => { el('target').value = candidate.target; perform('prepare', { target: candidate.target }); });
    row.append(identity, button); return row;
  }));
  }
  const review = state.review;
  const nextHandle = review ? review.handle : '';
  const newReview = nextHandle && nextHandle !== handle;
  handle = nextHandle;
  el('review').hidden = !review;
  if (review) {
    el('review-facts').replaceChildren();
    const facts = [['Computer', review.name], ['SSH target', review.target], ['Address', review.address], ['Platform', review.platform], ['Version', review.version], ['Native home', review.home], ['Launcher', review.launcher], ['Controller port', String(review.controller_port)], ['Download', `${review.download_bytes.toLocaleString()} bytes`]];
    if (review.sha256) facts.push(['SHA-256', review.sha256]);
    for (const [key, value] of facts) el('review-facts').append(text('dt', key), text('dd', value));
    el('changes').textContent = review.changes;
    if (newReview) { el('review-title').tabIndex = -1; el('review-title').focus(); }
  }
  const updates = state.updates;
  el('update-toggle').textContent = updates.latest ? `Update ${updates.latest}` : 'Updates';
  el('update-text').textContent = `${updates.current} installed. ${updates.text}`;
  el('automatic').checked = updates.enabled;
  el('release').hidden = !updates.url;
  if (updates.url && /^https:\/\/github\.com\/Perennos\/perennos-tempo\/releases\/tag\/v\d+\.\d+\.\d+$/.test(updates.url)) el('release').href = updates.url;
  else el('release').removeAttribute('href');
}
async function perform(action, fields) {
  if (sending) return;
  sending = true; disabled(true); el('error').textContent = '';
  try { show(await api(action, fields)); }
  catch (error) { el('error').textContent = error.message; }
  finally { sending = false; disabled(lastBusy); }
}
el('target-form').addEventListener('submit', event => { event.preventDefault(); perform('prepare', { target: el('target').value.trim() }); });
el('discover').addEventListener('click', () => perform('discover'));
el('approve').addEventListener('click', () => perform('approve', { handle }));
el('decline').addEventListener('click', () => perform('decline'));
el('check-update').addEventListener('click', () => perform('updates'));
el('automatic').addEventListener('change', () => perform('automatic', { enabled: el('automatic').checked }));
el('update-toggle').addEventListener('click', () => {
  const open = el('updates').hidden; el('updates').hidden = !open; el('update-toggle').setAttribute('aria-expanded', String(open));
});
async function poll() {
  try { if (!sending) show(await api('status')); }
  catch (error) { el('error').textContent = error.message; disabled(true); }
  setTimeout(poll, 1500);
}
if (token) poll();

