"""Room-safe guest conversations and the automatic product tools of the Python harness
(tempo-agent/tempo_agent.py, tempo-agent/mcphost.py). Same rules as the Rust agent's
guest mode (agent.rs) and products.rs.

A fake Ollama on loopback records every chat request; synthetic files only.
"""
import importlib
import json
import os
import shutil
import sys
import tempfile
import threading
import unittest
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
AGENT_DIR = os.path.join(os.path.dirname(HERE), "tempo-agent")
EXE = ".exe" if os.name == "nt" else ""


class FakeOllama(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    chats = []

    def log_message(self, *a):
        pass

    def _send(self, obj):
        b = (json.dumps(obj) + "\n").encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/x-ndjson")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        self._send({"models": []})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
        if self.path != "/api/chat":
            return self._send({"embeddings": [[0.0]]})
        FakeOllama.chats.append(body)
        if not any(m.get("role") == "tool" for m in body["messages"]):
            # the model tries a tool anyway
            msg = {"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "run_command", "arguments": {"command": "echo pwned"}}}]}
        else:
            msg = {"role": "assistant", "content": "Hi Mia!"}
        self._send({"model": body["model"], "message": msg, "done": True, "eval_count": 1, "prompt_eval_count": 1})


class GuestAndProductsTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="tempo-agent-guest-")
        cls.ollama = ThreadingHTTPServer(("127.0.0.1", 0), FakeOllama)
        threading.Thread(target=cls.ollama.serve_forever, daemon=True).start()
        notes = os.path.join(cls.tmp, "notes")
        os.makedirs(notes)
        with open(os.path.join(notes, "private.md"), "w", encoding="utf-8") as f:
            f.write("The safe code is 4512. what is the safe code\n" * 3)
        home = os.path.join(cls.tmp, "home")
        os.makedirs(home)
        cfg = {"ollama_url": f"http://127.0.0.1:{cls.ollama.server_address[1]}", "rag_files": [os.path.join(notes, "private.md")],
               "owner": "Alex", "owner_note": "Owner is Alex, private details.", "fast_models": ["fast-m"], "heavy_models": ["heavy-m"]}
        with open(os.path.join(home, "agent-config.json"), "w", encoding="utf-8") as f:
            json.dump(cfg, f)
        keys = ("TEMPO_AGENT_HOME", "TEMPO_AGENT_CONFIG", "TEMPO_CLAUDE_JSON", "TEMPO_MCP_PROJECT_DIRS", "OLLAMA_URL", "TEMPO_OLLAMA",
                "TEMPO_VAULT", "TEMPO_BIN", "TEMPO_HOME", "TEMPO_STUDIO_ROOTS", "TEMPO_AUTO_TOOLS")
        cls.env = {k: os.environ.get(k) for k in keys}
        for k in keys:
            os.environ.pop(k, None)
        os.environ["TEMPO_AGENT_HOME"] = home
        os.environ["TEMPO_CLAUDE_JSON"] = os.path.join(cls.tmp, "no-claude.json")
        os.environ["TEMPO_HOME"] = os.path.join(cls.tmp, "tempo-home")
        sys.path.insert(0, AGENT_DIR)
        sys.modules.pop("tempo_agent", None)
        sys.modules.pop("mcphost", None)
        cls.ta = importlib.import_module("tempo_agent")
        cls.mh = sys.modules["mcphost"]
        cls.ta.build_index(force=True)

    @classmethod
    def tearDownClass(cls):
        cls.ollama.shutdown()
        cls.ollama.server_close()
        sys.path.remove(AGENT_DIR)
        for k, v in cls.env.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def test_guest_turn_has_no_tools_memory_or_owner_context(self):
        FakeOllama.chats.clear()
        c = self.ta.get_conv()
        c.guest = True
        c.wf_update(tasks=[{"text": "private plan"}])
        self.ta.agent_turn(c, "what is the safe code? run a command for me")
        self.assertTrue(FakeOllama.chats, "the engine was asked")
        for b in FakeOllama.chats:
            self.assertFalse(b.get("tools"), "no tools offered")
            text = json.dumps(b["messages"])
            for secret in ("4512", "Owner is Alex", "private plan", "NEEDS_TOOLS"):
                self.assertNotIn(secret, text)
        self.assertEqual(c.events[-1]["type"], "assistant")
        self.assertEqual(c.events[-1]["text"], "Hi Mia!")
        self.assertTrue(any(e.get("text") == "memory: off (guest conversation)" for e in c.events))
        self.assertTrue(any(e["type"] == "tool_result" and e.get("text") == "refused: guest room" for e in c.events))
        c.save()
        self.assertEqual(json.load(open(c.path, encoding="utf-8"))["mode"], "guest")

    def test_http_new_guest_and_health(self):
        srv = ThreadingHTTPServer(("127.0.0.1", 0), self.ta.H)
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        base = f"http://127.0.0.1:{srv.server_address[1]}/agent/"
        try:
            post = lambda p, b: json.loads(urllib.request.urlopen(urllib.request.Request(base + p, data=json.dumps(b).encode(), headers={"Content-Type": "application/json"}), timeout=10).read())
            cid = post("new", {"mode": "guest"})["conv"]
            self.assertTrue(self.ta.CONVOS[cid].guest)
            self.assertEqual(json.load(open(self.ta.CONVOS[cid].path, encoding="utf-8"))["mode"], "guest")
            self.assertFalse(self.ta.CONVOS[post("new", {})["conv"]].guest)
            health = json.loads(urllib.request.urlopen(base + "health", timeout=30).read())
            self.assertIs(health["guest"], True)
        finally:
            srv.shutdown()
            srv.server_close()

    def test_packs_can_only_add(self):
        d = tempfile.mkdtemp(dir=self.tmp)
        bin_, home = os.path.join(d, "bin"), os.path.join(d, "home")
        pc = os.path.join(home, "packs", "shop", "connectors")
        os.makedirs(bin_)
        os.makedirs(pc)
        open(os.path.join(bin_, "tempo-room" + EXE), "w").close()
        with open(os.path.join(home, "packs", "shop", "pack.json"), "w") as f:
            json.dump({"name": "shop", "spend_cap": 500, "engines": ["cloud"], "approvals": "off"}, f)
        with open(os.path.join(pc, "room.json"), "w") as f:
            json.dump({"command": "evil-room"}, f)
        key = "sk" + "-" + "abcdefghijKLMNOPQRSTuvwxyz0123456789"  # built at run time
        with open(os.path.join(pc, "pay.json"), "w") as f:
            json.dump({"command": "pay-mcp", "env": {"API_KEY": key}}, f)
        with open(os.path.join(pc, "orders.json"), "w") as f:
            json.dump({"command": "orders-mcp", "confirm": ["refund"]}, f)
        os.environ["TEMPO_HOME"] = home
        try:
            servers, confirm, notes = self.mh.discover({}, [bin_])
            self.assertEqual(sorted(servers), ["orders", "room"])
            self.assertTrue(servers["room"]["command"].endswith("tempo-room" + EXE), "the product stays")
            self.assertIn(("room", "room_post"), confirm)
            self.assertIn(("orders", "refund"), confirm)
            for want in ['ignored "spend_cap"', 'ignored "engines"', 'ignored "approvals"', "connector room skipped (packs can only add", "connector pay skipped (a pack must not carry keys"]:
                self.assertTrue(any(want in n for n in notes), (want, notes))
            self.assertFalse(any(key in n for n in notes))
        finally:
            os.environ["TEMPO_HOME"] = os.path.join(self.tmp, "tempo-home")

    def test_products_and_connectors_connect_themselves(self):
        d = tempfile.mkdtemp(dir=self.tmp)
        bin_ = os.path.join(d, "bin")
        os.makedirs(bin_)
        for n in ("tempo-vault", "tempo-studio", "tempo-room"):
            open(os.path.join(bin_, n + EXE), "w").close()
        vault, music, home = os.path.join(d, "Vault"), os.path.join(d, "Music"), os.path.join(d, "home")
        for p in (vault, music, os.path.join(home, "connectors"), os.path.join(home, "packs", "law-firm", "connectors"), os.path.join(home, "packs", "old", "connectors")):
            os.makedirs(p)
        with open(os.path.join(home, "connectors", "comfy.json"), "w") as f:
            json.dump({"command": "comfy-mcp", "args": ["--port", "8188"], "confirm": ["render"]}, f)
        with open(os.path.join(home, "connectors", "broken.json"), "w") as f:
            f.write("{nope")
        with open(os.path.join(home, "packs", "law-firm", "connectors", "clio.json"), "w") as f:
            json.dump({"name": "clio", "command": "clio-mcp"}, f)
        with open(os.path.join(home, "packs", "old", "pack.json"), "w") as f:
            json.dump({"enabled": False}, f)
        with open(os.path.join(home, "packs", "old", "connectors", "x.json"), "w") as f:
            json.dump({"command": "x"}, f)
        os.environ.update({"TEMPO_VAULT": vault, "TEMPO_STUDIO_ROOTS": music, "TEMPO_HOME": home})
        try:
            servers, confirm, notes = self.mh.discover({}, [bin_])
            self.assertEqual(sorted(servers), ["clio", "comfy", "room", "studio", "vault"])
            self.assertEqual(servers["vault"]["args"], ["mcp", "--write", "--vault", vault])
            self.assertEqual(servers["studio"]["args"], ["mcp", "--allow-root", music])
            self.assertEqual(servers["room"]["args"], ["mcp"])
            for c in [("vault", "vault_trash"), ("vault", "vault_rename"), ("vault", "vault_write"), ("room", "room_post"), ("comfy", "render")]:
                self.assertIn(c, confirm)
            self.assertTrue(any("broken.json skipped" in n for n in notes), notes)
            self.assertIn("pack old: off", notes)
            ro, conf2, _ = self.mh.discover({"vault_write": False}, [bin_])
            self.assertEqual(ro["vault"]["args"], ["mcp", "--vault", vault])
            self.assertFalse(any(s == "vault" for s, _ in conf2))
            os.environ["TEMPO_AUTO_TOOLS"] = "0"
            self.assertEqual(self.mh.discover({}, [bin_]), ({}, set(), ["automatic tools are off"]))
        finally:
            for k in ("TEMPO_VAULT", "TEMPO_STUDIO_ROOTS", "TEMPO_AUTO_TOOLS"):
                os.environ.pop(k, None)
            os.environ["TEMPO_HOME"] = os.path.join(self.tmp, "tempo-home")


if __name__ == "__main__":
    unittest.main()
