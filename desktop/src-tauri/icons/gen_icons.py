#!/usr/bin/env python3
"""Generate ARC desktop icons from a solid gradient square.
Uses only the standard library (zlib + struct) so no PIL dep."""
import os
import struct
import zlib

OUT = os.path.dirname(os.path.abspath(__file__))


ARC_BLUE = (59, 99, 245)  # #3B63F5 — primary brand (swap with brandpad hex once available)
WHITE = (255, 255, 255)


def make_png(width: int, height: int) -> bytes:
    """Solid-blue app icon with a white lowercase 'arc' wordmark.
    Per brandpad.io/arc spec: white logotype inside a solid colour container.
    Wordmark drawn with a 5x7 monospace glyph bitmap — no font dependency."""
    raw = bytearray()
    glyph_pixels = _wordmark_mask(width, height)
    for y in range(height):
        raw.append(0)  # filter byte
        for x in range(width):
            if glyph_pixels[y * width + x]:
                raw += bytes([*WHITE, 255])
            else:
                raw += bytes([*ARC_BLUE, 255])

    return _encode_png(raw, width, height)


# Compact 5x7 glyph bitmaps for a–z (only a, r, c used but we'll include more
# for completeness in case we ever render another word).
GLYPHS_5x7 = {
    "a": [
        "01110",
        "00001",
        "01111",
        "10001",
        "10001",
        "10001",
        "01111",
    ],
    "r": [
        "00000",
        "00000",
        "10110",
        "11001",
        "10000",
        "10000",
        "10000",
    ],
    "c": [
        "00000",
        "00000",
        "01110",
        "10001",
        "10000",
        "10001",
        "01110",
    ],
}


def _wordmark_mask(w: int, h: int) -> list[bool]:
    """Render `arc` centred in a w×h buffer. Returns a flat list of booleans."""
    word = "arc"
    g_w, g_h = 5, 7
    # Space the glyphs with one pixel gap, same scale throughout.
    total_glyph_w = len(word) * g_w + (len(word) - 1)
    # Scale so the wordmark fills ~52% of the icon width.
    scale = max(1, int(min(w * 0.52 / total_glyph_w, h * 0.48 / g_h)))
    pixel_w = total_glyph_w * scale
    pixel_h = g_h * scale
    offset_x = (w - pixel_w) // 2
    offset_y = (h - pixel_h) // 2

    mask = [False] * (w * h)
    for i, ch in enumerate(word):
        bits = GLYPHS_5x7[ch]
        gx = offset_x + i * (g_w + 1) * scale
        for ry, row in enumerate(bits):
            for rx, cell in enumerate(row):
                if cell != "1":
                    continue
                px = gx + rx * scale
                py = offset_y + ry * scale
                for dy in range(scale):
                    for dx in range(scale):
                        yy = py + dy
                        xx = px + dx
                        if 0 <= xx < w and 0 <= yy < h:
                            mask[yy * w + xx] = True
    return mask


def _encode_png(raw: bytearray, width: int, height: int) -> bytes:
    def chunk(kind: bytes, data: bytes) -> bytes:
        length = struct.pack("!I", len(data))
        kd = kind + data
        crc = struct.pack("!I", zlib.crc32(kd) & 0xFFFFFFFF)
        return length + kd + crc

    sig = b"\x89PNG\r\n\x1a\n"
    ihdr = struct.pack("!IIBBBBB", width, height, 8, 6, 0, 0, 0)
    idat = zlib.compress(bytes(raw), 9)
    return sig + chunk(b"IHDR", ihdr) + chunk(b"IDAT", idat) + chunk(b"IEND", b"")


def make_png_wrapper(w: int, h: int) -> bytes:
    return make_png(w, h)


sizes = {
    "32x32.png": 32,
    "128x128.png": 128,
    "128x128@2x.png": 256,
    "icon.png": 512,
}

for name, size in sizes.items():
    data = make_png(size, size)
    path = os.path.join(OUT, name)
    with open(path, "wb") as f:
        f.write(data)
    print(f"wrote {path} ({size}x{size}, {len(data)} bytes)")

# .ico: bundle 32x32, 64x64, 128x128 png-in-ico
def make_ico() -> bytes:
    entries = []
    for size in (32, 64, 128):
        png = make_png(size, size)
        entries.append((size, png))

    header = struct.pack("!HHH", 0, 1, len(entries))
    # little-endian for Windows
    header = struct.pack("<HHH", 0, 1, len(entries))
    offset = 6 + 16 * len(entries)
    dir_entries = bytearray()
    data = bytearray()
    for size, png in entries:
        w = 0 if size >= 256 else size
        h = 0 if size >= 256 else size
        dir_entries += struct.pack(
            "<BBBBHHII", w, h, 0, 0, 1, 32, len(png), offset
        )
        data += png
        offset += len(png)
    return header + bytes(dir_entries) + bytes(data)


with open(os.path.join(OUT, "icon.ico"), "wb") as f:
    f.write(make_ico())
print("wrote icon.ico")

# .icns: minimal — Tauri's macOS bundler prefers icns. We'll just reuse the 512 PNG.
# macOS iconutil would normally do this; we approximate by writing a fake icns header
# that embeds one PNG. If Tauri rejects it, run `iconutil -c icns` on an .iconset dir.
def make_icns_from_png(png_512: bytes) -> bytes:
    body = b"ic09" + struct.pack(">I", 8 + len(png_512)) + png_512  # 512x512 PNG slot
    return b"icns" + struct.pack(">I", 8 + len(body)) + body


with open(os.path.join(OUT, "icon.icns"), "wb") as f:
    f.write(make_icns_from_png(make_png(512, 512)))
print("wrote icon.icns")
