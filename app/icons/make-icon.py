#!/usr/bin/env python3
"""Draws the app icon and writes app/icons/AppIcon.icns.

The icon is drawn here rather than taken from a design file, so it can be rebuilt
anywhere with a stock Python 3 and nothing else (no Pillow, no iconutil, no macOS):

    python3 app/icons/make-icon.py            # rewrites app/icons/AppIcon.icns

A dark rounded square on Apple's 1024-point icon grid (824-point body, 100-point
margin), a red "live" dot in the middle and two broadcast arcs on each side. Every
shape is a signed distance field, so the edges are antialiased with one sample per
pixel. Each size is drawn at its own resolution, not scaled down from 1024.

The .icns holds PNG images only, which every macOS since 10.7 reads:
    icp4 16, icp5 32, ic07 128, ic08 256, ic09 512, ic10 1024 (512@2x),
    ic11 32 (16@2x), ic12 64 (32@2x), ic13 256 (128@2x), ic14 512 (256@2x).
build-macos-app.sh copies the file to Contents/Resources/AppIcon.icns and names it
in Info.plist (CFBundleIconFile). The output is deterministic: same script, same bytes.
"""

import math
import os
import struct
import sys
import zlib

BACKGROUND_TOP = (0x2A, 0x31, 0x45)
BACKGROUND_BOTTOM = (0x12, 0x15, 0x1F)
LIVE_RED = (0xF0, 0x3E, 0x3E)
ARC_WHITE = (0xF4, 0xF6, 0xFA)


def rounded_square(x, y, half, radius):
    """Distance to a square of half-width `half` with corners of `radius`, centred on 0."""
    qx = abs(x) - half + radius
    qy = abs(y) - half + radius
    outside = math.hypot(max(qx, 0.0), max(qy, 0.0))
    return outside + min(max(qx, qy), 0.0) - radius


def arc(x, y, radius, width, half_angle):
    """Distance to a ring segment of `radius` around +x, +-`half_angle`, round ends."""
    ax = abs(y)  # symmetric about the horizontal axis
    angle = math.atan2(ax, x)
    if angle <= half_angle:
        return abs(math.hypot(x, ax) - radius) - width / 2
    ex = radius * math.cos(half_angle)
    ey = radius * math.sin(half_angle)
    return math.hypot(x - ex, ax - ey) - width / 2


def coverage(distance, pixel):
    """Area of a pixel inside a shape, from its distance in grid units."""
    return min(max(0.5 - distance / pixel, 0.0), 1.0)


def blend(under, over, alpha):
    return tuple(u + (o - u) * alpha for u, o in zip(under, over))


def draw(size):
    """RGBA rows of the icon at `size` x `size` pixels."""
    pixel = 1024.0 / size  # grid units per pixel
    rows = []
    for py in range(size):
        row = bytearray()
        gy = (py + 0.5) * pixel - 512.0
        shade = min(max((gy + 412.0) / 824.0, 0.0), 1.0)
        background = blend(BACKGROUND_TOP, BACKGROUND_BOTTOM, shade)
        for px in range(size):
            gx = (px + 0.5) * pixel - 512.0
            body = coverage(rounded_square(gx, gy, 412.0, 185.0), pixel)
            if body == 0.0:
                row += b"\0\0\0\0"
                continue
            colour = background
            colour = blend(colour, LIVE_RED, coverage(math.hypot(gx, gy) - 92.0, pixel))
            for radius in (190.0, 300.0):
                d = min(
                    arc(gx, gy, radius, 46.0, math.radians(40)),
                    arc(-gx, gy, radius, 46.0, math.radians(40)),
                )
                colour = blend(colour, ARC_WHITE, coverage(d, pixel))
            row += bytes(int(round(c)) for c in colour) + bytes([int(round(body * 255))])
        rows.append(bytes(row))
    return rows


def png(size, rows):
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    raw = b"".join(b"\0" + r for r in rows)  # filter type 0 on every row
    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)  # 8-bit RGBA
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


ICNS_TYPES = [
    (b"icp4", 16),
    (b"icp5", 32),
    (b"ic07", 128),
    (b"ic08", 256),
    (b"ic09", 512),
    (b"ic10", 1024),
    (b"ic11", 32),
    (b"ic12", 64),
    (b"ic13", 256),
    (b"ic14", 512),
]


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "AppIcon.icns"
    )
    images = {}
    for _, size in ICNS_TYPES:
        if size not in images:
            images[size] = png(size, draw(size))
    body = b"".join(
        kind + struct.pack(">I", 8 + len(images[size])) + images[size]
        for kind, size in ICNS_TYPES
    )
    with open(out, "wb") as f:
        f.write(b"icns" + struct.pack(">I", 8 + len(body)) + body)
    print(f"wrote {out} ({8 + len(body)} bytes)")


if __name__ == "__main__":
    main()
