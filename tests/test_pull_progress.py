import json
import http.server
import socket
import sys
import threading
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import tempo_pull


PORT = 11435
OWNER = {"pid": 1234, "port": PORT}
EXECUTABLE = Path("owned-ollama.exe")


class _Clock:
    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now

    def advance(self, seconds):
        self.now += seconds


class _Socket:
    def __init__(self):
        self.timeout = None
        self.closed = False
        self.shutdown_calls = 0

    def settimeout(self, timeout):
        self.timeout = timeout

    def dup(self):
        return _Socket()

    def shutdown(self, _how):
        self.shutdown_calls += 1

    def close(self):
        self.closed = True


class _Response:
    def __init__(self, connection, clock, packets, *, status=200, stall=False):
        self.connection = connection
        self.clock = clock
        self.packets = list(packets)
        self.status = status
        self.stall = stall
        self.closed = False

    def read1(self, size):
        if not self.packets:
            if self.stall:
                self.clock.advance(self.connection.sock.timeout)
                raise socket.timeout("synthetic idle read")
            return b""
        block, delay = self.packets.pop(0)
        if delay > self.connection.sock.timeout:
            self.clock.advance(self.connection.sock.timeout)
            raise socket.timeout("synthetic progress deadline")
        self.clock.advance(delay)
        return block[:size]

    def close(self):
        self.closed = True


class _Connection:
    def __init__(self, host, port, timeout, clock, packets, *, status=200, stall=False):
        self.host = host
        self.port = port
        self.timeout = timeout
        self.clock = clock
        self.packets = packets
        self.status = status
        self.stall = stall
        self.sock = None
        self.requests = []
        self.closed = False
        self.response = None

    def connect(self):
        self.sock = _Socket()

    def request(self, method, path, body=None, headers=None):
        self.requests.append((method, path, body, dict(headers or {})))

    def getresponse(self):
        self.response = _Response(self, self.clock, self.packets, status=self.status, stall=self.stall)
        return self.response

    def close(self):
        self.closed = True
        if self.sock is not None:
            self.sock.close()


def _packets(events):
    return [(json.dumps(event, separators=(",", ":")).encode("utf-8") + b"\n", delay)
            for event, delay in events]


