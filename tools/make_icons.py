#!/usr/bin/env python3
"""Generate gitwall's icon set.

The mark is the app's own mechanic: a row of sheared panels with one of them
open and lit. Run from the repo root:

    python3 tools/make_icons.py
"""

from pathlib import Path

from PIL import Image, ImageDraw

OUT = Path(__file__).resolve().parent.parent / "assets" / "icons"

BG = (11, 13, 18, 255)
PANEL = (58, 66, 84, 255)
PANEL_DIM = (36, 42, 55, 255)
ACCENT = (127, 212, 232, 255)

# Render large and downsample — cheap antialiasing for the shear.
SS = 8


def shear_quad(x, y, w, h, slant):
    """A parallelogram leaning left, matching the UI's skewX(-11deg)."""
    return [
        (x + slant, y),
        (x + w + slant, y),
        (x + w, y + h),
        (x, y + h),
    ]


def render(size: int) -> Image.Image:
    s = size * SS
    img = Image.new("RGBA", (s, s), BG)
    d = ImageDraw.Draw(img)

    # Rounded-square ground so the mark reads as an app icon at small sizes.
    d.rounded_rectangle([0, 0, s - 1, s - 1], radius=int(s * 0.22), fill=BG)

    top = s * 0.26
    height = s * 0.48
    slant = height * 0.20

    narrow = s * 0.055
    open_w = s * 0.26
    gap = s * 0.030

    # Two closed panels, the open one, then two more: the strip, mid-scroll.
    widths = [narrow, narrow, open_w, narrow, narrow]
    total = sum(widths) + gap * (len(widths) - 1)
    x = (s - total) / 2 - slant / 2

    for i, w in enumerate(widths):
        focused = i == 2
        fill = ACCENT if focused else (PANEL if i in (1, 3) else PANEL_DIM)
        d.polygon(shear_quad(x, top, w, height, slant), fill=fill)
        x += w + gap

    return img.resize((size, size), Image.LANCZOS)


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    targets = {
        "32x32.png": 32,
        "128x128.png": 128,
        "128x128@2x.png": 256,
        "icon.png": 512,
    }
    for name, size in targets.items():
        render(size).save(OUT / name)
        print(f"  {name:<16} {size}x{size}")
    print(f"wrote {len(targets)} icons to {OUT}")


if __name__ == "__main__":
    main()
