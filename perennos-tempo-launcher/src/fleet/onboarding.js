'use strict';
// Authority arrives once from the native launcher, stays out of requests/URLs, and survives refresh.
const fragment = new URLSearchParams(location.hash.slice(1));
const incoming = fragment.get('key');
history.replaceState(null, '', location.pathname);
if (incoming && /^[a-f0-9]{64}$/.test(incoming)) sessionStorage.setItem('fleet-owner', incoming);
const token = sessionStorage.getItem('fleet-owner') || '';
const el = id => document.getElementById(id);
const profileId = document.querySelector('.tempo-tabs')?.dataset.profileId || '';
const scanCacheKey = profileId ? `perennos-tempo.scan.v1.${profileId}` : '';
const scanTtlMs = 24 * 60 * 60 * 1000;
let handle = '', sending = false, lastBusy = false, memberKey = '', candidateKey = '', companyKey = '', engineKey = '', cloudKey = '', didInitialDiscovery = false, scanEnginesAfterDiscovery = false, scanCloudAfterEngines = false, initialScanFinished = false, rescanInProgress = false, scanFailed = false, currentState = null;

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
async function readState() {
  const response = await fetch('/state', {
    method: 'GET', cache: 'no-store', headers: { Authorization: `Bearer ${token}` }, signal: AbortSignal.timeout(8000)
  });
  if (!response.ok) {
    if (response.status === 403) throw new Error('Open Fleet from Perennos Tempo on this computer to continue.');
    throw new Error('The workbench state could not be read.');
  }
  return response.json();
}
function text(tag, value) { const node = document.createElement(tag); node.textContent = value; return node; }
function setupUi() {
  const company = document.createElement('section');
  company.id = 'company'; company.setAttribute('aria-labelledby', 'company-title');
  company.innerHTML = '<h2 id="company-title">Your org chart</h2><p class="small">You lead. Approved nodes, engines and agent lanes join only after you approve their node.</p><p id="membership-counts" class="small" aria-live="polite"></p><ul id="org-members" class="machine-list"></ul><h3 class="tempo-section-heading">Departments</h3><ul id="departments" class="machine-list"></ul><div class="actions"><label for="department-name">Add a department</label><input id="department-name" maxlength="64" autocomplete="off"><button id="add-department">Add</button></div>';
  const inventory = document.createElement('section');
  inventory.id = 'engine-inventory'; inventory.setAttribute('aria-labelledby', 'engine-title');
  inventory.innerHTML = '<div class="section-tools"><h2 id="engine-title">Engines by node</h2><div class="actions"><button id="scan-engines">Detect engines</button><button id="scan-cloud">Check cloud sign-in</button></div></div><p class="small">Detection is read-only. Tempo never starts sign-in. Each node and its listed engines stay pending until you approve their node.</p><p id="scan-freshness" class="small" aria-live="polite">No scan has been run.</p><h3>Cloud connections</h3><p id="cloud-notice" class="small">Connect opens provider instructions when you choose it. No sign-in starts automatically.</p><ul id="cloud-accounts" class="machine-list" aria-label="Cloud sign-in status"></ul><div class="actions"><label for="cloud-node">Node</label><select id="cloud-node"></select><label for="cloud-provider">Cloud provider</label><input id="cloud-provider" maxlength="64" autocomplete="off" placeholder="Provider name only"><button id="declare-cloud">Add unverified lane</button></div><ul id="engine-candidates" class="machine-list"></ul>';
  const settings = document.createElement('section');
  settings.id = 'setup-settings'; settings.innerHTML = '<h2>Settings</h2><p class="small">First-run setup can be opened again at any time.</p><button id="setup-again">Run setup again</button>';
  el('conductor-company-mount').append(company);
  document.querySelector('.operation').after(inventory, settings);
}
setupUi();
function scanResultValid(value) {
  const results = value?.results;
  return value?.schema === 1 && ['complete', 'partial'].includes(value.status) && Number.isFinite(value.timestamp) && value.timestamp > 0 && value.timestamp <= Date.now() &&
    Array.isArray(results?.candidates) && results.candidates.every(item => item && typeof item.target === 'string' && typeof item.state === 'string') &&
    Array.isArray(results?.engine_candidates) && results.engine_candidates.every(node => node && typeof node.node_id === 'string' && Array.isArray(node.engines) && node.engines.every(engine => engine && typeof engine.name === 'string' && typeof engine.kind === 'string')) &&
    Array.isArray(results?.cloud_accounts) && results.cloud_accounts.every(account => account && typeof account.id === 'string' && typeof account.name === 'string' && typeof account.status === 'string' && typeof account.source === 'string');
}
function readScanCache() {
  if (!scanCacheKey) return null;
  try {
    const raw = localStorage.getItem(scanCacheKey);
    if (!raw || raw.length > 262144) return null;
    const value = JSON.parse(raw);
    if (!scanResultValid(value)) return null;
    return { value, fresh: value.status === 'complete' && Date.now() - value.timestamp < scanTtlMs };
  } catch (_) { return null; }
}
function writeScanCache(state, status = 'complete') {
  if (!scanCacheKey) return false;
  const value = { schema: 1, status, timestamp: Date.now(), results: {
    candidates: state.candidates || [], engine_candidates: state.engine_candidates || [], cloud_accounts: state.cloud_accounts || []
  } };
  try {
    const raw = JSON.stringify(value);
    if (raw.length > 262144) return false;
    localStorage.setItem(scanCacheKey, raw);
    return true;
  } catch (_) { return false; }
}
function showScanFreshness(message) {
  for (const id of ['scan-freshness', 'conductor-scan-freshness']) {
    const node = el(id);
    if (node) node.textContent = message;
  }
}
function scannedAt(timestamp, cached) {
  return `Last scan: ${new Date(timestamp).toLocaleString()}${cached ? ' (cached)' : ''}.`;
}
function restoreScanCache(state, cached) {
  const results = cached.value.results;
  const restored = { ...state, candidates: results.candidates, engine_candidates: results.engine_candidates, cloud_accounts: results.cloud_accounts };
  show(restored);
  const partial = cached.value.status !== 'complete';
  showScanFreshness(`${scannedAt(cached.value.timestamp, true)}${partial ? ' Previous scan was incomplete; refreshing.' : cached.fresh ? '' : ' Refreshing because this scan is over 24 hours old.'}`);
  return restored;
}
function disabled(busy) {
  for (const id of ['discover', 'prepare', 'approve', 'decline', 'check-update', 'automatic', 'scan-engines', 'scan-cloud', 'declare-cloud', 'add-department', 'setup-again', 'conductor-setup-again', 'cloud-node', 'cloud-provider']) el(id).disabled = busy;
  el('target').disabled = busy;
  for (const button of document.querySelectorAll('#candidates button, #engine-candidates button, #org-members button')) button.disabled = busy;
}
function show(state) {
  currentState = state;
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
  const nextCompany = JSON.stringify(state.company || {});
  if (nextCompany !== companyKey) { companyKey = nextCompany; renderCompany(state.company || {}); }
  const nextEngines = JSON.stringify(state.engine_candidates || []);
  if (nextEngines !== engineKey) { engineKey = nextEngines; renderEngineCandidates(state.engine_candidates || []); }
  const nextCloud = JSON.stringify(state.cloud_accounts || []);
  if (nextCloud !== cloudKey) { cloudKey = nextCloud; renderCloudAccounts(state.cloud_accounts || []); }
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
  renderConductor(state);
}
function renderCompany(company) {
  const org = company.org || {}, ceo = org.ceo || {}, manager = org.manager || {}, peers = Array.isArray(org.peers) ? org.peers : [];
  const ordered = [ceo, manager, ...peers].filter((p, i, all) => p.id && all.findIndex(x => x.id === p.id) === i);
  const count = kind => peers.filter(p => p.kind === kind && p.status === 'active').length;
  const nodes = count('node'), engines = count('engine'), agents = count('agent');
  el('membership-counts').textContent = `${nodes} ${nodes === 1 ? 'node' : 'nodes'} · ${engines} ${engines === 1 ? 'engine' : 'engines'} · ${agents} ${agents === 1 ? 'agent lane' : 'agent lanes'}`;
  el('org-members').replaceChildren(...ordered.map(p => {
    const row = document.createElement('li');
    const label = text('strong', `${p.name || 'Employee'} · ${p.role || p.kind || 'Peer'}`);
    const input = document.createElement('input'); input.maxLength = 64; input.value = p.name || ''; input.setAttribute('aria-label', `Rename ${p.name || 'employee'}`);
    const rename = text('button', 'Rename'); rename.type = 'button'; rename.dataset.rename = p.id;
    const actions = document.createElement('div'); actions.className = 'org-actions';
    actions.append(rename);
    if (p.source_name) actions.append(text('span', p.source_name));
    if (peers.some(x => x.id === p.id)) {
      const up = text('button', 'Move up'); up.type = 'button'; up.dataset.move = p.id; up.dataset.direction = 'up';
      const down = text('button', 'Move down'); down.type = 'button'; down.dataset.move = p.id; down.dataset.direction = 'down';
      const remove = text('button', 'Remove from org'); remove.type = 'button'; remove.dataset.remove = p.id; remove.setAttribute('aria-label', `Remove ${p.name || 'employee'} from org`);
      actions.append(up, down, remove);
    }
    row.append(label, input, actions);
    return row;
  }));
  el('departments').replaceChildren(...(company.departments || []).map(d => text('li', d.name || 'Department')));
}
function renderEngineCandidates(nodes) {
  const select = el('cloud-node'); select.replaceChildren();
  for (const node of nodes) { const option = document.createElement('option'); option.value = node.node_id; option.textContent = node.name; select.append(option); }
  el('engine-candidates').replaceChildren(...nodes.map(node => {
    const row = document.createElement('li');
    const engines = node.engines || [];
    row.append(text('strong', `${node.name}: ${engines.length} engine${engines.length === 1 ? '' : 's'} found`));
    const list = document.createElement('ul');
    for (const engine of engines) list.append(text('li', `${engine.name} · ${engine.kind} · ${engine.status || 'Found'}`));
    row.append(list);
    const approve = text('button', 'Approve this node'); approve.type = 'button'; approve.dataset.approveNode = node.node_id; row.append(approve);
    return row;
  }));
}
function renderCloudAccounts(accounts) {
  const guides = {
    codex: 'https://developers.openai.com/codex/auth',
    claude: 'https://code.claude.com/docs/en/cli-usage',
    gemini: 'https://geminicli.com/docs/get-started/authentication/'
  };
  el('cloud-accounts').replaceChildren(...accounts.map(account => {
    const row = document.createElement('li'); row.className = 'cloud-account';
    const identity = document.createElement('div');
    identity.append(text('strong', account.name), text('span', `${account.status} · ${account.source}`));
    const connect = text('a', account.status === 'Connected' ? 'Connection guide' : 'Connect');
    connect.className = 'cloud-connect'; connect.href = guides[account.id] || '#';
    connect.target = '_blank'; connect.rel = 'noopener noreferrer';
    row.append(identity, connect); return row;
  }));
}
function knownNodes(state) {
  const nodes = new Map();
  nodes.set('local', { id: 'local', name: 'This computer', state: 'Ready', count: 0, pending: false });
  for (const member of state.members || []) nodes.set(member.name, { id: member.name, name: member.name, state: member.state, count: 0, pending: false });
  for (const node of state.engine_candidates || []) nodes.set(node.node_id, { id: node.node_id, name: node.name, state: 'Pending approval', count: (node.engines || []).length, pending: true });
  for (const node of (state.company || {}).nodes || []) {
    const id = node.node_id || node.id;
    nodes.set(id, { id, name: node.name, state: 'Approved', count: 0, pending: false });
  }
  if ((state.cloud_accounts || []).length && !nodes.has('cloud')) nodes.set('cloud', { id: 'cloud', name: 'Cloud connections', state: 'Status checked', count: 0, pending: false });
  return [...nodes.values()];
}
function renderTour(state, nodes) {
  const orgList = el('conductor-tour-org')?.querySelector('ul');
  const org = (state.company || {}).org || {};
  const roster = [org.ceo, org.manager, ...(org.peers || [])].filter((person, i, all) => person?.id && all.findIndex(other => other?.id === person.id) === i);
  if (orgList) orgList.replaceChildren(...roster.map(person => {
    const item = document.createElement('li');
    item.append(text('strong', person.name || 'Employee'), text('span', person.role || person.kind || 'Employee'));
    return item;
  }));
  const mapList = el('conductor-tour-map')?.querySelector('ul');
  if (mapList) mapList.replaceChildren(...nodes.map(node => {
    const item = document.createElement('li'); item.append(text('strong', node.name), text('span', node.state)); return item;
  }));
  const tourStandings = el('conductor-tour-standings')?.querySelector('p');
  const standings = state.standings || [];
  if (tourStandings) tourStandings.textContent = standings.length ? `Current leader: ${standings[0].name}, with ${standings[0].completed_steps} completed steps this month.` : 'No completed work this month yet. Standings use completed Conductor steps.';
}
let conductorKey = '';
function renderConductor(state, force = false) {
  const nodes = knownNodes(state);
  const nextKey = JSON.stringify([nodes, state.company, state.standings, state.floor_traffic]);
  if (nextKey !== conductorKey || force) {
    conductorKey = nextKey;
    el('map-directory').replaceChildren(...nodes.map(node => {
      const row = document.createElement('li');
      const identity = document.createElement('div');
      identity.append(text('strong', node.name), text('span', node.count ? `${node.state} · ${node.count} detected engines` : node.state));
      row.append(identity); return row;
    }));
    const standings = state.standings || [];
    el('standings').replaceChildren(...standings.map(item => {
      const row = document.createElement('li'); row.append(text('strong', `${item.rank}. ${item.name}`), text('span', `${item.completed_steps} completed steps`)); return row;
    }));
    el('standings-empty').hidden = standings.length > 0;
    const traffic = state.floor_traffic || [];
    el('floor-traffic').replaceChildren(...traffic.map(item => {
      const row = document.createElement('li'); row.append(text('strong', `${item.employee} · ${item.band}`), text('span', `${item.date} · step ${item.step}`)); return row;
    }));
    el('traffic-empty').hidden = traffic.length > 0;
    renderTour(state, nodes);
  }
  const canvas = el('conductor-map');
  const rect = canvas.getBoundingClientRect();
  if (rect.width < 1 || (el('conductor-view').hidden && !force)) return;
  const ratio = Math.min(window.devicePixelRatio || 1, 2);
  canvas.width = Math.round(rect.width * ratio); canvas.height = Math.round(rect.height * ratio);
  const ctx = canvas.getContext('2d');
  if (!ctx) return;
  ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
  ctx.clearRect(0, 0, rect.width, rect.height);
  const style = getComputedStyle(document.documentElement);
  const ink = style.getPropertyValue('--ink').trim() || '#e8e5df';
  const warm = style.getPropertyValue('--warm').trim() || '#e18a52';
  const line = style.getPropertyValue('--line2').trim() || 'rgba(180,190,198,.3)';
  const silver = style.getPropertyValue('--silver').trim() || '#9ba4aa';
  const cx = rect.width / 2, cy = rect.height / 2;
  const radius = Math.min(rect.width * .36, rect.height * .38);
  ctx.lineWidth = 1; ctx.strokeStyle = line;
  for (const scale of [.42, .7, 1]) { ctx.beginPath(); ctx.ellipse(cx, cy, radius * scale, radius * scale * .56, 0, 0, Math.PI * 2); ctx.stroke(); }
  ctx.fillStyle = warm; ctx.beginPath(); ctx.arc(cx, cy, 17, 0, Math.PI * 2); ctx.fill();
  ctx.fillStyle = ink; ctx.font = '600 11px sans-serif'; ctx.textAlign = 'center'; ctx.fillText('TEMPO', cx, cy + 34);
  ctx.font = '12px sans-serif';
  nodes.forEach((node, i) => {
    const angle = -Math.PI / 2 + i * 2.399963229728653;
    const ring = [.52, .77, .98][i % 3];
    const x = cx + Math.cos(angle) * radius * ring;
    const y = cy + Math.sin(angle) * radius * ring * .56;
    ctx.beginPath(); ctx.setLineDash(node.pending ? [3, 4] : []); ctx.moveTo(cx, cy); ctx.lineTo(x, y); ctx.strokeStyle = line; ctx.stroke();
    ctx.setLineDash([]); ctx.beginPath(); ctx.arc(x, y, node.pending ? 7 : 6, 0, Math.PI * 2); ctx.fillStyle = node.pending ? silver : warm; ctx.fill();
    ctx.fillStyle = ink; ctx.textAlign = Math.cos(angle) > 0 ? 'left' : 'right';
    const label = node.name.length > 16 ? `${node.name.slice(0, 15)}…` : node.name;
    ctx.fillText(label, x + (Math.cos(angle) > 0 ? 10 : -10), y + 4, Math.min(120, rect.width * .26));
  });
}
function setView(view) {
  const conductor = view === 'conductor';
  el('setup-view').hidden = conductor; el('conductor-view').hidden = !conductor;
  el('view-setup').setAttribute('aria-pressed', String(!conductor));
  el('view-conductor').setAttribute('aria-pressed', String(conductor));
  if (conductor) { el('conductor-title').focus(); if (currentState) renderConductor(currentState, true); }
}
async function perform(action, fields) {
  if (sending) return;
  sending = true; disabled(true); el('error').textContent = '';
  try { show(await api(action, fields)); }
  catch (error) {
    el('error').textContent = error.message;
    if (['discover', 'scan-engines', 'scan-cloud'].includes(action)) {
      if (rescanInProgress) {
        scanFailed = true; rescanInProgress = false;
        scanEnginesAfterDiscovery = false; scanCloudAfterEngines = false;
        if (currentState) writeScanCache(currentState, 'partial');
        showScanFreshness('Scan incomplete. The last complete results are shown when available; open Setup to retry.');
        initialScanFinished = true;
        setView('conductor'); window.startConductorTour();
      } else {
        if (currentState) writeScanCache(currentState, 'partial');
        showScanFreshness('Scan incomplete. Open Setup and run it again.');
      }
    }
  }
  finally { sending = false; disabled(lastBusy); }
}
el('target-form').addEventListener('submit', event => { event.preventDefault(); perform('prepare', { target: el('target').value.trim() }); });
function startDiscovery() {
  scanEnginesAfterDiscovery = true; scanCloudAfterEngines = true; initialScanFinished = false;
  rescanInProgress = true; scanFailed = false;
  showScanFreshness('Scanning computers, engines and cloud sign-in status.');
  if (currentState) writeScanCache(currentState, 'partial');
  perform('discover');
}
el('discover').addEventListener('click', startDiscovery);
el('approve').addEventListener('click', () => perform('approve', { handle }));
el('decline').addEventListener('click', () => perform('decline'));
el('check-update').addEventListener('click', () => perform('updates'));
el('automatic').addEventListener('change', () => perform('automatic', { enabled: el('automatic').checked }));
el('scan-engines').addEventListener('click', () => { if (currentState) writeScanCache(currentState, 'partial'); showScanFreshness('Checking engines.'); perform('scan-engines'); });
el('scan-cloud').addEventListener('click', () => { if (currentState) writeScanCache(currentState, 'partial'); showScanFreshness('Checking cloud sign-in status.'); perform('scan-cloud'); });
el('engine-candidates').addEventListener('click', event => {
  const button = event.target.closest('[data-approve-node]');
  if (button) perform('approve-node', { node: button.dataset.approveNode });
});
el('org-members').addEventListener('click', event => {
  const remove = event.target.closest('[data-remove]');
  if (remove) { perform('remove-member', { id: remove.dataset.remove }); return; }
  const rename = event.target.closest('[data-rename]');
  if (rename) { const input = rename.closest('li').querySelector('input'); perform('rename-member', { id: rename.dataset.rename, name: input.value.trim() }); return; }
  const move = event.target.closest('[data-move]');
  if (move) perform('reorder-peer', { id: move.dataset.move, direction: move.dataset.direction });
});
el('add-department').addEventListener('click', () => perform('add-department', { name: el('department-name').value.trim() }));
el('declare-cloud').addEventListener('click', () => perform('declare-cloud', { node: el('cloud-node').value, provider: el('cloud-provider').value.trim() }));
el('setup-again').addEventListener('click', startDiscovery);
el('conductor-setup-again').addEventListener('click', () => { setView('setup'); startDiscovery(); });
el('view-setup').addEventListener('click', () => setView('setup'));
el('view-conductor').addEventListener('click', () => setView('conductor'));
el('update-toggle').addEventListener('click', () => {
  const open = el('updates').hidden; el('updates').hidden = !open; el('update-toggle').setAttribute('aria-expanded', String(open));
});
async function poll() {
  try {
    if (!sending) {
      const state = await readState();
      show(state);
      if (token && !state.busy && !didInitialDiscovery) {
        didInitialDiscovery = true;
        const cached = readScanCache();
        if (cached) {
          restoreScanCache(state, cached);
          if (cached.fresh) {
            initialScanFinished = true;
            setView('conductor'); window.startConductorTour();
          } else startDiscovery();
        } else startDiscovery();
      } else if (token && !state.busy && scanEnginesAfterDiscovery) {
        scanEnginesAfterDiscovery = false;
        perform('scan-engines');
      } else if (token && !state.busy && scanCloudAfterEngines) {
        scanCloudAfterEngines = false;
        perform('scan-cloud');
      } else if (token && !state.busy && didInitialDiscovery && !initialScanFinished) {
        initialScanFinished = true;
        rescanInProgress = false;
        if (!scanFailed && currentState) {
          const saved = writeScanCache(currentState, 'complete');
          showScanFreshness(`${scannedAt(Date.now(), false)}${saved ? '' : ' Results could not be saved for the next open.'}`);
        }
        setView('conductor');
        window.startConductorTour();
      }
    }
  } catch (error) { el('error').textContent = error.message; disabled(true); }
  setTimeout(poll, 1500);
}
if (token) poll();
