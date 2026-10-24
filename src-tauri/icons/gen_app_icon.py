#!/usr/bin/env python3
"""Generates app-icon-source.png (1024x1024): a blue rounded square with a white
play triangle and two sound arcs. Standard library only. Then run:
  python3 src-tauri/icons/gen_app_icon.py && cargo tauri icon src-tauri/icons/app-icon-source.png
"""
import math
import os
import struct
import zlib

N, SS = 1024, 3
MARGIN, RADIUS = 100, 180  # macOS-style inset rounded square


def inside_round_rect(x, y):
    lo, hi = MARGIN, N - MARGIN
    if not (lo <= x <= hi and lo <= y <= hi):
        return False
    cx = min(max(x, lo + RADIUS), hi - RADIUS)
    cy = min(max(y, lo + RADIUS), hi - RADIUS)
    return (x - cx) ** 2 + (y - cy) ** 2 <= RADIUS ** 2


def in_triangle(x, y):
    # play triangle: left edge x=370, tip x=690, vertical 340..684
    if not (370 <= x <= 690):
        return False
    half = (684 - 340) / 2 * (1 - (x - 370) / 320)
    return abs(y - 512) <= half


def on_arc(x, y):
    d = math.hypot(x - 690, y - 512)
    ang = math.degrees(math.atan2(y - 512, x - 690))
    return abs(ang) <= 40 and any(abs(d - r) <= 16 for r in (110, 190)) and x > 700


def color(x, y):
    """Returns (r, g, b, a) float 0..1 for a sub-sample."""
    if not inside_round_rect(x, y):
        return (0, 0, 0, 0)
    t = (y - MARGIN) / (N - 2 * MARGIN)
    base = (0.20 - 0.08 * t, 0.50 - 0.13 * t, 0.78 - 0.13 * t)
    if in_triangle(x, y) or on_arc(x, y):
        return (1, 1, 1, 1)
    return (*base, 1)


rows = []
for py in range(N):
    row = bytearray([0])
    for px in range(N):
        r = g = b = a = 0.0
        for sy in range(SS):
            for sx in range(SS):
                cr, cg, cb, ca = color(px + (sx + 0.5) / SS, py + (sy + 0.5) / SS)
                r += cr * ca; g += cg * ca; b += cb * ca; a += ca
        if a:
            row += bytes([round(255 * r / a), round(255 * g / a), round(255 * b / a),
                          round(255 * a / (SS * SS))])
        else:
            row += bytes(4)
    rows.append(bytes(row))


def chunk(tag, data):
    c = struct.pack(">I", len(data)) + tag + data
    return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)


png = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", N, N, 8, 6, 0, 0, 0))
       + chunk(b"IDAT", zlib.compress(b"".join(rows), 9)) + chunk(b"IEND", b""))
out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "app-icon-source.png")
open(out, "wb").write(png)
print("wrote", out)
