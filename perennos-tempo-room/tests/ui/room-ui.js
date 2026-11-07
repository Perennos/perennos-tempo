// Drive the Perennos Tempo Room app in Chromium (Playwright) against a fake agent and the real binary.
// usage: node crates/perennos-tempo-room/tests/ui/room-ui.js <path to tempo-room> <screenshot folder>
// Covers the teammates feature too (Team section, @mentions, a mock Ollama, a DM room).
// Prints one line per step and ends with "PROBLEMS none" (exit 0) or the list of problems.
const { chromium } = require('playwright');
const http = require('http');
const { spawn } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

const BIN = process.argv[2];
const SHOTS = process.argv[3];
fs.mkdirSync(SHOTS, { recursive: true });
const problems = [];
const steps = [];
const fetched = {}; // who -> every JSON body that page fetched
const kids = []; // child processes this script started
const note = (s) => { steps.push(s); console.log('ok  ' + s); };

// ---------------------------------------------------------------- the fake agent
const convs = new Map();
let nconv = 0;
const prompts = [];
const news = []; // every POST /agent/new body, with its conversation id
function push(c, ev) { ev.n = c.events.length; ev.t = new Date().toISOString(); c.events.push(ev); }
function answer(p, c) {
  // A teammate's conversation was made with a role: the answer depends on it.
  if (c && c.role && /Cooking coach/.test(c.role)) return 'Sage here: try a lemon pasta.';
  const first = p.split('\n')[0];
  if (p.includes('as a haiku')) return 'Salt wind on the reef,\nthe sea folds its blue pages;\nTempo keeps the beat.';
  if (p.includes('Fact-check this message')) return 'Mostly true: tacos are a fine plan tonight. "Better than pasta" is an opinion, though, not a fact.';
  if (p.includes('Summarize these messages')) return '- Riley asked: pasta or tacos.\n- Mia voted tacos.\n- Still open: dessert.';
  if (/sky/i.test(first)) return 'Blue, thanks to Rayleigh scattering: the air scatters short blue wavelengths far more than red ones.';
  if (/cook/i.test(first)) return 'Tacos. Quick, cheerful, and everyone builds their own. Fish tacos if you want a Cayman touch.';
  return 'Happy to help. What would you like to know?';
}
const agent = http.createServer((req, res) => {
  let body = '';
  req.on('data', (d) => { body += d; });
  req.on('end', () => {
    const url = new URL(req.url, 'http://x');
    const j = body ? JSON.parse(body) : {};
    const send = (code, obj) => { res.writeHead(code, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(obj)); };
    if (req.headers.origin) problems.push('the agent got an Origin header');
    if (req.method === 'GET' && url.pathname === '/agent/health') return send(200, { ok: true, stream: true, guest: true, roles: true, auth: 'none', models: ['fake'] });
    if (req.method === 'POST' && url.pathname === '/agent/new') {
      // like the real agent: guest mode only, a role of at most 2000 bytes
      if (j.mode !== 'guest') return send(400, { error: 'mode must be guest' });
      if (j.role !== undefined && Buffer.byteLength(String(j.role)) > 2000) return send(400, { error: 'role is too long (2000 bytes at most)' });
      const id = 'conv-' + (++nconv);
      convs.set(id, { events: [], busy: false, mode: j.mode, model: j.model, role: j.role });
      news.push({ conv: id, mode: j.mode, model: j.model, role: j.role });
      return send(200, { conv: id });
    }
    if (req.method === 'POST' && url.pathname === '/agent/send') {
      const c = convs.get(j.conv);
      if (!c) return send(404, { error: 'unknown conversation' });
      if (c.busy) return send(409, { error: 'busy' });
      c.busy = true;
      push(c, { type: 'user', text: j.text });
      prompts.push(j.text);
      setTimeout(() => {
        push(c, { type: 'tool_result', name: 'read_file', text: 'TOOL-OUTPUT-NEVER-SHOWN' });
        push(c, { type: 'assistant', text: answer(j.text, c) });
        c.busy = false;
      }, 2600);
      return send(200, { ok: true });
    }
    if (req.method === 'GET' && url.pathname === '/agent/events') {
      const c = convs.get(url.searchParams.get('conv'));
      const since = Number(url.searchParams.get('since')) || 0;
      if (!c) return send(200, { events: [], busy: false });
      return send(200, { events: c.events.slice(since), busy: c.busy });
    }
    if (req.method === 'POST' && url.pathname === '/agent/confirm') return send(200, { ok: false });
    return send(404, { error: 'not found' });
  });
});

