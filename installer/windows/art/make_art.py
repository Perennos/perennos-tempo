#!/usr/bin/env python3
"""The Perennos Tempo installer art, drawn from code.

The Perennos mark (the P with the signal dot, from assets/icon.svg in the products) as Windows icons,
and the wizard images at every size Inno Setup asks for, so they stay sharp at every display
scaling. Standard library only; the same files on every run.

usage: python3 make_art.py OUT_DIR
"""
import math
import os
import random
import struct
import sys
import zlib

INK = (0xEC, 0xE6, 0xD8)
TILE_TOP = (0x24, 0x27, 0x2E)
TILE_BOTTOM = (0x13, 0x15, 0x19)
ORANGE = (0xE8, 0x55, 0x2F)
# one family: the same mark, its dot in each app's colour from the products' palette
ACCENTS = {"tempo": ORANGE, "tempo-vault": (0x8F, 0xB3, 0xD1), "tempo-room": (0xB4, 0x6C, 0xFF)}

# Inno Setup's image areas at 100% to 250% display scaling (its documentation)
LARGE = {100: (164, 314), 125: (202, 386), 150: (240, 459), 175: (290, 556), 200: (315, 604), 225: (366, 700), 250: (416, 797)}
SMALL = {100: 58, 125: 71, 150: 85, 175: 103, 200: 112, 225: 129, 250: 147}
ICON_SIZES = [16, 20, 24, 32, 40, 48, 64, 256]


def clamp01(v):
    return 0.0 if v < 0.0 else 1.0 if v > 1.0 else v


def lerp3(a, b, t):
    return (a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t)


# ---- the mark, as distances in its 64x64 drawing (the SVG's viewBox)

def sd_round_rect(px, py, x0, y0, x1, y1, r):
    cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
    qx = abs(px - cx) - ((x1 - x0) / 2 - r)
    qy = abs(py - cy) - ((y1 - y0) / 2 - r)
    return math.hypot(max(qx, 0.0), max(qy, 0.0)) + min(max(qx, qy), 0.0) - r


def d_segment(px, py, ax, ay, bx, by):
    dx, dy = bx - ax, by - ay
    t = clamp01(((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy))
    return math.hypot(px - ax - t * dx, py - ay - t * dy)


def d_bowl(px, py):
    # the P's bowl: the right half of a circle of radius 9 around (29, 27)
    if px >= 29:
        return abs(math.hypot(px - 29, py - 27) - 9)
    return min(math.hypot(px - 29, py - 18), math.hypot(px - 29, py - 36))


def d_p(px, py):
    # M18 46 V18 h11 a9 9 0 0 1 0 18 H24
    return min(d_segment(px, py, 18, 46, 18, 18), d_segment(px, py, 18, 18, 29, 18), d_bowl(px, py), d_segment(px, py, 29, 36, 24, 36))


def mark_sample(u, v, accent, px):
    """Colour and cover of the mark at (u, v), `px` drawing units per pixel (0 for a point sample)."""
    cover = clamp01(0.5 - sd_round_rect(u, v, 0, 0, 64, 64, 14) / px) if px else float(sd_round_rect(u, v, 0, 0, 64, 64, 14) <= 0)
    if cover <= 0:
        return (0.0, 0.0, 0.0), 0.0
    col = lerp3(TILE_TOP, TILE_BOTTOM, clamp01(v / 64))
    p = clamp01(0.5 - (d_p(u, v) - 2.5) / px) if px else float(d_p(u, v) <= 2.5)
    col = lerp3(col, INK, p)
    dot = clamp01(0.5 - (math.hypot(u - 44, v - 44) - 5) / px) if px else float(math.hypot(u - 44, v - 44) <= 5)
    col = lerp3(col, accent, dot)
    return col, cover


def draw_mark(size, accent):
    """The mark at size x size: rows of (r, g, b, a), a in 0..1 (straight alpha)."""
    units = 64.0 / size
    n = 6 if size < 48 else 1  # small icons: 36 samples a pixel; larger: exact edge distances
    rows = []
    for y in range(size):
        row = []
        for x in range(size):
            if n == 1:
                col, a = mark_sample((x + 0.5) * units, (y + 0.5) * units, accent, units)
            else:
                sr = sg = sb = sa = 0.0
                for sy in range(n):
                    for sx in range(n):
                        c, a = mark_sample((x + (sx + 0.5) / n) * units, (y + (sy + 0.5) / n) * units, accent, 0)
                        sr += c[0] * a
                        sg += c[1] * a
                        sb += c[2] * a
                        sa += a
                a = sa / (n * n)
                col = (sr / sa, sg / sa, sb / sa) if sa else (0.0, 0.0, 0.0)
            row.append((col[0], col[1], col[2], a))
        rows.append(row)
    return rows


# ---- file formats

def to8(v):
    v = int(v + 0.5)
    return 0 if v < 0 else 255 if v > 255 else v


def png_bytes(w, h, rgba_rows):
    raw = bytearray()
    for row in rgba_rows:
        raw.append(0)
        for r, g, b, a in row:
            raw += bytes((to8(r), to8(g), to8(b), to8(a * 255)))

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(bytes(raw), 9)) + chunk(b"IEND", b"")


