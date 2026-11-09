#!/usr/bin/env python3
"""Makes the Windows icon and the small wizard images from the official Perennos insignia
(art/perennos-insignia.png, the P that perennos.com shows). Only needed when the insignia changes:
it uses Pillow; the build itself reads the finished files with the standard library, like the wizard art.

  python3 installer/windows/art/make_insignia_icon.py

Writes, next to this file:
  perennos.ico            16 to 256 pixels, 32-bit bitmap frames (the form Inno Setup's compiler reads)
  wizard-small-<pct>.png  the same icon on the wizard's white header, at Inno Setup's seven sizes

The insignia is never redrawn or recoloured. It sits, as it is, on a tile in the brand's own cream (the
same cream the old mark's stroke used), because the stone P is dark and would disappear on a dark taskbar.
"""
import os
import sys

from PIL import Image, ImageDraw, ImageFilter

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import make_art as art  # noqa: E402  (its ico writer, so the frames are the ones the build already accepts)

CREAM = (0xEC, 0xE6, 0xD8, 255)
TILE_RADIUS = 0.22    # of the tile's side
MARK_SHARE = 0.78     # of the tile's side, the insignia's taller edge


def insignia():
    im = Image.open(os.path.join(HERE, "perennos-insignia.png")).convert("RGBA")
    return im.crop(im.getchannel("A").point(lambda a: 255 if a > 8 else 0).getbbox())


def tile(size, margin=0.0):
    """The cream tile with the insignia on it, `size` x `size`, straight alpha."""
    scale = 4   # drawn at 4x and reduced, so the corners are smooth at 16 px too
    big = size * scale
    t = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    m = Image.new("L", (big, big), 0)
    ImageDraw.Draw(m).rounded_rectangle((0, 0, big - 1, big - 1), radius=int(big * TILE_RADIUS), fill=255)
    cream = Image.new("RGBA", (big, big), CREAM)
    cream.putalpha(m)
    t.alpha_composite(cream)
    mark = insignia()
    s = big * MARK_SHARE / max(mark.size)
    mark = mark.resize((max(1, round(mark.width * s)), max(1, round(mark.height * s))), Image.LANCZOS)
    t.alpha_composite(mark, ((big - mark.width) // 2, (big - mark.height) // 2))
    return t.resize((size, size), Image.LANCZOS)


def rows(img):
    px = img.load()
    return [[(px[x, y][0], px[x, y][1], px[x, y][2], px[x, y][3] / 255.0) for x in range(img.width)] for y in range(img.height)]


def wizard_small(size):
    """The tile on the wizard's white header with a soft shadow under it (as the old art had)."""
    t = tile(round(size * 0.8))
    off = (size - t.width) // 2
    shadow = Image.new("L", (size, size), 0)
    ImageDraw.Draw(shadow).rounded_rectangle((off, off + round(0.035 * size), off + t.width, off + t.height + round(0.035 * size)),
                                             radius=int(t.width * TILE_RADIUS), fill=int(255 * 0.2))
    shadow = shadow.filter(ImageFilter.GaussianBlur(0.045 * size))
    out = Image.new("RGBA", (size, size), (255, 255, 255, 255))
    out.alpha_composite(Image.merge("RGBA", (Image.new("L", (size, size), 0),) * 3 + (shadow,)))
    out.alpha_composite(t, (off, off))
    return out.convert("RGB")


def main():
    frames = [(s, rows(tile(s))) for s in art.ICON_SIZES]
    with open(os.path.join(HERE, "perennos.ico"), "wb") as f:
        f.write(art.ico_bytes(frames))
    for pct, size in art.SMALL.items():
        wizard_small(size).save(os.path.join(HERE, f"wizard-small-{pct}.png"), optimize=True)
    print("perennos.ico (%s) and wizard-small-*.png in %s" % (", ".join(str(s) for s in art.ICON_SIZES), HERE))


if __name__ == "__main__":
    main()
