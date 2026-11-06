"""Perennos Tempo local superagent v1: the harness. Ollama is only the engine.
Chat web UI + agent loop + MCP host + RAG + delegate + workflow memory + safety.
Stdlib only. Everything local; the network is used only by fetch_url when it is up."""
import http.client, json, os, re, sys, time, threading, subprocess, socket, urllib.request, urllib.error, uuid, math, glob, html
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
from urllib.parse import urlparse, parse_qs
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import mcphost

# Run-time data (conversations, workflows, logs, rag-index.json) lives in TEMPO_AGENT_HOME, default this folder.
# Machine-specific settings (folders, hosts, memory files, owner) live in agent-config.json there, never in git.
# See agent-config.example.json for every key.
HOME = os.path.expanduser(os.environ.get("TEMPO_AGENT_HOME") or os.path.dirname(os.path.abspath(__file__)))
CFG_PATH = os.environ.get("TEMPO_AGENT_CONFIG") or os.path.join(HOME, "agent-config.json")
try:
    with open(CFG_PATH, encoding="utf-8") as _f:
        CFG = json.load(_f)
except FileNotFoundError:
    CFG = {}
_p = os.path.expanduser
GUI_PORT = int(os.environ.get("TEMPO_GUI_PORT", "18795"))
CONVS, WFS = os.path.join(HOME, "conversations"), os.path.join(HOME, "workflows")
for d in (CONVS, WFS, os.path.join(HOME, "logs")):
    os.makedirs(d, exist_ok=True)

OLLAMA = os.environ.get("OLLAMA_URL") or os.environ.get("TEMPO_OLLAMA") or CFG.get("ollama_url") or "http://127.0.0.1:11434"
MODELS = CFG.get("heavy_models") or ["uncensored-qwen3.8-27b-8k", "local-qwen3", "local-gpt-oss-32k"]  # 5 Nov: one fixed 8K alias so callers stop reloading each other  # fallback order
NUM_CTX = {"uncensored-qwen3.8-27b-8k": 8192, "uncensored-qwen3.8-27b": 8192, "local-gpt-oss-32k": 16384, "local-qwen3": 8192, **(CFG.get("num_ctx") or {})}  # 5 Nov: 27B at 16-32K spilled 12% onto CPU on a 16 GB GPU and timed out; 8K keeps it all on GPU
EMBED_MODEL = CFG.get("embed_model") or "embeddinggemma"
MAX_STEPS, TOOL_CAP, RESULT_CAP, HIST_CHARS = 40, 40, 6000, 16000  # 5 Nov: HIST 16000 chars (~4K tokens) so an 8K window never truncates
FAST_MODELS = CFG.get("fast_models") or ["local-qwen3", "uncensored-qwen3.8-27b-8k"]  # fast lane: Qwen3.5-9B fits fully on the GPU; heavy lane = MODELS
RAG_K, RAG_CHARS = 4, 700  # 5 Nov: cap memory in the prompt so prompt processing stays short
WRITE_ROOTS = [_p(r) for r in CFG.get("write_roots") or [HOME]]
SCRATCH = _p(CFG.get("scratch_dir") or os.path.join(HOME, "scratch"))
GPU_LEASE = _p(CFG.get("gpu_lease_script") or "")  # optional GPU lease helper used by comfy_image
COMFY_OUT = _p(CFG.get("comfy_output_dir") or "")
OWNER = CFG.get("owner") or "the owner"
OWNER_NOTE = CFG.get("owner_note") or ""
if CFG.get("claude_json") and not os.environ.get("TEMPO_CLAUDE_JSON"):
    mcphost.CLAUDE_JSON = _p(CFG["claude_json"])
if CFG.get("mcp_project_dirs") and not os.environ.get("TEMPO_MCP_PROJECT_DIRS"):
    mcphost.PROJECT_DIRS = [_p(d).replace("\\", "/").rstrip("/") for d in CFG["mcp_project_dirs"]]