def dib_bytes(size, rgba_rows):
    xor = bytearray()
    for row in reversed(rgba_rows):
        for r, g, b, a in row:
            xor += bytes((to8(b), to8(g), to8(r), to8(a * 255)))
    stride = (size + 31) // 32 * 4
    mask = bytearray()
    for row in reversed(rgba_rows):
        line = bytearray(stride)
        for x, (_, _, _, a) in enumerate(row):
            if a < 0.5:
                line[x // 8] |= 0x80 >> (x % 8)
        mask += line
    head = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, len(xor) + len(mask), 0, 0, 0, 0)
    return head + bytes(xor) + bytes(mask)


def ico_bytes(images):
    """images: [(size, rgba_rows)], every size a 32-bit bitmap with alpha. (Not PNG: Inno Setup's
    compiler reads each entry as a bitmap when it puts the icon into Setup.exe.)"""
    entries, blobs = [], []
    offset = 6 + 16 * len(images)
    for size, rows in images:
        blob = dib_bytes(size, rows)
        side = 0 if size >= 256 else size
        entries.append(struct.pack("<BBBBHHII", side, side, 0, 0, 1, 32, len(blob), offset))
        blobs.append(blob)
        offset += len(blob)
    return struct.pack("<HHH", 0, 1, len(images)) + b"".join(entries) + b"".join(blobs)


def png_rows(path):
    """A non-interlaced 8-bit PNG (RGB, RGBA or palette): (width, height, rows of RGB bytes)."""
    with open(path, "rb") as f:
        data = f.read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"{path} is not a PNG")
    pos, idat, plte, head = 8, [], b"", None
    while pos + 8 <= len(data):
        n = struct.unpack(">I", data[pos:pos + 4])[0]
        kind, body = data[pos + 4:pos + 8], data[pos + 8:pos + 8 + n]
        pos += 12 + n
        if kind == b"IHDR":
            head = struct.unpack(">IIBBBBB", body)
        elif kind == b"PLTE":
            plte = body
        elif kind == b"IDAT":
            idat.append(body)
        elif kind == b"IEND":
            break
    w, h, depth, ctype, _, _, interlace = head
    if depth != 8 or interlace or ctype not in (2, 3, 6):
        raise ValueError(f"{path}: only 8-bit, non-interlaced RGB, RGBA or palette PNGs")
    bpp = {2: 3, 3: 1, 6: 4}[ctype]
    raw = zlib.decompress(b"".join(idat))
    stride = w * bpp
    prev = bytearray(stride)
    rows = []
    for y in range(h):
        i = y * (stride + 1)
        f, line = raw[i], bytearray(raw[i + 1:i + 1 + stride])
        if f == 1:
            for x in range(bpp, stride):
                line[x] = (line[x] + line[x - bpp]) & 255
        elif f == 2:
            line = bytearray((a + b) & 255 for a, b in zip(line, prev))
        elif f == 3:
            for x in range(stride):
                left = line[x - bpp] if x >= bpp else 0
                line[x] = (line[x] + ((left + prev[x]) >> 1)) & 255
        elif f == 4:
            for x in range(stride):
                a = line[x - bpp] if x >= bpp else 0
                b, c = prev[x], prev[x - bpp] if x >= bpp else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pred = a if pa <= pb and pa <= pc else b if pb <= pc else c
                line[x] = (line[x] + pred) & 255
        elif f != 0:
            raise ValueError(f"{path}: unknown PNG filter {f}")
        prev = line
        if ctype == 3:
            rows.append(b"".join(plte[3 * k:3 * k + 3] for k in line))
        elif ctype == 6:
            rgb = bytearray(w * 3)
            rgb[0::3], rgb[1::3], rgb[2::3] = line[0::4], line[1::4], line[2::4]
            rows.append(bytes(rgb))
        else:
            rows.append(bytes(line))
    return w, h, rows


