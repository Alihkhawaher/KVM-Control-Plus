#!/usr/bin/env python3
"""Generate the KVM-Control Plus app icon (the mark: a bolted porthole).

Pure stdlib — no Pillow, no cairosvg, no image library of any kind. The mark is
drawn analytically with a signed-distance-ish renderer and 3x3 supersampling,
then written out as:

    assets/icon.ico        multi-size ICO (16..256), 32-bit BGRA entries
    assets/icon_256.png    256px preview for the repo / README
    assets/icon_32.png     32px preview — check this one, if it is mush the mark
                           is too detailed to be an app icon
    assets/icon.rgba       raw 256x256 RGBA, embedded by the app at startup for
                           the window / taskbar icon (eframe::IconData)

Everything is generated from the numbers below, so the icon is reproducible and
tweakable rather than a binary blob someone has to reverse-engineer.

Run:  python tools/make_icon.py
"""

import math
import os
import struct
import zlib

# ---------------------------------------------------------------------------
#  Design
# ---------------------------------------------------------------------------
SIZE = 256          # master render size
SS = 3              # supersampling factor per axis (3x3 = 9 samples/pixel)
SIMPLE_MASTER = 128  # master for the small sizes, rendered without bolts
DETAIL_MIN = 48      # at/above this size, render the bolted version

# Palette. The mark has to read on a dark taskbar AND a light Explorer window,
# so the ring is mid-tone steel rather than near-black.
RING_TOP = (0x4A, 0x58, 0x66)      # lit metal, upper edge
RING_BOT = (0x20, 0x28, 0x31)      # shadowed metal, lower edge
RING_EDGE = (0x14, 0x1A, 0x20)     # thin outline that separates it from light bg
BOLT = (0x2E, 0x39, 0x44)
BOLT_HI = (0xA6, 0xB8, 0xC9)
GLASS_DEEP = (0x07, 0x11, 0x1A)    # the far side of the window
GLASS_RIM = (0x0E, 0x22, 0x2E)
ACCENT = (0x3F, 0xC9, 0xF0)        # the light coming through

# Geometry, as fractions of the half-size.
R_OUT = 0.955
R_RING_IN = 0.760
R_GLASS = 0.735
R_BOLT_ORBIT = 0.862
R_BOLT = 0.038
N_BOLTS = 8


def _mix(a, b, t):
    t = 0.0 if t < 0.0 else (1.0 if t > 1.0 else t)
    return tuple(a[i] + (b[i] - a[i]) * t for i in range(3))


def _smoothstep(edge0, edge1, x):
    if edge1 == edge0:
        return 0.0 if x < edge0 else 1.0
    t = (x - edge0) / (edge1 - edge0)
    t = 0.0 if t < 0.0 else (1.0 if t > 1.0 else t)
    return t * t * (3.0 - 2.0 * t)


def _over(dst, src):
    """Source-over compositing, **premultiplied** output.

    Palette entries are 0..255; alpha is 0..1. Compositing premultiplied and
    un-premultiplying only once at the very end is the only way averaging many
    supersamples at an edge comes out right.
    """
    (dr, dg, db, da) = dst
    (sr, sg, sb), sa = src
    if sa <= 0.0:
        return dst
    oa = sa + da * (1.0 - sa)
    if oa <= 0.0:
        return (0.0, 0.0, 0.0, 0.0)
    return (
        sr * sa + dr * (1.0 - sa),
        sg * sa + dg * (1.0 - sa),
        sb * sa + db * (1.0 - sa),
        oa,
    )


