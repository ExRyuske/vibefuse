#!/usr/bin/env python3
"""Draws the VibeFuse icons into assets/: five level-meter bars hanging
from one line, the middle one longest, so together they read as a "V".

Pure Python (no Pillow): shapes are signed-distance functions, which gives
anti-aliased edges at any size. Run from the repository root:

    python3 scripts/icons.py
"""

import math
import os
import struct
import zlib

OUT = "assets"

# Bar lengths, relative to the longest; the tops line up.
LENGTHS = [0.46, 0.73, 1.0, 0.73, 0.46]
# The bars run from blue to violet.
LEFT_COLOR = (0x4C, 0x8D, 0xFF)
RIGHT_COLOR = (0xA0, 0x6B, 0xFF)


def png(width, height, rgba):
    """8-bit RGBA rows (a list of bytearrays) as a PNG file's bytes."""
    raw = b"".join(b"\x00" + bytes(row) for row in rgba)

    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    header = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def write_png(path, width, height, rgba):
    with open(path, "wb") as f:
        f.write(png(width, height, rgba))


def rounded_rect(px, py, x0, y0, x1, y1, r):
    """Signed distance from a point to a rounded rectangle (negative inside)."""
    cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
    hx, hy = (x1 - x0) / 2 - r, (y1 - y0) / 2 - r
    dx, dy = abs(px - cx) - hx, abs(py - cy) - hy
    outside = math.hypot(max(dx, 0), max(dy, 0))
    inside = min(max(dx, dy), 0)
    return outside + inside - r


def coverage(distance):
    return min(max(0.5 - distance, 0.0), 1.0)


def mix(a, b, t):
    return tuple(a[i] + (b[i] - a[i]) * t for i in range(len(a)))


def over(dst, src, alpha):
    """Composites straight-alpha `src` over premultiplied-free `dst` (RGBA 0..1)."""
    out_a = alpha + dst[3] * (1 - alpha)
    if out_a == 0:
        return (0, 0, 0, 0)
    rgb = tuple((src[i] * alpha + dst[i] * dst[3] * (1 - alpha)) / out_a for i in range(3))
    return rgb + (out_a,)


def bars(box, color):
    """The bars inside `box` = (x0, y0, x1, y1), in pixels. `color(i, t)` gives
    the RGB (0..1) of bar `i`, `t` running from its top to its bottom."""
    x0, y0, x1, y1 = box
    count = len(LENGTHS)
    gap_ratio = 0.62  # gap width relative to a bar's width
    width = (x1 - x0) / (count + (count - 1) * gap_ratio)
    gap = width * gap_ratio
    shapes = []
    for i, length in enumerate(LENGTHS):
        bx = x0 + i * (width + gap)
        shapes.append((i, bx, y0, bx + width, y0 + (y1 - y0) * length, width / 2))
    return shapes, color


def render(size, background, bar_box, bar_color):
    rows = []
    shapes, color = bars(bar_box, bar_color)
    for y in range(size):
        row = bytearray()
        py = y + 0.5
        for x in range(size):
            px = x + 0.5
            pixel = (0.0, 0.0, 0.0, 0.0)
            if background is not None:
                pixel = background(px, py, pixel)
            for i, bx0, by0, bx1, by1, r in shapes:
                d = rounded_rect(px, py, bx0, by0, bx1, by1, r)
                if d < 1:
                    t = min(max((py - by0) / max(by1 - by0, 1), 0), 1)
                    pixel = over(pixel, color(i, t), coverage(d))
            row.extend(round(min(max(c, 0), 1) * 255) for c in pixel)
        rows.append(row)
    return rows


def app_icon(size):
    # The body of a macOS icon fills about 80 % of the canvas.
    margin = size * 0.098
    radius = size * 0.18
    top = (0x24, 0x2A, 0x36)
    bottom = (0x10, 0x13, 0x18)

    def background(px, py, pixel):
        d = rounded_rect(px, py, margin, margin, size - margin, size - margin, radius)
        if d >= 1:
            return pixel
        t = (py - margin) / (size - 2 * margin)
        rgb = tuple(c / 255 for c in mix(top, bottom, min(max(t, 0), 1)))
        return over(pixel, rgb, coverage(d))

    def color(i, t):
        across = mix(LEFT_COLOR, RIGHT_COLOR, i / (len(LENGTHS) - 1))
        lighter = mix(across, (255, 255, 255), 0.18 * (1 - t))
        return tuple(c / 255 for c in lighter)

    inner = size - 2 * margin
    box = (margin + inner * 0.2, margin + inner * 0.22, size - margin - inner * 0.2, size - margin - inner * 0.22)
    return render(size, background, box, color)


def tray_icon(size, rgb):
    """Bars alone, for the menu bar or the notification area."""
    pad = size * 0.06
    def color(i, t):
        if rgb is None:
            return tuple(c / 255 for c in mix(LEFT_COLOR, RIGHT_COLOR, i / (len(LENGTHS) - 1)))
        return rgb
    return render(size, None, (pad, pad * 1.5, size - pad, size - pad * 1.5), color)


def write_ico(path, sizes):
    """An .ico holding the PNGs of these sizes (Windows Vista and later)."""
    images = [(size, png(size, size, app_icon(size))) for size in sizes]
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = len(header) + 16 * len(images)
    entries, data = b"", b""
    for size, image in images:
        side = size if size < 256 else 0  # 0 means 256
        entries += struct.pack("<BBBBHHII", side, side, 0, 0, 1, 32, len(image), offset + len(data))
        data += image
    with open(path, "wb") as f:
        f.write(header + entries + data)


def main():
    os.makedirs(OUT, exist_ok=True)
    # The window uses 256 px; macOS scales its icon from 1024 px.
    for size in (256, 1024):
        write_png(f"{OUT}/icon-{size}.png", size, size, app_icon(size))
    write_ico(f"{OUT}/icon.ico", (16, 32, 48, 64, 256))
    # macOS draws menu bar icons 18 pt tall; 54 px covers 3x screens.
    write_png(f"{OUT}/tray-black.png", 54, 54, tray_icon(54, (0, 0, 0)))
    write_png(f"{OUT}/tray-white.png", 54, 54, tray_icon(54, (1, 1, 1)))
    write_png(f"{OUT}/tray-color.png", 64, 64, tray_icon(64, None))


if __name__ == "__main__":
    main()