def bmp_from_rows(w, h, rows):
    """A 24-bit BMP from rows of RGB bytes, top row first."""
    pad = b"\0" * ((4 - (w * 3) % 4) % 4)
    data = bytearray()
    for row in reversed(rows):
        bgr = bytearray(len(row))
        bgr[0::3], bgr[1::3], bgr[2::3] = row[2::3], row[1::3], row[0::3]
        data += bgr + pad
    head = struct.pack("<2sIHHI", b"BM", 54 + len(data), 0, 0, 54)
    info = struct.pack("<IiiHHIIiiII", 40, w, h, 1, 24, 0, len(data), 2835, 2835, 0, 0)
    return head + info + bytes(data)


def bmp_bytes(w, h, R, G, B):
    pad = b"\0" * ((4 - (w * 3) % 4) % 4)
    data = bytearray()
    for y in range(h - 1, -1, -1):
        base = y * w
        for i in range(base, base + w):
            data += bytes((to8(B[i]), to8(G[i]), to8(R[i])))
        data += pad
    head = struct.pack("<2sIHHI", b"BM", 54 + len(data), 0, 0, 54)
    info = struct.pack("<IiiHHIIiiII", 40, w, h, 1, 24, 0, len(data), 2835, 2835, 0, 0)
    return head + info + bytes(data)


# ---- the wizard images

def blend_mark(R, G, B, w, cx, cy, size, accent):
    """The mark, centred at (cx, cy), over the picture."""
    rows = draw_mark(size, accent)
    x0, y0 = int(round(cx - size / 2)), int(round(cy - size / 2))
    for j, row in enumerate(rows):
        for i, (r, g, b, a) in enumerate(row):
            if a <= 0:
                continue
            k = (y0 + j) * w + x0 + i
            R[k] += (r - R[k]) * a
            G[k] += (g - G[k]) * a
            B[k] += (b - B[k]) * a


def splat(R, G, B, w, h, x, y, sigma, col, amount):
    """A soft point of light, added to the picture."""
    reach = sigma * 2.6
    x0, x1 = max(0, int(x - reach)), min(w - 1, int(x + reach) + 1)
    y0, y1 = max(0, int(y - reach)), min(h - 1, int(y + reach) + 1)
    inv = 1.0 / (2 * sigma * sigma)
    for py in range(y0, y1 + 1):
        dy = (py + 0.5 - y) ** 2
        base = py * w
        for px in range(x0, x1 + 1):
            f = amount * math.exp(-((px + 0.5 - x) ** 2 + dy) * inv)
            if f > 0.002:
                k = base + px
                R[k] += col[0] * f
                G[k] += col[1] * f
                B[k] += col[2] * f


