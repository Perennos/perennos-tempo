#!/usr/bin/env python3
"""Makes every picture the Perennos Tempo MSIX and its Store listing need, from the one brand mark.

The mark is the official Perennos insignia (installer/windows/art/perennos-insignia.png) on its cream
tile, as installer/windows/art/make_insignia_icon.py makes it, so the package, the installer and the
apps all show the same thing. Run it once and commit the PNGs:

    python3 installer/windows/msix/make_assets.py            # Assets/, ../store/art/ and the app icons
    python3 installer/windows/msix/make_assets.py --check    # only say what is missing or the wrong size

Needs Pillow (pip install pillow). The build script does not run this: the PNGs are committed.
"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
sys.path.insert(0, os.path.join(HERE, "..", "art"))
import make_art as art  # noqa: E402  (the brand colours)

try:
    from PIL import Image, ImageDraw, ImageFont
    import make_insignia_icon as insignia  # noqa: E402  (the insignia on its tile)
except ImportError:
    sys.exit("make_assets.py needs Pillow: pip install pillow")

ASSETS = os.path.join(HERE, "Assets")
STORE_ART = os.path.abspath(os.path.join(HERE, "..", "store", "art"))
BG_TOP, BG_BOTTOM = art.TILE_TOP, art.TILE_BOTTOM
MASTER = 1024
SCALES = [100, 125, 150, 200, 400]
# Microsoft's recommended target sizes for the app list, taskbar and Start (16, 24, 32, 48, 256)
TARGETS = [16, 24, 32, 48, 256]

# base name -> size at 100% scale. Files are <base>.scale-<n>.png; the unqualified <base>.png is the 100% one.
TILES = {
    "Square44x44Logo": (44, 44),
    "Square71x71Logo": (71, 71),
    "Square150x150Logo": (150, 150),
    "Wide310x150Logo": (310, 150),
    "Square310x310Logo": (310, 310),
    "StoreLogo": (50, 50),
}


_cache = {}


def master(kind):
    """The insignia on its tile. The insignia is a dark stone P and is never recoloured, so the unplated
    icons (`kind` 'light' and 'dark', for dark and light themes) keep the tile too: it is what makes the P readable."""
    if "tile" not in _cache:
        _cache["tile"] = insignia.tile(MASTER)
    return _cache["tile"]


def mark(kind, size):
    return master(kind).resize((size, size), Image.LANCZOS)


def gradient(w, h):
    im = Image.new("RGBA", (w, h))
    px = im.load()
    for y in range(h):
        c = art.lerp3(BG_TOP, BG_BOTTOM, y / max(h - 1, 1))
        for x in range(w):
            px[x, y] = (art.to8(c[0]), art.to8(c[1]), art.to8(c[2]), 255)
    return im


def font(size):
    for p in (
        "/usr/share/fonts/opentype/inter/Inter-SemiBold.otf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        "C:/Windows/Fonts/segoeuib.ttf",
    ):
        if os.path.isfile(p):
            return ImageFont.truetype(p, size)
    return ImageFont.load_default()


def tile_image(w, h):
    """A medium, wide or large tile: the charcoal field with the mark centred (Windows prints the name itself)."""
    im = gradient(w, h)
    side = int(min(w, h) * (0.56 if w == h else 0.62))
    im.alpha_composite(mark("tile", side), ((w - side) // 2, (h - side) // 2))
    return im


def plated(w, h):
    if w == h and w <= 71:
        return mark("tile", w)  # the mark carries its own rounded tile, so a small tile is just the mark
    return tile_image(w, h)


def save(im, path, want, report):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    assert im.size == want, (path, im.size, want)
    im.save(path, optimize=True)
    report.append((path, want))


def make_all():
    report = []
    for base, (w, h) in TILES.items():
        for s in SCALES:
            sw, sh = w * s // 100, h * s // 100
            im = plated(sw, sh)
            save(im, os.path.join(ASSETS, f"{base}.scale-{s}.png"), (sw, sh), report)
        save(plated(w, h), os.path.join(ASSETS, f"{base}.png"), (w, h), report)
    # app list, taskbar and Start sizes: with the tile, and unplated (no tile) for dark and light themes
    for t in TARGETS:
        save(mark("tile", t), os.path.join(ASSETS, f"Square44x44Logo.targetsize-{t}.png"), (t, t), report)
        save(mark("light", t), os.path.join(ASSETS, f"Square44x44Logo.targetsize-{t}_altform-unplated.png"), (t, t), report)
        save(mark("dark", t), os.path.join(ASSETS, f"Square44x44Logo.targetsize-{t}_altform-lightunplated.png"), (t, t), report)
    # Store listing art (uploaded in Partner Center, not part of the package)
    save(mark("tile", 300), os.path.join(STORE_ART, "app-tile-icon-300x300.png"), (300, 300), report)
    box = gradient(1080, 1080)
    box.alpha_composite(mark("tile", 520), (280, 190))
    d = ImageDraw.Draw(box)
    f = font(88)
    text = "Perennos Tempo"
    tw = d.textlength(text, font=f)
    d.text(((1080 - tw) / 2, 790), text, font=f, fill=art.INK)
    save(box, os.path.join(STORE_ART, "box-art-1080x1080.png"), (1080, 1080), report)
    poster = gradient(720, 1080)
    poster.alpha_composite(mark("tile", 440), (140, 250))
    d = ImageDraw.Draw(poster)
    f = font(66)
    tw = d.textlength(text, font=f)
    d.text(((720 - tw) / 2, 760), text, font=f, fill=art.INK)
    save(poster, os.path.join(STORE_ART, "poster-art-720x1080.png"), (720, 1080), report)
    # the programs' own icons are art/perennos.ico (make_insignia_icon.py), copied into each crate's assets/
    return report


def check():
    """Every file the manifest names exists at its size. (check_manifest.py does the same from the manifest.)"""
    bad = 0
    for base, (w, h) in TILES.items():
        want = [(f"{base}.png", (w, h))] + [(f"{base}.scale-{s}.png", (w * s // 100, h * s // 100)) for s in SCALES]
        for name, size in want:
            p = os.path.join(ASSETS, name)
            if not os.path.isfile(p) or Image.open(p).size != size:
                print("bad or missing:", name, size)
                bad += 1
    for t in TARGETS:
        for suffix in ("", "_altform-unplated", "_altform-lightunplated"):
            name = f"Square44x44Logo.targetsize-{t}{suffix}.png"
            p = os.path.join(ASSETS, name)
            if not os.path.isfile(p) or Image.open(p).size != (t, t):
                print("bad or missing:", name)
                bad += 1
    return bad


if __name__ == "__main__":
    if "--check" in sys.argv:
        n = check()
        print("assets ok" if not n else f"{n} problem(s)")
        sys.exit(1 if n else 0)
    rep = make_all()
    print(f"wrote {len(rep)} files")
    sys.exit(1 if check() else 0)
