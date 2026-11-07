"""A mock Ollama for the Room rehearsal: /api/version, /api/tags, /api/ps, /api/show and a
streaming /api/chat whose answer depends on a keyword in the first line of the last user
message (the room's "[Perennos Tempo room #id] Speaker: text" line). Every chat body is logged.

Usage: python3 -I mock_chat.py PORT LOG_FILE PWNED_FILE (rehearse.py starts it)."""
import json, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT, LOG, PWNED = int(sys.argv[1]), sys.argv[2], sys.argv[3]
lock = threading.Lock()
FAKE_KEY = "sk-" + "A" * 32  # an API-key-shaped string the room must redact


def answer_for(first_line):
    if "THINKTEST" in first_line:
        return ["<think>hidden reasoning about", " the owner</think>", "Visible answer."], 0, None
    if "LONGTEST" in first_line:
        return ["word " * 100 for _ in range(8)], 0, None
    if "KEYTEST" in first_line:
        return ["Here is a key: ", FAKE_KEY, " keep it safe."], 0, None
    if "SLOWTEST" in first_line:
        return ["slow ", "answer ", "part ", "four ", "done."], 0.8, None
    if "TOOLTEST" in first_line:
        # a guest question that makes the model try a command: it must never run
        call = {"function": {"name": "run_command", "arguments": {"command": "echo pwned > " + PWNED}}}
        return [], 0, call
    return ["Mock answer to: ", first_line[-60:]], 0, None


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _json(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        if self.path.startswith("/api/version"):
            return self._json(200, {"version": "0.40.6"})
        if self.path.startswith("/api/tags"):
            return self._json(200, {"models": [{"name": "mock:4b", "model": "mock:4b", "size": 2_500_000_000,
                                                "details": {"family": "qwen3", "parameter_size": "4.0B"}}]})
        if self.path.startswith("/api/ps"):
            return self._json(200, {"models": []})
        return self._json(404, {"error": "not found"})

    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n) if n else b""
        try:
            body = json.loads(raw or b"{}")
        except ValueError:
            body = {"unparsed": raw[:200].decode("utf-8", "replace")}
        with lock, open(LOG, "a") as f:
            f.write(json.dumps({"path": self.path, "body": body}) + "\n")
        if self.path.startswith("/api/show"):
            return self._json(200, {"capabilities": ["completion", "tools"], "model_info": {"qwen3.context_length": 32768}})
        if not self.path.startswith("/api/chat"):
            return self._json(404, {"error": "not in this mock"})
        users = [m for m in body.get("messages", []) if m.get("role") == "user"]
        first = (users[-1].get("content") or "").split("\n", 1)[0] if users else ""
        pieces, delay, call = answer_for(first)
        self.send_response(200)
        self.send_header("Content-Type", "application/x-ndjson")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

        def chunk(obj):
            data = (json.dumps(obj) + "\n").encode()
            self.wfile.write(b"%x\r\n%s\r\n" % (len(data), data))
            self.wfile.flush()

        for p in pieces:
            chunk({"model": body.get("model"), "message": {"role": "assistant", "content": p}, "done": False})
            if delay:
                time.sleep(delay)
        if call:
            chunk({"model": body.get("model"), "message": {"role": "assistant", "content": "", "tool_calls": [call]}, "done": False})
        chunk({"model": body.get("model"), "message": {"role": "assistant", "content": ""}, "done": True,
               "done_reason": "stop", "prompt_eval_count": 12, "eval_count": 7,
               "load_duration": 1000000, "eval_duration": 2000000})
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()

    def log_message(self, *a):
        pass


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
