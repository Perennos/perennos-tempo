"""A mock Ollama for the Room rehearsal: /api/version, /api/tags, /api/ps, /api/show and
/api/chat, whose answer depends on a keyword in the first line of the last user message (the
room's "[Perennos Tempo room #id] Speaker: text" line). With "stream": false (how an engine
teammate calls it) the answer is one JSON reply; otherwise it streams (how the agent calls it).
Every chat body is logged.

An owner conversation (an owner-only room) asks for OWNERTOOL: the model then wants to run one
command that writes a file (RUN_DIR/owner-approved.txt, or owner-denied.txt when the text says
DENY); once the tool's result is back it answers in words.

Usage: python3 -I mock_chat.py PORT LOG_FILE PWNED_FILE [RUN_DIR] (rehearse.py starts it)."""
import json, re, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT, LOG, PWNED = int(sys.argv[1]), sys.argv[2], sys.argv[3]
RUN_DIR = sys.argv[4] if len(sys.argv) > 4 else ""
lock = threading.Lock()
FAKE_KEY = "sk-" + "A" * 32  # an API-key-shaped string the room must redact


def answer_for(first_line, system=""):
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
    # an engine teammate's system message starts "You are <Name>. <role>"
    m = re.match(r"You are ([A-Za-z0-9-]{1,32})\. ", system)
    who = m.group(1) if m else ""
    return [(who + " here. " if who else "") + "Mock answer to: ", first_line[-60:]], 0, None


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
        system = " ".join(m.get("content") or "" for m in body.get("messages", []) if m.get("role") == "system")
        pieces, delay, call = answer_for(first, system)
        full = (users[-1].get("content") or "") if users else ""
        if "OWNERTOOL" in full:
            # the owner's own conversation (the request's text is on the lines after the header)
            if body.get("messages") and body["messages"][-1].get("role") == "tool":
                pieces, delay, call = ["Owner tool finished."], 0, None
            else:
                name = "owner-denied.txt" if "DENY" in full else "owner-approved.txt"
                path = (RUN_DIR + "/" if RUN_DIR else "") + name
                pieces, delay, call = [], 0, {"function": {"name": "run_command", "arguments": {"command": "echo owner-approved > " + path}}}
        if body.get("stream") is False:
            # one JSON reply, the way a direct chat (no streaming) is answered
            message = {"role": "assistant", "content": "".join(pieces)}
            if call:
                message["tool_calls"] = [call]
            time.sleep(delay * len(pieces))
            return self._json(200, {"model": body.get("model"), "message": message, "done": True, "done_reason": "stop",
                                    "prompt_eval_count": 12, "eval_count": 7})
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