SECRET_PATH = re.compile(r"(\.claude\.json|\.token$|\.key$|\.env|secret|credential|executor-token|password|id_rsa|\.pem$)", re.I)
SECRET_TXT = [(re.compile(r"(?i)((?:api[_-]?key|token|secret|password|passwd|authorization)['\"]?\s*[:=]\s*['\"]?)[^\s'\",;]{6,}"), r"\1[REDACTED]"),
              (re.compile(r"\b(sk-[A-Za-z0-9_-]{20,}|(?:sk|rk|pk)_(?:live|test)_[A-Za-z0-9]{16,}|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|"
                          r"xox[abprs]-[A-Za-z0-9-]{10,}|hf_[A-Za-z0-9]{20,}|AIza[0-9A-Za-z_-]{30,}|AKIA[0-9A-Z]{16}\b|"
                          r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,})"), "[REDACTED]")]
# Run first: whole PEM private-key blocks (even cut off) and user:password@ in URLs.
SECRET_PRE = [(re.compile(r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|$)"), "[REDACTED PRIVATE KEY]"),
              (re.compile(r"(?i)\b([a-z][a-z0-9+.-]*://)[^/\s:@]+:[^/\s@]+@"), r"\1[REDACTED]@")]

# Hosts come from agent-config.json "hosts": {"name": {"urls": [LAN url first, then a VPN url], "models": [house model] or null}}.
# Protection: when "models" is set, only that house model may be requested there, so a delegate call can never
# evict it; keep_alive is never set remotely. This machine is always available as LOCAL_HOST.
LOCAL_HOST = CFG.get("local_host") or "local"
HOSTS = {LOCAL_HOST: {"urls": [OLLAMA], "models": None}, **(CFG.get("hosts") or {})}
HOST_OVERRIDE = {}  # tests: {"mockhost": {"urls": [...], "models": None}}


def redact(s):
    for rx, rep in SECRET_PRE + SECRET_TXT:
        s = rx.sub(rep, s)
    return s


def cap(s, n=RESULT_CAP):
    s = redact(str(s))
    return s if len(s) <= n else s[:n] + f"\n...[truncated {len(s) - n} chars]"


def now():
    return time.strftime("%Y-%m-%d %H:%M:%S")


def http_json(url, body=None, timeout=600):
    req = urllib.request.Request(url, data=None if body is None else json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def net_up(_c={"t": 0, "v": False}):
    if os.environ.get("TEMPO_OFFLINE"):  # simulate unplugged network for tests
        return False
    if time.time() - _c["t"] < 20:
        return _c["v"]
    try:
        socket.create_connection(("1.1.1.1", 443), 1.5).close()
        v = True
    except OSError:
        v = False
    _c.update(t=time.time(), v=v)
    return v


# ---------------------------------------------------------------- RAG (local, embeddinggemma)
# Memory sources, all from agent-config.json: core files, [glob, newest N] pairs, an optional JSONL knowledge pack,
# and [file, tail lines (0 = whole file)] pairs that lead "what am I working on" answers.
RAG_FILES = [_p(f) for f in CFG.get("rag_files") or []]
RAG_GLOBS = [(_p(g), int(n)) for g, n in CFG.get("rag_globs") or []]  # newest N
S16_PACK = _p(CFG.get("knowledge_pack") or "")
CURRENT_FILES = [(_p(f), int(t)) for f, t in CFG.get("current_files") or []]
INDEX = os.path.join(HOME, "rag-index.json")
_rag = {"docs": None, "lock": threading.Lock(), "embed_ok": None}


def rag_sources():
    files = list(RAG_FILES)
    for g, n in RAG_GLOBS:
        fs = sorted(glob.glob(g), key=os.path.getmtime, reverse=True)[:n]
        files += fs
    return [f for f in files if os.path.isfile(f)]


def chunks_of(path):
    txt = open(path, encoding="utf-8", errors="replace").read()
    out, cur = [], ""
    for para in re.split(r"\n\s*\n", txt):
        if len(cur) + len(para) > 1100 and cur:
            out.append(cur.strip())
            cur = ""
        cur += para + "\n\n"
        while len(cur) > 1800:
            out.append(cur[:1500])
            cur = cur[1500:]
    if cur.strip():
        out.append(cur.strip())
    return out


def embed(texts):
    try:
        r = http_json(OLLAMA + "/api/embed", {"model": EMBED_MODEL, "input": texts, "keep_alive": "30m"}, 120)
        _rag["embed_ok"] = True
        return r["embeddings"]
    except Exception:
        _rag["embed_ok"] = False
        return None


def build_index(force=False):
    with _rag["lock"]:
        old = {}
        if os.path.exists(INDEX) and not force:
            try:
                old = {d["src"]: d for d in json.load(open(INDEX, encoding="utf-8"))["files"]}
            except Exception:
                old = {}
        files, srcs = [], rag_sources()
        for p in srcs:
            m = os.path.getmtime(p)
            o = old.get(p)
            if o and o["mtime"] == m and o.get("vecs"):
                files.append(o)
                continue
            cs = chunks_of(p)
            vecs = []
            for i in range(0, len(cs), 16):
                v = embed(cs[i:i + 16])
                if v is None:
                    vecs = None
                    break
                vecs += v
            files.append({"src": p, "mtime": m, "chunks": cs, "vecs": vecs})
        if os.path.isfile(S16_PACK):
            m = os.path.getmtime(S16_PACK)
            o = old.get(S16_PACK)
            if o and o["mtime"] == m:
                files.append(o)
            else:
                cs = []
                for line in open(S16_PACK, encoding="utf-8"):
                    try:
                        r = json.loads(line)
                        cs.append(str(r.get("title", "")) + ": " + str(r.get("text") or r.get("content") or r.get("body") or json.dumps(r))[:1500])
                    except Exception:
                        pass
                vecs = []
                for i in range(0, len(cs), 16):
                    v = embed(cs[i:i + 16])
                    if v is None:
                        vecs = None
                        break
                    vecs += v
                files.append({"src": S16_PACK, "mtime": m, "chunks": cs, "vecs": vecs})
        json.dump({"files": files, "built": now()}, open(INDEX, "w", encoding="utf-8"))
        _rag["docs"] = files
        return files


def tok(s):
    return set(w for w in re.findall(r"[a-z0-9]{3,}", s.lower()))


def cos(a, b):
    d = sum(x * y for x, y in zip(a, b))
    return d / (math.sqrt(sum(x * x for x in a)) * math.sqrt(sum(y * y for y in b)) + 1e-9)


CURRENT_RX = re.compile(r"(?i)(working on|work on|current work|doing right now|what.?s (next|going on|on)|status|overnight|morning|today|this (week|morning)|where (are|were) we|left off)")


DEFN_RX = re.compile(r"(?i)^\s*(what|who)('s| is| are)|tell me about|explain")


def rag_search(q, k=6):
    if _rag["docs"] is None:
        try:
            _rag["docs"] = json.load(open(INDEX, encoding="utf-8"))["files"]
        except Exception:
            build_index()
    qv = None
    if any(d.get("vecs") for d in _rag["docs"]):
        e = embed([q])
        qv = e[0] if e else None
    qt = tok(q)
    stop = {"what", "the", "and", "for", "are", "how", "you", "that", "this", "with", "who", "does"}
    qt_h = qt - stop
    scored = []
    t_now = time.time()
    for d in _rag["docs"]:
        src = d["src"]
        ismem = os.sep + "memory" + os.sep in src
        head = 0.0
        if ismem and d["chunks"]:  # 5 Nov: rank by title + description line before the body
            m = re.search(r'(?m)^name:\s*(.+)$', d["chunks"][0]); n = m.group(1) if m else os.path.basename(src)
            m = re.search(r'(?m)^description:\s*(.+)$', d["chunks"][0]); desc = m.group(1) if m else ""
            ht = tok(n.replace("-", " ") + " " + desc)
            head = len(qt_h & ht) / (len(qt_h) + 1) + 0.6 * len(qt_h & tok(n.replace('-', ' '))) / (len(qt_h) + 1)  # title hits count extra
        age_d = (t_now - d.get("mtime", t_now)) / 86400
        rec = 0.08 * max(0.0, 1 - age_d / 14) if ismem or "continuity" in src else 0.0  # recency boost, fades over 2 weeks
        for i, c in enumerate(d["chunks"]):
            kw = len(qt & tok(c)) / (len(qt) + 1)
            sem = cos(qv, d["vecs"][i]) if qv and d.get("vecs") and i < len(d["vecs"]) else 0
            core = 0.15 if (src in RAG_FILES and DEFN_RX.search(q)) else 0.0  # definitional questions: the owner's core files first
            scored.append((sem + 0.6 * kw + 1.2 * head + rec + core + 0.5 * (kw if core else 0), src, i, c))
    scored.sort(key=lambda x: -x[0])
    out = []
    if CURRENT_RX.search(q):  # "what am I working on": always lead with the live work files
        for p, tail in CURRENT_FILES:
            try:
                ls = open(p, encoding="utf-8", errors="replace").read().splitlines()
                out.append({"score": 9.0, "src": p, "chunk": -1, "text": "\n".join(ls[-tail:] if tail else ls)[-3500:]})
            except Exception:
                pass
    pinned = {o["src"] for o in out}
    out += [{"score": round(s, 3), "src": src, "chunk": i, "text": c} for s, src, i, c in scored if src not in pinned][:(3 if out else k)]
    return out


# ---------------------------------------------------------------- second brain views (read-only, indexed files only)
GRAPH_STOP = set("""that this with from have will your what when they there their which about would should could into also been
were than then them only some more most other such each here just like make made does done over very after before because where
while these those name description type true false none null http https file files using used need needs want first last next
still even much many well back down same both every until within without across between""".split())
_graph = {"key": None, "val": None}


def mem_docs():
    if _rag["docs"] is None:
        try:
            _rag["docs"] = json.load(open(INDEX, encoding="utf-8"))["files"]
        except Exception:
            return []
    return _rag["docs"]


def mem_kind(src):
    if src == S16_PACK:
        return "pack"
    if src in RAG_FILES:
        return "core"
    if any(src == p for p, _ in CURRENT_FILES):
        return "current"
    return "memory"


def memory_files():
    return [{"src": d["src"], "name": os.path.basename(d["src"]), "kind": mem_kind(d["src"]),
             "chunks": len(d.get("chunks") or []), "mtime": d.get("mtime", 0)} for d in mem_docs()]


def memory_file(src):
    """Excerpts of one indexed file (never an arbitrary path), redacted and capped."""
    for d in mem_docs():
        if d["src"] == src:
            cs = d.get("chunks") or []
            return {"src": src, "name": os.path.basename(src), "kind": mem_kind(src), "total": len(cs), "chunks": [cap(c, 1200) for c in cs[:6]]}
    return None


def memory_graph(max_nodes=400, terms=12, per_node=4):
    """Files as nodes; an edge joins two files that share at least two of their most distinctive terms (tf-idf).
    w is the share of those top terms in common, 0..1."""
    docs = mem_docs()[:max_nodes]
    key = tuple((d["src"], d.get("mtime")) for d in docs)
    if _graph["key"] == key:
        return _graph["val"]
    tfs, df = [], {}
    for d in docs:
        cnt = {}
        for c in d.get("chunks") or []:
            for w in re.findall(r"[a-z][a-z0-9]{3,}", c.lower()):
                if w not in GRAPH_STOP:
                    cnt[w] = cnt.get(w, 0) + 1
        tfs.append(cnt)
        for w in cnt:
            df[w] = df.get(w, 0) + 1
    n = len(docs)
    tops = [set(sorted(cnt, key=lambda w: (-cnt[w] * math.log((1 + n) / (1 + df[w])), w))[:terms]) for cnt in tfs]
    cand = [[] for _ in docs]
    for i in range(n):
        for j in range(i + 1, n):
            s = len(tops[i] & tops[j])
            if s >= 2:
                cand[i].append((s, j))
                cand[j].append((s, i))
    edges = {}
    for i, lst in enumerate(cand):
        for s, j in sorted(lst, key=lambda x: (-x[0], x[1]))[:per_node]:
            edges[(min(i, j), max(i, j))] = s
    val = {"nodes": [{"id": i, "src": d["src"], "name": os.path.basename(d["src"]), "kind": mem_kind(d["src"]),
                      "chunks": len(d.get("chunks") or [])} for i, d in enumerate(docs)],
           "edges": [{"a": a, "b": b, "w": round(s / terms, 3)} for (a, b), s in sorted(edges.items())]}
    _graph.update(key=key, val=val)
    return val


# ---------------------------------------------------------------- safety helpers
def in_roots(path):
    p = os.path.abspath(path).lower()
    return any(p == r.lower() or p.startswith(r.lower() + os.sep) for r in WRITE_ROOTS)


DELETE_RX = re.compile(r"(?i)(^|[\s;|&(])(rm|del|erase|rmdir|rd|Remove-Item|ri|shred|unlink)(\s|$)")
PROTECTED = CFG.get("protected_names") or []  # services/models run_command may never stop, kill or unload
PROTECT_RX = re.compile("(?i)(" + "|".join(map(re.escape, PROTECTED)) + ")") if PROTECTED else None
COMFY_RX = re.compile(r"(?i)comfy(\.exe)?\"?\s+(launch|start|stop|run)|ComfyUI[\\/]main\.py")
CONFIRM_RX = re.compile(r"(?i)(>|\bSet-Content|\bOut-File|\bAdd-Content|\bNew-Item|\bMove-Item|\bCopy-Item|\bmv\b|\bcp\b|\bmkdir\b|"
                        r"\bInvoke-(WebRequest|RestMethod|Expression)|\bcurl\b|\bwget\b|\bpip\b|\bnpm\b|\bwinget\b|\bStart-Process|"
                        r"\bStop-Process|\btaskkill|\bshutdown|\bssh\b|\bscp\b|\bdocker\b|\bsystemctl\b|\bgit\s+(reset|clean|push|checkout)|"
                        r"\bollama\s+(rm|stop|pull|create)|\breg\s+(add|delete)|\bformat\b|\bSet-ExecutionPolicy|\bnetsh\b|\bsc(\.exe)?\s+(stop|delete))")
BLOCKED_MCP = {  # replaced by guarded builtins or unsafe offline/spending
    "perennos_tempo": {"terminal", "execute_code", "process", "write_file", "patch", "read_file", "search_files", "cronjob",
                       "browser_cdp", "ha_call_service", "delegate_task", "tempo_run", "tempo_submit", "tempo_improve_run",
                       "tempo_waitlist_cancel", "tempo_hold", "tempo_gpu_lease", "tempo_book", "tempo_finance_execution_contract"},
    "comfy-media": {"launch_comfyui", "stop_comfyui", "restart_comfyui", "update_comfyui", "switch_comfyui_version", "install_node",
                    "download_model", "partner_generate", "emit_partner_workflow", "auth_login", "generate_image", "run_template"},
    "playwright-isolated": {"browser_run_code_unsafe", "browser_evaluate", "browser_file_upload"},
}
CONFIRM_MCP = {("comfy-media", "run_workflow"), ("perennos_tempo", "memory"), ("perennos_tempo", "web_extract")}
HINTS = {"comfy-media": r"image|picture|photo|comfy|render|draw|video|workflow|generate", "playwright-isolated": r"browse|browser|web|page|site|url|http|click|screenshot",
         "perennos_tempo": r"tempo|kanban|memory|research|status|job|route|lint|todo|house|engine|home ?assistant|\bha\b|search"}

ps_lock = threading.Lock()


def ps(cmd, timeout=60):
    r = subprocess.run(["powershell", "-NoProfile", "-NonInteractive", "-Command", cmd], capture_output=True, timeout=timeout,
                       cwd=WRITE_ROOTS[0], creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
    return (r.stdout.decode("utf-8", "replace") + r.stderr.decode("utf-8", "replace")).strip(), r.returncode


# ---------------------------------------------------------------- conversations + workflow memory
class Conv:
    def __init__(self, cid=None):
        self.id = cid or time.strftime("%Y%m%d-%H%M%S-") + uuid.uuid4().hex[:4]
        self.path = os.path.join(CONVS, self.id + ".json")
        self.wf = os.path.join(WFS, self.id + ".md")
        self.messages, self.events, self.busy, self.last_mem = [], [], False, []
        self.confirms, self.lock = {}, threading.Lock()
        self.guest = False  # a chat room's conversation: no tools, no memory, no owner context
        if os.path.exists(self.path):
            d = json.load(open(self.path, encoding="utf-8"))
            self.messages, self.events = d["messages"], d["events"]
            self.guest = d.get("mode") == "guest"
        if not os.path.exists(self.wf):
            open(self.wf, "w", encoding="utf-8").write(f"# Workflow {self.id}\nStarted {now()}\n\n## Task list\n(none yet)\n\n## Notes\n\n## Log\n")

    def save(self):
        tmp = self.path + ".tmp"
        json.dump({"messages": self.messages, "events": self.events, "mode": "guest" if self.guest else "owner"}, open(tmp, "w", encoding="utf-8"))
        os.replace(tmp, self.path)

    def emit(self, type_, **kw):
        with self.lock:
            e = {"n": len(self.events), "t": now(), "type": type_, **{k: (cap(v, 3000) if isinstance(v, str) else v) for k, v in kw.items()}}
            self.events.append(e)
            return e

    def wf_text(self):
        return open(self.wf, encoding="utf-8").read()

    def wf_log(self, line):
        open(self.wf, "a", encoding="utf-8").write(f"- {now()} {redact(line)[:300]}\n")

    def wf_update(self, tasks=None, note=None):
        t = self.wf_text()
        if tasks is not None:
            body = "\n".join(f"- [{'x' if (x.get('done') if isinstance(x, dict) else False) else ' '}] {x.get('text') if isinstance(x, dict) else x}" for x in tasks) or "(none)"
            t = re.sub(r"## Task list\n.*?\n\n## Notes", lambda m: "## Task list\n" + body + "\n\n## Notes", t, flags=re.S)
        if note:
            t = t.replace("\n## Log", f"- {now()} {note}\n\n## Log", 1) if "\n## Log" in t else t + f"\n- {note}\n"
        open(self.wf, "w", encoding="utf-8").write(t)
        return "workflow updated"

    def confirm(self, what, detail):
        cid = uuid.uuid4().hex[:6]
        ev = threading.Event()
        self.confirms[cid] = {"ev": ev, "ok": False}
        self.emit("confirm", id=cid, what=what, detail=detail)
        self.save()
        ev.wait(CONFIRM_TIMEOUT)
        ok = self.confirms.pop(cid)["ok"]
        self.emit("status", text=f"{what}: {'approved' if ok else 'denied or timed out'}")
        return ok


CONFIRM_TIMEOUT = 900
CONVOS = {}


def get_conv(cid=None):
    if cid and cid in CONVOS:
        return CONVOS[cid]
    c = Conv(cid)
    CONVOS[c.id] = c
    return c


# ---------------------------------------------------------------- tools
class Tools:
    def __init__(self):
        self.servers, self.lock, self.cfgs = {}, threading.Lock(), mcphost.load_server_configs()
        # Perennos Tempo's own products and plug-in connectors connect by themselves; explicit wins.
        found, confirm, notes = mcphost.discover(CFG)
        for n in notes:
            print(f"[tools: {n}]", file=sys.stderr)
        self.cfgs = {**found, **self.cfgs}
        CONFIRM_MCP.update(confirm)
        self.catalog = None  # [(server, tool dict)]

    def server(self, name):
        with self.lock:
            s = self.servers.get(name)
            if s is None:
                s = self.servers[name] = mcphost.McpServer(name, self.cfgs[name]).start(25 if name == "perennos_knowledge" else 90)
            return s

    def load_catalog(self):
        if self.catalog is None:
            cat = []
            for n in self.cfgs:
                if n == "perennos_knowledge":
                    continue  # bridge timed out in Claude Code too; reachable only via tempo_ask / rag_search
                s = self.server(n)
                for t in s.tools:
                    if t["name"] not in BLOCKED_MCP.get(n, set()):
                        cat.append((n, t))
            self.catalog = cat
        return self.catalog

    def shrink(self, schema):
        def walk(x):
            if isinstance(x, dict):
                out = {}
                for k, v in x.items():
                    if k in ("title", "$schema", "examples", "default") and not isinstance(v, dict):
                        continue  # metadata only; a PROPERTY named 'title' or 'default' is a dict and is kept
                    if k == "description" and isinstance(v, str):
                        out[k] = v[:120]
                    elif k == "properties" and isinstance(v, dict):
                        out[k] = {pk: walk(pv) for pk, pv in v.items()}
                    else:
                        out[k] = walk(v)
                return out
            if isinstance(x, list):
                return [walk(i) for i in x]
            return x
        return walk(schema or {"type": "object", "properties": {}})

    def select(self, prompt, n_builtin):
        """About TOOL_CAP tools: all builtins, then MCP tools ranked by keyword fit to the prompt."""
        room = max(0, TOOL_CAP - n_builtin)
        qt = tok(prompt)
        ranked = []
        for srv, t in self.load_catalog():
            words = tok(t["name"].replace("_", " ") + " " + (t.get("description") or "")[:200])
            sc = 3 * len(qt & tok(t["name"].replace("_", " "))) + len(qt & words)
            if re.search(HINTS.get(srv, "$^"), prompt.lower()):
                sc += 1
            if sc >= 3:
                ranked.append((sc, srv, t))
        ranked.sort(key=lambda x: -x[0])
        return [(s, t) for _, s, t in ranked[:min(room, 8)]]

    def stop_all(self):
        for s in self.servers.values():
            s.stop()


TOOLS = Tools()
BUILTIN = {}


def builtin(name, desc, props, required=()):
    def deco(fn):
        BUILTIN[name] = (fn, {"type": "function", "function": {"name": name, "description": desc,
                                                               "parameters": {"type": "object", "properties": props, "required": list(required)}}})
        return fn
    return deco


S = lambda d: {"type": "string", "description": d}


@builtin("list_dir", "List the files and folders in a directory.", {"path": S("directory path")}, ["path"])
def t_list_dir(conv, path):
    if SECRET_PATH.search(path):
        return "refused: secret path"
    out = []
    for e in sorted(os.scandir(path), key=lambda e: (not e.is_dir(), e.name.lower()))[:200]:
        out.append(("[dir] " if e.is_dir() else f"{e.stat().st_size:>10}  ") + e.name)
    return "\n".join(out) or "(empty)"


@builtin("read_file", "Read a text file (first 6000 chars unless offset given).", {"path": S("file path"), "offset": {"type": "integer"}}, ["path"])
def t_read_file(conv, path, offset=0):
    if SECRET_PATH.search(path):
        return "refused: secret path"
    return open(path, encoding="utf-8", errors="replace").read()[int(offset):int(offset) + RESULT_CAP]


@builtin("write_file", "Write a text file inside the allowed folders (" + ", ".join(WRITE_ROOTS) + "). Overwrites need approval.",
         {"path": S("file path"), "content": S("text")}, ["path", "content"])
def t_write_file(conv, path, content):
    if not in_roots(path):
        return "refused: outside allowed write folders: " + "; ".join(WRITE_ROOTS)
    if os.path.exists(path) and not conv.confirm("overwrite file", path):
        return "denied by user"
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    open(path, "w", encoding="utf-8").write(content)
    return f"wrote {len(content)} chars to {path}"


@builtin("trash", "Move a file or folder to the Windows Recycle Bin (the only way to remove anything). Needs approval.", {"path": S("path")}, ["path"])
def t_trash(conv, path):
    if not in_roots(path) or not os.path.exists(path):
        return "refused: missing or outside allowed folders"
    if not conv.confirm("move to Recycle Bin", path):
        return "denied by user"
    kind = "DeleteDirectory" if os.path.isdir(path) else "DeleteFile"
    q = path.replace("'", "''")
    cmd = f"Add-Type -AssemblyName Microsoft.VisualBasic; [Microsoft.VisualBasic.FileIO.FileSystem]::{kind}('{q}','OnlyErrorDialogs','SendToRecycleBin')"
    out, rc = ps(cmd)
    return "moved to Recycle Bin" if rc == 0 else "failed: " + out


@builtin("run_command", "Run a PowerShell command in " + WRITE_ROOTS[0] + ". Read-only commands run at once; anything that writes, installs, stops things or uses the network asks the user first. Deleting is refused (use trash).",
         {"command": S("PowerShell command")}, ["command"])
def t_run_command(conv, command):
    if DELETE_RX.search(command):
        return "refused: deleting is not allowed here; use the trash tool (Recycle Bin)"
    if COMFY_RX.search(command):
        return "refused: start ComfyUI only through the comfy_image tool (it takes the GPU lease)"
    if PROTECT_RX and PROTECT_RX.search(command) and re.search(r"(?i)stop|kill|rm|unload|delete|restart", command):
        return "refused: protected service (" + ", ".join(PROTECTED) + ")"
    if CONFIRM_RX.search(command) and not conv.confirm("run command", command):
        return "denied by user"
    out, rc = ps(command)
    return f"[exit {rc}]\n{out}"


@builtin("fetch_url", "Fetch a web page as text. Works only when the internet is up; fails cleanly offline.", {"url": S("http(s) URL")}, ["url"])
def t_fetch_url(conv, url):
    if not url.startswith(("http://", "https://")):
        return "refused: http(s) only"
    if not net_up():
        return "offline: no internet right now. Use local memory (search_memory) or tell the user."
    req = urllib.request.Request(url, headers={"User-Agent": "PerennosTempo/1"})
    raw = urllib.request.urlopen(req, timeout=20).read(400000).decode("utf-8", "replace")
    raw = re.sub(r"(?is)<(script|style).*?</\1>", " ", raw)
    return re.sub(r"\s+", " ", html.unescape(re.sub(r"<[^>]+>", " ", raw))).strip()


@builtin("search_memory", "Search the owner's memory files (core notes, memory notes, continuity, knowledge pack). Returns cited excerpts.",
         {"query": S("what to look for")}, ["query"])
def t_search_memory(conv, query):
    return "\n\n".join(f"[{os.path.basename(r['src'])}#{r['chunk']}] {r['text'][:700]}" for r in rag_search(query, 5))


@builtin("workflow_update", "Update this conversation's persistent task list and notes. Call it when you plan work and after each step.",
         {"tasks": {"type": "array", "items": {"type": "object", "properties": {"text": {"type": "string"}, "done": {"type": "boolean"}}}}, "note": S("short note")})
def t_workflow_update(conv, tasks=None, note=None):
    return conv.wf_update(tasks, note)


def probe_host(name):
    h = HOST_OVERRIDE.get(name) or HOSTS.get(name)
    if not h:
        return None, f"unknown host '{name}'. Known: {', '.join(list(HOSTS) + list(HOST_OVERRIDE))}"
    for u in h["urls"]:
        try:
            tags = http_json(u + "/api/tags", timeout=10 if "127.0.0.1" in u else 2.5)
            return (u, h, [m["name"] for m in tags.get("models", [])]), None
        except Exception:
            continue
    return None, f"host '{name}' unreachable on {', '.join(h['urls'])}"


@builtin("delegate", "Run a subtask on another local engine (host: " + ", ".join(HOSTS) + ") with its own small tool loop. Use for second opinions, research or work another machine already has.",
         {"host": S("|".join(HOSTS)), "model": S("model name on that host, or empty for its house model"), "task": S("the subtask, self-contained")}, ["host", "task"])
def t_delegate(conv, host, task, model=""):
    p, err = probe_host(host)
    if err:
        return "delegate failed: " + err
    url, h, avail = p
    if h["models"]:
        if model and model not in h["models"]:
            return f"refused: on {host} only the house model {h['models']} may be used (never evict it)"
        model = h["models"][0]
    model = model or MODELS[0]
    if model not in avail and not any(a.startswith(model) for a in avail):
        return f"model '{model}' not on {host}. Available: {', '.join(avail[:15])}"
    return sub_loop(conv, url, model, task, local=(host == LOCAL_HOST))


def sub_loop(conv, url, model, task, local):
    ro = [BUILTIN[n][1] for n in ("list_dir", "read_file", "search_memory")]
    msgs = [{"role": "system", "content": "You are a Perennos Tempo subagent. Do the task, use tools if needed, answer briefly and concretely."},
            {"role": "user", "content": task}]
    for _ in range(4):
        body = {"model": model, "messages": msgs, "tools": ro, "stream": False, "options": {"num_ctx": 8192}}
        if local:
            body["keep_alive"] = "10m"
        if model.startswith("uncensored-qwen") or model.startswith("local-qwen"):
            body["think"] = False
        m = http_json(url + "/api/chat", body, 900)["message"]
        msgs.append(m)
        if not m.get("tool_calls"):
            return f"[{model} @ {url}] " + (m.get("content") or "")
        for tc in m["tool_calls"]:
            fn = tc["function"]
            try:
                r = BUILTIN[fn["name"]][0](conv, **(fn.get("arguments") or {}))
            except Exception as e:
                r = f"error: {e}"
            msgs.append({"role": "tool", "tool_name": fn["name"], "content": cap(r, 3000)})
    return f"[{model}] step cap reached"


CLAUDE_EXE = os.path.expandvars(r"%USERPROFILE%\.local\bin\claude.exe")


@builtin("claude_code", "Hand a coding or multi-file task to the Claude Code harness running on the LOCAL engine (Ollama's Anthropic-compatible API, no cloud). Gives the full Claude Code tool set and its MCP servers. read_only=true by default.",
         {"task": S("self-contained task"), "cwd": S("working folder (default " + SCRATCH + ")"), "read_only": {"type": "boolean"}, "model": S("default local-qwen3 (27B fails: its template rejects mid-conversation system messages)")}, ["task"])
def t_claude_code(conv, task, cwd=SCRATCH, read_only=True, model=""):
    if not os.path.isfile(CLAUDE_EXE):
        return "claude.exe not found"
    if not read_only and not conv.confirm("claude_code with write access", f"{cwd}: {task[:200]}"):
        return "denied by user"
    env = dict(os.environ, ANTHROPIC_BASE_URL=OLLAMA, ANTHROPIC_AUTH_TOKEN="ollama", ANTHROPIC_API_KEY="",
               CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", DISABLE_AUTOUPDATER="1", NO_PROXY="127.0.0.1,localhost")
    tools = "Read,Glob,Grep,LS" if read_only else "Read,Glob,Grep,LS,Edit,Write,Bash"
    r = subprocess.run([CLAUDE_EXE, "-p", task, "--model", model or "local-qwen3", "--allowedTools", tools, "--max-turns", "12"],
                       capture_output=True, timeout=1500, cwd=cwd, env=env, creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
    return (r.stdout.decode("utf-8", "replace") + r.stderr.decode("utf-8", "replace")).strip() or f"(no output, exit {r.returncode})"


def gpu_status():
    if not GPU_LEASE:
        return "gpu_lease_script is not set in agent-config.json"
    out, _ = ps(f'python "{GPU_LEASE}" status')
    return out


def unload_ollama():
    try:
        ps_ = http_json(OLLAMA + "/api/ps", timeout=5).get("models", [])
        ours = set(MODELS) | {EMBED_MODEL}
        for m in ps_:
            if m["name"].split(":")[0] not in ours:
                continue
            http_json(OLLAMA + "/api/generate", {"model": m["name"], "keep_alive": 0}, 60)
    except Exception:
        pass


def start_comfy():
    """Start ComfyUI via comfy-media's launch tool (the same path Claude Code uses)."""
    s = TOOLS.server("comfy-media")
    return s.call("launch_comfyui", {}, 240)


COMFY = "http://127.0.0.1:8188"


def comfy_txt2img(prompt, w, h):
    """Minimal SD1.5 graph over the ComfyUI HTTP API using the installed checkpoint."""
    ck = http_json(COMFY + "/object_info/CheckpointLoaderSimple", timeout=30)["CheckpointLoaderSimple"]["input"]["required"]["ckpt_name"][0]
    if not ck:
        raise RuntimeError("no checkpoint installed in ComfyUI")
    g = {"1": {"class_type": "CheckpointLoaderSimple", "inputs": {"ckpt_name": ck[0]}},
         "2": {"class_type": "CLIPTextEncode", "inputs": {"text": prompt, "clip": ["1", 1]}},
         "3": {"class_type": "CLIPTextEncode", "inputs": {"text": "blurry, low quality, text, watermark", "clip": ["1", 1]}},
         "4": {"class_type": "EmptyLatentImage", "inputs": {"width": w, "height": h, "batch_size": 1}},
         "5": {"class_type": "KSampler", "inputs": {"model": ["1", 0], "positive": ["2", 0], "negative": ["3", 0], "latent_image": ["4", 0],
                                                    "seed": int(time.time()) % 10**9, "steps": 20, "cfg": 7, "sampler_name": "euler", "scheduler": "normal", "denoise": 1}},
         "6": {"class_type": "VAEDecode", "inputs": {"samples": ["5", 0], "vae": ["1", 2]}},
         "7": {"class_type": "SaveImage", "inputs": {"images": ["6", 0], "filename_prefix": "tempo-agent"}}}
    pid = http_json(COMFY + "/prompt", {"prompt": g}, 30)["prompt_id"]
    for _ in range(300):
        hist = http_json(COMFY + "/history/" + pid, timeout=30).get(pid)
        if hist and hist.get("outputs"):
            im = hist["outputs"]["7"]["images"][0]
            return os.path.join(COMFY_OUT or "ComfyUI output", im.get("subfolder", ""), im["filename"])
        time.sleep(2)
    raise TimeoutError("ComfyUI did not finish in 10 minutes")

@builtin("comfy_image", "Make one image with local ComfyUI and return the file path. Unloads the language model, takes the GPU lease, runs ComfyUI, then releases the GPU.",
         {"prompt": S("image prompt"), "width": {"type": "integer"}, "height": {"type": "integer"}}, ["prompt"])
def t_comfy_image(conv, prompt, width=512, height=512):
    if not GPU_LEASE:
        return "refused: set gpu_lease_script in agent-config.json so ComfyUI never takes a GPU another job holds"
    st = gpu_status()
    try:
        free = json.loads(st).get("lease") is None
    except Exception:
        free = False
    if not free:
        # someone else holds the lease (maybe a stream encoder): never evict
        return "refused: GPU lease is held by another job. Not evicting it. Status:\n" + st[:500]
    holder = "tempo-agent-comfy"
    conv.emit("status", text="unloading Tempo's own language model so ComfyUI gets the GPU")
    unload_ollama()  # only our own models; the lease below still refuses if anything else holds VRAM
    time.sleep(3)
    out, rc = ps(f'python "{GPU_LEASE}" --holder {holder} --portfolio tempo-agent --need-mib 8000 --ttl-min 20 --wait-min 0 acquire')
    if rc != 0:
        return "lease refused: " + out[:400]
    conv.emit("status", text="GPU lease taken")
    try:
        srv = TOOLS.server("comfy-media")
        try:
            urllib.request.urlopen("http://127.0.0.1:8188/system_stats", timeout=3)
        except Exception:
            conv.emit("status", text="starting ComfyUI")
            start_comfy()
        info = srv.call("server_info", {}, 60)  # via the comfy-media MCP server (same one Claude Code uses)
        for _ in range(60):
            try:
                urllib.request.urlopen(COMFY + "/system_stats", timeout=3)
                break
            except Exception:
                time.sleep(3)
        path = comfy_txt2img(prompt, int(width), int(height))
        return f"image path: {path}"
    finally:
        try:
            TOOLS.server("comfy-media").call("stop_comfyui", {}, 60)
        except Exception:
            pass
        ps(f'python "{GPU_LEASE}" --holder {holder} release')
        conv.emit("status", text="GPU lease released")


# ---------------------------------------------------------------- engine + loop
def chat_once(model, messages, tools, conv=None):
    """Streams from Ollama. Deltas go to the GUI (batched ~0.4 s); returns the same shape as stream:false plus timings."""
    body = {"model": model, "messages": messages, "stream": True, "keep_alive": "60m",
            "options": {"num_ctx": NUM_CTX.get(model, 8192), "temperature": 0.4}}
    if tools:
        body["tools"] = tools
    if model.startswith("uncensored-qwen") or model.startswith("local-qwen"):
        body["think"] = False  # gpt-oss never gets think:false
    req = urllib.request.Request(OLLAMA + "/api/chat", data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})
    t0, first, parts, calls, buf, last, final = time.time(), None, [], [], "", 0.0, {}
    with urllib.request.urlopen(req, timeout=1200) as r:
        for line in r:
            if not line.strip():
                continue
            d = json.loads(line)
            m = d.get("message") or {}
            piece = m.get("content") or ""
            if m.get("tool_calls"):
                calls += m["tool_calls"]
            if (piece or m.get("tool_calls")) and first is None:
                first = time.time() - t0
            if piece:
                parts.append(piece)
                buf += piece
                if conv and time.time() - last > 0.4:
                    conv.emit("delta", text=buf)
                    buf, last = "", time.time()
            if d.get("done"):
                final = d
                break
    if conv and buf:
        conv.emit("delta", text=buf)
    msg = {"role": "assistant", "content": "".join(parts)}
    if calls:
        msg["tool_calls"] = calls
    total = time.time() - t0
    info = {"model": model, "first_s": round(first if first is not None else total, 2), "total_s": round(total, 2),
            "load_s": round(final.get("load_duration", 0) / 1e9, 2), "prompt_tok": final.get("prompt_eval_count"),
            "prompt_s": round(final.get("prompt_eval_duration", 0) / 1e9, 2), "out_tok": final.get("eval_count"),
            "t": now()}
    try:
        open(os.path.join(HOME, "logs", "timing.jsonl"), "a", encoding="utf-8").write(json.dumps(info) + "\n")
    except Exception:
        pass
    return {"message": msg, "info": info}


TOOLISH = re.compile(r"\b(list|ls|dir|folder|directory|read|open|run|execute|command|write|save|create|make|generate|render|image|picture|"
                     r"delegate|" + "|".join(map(re.escape, HOSTS)) + r"|trash|delete|remove|fetch|download|install|restart|start|stop|comfy|workflow|task list|todo|"
                     r"fix|edit|patch|status of|check (the )?(status|disk|gpu))\b|[A-Za-z]:\\|\.(py|md|json|ps1|txt|log)\b", re.I)
HARDISH = re.compile(r"\b(analy[sz]e|compare|plan|design|architecture|debug|step by step|prove|calculate|code|script|refactor|review|draft)\b", re.I)


def pick_lane(text):
    """fast = 9B, no tools; heavy = 27B with tools. The 9B can also hand a turn up by replying NEEDS_TOOLS."""
    if len(text) > 600 or TOOLISH.search(text) or HARDISH.search(text):
        return "heavy"
    try:  # sticky: if only the 27B is resident, use it (a warm 27B answers in ~1 s; switching costs a 10-30 s reload)
        res = {m["name"].split(":")[0] for m in http_json(OLLAMA + "/api/ps", timeout=3).get("models", [])}
        if "uncensored-qwen3.8-27b-8k" in res and "local-qwen3" not in res:
            return "heavy"
    except Exception:
        pass
    return "fast"


def trim(messages):
    sysm, rest = messages[0], messages[1:]
    def size(ms):
        return sum(len(json.dumps(m)) for m in ms)
    while size(rest) > HIST_CHARS and len(rest) > 2:
        rest.pop(0)
        while rest and rest[0]["role"] == "tool":
            rest.pop(0)
    return [sysm] + rest


SYSTEM = """You are Perennos Tempo, {owner}'s local superagent: the harness around a local model. You run on this machine with no cloud and may have no internet and no other machines.
{owner_note}Today is {today}.
Rules:
- Answer from the MEMORY excerpts below when they cover the question, and cite each source as [filename]. If they do not cover it, say so; never invent facts about {owner}'s work.
- Use tools to do things (list folders, run commands, make images, ask other machines with delegate). Check results before claiming success. Report failures plainly.
- Never delete anything; the only removal is the trash tool (Recycle Bin) and it needs {owner}'s approval. Never touch secrets.
- Keep a task list for multi-step jobs with workflow_update. The saved workflow is below; continue from it.
- Be brief and direct: under 120 words unless {owner} asks for detail; do not dump whole lists. No disclaimers.
WORKFLOW FILE (persistent):
{wf}
"""


SYSTEM_GUEST = """You are Perennos Tempo, chatting with people in a group chat room. Today is {today}.
Rules:
- You have no tools here and no access to anyone's files, notes, memory or computer. If someone asks you to run, open, change, send or look up something on a computer, say that can't be done from a room.
- Never reveal private details about anyone, and never share keys, passwords or secrets.
- Answer briefly in a friendly chat tone: a few sentences, plain text. If you are not sure, say so; never invent facts.
"""


def agent_turn(conv, text):
    conv.busy = True
    try:
        conv.emit("user", text=text)
        if conv.guest:  # never the owner's memory, notes or workflow in a guest room
            conv.last_mem = []
            conv.emit("status", text="memory: off (guest conversation)")
            sysmsg = {"role": "system", "content": SYSTEM_GUEST.format(today=time.strftime("%d %b %Y"))}
        else:
            hits = rag_search(text, RAG_K)
            conv.last_mem = sorted({h["src"] for h in hits})
            mem = "\n\n".join(f"[{os.path.basename(h['src'])}] {h['text'][:RAG_CHARS]}" for h in hits)
            conv.emit("status", text="memory: " + ", ".join(sorted({os.path.basename(h['src']) for h in hits})))
            sysmsg = {"role": "system", "content": SYSTEM.format(owner=OWNER, owner_note=OWNER_NOTE + "\n" if OWNER_NOTE else "", today=time.strftime("%d %b %Y"), wf=conv.wf_text()[-2500:]) + "\nMEMORY EXCERPTS:\n" + mem}
        if not conv.messages or conv.messages[0]["role"] != "system":
            conv.messages.insert(0, sysmsg)
        else:
            conv.messages[0] = sysmsg
        conv.messages.append({"role": "user", "content": text})
        lane = pick_lane(text)
        sel, route = [], {}
        if conv.guest:
            tools = []  # no tools in a guest room, on either lane
        elif lane == "heavy":
            sel = TOOLS.select(text, len(BUILTIN))
            tools = [v[1] for v in BUILTIN.values()] + [{"type": "function", "function": {
                "name": (s + "__" + t["name"]).replace("-", "_"), "description": (t.get("description") or "")[:300].strip(),
                "parameters": TOOLS.shrink(t.get("inputSchema"))}} for s, t in sel]
            route = {(s + "__" + t["name"]).replace("-", "_"): (s, t["name"]) for s, t in TOOLS.load_catalog()}
        else:
            tools = []
            conv.messages[0]["content"] += "\nFAST LANE: no tools this turn. If the request needs a tool, a file, a command, another machine or a long careful analysis, reply with exactly NEEDS_TOOLS and nothing else."
        models = list(MODELS if lane == "heavy" else FAST_MODELS)
        conv.emit("status", text=f"lane: {lane} ({models[0]}); tools offered: {len(tools)} ({len(sel)} MCP)")
        model_i = 0
        for step in range(MAX_STEPS):
            conv.messages = trim(conv.messages)
            while True:
                try:
                    try:
                        resp = chat_once(models[model_i], conv.messages, tools, conv)
                    except urllib.error.HTTPError as e:  # model swap or transient load error: one retry
                        why = e.read()[:160].decode("utf-8", "replace")
                        conv.emit("status", text=f"engine {models[model_i]} HTTP {e.code}: {why}; retrying once")
                        time.sleep(8)
                        resp = chat_once(models[model_i], conv.messages, tools, conv)
                    break
                except Exception as e:
                    conv.emit("status", text=f"engine {models[model_i]} failed ({type(e).__name__}); trying fallback")
                    model_i += 1
                    if model_i >= len(models):
                        raise RuntimeError("all engines failed")
            i_ = resp["info"]
            conv.emit("status", text=f"{i_['model']}: first token {i_['first_s']} s, total {i_['total_s']} s (load {i_['load_s']} s, prompt {i_['prompt_tok']} tok in {i_['prompt_s']} s, {i_['out_tok']} out)")
            if lane == "fast" and (resp["message"].get("content", "").strip().startswith("NEEDS_TOOLS") or not resp["message"].get("content", "").strip()):
                lane = "heavy"
                conv.emit("status", text="fast lane handed this turn to the 27B")
                if not conv.guest:
                    sel = TOOLS.select(text, len(BUILTIN))
                    tools = [v[1] for v in BUILTIN.values()] + [{"type": "function", "function": {
                        "name": (s + "__" + t["name"]).replace("-", "_"), "description": (t.get("description") or "")[:300].strip(),
                        "parameters": TOOLS.shrink(t.get("inputSchema"))}} for s, t in sel]
                    route = {(s + "__" + t["name"]).replace("-", "_"): (s, t["name"]) for s, t in TOOLS.load_catalog()}
                models, model_i = list(MODELS), 0
                continue
            m = resp["message"]
            m.pop("thinking", None)
            conv.messages.append(m)
            calls = m.get("tool_calls") or []
            if not calls:
                conv.emit("assistant", text=m.get("content", ""), model=models[model_i])
                conv.wf_log(f"turn done: {text[:80]}")
                return
            if m.get("content"):
                conv.emit("assistant_partial", text=m["content"])
            for tc in calls:
                fn = tc["function"]
                name, args = fn["name"], fn.get("arguments") or {}
                conv.emit("tool_call", name=name, args=cap(json.dumps(args), 600))
                if conv.guest:  # none were offered; a model that calls one anyway gets nothing done
                    conv.messages.append({"role": "tool", "tool_name": name, "content": "tools are off in this conversation (a guest room)"})
                    conv.emit("tool_result", name=name, text="refused: guest room")
                    continue
                key = name + json.dumps(args, sort_keys=True)  # 5 Nov: local models loop on the same call; answer repeats from memory
                seen = conv.__dict__.setdefault("_seen_calls", {})
                if key in seen:
                    seen[key][1] += 1
                    r = f"You already ran this exact call (x{seen[key][1]}). Its result was:\n{seen[key][0][:1500]}\nUse that result: take the next step or give the final answer. Do not repeat it."
                    conv.emit("tool_result", name=name, text="(repeat skipped)")
                    conv.messages.append({"role": "tool", "tool_name": name, "content": r})
                    continue
                try:
                    if name in BUILTIN:
                        r = BUILTIN[name][0](conv, **args)
                    elif name in route:
                        srv, tn = route[name]
                        if (srv, tn) in CONFIRM_MCP and not conv.confirm(f"MCP {srv}:{tn}", json.dumps(args)[:300]):
                            r = "denied by user"
                        else:
                            r = TOOLS.server(srv).call(tn, args, 600)
                    else:
                        r = f"unknown tool {name}"
                except Exception as e:
                    r = f"ERROR {type(e).__name__}: {e}"
                r = cap(r)
                seen[key] = [r, 1]
                conv.emit("tool_result", name=name, text=r[:1500])
                conv.messages.append({"role": "tool", "tool_name": name, "content": r})
                conv.wf_log(f"{name} -> {r[:100].replace(chr(10), ' ')}")
            conv.save()
        # 5 Nov: at the cap, ask once (no tools) for what was done and what is next, instead of just stopping.
        conv.messages.append({"role": "user", "content": "Step limit reached. Without calling tools, report what you finished, what is left, and the next step. Be brief."})
        try:
            fin = chat_once(models[model_i], trim(conv.messages), [], conv)["message"].get("content", "")
        except Exception:
            fin = ""
        conv.emit("assistant", text=(fin or "") + "\n\n(step limit reached; say 'continue' to keep going, the workflow file keeps the plan.)")
        conv.wf_log("step cap: " + (fin or "")[:200].replace("\n", " "))
        conv.save()
    except Exception as e:
        conv.emit("error", text=f"{type(e).__name__}: {e}")
    finally:
        conv.busy = False
        conv.save()


# ---------------------------------------------------------------- web server


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send_json(self, o, code=200):
        b = json.dumps(o).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(n) or b"{}")

    def proxy(self, method):
        """Everything that is not /agent/* is the existing Perennos Tempo GUI server (tempo-gui --serve), same origin so its CSP holds."""
        try:
            n = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(n) if n else None
            hdr = {k: v for k, v in self.headers.items() if k.lower() not in ("host", "connection", "accept-encoding")}
            hdr["Host"] = f"127.0.0.1:{GUI_PORT}"
            conn = http.client.HTTPConnection("127.0.0.1", GUI_PORT, timeout=600)
            conn.request(method, self.path, body, hdr)
            r = conn.getresponse()
            self.send_response(r.status)
            for k, v in r.getheaders():
                if k.lower() not in ("transfer-encoding", "connection", "content-length"):
                    self.send_header(k, v)
            self.send_header("Connection", "close")
            self.end_headers()
            while True:
                chunk = r.read1(8192) if hasattr(r, "read1") else r.read(8192)
                if not chunk:
                    break
                self.wfile.write(chunk)
                self.wfile.flush()
        except Exception as e:
            self.send_json({"error": "gui server unavailable: " + type(e).__name__}, 502)
    def do_GET(self):
        u = urlparse(self.path)
        q = {k: v[0] for k, v in parse_qs(u.query).items()}
        if u.path.startswith("/chat/") and ".." not in u.path:
            f = os.path.join(HOME, "gui-ui", u.path.lstrip("/").replace("/", os.sep))
            if os.path.isfile(f):
                b = open(f, "rb").read()
                self.send_response(200)
                self.send_header("Content-Type", "application/json" if f.endswith(".json") else "text/css" if f.endswith(".css") else "text/javascript")
                self.send_header("Content-Length", str(len(b)))
                self.send_header("Cache-Control", "no-store")
                self.end_headers()
                self.wfile.write(b)
                return
        if not u.path.startswith("/agent/"):
            return self.proxy("GET")
        if u.path == "/agent/convs":
            fs = sorted(glob.glob(os.path.join(CONVS, "*.json")), key=os.path.getmtime, reverse=True)[:30]
            self.send_json([os.path.basename(f)[:-5] for f in fs])
        elif u.path == "/agent/events":
            c = get_conv(q["conv"])
            since = int(q.get("since", 0))
            self.send_json({"events": c.events[since:], "busy": c.busy})
        elif u.path == "/agent/workflow":
            self.send_json({"text": get_conv(q["conv"]).wf_text()})
        elif u.path == "/agent/memory":
            c = CONVOS.get(q.get("conv", ""))  # never create a conversation just to read
            self.send_json({"files": memory_files(), "used": c.last_mem if c else [], "embed": _rag["embed_ok"]})
        elif u.path == "/agent/memory/graph":
            g, c = memory_graph(), CONVOS.get(q.get("conv", ""))
            used = set(c.last_mem) if c else set()
            self.send_json({"nodes": [dict(n, used=n["src"] in used) for n in g["nodes"]], "edges": g["edges"]})
        elif u.path == "/agent/memory/file":
            r = memory_file(q.get("src", ""))
            self.send_json(r if r else {"error": "not an indexed memory file"}, 200 if r else 404)
        elif u.path == "/agent/health":
            hosts = {n: (probe_host(n)[1] or "up") for n in HOSTS}
            # guest: POST /agent/new {"mode": "guest"} makes a room-safe conversation
            self.send_json({"net": net_up(), "models": MODELS, "hosts": hosts, "rag": _rag["embed_ok"], "guest": True})
        else:
            self.send_json({"error": "not found"}, 404)

    def do_POST(self):
        u = urlparse(self.path)
        if not u.path.startswith("/agent/"):
            return self.proxy("POST")
        b = self.body()
        if u.path == "/agent/new":
            c = get_conv()
            if isinstance(b, dict) and b.get("mode") == "guest":
                c.guest = True
                c.save()
            self.send_json({"conv": c.id})
        elif u.path == "/agent/send":
            c = get_conv(b["conv"])
            if c.busy:
                return self.send_json({"error": "busy"}, 409)
            c.busy = True
            threading.Thread(target=agent_turn, args=(c, b["text"]), daemon=True).start()
            self.send_json({"ok": True})
        elif u.path == "/agent/confirm":
            c = get_conv(b["conv"])
            x = c.confirms.get(b["id"])
            if x:
                x["ok"] = bool(b["ok"])
                x["ev"].set()
            self.send_json({"ok": bool(x)})
        else:
            self.send_json({"error": "not found"}, 404)


def main():
    port = int(os.environ.get("TEMPO_PORT", "8765"))
    threading.Thread(target=lambda: (build_index()), daemon=True).start()
    ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()


if __name__ == "__main__":
    main()
