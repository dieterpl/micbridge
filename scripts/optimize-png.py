#!/usr/bin/env python3
"""Recompresses a PNG in place.

The GUI's own screenshot writer (crates/micbridge-gui/src/screenshot.rs) emits
*stored* deflate blocks so that it needs no compression dependency, which makes
its output perfectly valid and roughly twenty times larger than it needs to be.
A screenshot on its way into the repository goes through here first.

    python3 scripts/optimize-png.py docs/images/*.png
"""

import struct
import sys
import zlib


def chunks(data):
    offset = 8
    while offset < len(data):
        (length,) = struct.unpack(">I", data[offset : offset + 4])
        tag = data[offset + 4 : offset + 8]
        body = data[offset + 8 : offset + 8 + length]
        yield tag, body
        offset += 12 + length


def build(tag, body):
    return struct.pack(">I", len(body)) + tag + body + struct.pack(
        ">I", zlib.crc32(tag + body) & 0xFFFFFFFF
    )


def optimize(path):
    original = open(path, "rb").read()
    if original[:8] != b"\x89PNG\r\n\x1a\n":
        sys.exit(f"{path}: not a PNG")

    # Concatenated because a PNG is allowed to split its image data over any
    # number of IDAT chunks, and they form one zlib stream between them.
    pixels = zlib.decompress(b"".join(b for tag, b in chunks(original) if tag == b"IDAT"))

    out = bytearray(b"\x89PNG\r\n\x1a\n")
    for tag, body in chunks(original):
        if tag == b"IDAT":
            continue
        if tag == b"IEND":
            out += build(b"IDAT", zlib.compress(pixels, 9))
        out += build(tag, body)

    open(path, "wb").write(out)
    before, after = len(original), len(out)
    print(f"{path}: {before // 1024} KB -> {after // 1024} KB ({after * 100 // before}%)")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    for path in sys.argv[1:]:
        optimize(path)
