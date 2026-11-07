"""Minimal stdio MCP client (stdlib only). Loads the same mcpServers Claude Code uses.
Never prints env values. One reader thread per server; calls are matched by JSON-RPC id."""
import glob, json, os, re, subprocess, threading, itertools, queue, time

CLAUDE_JSON = os.path.expanduser(os.environ.get("TEMPO_CLAUDE_JSON") or "~/.claude.json")
# Project folders whose MCP servers load too (TEMPO_MCP_PROJECT_DIRS, os.pathsep-separated).
# tempo_agent.py may also set this from "mcp_project_dirs" in agent-config.json.
PROJECT_DIRS = [os.path.expanduser(p).replace("\\", "/").rstrip("/")
                for p in os.environ.get("TEMPO_MCP_PROJECT_DIRS", "").split(os.pathsep) if p.strip()]


def load_server_configs():
    """Return {name: cfg} from ~/.claude.json (global + project) and any project .mcp.json."""
    out = {}
    try:
        j = json.load(open(CLAUDE_JSON, encoding="utf-8"))
    except Exception:
        return out
    out.update(j.get("mcpServers") or {})
    for p, v in (j.get("projects") or {}).items():
        if p.replace("\\", "/").rstrip("/") in PROJECT_DIRS:
            out.update(v.get("mcpServers") or {})
    for d in PROJECT_DIRS:
        f = os.path.join(d, ".mcp.json")
        if os.path.exists(f):
            try:
                out.update(json.load(open(f, encoding="utf-8")).get("mcpServers") or {})
            except Exception:
                pass
    return {k: v for k, v in out.items() if v.get("command")}  # stdio only


# ---------------------------------------------------------------- Perennos Tempo products, no setup
# tempo-vault, tempo-studio and tempo-room found next to this harness (then TEMPO_BIN, then PATH)
# connect as MCP servers with fixed arguments; <tempo home>/connectors/*.json, and the same in each
# pack (<tempo home>/packs/<name>/), add more. Explicit mcpServers always win. TEMPO_AUTO_TOOLS=0
# or "auto_tools": false turns it off. Overwriting, renaming or trashing a note, and posting into a
# chat room, ask first. Same rules as perennos-tempo-local-agent/src/products.rs.

def bin_dirs():
    d = [os.path.dirname(os.path.abspath(__file__))]
    for var in ("TEMPO_BIN", "PATH"):
        d += [p for p in os.environ.get(var, "").split(os.pathsep) if p]
    return d


def find_bin(name, dirs=None):
    names = [name + ".exe", name] if os.name == "nt" else [name]
    for d in (bin_dirs() if dirs is None else dirs):
        for n in names:
            p = os.path.join(d, n)
            if os.path.isfile(p):
                return p
    return None


def tempo_home(cfg):
    return os.path.expanduser(os.environ.get("TEMPO_HOME") or cfg.get("tempo_home") or "~/.tempo")


# The only pack.json keys a pack may have; anything else is ignored and noted.
PACK_KEYS = ("name", "description", "version", "enabled")
redact = lambda s: s  # tempo_agent.py sets this to its own redact() at import