// ---------------------------------------------------------------- a mock Ollama (engine teammates)
const ollamaCalls = [];
const ollama = http.createServer((req, res) => {
  let body = '';
  req.on('data', (d) => { body += d; });
  req.on('end', () => {
    const send = (code, obj) => { res.writeHead(code, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(obj)); };
    if (req.method === 'GET' && req.url === '/api/tags') return send(200, { models: [{ name: 'mock:4b' }] });
    if (req.method === 'POST' && req.url === '/api/chat') {
      ollamaCalls.push(JSON.parse(body));
      return setTimeout(() => send(200, { message: { role: 'assistant', content: 'Coder here: use a for loop.' }, done: true }), 1500);
    }
    return send(404, { error: 'not found' });
  });
});

function startRoom(agentPort, extraCfg) {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'tempo-room-ui-'));
  fs.writeFileSync(path.join(data, 'config.json'), JSON.stringify(Object.assign({ owner_name: 'Riley' }, extraCfg)));
  const child = spawn(BIN, ['serve', '--port', '0', '--data', data, '--agent', 'http://127.0.0.1:' + agentPort], { stdio: ['ignore', 'ignore', 'pipe'] });
  kids.push(child);
  return new Promise((resolve, reject) => {
    let buf = '';
    let addr = null;
    const t = setTimeout(() => reject(new Error('serve did not start: ' + buf)), 15000);
    child.stderr.on('data', (d) => {
      buf += d.toString();
      const a = buf.match(/\[Perennos Tempo Room on http:\/\/([^/]+)\/\]/);
      const o = buf.match(/(http:\/\/127\.0\.0\.1:\d+\/#owner=[0-9a-f]+)/);
      if (a) addr = a[1];
      if (addr && o) { clearTimeout(t); resolve({ child, addr, owner: o[1], data }); }
    });
  });
}

function watch(page, who) {
  page.on('console', (m) => {
    if (m.type() === 'error' || m.type() === 'warning') problems.push(who + ' console ' + m.type() + ': ' + m.text());
  });
  page.on('pageerror', (e) => problems.push(who + ' page error: ' + e.message));
  fetched[who] = [];
  page.on('response', async (r) => {
    try { if (/json/.test(r.headers()['content-type'] || '')) fetched[who].push(await r.text()); } catch (_) { /* gone with the page */ }
  });
  page.on('response', (r) => { if (r.status() >= 400) problems.push(who + ' HTTP ' + r.status() + ' ' + r.url()); });
  page.on('requestfailed', (r) => {
    const f = r.failure() ? r.failure().errorText : '';
    if (!/ERR_ABORTED/.test(f)) problems.push(who + ' request failed ' + r.url() + ' ' + f);
  });
}

async function newPage(ctx, who) {
  await ctx.addInitScript(() => {
    window.__csp = [];
    document.addEventListener('securitypolicyviolation', (e) => window.__csp.push(e.violatedDirective + ' ' + e.blockedURI));
  });
  const page = await ctx.newPage();
  watch(page, who);
  return page;
}

async function csp(page, who) {
  const v = await page.evaluate(() => window.__csp || []);
  for (const x of v) problems.push(who + ' CSP violation: ' + x);
}

async function noSideScroll(page, who) {
  const w = await page.evaluate(() => [document.documentElement.scrollWidth, window.innerWidth]);
  if (w[0] > w[1]) problems.push(who + ' scrolls sideways: ' + w[0] + ' > ' + w[1]);
}

const shot = (page, name) => page.screenshot({ path: path.join(SHOTS, name) });

(async () => {
  await new Promise((r) => agent.listen(0, '127.0.0.1', r));
  const agentPort = agent.address().port;
  // Ollama's port is reserved now and listened on later: until then Ollama is "down", so the Team
  // form offers a plain model box (an agent teammate's model, "fake", is not an Ollama model).
  const probe = http.createServer();
  await new Promise((r) => probe.listen(0, '127.0.0.1', r));
  const ollamaPort = probe.address().port;
  await new Promise((r) => probe.close(r));
  const room = await startRoom(agentPort, { ollama_url: 'http://127.0.0.1:' + ollamaPort });
  const base = 'http://' + room.addr;
  const browser = await chromium.launch();

  // ---- the owner, at the machine, on a desktop
  const desk = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const owner = await newPage(desk, 'owner');
  await owner.goto(room.owner);
  await owner.waitForFunction(() => /Riley/.test(document.getElementById('greeting').textContent));
  if (owner.url().includes('#owner=')) problems.push('the owner token stayed in the address bar');
  await shot(owner, '01-desktop-empty.png');
  note('owner signed in from the owner link; greeting: ' + await owner.textContent('#greeting'));
  await owner.click('#new-room');
  await owner.fill('#new-room-name', 'Family');
  await owner.press('#new-room-name', 'Enter');
  await owner.waitForSelector('#chat:not([hidden])');
  await owner.waitForFunction(() => document.getElementById('chat-name').textContent === 'Family');
  await shot(owner, '02-desktop-new-room.png');
  note('room made: ' + await owner.textContent('#chat-meta'));
  await owner.fill('#text', 'Pasta or tacos tonight?');
  await owner.press('#text', 'Enter');
  await owner.waitForSelector('.msg.mine >> text=Pasta or tacos tonight?');
  await owner.click('#text');
  await owner.locator('#text').pressSequentially('@te', { delay: 40 });
  await owner.waitForSelector('#suggest:not([hidden]) .opt');
  await shot(owner, '03-desktop-autocomplete.png');
  await owner.press('#text', 'Enter');
  const picked = await owner.inputValue('#text');
  if (picked !== '@tempo ') problems.push('autocomplete gave "' + picked + '"');
  await owner.locator('#text').pressSequentially('what should we cook?', { delay: 10 });
  await owner.press('#text', 'Enter');
  await owner.waitForSelector('#thinking:not([hidden])', { timeout: 5000 });
  await shot(owner, '04-desktop-thinking.png');
  note('thinking indicator shown while the agent works');
  await owner.waitForSelector('.msg.tempo >> text=Tacos. Quick, cheerful', { timeout: 15000 });
  await owner.waitForSelector('#thinking[hidden]', { state: 'attached', timeout: 5000 });
  await shot(owner, '05-desktop-answer.png');
  note('Tempo answered in the thread: ' + (await owner.locator('.msg.tempo .text').last().textContent()).trim());
  await owner.click('#invite');
  await owner.waitForSelector('.code-big');
  const link = await owner.inputValue('.copybox input');
  const code = (await owner.textContent('.code-big')).trim();
  await shot(owner, '06-desktop-invite.png');
  note('invite made: code ' + code.replace(/[A-Z0-9]/g, 'X') + ', link ' + link.replace(/#join=.*/, '#join=...'));
  await owner.keyboard.press('Escape');

  // ---- a member on a phone
  const phone = await browser.newContext({ viewport: { width: 375, height: 812 }, isMobile: true, hasTouch: true, deviceScaleFactor: 2 });
  const mia = await newPage(phone, 'phone');
  await mia.goto(link);
  await mia.waitForSelector('#join:not([hidden])');
  if ((await mia.inputValue('#join-code')) !== code) problems.push('the join code was not filled in from the link');
  await mia.fill('#join-name', 'Mia');
  await shot(mia, '07-phone-join.png');
  await Promise.all([mia.waitForNavigation(), mia.tap('#join-form button[type=submit]')]);
  await mia.waitForSelector('.room');
  await shot(mia, '08-phone-rooms.png');
  await noSideScroll(mia, 'phone rooms');
  note('Mia joined from the link on a phone; greeting: ' + await mia.textContent('#greeting'));
  await mia.tap('.room');
  await mia.waitForSelector('#chat:not([hidden]) .msg.tempo');
  await shot(mia, '09-phone-room.png');
  await noSideScroll(mia, 'phone room');
  await mia.tap('#text');
  await mia.locator('#text').pressSequentially('@te', { delay: 40 });
  await mia.waitForSelector('#suggest:not([hidden]) .opt');
  await shot(mia, '10-phone-autocomplete.png');
  await mia.tap('#suggest .opt');
  await mia.locator('#text').pressSequentially('is the sky blue?', { delay: 10 });
  await mia.tap('#send');
  await mia.waitForSelector('#thinking:not([hidden])', { timeout: 5000 });
  await mia.waitForSelector('.msg.tempo >> text=Rayleigh', { timeout: 15000 });
  await shot(mia, '11-phone-answer.png');
  note('Mia asked from the phone and got an answer');
  // Reply-to thread: ask Tempo to check Riley's message.
  await mia.tap('.msg >> text=Pasta or tacos tonight?');
  await mia.waitForSelector('.msg.open .acts');
  await mia.tap('.msg.open >> text=Is this true?');
  await mia.waitForSelector('#replying:not([hidden])');
  await shot(mia, '12-phone-replying.png');
  await mia.tap('#send');
  await mia.waitForSelector('.msg.tempo >> text=Mostly true', { timeout: 15000 });
  await mia.evaluate(() => { const b = document.getElementById('msgs'); b.scrollTop = b.scrollHeight; });
  await shot(mia, '13-phone-fact-check.png');
  note('reply thread: Tempo fact-checked the replied-to message');
  await csp(mia, 'phone');

  // ---- back to the owner: live updates, the small things
  await owner.waitForSelector('.msg .who >> text=Mia', { timeout: 5000 });
  note('the owner saw Mia live (SSE)');
  await owner.fill('#text', '/bpm');
  await owner.press('#text', 'Enter');
  await owner.waitForSelector('.pad');
  for (let i = 0; i < 4; i++) { await owner.click('.pad'); await owner.waitForTimeout(500); }
  const bpm = await owner.textContent('.bpm');
  await shot(owner, '14-desktop-bpm.png');
  note('/bpm tap tempo: ' + bpm + ' (' + await owner.textContent('.tap .lbl') + ')');
  await owner.keyboard.press('Escape');
  await owner.waitForSelector('.msg.local >> text=BPM');
  await owner.click('#chat-name');
  for (const k of ['ArrowUp', 'ArrowUp', 'ArrowDown', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'ArrowLeft', 'ArrowRight', 'b', 'a']) await owner.keyboard.press(k);
  await owner.waitForSelector('#toast:not([hidden]) >> text=haiku');
  await shot(owner, '15-desktop-konami.png');
  await owner.fill('#text', '@tempo describe the sea');
  await owner.press('#text', 'Enter');
  await owner.waitForSelector('.msg.tempo >> text=Salt wind on the reef', { timeout: 15000 });
  await shot(owner, '16-desktop-haiku.png');
  note('Konami code: the next answer was a haiku');
  await owner.click('#people');
  await owner.waitForSelector('.person >> text=Mia');
  await shot(owner, '17-desktop-people.png');
  await owner.keyboard.press('Escape');
  await owner.click('#theme');
  await owner.click('#theme');
  await shot(owner, '18-desktop-paper.png');
  note('themes: ' + await owner.evaluate(() => document.documentElement.dataset.theme));
  await csp(owner, 'owner');


  // ---- teammates: the owner sets up Sage (agent) and Coder (engine) through the Team section
  const sheetUp = () => owner.waitForSelector('#sheet:not([hidden])');
  const toggleOf = (name) => owner.locator('#sheet .person', { has: owner.locator('b', { hasText: new RegExp('^' + name + '$') }) }).locator('button[aria-pressed]');
  await owner.click('#people');
  await sheetUp();
  await owner.waitForSelector('#sheet .lbl.sec >> text=Team');
  if (!(await owner.textContent('#sheet')).includes('No teammates yet.')) problems.push('the Team section did not start empty');
  note('People shows the Team section, empty: ' + (await owner.locator('#sheet .hint').first().textContent()));
  await owner.click('#sheet >> text=Add a teammate');
  await owner.waitForSelector('.team-form');
  await owner.getByLabel(/^Handle/).fill('sage');
  await owner.getByLabel(/^Name$/).fill('Sage');
  await owner.getByLabel(/^Kind/).selectOption('agent');
  await owner.getByLabel(/^Model/).fill('fake');
  await owner.getByLabel(/^Role/).fill('Cooking coach for the family');
  await owner.click('.team-form button[type=submit]');
  await owner.waitForSelector('#sheet .person >> text=Sage');
  await owner.waitForSelector('#toast:not([hidden]) >> text=@sage');
  const sageRow = (await owner.locator('#sheet .person', { hasText: 'Sage' }).locator('.lbl').textContent()).replace(/\s+/g, ' ');
  if (!/@sage · agent · fake/.test(sageRow)) problems.push('Sage row reads "' + sageRow + '"');
  // the form puts the teammate in this room; switch it off and on again like the owner would
  if ((await toggleOf('Sage').getAttribute('aria-pressed')) !== 'true') problems.push('Sage was not in the room after adding');
  await toggleOf('Sage').click();
  await owner.waitForFunction(() => [...document.querySelectorAll('#sheet .person button[aria-pressed]')].some((b) => b.textContent === 'Not in this room'));
  await toggleOf('Sage').click();
  await owner.waitForFunction(() => [...document.querySelectorAll('#sheet .person button[aria-pressed]')].some((b) => b.textContent === 'In this room'));
  await owner.waitForFunction(() => /with Sage/.test(document.getElementById('chat-meta').textContent));
  note('agent teammate Sage added (' + sageRow + '), switched off and on for #family; header: ' + await owner.textContent('#chat-meta'));

  await new Promise((r) => ollama.listen(ollamaPort, '127.0.0.1', r));
  await owner.click('#sheet >> text=Add a teammate');
  await owner.waitForSelector('.team-form');
  await owner.getByLabel(/^Handle/).fill('coder');
  await owner.getByLabel(/^Name$/).fill('Coder');
  await owner.getByLabel(/^Kind/).selectOption('engine');
  const models = await owner.getByLabel(/^Model/).evaluate((n) => [n.tagName, ...[...(n.options || [])].map((o) => o.value)]);
  if (models.join() !== 'SELECT,,mock:4b') problems.push('the model list from Ollama was ' + models.join('|'));
  await owner.getByLabel(/^Model/).selectOption('mock:4b');
  await owner.getByLabel(/^Role/).fill('A terse coding helper.');
  await shot(owner, '21-desktop-team-form.png');
  await owner.click('.team-form button[type=submit]');
  await owner.waitForSelector('#sheet .person >> text=Coder');
  const coderRow = (await owner.locator('#sheet .person', { hasText: 'Coder' }).locator('.lbl').textContent()).replace(/\s+/g, ' ');
  if (!/@coder · engine · mock:4b/.test(coderRow)) problems.push('Coder row reads "' + coderRow + '"');
  await owner.waitForFunction(() => /with Sage, Coder|with Coder, Sage/.test(document.getElementById('chat-meta').textContent));
  await owner.locator('#sheet .lbl.sec', { hasText: 'Team' }).scrollIntoViewIfNeeded();
  await shot(owner, '22-desktop-team-sheet.png');
  note('engine teammate Coder added from the models Ollama offers (' + models.slice(1).filter(Boolean).join(', ') + '); ' + coderRow);
  await owner.keyboard.press('Escape');

  // ---- asking teammates in the room
  await owner.fill('#text', '@sage what should we cook?');
  await owner.press('#text', 'Enter');
  await owner.waitForSelector('#thinking:not([hidden])', { timeout: 5000 });
  await owner.waitForFunction(() => /Sage/.test(document.getElementById('thinking-mark').textContent) && /is thinking/.test(document.getElementById('thinking-text').textContent));
  const thinkSage = (await owner.textContent('#thinking')).replace(/\s+/g, ' ').trim();
  await owner.waitForSelector('.msg.team >> text=Sage here: try a lemon pasta.', { timeout: 15000 });
  await owner.waitForSelector('#thinking[hidden]', { state: 'attached', timeout: 5000 });
  const sageMsg = owner.locator('.msg.team', { hasText: 'lemon pasta' });
  const sageHd = [await sageMsg.locator('.who').textContent(), await sageMsg.locator('.via').first().textContent()].join(' ');
  if (!/^Sage @sage$/.test(sageHd)) problems.push('Sage header reads "' + sageHd + '"');
  if (await sageMsg.locator('.mark').count()) problems.push('a teammate answer carries the tempo mark');
  await owner.evaluate(() => { const b = document.getElementById('msgs'); b.scrollTop = b.scrollHeight; });
  await shot(owner, '23-desktop-teammate-answer.png');
  note('"' + thinkSage + '" then Sage answered under its own name: ' + sageHd.trim() + ' / ' + (await sageMsg.locator('.text').textContent()).trim());

  await owner.fill('#text', '@coder how do I loop?');
  await owner.press('#text', 'Enter');
  await owner.waitForFunction(() => /Coder/.test(document.getElementById('thinking-mark').textContent));
  await owner.waitForSelector('.msg.team >> text=Coder here: use a for loop.', { timeout: 15000 });
  const coderMsg = owner.locator('.msg.team', { hasText: 'for loop' });
  const coderHd = [await coderMsg.locator('.who').textContent(), await coderMsg.locator('.via').first().textContent()].join(' ');
  if (!/^Coder @coder$/.test(coderHd)) problems.push('Coder header reads "' + coderHd + '"');
  const call = ollamaCalls[ollamaCalls.length - 1] || {};
  if (call.model !== 'mock:4b' || 'tools' in call) problems.push('the Ollama request was ' + JSON.stringify(call).slice(0, 200));
  if (!(call.messages || []).some((m) => m.role === 'system' && /terse coding helper/.test(m.content))) problems.push('the role was not the system message for Ollama');
  note('Coder answered through the mock Ollama (model ' + call.model + ', no tools): ' + coderHd.trim());
  const sageNew = news.find((n) => n.role && /Cooking coach/.test(n.role));
  if (!sageNew || sageNew.mode !== 'guest' || sageNew.model !== 'fake') problems.push('Sage conversation was made with ' + JSON.stringify(sageNew));
  if (news.some((n) => n.mode !== 'guest')) problems.push('the agent was asked for a non-guest conversation');
  note('the agent made Sage a guest conversation with its role and model: ' + JSON.stringify({ mode: sageNew.mode, model: sageNew.model, role: sageNew.role }));
  await csp(owner, 'owner');

  // ---- the owner on a phone (390x844): the Team sheet and a teammate answer
  const ownerPhone = await browser.newContext({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true, deviceScaleFactor: 2 });
  await ownerPhone.addCookies(await desk.cookies());
  const rp = await newPage(ownerPhone, 'owner-phone');
  await rp.goto(base + '/');
  await rp.waitForSelector('.room');
  await rp.tap('.room');
  await rp.waitForSelector('.msg.team >> text=lemon pasta');
  await rp.evaluate(() => { const b = document.getElementById('msgs'); b.scrollTop = b.scrollHeight; });
  await shot(rp, '24-phone-teammate-answer.png');
  await noSideScroll(rp, 'owner phone room');
  await rp.tap('#people');
  await rp.waitForSelector('#sheet .lbl.sec >> text=Team');
  await rp.locator('#sheet .lbl.sec', { hasText: 'Team' }).scrollIntoViewIfNeeded();
  await shot(rp, '25-phone-team-sheet.png');
  await noSideScroll(rp, 'owner phone team sheet');
  await rp.tap('#sheet >> text=Add a teammate');
  await rp.waitForSelector('.team-form');
  await shot(rp, '26-phone-team-form.png');
  await noSideScroll(rp, 'owner phone team form');
  note('phone width 390x844: teammate answer, Team sheet and form have no sideways scroll');
  await csp(rp, 'owner-phone');

  // ---- a member sees the teammates by name and handle, never the role or the model
  await mia.bringToFront();
  await mia.reload();
  await mia.waitForSelector('.room');
  if (!(await mia.isVisible('#chat:not([hidden])'))) await mia.tap('.room');
  await mia.waitForSelector('#chat:not([hidden]) .msg.team');
  await mia.tap('#text');
  await mia.locator('#text').pressSequentially('@', { delay: 40 });
  await mia.waitForSelector('#suggest:not([hidden]) .opt');
  const opts = (await mia.locator('#suggest .opt').allTextContents()).map((t) => t.replace(/\s+/g, ' ').trim());
  const optsShown = (await mia.locator('#suggest .opt').evaluateAll((os) => os.map((o) => [...o.children].map((c) => c.textContent).join(' ')))).join(' | ');
  if (!opts.some((t) => t.startsWith('@sage')) || !opts.some((t) => t.startsWith('@coder'))) problems.push('member autocomplete offered ' + JSON.stringify(opts));
  await shot(mia, '27-phone-member-mentions.png');
  note('Mia sees the teammates in mention autocomplete: ' + opts.join(' | '));
  await mia.tap('#suggest .opt >> text=@sage');
  await mia.locator('#text').pressSequentially('what is good for dessert?', { delay: 10 });
  await mia.tap('#send');
  await mia.waitForFunction(() => [...document.querySelectorAll('.msg.team')].filter((m) => /lemon pasta/.test(m.textContent)).length >= 2, null, { timeout: 15000 });
  note('Mia asked @sage and Sage answered her too');
  await mia.tap('#people');
  await mia.waitForSelector('#sheet .person >> text=Sage');
  if (await mia.locator('#sheet .lbl.sec', { hasText: 'Team' }).count()) problems.push('a member sees the owner\'s Team section');
  const miaPeople = (await mia.textContent('#sheet')).replace(/\s+/g, ' ');
  if (!/@sage · teammate/.test(miaPeople)) problems.push('member People list: ' + miaPeople);
  await shot(mia, '28-phone-member-people.png');
  await mia.keyboard.press('Escape');
  const miaHtml = await mia.content();
  const miaJson = fetched.phone.join('\n');
  for (const secret of ['Cooking coach', 'mock:4b', 'terse coding helper']) {
    if (miaHtml.includes(secret)) problems.push('the member page contains "' + secret + '"');
    if (miaJson.includes(secret)) problems.push('JSON the member page fetched contains "' + secret + '"');
  }
  if (!fetched.owner.join('\n').includes('Cooking coach')) problems.push('the owner JSON never carried the role (the member check proves nothing)');
  for (const route of ['team', 'team/models']) {
    const r = await phone.request.get(base + '/room/' + route);
    if (r.status() !== 403) problems.push('a member got HTTP ' + r.status() + ' from /room/' + route);
  }
  const r2 = await phone.request.post(base + '/room/team/add', { data: { handle: 'evil', name: 'Evil', kind: 'agent', role: 'x' } });
  if (r2.status() === 200) problems.push('a member could add a teammate');
  note('the member page and the JSON it fetched (' + fetched.phone.length + ' responses) hold no role text or model name; the owner routes answer a member with 403');
  await csp(mia, 'phone');

  // ---- reduced motion: the thinking dots stand still
  const still = await browser.newContext({ viewport: { width: 1440, height: 900 }, reducedMotion: 'reduce' });
  await still.addCookies(await desk.cookies());
  const calm = await newPage(still, 'reduced-motion');
  await calm.goto(base + '/');
  await calm.waitForSelector('.room');
  await calm.click('.room');
  await calm.waitForSelector('#chat:not([hidden])');
  await calm.fill('#text', '@tempo what should we cook?');
  await calm.press('#text', 'Enter');
  await calm.waitForSelector('#thinking:not([hidden])');
  const anim = await calm.evaluate(() => getComputedStyle(document.querySelector('.beat i')).animationName);
  if (anim !== 'none') problems.push('reduced motion still animates: ' + anim);
  await shot(calm, '19-reduced-motion.png');
  note('reduced motion: thinking dots animation = ' + anim);
  await calm.waitForSelector('.msg.tempo >> nth=3', { timeout: 15000 });
  await csp(calm, 'reduced-motion');

  // ---- the birthday (a config value), on a second room server
  const md = new Date().toISOString().slice(5, 10);
  const bday = await startRoom(agentPort, { birthday: md });
  const party = await browser.newContext({ viewport: { width: 375, height: 812 }, isMobile: true, hasTouch: true });
  const riley = await newPage(party, 'birthday');
  await riley.goto(bday.owner);
  await riley.waitForFunction(() => /birthday/i.test(document.getElementById('greeting').textContent));
  await shot(riley, '20-phone-birthday.png');
  note('birthday greeting: ' + await riley.textContent('#greeting'));
  await csp(riley, 'birthday');
  await noSideScroll(riley, 'birthday');


  // ---- a DM: a room set to answer every message with exactly one teammate in it
  await owner.bringToFront();
  await owner.click('#new-room');
  await owner.fill('#new-room-name', 'Chef');
  await owner.press('#new-room-name', 'Enter');
  await owner.waitForFunction(() => document.getElementById('chat-name').textContent === 'Chef');
  await owner.click('#people');
  await sheetUp();
  await owner.waitForSelector('#sheet .lbl.sec >> text=Team');
  if ((await toggleOf('Sage').getAttribute('aria-pressed')) !== 'false') problems.push('Sage was already in the new room');
  await toggleOf('Sage').click();
  await owner.waitForFunction(() => [...document.querySelectorAll('#sheet .person button[aria-pressed]')].some((b) => b.textContent === 'In this room'));
  await owner.click('#sheet button:has-text("Every message")');
  await owner.waitForFunction(() => /Chat with Sage/.test(document.getElementById('chat-meta').textContent));
  await owner.keyboard.press('Escape');
  const dmMeta = await owner.textContent('#chat-meta');
  await shot(owner, '29-desktop-dm-empty.png');
  await owner.fill('#text', 'what should we cook?');
  await owner.press('#text', 'Enter');
  await owner.waitForFunction(() => /Sage/.test(document.getElementById('thinking-mark').textContent));
  await owner.waitForSelector('.msg.team >> text=Sage here: try a lemon pasta.', { timeout: 15000 });
  if (await owner.locator('.msg.tempo').count()) problems.push('Tempo answered in the DM room');
  await shot(owner, '30-desktop-dm-answer.png');
  note('DM room "' + dmMeta + '": Sage answered without an @mention');
  await owner.fill('#text', '@tempo is the sky blue?');
  await owner.press('#text', 'Enter');
  await owner.waitForSelector('.msg.tempo >> text=Rayleigh', { timeout: 15000 });
  note('in the DM, @tempo still asks Tempo');
  await csp(owner, 'owner');

  // Tool output never reaches the page.
  for (const p of [owner, mia]) {
    if ((await p.content()).includes('TOOL-OUTPUT-NEVER-SHOWN')) problems.push('tool output reached the page');
  }
  if (!prompts.some((p) => p.startsWith('[Perennos Tempo room #family] Mia: @tempo is the sky blue?'))) problems.push('the request was not wrapped');

  await browser.close();
  room.child.kill();
  bday.child.kill();
  agent.close();
  ollama.close();
  console.log('\nSTEPS ' + steps.length);
  console.log(problems.length ? 'PROBLEMS\n' + problems.join('\n') : 'PROBLEMS none');
  process.exit(problems.length ? 1 : 0);
})().catch((e) => {
  for (const k of kids) k.kill();
  console.error('FAILED', e.message);
  console.log(problems.length ? 'PROBLEMS\n' + problems.join('\n') : '');
  process.exit(2);
});