class PullProgressTests(unittest.TestCase):
    def _harness(self, responses, *, idle_timeout=6, overall_timeout=120,
                 owner_check=None, peer_check=None, max_attempts=3):
        clock = _Clock()
        connections = []

        def connection_factory(host, port, timeout):
            self.assertEqual(host, "127.0.0.1")
            self.assertEqual(port, PORT)
            if connections:
                self.assertGreater(connections[-1].sock.shutdown_calls, 0,
                                   "the previous pull transport must be shut down before retry connect")
            spec = responses[len(connections)]
            conn = _Connection(host, port, timeout, clock, spec[0],
                               status=spec[1] if len(spec) > 1 else 200,
                               stall=spec[2] if len(spec) > 2 else False)
            connections.append(conn)
            return conn

        def assert_owned(record, executable, port):
            self.assertIs(record, OWNER)
            self.assertIs(executable, EXECUTABLE)
            self.assertEqual(port, PORT)
            if owner_check:
                owner_check()

        def assert_peer(sock, record, executable, port):
            self.assertIs(sock, connections[-1].sock)
            if peer_check:
                peer_check()

        def run():
            with patch.object(tempo_pull.http.client, "HTTPConnection", side_effect=connection_factory):
                return tempo_pull.pull_owned_model(
                    "qwen3:4b", PORT, OWNER, EXECUTABLE,
                    assert_owned_listener=assert_owned,
                    assert_connected_peer=assert_peer,
                    idle_timeout=idle_timeout,
                    overall_timeout=overall_timeout,
                    max_attempts=max_attempts,
                    clock=clock,
                )

        return clock, connections, run

    def test_repeated_zero_heartbeat_stalls_then_retry_completes(self):
        heartbeat = {"status": "pulling layer", "digest": "sha256:one", "completed": 0, "total": 100}
        first = _packets([(heartbeat, 0)] + [({**heartbeat, "total": 100 + index}, 1) for index in range(1, 6)])
        second = _packets([
            (heartbeat, 0),
            ({"status": "pulling layer", "digest": "sha256:one", "completed": 1}, 1),
            ({"status": "pulling layer", "digest": "sha256:one", "completed": 2, "total": 100}, 1),
            ({"status": "success"}, 1),
        ])
        clock, connections, run = self._harness([(first, 200, True), (second, 200, False)])

        self.assertIsNone(run())
        self.assertEqual(len(connections), 2)
        self.assertEqual(clock.now, 9)
        method, path, body, headers = connections[1].requests[0]
        self.assertEqual((method, path), ("POST", "/api/pull"))
        self.assertIn(b'"name":"qwen3:4b"', body)
        self.assertEqual(set(headers), {"Accept", "Content-Type", "Connection"})
        self.assertTrue(all(connection.closed for connection in connections))

    def test_completed_advances_per_digest_and_status_changes_are_progress(self):
        events = [
            ({"status": "pulling manifest"}, 0),
            ({"status": "pulling layer A", "digest": "sha256:a", "completed": 0, "total": 3}, 1),
            ({"status": "pulling layer A", "digest": "sha256:a", "completed": 1}, 1),
            ({"status": "pulling layer A", "digest": "sha256:a", "completed": 2, "total": 4}, 1),
            ({"status": "pulling layer B", "digest": "sha256:b", "completed": 0}, 1),
            ({"status": "pulling layer B", "digest": "sha256:b", "completed": 1}, 1),
            ({"status": "verifying sha256 digest"}, 1),
            ({"status": "success"}, 1),
        ]
        clock, connections, run = self._harness([(_packets(events), 200, False)])

        self.assertIsNone(run())
        self.assertEqual(len(connections), 1)
        self.assertEqual(clock.now, 7)

    def test_repeated_heartbeat_is_bounded_to_three_attempts(self):
        heartbeat = {"status": "pulling layer", "digest": "sha256:one", "completed": 0}
        packets = _packets([(heartbeat, 0)] + [(heartbeat, 1)] * 5)
        clock, connections, run = self._harness([(packets, 200, True)] * 3)

        with self.assertRaisesRegex(tempo_pull.PullError, "after 3 attempts"):
            run()
        self.assertEqual(len(connections), 3)
        self.assertEqual(clock.now, 18)

    def test_overall_deadline_bounds_a_stream_with_changing_status(self):
        events = _packets([
            ({"status": "pulling manifest"}, 0),
            ({"status": "pulling layer A", "digest": "sha256:a", "completed": 0}, 2),
            ({"status": "pulling layer B", "digest": "sha256:b", "completed": 0}, 2),
        ])
        clock, connections, run = self._harness([ (events, 200, True) ], idle_timeout=6, overall_timeout=5)

        with self.assertRaisesRegex(tempo_pull.PullError, "overall deadline"):
            run()
        self.assertEqual(len(connections), 1)
        self.assertEqual(clock.now, 5)

    def test_owner_refusal_precedes_connection(self):
        def refuse():
            raise RuntimeError("foreign listener")

        _clock, connections, run = self._harness([], owner_check=refuse)
        with self.assertRaisesRegex(tempo_pull.PullError, "could not verify its recorded listener"):
            run()
        self.assertEqual(connections, [])

    def test_peer_refusal_sends_no_request(self):
        def refuse():
            raise RuntimeError("foreign peer")

        _clock, connections, run = self._harness([([], 200, False)], peer_check=refuse)
        with self.assertRaisesRegex(tempo_pull.PullError, "peer failed ownership verification"):
            run()
        self.assertEqual(connections[0].requests, [])

    def test_redirect_is_refused_without_following_it(self):
        _clock, connections, run = self._harness([([], 307, False)])
        with self.assertRaisesRegex(tempo_pull.PullError, "HTTP 307"):
            run()
        self.assertEqual(len(connections), 1)
        self.assertEqual(len(connections[0].requests), 1)

    def test_real_http_connection_close_keeps_response_transport_readable(self):
        received = []

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_POST(self):
                size = int(self.headers.get("Content-Length", "0"))
                received.append((self.path, dict(self.headers), self.rfile.read(size)))
                self.send_response(200)
                self.send_header("Content-Type", "application/x-ndjson")
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(b'{"status":"pulling layer","digest":"sha256:test","completed":1}\n'
                                 b'{"status":"success"}\n')
                self.wfile.flush()
                self.close_connection = True

            def log_message(self, *_args):
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        port = server.server_address[1]
        owner = {"pid": 1, "port": port}
        try:
            result = tempo_pull.pull_owned_model(
                "qwen3:4b", port, owner, EXECUTABLE,
                assert_owned_listener=lambda _record, _executable, _port: None,
                assert_connected_peer=lambda sock, _record, _executable, expected_port: (
                    self.assertEqual(sock.getpeername(), ("127.0.0.1", expected_port))),
                idle_timeout=2,
                overall_timeout=5,
            )
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)

        self.assertIsNone(result)
        self.assertEqual(len(received), 1)
        path, headers, body = received[0]
        self.assertEqual(path, "/api/pull")
        self.assertNotIn("Authorization", headers)
        self.assertIn(b'"name":"qwen3:4b"', body)


if __name__ == "__main__":
    unittest.main()
