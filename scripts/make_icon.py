"""Render the project icon.

    uv run python scripts/make_icon.py 512 mcpb/icon.png
    uv run python scripts/make_icon.py 128 assets/icon.png

Two sizes of one drawing: the 128px one is what the server advertises in its
MCP identity (a short https URL, because modern-era results repeat serverInfo
in every `_meta`), the 512px one is what Claude Desktop shows for the bundle.
Coordinates are in 128ths of the edge, drawn 4x oversized and downsampled —
Pillow's primitives are not antialiased on their own.
"""

from __future__ import annotations

import sys

from PIL import Image, ImageDraw

BLUE = (24, 95, 165, 255)
WHITE = (255, 255, 255, 255)
OVERSAMPLE = 4


def render(size: int) -> Image.Image:
    edge = size * OVERSAMPLE
    u = edge / 128  # one design unit
    img = Image.new("RGBA", (edge, edge), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    draw.rounded_rectangle((0, 0, edge - 1, edge - 1), radius=28 * u, fill=BLUE)

    # Handle first, lens over it, so the ring's inner edge stays a clean circle.
    start, end, half = 72 * u, 100 * u, 5.5 * u
    draw.line((start, start, end, end), fill=WHITE, width=round(half * 2))
    draw.ellipse((end - half, end - half, end + half, end + half), fill=WHITE)

    cx = cy = 54 * u
    outer, inner = 27 * u, 17 * u
    draw.ellipse((cx - outer, cy - outer, cx + outer, cy + outer), fill=WHITE)
    draw.ellipse((cx - inner, cy - inner, cx + inner, cy + inner), fill=BLUE)
    return img.resize((size, size), Image.LANCZOS)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    render(int(sys.argv[1])).save(sys.argv[2], optimize=True)
