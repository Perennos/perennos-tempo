"""Real loopback protocol boundaries; no models, credentials or external network."""
import contextlib
import json
import os
from pathlib import Path
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace
from unittest.mock import patch
from urllib.error import HTTPError
from urllib.request import Request

os.environ.setdefault("TEMPO_HOME", tempfile.gettempdir())
import tempo_client
import tempo_gateway

TOKEN = "synthetic-test-token-" * 3


@contextlib.contextmanager
def server(handler):
    instance = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    thread = threading.Thread(target=instance.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)
    thread.start()
    try:
        yield instance.server_address[1]
    finally:
        instance.shutdown()
        instance.server_close()
        thread.join(timeout=2)


class SecurityRoundtrip(unittest.TestCase):
    def test_source_headless_client_and_real_gateway_complete_private_request(self):
        with tempfile.TemporaryDirectory() as folder:
            home = Path(folder)
            (home / "config.json").write_text("{}")
            def answer(route, **_):
                return {"model": "synthetic", "choices": [{"message": {"content": "synthetic answer"}, "finish_reason": "stop"}],
                        "_tempo": {"receipt_id": "a" * 32, "completion_receipt_id": "b" * 32, "job_id": "c" * 32}}
            gateway = SimpleNamespace(data_dir=home, run=answer, models=lambda: [])
            with server(tempo_gateway.make_handler(gateway, TOKEN)) as port, patch.dict(os.environ, {"TEMPO_HOME": folder}):
                models, _ = tempo_client.list_models("127.0.0.1", port, TOKEN)
                self.assertTrue(models["data"])
                result, headers = tempo_client.chat("127.0.0.1", port, TOKEN, "synthetic private question", timeout=3)
                self.assertEqual(result["choices"][0]["message"]["content"], "synthetic answer")
                self.assertEqual(headers["x-tempo-completion-receipt-id"], "b" * 32)

    def test_foreign_http_listener_receives_neither_bearer_nor_private_body(self):
        seen = []
        class Foreign(BaseHTTPRequestHandler):
            def do_GET(self):
                seen.append((self.command, dict(self.headers), b""))
                self.send_response(200); self.send_header("Content-Length", "2"); self.end_headers(); self.wfile.write(b"{}")
            def do_POST(self):
                seen.append((self.command, dict(self.headers), self.rfile.read(int(self.headers.get("Content-Length", 0)))))
                self.send_response(200); self.send_header("Content-Length", "2"); self.end_headers(); self.wfile.write(b"{}")
            def log_message(self, *_):
                pass
        with server(Foreign) as port, tempfile.TemporaryDirectory() as folder, patch.dict(os.environ, {"TEMPO_HOME": folder}):
            with self.assertRaises(tempo_client.TempoAPIError):
                tempo_client.chat("127.0.0.1", port, TOKEN, "PRIVATE_BODY_MUST_NOT_BE_SENT", timeout=3)
        self.assertEqual(len(seen), 1)
        self.assertEqual(seen[0][0], "GET")
        self.assertNotIn("Authorization", seen[0][1])
        self.assertEqual(seen[0][2], b"")

    def test_local_post_bypasses_hostile_proxy_and_refuses_redirect(self):
        from tempo_local_http import open_local
        received = []
        class Destination(BaseHTTPRequestHandler):
            def do_POST(self):
                received.append(self.rfile.read(int(self.headers.get("Content-Length", 0))))
                self.send_response(200); self.send_header("Content-Length", "2"); self.end_headers(); self.wfile.write(b"{}")
            def log_message(self, *_):
                pass
        with server(Destination) as target:
            class Redirect(Destination):
                def do_POST(self):
                    self.rfile.read(int(self.headers.get("Content-Length", 0)))
                    self.send_response(307); self.send_header("Location", f"http://127.0.0.1:{target}/sink")
                    self.send_header("Content-Length", "0"); self.end_headers()
            with server(Redirect) as redirect, patch.dict(os.environ, {"HTTP_PROXY": "http://127.0.0.1:1", "http_proxy": "http://127.0.0.1:1", "NO_PROXY": "", "no_proxy": ""}):
                with open_local(Request(f"http://127.0.0.1:{target}/local", b"synthetic"), timeout=2) as response:
                    self.assertEqual(response.status, 200)
                with self.assertRaises(HTTPError) as refused:
                    open_local(Request(f"http://127.0.0.1:{redirect}/local", b"must-not-follow"), timeout=2)
                refused.exception.close()
        self.assertEqual(received, [b"synthetic"])
