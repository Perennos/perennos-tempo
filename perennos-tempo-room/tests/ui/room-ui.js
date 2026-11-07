// Drive the Perennos Tempo Room app in Chromium (Playwright) against a fake agent and the real binary.
// usage: node crates/perennos-tempo-room/tests/ui/room-ui.js <path to tempo-room> <screenshot folder>
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
const note = (s) => { steps.push(s); console.log('ok  ' + s); };

// ---------------------------------------------------------------- the fake agent
const convs = new Map();
let nconv = 0;
const prompts = [];
function push(c, ev) { ev.n = c.events.length; ev.t = new Date().toISOString(); c.events.push(ev); }
function answer(p) {
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
    if (req.method === 'GET' && url.pathname === '/agent/health') return send(200, { net: true, models: ['fake'], stream: true });
    if (req.method === 'POST' && url.pathname === '/agent/new') { const id = 'conv-' + (++nconv); convs.set(id, { events: [], busy: false }); return send(200, { conv: id }); }
    if (req.method === 'POST' && url.pathname === '/agent/send') {
      const c = convs.get(j.conv);
      if (!c) return send(404, { error: 'unknown conversation' });
      if (c.busy) return send(409, { error: 'busy' });
      c.busy = true;
      push(c, { type: 'user', text: j.text });
      prompts.push(j.text);
      setTimeout(() => {
        push(c, { type: 'tool_result', name: 'read_file', text: 'TOOL-OUTPUT-NEVER-SHOWN' });
        push(c, { type: 'assistant', text: answer(j.text) });
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

function startRoom(agentPort, extraCfg) {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'tempo-room-ui-'));
  fs.writeFileSync(path.join(data, 'config.json'), JSON.stringify(Object.assign({ owner_name: 'Riley' }, extraCfg)));
  const child = spawn(BIN, ['serve', '--port', '0', '--data', data, '--agent', 'http://127.0.0.1:' + agentPort], { stdio: ['ignore', 'ignore', 'pipe'] });
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
  const room = await startRoom(agentPort, {});
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
  note('Tempo answered in the thread: ' + (await owner.textContent('.msg.tempo .quote')).trim());
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

  // Tool output never reaches the page.
  for (const p of [owner, mia]) {
    if ((await p.content()).includes('TOOL-OUTPUT-NEVER-SHOWN')) problems.push('tool output reached the page');
  }
  if (!prompts.some((p) => p.startsWith('[Perennos Tempo room #family] Mia: @tempo is the sky blue?'))) problems.push('the request was not wrapped');

  await browser.close();
  room.child.kill();
  bday.child.kill();
  agent.close();
  console.log('\nSTEPS ' + steps.length);
  console.log(problems.length ? 'PROBLEMS\n' + problems.join('\n') : 'PROBLEMS none');
  process.exit(problems.length ? 1 : 0);
})().catch((e) => {
  console.error('FAILED', e.message);
  console.log(problems.length ? 'PROBLEMS\n' + problems.join('\n') : '');
  process.exit(2);
});
