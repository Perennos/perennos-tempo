"""Read-only diagnostics must be useful even on a broken installation."""
import contextlib
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import tempo_doctor


class DoctorTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)

    def ready(self):
        (self.home / "tenant.json").write_text('{"local_only":["private"]}')
        (self.home / "config.json").write_text('{}')
        (self.home / "engines.json").write_text('{"engines":[{"locality":"local"}]}')
        gates = {name: True for name in tempo_doctor.SETUP_GATES}
        (self.home / "setup-receipt.json").write_text(json.dumps({"ready": True, "gates": gates}))
        (self.home / "ledger").mkdir()
        (self.home / "ledger" / "gateway-token").write_text("synthetic-token-" * 4)

    def test_missing_home_produces_recovery_without_creating_it(self):
        home = self.home / "missing"
        report = tempo_doctor.diagnose(home)
        self.assertEqual(report["state"], "In progress")
        self.assertEqual(tempo_doctor.exit_code(report), 1)
        self.assertFalse(home.exists())
        self.assertNotIn(str(home), json.dumps(report))

    def test_offline_report_is_read_only_and_never_connects(self):
        self.ready()
        before = {p.relative_to(self.home): (p.read_bytes(), p.stat().st_mtime_ns, p.stat().st_mode)
                  for p in self.home.rglob("*") if p.is_file()}
        with patch("tempo_client.list_models", side_effect=AssertionError("network attempted")):
            report = tempo_doctor.diagnose(self.home)
        after = {p.relative_to(self.home): (p.read_bytes(), p.stat().st_mtime_ns, p.stat().st_mode)
                 for p in self.home.rglob("*") if p.is_file()}
        self.assertEqual(before, after)
        self.assertEqual(tempo_doctor.exit_code(report), 0)
        self.assertEqual(report["state"], "Untested")
        self.assertEqual(report["checks"][-1]["code"], "gateway_not_probed")

    def test_broken_oversized_and_non_object_json_never_echo_contents(self):
        for value in (b'PRIVATE-CONTENT-not-json', b'"PRIVATE-CONTENT"', b' ' * (tempo_doctor.MAX_FILE_BYTES + 1)):
            with self.subTest(value=value[:30]):
                self.ready_if_empty()
                (self.home / "tenant.json").write_bytes(value)
                report = tempo_doctor.diagnose(self.home)
                self.assertEqual(tempo_doctor.exit_code(report), 1)
                self.assertNotIn("PRIVATE-CONTENT", json.dumps(report))
                self.assertIn("tenant_invalid", {c["code"] for c in report["checks"]})

    def ready_if_empty(self):
        if not (self.home / "ledger").exists():
            self.ready()

    def test_false_setup_gate_blocks_even_if_ready_claim_is_true(self):
        self.ready()
        path = self.home / "setup-receipt.json"
        receipt = json.loads(path.read_text())
        receipt["gates"]["qualified_local_engine"] = False
        path.write_text(json.dumps(receipt))
        self.assertEqual(tempo_doctor.exit_code(tempo_doctor.diagnose(self.home)), 1)

    def test_missing_gate_or_string_boolean_is_not_ready(self):
        self.ready()
        for receipt in ({"ready": True, "gates": {}}, {"ready": "true", "gates": {}}):
            (self.home / "setup-receipt.json").write_text(json.dumps(receipt))
            self.assertEqual(tempo_doctor.exit_code(tempo_doctor.diagnose(self.home)), 1)

    def test_symlink_input_is_refused(self):
        self.ready()
        path = self.home / "tenant.json"
        path.unlink()
        try:
            path.symlink_to(self.home / "config.json")
        except OSError:
            self.skipTest("host does not allow symlinks")
        report = tempo_doctor.diagnose(self.home)
        self.assertIn("tenant_invalid", {c["code"] for c in report["checks"]})

    def test_connect_uses_only_existing_token_and_authenticated_model_listing(self):
        self.ready()
        with patch("tempo_client.list_models", return_value=({"data": [{"id": "PRIVATE-MODEL"}]}, {})) as probe:
            report = tempo_doctor.diagnose(self.home, port=8912, connect=True)
        probe.assert_called_once_with("127.0.0.1", 8912, "synthetic-token-" * 4, timeout=5)
        self.assertEqual(report["state"], "Verified complete")
        self.assertNotIn("PRIVATE-MODEL", json.dumps(report))

    def test_failed_probe_does_not_echo_exception(self):
        self.ready()
        with patch("tempo_client.list_models", side_effect=RuntimeError("PRIVATE-DETAILS")):
            report = tempo_doctor.diagnose(self.home, connect=True)
        self.assertEqual(tempo_doctor.exit_code(report), 1)
        self.assertNotIn("PRIVATE-DETAILS", json.dumps(report))

    def test_probe_uses_selected_home_and_restores_environment_on_failure(self):
        self.ready()
        def fail(*args, **kwargs):
            self.assertEqual(os.environ["TEMPO_HOME"], str(self.home))
            raise RuntimeError("synthetic")
        with patch.dict(os.environ, {"TEMPO_HOME": "previous-selection"}), patch("tempo_client.list_models", side_effect=fail):
            tempo_doctor.diagnose(self.home, connect=True)
            self.assertEqual(os.environ["TEMPO_HOME"], "previous-selection")

    def test_missing_home_cli_returns_json_instead_of_argparse_error(self):
        import tempo
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            status = tempo.main(["doctor", "--home", str(self.home / "missing")])
        self.assertEqual(status, 1)
        self.assertEqual(json.loads(out.getvalue())["schema_version"], 1)


if __name__ == "__main__":
    unittest.main()