def sample(x, y, detailed=True):
    """One supersample at normalised coords x,y in [-1, 1]. Returns rgb,alpha.

    `detailed=False` drops the bolts and tightens the mark for the small sizes:
    below ~48px eight bolt heads average into grey noise, so the honest fix is to
    render a simpler icon for those sizes rather than ship a smudge.
    """
    px = (0.0, 0.0, 0.0, 0.0)
    r = math.hypot(x, y)
    r_ring_in = R_RING_IN - (0.035 if not detailed else 0.0)

    # ---- glass: the window itself -----------------------------------------
    if r < R_GLASS + 0.03:
        a = 1.0 - _smoothstep(R_GLASS - 0.02, R_GLASS + 0.02, r)
        # radial gradient: deep at the rim, a touch lighter in the middle
        t = r / R_GLASS
        col = _mix(GLASS_RIM, GLASS_DEEP, t)
        # the glow: light coming through from the far side
        glow = math.exp(-((r / 0.44) ** 2))
        col = _mix(col, ACCENT, 0.30 * glow)
        # a soft diagonal reflection across the glass
        streak = math.exp(-(((y - x * 0.62 - 0.34) / 0.20) ** 2))
        streak *= _smoothstep(1.0, 0.2, r)      # fade out toward the rim
        col = _mix(col, (0xFF, 0xFF, 0xFF), 0.055 * streak)
        # inner rim: a bright leak right at the aperture edge
        lip = _smoothstep(R_GLASS - 0.075, R_GLASS - 0.005, r)
        col = _mix(col, ACCENT, 0.55 * lip)
        px = _over(px, (col, a))

    # ---- the bolt ring / bezel --------------------------------------------
    if r_ring_in - 0.04 < r < R_OUT + 0.04:
        a = _smoothstep(r_ring_in - 0.025, r_ring_in + 0.025, r) * (
            1.0 - _smoothstep(R_OUT - 0.030, R_OUT + 0.005, r)
        )
        # vertical metal gradient — a wider range reads as metal, not plastic
        t = (y + 1.0) * 0.5
        col = _mix(RING_TOP, RING_BOT, _smoothstep(-0.05, 1.05, t))
        # Specular: a continuous function of the surface direction.
        # NOTE: this was an atan2() angle at first, which has a branch cut along
        # the negative x-axis — it drew a straight vertical step on the left edge
        # of the ring. A dot-product form has no discontinuity anywhere.
        m = -(x + y) / (r * 1.41421356) if r > 1e-6 else 0.0
        m = m if m > 0.0 else 0.0
        shoulder = m ** 2.2
        gleam = m ** 12.0
        col = _mix(col, (0xFF, 0xFF, 0xFF), 0.40 * shoulder + 0.34 * gleam)
        # occlusion: the bezel darkens as it turns in toward the glass
        seam = _smoothstep(r_ring_in + 0.075, r_ring_in - 0.005, r)
        col = _mix(col, RING_EDGE, 0.72 * seam)
        # ...and darkens again under the outer lip, for a rounded cross-section
        lip = _smoothstep(R_OUT - 0.075, R_OUT + 0.005, r)
        col = _mix(col, RING_EDGE, 0.45 * lip)
        px = _over(px, (col, a))

        # ---- bolts: countersunk, not pearls -------------------------------
        for k in range(N_BOLTS if detailed else 0):
            ang = (2.0 * math.pi * k / N_BOLTS) + math.pi / N_BOLTS
            bx, by = R_BOLT_ORBIT * math.cos(ang), R_BOLT_ORBIT * math.sin(ang)
            db = math.hypot(x - bx, y - by)
            if db < R_BOLT + 0.03:
                # the recess itself: darker than the bezel around it
                ba = 1.0 - _smoothstep(R_BOLT - 0.004, R_BOLT + 0.007, db)
                radial = _smoothstep(0.0, R_BOLT, db)
                bcol = _mix((0x0B, 0x10, 0x15), BOLT, radial)
                # countersink rim: lit along the upper-left arc only
                towards_ul = -((x - bx) + (y - by)) / (R_BOLT * 1.25)
                crescent = _smoothstep(-0.15, 0.75, towards_ul)
                near_rim = _smoothstep(0.25, 0.92, db / R_BOLT)
                bcol = _mix(bcol, BOLT_HI, 0.95 * crescent * near_rim)
                px = _over(px, (bcol, ba))

    # ---- thin outer outline ------------------------------------------------
    ring_a = 1.0 - _smoothstep(R_OUT - 0.012, R_OUT + 0.004, r)
    if r > R_OUT - 0.030:
        if ring_a > 0.0:
            px = _over(px, (RING_EDGE, ring_a * 0.55))

    return px


