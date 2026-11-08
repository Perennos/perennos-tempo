"""The brain world is the same bytes in the chat surface and in Vault, and its pure parts pass node:test.

Run: python3 -m unittest tests.test_brain_copies
"""
import os
import shutil
import subprocess
import unittest

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
GUI = "tempo-agent/gui-chat"
VAULT = "perennos-tempo-vault/assets"
SHARED = ["brain-world.mjs", "brain-sim.mjs", "brain-feed.mjs", "vendor/three-post.js", "vendor/three.module.min.js", "vendor/three.LICENSE"]


def read(rel):
    with open(os.path.join(ROOT, rel), "rb") as f:
        return f.read()


class BrainCopies(unittest.TestCase):
    def test_shared_files_are_the_same_bytes(self):
        for name in SHARED:
            self.assertEqual(read(f"{GUI}/{name}"), read(f"{VAULT}/{name}"), name)

    def test_no_green_in_the_brain(self):
        for name in ["brain-world.mjs", "brain-sim.mjs"]:
            text = read(f"{GUI}/{name}").decode().lower()
            for g in ["#b8ff3b", "#4f8a00", "#46b35f", "#8fcf9a"]:
                self.assertNotIn(g, text)

    @unittest.skipUnless(shutil.which("node"), "node is not installed")
    def test_node_tests_and_syntax(self):
        for name in ["brain-world.mjs", "brain-sim.mjs", "brain-feed.mjs", "chat.mjs"]:
            subprocess.run(["node", "--check", os.path.join(ROOT, GUI, name)], check=True)
        subprocess.run(["node", "--test", os.path.join(ROOT, GUI, "brain.test.mjs")], check=True, capture_output=True)


if __name__ == "__main__":
    unittest.main()
