"""Crop a horizontal band out of a tall PNG.

Zero's `--png` mode renders whole pages, which can be tens of thousands of pixels
tall. This pulls out a readable slice for inspection.

    python tools/crop.py page.png out.png 2600 760
"""

import struct
import sys
import zlib


def read_png(path):
    data = open(path, "rb").read()
    pos, width, height, idat = 8, 0, 0, b""
    while pos < len(data):
        length = struct.unpack(">I", data[pos : pos + 4])[0]
        kind = data[pos + 4 : pos + 8]
        body = data[pos + 8 : pos + 8 + length]
        if kind == b"IHDR":
            width, height = struct.unpack(">II", body[:8])
        elif kind == b"IDAT":
            idat += body
        pos += 12 + length
    return width, height, zlib.decompress(idat)


def paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    return b if pb <= pc else c


def defilter(raw, width, height, bpp=4):
    """Undo the per-row PNG filters, returning raw RGBA scanlines."""
    stride = width * bpp
    rows, prev, off = [], bytearray(stride), 0
    for _ in range(height):
        kind = raw[off]
        line = bytearray(raw[off + 1 : off + 1 + stride])
        off += 1 + stride
        for i in range(stride):
            a = line[i - bpp] if i >= bpp else 0
            b = prev[i]
            c = prev[i - bpp] if i >= bpp else 0
            if kind == 1:
                line[i] = (line[i] + a) & 255
            elif kind == 2:
                line[i] = (line[i] + b) & 255
            elif kind == 3:
                line[i] = (line[i] + (a + b) // 2) & 255
            elif kind == 4:
                line[i] = (line[i] + paeth(a, b, c)) & 255
        rows.append(bytes(line))
        prev = line
    return rows


def chunk(kind, body):
    return (
        struct.pack(">I", len(body))
        + kind
        + body
        + struct.pack(">I", zlib.crc32(kind + body) & 0xFFFFFFFF)
    )


def main():
    src, dst, top, height = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
    width, full_height, raw = read_png(src)
    rows = defilter(raw, width, full_height)
    top = max(0, min(top, full_height - 1))
    height = min(height, full_height - top)
    body = b"".join(b"\x00" + rows[y] for y in range(top, top + height))
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(body))
        + chunk(b"IEND", b"")
    )
    open(dst, "wb").write(png)
    print(f"{src} [{top}..{top + height}] of {full_height} -> {dst}")


if __name__ == "__main__":
    main()
