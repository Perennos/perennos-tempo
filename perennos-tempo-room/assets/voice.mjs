// Talk to Tempo: a mic in the composer. Speech is heard by whisper.cpp on this computer (the agent
// runs it) and never goes anywhere else. No Web Speech API: Chrome sends that audio to Google.
//   hold the mic to talk, or tap it to start and tap again to stop; Space does the same when the
//   box is empty and focused. MediaRecorder captures, the browser resamples to 16 kHz mono WAV,
//   and the text lands in the box for a moment before it sends (or waits for you, in Settings).
// This file is the same bytes in the chat surface and in Room; each passes its own endpoint and
// look-ups to mountVoice. Pure helpers (WAV, resampling, levels, addressing) are exported for tests.

export const RATE = 16000;
export const MAX_SECONDS = 110;

// ---- pure helpers (tested in tests/voice.test.mjs)

/** Mix any number of channels down to one. */
export function downmix(chs) {
  if (chs.length === 1) return chs[0];
  const out = new Float32Array(chs[0].length);
  for (const c of chs) for (let i = 0; i < out.length; i++) out[i] += c[i] / chs.length;
  return out;
}

/** Resample mono samples. Down: average each window (a plain low-pass). Up: linear. */
export function resample(x, from, to = RATE) {
  if (from === to) return x;
  const ratio = from / to, n = Math.floor(x.length / ratio), out = new Float32Array(n);
  if (ratio > 1) {
    for (let i = 0; i < n; i++) {
      const s = i * ratio, e = Math.min(x.length, s + ratio);
      let a = Math.floor(s), sum = 0, w = 0;
      for (; a < e; a++) {
        const wt = Math.min(a + 1, e) - Math.max(a, s);
        sum += x[a] * wt; w += wt;
      }
      out[i] = w ? sum / w : 0;
    }
  } else {
    for (let i = 0; i < n; i++) {
      const p = i * ratio, a = Math.floor(p), f = p - a;
      out[i] = x[a] * (1 - f) + (x[Math.min(a + 1, x.length - 1)]) * f;
    }
  }
  return out;
}

/** 16-bit PCM mono WAV, as bytes. */
export function encodeWav(x, rate = RATE) {
  const n = x.length, b = new Uint8Array(44 + n * 2), v = new DataView(b.buffer);
  const tag = (o, s) => { for (let i = 0; i < s.length; i++) b[o + i] = s.charCodeAt(i); };
  tag(0, 'RIFF'); v.setUint32(4, 36 + n * 2, true); tag(8, 'WAVE'); tag(12, 'fmt ');
  v.setUint32(16, 16, true); v.setUint16(20, 1, true); v.setUint16(22, 1, true);
  v.setUint32(24, rate, true); v.setUint32(28, rate * 2, true); v.setUint16(32, 2, true); v.setUint16(34, 16, true);
  tag(36, 'data'); v.setUint32(40, n * 2, true);
  for (let i = 0; i < n; i++) {
    const s = Math.max(-1, Math.min(1, x[i]));
    v.setInt16(44 + i * 2, s < 0 ? s * 0x8000 : s * 0x7fff, true);
  }
  return b;
}

/** Loudness 0..1 of a block of samples (root mean square, lifted so quiet speech still shows). */
export function levelOf(x) {
  if (!x.length) return 0;
  let s = 0;
  for (let i = 0; i < x.length; i++) s += x[i] * x[i];
  return Math.min(1, Math.sqrt(s / x.length) * 5);
}

/** Speech into a Room goes to @tempo unless it already names someone ("at sam" becomes @sam). */
export function addressTo(text, { mention = '@tempo', names = ['tempo'] } = {}) {
  let t = (text || '').trim();
  if (!t || !mention) return t;
  const lead = new RegExp('^(?:hey |hi |ok |okay )?at (' + names.map(n => n.replace(/[^\w-]/g, '')).filter(Boolean).join('|') + ')\\b[,:]?\\s*', 'i');
  const m = lead.exec(t);
  if (m) t = '@' + m[1].toLowerCase() + ' ' + t.slice(m[0].length);
  return /(^|\s)@[\w-]+/.test(t) ? t : mention + ' ' + t;
}

