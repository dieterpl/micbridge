#!/usr/bin/env python3
"""Renders every form of the micbridge mark from one set of constants.

There is no SVG rasterizer on a stock macOS, and adding one as a build
dependency to draw six rectangles would be absurd. The mark is simple enough to
rasterize analytically, so this script is the single source of truth: it emits
the SVG, the PNGs, the Windows .ico and the macOS .iconset from the same
geometry. Nothing can drift, because nothing is drawn twice.

The same constants are mirrored in crates/micbridge-gui/src/logo.rs, which draws
the mark with the egui painter. GEOMETRY below is the contract between them.

Antialiasing is analytic rather than supersampled: every shape here is a rounded
rectangle or a circle, both of which have a cheap exact signed distance
function, so one sample per pixel gives a clean edge at any size for the cost of
a few million float operations.

    python3 scripts/render-logo.py
"""

import math
import os
import struct
import subprocess
import sys
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
ASSETS = os.path.join(ROOT, "assets")

# ── geometry, in a 64x64 design space ────────────────────────────────────
#
# Five rounded bars rising to a centre peak: a level meter whose silhouette is
# also a span. One primitive and one colour, because the binding constraint is
# legibility as a 16px menu bar item.
#
# An earlier draft sat a capsule above the centre bar to make the microphone
# explicit. It was dropped: a circle on a stem reads as a person icon at every
# size, and a mark that says "user account" on a Dock is worse than one that
# only says "audio".
BADGE_RADIUS = 15.0
BARS = [
    # (x, y, width, height)  — outer pair short, inner pair tall, centre tallest
    (11.0, 38.0, 6.5, 13.0),
    (21.0, 30.0, 6.5, 21.0),
    (28.75, 22.0, 6.5, 29.0),
    (36.5, 30.0, 6.5, 21.0),
    (46.5, 38.0, 6.5, 13.0),
]

# ── palette ──────────────────────────────────────────────────────────────
BADGE = (0x2E, 0x86, 0xD6)   # signal blue, the app accent at icon weight
MARK = (0xF2, 0xF8, 0xFC)    # near-white, high contrast in any Dock
INK = (0x00, 0x00, 0x00)     # menu bar template images are black + alpha


def rounded_rect_sdf(px, py, x, y, w, h, r):
    """Signed distance to a rounded rectangle. Negative inside."""
    cx, cy = x + w / 2.0, y + h / 2.0
    hw, hh = w / 2.0 - r, h / 2.0 - r
    qx, qy = abs(px - cx) - hw, abs(py - cy) - hh
    outside = math.hypot(max(qx, 0.0), max(qy, 0.0))
    inside = min(max(qx, qy), 0.0)
    return outside + inside - r


def coverage(distance, scale):
    """Distance in design units -> pixel coverage, antialiased over one pixel."""
    d = distance * scale
    return min(max(0.5 - d, 0.0), 1.0)


def mark_bounds():
    """The mark's own bounding box in design units, ignoring the badge."""
    left = min(x for x, _, _, _ in BARS)
    right = max(x + w for x, _, w, _ in BARS)
    top = min(y for _, y, _, _ in BARS)
    bottom = max(y + h for _, y, _, h in BARS)
    return left, top, right, bottom


def render(size, badge_rgb, mark_rgb, inset=0.0, badge_alpha=1.0, viewport=None, height=None):
    """RGBA bytes for the mark.

    `inset` shrinks the badge within the canvas, which is what macOS expects of
    an app icon; `badge_alpha` of 0 gives the mark alone on transparency, used
    for the menu bar template image.

    `viewport` is a `(x0, y0, x1, y1)` window in design units, so a caller can
    crop to the artwork rather than to the full 64x64 square — which is what a
    menu bar image needs, since the bar scales whatever it is given to a fixed
    height and empty canvas is height the mark does not get to use.
    """
    x0, y0, x1, y1 = viewport if viewport else (0.0, 0.0, 64.0, 64.0)
    width, height = size, height or size
    # Pixels per design unit, used both to walk the viewport and to set the width
    # of the antialiased edge.
    scale = width / (x1 - x0)
    span = 64.0 - 2.0 * inset
    rows = bytearray()

    for py in range(height):
        rows.append(0)  # PNG filter: none
        dy = y0 + (py + 0.5) * (y1 - y0) / height
        for px in range(width):
            dx = x0 + (px + 0.5) * (x1 - x0) / width

            badge_cov = coverage(
                rounded_rect_sdf(dx, dy, inset, inset, span, span,
                                 BADGE_RADIUS * span / 64.0),
                scale,
            ) * badge_alpha

            # The mark is a union of shapes, so the nearest surface wins.
            d = min(rounded_rect_sdf(dx, dy, *bar, bar[2] / 2.0) for bar in BARS)
            mark_cov = coverage(d, scale)

            # Mark over badge, then the whole thing over transparency.
            alpha = badge_cov + mark_cov * (1.0 - badge_cov)
            if alpha <= 0.0:
                rows += b"\0\0\0\0"
                continue
            out = []
            for i in range(3):
                premul = (badge_rgb[i] * badge_cov * (1.0 - mark_cov)
                          + mark_rgb[i] * mark_cov)
                out.append(int(round(min(premul / alpha, 255.0))))
            rows += bytes(out) + bytes([int(round(alpha * 255))])

    return bytes(rows)


