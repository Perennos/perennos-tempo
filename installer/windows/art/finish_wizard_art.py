#!/usr/bin/env python3
"""Makes the rendered large wizard images exact in size. Only needed when the art is redrawn
(it uses Pillow); the build itself reads the finished PNGs with the standard library.

  1. Serve the brand art sources (kept privately, not in this repository):
     python3 -m http.server 8931 --bind 127.0.0.1
  2. Render the wizard page from them with the private shoot-wizard.js:
     node shoot-wizard.js http://127.0.0.1:8931 RAW_DIR
  3. python3 installer/windows/art/finish_wizard_art.py RAW_DIR installer/windows/art

Each render is trimmed to Inno Setup's exact size (evenly from both edges, so nothing is
stretched) and kept in full colour: 256 colours (as the private brand README does for the README)
flattens the warm glow and the grain behind the lattice.
"""
import os
import sys

from PIL import Image

SIZES = {100: (164, 314), 125: (202, 386), 150: (240, 459), 175: (290, 556), 200: (315, 604), 225: (366, 700), 250: (416, 797)}


def main(raw, out):
    for pct, (w, h) in SIZES.items():
        im = Image.open(os.path.join(raw, f"wizard-large-{pct}.raw.png")).convert("RGB")
        dx, dy = im.width - w, im.height - h
        if not (0 <= dx <= 4 and 0 <= dy <= 4):
            sys.exit(f"{pct}%: rendered {im.width}x{im.height}, expected {w}x{h}")
        im = im.crop((dx // 2, dy // 2, dx // 2 + w, dy // 2 + h))
        path = os.path.join(out, f"wizard-large-{pct}.png")
        im.save(path, optimize=True)
        print(f"{path}: {w}x{h}, {os.path.getsize(path)} bytes")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__.strip())
    main(sys.argv[1], sys.argv[2])
