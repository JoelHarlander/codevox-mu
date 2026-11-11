#!/usr/bin/env python3
"""Draw the CodeVox mU placeholder icon from the theme tokens.

Colours are the ones in design/tokens.json: background #060606, accent
#FF6A00, text #FAFAFA. The wordmark is Space Mono Bold, the heading face.
Run from the repository root. Rewrites assets/icon.png, assets/icon.ico,
and assets/icon.icns.
"""

import struct
from io import BytesIO
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parents[1]
FONT = ROOT / "assets" / "fonts" / "SpaceMono-Bold.ttf"
OUT_PNG = ROOT / "assets" / "icon.png"
OUT_ICO = ROOT / "assets" / "icon.ico"
OUT_ICNS = ROOT / "assets" / "icon.icns"

BACKGROUND = (0x06, 0x06, 0x06, 255)
ACCENT = (0xFF, 0x6A, 0x00, 255)
TEXT = (0xFA, 0xFA, 0xFA, 255)
FRAME = (0x1A, 0x1A, 0x1A, 255)

MASTER = 1024

# PNG chunks inside an .icns, newest macOS sizes included.
ICNS_CHUNKS = (
    (b"icp4", 16),
    (b"icp5", 32),
    (b"icp6", 64),
    (b"ic07", 128),
    (b"ic08", 256),
    (b"ic09", 512),
    (b"ic10", 1024),
    (b"ic11", 32),
    (b"ic12", 64),
    (b"ic13", 256),
    (b"ic14", 512),
)


def draw_master() -> Image.Image:
    image = Image.new("RGBA", (MASTER, MASTER), BACKGROUND)
    draw = ImageDraw.Draw(image)

    frame = 28
    draw.rectangle(
        [frame, frame, MASTER - 1 - frame, MASTER - 1 - frame],
        outline=FRAME,
        width=18,
    )

    # The shell's accent mark, scaled so it survives a 16px dock icon.
    bar_width = 560
    bar_height = 168
    bar_x = (MASTER - bar_width) // 2
    bar_y = 700
    draw.rectangle(
        [bar_x, bar_y, bar_x + bar_width, bar_y + bar_height],
        fill=ACCENT,
    )

    font = ImageFont.truetype(str(FONT), 390)
    label = "mU"
    bbox = draw.textbbox((0, 0), label, font=font)
    text_width = bbox[2] - bbox[0]
    text_x = (MASTER - text_width) / 2 - bbox[0]
    text_y = 250 - bbox[1]
    draw.text((text_x, text_y), label, font=font, fill=TEXT)
    return image


def png_bytes(image: Image.Image, size: int) -> bytes:
    scaled = image.resize((size, size), Image.Resampling.LANCZOS)
    buffer = BytesIO()
    scaled.save(buffer, format="PNG")
    return buffer.getvalue()


def write_icns(image: Image.Image, path: Path) -> None:
    chunks = []
    for kind, size in ICNS_CHUNKS:
        payload = png_bytes(image, size)
        chunks.append(kind + struct.pack(">I", len(payload) + 8) + payload)
    body = b"".join(chunks)
    path.write_bytes(b"icns" + struct.pack(">I", len(body) + 8) + body)


def write_ico(image: Image.Image, path: Path) -> None:
    source = image.resize((256, 256), Image.Resampling.LANCZOS)
    source.save(
        path,
        format="ICO",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )


def main() -> None:
    master = draw_master()
    icon = master.resize((512, 512), Image.Resampling.LANCZOS)
    icon.save(OUT_PNG, format="PNG")
    write_ico(master, OUT_ICO)
    write_icns(master, OUT_ICNS)


if __name__ == "__main__":
    main()