def png(width, raw, height=None):
    height = height or width

    def chunk(tag, data):
        body = tag + data
        return (struct.pack(">I", len(data)) + body
                + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF))

    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9))
            + chunk(b"IEND", b""))


def write_png(path, size, **kw):
    height = kw.get("height") or size
    with open(path, "wb") as fh:
        fh.write(png(size, render(size, **kw), height))
    print("  %-34s %d x %d" % (os.path.basename(path), size, height))


def write_svg(path, badge_rgb, mark_rgb, badge_opacity=1.0):
    hexc = lambda c: "#%02X%02X%02X" % c
    bars = "\n".join(
        '    <rect x="%g" y="%g" width="%g" height="%g" rx="%g"/>'
        % (x, y, w, h, w / 2.0) for x, y, w, h in BARS)
    svg = f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" width="64" height="64">
  <title>micbridge</title>
  <rect width="64" height="64" rx="{BADGE_RADIUS:g}" fill="{hexc(badge_rgb)}" opacity="{badge_opacity:g}"/>
  <g fill="{hexc(mark_rgb)}">
{bars}
  </g>
</svg>
'''
    with open(path, "w") as fh:
        fh.write(svg)
    print("  %-34s vector" % os.path.basename(path))


def write_ico(path, sizes):
    """ICO with PNG-compressed entries, which every Windows since Vista reads."""
    images = [png(s, render(s, badge_rgb=BADGE, mark_rgb=MARK)) for s in sizes]
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = len(header) + 16 * len(images)
    entries, blob = b"", b""
    for size, data in zip(sizes, images):
        entries += struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32,
                               len(data), offset)
        offset += len(data)
        blob += data
    with open(path, "wb") as fh:
        fh.write(header + entries + blob)
    print("  %-34s %s" % (os.path.basename(path), ", ".join(map(str, sizes))))


def main():
    os.makedirs(ASSETS, exist_ok=True)
    print("vector")
    write_svg(os.path.join(ASSETS, "logo.svg"), BADGE, MARK)
    write_svg(os.path.join(ASSETS, "logo-mono.svg"), INK, INK, badge_opacity=0.14)

    print("raster")
    for size in (1024, 512, 256, 128, 64, 32, 16):
        write_png(os.path.join(ASSETS, "logo-%d.png" % size), size,
                  badge_rgb=BADGE, mark_rgb=MARK)

    # Menu bar template images: black plus alpha, which macOS re-tints for the
    # light or dark bar. Any colour here would be discarded.
    #
    # Cropped to the artwork with a hair of padding, because the bar scales
    # whatever it is handed to a fixed 18 pt height. Handed the full 64x64 square
    # the mark occupies less than half of it, so it arrives at roughly 8 pt and
    # sits low — which is exactly how it looked before this crop existed.
    left, top, right, bottom = mark_bounds()
    pad = 1.5
    viewport = (left - pad, top - pad, right + pad, bottom + pad)
    aspect = (viewport[2] - viewport[0]) / (viewport[3] - viewport[1])
    for tall, name in ((18, "tray.png"), (36, "tray@2x.png")):
        write_png(os.path.join(ASSETS, name), round(tall * aspect),
                  height=tall, badge_rgb=INK, mark_rgb=INK, badge_alpha=0.0,
                  viewport=viewport)

    print("windows")
    write_ico(os.path.join(ASSETS, "micbridge.ico"), [16, 32, 48, 64, 128, 256])

    print("macos")
    iconset = os.path.join(ASSETS, "micbridge.iconset")
    os.makedirs(iconset, exist_ok=True)
    # 8.5% inset: macOS app icons do not fill their canvas, and one that does
    # looks oversized next to every other icon in the Dock.
    for base in (16, 32, 128, 256, 512):
        for scale, suffix in ((1, ""), (2, "@2x")):
            write_png(os.path.join(iconset, "icon_%dx%d%s.png" % (base, base, suffix)),
                      base * scale, badge_rgb=BADGE, mark_rgb=MARK, inset=5.5)
    icns = os.path.join(ASSETS, "micbridge.icns")
    try:
        subprocess.run(["iconutil", "-c", "icns", iconset, "-o", icns], check=True)
        print("  %-34s from iconset" % os.path.basename(icns))
    except (FileNotFoundError, subprocess.CalledProcessError) as err:
        print("  iconutil unavailable (%s); .iconset kept for a Mac to convert" % err,
              file=sys.stderr)


if __name__ == "__main__":
    main()
