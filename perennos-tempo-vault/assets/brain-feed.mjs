// Keeps the second brain current without being asked: polls a loader, and hands the world new data only
// when the picture of memory actually changed. Slower while the tab is hidden, faster while the agent is
// thinking (nodes used in the turn show up within a couple of seconds), and backs off when the agent is down.
// No DOM here beyond what the caller passes in, so it runs under node:test with fake timers.
import { hashStr } from './brain-sim.mjs';

// A cheap fingerprint of a raw graph: nodes (source, kind, size, used, mtime) and edges.
export function graphSignature(raw) {
  const nodes = (raw && raw.nodes) || [], edges = (raw && raw.edges) || [];
  let s = nodes.length + ':' + edges.length + ';';
  for (const n of nodes) s += `${n.src ?? n.id}|${n.kind}|${n.chunks}|${n.used ? 1 : 0}|${n.mtime ?? ''};`;
  for (const e of edges) s += `${e.a}-${e.b}-${e.w};`;
  return hashStr(s).toString(36) + ':' + s.length;
}

// o: {load(): Promise<raw>, onData(raw), onError?(err), interval, fast, hidden, isHidden?, timers?}
export function createFeed(o) {
  const interval = o.interval ?? 8000, fast = o.fast ?? 2500, hiddenEvery = o.hidden ?? 60000;
  const set = o.timers?.set ?? ((f, ms) => setTimeout(f, ms)), clear = o.timers?.clear ?? (id => clearTimeout(id));
  const isHidden = o.isHidden ?? (() => typeof document !== 'undefined' && document.hidden);
  let timer = null, running = false, inflight = false, again = false, quick = false, fails = 0, last = null;

  const delay = () => {
    if (fails) return Math.min(30000, interval * 2 ** fails);
    if (isHidden()) return hiddenEvery;
    return quick ? fast : interval;
  };
  async function run() {
    timer = null;
    if (!running) return;
    inflight = true;
    try {
      const raw = await o.load();
      fails = 0;
      const sig = graphSignature(raw);
      if (sig !== last) { last = sig; if (running) o.onData(raw, sig); }
    } catch (err) {
      fails++;
      o.onError?.(err);
    }
    inflight = false;
    if (!running) return;
    if (again) { again = false; return run(); }
    timer = set(run, delay());
  }
  return {
    // `first: false` waits one interval before the first look (the caller already has a picture: see prime).
    start(first = true) { if (running) return; running = true; if (first) run(); else timer = set(run, delay()); },
    prime(raw) { last = graphSignature(raw); },
    stop() { running = false; if (timer != null) clear(timer); timer = null; },
    // Look now (a message arrived, a note changed): joins a load already in flight instead of doubling it.
    poke() {
      if (!running) return;
      if (inflight) { again = true; return; }
      if (timer != null) clear(timer);
      run();
    },
    setFast(b) { quick = !!b; },
    get running() { return running; },
  };
}
