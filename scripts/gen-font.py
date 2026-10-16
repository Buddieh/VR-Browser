#!/usr/bin/env python3
"""Rasterises printable ASCII from DejaVu Sans into the player's panel fonts.

Output format (player/assets/font-<px>.bin): one byte line height, one byte
baseline, then for each character 32..126 one byte width followed by
width * line height bytes of coverage (row-major, 0..255).

Run: python3 scripts/gen-font.py /usr/share/fonts/truetype/dejavu/DejaVuSans.ttf
"""
import sys
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont

ttf = sys.argv[1]
out_dir = Path(__file__).resolve().parent.parent / "player" / "assets"
for px in (24, 34):
    font = ImageFont.truetype(ttf, px)
    ascent, descent = font.getmetrics()
    height = ascent + descent
    data = bytearray([height, ascent])
    for code in range(32, 127):
        ch = chr(code)
        width = max(1, round(font.getlength(ch)))
        img = Image.new("L", (width, height), 0)
        ImageDraw.Draw(img).text((0, ascent), ch, font=font, fill=255, anchor="ls")
        data.append(width)
        data += img.tobytes()
    (out_dir / f"font-{px}.bin").write_bytes(data)
    print(f"font-{px}.bin: {len(data)} bytes, line height {height}")
