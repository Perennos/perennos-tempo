import io
import json
import os
import tempfile
import threading
import unittest
from email.message import Message
from pathlib import Path
from unittest.mock import patch
from urllib.request import ProxyHandler

_original_tempo_home = os.environ.get("TEMPO_HOME")
_test_home = tempfile.TemporaryDirectory(prefix="tempo-security-network-")
os.environ["TEMPO_HOME"] = _test_home.name
import tempo_client
import tempo_gateway
try:
    import tempo_local_http
except ImportError:
    tempo_local_http = None
import tempo_setup
if _original_tempo_home is None:
    os.environ.pop("TEMPO_HOME", None)
else:
    os.environ["TEMPO_HOME"] = _original_tempo_home


TOKEN = "t" * 48
PORT = 8765


class _NoopTimer:
    def __init__(self, *args, **kwargs):
        self.daemon = False

    def start(self):
        pass

    def cancel(self):
        pass


class _FakeSocket:
    def __init__(self):
        self.closed = False
        self.timeout = None

    def dup(self):
        return _FakeSocket()

    def settimeout(self, value):
        self.timeout = value

    def shutdown(self, _how):
        pass

    def close(self):
        self.closed = True


class _FakeResponse:
    def __init__(self, body, headers=None, status=200, will_close=False):
        self.body = body
        self.headers = {"Content-Length": str(len(body)), **(headers or {})}
        self.status = status
        self.will_close = will_close

    def getheader(self, name):
        return self.headers.get(name)

    def getheaders(self):
        return list(self.headers.items())

    def read1(self, size):
        body, self.body = self.body[:size], self.body[size:]
        return body


class _FakeConnection:
    def __init__(self, _host, _port, timeout=None, *, challenge_ok=True, proof_override=None):
        self.timeout = timeout
        self.challenge_ok = challenge_ok
        self.proof_override = proof_override
        self.sock = None
        self.requests = []
        self.responses = 0
        self.closed = False

    def connect(self):
        self.sock = _FakeSocket()

    def request(self, method, path, body=None, headers=None):
        self.requests.append((method, path, body, dict(headers or {})))

    def getresponse(self):
        self.responses += 1
        if self.responses == 1 and self.requests[-1][1] == "/health" and tempo_client.CHALLENGE_HEADER in self.requests[-1][3]:
            nonce = self.requests[-1][3][tempo_client.CHALLENGE_HEADER]
            proof = tempo_client._gateway_proof(TOKEN, nonce, PORT)
            if self.proof_override is not None:
                proof = self.proof_override
            if not self.challenge_ok:
                proof = "0" * 64
            return _FakeResponse(b'{"status":"ok"}', {tempo_client.PROOF_HEADER: proof})
        return _FakeResponse(b'{"ok":true}')

    def close(self):
        self.closed = True
        if self.sock is not None:
            self.sock.close()
            self.sock = None


