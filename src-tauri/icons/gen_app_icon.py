#!/usr/bin/env python3
"""Generates app-icon-source.png (1024x1024): a blue rounded square with a white
play triangle and, below it, "SMB" in bold blocky white letters (rectangles and
rounded corners, readable down to 32x32). Standard library only. Then run:
  python3 src-tauri/icons/gen_app_icon.py && cargo tauri icon src-tauri/icons/app-icon-source.png
"""
import os
import struct
import zlib

N, SS = 1024, 3
MARGIN, RADIUS = 100, 180  # macOS-style inset rounded square

# Triangle above the text; the pair is centred in the square.
TRI_LEFT, TRI_TIP, TRI_TOP, TRI_BOTTOM = 398, 642, 206, 486
# Letters: height, stroke, widths (S, M, B), gap; baseline below the triangle.
TEXT_TOP, TEXT_H, STROKE = 586, 236, 46
WIDTHS, GAP = (150, 196, 150), 34
CORNER = 66  # outer radius of the S and B curves


def inside_round_rect(x, y):
    lo, hi = MARGIN, N - MARGIN
    if not (lo <= x <= hi and lo <= y <= hi):
        return False
    cx = min(max(x, lo + RADIUS), hi - RADIUS)
    cy = min(max(y, lo + RADIUS), hi - RADIUS)
    return (x - cx) ** 2 + (y - cy) ** 2 <= RADIUS ** 2


def in_triangle(x, y):
    if not (TRI_LEFT <= x <= TRI_TIP):
        return False
    mid = (TRI_TOP + TRI_BOTTOM) / 2
    half = (TRI_BOTTOM - TRI_TOP) / 2 * (1 - (x - TRI_LEFT) / (TRI_TIP - TRI_LEFT))
    return abs(y - mid) <= half


def in_rounded(x, y, x0, y0, x1, y1, r, corners="tl tr bl br"):
    """Inside the box with the named corners rounded by `r`."""
    if not (x0 <= x <= x1 and y0 <= y <= y1) or x1 <= x0 or y1 <= y0:
        return False
    if r <= 0:
        return True
    for name, cx, cy, in_corner in (
        ("tl", x0 + r, y0 + r, x < x0 + r and y < y0 + r),
        ("tr", x1 - r, y0 + r, x > x1 - r and y < y0 + r),
        ("bl", x0 + r, y1 - r, x < x0 + r and y > y1 - r),
        ("br", x1 - r, y1 - r, x > x1 - r and y > y1 - r),
    ):
        if name in corners and in_corner:
            return (x - cx) ** 2 + (y - cy) ** 2 <= r * r
    return True


def in_ring(x, y, x0, y0, x1, y1, corners):
    """A stroke-wide outline of a box with rounded `corners`."""
    s = STROKE
    return in_rounded(x, y, x0, y0, x1, y1, CORNER, corners) and not in_rounded(
        x, y, x0 + s, y0 + s, x1 - s, y1 - s, CORNER - s, corners)


def near_segment(x, y, ax, ay, bx, by, half):
    dx, dy = bx - ax, by - ay
    t = max(0.0, min(1.0, ((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)))
    px, py = ax + t * dx, ay + t * dy
    return (x - px) ** 2 + (y - py) ** 2 <= half * half


def letter_s(x, y, x0, y0, x1, y1):
    s, mid, xc = STROKE, (y0 + y1) / 2, (x0 + x1) / 2
    all4 = "tl tr bl br"
    upper = in_ring(x, y, x0, y0, x1, mid + s / 2, all4) and not (x > xc and y > y0 + CORNER)
    lower = in_ring(x, y, x0, mid - s / 2, x1, y1, all4) and not (x < xc and y < y1 - CORNER)
    return upper or lower


def letter_m(x, y, x0, y0, x1, y1):
    s, xc = STROKE, (x0 + x1) / 2
    if x0 <= x <= x0 + s or x1 - s <= x <= x1:
        return y0 <= y <= y1
    if y < y0:
        return False
    vy = y0 + 0.64 * (y1 - y0)
    half = s * 0.46
    return (near_segment(x, y, x0 + s / 2, y0 + half, xc, vy, half)
            or near_segment(x, y, x1 - s / 2, y0 + half, xc, vy, half))


def letter_b(x, y, x0, y0, x1, y1):
    s, mid = STROKE, (y0 + y1) / 2
    if x0 <= x <= x0 + s and y0 <= y <= y1:
        return True
    return (in_ring(x, y, x0, y0, x1 - 14, mid + s / 2, "tr br")
            or in_ring(x, y, x0, mid - s / 2, x1, y1, "tr br"))


LETTERS = []
_x = (N - sum(WIDTHS) - GAP * (len(WIDTHS) - 1)) / 2
for _f, _w in zip((letter_s, letter_m, letter_b), WIDTHS):
    LETTERS.append((_f, _x, TEXT_TOP, _x + _w, TEXT_TOP + TEXT_H))
    _x += _w + GAP


def in_text(x, y):
    if not (TEXT_TOP <= y <= TEXT_TOP + TEXT_H):
        return False
    return any(x0 <= x <= x1 and f(x, y, x0, y0, x1, y1) for f, x0, y0, x1, y1 in LETTERS)


def color(x, y):
    """Returns (r, g, b, a) float 0..1 for a sub-sample."""
    if not inside_round_rect(x, y):
        return (0, 0, 0, 0)
    t = (y - MARGIN) / (N - 2 * MARGIN)
    base = (0.20 - 0.08 * t, 0.50 - 0.13 * t, 0.78 - 0.13 * t)
    if in_triangle(x, y) or in_text(x, y):
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
