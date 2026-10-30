import errno
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import tempo_bootstrap as bootstrap


class _ReplySocket:
    def __init__(self, payload):
        body = json.dumps(payload).encode("utf-8")
        self._response = io.BytesIO(
            b"HTTP/1.1 200 OK\r\nContent-Length: "
            + str(len(body)).encode("ascii")
            + b"\r\nConnection: close\r\n\r\n"
            + body
        )

    def __enter__(self):
        return self

    def __exit__(self, *_):
        return False

    def settimeout(self, _timeout):
        pass

    def sendall(self, _request):
        pass

    def makefile(self, _mode="rb"):
        return self._response


class BootstrapReadinessTests(unittest.TestCase):
    def _wait_patches(self, *, connect_side_effect):
        return (
            patch.object(bootstrap, "_assert_owned_listener", return_value=True),
            patch.object(bootstrap, "_assert_connected_peer"),
            patch.object(bootstrap.socket, "create_connection", side_effect=connect_side_effect),
            patch.object(bootstrap.time, "sleep"),
        )

    def test_first_timeout_retries_and_diagnostic_excludes_exception_text(self):
        with tempfile.TemporaryDirectory() as folder:
            diagnostic = Path(folder) / "ollama-probe-diagnostic.json"
            timeout = TimeoutError(errno.ETIMEDOUT, "PRIVATE_PROMPT_MARKER")
            patches = self._wait_patches(connect_side_effect=[timeout, _ReplySocket({"version": "0.40.6"})])
            with patches[0], patches[1], patches[2] as connect, patches[3]:
                bootstrap._wait_ollama(
                    "http://127.0.0.1:11435", bootstrap.time.time() + 5,
                    {"pid": 1234, "port": 11435}, Path("ollama.exe"),
                    diagnostic_path=diagnostic,
                )
            self.assertEqual(connect.call_count, 2)
            record_text = diagnostic.read_text(encoding="utf-8")
            record = json.loads(record_text)
            self.assertEqual(record["stage"], "connect")
            self.assertEqual(record["error_type"], "TimeoutError")
            self.assertEqual(record["errno"], errno.ETIMEDOUT)
            self.assertNotIn("PRIVATE_PROMPT_MARKER", record_text)

    def test_legacy_wait_call_retries_timeout_without_diagnostic_path_keyword(self):
        patches = self._wait_patches(connect_side_effect=[
            TimeoutError(errno.ETIMEDOUT, "startup timeout"), _ReplySocket({"version": "0.40.6"})
        ])
        with patches[0], patches[1], patches[2] as connect, patches[3]:
            bootstrap._wait_ollama(
                "http://127.0.0.1:11435", bootstrap.time.time() + 5,
                {"pid": 1234, "port": 11435}, Path("ollama.exe"),
            )
        self.assertEqual(connect.call_count, 2)

    def test_connection_reset_retries_until_valid_version(self):
        with tempfile.TemporaryDirectory() as folder:
            diagnostic = Path(folder) / "ollama-probe-diagnostic.json"
            reset = ConnectionResetError(errno.ECONNRESET, "PRIVATE_HEADER_MARKER")
            patches = self._wait_patches(connect_side_effect=[reset, _ReplySocket({"version": "0.40.6"})])
            with patches[0], patches[1], patches[2] as connect, patches[3]:
                bootstrap._wait_ollama(
                    "http://127.0.0.1:11435", bootstrap.time.time() + 5,
                    {"pid": 1234, "port": 11435}, Path("ollama.exe"),
                    diagnostic_path=diagnostic,
                )
            self.assertEqual(connect.call_count, 2)
            record_text = diagnostic.read_text(encoding="utf-8")
            record = json.loads(record_text)
            self.assertEqual(record["stage"], "connect")
            self.assertEqual(record["error_type"], "ConnectionResetError")
            self.assertEqual(record["errno"], errno.ECONNRESET)
            self.assertNotIn("PRIVATE_HEADER_MARKER", record_text)

    def test_wrong_owner_is_fatal_before_connecting(self):
        with tempfile.TemporaryDirectory() as folder:
            diagnostic = Path(folder) / "ollama-probe-diagnostic.json"
            with (
                patch.object(bootstrap, "_assert_owned_listener", side_effect=bootstrap.BootstrapError("wrong owner")),
                patch.object(bootstrap.socket, "create_connection") as connect,
            ):
                with self.assertRaisesRegex(bootstrap.BootstrapError, "ownership") as raised:
                    bootstrap._wait_ollama(
                        "http://127.0.0.1:11435", bootstrap.time.time() + 5,
                        {"pid": 1234, "port": 11435}, Path("ollama.exe"),
                        diagnostic_path=diagnostic,
                    )
            self.assertNotIsInstance(raised.exception, bootstrap._TransientTransportError)
            connect.assert_not_called()
            record = json.loads(diagnostic.read_text(encoding="utf-8"))
            self.assertEqual(record["stage"], "ownership")

    def test_retries_stop_at_deadline_and_report_last_transport_error(self):
        with tempfile.TemporaryDirectory() as folder:
            diagnostic = Path(folder) / "ollama-probe-diagnostic.json"
            now = [100.0]

            def sleep(seconds):
                now[0] += seconds

            with (
                patch.object(bootstrap, "_assert_owned_listener", return_value=True),
                patch.object(bootstrap, "_assert_connected_peer"),
                patch.object(bootstrap.socket, "create_connection", side_effect=TimeoutError(errno.ETIMEDOUT, "secret")) as connect,
                patch.object(bootstrap.time, "time", side_effect=lambda: now[0]),
                patch.object(bootstrap.time, "sleep", side_effect=sleep),
            ):
                with self.assertRaises(bootstrap.BootstrapError) as raised:
                    bootstrap._wait_ollama(
                        "http://127.0.0.1:11435", 102.0,
                        {"pid": 1234, "port": 11435}, Path("ollama.exe"),
                        diagnostic_path=diagnostic,
                    )
            self.assertEqual(connect.call_count, 2)
            self.assertEqual(now[0], 102.0)
            self.assertIn("Local engine probe failed at connect", str(raised.exception))
            self.assertIn(f"errno={errno.ETIMEDOUT}", str(raised.exception))

    def test_launched_child_identity_is_persisted_before_readiness(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            app_root = root / "app"
            app_root.mkdir()
            models_root = root / "models"
            record = {"pid": 1234, "port": 11435, "path": "ollama.exe"}
            state = {"existing": "preserved"}

            def fail_after_persist(url, deadline, saved_record, executable, **kwargs):
                persisted = json.loads((app_root / bootstrap.STATE_NAME).read_text(encoding="utf-8"))
                self.assertEqual(persisted["existing"], "preserved")
                self.assertEqual(persisted["ollama"]["pid"], record["pid"])
                self.assertEqual(persisted["ollama"]["models_root"], str((models_root / "ollama").resolve()))
                raise bootstrap.BootstrapError("readiness failed")

            with (
                patch.object(bootstrap, "_verified_ollama_runtime", return_value=True),
                patch.object(bootstrap, "_choose_port", return_value=11435),
                patch.object(bootstrap, "_child_env", return_value={}),
                patch.object(bootstrap, "_launch_owned", return_value=record),
                patch.object(bootstrap, "_wait_ollama", side_effect=fail_after_persist) as wait,
            ):
                with self.assertRaisesRegex(bootstrap.BootstrapError, "readiness failed"):
                    bootstrap._start_or_reuse_ollama(state, app_root, models_root, root / "code")
            self.assertEqual(wait.call_count, 1)

    def test_reused_owned_child_gets_startup_window_and_is_never_replaced_while_live(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            app_root = root / "app"
            models_root = root / "models"
            executable = app_root / "runtime" / "ollama" / "ollama.exe"
            record = {
                "pid": 1234,
                "port": 11435,
                "models_root": str((models_root / "ollama").resolve()),
            }
            with (
                patch.object(bootstrap, "_verified_ollama_runtime", return_value=True),
                patch.object(bootstrap, "_process_identity", return_value={"pid": 1234}),
                patch.object(bootstrap, "_owned_process", return_value=True),
                patch.object(bootstrap, "_assert_owned_listener", return_value=True),
                patch.object(bootstrap, "_choose_port", return_value=11436),
                patch.object(bootstrap, "_child_env", return_value={}),
                patch.object(bootstrap, "_launch_owned", return_value={"pid": 4321, "port": 11436}),
                patch.object(bootstrap, "_wait_ollama", side_effect=bootstrap.BootstrapError("still starting")) as wait,
                patch.object(bootstrap, "_launch_owned") as launch,
                patch.object(bootstrap.time, "time", return_value=100.0),
            ):
                with self.assertRaisesRegex(bootstrap.BootstrapError, "still starting"):
                    bootstrap._start_or_reuse_ollama({"ollama": record}, app_root, models_root, root / "code")
            self.assertEqual(wait.call_args.args[1], 280.0)
            launch.assert_not_called()

    def test_bundled_ollama_path_excludes_foreign_entries_without_mutating_process_path(self):
        foreign_path = r"C:\Users\Alex\AppData\Local\Microsoft\WinGet\Packages\llama.cpp"
        system_root = r"C:\Windows"
        runtime = Path(r"C:\PerennosTempo\runtime\ollama")
        with patch.dict(bootstrap.os.environ, {"PATH": foreign_path, "SystemRoot": system_root}, clear=True):
            child_env = bootstrap._child_env(ollama_runtime=runtime)
            expected = os.pathsep.join((str(runtime.resolve()), str(Path(system_root) / "System32"), system_root))
            self.assertEqual(child_env["PATH"], expected)
            self.assertNotIn("WinGet", child_env["PATH"])
            self.assertEqual(bootstrap.os.environ["PATH"], foreign_path)


if __name__ == "__main__":
    unittest.main()