class ClientTrustTests(unittest.TestCase):
    def _call(self, *, challenge_ok=True, proof_override=None, method="GET", path="/v1/models",
              callback=None, installed_guard=None):
        instance = None

        def make_connection(host, port, timeout=None):
            nonlocal instance
            instance = _FakeConnection(host, port, timeout, challenge_ok=challenge_ok,
                                       proof_override=proof_override)
            return instance

        with (patch.object(tempo_client.http.client, "HTTPConnection", side_effect=make_connection),
              patch.object(tempo_client.threading, "Timer", _NoopTimer),
              patch.object(tempo_client, "_remaining"),
              patch.object(tempo_client, "_installed_windows_owner_guard", return_value=installed_guard)):
            self.last_connection = None
            try:
                result = tempo_client.request_json("127.0.0.1", PORT, TOKEN, method, path,
                                                   {"prompt": "secret"} if method == "POST" else None,
                                                   on_connection=callback)
            except Exception:
                self.last_connection = instance
                raise
            self.last_connection = instance
        return result, instance

    def test_foreign_listener_fails_closed_before_bearer_or_body(self):
        with self.assertRaises(tempo_client.TempoAPIError):
            self._call(challenge_ok=False)
        self.assertEqual(len(self.last_connection.requests), 1)
        self.assertEqual(self.last_connection.requests[0][:3], ("GET", "/health", None))
        self.assertNotIn("Authorization", self.last_connection.requests[0][3])

        seen = []
        with self.assertRaises(tempo_client.TempoAPIError):
            self._call(challenge_ok=False, method="POST", path="/v1/chat/completions",
                       callback=lambda _active: seen.append("callback"))
        self.assertEqual(seen, ["callback"])
        self.assertEqual(len(self.last_connection.requests), 1)
        self.assertEqual(self.last_connection.requests[0][:3], ("GET", "/health", None))
        self.assertNotIn("Authorization", self.last_connection.requests[0][3])

    def test_valid_same_connection_proof_allows_auth_only_on_followup(self):
        payload, conn = self._call()
        self.assertEqual(payload[0], {"ok": True})
        self.assertEqual(len(conn.requests), 2)
        challenge, request = conn.requests
        self.assertEqual((challenge[0], challenge[1], challenge[2]), ("GET", "/health", None))
        self.assertIn(tempo_client.CHALLENGE_HEADER, challenge[3])
        self.assertNotIn("Authorization", challenge[3])
        self.assertIn("Authorization", request[3])
        self.assertEqual(request[3]["Authorization"], "Bearer " + TOKEN)

    def test_malformed_non_ascii_proof_fails_closed(self):
        with self.assertRaises(tempo_client.TempoAPIError):
            self._call(proof_override="\u00e9" * 64)
        self.assertEqual(len(self.last_connection.requests), 1)
        self.assertNotIn("Authorization", self.last_connection.requests[0][3])

    def test_deeply_nested_response_is_a_bounded_api_error(self):
        with patch.object(tempo_client.json, "loads", side_effect=RecursionError):
            with self.assertRaises(tempo_client.TempoAPIError):
                self._call()

    def test_bootstrap_owner_callback_is_preserved_without_receipt_requirement(self):
        import tempo_bootstrap

        guard = tempo_bootstrap._connection_owner_guard({}, Path("."), PORT)

        def receipt_must_not_be_loaded(_port):
            raise AssertionError("bootstrap already supplied the verified listener guard")

        with (patch.object(tempo_bootstrap, "_assert_owned_listener"),
              patch.object(tempo_bootstrap, "_assert_connected_peer")):
            _, conn = self._call(callback=guard, installed_guard=receipt_must_not_be_loaded)
        self.assertEqual(len(conn.requests), 2)

    def test_health_never_sends_bearer_or_challenge(self):
        _, conn = self._call(method="GET", path="/health")
        self.assertEqual(len(conn.requests), 1)
        self.assertEqual(conn.requests[0][0:3], ("GET", "/health", None))
        self.assertNotIn("Authorization", conn.requests[0][3])
        self.assertNotIn(tempo_client.CHALLENGE_HEADER, conn.requests[0][3])

    def test_desktop_cancellation_callback_composes_after_trusted_guard(self):
        events = []

        def owner(_active):
            events.append("owner")

        def cancel_registration(_active):
            events.append("cancel")

        _, conn = self._call(callback=cancel_registration, installed_guard=owner)
        self.assertEqual(events, ["owner", "cancel"])
        self.assertEqual(conn.requests[0][1], "/health")


class LocalHTTPBoundaryTests(unittest.TestCase):
    @unittest.skipIf(tempo_local_http is None, "candidate-only local HTTP boundary")
    def test_opener_disables_environment_proxy_and_redirects_but_allows_lan(self):
        fake_opener = type("Opener", (), {"open": lambda self, request, timeout: (request, timeout)})()
        with patch.object(tempo_local_http, "build_opener", return_value=fake_opener) as build:
            request, timeout = tempo_local_http.open_local("http://10.23.45.67:11434/api/tags", timeout=5)
        self.assertEqual(request, "http://10.23.45.67:11434/api/tags")
        self.assertEqual(timeout, 5)
        handlers = build.call_args.args
        self.assertIsInstance(handlers[0], ProxyHandler)
        self.assertEqual(handlers[0].proxies, {})
        no_redirect = next(handler for handler in handlers if isinstance(handler, tempo_local_http._NoRedirect))
        self.assertIsNone(no_redirect.redirect_request(None, None, 302, "Found", Message(), "http://elsewhere/"))

    @unittest.skipIf(tempo_local_http is None, "candidate-only local HTTP boundary")
    def test_invalid_local_runtime_urls_are_rejected_without_opening(self):
        with patch.object(tempo_local_http, "build_opener") as build:
            for url in ("file:///tmp/model", "http:///missing-host", "http://[bad/"):
                with self.subTest(url=url), self.assertRaises(ValueError):
                    tempo_local_http.open_local(url, timeout=1)
        build.assert_not_called()

    def test_offline_probe_uses_parsed_loopback_authority(self):
        for url in ("http://127.0.0.1:11434", "https://localhost:11434", "http://[::1]:11434"):
            with self.subTest(url=url):
                self.assertTrue(tempo_setup._offline_loopback_url(url))
        for url in ("http://127.0.0.1.attacker:11434", "http://localhost.attacker:11434",
                    "https://example.test/127.0.0.1", "file:///127.0.0.1"):
            with self.subTest(url=url):
                self.assertFalse(tempo_setup._offline_loopback_url(url))