def render(size, detailed=True):
    """Render the mark at `size`, box-downsampled from a supersampled master."""
    n = size * SS
    half = n / 2.0
    # accumulate supersamples per output pixel
    acc = [[0.0] * 4 for _ in range(size * size)]
    inv = SS * SS
    for sy in range(n):
        y = (sy + 0.5) / half - 1.0
        row_base = (sy // SS) * size
        for sx in range(n):
            x = (sx + 0.5) / half - 1.0
            r, g, b, a = sample(x, y, detailed)
            acc[row_base + (sx // SS)][0] += r
            acc[(row_base + (sx // SS))][1] += g
            acc[(row_base + (sx // SS))][2] += b
            acc[(row_base + (sx // SS))][3] += a
    out = bytearray(size * size * 4)
    for i in range(size * size):
        r, g, b, a = acc[i]
        r /= inv
        g /= inv
        b /= inv
        a /= inv
        # un-premultiply: rgb was accumulated multiplied by its coverage
        if a > 0.0001:
            r, g, b = r / a, g / a, b / a
        out[i * 4 + 0] = max(0, min(255, int(r + 0.5)))
        out[i * 4 + 1] = max(0, min(255, int(g + 0.5)))
        out[i * 4 + 2] = max(0, min(255, int(b + 0.5)))
        out[i * 4 + 3] = max(0, min(255, int(a * 255.0 + 0.5)))
    return out


# ---------------------------------------------------------------------------
#  Encoders (stdlib only)
# ---------------------------------------------------------------------------
def _chunk(tag, data):
    return (
        struct.pack(">I", len(data))
        + tag
        + data
        + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    )


def png_bytes(size, rgba):
    raw = bytearray()
    for y in range(size):
        raw.append(0)                       # filter: none
        raw += rgba[y * size * 4:(y + 1) * size * 4]
    return (
        b"\x89PNG\r\n\x1a\n"
        + _chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + _chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + _chunk(b"IEND", b"")
    )


def dib_bytes(size, rgba):
    """32-bit BGRA DIB for an ICO entry: header + bottom-up XOR + AND mask."""
    hdr = struct.pack(
        "<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, 0, 0, 0, 0, 0
    )
    xor = bytearray()
    for y in range(size - 1, -1, -1):       # bottom-up
        for x in range(size):
            i = (y * size + x) * 4
            xor += bytes((rgba[i + 2], rgba[i + 1], rgba[i + 0], rgba[i + 3]))
    row = ((size + 31) // 32) * 4           # AND mask, 1bpp, 4-byte aligned
    and_mask = bytes(row * size)
    return hdr + bytes(xor) + and_mask


def ico_bytes(images):
    """images: list of (size, entry_bytes)."""
    out = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    dirs = b""
    body = b""
    for size, data in images:
        dirs += struct.pack(
            "<BBBBHHII",
            0 if size >= 256 else size,
            0 if size >= 256 else size,
            0, 0, 1, 32, len(data), offset,
        )
        body += data
        offset += len(data)
    return out + dirs + body


def write_preview_sheet(path, buffers):
    """Compose assets/icon_preview.png: the mark at every size, on a dark band
    (taskbar) and a light band (Explorer), alpha-composited so it is judged the
    way Windows will actually draw it."""
    W, H = 512, 384
    DARK_TOP = 288
    sheet = bytearray(W * H * 4)

    def fill(y0, y1, color):
        for y in range(y0, y1):
            for x in range(W):
                i = (y * W + x) * 4
                sheet[i:i + 4] = bytes((color[0], color[1], color[2], 255))

    fill(0, DARK_TOP, (0x1C, 0x1C, 0x1C))
    fill(DARK_TOP, H, (0xF0, 0xF0, 0xF0))

    def place(size, ox, oy):
        buf = buffers[size]
        for y in range(size):
            for x in range(size):
                s = (y * size + x) * 4
                a = buf[s + 3] / 255.0
                if a <= 0.0:
                    continue
                d = ((oy + y) * W + (ox + x)) * 4
                if not (0 <= oy + y < H and 0 <= ox + x < W):
                    continue
                for c in range(3):
                    sheet[d + c] = int(buf[s + c] * a + sheet[d + c] * (1.0 - a) + 0.5)

    # dark band: the sizes the taskbar and alt-tab use
    place(256, 16, 16)
    place(128, 296, 80)
    place(64, 440, 112)
    # light band: the small sizes, in Explorer and menus
    place(48, 16, DARK_TOP + 8)
    place(32, 80, DARK_TOP + 16)
    place(24, 128, DARK_TOP + 20)
    place(16, 168, DARK_TOP + 24)

    with open(path, "wb") as f:
        f.write(png_bytes_wh(W, H, sheet))
    print(f"  wrote {path}  (preview sheet: all sizes, dark + light)")


def png_bytes_wh(w, h, rgba):
    raw = bytearray()
    for y in range(h):
        raw.append(0)
        raw += rgba[y * w * 4:(y + 1) * w * 4]
    return (
        b"\x89PNG\r\n\x1a\n"
        + _chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
        + _chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + _chunk(b"IEND", b"")
    )


def downsample(src, src_size, size):
    """Box-filter `src` (src_size square RGBA) down to `size` square RGBA."""
    buf = bytearray(size * size * 4)
    step = src_size / size
    for oy in range(size):
        y0, y1 = int(oy * step), max(int((oy + 1) * step), int(oy * step) + 1)
        for ox in range(size):
            x0, x1 = int(ox * step), max(int((ox + 1) * step), int(ox * step) + 1)
            tr = tg = tb = ta = 0
            n = 0
            for yy in range(y0, y1):
                for xx in range(x0, x1):
                    i = (yy * src_size + xx) * 4
                    tr += src[i]
                    tg += src[i + 1]
                    tb += src[i + 2]
                    ta += src[i + 3]
                    n += 1
            i = (oy * size + ox) * 4
            buf[i] = tr // n
            buf[i + 1] = tg // n
            buf[i + 2] = tb // n
            buf[i + 3] = ta // n
    return buf


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    assets = os.path.join(os.path.dirname(here), "assets")
    os.makedirs(assets, exist_ok=True)

    print(f"rendering detailed master {SIZE}px ({SS}x{SS} supersampled)...")
    master = render(SIZE, detailed=True)

    print(f"rendering simplified master {SIMPLE_MASTER}px (no bolts)...")
    simple = render(SIMPLE_MASTER, detailed=False)

    sizes = [256, 128, 64, 48, 32, 24, 16]
    images = []
    buffers = {}
    for s in sizes:
        detailed = s >= DETAIL_MIN
        src, src_size = (master, SIZE) if detailed else (simple, SIMPLE_MASTER)
        buf = src if s == src_size else downsample(src, src_size, s)
        buffers[s] = buf
        images.append((s, dib_bytes(s, buf)))
        if s in (256, 32):
            p = os.path.join(assets, f"icon_{s}.png")
            with open(p, "wb") as f:
                f.write(png_bytes(s, buf))
            print(f"  wrote {p}  ({'detailed' if detailed else 'simplified'})")

    write_preview_sheet(os.path.join(assets, "icon_preview.png"), buffers)

    ico = os.path.join(assets, "icon.ico")
    with open(ico, "wb") as f:
        f.write(ico_bytes(images))
    print(f"  wrote {ico}  ({os.path.getsize(ico)} bytes, sizes {sizes})")

    master_path = os.path.join(assets, "icon.rgba")
    with open(master_path, "wb") as f:
        f.write(master)
    print(f"  wrote {master_path}  (raw {SIZE}x{SIZE} RGBA for in-app use)")


if __name__ == "__main__":
    main()