def _json(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def needs_confirm(confirm, trust, server, tool):
    """Whether a tool asks the owner first. A named (server, tool) always asks. (server, "*"),
    which every pack connector gets, asks for each of its tools unless the owner's own
    "mcp_trust" names that tool; a pack can never set that. Same rule as the Rust agent."""
    return (server, tool) in confirm or ((server, "*") in confirm and (server, tool) not in trust)


def trust_pairs(cfg):
    """The owner's "mcp_trust": [["server", "tool"], ...] as a set of pairs."""
    return {(p[0], p[1]) for p in (cfg or {}).get("mcp_trust") or []
            if isinstance(p, (list, tuple)) and len(p) == 2 and all(isinstance(x, str) for x in p)}


def discover(cfg=None, dirs=None):
    """(servers, confirm, notes): what Tempo connects by itself on this machine."""
    cfg = cfg or {}
    servers, confirm, notes = {}, set(), []
    if cfg.get("auto_tools") is False or os.environ.get("TEMPO_AUTO_TOOLS", "1").strip().lower() in ("0", "false", "off", "no"):
        return servers, confirm, ["automatic tools are off"]
    vault, vb = os.environ.get("TEMPO_VAULT") or cfg.get("vault"), find_bin("tempo-vault", dirs)
    if vb and vault:
        v = os.path.expanduser(vault)
        if os.path.isdir(v):
            args = ["mcp"]
            if cfg.get("vault_write", True):
                args.append("--write")
                confirm |= {("vault", "vault_write"), ("vault", "vault_rename"), ("vault", "vault_trash")}
            servers["vault"] = {"command": vb, "args": args + ["--vault", v]}
            notes.append(f"vault: {v} ({vb})" + ("" if cfg.get("vault_write", True) else ", read-only"))
        else:
            notes.append(f"vault: not connected, {v} is not a folder")
    elif vb:
        notes.append('vault: tempo-vault is here; set TEMPO_VAULT (or "vault" in agent-config.json) to connect it')
    elif vault:
        notes.append("vault: a vault is set but tempo-vault was not found next to Tempo, in TEMPO_BIN or on PATH")
    sb = find_bin("tempo-studio", dirs)
    if sb:
        env_roots = os.environ.get("TEMPO_STUDIO_ROOTS")
        roots = env_roots.split(os.pathsep) if env_roots else (cfg.get("studio_roots") or [os.path.join(os.path.expanduser("~"), "Music")])
        roots = [os.path.expanduser(r) for r in roots if r and os.path.isdir(os.path.expanduser(r))]
        if roots:
            servers["studio"] = {"command": sb, "args": ["mcp"] + [a for r in roots for a in ("--allow-root", r)]}
            notes.append(f"studio: {', '.join(roots)} ({sb})")
        else:
            notes.append('studio: tempo-studio is here; set TEMPO_STUDIO_ROOTS (or "studio_roots") to the folders it may use')
    rb = find_bin("tempo-room", dirs)
    if rb:
        servers["room"] = {"command": rb, "args": ["mcp"]}
        confirm.add(("room", "room_post"))
        notes.append(f"room: {rb}")
    home = tempo_home(cfg)

    def read_connector(f, where):
        try:
            j = _json(f)
        except Exception:
            notes.append(f"{where}connector {f} skipped: not JSON")
            return None
        if j.get("enabled") is False:
            return None
        name = j.get("name") or os.path.splitext(os.path.basename(f))[0]
        if not re.fullmatch(r"[A-Za-z0-9_-]{1,40}", name) or not j.get("command"):
            notes.append(f"{where}connector {f} skipped: it needs a simple name and a command")
            return None
        return name, {"command": os.path.expanduser(j["command"]), "args": list(j.get("args") or []), "env": dict(j.get("env") or {})}, set(j.get("confirm") or [])

    # your own connectors may replace a product
    for f in sorted(glob.glob(os.path.join(home, "connectors", "*.json"))):
        c = read_connector(f, "")
        if c:
            name, srv, tools = c
            confirm = {x for x in confirm if x[0] != name} | {(name, t) for t in tools}
            servers[name] = srv
            notes.append(f"connector {name}: {f}")
    # a pack can only add: no replacing, no keys, and no settings
    for p in sorted(glob.glob(os.path.join(home, "packs", "*", ""))):
        pack = os.path.basename(os.path.normpath(p))
        try:
            man = _json(os.path.join(p, "pack.json"))
        except Exception:
            man = {}
        if man.get("enabled") is False:
            notes.append(f"pack {pack}: off")
            continue
        for k in man:
            if k not in PACK_KEYS:
                notes.append(f'pack {pack}: ignored "{k}" (packs can only add connectors, never change settings)')
        for f in sorted(glob.glob(os.path.join(p, "connectors", "*.json"))):
            c = read_connector(f, f"pack {pack}: ")
            if not c:
                continue
            name, srv, tools = c
            if name in servers:
                notes.append(f"pack {pack}: connector {name} skipped (packs can only add; {name} is already connected)")
            elif any(redact(str(v)) != str(v) for v in srv["env"].values()):
                notes.append(f"pack {pack}: connector {name} skipped (a pack must not carry keys; the program reads its own config)")
            else:
                # a pack comes from someone else: every one of its tools asks first
                servers[name] = srv
                confirm.add((name, "*"))
                notes.append(f"pack {pack}: connector {name}: {f} (every tool asks first)")
    return servers, confirm, notes


class McpServer:
    def __init__(self, name, cfg):
        self.name, self.cfg = name, cfg
        self.proc, self.tools, self.error = None, [], None
        self._ids, self._pending, self._lock = itertools.count(1), {}, threading.Lock()

    def start(self, timeout=60):
        env = dict(os.environ)
        env.update({k: str(v) for k, v in (self.cfg.get("env") or {}).items()})
        try:
            self.proc = subprocess.Popen(
                [self.cfg["command"], *self.cfg.get("args", [])], stdin=subprocess.PIPE,
                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env,
                creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
            threading.Thread(target=self._read, daemon=True).start()
            self._rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                     "clientInfo": {"name": "perennos-tempo-agent", "version": "1"}}, timeout)
            self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})
            self.tools = self._rpc("tools/list", {}, timeout).get("tools", [])
        except Exception as e:
            self.error = f"{type(e).__name__}: {e}"[:200]
            self.stop()
        return self

    def _send(self, msg):
        self.proc.stdin.write((json.dumps(msg) + "\n").encode())
        self.proc.stdin.flush()

    def _read(self):
        for line in self.proc.stdout:
            try:
                m = json.loads(line)
            except Exception:
                continue
            q = self._pending.get(m.get("id"))
            if q is not None and ("result" in m or "error" in m):
                q.put(m)
            elif "method" in m and "id" in m:  # server->client request: decline politely
                try:
                    self._send({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32601, "message": "unsupported"}})
                except Exception:
                    pass

    def _rpc(self, method, params, timeout):
        i = next(self._ids)
        q = queue.Queue()
        self._pending[i] = q
        self._send({"jsonrpc": "2.0", "id": i, "method": method, "params": params})
        try:
            m = q.get(timeout=timeout)
        except queue.Empty:
            raise TimeoutError(f"{self.name} {method} timed out after {timeout}s")
        finally:
            self._pending.pop(i, None)
        if "error" in m:
            raise RuntimeError(str(m["error"])[:300])
        return m["result"]

    def call(self, tool, args, timeout=300):
        r = self._rpc("tools/call", {"name": tool, "arguments": args or {}}, timeout)
        text = "\n".join(c.get("text", "") for c in r.get("content", []) if c.get("type") == "text")
        return ("ERROR: " if r.get("isError") else "") + text

    def stop(self):
        try:
            if self.proc and self.proc.poll() is None:
                self.proc.terminate()  # exact child only, never by name
        except Exception:
            pass
