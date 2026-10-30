import http.client
import http.server
import io
import json
from pathlib import Path
import socket
import sys
import tempfile
import threading
import time
import types
import unittest
from contextlib import ExitStack
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import tempo_bootstrap as bootstrap
import tempo_pull


ENGINE_DIGEST = "sha256:" + "a" * 64
MANIFEST_DIGEST = "sha256:" + "b" * 64


class InstallOverlapTests(unittest.TestCase):
    def _plan(self, root, *, port=11435, decision=True, wheels=True):
        return {
            "download_bytes": int(wheels) + int(decision),
            "wheel_count": int(wheels),
            "decision_file_count": int(decision),
            "ollama_zip": str(root / "ollama.zip"),
            "ollama_zip_bytes": 0,
            "engine_manifest_digest": MANIFEST_DIGEST,
            "engine_manifest_total_bytes": 1,
            "engine_digest": ENGINE_DIGEST,
            "venv": str(root / "venv"),
            "venv_python": str(root / "venv" / "python.exe"),
            "pip_report": str(root / "pip-report.json"),
            "downloads": {
                "wheels": ([{
                    "url": "wheel-fixture",
                    "sha256": "c" * 64,
                    "filename": "fixture.whl",
                    "bytes": 1,
                }] if wheels else []),
                "decision_files": ([{
                    "url": "decision-fixture",
                    "sha256": "d" * 64,
                    "path": str(root / "models" / "decision.bin"),
                    "bytes": 1,
                }] if decision else []),
            },
        }

    def _patch_common(self, stack, root, plan_doc, record, events, *, on_owned=None,
                      download=None, run_checked=None, engine_model=None):
        home = root / "home"
        home.mkdir(parents=True, exist_ok=True)
        app_root = root / "app"
        models_root = root / "models"
        code = root / "code"
        python = root / "bootstrap-python.exe"
        gateway = {"pid": 2234, "port": 11600}
        client = types.ModuleType("tempo_client")
        client.chat = lambda *_args, **_kwargs: ("READY.", {})
        client.gateway_token = lambda _home: "fixture-token"

        stack.enter_context(patch.object(bootstrap, "_verified_bootstrap_python",
                                         return_value=(python, "e" * 64)))
        stack.enter_context(patch.object(bootstrap, "plan", return_value=plan_doc))
        stack.enter_context(patch.object(bootstrap, "_registry_manifest", return_value={
            "digest": MANIFEST_DIGEST,
            "total_bytes": 1,
        }))
        stack.enter_context(patch.object(bootstrap, "_verified_ollama_runtime", return_value=True))
        stack.enter_context(patch.object(bootstrap, "_ensure_owned_model_root"))
        stack.enter_context(patch.object(bootstrap, "_load_state", return_value={}))
        stack.enter_context(patch.object(bootstrap, "_verify_home_binding"))
        stack.enter_context(patch.object(bootstrap, "_start_or_reuse_ollama",
                                         return_value=(record, f"http://127.0.0.1:{record['port']}")))
        stack.enter_context(patch.object(bootstrap, "_atomic_json"))
        stack.enter_context(patch.object(bootstrap, "_update_owned_tenant_url"))
        stack.enter_context(patch.object(bootstrap, "_valid_pinned_file", return_value=False))
        stack.enter_context(patch.object(bootstrap, "_download", side_effect=download or
                                         (lambda _url, _dest, _digest, *, expected_bytes: {
                                             "bytes": expected_bytes})))
        stack.enter_context(patch.object(bootstrap, "_run_checked", side_effect=run_checked or
                                         (lambda *_args, **_kwargs: "{}")))
        stack.enter_context(patch.object(bootstrap, "_engine_model", side_effect=engine_model or
                                         (lambda *_args: {"digest": ENGINE_DIGEST})))
        stack.enter_context(patch.object(bootstrap, "_assert_owned_listener", side_effect=on_owned or
                                         (lambda *_args: None)))
        stack.enter_context(patch.object(bootstrap, "_assert_connected_peer"))
        stack.enter_context(patch.object(bootstrap, "_setup_receipt_ready", return_value=True))
        stack.enter_context(patch.object(bootstrap, "_start_or_reuse_gateway", return_value=gateway))
        stack.enter_context(patch.object(bootstrap, "_child_env", return_value={}))
        stack.enter_context(patch.object(bootstrap, "_completed_answer", return_value=("fixture-receipt", "false")))
        stack.enter_context(patch.object(bootstrap, "_sha256_file", return_value="f" * 64))
        stack.enter_context(patch.object(bootstrap, "emit",
                                         side_effect=lambda kind, **details: events.append((kind, details))))
        stack.enter_context(patch.dict(sys.modules, {"tempo_client": client}))
        stack.enter_context(patch("sys.stdin", io.StringIO("yes\n")))
        return app_root, home, models_root, code, python

    def test_owned_pull_overlaps_wheel_pip_and_decision_downloads_then_joins_before_digest(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            record = {"pid": 1234, "port": 11435}
            plan_doc = self._plan(root, decision=True)
            events = []
            pull_started = threading.Event()
            pull_release = threading.Event()
            pull_finished = threading.Event()
            wheel_entered = threading.Event()
            wheel_release = threading.Event()
            pip_finished = threading.Event()
            decision_finished = threading.Event()
            engine_model_entered = threading.Event()
            setup_entered = threading.Event()
            errors = []
            app_root = home = models_root = code = python = None

            def pull(*_args, cancel_event, **_kwargs):
                pull_started.set()
                if not pull_release.wait(2):
                    raise AssertionError("the test did not release the owned pull")
                pull_finished.set()

            def download(url, _dest, _digest, *, expected_bytes):
                if url == "wheel-fixture":
                    if not pull_started.wait(1):
                        raise AssertionError("wheel work started before the model pull")
                    wheel_entered.set()
                    if not wheel_release.wait(2):
                        raise AssertionError("the test did not release the wheel download")
                else:
                    self.assertEqual(url, "decision-fixture")
                    self.assertFalse(pull_finished.is_set())
                    decision_finished.set()
                return {"bytes": expected_bytes}

            def run_checked(args, **_kwargs):
                if "pip" in args:
                    self.assertTrue(pull_started.is_set())
                    self.assertFalse(pull_finished.is_set())
                    pip_finished.set()
                    return ""
                setup_entered.set()
                self.assertTrue(pull_finished.is_set())
                self.assertTrue(decision_finished.is_set())
                receipt = {"steps": {"qualification": {
                    "qualified": bootstrap.ENGINE,
                    "tried": [{"engine": bootstrap.ENGINE, "ok": True, "revision": ENGINE_DIGEST}],
                }}}
                (home / "setup-receipt.json").write_text(json.dumps(receipt), encoding="utf-8")
                return json.dumps({"ready": True})

            def engine_model(*_args):
                engine_model_entered.set()
                self.assertTrue(pull_finished.is_set())
                self.assertTrue(decision_finished.is_set())
                return {"digest": ENGINE_DIGEST}

            def install():
                try:
                    bootstrap.install(python, app_root, home, models_root, code, consent_resolve=True)
                except BaseException as exc:
                    errors.append(exc)

            with ExitStack() as stack:
                app_root, home, models_root, code, python = self._patch_common(
                    stack, root, plan_doc, record, events,
                    download=download, run_checked=run_checked, engine_model=engine_model,
                )
                stack.enter_context(patch("tempo_pull.pull_owned_model", side_effect=pull))
                installer = threading.Thread(target=install)
                installer.start()
                try:
                    self.assertTrue(wheel_entered.wait(1), "foreground wheel download did not block")
                    self.assertTrue(pull_started.wait(1), "the owned pull did not start during wheel work")
                    self.assertFalse(pull_finished.is_set())
                    wheel_release.set()
                    self.assertTrue(pip_finished.wait(1), "pip did not run while the pull was outstanding")
                    self.assertTrue(decision_finished.wait(1), "decision download did not overlap the pull")
                    self.assertFalse(engine_model_entered.is_set(), "digest verification ran before the pull joined")
                    self.assertFalse(setup_entered.is_set(), "qualification ran before the pull joined")
                    pull_release.set()
                    installer.join(2)
                finally:
                    wheel_release.set()
                    pull_release.set()
                    installer.join(2)

            self.assertFalse(installer.is_alive())
            self.assertEqual(errors, [])
            self.assertTrue(engine_model_entered.is_set())
            self.assertTrue(setup_entered.is_set())
            self.assertTrue(any(kind == "ready" for kind, _details in events))

    def test_pull_failure_stops_before_digest_qualification_and_ready(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            record = {"pid": 1234, "port": 11435}
            events = []
            plan_doc = self._plan(root, decision=True)
            engine_model_calls = []
            decision_downloads = []
            app_root = home = models_root = code = python = None

            def pull(*_args, **_kwargs):
                raise tempo_pull.PullError("fixture pull failure")

            def download(url, _dest, _digest, *, expected_bytes):
                if url == "decision-fixture":
                    decision_downloads.append(url)
                return {"bytes": expected_bytes}

            with ExitStack() as stack:
                app_root, home, models_root, code, python = self._patch_common(
                    stack, root, plan_doc, record, events,
                    download=download,
                    engine_model=lambda *_args: engine_model_calls.append("called"),
                )
                stack.enter_context(patch("tempo_pull.pull_owned_model", side_effect=pull))
                with self.assertRaisesRegex(bootstrap.BootstrapError, "fixture pull failure"):
                    bootstrap.install(python, app_root, home, models_root, code, consent_resolve=True)

            self.assertEqual(decision_downloads, ["decision-fixture"])
            self.assertEqual(engine_model_calls, [])
            self.assertFalse(any(kind == "ready" for kind, _details in events))

    def test_foreground_failure_interrupts_and_joins_pull_blocked_on_headers_or_body(self):
        for phase in ("headers", "body"):
            with self.subTest(phase=phase), tempfile.TemporaryDirectory() as folder:
                root = Path(folder)
                request_received = threading.Event()
                headers_sent = threading.Event()
                release_server = threading.Event()
                response_waiting = threading.Event()
                body_reading = threading.Event()
                pulled_worker_ids = []
                watcher_ids = []
                interrupt_lock = threading.Lock()
                original_interrupt = tempo_pull._interrupt

                class Handler(http.server.BaseHTTPRequestHandler):
                    protocol_version = "HTTP/1.1"

                    def do_POST(self):
                        size = int(self.headers.get("Content-Length", "0"))
                        self.rfile.read(size)
                        request_received.set()
                        if phase == "body":
                            self.send_response(200)
                            self.send_header("Content-Type", "application/x-ndjson")
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
                record = {"pid": 1234, "port": server.server_address[1]}
                plan_doc = self._plan(root, port=record["port"], decision=True)
                events = []
                app_root = home = models_root = code = python = None

                def owned_listener(*_args):
                    pulled_worker_ids.append(threading.get_ident())

                def tracked_interrupt(sock):
                    ident = threading.get_ident()
                    with interrupt_lock:
                        if ident not in pulled_worker_ids:
                            watcher_ids.append(ident)
                    return original_interrupt(sock)

                def download(url, _dest, _digest, *, expected_bytes):
                    if url == "wheel-fixture":
                        ready = request_received.wait(1)
                        if phase == "headers":
                            ready = ready and response_waiting.wait(1)
                        else:
                            ready = ready and headers_sent.wait(1) and body_reading.wait(1)
                        if not ready:
                            raise AssertionError(f"pull did not reach the blocked {phase} fixture")
                        raise bootstrap.BootstrapError("foreground wheel failure")
                    return {"bytes": expected_bytes}

                original_getresponse = http.client.HTTPConnection.getresponse
                original_read1 = http.client.HTTPResponse.read1

                def tracked_getresponse(connection, *args, **kwargs):
                    if connection.port == record["port"]:
                        response_waiting.set()
                    return original_getresponse(connection, *args, **kwargs)

                def tracked_read1(response, *args, **kwargs):
                    body_reading.set()
                    return original_read1(response, *args, **kwargs)

                try:
                    with ExitStack() as stack:
                        app_root, home, models_root, code, python = self._patch_common(
                            stack, root, plan_doc, record, events,
                            on_owned=owned_listener, download=download,
                        )
                        stack.enter_context(patch.object(tempo_pull, "_interrupt", side_effect=tracked_interrupt))
                        stack.enter_context(patch.object(http.client.HTTPConnection, "getresponse",
                                                         tracked_getresponse))
                        stack.enter_context(patch.object(http.client.HTTPResponse, "read1", tracked_read1))
                        started = time.monotonic()
                        with self.assertRaisesRegex(bootstrap.BootstrapError, "foreground wheel failure"):
                            bootstrap.install(python, app_root, home, models_root, code, consent_resolve=True)
                        elapsed = time.monotonic() - started
                finally:
                    release_server.set()
                    server.shutdown()
                    server.server_close()
                    server_thread.join(2)

                self.assertLess(elapsed, 2.0,
                                f"blocked {phase} pull was not interrupted promptly; watcher callbacks={watcher_ids}")
                self.assertGreaterEqual(len(watcher_ids), 1, "the per-attempt cancellation watcher did not interrupt")
                self.assertTrue(all(ident not in {t.ident for t in threading.enumerate()} for ident in watcher_ids),
                                "a cancellation watcher survived install failure cleanup")

    def test_keyboard_interrupt_during_final_join_cancels_and_joins_worker(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            record = {"pid": 1234, "port": 11435}
            plan_doc = self._plan(root, decision=False, wheels=False)
            events = []
            pull_started = threading.Event()
            cancellation_seen = threading.Event()
            pull_finished = threading.Event()
            fallback_expired = threading.Event()
            worker_ids = []
            interrupt = KeyboardInterrupt("fixture interrupt during pull join")
            engine_model_calls = []
            setup_calls = []
            pull_result_calls = []

            class InterruptingFuture:
                def __init__(self, future):
                    self.future = future

                def result(self):
                    pull_result_calls.append("result")
                    if not pull_started.wait(1):
                        raise AssertionError("the background pull did not start before final join")
                    raise interrupt

            real_executor = bootstrap.ThreadPoolExecutor

            class InterruptingExecutor(real_executor):
                def submit(self, *args, **kwargs):
                    return InterruptingFuture(super().submit(*args, **kwargs))

            def pull(*_args, cancel_event, **_kwargs):
                worker_ids.append(threading.get_ident())
                pull_started.set()
                if cancel_event.wait(1):
                    cancellation_seen.set()
                else:
                    fallback_expired.set()
                pull_finished.set()

            def setup_or_pip(*_args, **_kwargs):
                setup_calls.append("called")
                return "{}"

            with ExitStack() as stack:
                app_root, home, models_root, code, python = self._patch_common(
                    stack, root, plan_doc, record, events,
                    run_checked=setup_or_pip,
                    engine_model=lambda *_args: engine_model_calls.append("called"),
                )
                stack.enter_context(patch.object(bootstrap, "ThreadPoolExecutor", InterruptingExecutor))
                stack.enter_context(patch("tempo_pull.pull_owned_model", side_effect=pull))

                with self.assertRaises(KeyboardInterrupt) as raised:
                    bootstrap.install(python, app_root, home, models_root, code, consent_resolve=True)

            self.assertIs(raised.exception, interrupt)
            self.assertEqual(pull_result_calls, ["result"])
            self.assertTrue(cancellation_seen.is_set(), "final-join interruption did not cancel the worker")
            self.assertFalse(fallback_expired.is_set(), "the worker exited only by its timeout fallback")
            self.assertTrue(pull_finished.is_set())
            self.assertFalse(any(thread.ident == worker_ids[0] for thread in threading.enumerate()),
                             "the pull worker survived exception propagation")
            self.assertEqual(engine_model_calls, [])
            self.assertEqual(setup_calls, [])
            self.assertFalse(any(kind == "ready" for kind, _details in events))


if __name__ == "__main__":
    unittest.main()