def wizard_large(w, h):
    """A charcoal field with the particle body of the Tempo Field, an orbit and the signal dot."""
    s = w / 164.0
    n = w * h
    rnd = random.Random(1991)
    cx, cy, radius = 0.5 * w, 0.585 * h, 0.355 * w
    # the background: a charcoal fall-off, edges a little darker, a faint warm glow behind the body
    top, bottom = (0x21, 0x23, 0x29), (0x0B, 0x0C, 0x0F)
    rows = [lerp3(top, bottom, (y / (h - 1)) ** 1.15) for y in range(h)]
    vig = [1.0 - 0.16 * ((x / (w - 1) - 0.5) * 2) ** 2 for x in range(w)]
    gx = [math.exp(-(((x + 0.5 - cx) / (0.62 * w)) ** 2)) for x in range(w)]
    gy = [math.exp(-(((y + 0.5 - cy) / (0.36 * h)) ** 2)) for y in range(h)]
    hy = [0.035 * math.exp(-(((y + 0.5) / (0.3 * h)) ** 2)) for y in range(h)]
    R, G, B = [0.0] * n, [0.0] * n, [0.0] * n
    for y in range(h):
        br, bg, bb = rows[y]
        gyv, hv = gy[y] * 0.11, hy[y]
        base = y * w
        for x in range(w):
            v, g = vig[x], gx[x] * gyv
            k = base + x
            R[k] = br * v + ORANGE[0] * g + INK[0] * hv
            G[k] = bg * v + ORANGE[1] * g + INK[1] * hv
            B[k] = bb * v + ORANGE[2] * g + INK[2] * hv
    # the orbit, drawn first so the body's light sits on top of it
    tilt = math.radians(-15)
    ca, sa = math.cos(tilt), math.sin(tilt)
    ea, eb = 0.6 * w, 0.14 * w
    ocy = cy + 0.015 * h
    cover = [0.0] * n
    steps = int(2 * math.pi * ea / (0.35 * s)) + 1
    lw = 0.62 * s
    for i in range(steps):
        t = 2 * math.pi * i / steps
        lx, ly = ea * math.cos(t), eb * math.sin(t)
        x, y = cx + lx * ca - ly * sa, ocy + lx * sa + ly * ca
        behind = ly < 0
        inside = math.hypot(x - cx, y - cy) < radius
        alpha = (0.05 if inside else 0.17) if behind else 0.6
        for py in range(max(0, int(y - 2 * lw) - 1), min(h, int(y + 2 * lw) + 2)):
            for px in range(max(0, int(x - 2 * lw) - 1), min(w, int(x + 2 * lw) + 2)):
                d = math.hypot(px + 0.5 - x, py + 0.5 - y)
                c = alpha * clamp01(1.0 - (d - lw * 0.5) / lw)
                k = py * w + px
                if c > cover[k]:
                    cover[k] = c
    for k in range(n):
        c = cover[k]
        if c:
            R[k] += (INK[0] - R[k]) * c * 0.55
            G[k] += (INK[1] - G[k]) * c * 0.55
            B[k] += (INK[2] - B[k]) * c * 0.55
    # the particle body: a soft glow, then points on and inside a sphere, brighter where they face you
    splat(R, G, B, w, h, cx, cy, 0.5 * radius, INK, 0.05)
    for i in range(900):
        # uniform on the sphere, a quarter of them inside it
        z = rnd.uniform(-1, 1)
        phi = rnd.uniform(0, 2 * math.pi)
        r = math.sqrt(1 - z * z) * (1.0 if i % 4 else rnd.random() ** (1 / 3))
        px, py, pz = math.cos(phi) * r, math.sin(phi) * r, z * (1.0 if i % 4 else r)
        depth = (pz + 1) / 2
        bright = 0.07 + 0.93 * depth ** 2.2
        size = s * (0.36 + 0.78 * depth) * rnd.uniform(0.8, 1.2)
        col = ORANGE if depth > 0.6 and rnd.random() < 0.045 else INK
        splat(R, G, B, w, h, cx + px * radius, cy - py * radius, size, col, bright)
    # the signal dot riding the orbit, in front, on the right
    t0 = math.radians(60)
    lx, ly = ea * math.cos(t0), eb * math.sin(t0)
    dx, dy = cx + lx * ca - ly * sa, ocy + lx * sa + ly * ca
    splat(R, G, B, w, h, dx, dy, 7.5 * s, ORANGE, 0.3)
    core = 3.1 * s
    for py in range(max(0, int(dy - core - 2)), min(h, int(dy + core + 3))):
        for px in range(max(0, int(dx - core - 2)), min(w, int(dx + core + 3))):
            c = clamp01(0.5 - (math.hypot(px + 0.5 - dx, py + 0.5 - dy) - core))
            k = py * w + px
            R[k] += (ORANGE[0] - R[k]) * c
            G[k] += (ORANGE[1] - G[k]) * c
            B[k] += (ORANGE[2] - B[k]) * c
    # the mark, near the top
    blend_mark(R, G, B, w, 0.5 * w, 0.135 * h, int(round(0.22 * w)), ORANGE)
    # a whisper of grain, so the dark gradients never band
    for k in range(n):
        g = (rnd.random() - 0.5) * 2.4
        R[k] += g
        G[k] += g
        B[k] += g
    return bmp_bytes(w, h, R, G, B)


