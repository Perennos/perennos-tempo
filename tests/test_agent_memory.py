"""Second-brain endpoints of the local agent (tempo-agent/tempo_agent.py): memory list, excerpts, graph.

Synthetic files only; the engine URL points at a closed port, so embeddings fail and the index is
keyword-only, as it is when Ollama is down.
"""
import importlib
import json
import os
import shutil
import sys
import tempfile
import threading
import unittest
import urllib.error
import urllib.request
from http.server import ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
AGENT_DIR = os.path.join(os.path.dirname(HERE), "tempo-agent")


class AgentMemoryTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="tempo-agent-memory-")
        notes = os.path.join(cls.tmp, "notes", "memory")
        os.makedirs(notes)
        topics = ["rust crate agent loop lanes tests", "gpu lease comfyui render vram", "music mastering stream tracks"]
        for i in range(9):
            words = (topics[i % 3] + " ") * 6
            with open(os.path.join(notes, f"note-{i}.md"), "w", encoding="utf-8") as f:
                f.write(f"---\nname: note {i}\n---\n\n{words}\n\n{words}\n")
        cls.core = os.path.join(cls.tmp, "notes", "ABOUT.md")
        secret = "api_key = " + "abcdef" * 4  # redaction must hide this in excerpts
        with open(cls.core, "w", encoding="utf-8") as f:
            f.write(f"# About\n\nrust crate agent loop and gpu lease notes.\n\n{secret}\n")
        cfg = {"ollama_url": "http://127.0.0.1:9", "rag_files": [cls.core],
               "rag_globs": [[os.path.join(notes, "*.md"), 100]]}
        home = os.path.join(cls.tmp, "home")
        os.makedirs(home)
        with open(os.path.join(home, "agent-config.json"), "w", encoding="utf-8") as f:
            json.dump(cfg, f)
        cls.env = {k: os.environ.get(k) for k in ("TEMPO_AGENT_HOME", "TEMPO_AGENT_CONFIG", "TEMPO_CLAUDE_JSON",
                                                    "TEMPO_MCP_PROJECT_DIRS", "OLLAMA_URL", "TEMPO_OLLAMA")}
        for k in cls.env:
            os.environ.pop(k, None)
        os.environ["TEMPO_AGENT_HOME"] = home
        os.environ["TEMPO_CLAUDE_JSON"] = os.path.join(cls.tmp, "no-claude.json")
        sys.path.insert(0, AGENT_DIR)
        sys.modules.pop("tempo_agent", None)
        sys.modules.pop("mcphost", None)
        cls.ta = importlib.import_module("tempo_agent")
        cls.ta.build_index(force=True)

    @classmethod
    def tearDownClass(cls):
        sys.path.remove(AGENT_DIR)
        for k, v in cls.env.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def test_files_have_kinds_and_counts(self):
        files = {os.path.basename(f["src"]): f for f in self.ta.memory_files()}
        self.assertEqual(len(files), 10)
        self.assertEqual(files["ABOUT.md"]["kind"], "core")
        self.assertEqual(files["note-0.md"]["kind"], "memory")
        self.assertTrue(all(f["chunks"] >= 1 for f in files.values()))

    def test_excerpts_only_for_indexed_files_and_redacted(self):
        r = self.ta.memory_file(self.core)
        self.assertEqual(r["name"], "ABOUT.md")
        text = "\n".join(r["chunks"])
        self.assertIn("[REDACTED]", text)
        self.assertNotIn("abcdef" * 4, text)
        self.assertIsNone(self.ta.memory_file(os.path.abspath(__file__)))
        self.assertIsNone(self.ta.memory_file(""))

    def test_redacts_token_shapes(self):
        a = "abcdefghijKLMNOPQRSTuvwxyz0123456789"  # tokens are built at run time: the leak scan must never match
        for t in ["sk-" + a, "sk_live_" + a, "rk_test_" + a, "ghp_" + a, "gho_" + a, "github_pat_11" + a,
                  "xoxb-1234-" + a, "hf_" + a, "AKIA" + "ABCDEFGHIJKLMNOP", "eyJ%s.eyJ%s.%s" % (a, a, a)]:
            self.assertEqual(self.ta.redact("value %s end" % t), "value [REDACTED] end", t)
        pem = "before\n-----BEGIN RSA " + "PRIVATE KEY-----\nMIIEv" + a + "\n-----END RSA " + "PRIVATE KEY-----\nafter"
        self.assertEqual(self.ta.redact(pem), "before\n[REDACTED PRIVATE KEY]\nafter")
        self.assertEqual(self.ta.redact("https://riley:hunter2pass@git.example.com/r.git"), "https://[REDACTED]@git.example.com/r.git")
        for plain in ["see https://example.com/path?q=1", "the sk_ prefix alone"]:
            self.assertEqual(self.ta.redact(plain), plain)

    def test_graph_links_files_that_share_terms(self):
        g = self.ta.memory_graph()
        self.assertEqual(len(g["nodes"]), 10)
        n = {x["id"]: os.path.basename(x["src"]) for x in g["nodes"]}
        pairs = {frozenset((n[e["a"]], n[e["b"]])) for e in g["edges"]}
        self.assertIn(frozenset(("note-0.md", "note-3.md")), pairs)  # same topic
        topic = {f"note-{i}.md": i % 3 for i in range(9)}
        for a, b in map(tuple, pairs):
            if a in topic and b in topic:
                self.assertEqual(topic[a], topic[b], (a, b))
        for e in g["edges"]:
            self.assertLess(e["a"], e["b"])
            self.assertTrue(0 < e["w"] <= 1, e)
        self.assertIs(self.ta.memory_graph(), g, "unchanged index: cached")

    def test_http_routes(self):
        srv = ThreadingHTTPServer(("127.0.0.1", 0), self.ta.H)
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        base = f"http://127.0.0.1:{srv.server_address[1]}/agent/"
        try:
            get = lambda p: json.loads(urllib.request.urlopen(base + p, timeout=10).read())
            m = get("memory?conv=no-such-conversation")
            self.assertEqual((len(m["files"]), m["used"]), (10, []))
            self.assertNotIn("no-such-conversation", self.ta.CONVOS, "reading memory never creates a conversation")
            g = get("memory/graph?conv=no-such-conversation")
            self.assertEqual(len(g["nodes"]), 10)
            self.assertFalse(any(n["used"] for n in g["nodes"]))
            with self.assertRaises(urllib.error.HTTPError) as ctx:
                urllib.request.urlopen(base + "memory/file?src=" + urllib.request.quote(os.path.abspath(__file__)), timeout=10)
            self.assertEqual(ctx.exception.code, 404)
        finally:
            srv.shutdown()
            srv.server_close()


if __name__ == "__main__":
    unittest.main()
