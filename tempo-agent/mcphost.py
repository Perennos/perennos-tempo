"""Minimal stdio MCP client (stdlib only). Loads the same mcpServers Claude Code uses.
Never prints env values. One reader thread per server; calls are matched by JSON-RPC id."""
import json, os, subprocess, threading, itertools, queue, time

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
