# SPDX-License-Identifier: GPL-2.0-or-later
"""Prepare a photographic grass texture for the shared `.psxt` cooker.

Emits `assets-src/grass.bmp`: a 64x64 true-colour source tile. The arena
cooker owns palette quantisation, atlas layout, and `.psxt` encoding so the
game does not carry a generated Rust texel array.

Source: ambientCG "Grass004", CC0 public domain. CC0 requires no attribution
and puts nothing on this repo's own licence, which is why it was chosen over
the better-known texture libraries that are not CC0.

    curl -L -o g.zip 'https://ambientcg.com/get?file=Grass004_1K-JPG.zip'
    unzip g.zip
    sips -Z 64 Grass004_1K-JPG_Color.jpg --out g64.png
    sips -s format bmp g64.png --out g64.bmp
    python3 tools/grass.py g64.bmp assets-src/grass.bmp

BMP rather than PNG because macOS ships `sips` but not Pillow, and a 24-bit
BMP is forty lines of struct unpacking where a PNG is zlib plus unfiltering.

Two things happen before cooking, and both matter more than the choice of
photograph:

  * Mown stripes are modulated in at 16 texels a band. Doing it here rather
    than at draw time gives the cooker real authored colours to quantise.
  * The source's luminance is stretched. Grass photographs flat, around
    75..123 out of 255, and sixteen colours drawn from that all land on top of
    each other and grey out at any distance.
"""

import struct
import sys

# Texels per mown band, and how far each band moves from the mean.
STRIPE = 16
AMOUNT = 26
# The source's usable luminance window, stretched to fill the palette.
SRC_LO, SRC_HI = 70, 128
# Where the stretched range is then placed, so the pitch sits in the same
# tonal band as the rest of the arena rather than glaring against it.
RANGE, FLOOR = 62, 26

TILE = 64


def read_bmp(path):
    """Bottom-up or top-down 24-bit BMP to a list of rows of (r, g, b)."""
    with open(path, "rb") as source:
        data = source.read()
    off = struct.unpack_from("<I", data, 10)[0]
    width, height = struct.unpack_from("<ii", data, 18)
    bpp = struct.unpack_from("<H", data, 28)[0]
    if bpp != 24:
        raise SystemExit(f"{path}: need a 24-bit BMP, got {bpp}")
    bottom_up = height > 0
    height = abs(height)
    stride = ((width * bpp // 8) + 3) // 4 * 4
    rows = []
    for y in range(height):
        base = off + (height - 1 - y if bottom_up else y) * stride
        rows.append(
            [
                (
                    data[base + x * 3 + 2],
                    data[base + x * 3 + 1],
                    data[base + x * 3],
                )
                for x in range(width)
            ]
        )
    return rows


def stripe_and_stretch(rows):
    out = []
    for y, row in enumerate(rows):
        line = []
        for x, (r, g, b) in enumerate(row):
            k = AMOUNT if (x // STRIPE) % 2 == 0 else -AMOUNT

            def adjust(channel):
                channel = (channel - SRC_LO) * 255 // (SRC_HI - SRC_LO)
                return max(0, min(255, channel * RANGE // 100 + FLOOR + k))

            line.append((adjust(r), adjust(g), adjust(b)))
        out.append(line)
    return out


def write_bmp(path, rows):
    """Write a top-down 24-bit BMP without a Pillow dependency."""
    height = len(rows)
    width = len(rows[0])
    stride = (width * 3 + 3) & ~3
    pixel_bytes = stride * height
    header_bytes = 14 + 40
    with open(path, "wb") as out:
        out.write(
            struct.pack(
                "<2sIHHI", b"BM", header_bytes + pixel_bytes, 0, 0, header_bytes
            )
        )
        out.write(
            struct.pack(
                "<IiiHHIIiiII",
                40,
                width,
                -height,
                1,
                24,
                0,
                pixel_bytes,
                2835,
                2835,
                0,
                0,
            )
        )
        padding = b"\0" * (stride - width * 3)
        for row in rows:
            for r, g, b in row:
                out.write(bytes((b, g, r)))
            out.write(padding)


def main():
    if len(sys.argv) != 3:
        raise SystemExit("usage: grass.py <in.bmp> <out.bmp>")
    rows = stripe_and_stretch(read_bmp(sys.argv[1]))
    if len(rows) != TILE or len(rows[0]) != TILE:
        raise SystemExit(f"need a {TILE}x{TILE} source, got {len(rows[0])}x{len(rows)}")
    write_bmp(sys.argv[2], rows)
    print(f"{sys.argv[2]}: {TILE}x{TILE} 24-bit prepared grass")


main()