/** "Hey Tempo, what's next?" -> {hit: true, rest: "what's next"}. */
export function wakeWord(text) {
  const t = (text || '').toLowerCase().replace(/[^a-z0-9' ]+/g, ' ').replace(/\s+/g, ' ').trim();
  const m = /(?:^| )(?:hey|hi|hay|okay|ok) tempo(?:s)?(?: |$)(.*)$/.exec(t);
  return m ? { hit: true, rest: m[1].trim() } : { hit: false, rest: '' };
}

/** Reply text as something worth hearing: no code, links or markdown marks; one stretch at most. */
export function speakable(text, max = 1400) {
  let t = (text || '').replace(/```[\s\S]*?```/g, ' ').replace(/`([^`]*)`/g, '$1')
    .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1').replace(/\bhttps?:\/\/\S+/g, ' ')
    .replace(/[*_#>|~]+/g, ' ').replace(/\s+/g, ' ').trim();
  if (t.length > max) { const cut = t.slice(0, max), at = Math.max(cut.lastIndexOf('. '), cut.lastIndexOf('? '), cut.lastIndexOf('! ')); t = at > 200 ? cut.slice(0, at + 1) : cut; }
  return t;
}

// ---- words (the host's copy.voice overrides any of these)

const COPY = {
  mic: 'Talk to Tempo', micHint: 'Hold to talk, or tap to start and tap to stop. Space does it too when the box is empty.',
  explain: 'Tempo will ask your browser for the microphone. What you say is heard on this computer and goes nowhere else.',
  go: 'Go ahead', listeningHold: 'Listening. Let go to send.', listeningTap: 'Listening. Tap the mic to finish.',
  transcribing: 'Transcribing on this computer', heard: 'Heard you.', tooShort: 'That was too short to hear. Hold the mic a little longer.',
  nothing: 'Nothing came through. Try again, a bit closer to the mic.', denied: 'The browser said no to the microphone. Allow it for this page in the address bar, then try again.',
  noMic: 'No microphone found on this device.', notSetUp: "Voice isn't set up yet. Run tempo-launch setup --voice.",
  failed: "Voice couldn't hear that. Try again.", settings: 'Voice', sendNow: 'Send when I stop', sendEdit: 'Let me edit first',
  readAloud: 'Read replies aloud', readAloudOff: 'Needs Piper: run tempo-launch setup --voice --piper.', hands: 'Hands-free: say "Hey Tempo"',
  handsNote: 'Off by default. While it is on, this page listens through your microphone, here, and only checks for the two words. Nothing is sent anywhere.',
  wakeOn: 'Listening for "Hey Tempo"', wakeOff: 'Turn off', stopListening: 'Stop', level: 'Mic level', local: 'Stays on this computer.',
};

const SVG_MIC = '<svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true" focusable="false"><path fill="currentColor" d="M12 15a3.5 3.5 0 0 0 3.5-3.5v-6a3.5 3.5 0 0 0-7 0v6A3.5 3.5 0 0 0 12 15Zm6-3.5a1 1 0 1 0-2 0 4 4 0 0 1-8 0 1 1 0 1 0-2 0 6 6 0 0 0 5 5.9V20H9.5a1 1 0 1 0 0 2h5a1 1 0 1 0 0-2H13v-2.6a6 6 0 0 0 5-5.9Z"/></svg>';

const mk = (tag, cls, text) => { const e = document.createElement(tag); if (cls) e.className = cls; if (text != null) e.textContent = text; return e; };
const calm = () => window.matchMedia?.('(prefers-reduced-motion: reduce)').matches;
const sleep = ms => new Promise(r => setTimeout(r, ms));

function pickMime() {
  for (const m of ['audio/webm;codecs=opus', 'audio/webm', 'audio/mp4', 'audio/ogg;codecs=opus'])
    if (globalThis.MediaRecorder?.isTypeSupported?.(m)) return m;
  return '';
}

/** A recording (webm, mp4, ogg) as 16 kHz mono WAV bytes; null when this browser cannot decode it. */
export async function toWav16k(blob) {
  const AC = globalThis.AudioContext || globalThis.webkitAudioContext;
  if (!AC) return null;
  const ctx = new AC();
  try {
    const buf = await ctx.decodeAudioData(await blob.arrayBuffer());
    let mono;
    if (globalThis.OfflineAudioContext) {
      const off = new OfflineAudioContext(1, Math.max(1, Math.ceil(buf.duration * RATE)), RATE);
      const src = off.createBufferSource();
      src.buffer = buf; src.connect(off.destination); src.start();
      mono = (await off.startRendering()).getChannelData(0);
    } else {
      mono = resample(downmix(Array.from({ length: buf.numberOfChannels }, (_, i) => buf.getChannelData(i))), buf.sampleRate);
    }
    return encodeWav(mono);
  } catch { return null; } finally { ctx.close?.(); }
}

/**
 * o: {
 *   comp        the composer element (the mic goes before `anchor`, the status bar before the composer)
 *   anchor      the Send button
 *   input       the textarea
 *   send        () => Promise: sends what is in the box
 *   endpoint    '/agent/voice' or '/room/voice'
 *   headers     () => extra request headers (the agent's key), or {}
 *   query       () => '&room=...' extras for the transcribe call
 *   address     (text) => text; where speech goes (a Room adds @tempo)
 *   led         the insignia's .tt-led, which pulses with your voice
 *   settings    element to put the Voice settings in (optional)
 *   store       {get, set}: small per-device memory
 *   copy        word overrides
 *   canUse      () => bool: hide the mic when this person may not use it
 * }
 */
export function mountVoice(o) {
  const c = { ...COPY, ...(o.copy || {}) };
  const store = o.store || { get: (_, d) => d, set() {} };
  const hdr = () => ({ ...(o.headers ? o.headers() : {}) });
  const cred = { credentials: 'same-origin' };
  const link = mk('link'); link.rel = 'stylesheet'; link.href = new URL('./voice.css', import.meta.url).href;
  document.head.append(link);

  let caps = { listen: false, speak: false }, here = false;
  let state = 'idle', mode = 'hold', rec = null, stream = null, chunks = [], t0 = 0, ctx = null, an = null, buf = null;
  let level = 0, wave = [], raf = 0, auto = false, quiet = 0, heardSpeech = false, spaceDown = false, speaking = null;
  let aud = null, wake = null, destroyed = false;
  const opt = {
    send: store.get('voice:send', 'now'), aloud: store.get('voice:aloud', '0') === '1', hands: false,
  };

  // ---- the mic and its ring
  const ring = mk('canvas', 'vx-ring'); ring.width = ring.height = 72; ring.setAttribute('aria-hidden', 'true');
  const mic = mk('button', 'vx-mic'); mic.type = 'button'; mic.innerHTML = SVG_MIC;
  mic.setAttribute('aria-label', c.mic); mic.title = c.micHint; mic.setAttribute('aria-pressed', 'false');
  const wrap = mk('span', 'vx-wrap'); wrap.append(ring, mic);
  wrap.hidden = true;
  o.anchor ? o.anchor.before(wrap) : o.comp.append(wrap);

  // ---- the status bar: what is happening, a live wave (or a level bar), and the way out
  const dot = mk('i', 'vx-dot'), msg = mk('span', 'vx-msg'), wcan = mk('canvas', 'vx-wave'); wcan.width = 240; wcan.height = 28; wcan.setAttribute('aria-hidden', 'true');
  const meter = mk('span', 'vx-meter'); const meterFill = mk('i'); meter.append(meterFill); meter.setAttribute('role', 'meter'); meter.setAttribute('aria-label', c.level);
  const okBtn = mk('button', 'vx-go', c.go); okBtn.type = 'button'; okBtn.hidden = true;
  const bar = mk('div', 'vx-bar'); bar.hidden = true; bar.setAttribute('role', 'status'); bar.setAttribute('aria-live', 'polite');
  bar.append(dot, msg, wcan, meter, okBtn);
  const chip = mk('div', 'vx-wake'); chip.hidden = true;
  const chipText = mk('span', null, c.wakeOn), chipOff = mk('button', 'vx-wake-off', c.wakeOff); chipOff.type = 'button';
  chip.append(mk('i', 'vx-dot on'), chipText, chipOff);
  o.comp.before(chip, bar);

  const root = () => o.comp.closest('.tempo') || document.documentElement;
  const ledEl = () => o.led || document.querySelector('[data-insignia]');
  function setLed(v) {
    const e = ledEl(); if (!e) return;
    const host = e.closest?.('.tt-ins') || e;
    host.style.setProperty('--vx-lvl', v.toFixed(3));
    host.toggleAttribute('data-listening', state === 'listening' || !!wake?.talking);
  }
  function paint(s, text) {
    state = s;
    bar.dataset.state = s;
    bar.hidden = s === 'idle';
    msg.textContent = text || '';
    mic.setAttribute('aria-pressed', String(s === 'listening'));
    wrap.dataset.state = s;
    wcan.hidden = s !== 'listening' || calm();
    meter.hidden = !(s === 'listening' && calm());
    okBtn.hidden = s !== 'asking';
    if (s !== 'listening') { setLed(0); drawRing(0); }
  }
  const say_ = (text) => { paint('error', text); };
  bar.addEventListener('click', e => { if (state === 'error' && e.target !== okBtn) paint('idle'); });

  // ---- drawing: a ring of bars around the mic, and the last second or two as a scrolling wave
  function colour() { return getComputedStyle(root()).getPropertyValue('--warm').trim() || '#ff5b1f'; }
  function drawRing(l) {
    const g = ring.getContext('2d'), w = ring.width, cx = w / 2, n = 40;
    g.clearRect(0, 0, w, w);
    if (state !== 'listening' || calm()) return;
    g.strokeStyle = colour(); g.lineWidth = 2; g.lineCap = 'round';
    const t = performance.now() / 600;
    for (let i = 0; i < n; i++) {
      const a = (i / n) * Math.PI * 2, wob = 0.5 + 0.5 * Math.sin(a * 3 + t * 2 + i), len = 2 + (l * 11) * (0.35 + 0.65 * wob);
      g.globalAlpha = 0.35 + 0.65 * Math.min(1, l * 1.6);
      g.beginPath(); g.moveTo(cx + Math.cos(a) * 26, cx + Math.sin(a) * 26); g.lineTo(cx + Math.cos(a) * (26 + len), cx + Math.sin(a) * (26 + len)); g.stroke();
    }
    g.globalAlpha = 1;
  }
  function drawWave() {
    const g = wcan.getContext('2d'), w = wcan.width, h = wcan.height, n = 60, bw = w / n;
    g.clearRect(0, 0, w, h);
    g.fillStyle = colour();
    for (let i = 0; i < n; i++) {
      const v = wave[wave.length - n + i] ?? 0, bh = Math.max(2, v * (h - 2));
      g.globalAlpha = 0.3 + 0.7 * (i / n);
      g.fillRect(i * bw + 1, (h - bh) / 2, Math.max(1, bw - 2), bh);
    }
    g.globalAlpha = 1;
  }

  // ---- the level loop (runs only while listening)
  function loop() {
    if (state !== 'listening' || !an) return;
    an.getFloatTimeDomainData(buf);
    const l = levelOf(buf);
    level = Math.max(l, level * 0.85);
    wave.push(level);
    if (wave.length > 400) wave.splice(0, wave.length - 200);
    const secs = (performance.now() - t0) / 1000;
    meterFill.style.width = Math.round(level * 100) + '%';
    meter.setAttribute('aria-valuenow', String(Math.round(level * 100)));
    setLed(level); drawRing(level); if (!calm()) drawWave();
    if (secs > MAX_SECONDS) return stop();
    if (auto) { // hands-free: stop when you stop talking
      if (l > 0.06) { heardSpeech = true; quiet = 0; } else quiet += 1;
      if ((heardSpeech && quiet > 80) || (!heardSpeech && secs > 6)) return stop();
    }
    raf = requestAnimationFrame(loop);
  }

  // ---- recording
  async function begin(how, opts = {}) {
    if (state === 'listening' || state === 'transcribing' || destroyed) return;
    stopSpeaking();
    if (!caps.listen) return say_(c.notSetUp);
    if (!navigator.mediaDevices?.getUserMedia || !globalThis.MediaRecorder) return say_(c.noMic);
    if (!(await mayAsk())) return;
    mode = how; auto = !!opts.auto; quiet = 0; heardSpeech = false; chunks = []; wave = []; level = 0;
    try {
      stream = await navigator.mediaDevices.getUserMedia({ audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true } });
    } catch (e) {
      return say_(e && (e.name === 'NotFoundError' || e.name === 'OverconstrainedError') ? c.noMic : c.denied);
    }
    // the person let go while the browser was asking: nothing to record
    if (opts.cancelled?.()) { stream.getTracks().forEach(t => t.stop()); return paint('idle'); }
    const AC = globalThis.AudioContext || globalThis.webkitAudioContext;
    ctx = new AC(); an = ctx.createAnalyser(); an.fftSize = 1024; buf = new Float32Array(an.fftSize);
    ctx.createMediaStreamSource(stream).connect(an);
    const mime = pickMime();
    rec = new MediaRecorder(stream, mime ? { mimeType: mime } : undefined);
    rec.ondataavailable = e => { if (e.data?.size) chunks.push(e.data); };
    rec.onstop = () => finish(rec.mimeType || mime || 'audio/webm');
    rec.start(250);
    t0 = performance.now();
    paint('listening', how === 'hold' ? c.listeningHold : c.listeningTap);
    raf = requestAnimationFrame(loop);
  }
  function stop() {
    if (state !== 'listening' || !rec) return;
    cancelAnimationFrame(raf);
    try { rec.stop(); } catch { finish(''); }
    stream?.getTracks().forEach(t => t.stop());
    ctx?.close?.(); ctx = an = null;
  }
  function cancel() { // let go too soon: discard
    if (state !== 'listening') return;
    rec.onstop = null; cancelAnimationFrame(raf);
    try { rec.stop(); } catch { /* already stopped */ }
    stream?.getTracks().forEach(t => t.stop()); ctx?.close?.(); ctx = an = null;
    paint('idle');
  }
  async function mayAsk() {
    try {
      const p = await navigator.permissions?.query({ name: 'microphone' });
      if (p?.state === 'granted') return true;
    } catch { /* not every browser can say */ }
    if (store.get('voice:told', '') === '1') return true;
    paint('asking', c.explain);
    const ok = await new Promise(r => { okBtn.onclick = () => r(true); okBtn.focus(); });
    store.set('voice:told', '1');
    paint('idle');
    return ok;
  }

  async function finish(mime) {
    const secs = (performance.now() - t0) / 1000;
    if (secs < 0.5 || !chunks.length) return paint('error', c.tooShort);
    paint('transcribing', c.transcribing);
    const blob = new Blob(chunks, { type: mime.split(';')[0] });
    chunks = [];
    try {
      const wav = await toWav16k(blob);
      const body = wav ? new Blob([wav], { type: 'audio/wav' }) : blob; // no decoder: the agent converts if it can
      const text = await transcribe(body);
      if (!text) return paint('error', c.nothing);
      land(text);
    } catch (e) { paint('error', e.message || c.failed); }
  }

  async function transcribe(body, lang = '') {
    const r = await fetch(o.endpoint + '/transcribe?lang=' + encodeURIComponent(lang) + (o.query ? o.query() : ''), {
      method: 'POST', ...cred, headers: { 'Content-Type': body.type || 'audio/wav', ...hdr() }, body,
    });
    const j = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(j.error || c.failed);
    return (j.text || '').trim();
  }

  // the words land in the box, stay a moment where you can see them, then go (or wait for you)
  async function land(text) {
    const said = o.address ? o.address(text) : text;
    const was = o.input.value.trim();
    const next = was ? was + ' ' + said : said;
    o.input.value = next;
    o.input.dispatchEvent(new Event('input', { bubbles: true }));
    o.input.classList.add('vx-landed');
    paint('idle');
    if (opt.send !== 'now') { o.input.focus(); setTimeout(() => o.input.classList.remove('vx-landed'), 1200); return; }
    paint('transcribing', c.heard);
    await sleep(calm() ? 500 : 900);
    o.input.classList.remove('vx-landed');
    paint('idle');
    if (o.input.value === next) await o.send(); // you typed over it: leave it alone
  }

  // ---- pointer: hold to talk, or tap to start and tap to stop
  let down = 0, startedByDown = false, released = false;
  mic.addEventListener('contextmenu', e => e.preventDefault());
  mic.addEventListener('pointerdown', async e => {
    if (e.button > 0) return;
    e.preventDefault();
    mic.setPointerCapture?.(e.pointerId);
    if (state === 'listening') { stop(); startedByDown = false; return; }
    if (state === 'asking') return;
    down = performance.now(); released = false; startedByDown = true;
    await begin('hold', { cancelled: () => released });
  });
  const up = () => {
    if (!startedByDown) return;
    startedByDown = false; released = true;
    if (state !== 'listening') return;
    if (performance.now() - down > 450) stop(); // held: let go sends
    else { mode = 'tap'; msg.textContent = c.listeningTap; }
  };
  mic.addEventListener('pointerup', up);
  mic.addEventListener('pointercancel', up);
  mic.addEventListener('keydown', e => { // keyboard: Enter or Space toggles
    if (e.key !== 'Enter' && e.key !== ' ') return;
    e.preventDefault();
    if (e.repeat) return;
    if (state === 'listening') stop(); else begin('tap');
  });

  // ---- space-bar push-to-talk: only with the box empty and focused
  const onKeyDown = e => {
    if (e.key !== ' ' || e.ctrlKey || e.metaKey || e.altKey || e.isComposing) return;
    if (document.activeElement !== o.input || o.input.value !== '' || !here || !caps.listen) return;
    e.preventDefault();
    if (e.repeat || spaceDown) return;
    spaceDown = true; down = performance.now(); released = false;
    begin('hold', { cancelled: () => released });
  };
  const onKeyUp = e => {
    if (e.key !== ' ' || !spaceDown) return;
    spaceDown = false; released = true;
    if (state !== 'listening') return;
    if (performance.now() - down > 300) stop(); else cancel(); // a tap on Space is not a request
  };
  document.addEventListener('keydown', onKeyDown);
  document.addEventListener('keyup', onKeyUp);

  // ---- reading replies aloud (Piper, on this computer)
  let player = null;
  function stopSpeaking() { try { aud?.stop(); } catch { /* not playing */ } aud = null; speaking = null; }
  async function say(text) {
    if (!opt.aloud || !caps.speak || destroyed) return;
    const t = speakable(text);
    if (!t) return;
    stopSpeaking();
    const mine = speaking = Symbol('say');
    try {
      const r = await fetch(o.endpoint + '/speak', { method: 'POST', ...cred, headers: { 'Content-Type': 'application/json', ...hdr() }, body: JSON.stringify({ text: t }) });
      if (!r.ok || speaking !== mine) return;
      const AC = globalThis.AudioContext || globalThis.webkitAudioContext;
      player ??= new AC();
      const b = await player.decodeAudioData(await r.arrayBuffer());
      if (speaking !== mine) return;
      const s = player.createBufferSource(); s.buffer = b; s.connect(player.destination);
      aud = s; s.onended = () => { if (aud === s) aud = null; };
      s.start();
    } catch { /* a reply that cannot be read aloud is still a reply */ }
  }

  // ---- hands-free: "Hey Tempo", heard here, 2-second windows, nothing sent anywhere
  async function wakeStart() {
    if (wake || destroyed) return;
    let s;
    try { s = await navigator.mediaDevices.getUserMedia({ audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true } }); }
    catch { opt.hands = false; syncOpts(); return say_(c.denied); }
    const AC = globalThis.AudioContext || globalThis.webkitAudioContext;
    const x = new AC(), a = x.createAnalyser(); a.fftSize = 2048;
    x.createMediaStreamSource(s).connect(a);
    const sr = x.sampleRate, ringBuf = new Float32Array(Math.floor(sr * 3)), blk = new Float32Array(a.fftSize);
    const w = wake = { s, x, talking: false, busy: false, floor: 0.01, at: 0, start: 0, quiet: 0, filled: 0 };
    w.timer = setInterval(() => {
      if (state !== 'idle' || aud || w.busy) { w.talking = false; return; }
      a.getFloatTimeDomainData(blk);
      ringBuf.copyWithin(0, blk.length); ringBuf.set(blk, ringBuf.length - blk.length); w.filled = Math.min(ringBuf.length, w.filled + blk.length);
      let sum = 0; for (let i = 0; i < blk.length; i++) sum += blk[i] * blk[i];
      const rms = Math.sqrt(sum / blk.length), gate = Math.max(0.02, w.floor * 3.5), now = performance.now();
      if (!w.talking) {
        w.floor = w.floor * 0.97 + rms * 0.03;
        if (rms > gate) { w.talking = true; w.start = now; w.quiet = 0; }
      } else {
        w.quiet = rms < gate * 0.6 ? w.quiet + 1 : 0;
        if (w.quiet >= 10 || now - w.start > 2200) {
          w.talking = false;
          const len = Math.min(w.filled, Math.floor(sr * Math.min(2.4, (now - w.start) / 1000 + 0.4)));
          if (now - w.start > 350) check(w, ringBuf.slice(ringBuf.length - len), sr);
        }
      }
      setLed(w.talking ? Math.min(1, rms * 5) : 0);
    }, 40);
    chip.hidden = false;
  }
  async function check(w, samples, sr) {
    w.busy = true;
    try {
      const text = await transcribe(new Blob([encodeWav(resample(samples, sr))], { type: 'audio/wav' }), 'en');
      const m = wakeWord(text);
      if (!m.hit || !wake) return;
      if (m.rest.length > 2) { chip.dataset.heard = '1'; await land(m.rest); }
      else await begin('tap', { auto: true });
    } catch { /* a window that could not be heard is skipped */ } finally { w.busy = false; }
  }
  function wakeStop() {
    if (!wake) return;
    clearInterval(wake.timer); wake.s.getTracks().forEach(t => t.stop()); wake.x.close?.();
    wake = null; chip.hidden = true; setLed(0);
  }
  chipOff.addEventListener('click', () => { opt.hands = false; store.set('voice:hands', '0'); syncOpts(); wakeStop(); });

  // ---- Settings: how it sends, reading aloud, hands-free
  const sec = mk('div', 'vx-set');
  const sw = (label, get, set, note) => {
    const b = mk('button', 'vx-sw', label); b.type = 'button'; b.setAttribute('role', 'switch');
    b.addEventListener('click', () => { if (!b.disabled) set(!get()); });
    return { b, note: note ? mk('p', 'vx-note', note) : null, get };
  };
  const seg = mk('div', 'vx-seg'); seg.setAttribute('role', 'group'); seg.setAttribute('aria-label', c.settings);
  const sNow = mk('button', null, c.sendNow), sEdit = mk('button', null, c.sendEdit);
  for (const b of [sNow, sEdit]) b.type = 'button';
  sNow.onclick = () => { opt.send = 'now'; store.set('voice:send', 'now'); syncOpts(); };
  sEdit.onclick = () => { opt.send = 'edit'; store.set('voice:send', 'edit'); syncOpts(); };
  seg.append(sNow, sEdit);
  const aloud = sw(c.readAloud, () => opt.aloud, v => { opt.aloud = v; store.set('voice:aloud', v ? '1' : '0'); if (!v) stopSpeaking(); syncOpts(); });
  const hands = sw(c.hands, () => opt.hands, v => { opt.hands = v; syncOpts(); v ? wakeStart() : wakeStop(); }, c.handsNote);
  const head = mk('span', 'vx-lbl', c.settings + ' · ' + c.local);
  sec.append(head, seg, aloud.b, aloud.note || '', hands.b, hands.note);
  function syncOpts() {
    sNow.setAttribute('aria-pressed', String(opt.send === 'now')); sEdit.setAttribute('aria-pressed', String(opt.send !== 'now'));
    aloud.b.setAttribute('aria-checked', String(opt.aloud)); aloud.b.disabled = !caps.speak;
    aloud.b.title = caps.speak ? '' : c.readAloudOff;
    hands.b.setAttribute('aria-checked', String(opt.hands)); hands.b.disabled = !caps.listen;
  }
  if (o.settings) o.settings.append(sec);
  syncOpts();

  // ---- what this machine can do, and whether this person may
  async function refresh() {
    here = !!(o.canUse ? o.canUse() : true);
    try {
      const r = await fetch(o.endpoint + '?x=1' + (o.query ? o.query() : ''), { ...cred, headers: hdr() });
      if (!r.ok) throw new Error('no');
      caps = await r.json();
      here = here && caps.allowed !== false;
      o.onStatus?.(caps);
    } catch { here = false; }
    wrap.hidden = !here;
    mic.dataset.ready = String(!!caps.listen);
    mic.title = caps.listen ? c.micHint : c.notSetUp;
    sec.hidden = !here;
    syncOpts();
  }
  refresh();

  const onHide = () => wakeStop();
  addEventListener('pagehide', onHide);
  return {
    say, refresh, settings: sec, isOn: () => state,
    unmount() {
      destroyed = true; cancel(); wakeStop(); stopSpeaking(); cancelAnimationFrame(raf);
      document.removeEventListener('keydown', onKeyDown); document.removeEventListener('keyup', onKeyUp); removeEventListener('pagehide', onHide);
      wrap.remove(); bar.remove(); chip.remove(); sec.remove(); link.remove();
    },
  };
}
