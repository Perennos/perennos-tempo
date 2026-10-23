"""Offline regressions for audit-pack confinement and verified model promotion."""
import hashlib
import io
import json
import os
import pathlib
import tempfile
import unittest
from unittest.mock import patch

import tempo_audit
import tempo_mcp


class SecurityFiles(unittest.TestCase):
    def test_mcp_escaped_id_cannot_expand_past_output_cap(self):
        raw = json.dumps({"jsonrpc": "2.0", "id": "\U0001f680" * 100000, "method": "ping"}, ensure_ascii=False).encode() + b"\n"
        self.assertLess(len(raw), tempo_mcp.MAX_MESSAGE_BYTES)
        sink = io.BytesIO()
        tempo_mcp.serve("127.0.0.1", 8765, "x" * 32, stdin=io.BytesIO(raw), stdout=sink)
        self.assertLessEqual(len(sink.getvalue()), tempo_mcp.MAX_OUTPUT_BYTES)
        response = json.loads(sink.getvalue())
        self.assertIsNone(response["id"])
        self.assertEqual(response["error"]["code"], -32603)

    def test_mcp_oversized_integer_and_surrogate_id_do_not_kill_adapter(self):
        raw = (b'{"jsonrpc":"2.0","id":' + b'9' * 5000 + b',"method":"ping"}\n'
               b'{"jsonrpc":"2.0","id":"\\ud800","method":"ping"}\n'
               b'{"jsonrpc":"2.0","id":3,"method":"ping"}\n')
        sink = io.BytesIO()
        tempo_mcp.serve("127.0.0.1", 8765, "x" * 32, stdin=io.BytesIO(raw), stdout=sink)
        rows = [json.loads(line) for line in sink.getvalue().splitlines()]
        self.assertEqual(rows[0]["error"]["code"], -32700)
        self.assertEqual(rows[1]["id"], "\ud800")
        self.assertEqual(rows[2]["id"], 3)
        self.assertEqual(rows[2]["result"], {})

    def test_audit_refuses_unsafe_names_before_member_access(self):
        for name in ("../outside", "/outside", "C:/outside", "//attacker.invalid/share/probe", "..\\outside"):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as folder:
                root = pathlib.Path(folder)
                (root / "CHAIN.json").write_text(json.dumps({"files": {name: "0" * 64}}))
                with patch.object(pathlib.Path, "exists", side_effect=AssertionError("member accessed")):
                    with self.assertRaises(ValueError):
                        tempo_audit.verify(root)

    def test_audit_refuses_link_member(self):
        with tempfile.TemporaryDirectory() as folder:
            root = pathlib.Path(folder)
            (root / "CHAIN.json").write_text(json.dumps({"files": {"decisions.jsonl": "0" * 64}}))
            with patch.object(pathlib.Path, "lstat", return_value=type("Stat", (), {"st_mode": 0o120777, "st_file_attributes": 0x400})()):
                with self.assertRaises(ValueError):
                    tempo_audit.verify(root)

    def test_recursive_mcp_json_returns_parse_error_and_continues(self):
        source = io.BytesIO(b"[" * 20000 + b"0" + b"]" * 20000 + b'\n{"jsonrpc":"2.0","id":2,"method":"ping"}\n')
        sink = io.BytesIO()
        # Python 3.14's iterative JSON decoder need not raise for this depth;
        # exercise the actual exception seam on every supported interpreter.
        with patch.object(tempo_mcp.json, "loads", side_effect=[RecursionError(), {"jsonrpc": "2.0", "id": 2, "method": "ping"}]):
            tempo_mcp.serve("127.0.0.1", 8765, "x" * 32, stdin=source, stdout=sink)
        rows = [json.loads(line) for line in sink.getvalue().splitlines()]
        self.assertEqual(rows[0]["error"]["code"], -32700)
        self.assertEqual(rows[1]["result"], {})

    def test_empty_audit_manifest_is_refused(self):
        with tempfile.TemporaryDirectory() as folder:
            root = pathlib.Path(folder)
            (root / "CHAIN.json").write_text(json.dumps({"files": {}, "head": None, "length": 0}))
            with self.assertRaises(ValueError):
                tempo_audit.verify(root)

    def test_valid_audit_pack_still_verifies(self):
        with tempfile.TemporaryDirectory() as folder:
            root = pathlib.Path(folder)
            names = ("AUDIT-SUMMARY.md", "PROFILE-SUMMARY.json", "decisions.jsonl", "outcomes.jsonl",
                     "attempts.jsonl", "receipts.jsonl", "egress-policy.json")
            for name in names:
                (root / name).write_bytes(b"")
            files = {n: hashlib.sha256(b"").hexdigest() for n in names}
            (root / "CHAIN.json").write_text(json.dumps({"files": files, "head": None, "length": 0}))
            result = tempo_audit.verify(root)
            self.assertTrue(all(result[k] for k in ("files_ok", "chain_ok", "receipt_chain_ok")))

    def _fetch(self):
        # Import with an isolated data home; never use the operator's tenant.
        os.environ.setdefault("TEMPO_HOME", tempfile.gettempdir())
        import fetch_models
        return fetch_models

    def test_bad_download_preserves_previous_destination(self):
        fetch = self._fetch()
        with tempfile.TemporaryDirectory() as folder:
            dest = pathlib.Path(folder) / "model"
            dest.write_bytes(b"previous")
            with patch.object(fetch, "_open_https", return_value=io.BytesIO(b"corrupt")):
                with self.assertRaises(ValueError):
                    fetch.get("https://huggingface.co/pinned", dest, hashlib.sha256(b"expected").hexdigest())
            self.assertEqual(dest.read_bytes(), b"previous")
            self.assertFalse(dest.with_suffix(".part").exists())

    def test_run_never_promotes_unverified_download(self):
        fetch = self._fetch()
        with tempfile.TemporaryDirectory() as folder:
            dest = pathlib.Path(folder) / "model"
            dest.write_bytes(b"previous")
            with patch.object(fetch, "MODELS", pathlib.Path(folder)), patch.object(fetch, "PINS", [
                    ("model", "https://huggingface.co/pinned", hashlib.sha256(b"expected").hexdigest())
            ]), patch.object(fetch, "_open_https", return_value=io.BytesIO(b"corrupt")):
                with self.assertRaises(ValueError):
                    fetch.run()
            self.assertEqual(dest.read_bytes(), b"previous")

    def test_download_deadline_preserves_destination(self):
        fetch = self._fetch()
        with tempfile.TemporaryDirectory() as folder:
            dest = pathlib.Path(folder) / "model"
            dest.write_bytes(b"previous")
            with patch.object(fetch.time, "monotonic", side_effect=[0, 2]), patch.object(
                    fetch, "_open_https", return_value=io.BytesIO(b"good")):
                with self.assertRaises(TimeoutError):
                    fetch.get("https://huggingface.co/pinned", dest, hashlib.sha256(b"good").hexdigest(), timeout=1)
            self.assertEqual(dest.read_bytes(), b"previous")
            self.assertEqual(list(pathlib.Path(folder).glob("*.part")), [])

    def test_verified_download_promotes(self):
        fetch = self._fetch()
        with tempfile.TemporaryDirectory() as folder:
            dest = pathlib.Path(folder) / "model"
            with patch.object(fetch, "_open_https", return_value=io.BytesIO(b"good")):
                fetch.get("https://huggingface.co/pinned", dest, hashlib.sha256(b"good").hexdigest())
            self.assertEqual(dest.read_bytes(), b"good")

    def test_model_fetch_bounds_response_headers_with_shared_deadline(self):
        fetch = self._fetch()
        with tempfile.TemporaryDirectory() as folder:
            dest = pathlib.Path(folder) / "model"
            dest.write_bytes(b"previous")
            with patch.object(fetch, "_open_https", side_effect=TimeoutError("header deadline")) as opened:
                with self.assertRaises(TimeoutError):
                    fetch.get("https://huggingface.co/pinned", dest, "0" * 64, timeout=1)
            self.assertIn("deadline", opened.call_args.kwargs)
            self.assertEqual(opened.call_args.kwargs["timeout"], 1)
            self.assertEqual(dest.read_bytes(), b"previous")
            self.assertEqual(list(pathlib.Path(folder).glob("*.part")), [])

    def test_download_limit_preserves_destination(self):
        fetch = self._fetch()
        with tempfile.TemporaryDirectory() as folder:
            dest = pathlib.Path(folder) / "model"
            dest.write_bytes(b"previous")
            with patch.object(fetch, "_open_https", return_value=io.BytesIO(b"oversize")):
                with self.assertRaises(ValueError):
                    fetch.get("https://huggingface.co/pinned", dest, "0" * 64, max_bytes=3)
            self.assertEqual(dest.read_bytes(), b"previous")
