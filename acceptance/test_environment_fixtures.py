"""Record the exact Ollama warning spelling observed on the failed workstation."""
import pathlib
import tempfile
import unittest

from windows_acceptance import _ollama_log_check


class EnvironmentFixtures(unittest.TestCase):
    def test_actual_warn_spelling_is_not_missed(self):
        with tempfile.TemporaryDirectory() as folder:
            log = pathlib.Path(folder) / "ollama.log"
            log.write_text('level=WARN msg="potentially incompatible library detected in PATH" location=foreign/ggml-base.dll\n')
            self.assertFalse(_ollama_log_check(log)["passed"])
            log.write_text('level=INFO msg="Listening on 127.0.0.1"\n')
            self.assertTrue(_ollama_log_check(log)["passed"])
