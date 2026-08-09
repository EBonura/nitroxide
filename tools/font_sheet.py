#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""Render every PSoXide bitmap font as a comparison sheet.

Picking a font by reading 38 crate names is guesswork; picking one by looking
at the same line of text in all of them takes a minute. The generated `.rs`
files hold the glyph bitmaps as plain byte arrays with the metrics right below
them, so this parses them directly rather than going through a PS1 build.

Bit order is LSB-first: bit 0 of each byte is the leftmost pixel, and each row
is `ceil(glyph_w / 8)` bytes.

    python3 tools/font_sheet.py <psx-font-src-dir> <out.png> [sample text]
"""

import re
import sys
import zlib
import struct
from pathlib import Path

SCALE = 2
LABEL_SCALE = 1
PAD = 6
LABEL_W = 150


def parse(path):
    """Pull the bitmap and metrics out of one generated font module."""
    text = path.read_text()

    def field(name, cast=int):
        m = re.search(rf"^\s*{name}:\s*([^,\n]+),", text, re.M)
        if not m:
            return None
        raw = m.group(1).strip()
        return cast(raw, 0) if cast is int else cast(raw)

    body = re.search(r"BITMAP: \[u8; \d+\] = \[(.*?)\n\];", text, re.S)
    if not body:
        return None
    data = [int(b, 16) for b in re.findall(r"0x([0-9a-fA-F]{2})", body.group(1))]

    order = re.search(r"bit_order:\s*BitOrder::(\w+)", text)
    glyph_w = field("glyph_w")
    glyph_h = field("glyph_h")
    if glyph_w is None or glyph_h is None:
        return None
    return {
        "name": path.stem,
        "w": glyph_w,
        "h": glyph_h,
        "first": field("first_char"),
        "count": field("glyph_count"),
        "advance": field("advance_x") or glyph_w,
        "msb": bool(order and order.group(1) == "Msb"),
        "data": data,
    }


def glyph_rows(font, ch):
    """The glyph for `ch` as a list of rows of booleans."""
    idx = ord(ch) - font["first"]
    if idx < 0 or idx >= font["count"]:
        idx = 0
    row_bytes = (font["w"] + 7) // 8
    base = idx * row_bytes * font["h"]
    out = []
    for y in range(font["h"]):
        row = []
        for x in range(font["w"]):
            byte = font["data"][base + y * row_bytes + x // 8]
            # Msb fonts put the leftmost pixel in bit 7, not bit 0. Decoding
            # one as the other renders it mirrored, which is exactly what the
            # first sheet did to a couple of these.
            bit = (7 - x % 8) if font["msb"] else (x % 8)
            row.append(bool(byte >> bit & 1))
        out.append(row)
    return out


def draw(canvas, cw, x0, y0, font, text, colour, scale=SCALE):
    pen = x0
    for ch in text:
        rows = glyph_rows(font, ch)
        for y, row in enumerate(rows):
            for x, on in enumerate(row):
                if not on:
                    continue
                for sy in range(scale):
                    for sx in range(scale):
                        px = pen + x * scale + sx
                        py = y0 + y * scale + sy
                        if 0 <= px < cw and 0 <= py < len(canvas):
                            canvas[py][px] = colour
        pen += font["advance"] * scale
    return pen


def png(path, canvas):
    h, w = len(canvas), len(canvas[0])
    raw = b"".join(
        b"\x00" + b"".join(bytes(px) for px in row) for row in canvas
    )

    def chunk(tag, payload):
        c = struct.pack(">I", len(payload)) + tag + payload
        return c + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF)

    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def main():
    src = Path(sys.argv[1])
    out = Path(sys.argv[2])
    sample = sys.argv[3] if len(sys.argv) > 3 else "PLAY GARAGE 3-1"

    fonts = []
    for path in sorted(src.glob("*.rs")):
        if path.stem in ("mod", "ext_latin", "boxdraw"):
            continue
        f = parse(path)
        if f and f["first"] is not None and f["count"]:
            fonts.append(f)

    row_h = max(f["h"] for f in fonts) * SCALE + PAD
    height = row_h * len(fonts) + PAD
    width = LABEL_W + max(f["advance"] for f in fonts) * SCALE * len(sample) + 40
    canvas = [[(16, 18, 30)] * width for _ in range(height)]

    # A plain, legible face for the names, so the sheet is about the samples.
    label_font = next(
        f for f in fonts if f["name"] in ("basic", "basic_8x16", "spleen_5x8")
    )
    for i, f in enumerate(fonts):
        y = PAD + i * row_h
        # Alternate row tint, so 30-odd lines stay readable.
        if i % 2:
            for yy in range(y - 2, min(y + row_h - 2, height)):
                canvas[yy] = [(24, 27, 42)] * width
        draw(
            canvas,
            width,
            4,
            y + 4,
            label_font,
            f["name"][:17].upper(),
            (130, 150, 195),
            LABEL_SCALE,
        )
        draw(canvas, width, LABEL_W, y, f, sample, (240, 240, 245))

    png(out, canvas)
    print(f"{len(fonts)} fonts -> {out}")


main()
