#!/usr/bin/env python3
"""Generates the menu bar icon (a "next track" symbol) as macOS template PNGs.

Template images are black with alpha; macOS recolours them for light and dark menu
bars. Writes tray-next.png (22x22) and tray-next@2x.png (44x44) next to this script.
Standard library only: python3 src-tauri/icons/gen_tray_icon.py
"""

import os
import struct
import zlib

SIZE_PT = 22
SUPERSAMPLE = 8

# Shapes in points on the 22x22 canvas: two triangles pointing right, then a bar.
TOP, BOTTOM = 5.0, 17.0
TRIANGLES = [(2.5, 10.5), (10.0, 18.0)]  # (left x, tip x)
BAR = (17.5, 19.5)  # (left x, right x)


def inside(x, y):
    if not TOP <= y <= BOTTOM:
        return False
    mid, half = (TOP + BOTTOM) / 2, (BOTTOM - TOP) / 2
    for left, tip in TRIANGLES:
        if left <= x <= tip:
            # Half height shrinks linearly from the base (left) to the tip.
            if abs(y - mid) <= half * (tip - x) / (tip - left):
                return True
    return BAR[0] <= x <= BAR[1]


def render(scale):
    px = SIZE_PT * scale
    n = SUPERSAMPLE
    rows = []
    for py in range(px):
        row = bytearray([0])  # PNG filter type: none
        for pxi in range(px):
            hits = 0
            for sy in range(n):
                for sx in range(n):
                    x = (pxi + (sx + 0.5) / n) / scale
                    y = (py + (sy + 0.5) / n) / scale
                    hits += inside(x, y)
            alpha = round(255 * hits / (n * n))
            row += bytes([0, 0, 0, alpha])
        rows.append(bytes(row))
    return px, b"".join(rows)


def png(width, raw):
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    header = struct.pack(">IIBBBBB", width, width, 8, 6, 0, 0, 0)  # 8-bit RGBA
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    for scale, name in [(1, "tray-next.png"), (2, "tray-next@2x.png")]:
        width, raw = render(scale)
        with open(os.path.join(here, name), "wb") as f:
            f.write(png(width, raw))
        print(f"{name}: {width}x{width}")


if __name__ == "__main__":
    main()