class GatewayBoundaryTests(unittest.TestCase):
    def test_non_ascii_bearer_is_rejected_as_unauthorized_input(self):
        self.assertFalse(tempo_gateway.authorized("Bearer " + "t" * 47 + "\u00e9", TOKEN))

    def test_gateway_and_client_share_port_bound_proof(self):
        nonce = "A" * 43
        self.assertEqual(tempo_client._gateway_proof(TOKEN, nonce, PORT),
                         tempo_gateway._gateway_proof(TOKEN, nonce, PORT))
        self.assertNotEqual(tempo_gateway._gateway_proof(TOKEN, nonce, PORT),
                            tempo_gateway._gateway_proof(TOKEN, nonce, PORT + 1))

    def test_local_ollama_stream_uses_the_proxy_free_opener(self):
        import tempo_runtime

        class Response:
            def __iter__(self):
                return iter([b'{"message":{"content":"ok"},"done":true}\n'])

            def close(self):
                pass

        class FakeOpener:
            def __init__(self):
                self.opened = []

            def open(self, request, timeout):
                self.opened.append((request, timeout))
                return Response()

        fake_opener = FakeOpener()
        with (patch.object(tempo_runtime, "request", return_value=("/api/chat", {"messages": []})),
              patch.object(tempo_runtime, "policy", return_value={"think": True}),
              patch.object(tempo_gateway.urllib.request, "urlopen", return_value=Response()) as ambient_open):
            if tempo_local_http is None:
                chunks = list(tempo_gateway._ollama_chunks(
                    "http://10.23.45.67:11434", "test-model", [], 0.2, 16, 5))
            else:
                with patch.object(tempo_local_http, "build_opener", return_value=fake_opener) as build:
                    chunks = list(tempo_gateway._ollama_chunks(
                        "http://10.23.45.67:11434", "test-model", [], 0.2, 16, 5))
                handlers = build.call_args.args
                self.assertEqual(handlers[0].proxies, {})
                self.assertTrue(any(isinstance(h, tempo_local_http._NoRedirect) for h in handlers))
                self.assertEqual(len(fake_opener.opened), 1)
                self.assertEqual(fake_opener.opened[0][0].full_url, "http://10.23.45.67:11434/api/chat")
        self.assertFalse(ambient_open.called, "local engine traffic must not use ambient urllib proxies")
        self.assertEqual(chunks[-1]["text"], "ok")

    def test_handler_setup_applies_a_finite_read_timeout(self):
        class FakeSocket:
            def __init__(self):
                self.timeouts = []
                self.files = []

            def settimeout(self, value):
                self.timeouts.append(value)

            def makefile(self, *_args):
                stream = io.BytesIO()
                self.files.append(stream)
                return stream

        handler_type = tempo_gateway.make_handler(object(), TOKEN)
        handler = handler_type.__new__(handler_type)
        handler.request = FakeSocket()
        handler.setup()
        self.assertEqual(handler.request.timeouts, [tempo_gateway.PREAUTH_READ_TIMEOUT])
        self.assertGreater(tempo_gateway.PREAUTH_READ_TIMEOUT, 0)

    def test_handler_capacity_is_capped_without_spawning_unbounded_threads(self):
        class FakeSocket:
            def __init__(self):
                self.sent = []
                self.timeout = None
                self.closed = False

            def settimeout(self, value):
                self.timeout = value

            def sendall(self, data):
                self.sent.append(data)

        server = tempo_gateway._CappedThreadingHTTPServer.__new__(tempo_gateway._CappedThreadingHTTPServer)
        server._handler_slots = threading.BoundedSemaphore(1)
        server.closed_requests = []
        server.shutdown_request = lambda request: server.closed_requests.append(request)
        accepted, rejected = FakeSocket(), FakeSocket()
        with patch.object(tempo_gateway.ThreadingHTTPServer, "process_request") as start_thread:
            server.process_request(accepted, ("local", 1))
            server.process_request(rejected, ("local", 2))
        start_thread.assert_called_once()
        self.assertEqual(rejected.timeout, 1)
        self.assertIn(b"503 Service Unavailable", rejected.sent[0])
        self.assertEqual(server.closed_requests, [rejected])
        server._handler_slots.release()


if __name__ == "__main__":
    unittest.main()
