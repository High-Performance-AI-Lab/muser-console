#!/usr/bin/env python3
"""Generate the muser-console social card (1200x630 PNG).

Deterministic output: no randomness, no external assets. The committed
assets/muser-console-social-card.png is the render used as the GitHub
social preview. Re-run after changing any text:

    python3 scripts/generate_social_card.py --check   # verify committed PNG
    python3 scripts/generate_social_card.py           # regenerate
"""

import argparse
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

W, H = 1200, 630
BG = (13, 17, 23)          # deep charcoal
FG = (240, 246, 252)       # near-white
ACCENT = (98, 160, 255)    # console blue
ACCENT2 = (63, 185, 170)   # teal
DIM = (139, 148, 158)      # muted grey

FONT_PATHS = [
    "/System/Library/Fonts/SFNS.ttf",
    "/System/Library/Fonts/Helvetica.ttc",
    "/System/Library/Fonts/SUPPLEMENTARY/Arial Bold.ttf",
]


def font(size: int) -> ImageFont.FreeTypeFont:
    for p in FONT_PATHS:
        if Path(p).exists():
            return ImageFont.truetype(p, size)
    return ImageFont.load_default()


# Deterministic decorative bars for the telemetry strip (not a screenshot:
# pure geometry, fixed values).
BARS = [22, 34, 18, 41, 29, 47, 36, 52, 31, 44, 26, 39, 55, 33, 24, 43, 30, 48, 21, 37]


def render() -> Image.Image:
    img = Image.new("RGB", (W, H), BG)
    d = ImageDraw.Draw(img)

    x = 84
    d.text((x, 96), "muser-console", font=font(112), fill=FG)
    d.text((x, 268), "Live telemetry for muser engines.", font=font(54), fill=ACCENT)
    d.text((x, 352), "Fleet health · cache savings · sessions · gap-preserving history",
           font=font(34), fill=DIM)

    # telemetry strip: alternating accent bars on a baseline
    bx, base, bw = x, 520, 26
    for i, h in enumerate(BARS):
        color = ACCENT if i % 3 else ACCENT2
        d.rectangle([bx, base - h * 3, bx + bw - 6, base], fill=color)
        bx += bw
    d.line([x - 10, base + 6, x + len(BARS) * bw, base + 6], fill=DIM, width=2)

    d.text((x, 560), "github.com/High-Performance-AI-Lab/muser-console",
           font=font(30), fill=DIM)
    return img


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true",
                    help="verify the committed PNG matches a fresh render")
    args = ap.parse_args()

    out = Path(__file__).resolve().parent.parent / "assets" / "muser-console-social-card.png"
    fresh = render()

    if args.check:
        if not out.exists():
            print("FAIL: committed card missing:", out)
            return 1
        committed = Image.open(out).convert("RGB")
        if committed.tobytes() != fresh.tobytes():
            print("FAIL: committed card differs from fresh render")
            return 1
        print("PASS: social card matches render")
        return 0

    out.parent.mkdir(parents=True, exist_ok=True)
    fresh.save(out, format="PNG", optimize=True)
    print("wrote", out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
