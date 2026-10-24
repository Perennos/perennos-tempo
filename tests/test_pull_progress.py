import json
import http.server
import socket
import sys
import threading
import time
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
    duplicates = []

    def __init__(self):
        self.timeout = None
        self.closed = False
        self.shutdown_calls = 0

    def settimeout(self, timeout):
        self.timeout = timeout

    def dup(self):
        duplicate = _Socket()
        self.duplicates.append(duplicate)
        return duplicate

    def shutdown(self, _how):
        self.shutdown_calls += 1

    def detach(self):
        self.closed = True
        return -1

    def close(self):
        self.closed = True


class _Response:
    def __init__(self, connection, clock, packets, *, status=200, stall=False, on_stall=None):
        self.connection = connection
        self.clock = clock
        self.packets = list(packets)
        self.status = status
        self.stall = stall
        self.on_stall = on_stall
        self.closed = False

    def read1(self, size):
        if not self.packets:
            if self.stall:
                self.clock.advance(self.connection.sock.timeout)
                if self.on_stall is not None:
                    self.on_stall()
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
    def __init__(self, host, port, timeout, clock, packets, *, status=200, stall=False, on_stall=None):
        self.host = host
        self.port = port
        self.timeout = timeout
        self.clock = clock
        self.packets = packets
        self.status = status
        self.stall = stall
        self.on_stall = on_stall
        self.sock = None
        self.requests = []
        self.closed = False
        self.response = None

    def connect(self):
        self.sock = _Socket()

    def request(self, method, path, body=None, headers=None):
        self.requests.append((method, path, body, dict(headers or {})))

    def getresponse(self):
        self.response = _Response(self, self.clock, self.packets, status=self.status,
                                  stall=self.stall, on_stall=self.on_stall)
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
                 owner_check=None, peer_check=None, max_attempts=3,
                 cancel_event=None, on_stall=None, on_connect=None):
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
                               stall=spec[2] if len(spec) > 2 else False,
                               on_stall=on_stall)
            connections.append(conn)
            if on_connect:
                on_connect(conn, len(connections))
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
                    cancel_event=cancel_event,
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

    def test_pre_cancelled_pull_sends_no_request_or_connection(self):
        cancel = threading.Event()
        cancel.set()
        _clock, connections, run = self._harness([], cancel_event=cancel)

        with self.assertRaisesRegex(tempo_pull.PullError, "was cancelled"):
            run()
        self.assertEqual(connections, [])

    def test_cancel_during_bounded_connect_gap_sends_no_request(self):
        clock = _Clock()
        cancel = threading.Event()
        connecting = threading.Event()
        release_connect = threading.Event()
        conn = _Connection("127.0.0.1", PORT, 1.0, clock, [])
        result = []

        def connect():
            connecting.set()
            release_connect.wait(1)
            conn.sock = _Socket()

        conn.connect = connect

        def run():
            try:
                def connection_factory(host, port, timeout):
                    self.assertEqual((host, port), ("127.0.0.1", PORT))
                    self.assertLessEqual(timeout, 1.0)
                    return conn

                with patch.object(tempo_pull.http.client, "HTTPConnection", side_effect=connection_factory):
                    tempo_pull.pull_owned_model(
                        "qwen3:4b", PORT, OWNER, EXECUTABLE,
                        assert_owned_listener=lambda *_args: None,
                        assert_connected_peer=lambda *_args: None,
                        idle_timeout=60,
                        overall_timeout=120,
                        cancel_event=cancel,
                        clock=clock,
                    )
                    result.append(None)
            except BaseException as exc:
                result.append(exc)

        worker = threading.Thread(target=run)
        worker.start()
        self.assertTrue(connecting.wait(1), "the bounded local connect did not start")
        cancel.set()
        release_connect.set()
        worker.join(2)

        self.assertFalse(worker.is_alive(), "the pull did not leave the connect-registration gap")
        self.assertEqual(len(result), 1)
        self.assertIsInstance(result[0], tempo_pull.PullError)
        self.assertIn("was cancelled", str(result[0]))
        self.assertEqual(conn.requests, [])

    def test_cancel_at_idle_retry_boundary_prevents_another_attempt(self):
        cancel = threading.Event()
        heartbeat = {"status": "pulling layer", "digest": "sha256:one", "completed": 0}
        _clock, connections, run = self._harness(
            [(_packets([(heartbeat, 0)]), 200, True)],
            cancel_event=cancel,
            on_stall=cancel.set,
        )

        with self.assertRaisesRegex(tempo_pull.PullError, "was cancelled"):
            run()
        self.assertEqual(len(connections), 1)
        self.assertEqual(connections[0].requests[0][1], "/api/pull")

    def test_attempt_watcher_is_joined_before_retry_and_cannot_touch_next_socket(self):
        _Socket.duplicates.clear()
        watcher_entered = threading.Event()
        release_watcher = threading.Event()
        watchers = []
        connections = []
        clock = _Clock()
        pull_thread_ids = set()

        class CancelProbe:
            def is_set(self):
                current = threading.current_thread()
                if current.ident not in pull_thread_ids:
                    watcher_entered.set()
                    if current not in watchers:
                        watchers.append(current)
                    release_watcher.wait(2)
                return False

        heartbeat = {"status": "pulling layer", "digest": "sha256:one", "completed": 0}
        specs = [(_packets([(heartbeat, 0)]), 200, True),
                 (_packets([({"status": "success"}, 0)]), 200, False)]

        def connection_factory(host, port, timeout):
            self.assertEqual((host, port), ("127.0.0.1", PORT))
            if connections:
                self.assertGreater(connections[-1].sock.shutdown_calls, 0)
                self.assertFalse(watchers[0].is_alive(), "attempt watcher survived into the next connection")
                self.assertTrue(_Socket.duplicates[0].closed)
            spec = specs[len(connections)]
            conn = _Connection(host, port, timeout, clock, spec[0], status=spec[1], stall=spec[2],
                               on_stall=lambda: self.assertTrue(watcher_entered.wait(1)))
            connections.append(conn)
            return conn

        result = []

        def run():
            pull_thread_ids.add(threading.get_ident())
            try:
                with patch.object(tempo_pull.http.client, "HTTPConnection", side_effect=connection_factory):
                    tempo_pull.pull_owned_model(
                        "qwen3:4b", PORT, OWNER, EXECUTABLE,
                        assert_owned_listener=lambda *_args: pull_thread_ids.add(threading.get_ident()),
                        assert_connected_peer=lambda *_args: None,
                        idle_timeout=6,
                        overall_timeout=120,
                        cancel_event=CancelProbe(),
                        clock=clock,
                    )
                result.append(None)
            except BaseException as exc:
                result.append(exc)

        worker = threading.Thread(target=run)
        worker.start()
        try:
            self.assertTrue(watcher_entered.wait(1),
                            f"the first attempt watcher did not start; watchers={len(watchers)}, result={result}")
            self.assertEqual(len(connections), 1)
            self.assertTrue(watchers[0].is_alive())
            release_watcher.set()
            worker.join(2)
        finally:
            release_watcher.set()
            worker.join(2)

        self.assertFalse(worker.is_alive())
        self.assertEqual(len(result), 1)
        self.assertIsNone(result[0])
        self.assertEqual(len(connections), 2)
        self.assertEqual(len(watchers), 1)
        self.assertTrue(all(not watcher.is_alive() for watcher in watchers))
        self.assertTrue(_Socket.duplicates[0].closed)
        first_shutdowns = _Socket.duplicates[0].shutdown_calls
        self.assertEqual(first_shutdowns, 0, "a stale attempt watcher interrupted after cleanup")

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

    def test_total_deadline_timer_interrupts_blocked_headers_and_close_body(self):
        for phase in ("headers", "body"):
            with self.subTest(phase=phase):
                request_received = threading.Event()
                response_waiting = threading.Event()
                headers_sent = threading.Event()
                body_reading = threading.Event()
                release_server = threading.Event()
                timer_sockets = []
                timer_threads = []
                pull_thread_id = []

                class Handler(http.server.BaseHTTPRequestHandler):
                    protocol_version = "HTTP/1.1"

                    def do_POST(self):
                        size = int(self.headers.get("Content-Length", "0"))
                        self.rfile.read(size)
                        request_received.set()
                        if phase == "body":
                            self.send_response(200)
                            self.send_header("Content-Length", "128")
                            self.send_header("Connection", "close")
                            self.end_headers()
                            self.wfile.flush()
                            headers_sent.set()
                        release_server.wait(3)
                        self.close_connection = True

                    def log_message(self, *_args):
                        pass

                server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
                server.daemon_threads = True
                server_thread = threading.Thread(target=server.serve_forever, daemon=True)
                server_thread.start()
                port = server.server_address[1]
                owner = {"pid": 1, "port": port}
                original_interrupt = tempo_pull._interrupt
                original_getresponse = http.client.HTTPConnection.getresponse
                original_read1 = http.client.HTTPResponse.read1

                def tracked_interrupt(sock):
                    if pull_thread_id and threading.get_ident() != pull_thread_id[0]:
                        timer_sockets.append((sock, sock.fileno()))
                        timer_threads.append(threading.current_thread())
                    return original_interrupt(sock)

                def tracked_getresponse(connection, *args, **kwargs):
                    if connection.port == port:
                        response_waiting.set()
                        # Let the overall Timer, not the per-read timeout, interrupt this blocked header read.
                        connection.sock.settimeout(2)
                    return original_getresponse(connection, *args, **kwargs)

                def tracked_read1(response, *args, **kwargs):
                    body_reading.set()
                    # Keep the body read blocked past the 0.5s overall deadline (idle is 2s).
                    response.fp.raw._sock.settimeout(2)
                    return original_read1(response, *args, **kwargs)

                def owned_listener(*_args):
                    pull_thread_id[:] = [threading.get_ident()]

                started = time.monotonic()
                try:
                    with (patch.object(tempo_pull, "_interrupt", side_effect=tracked_interrupt),
                          patch.object(http.client.HTTPConnection, "getresponse", tracked_getresponse),
                          patch.object(http.client.HTTPResponse, "read1", tracked_read1)):
                        with self.assertRaisesRegex(tempo_pull.PullError, "overall deadline"):
                            tempo_pull.pull_owned_model(
                                "qwen3:4b", port, owner, EXECUTABLE,
                                assert_owned_listener=owned_listener,
                                assert_connected_peer=lambda sock, _record, _executable, expected: (
                                    self.assertEqual(sock.getpeername(), ("127.0.0.1", expected))),
                                idle_timeout=2,
                                overall_timeout=0.5,
                            )
                    elapsed = time.monotonic() - started
                    self.assertTrue(request_received.is_set())
                    self.assertTrue(response_waiting.is_set())
                    if phase == "body":
                        self.assertTrue(headers_sent.is_set())
                        self.assertTrue(body_reading.is_set())
                    self.assertEqual(len(timer_sockets), 2)
                    self.assertTrue(all(descriptor >= 0 for _sock, descriptor in timer_sockets))
                    self.assertTrue(all(sock.fileno() == -1 for sock, _descriptor in timer_sockets))
                    self.assertIs(timer_threads[0], timer_threads[1])
                    self.assertFalse(timer_threads[0].is_alive())
                    self.assertLess(elapsed, 1.5)
                finally:
                    release_server.set()
                    server.shutdown()
                    server.server_close()
                    server_thread.join(2)


if __name__ == "__main__":
    unittest.main()