def wizard_small(size):
    """The mark on the wizard's white header, with a soft shadow under it."""
    n = size * size
    R, G, B = [255.0] * n, [255.0] * n, [255.0] * n
    tile = size * 0.8
    off = (size - tile) / 2
    units = 64.0 / tile
    sigma = 0.045 * size
    for y in range(size):
        for x in range(size):
            u = (x + 0.5 - off) * units
            v = (y + 0.5 - off - 0.035 * size) * units
            d = max(0.0, sd_round_rect(u, v, 0, 0, 64, 64, 14) / units)
            a = 0.2 * math.exp(-(d * d) / (2 * sigma * sigma))
            k = y * size + x
            R[k] *= 1 - a
            G[k] *= 1 - a
            B[k] *= 1 - a
    rows = draw_mark(int(round(tile)), ORANGE)
    x0 = int(round(off))
    for j, row in enumerate(rows):
        for i, (r, g, b, a) in enumerate(row):
            k = (x0 + j) * size + x0 + i
            R[k] += (r - R[k]) * a
            G[k] += (g - G[k]) * a
            B[k] += (b - B[k]) * a
    return bmp_bytes(size, size, R, G, B)


def main(out):
    os.makedirs(out, exist_ok=True)
    made = []

    def save(name, data):
        with open(os.path.join(out, name), "wb") as f:
            f.write(data)
        made.append(name)

    for name, accent in ACCENTS.items():
        images = [(sz, draw_mark(sz, accent)) for sz in ICON_SIZES]
        save(name + ".ico", ico_bytes(images))
        if name == "tempo":
            save("tempo-256.png", png_bytes(256, 256, images[-1][1]))
    here = os.path.dirname(os.path.abspath(__file__))
    for pct, (w, h) in LARGE.items():
        # the README's lattice look, rendered from brand art sources kept privately (finish_wizard_art.py); drawn here if missing
        png = os.path.join(here, f"wizard-large-{pct}.png")
        if os.path.isfile(png):
            pw, ph, rows = png_rows(png)
            if (pw, ph) != (w, h):
                sys.exit(f"{png} is {pw}x{ph}; Inno Setup's {pct}% size is {w}x{h}")
            save(f"wizard-large-{pct}.bmp", bmp_from_rows(w, h, rows))
        else:
            save(f"wizard-large-{pct}.bmp", wizard_large(w, h))
    for pct, sz in SMALL.items():
        save(f"wizard-small-{pct}.bmp", wizard_small(sz))
    print(f"{len(made)} files in {out}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__.strip())
    main(sys.argv[1])
