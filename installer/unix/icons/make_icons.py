#!/usr/bin/env python3
"""Make the PNG icons (and tempo.icns) for the Linux and macOS installers from the official Perennos
insignia (installer/windows/art/perennos-insignia.png) on its cream tile, the same icon as the Windows
programs. Needs Pillow; run once, commit the output.

usage: python3 make_icons.py OUT_DIR      (default: this folder)
Files: <name>-<size>.png for sizes 16..256 (512 for tempo), and tempo.icns.
"""
import io
import os
import struct
import sys
sys.dont_write_bytecode = True

here = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(here, "..", "..", "windows", "art"))
import make_art as art  # noqa: E402
import make_insignia_icon as insignia  # noqa: E402

NAMES = list(art.ACCENTS) + ["tempo-field"]   # the file names the installers use; every one is the same icon
SIZES = [16, 32, 48, 64, 128, 256]


def png(size):
    buf = io.BytesIO()
    insignia.tile(size).save(buf, "PNG", optimize=True)
    return buf.getvalue()


def main(out):
    os.makedirs(out, exist_ok=True)
    for name in NAMES:
        sizes = SIZES + ([512] if name == "tempo" else [])
        pngs = {}
        for s in sizes:
            data = png(s)
            pngs[s] = data
            with open(os.path.join(out, f"{name}-{s}.png"), "wb") as f:
                f.write(data)
        if name == "tempo":
            # icns: 'icns' + length, then (type, length, PNG) per size
            kinds = {16: b"icp4", 32: b"icp5", 64: b"icp6", 128: b"ic07", 256: b"ic08", 512: b"ic09"}
            body = b"".join(kinds[s] + struct.pack(">I", 8 + len(pngs[s])) + pngs[s] for s in kinds)
            with open(os.path.join(out, "tempo.icns"), "wb") as f:
                f.write(b"icns" + struct.pack(">I", 8 + len(body)) + body)
    print("icons in", out)


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else here)
