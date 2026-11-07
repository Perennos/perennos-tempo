"""The one Perennos look is the same bytes in every Tempo crate that ships a page.

tempo-theme.css, the official wordmark, insignia and favicon, and the three fonts are copied into
each crate's assets (each crate builds standalone, like tempo_home.rs). This says if a copy drifts.
Run: python3 -m unittest tests.test_brand_copies
"""
import os
import re
import unittest

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
DIRS = [
    "tempo-agent/gui-chat",
    "perennos-tempo-launcher/assets",
    "perennos-tempo-vault/assets",
    "perennos-tempo-room/assets",
]
FILES = [
    "tempo-theme.css",
    "perennos-insignia.webp",
    "perennos-wordmark-cream.png",
    "perennos-wordmark-ink.png",
    "favicon-64.png",
    "fonts/archivo-latin-variable.woff2",
    "fonts/instrument-serif-italic-latin.woff2",
    "fonts/jetbrains-mono-latin-700.woff2",
    "fonts/bodoni-moda-latin-variable.woff2",
    "fonts/bodoni-moda-italic-latin-variable.woff2",
]
SHEETS = ["tempo-agent/gui-chat/chat.css", "perennos-tempo-vault/assets/app.css", "perennos-tempo-room/assets/app.css", "perennos-tempo-vault/assets/publish.css"]
GREENS = ["#b8ff3b", "#4f8a00", "#46b35f", "#8fcf9a", "--learn"]


def read(rel):
    with open(os.path.join(ROOT, rel), "rb") as f:
        return f.read()


class BrandCopies(unittest.TestCase):
    def test_every_copy_is_the_same_bytes(self):
        for name in FILES:
            first = read(f"{DIRS[0]}/{name}")
            for d in DIRS[1:]:
                self.assertEqual(first, read(f"{d}/{name}"), f"{d}/{name} differs from {DIRS[0]}/{name}")

    def test_no_green_in_the_look(self):
        for rel in ["tempo-agent/gui-chat/tempo-theme.css"] + SHEETS:
            text = read(rel).decode().lower()
            for g in GREENS:
                self.assertNotIn(g, text, f"{g} in {rel}")

    def test_the_artwork_is_named_in_one_place_per_app(self):
        for rel in SHEETS[:3]:
            text = read(rel).decode()
            self.assertIn("--ins-src:", text, rel)
            self.assertIn("--wm-src:", text, rel)
            self.assertEqual(len(re.findall(r"@font-face", text)), 5, rel)


if __name__ == "__main__":
    unittest.main()
