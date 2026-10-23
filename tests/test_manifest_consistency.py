"""Ordinary CI must refuse stale, linked or missing release payload inputs."""
import hashlib
import json
import pathlib
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "acceptance"))
from prepare_payload import clean_name, confined_source


class ManifestConsistency(unittest.TestCase):
    def test_every_manifest_input_matches(self):
        manifest = json.loads((ROOT / "MANIFEST.json").read_text())
        for name, digest in manifest["files"].items():
            with self.subTest(name=name):
                path = confined_source(ROOT, clean_name(name))
                self.assertEqual(hashlib.sha256(path.read_bytes()).hexdigest(), digest)
