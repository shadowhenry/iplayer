#!/usr/bin/env python3
"""Generate the iPlayer application icon set (PNG / ICNS / ICO)."""

import os
import shutil
import subprocess
import sys

from PIL import Image, ImageDraw

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ICON_DIR = os.path.join(ROOT, "src-tauri", "icons")

SS = 4  # supersampling factor


def lerp(a, b, t):
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


def rounded_mask(size, radius):
    m = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(m)
    d.rounded_rectangle((0, 0, size - 1, size - 1), radius=radius, fill=255)
    return m


def make_icon(size=1024, source=None):
    if source:
        img = Image.open(source).convert("RGBA")
        return img.resize((size, size), Image.LANCZOS)
    s = size * SS
    # --- gradient background -------------------------------------------------
    top = (0x5C, 0x9B, 0xFF)
    bottom = (0x2A, 0x5B, 0xE8)
    grad = Image.new("RGB", (s, s))
    px = grad.load()
    for y in range(s):
        # diagonal-ish gradient
        base = y / (s - 1)
        for x in range(0, s, 1):
            t = min(1.0, base * 0.82 + (x / (s - 1)) * 0.18)
            px[x, y] = lerp(top, bottom, t)

    # --- soft highlight ------------------------------------------------------
    hi = Image.new("L", (s, s), 0)
    ImageDraw.Draw(hi).ellipse(
        (-s * 0.35, -s * 0.85, s * 0.95, s * 0.45), fill=52
    )
    grad = Image.composite(Image.new("RGB", (s, s), (255, 255, 255)), grad, hi)

    icon = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    icon.paste(grad, (0, 0), rounded_mask(s, int(s * 0.225)))

    # --- play triangle -------------------------------------------------------
    d = ImageDraw.Draw(icon)
    cx, cy = s / 2, s / 2
    r = s * 0.20
    offset = s * 0.022
    pts = [
        (cx - r * 0.86 + offset, cy - r),
        (cx - r * 0.86 + offset, cy + r),
        (cx + r * 1.02 + offset, cy),
    ]
    d.polygon(pts, fill=(255, 255, 255, 255))

    return icon.resize((size, size), Image.LANCZOS)


def main():
    os.makedirs(ICON_DIR, exist_ok=True)
    src = sys.argv[1] if len(sys.argv) > 1 else None
    master = make_icon(1024, source=src)
    master.save(os.path.join(ICON_DIR, "source.png"))

    sizes = {
        "32x32.png": 32,
        "128x128.png": 128,
        "128x128@2x.png": 256,
        "icon.png": 512,
        "Square30x30Logo.png": 30,
        "Square44x44Logo.png": 44,
        "Square71x71Logo.png": 71,
        "Square89x89Logo.png": 89,
        "Square107x107Logo.png": 107,
        "Square142x142Logo.png": 142,
        "Square150x150Logo.png": 150,
        "Square284x284Logo.png": 284,
        "Square310x310Logo.png": 310,
        "StoreLogo.png": 50,
    }
    for name, size in sizes.items():
        master.resize((size, size), Image.LANCZOS).save(os.path.join(ICON_DIR, name))

    # --- .ico ---------------------------------------------------------------
    master.save(
        os.path.join(ICON_DIR, "icon.ico"),
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )

    # --- .icns (macOS) ------------------------------------------------------
    if shutil.which("iconutil"):
        iconset = os.path.join(ICON_DIR, "icon.iconset")
        if os.path.isdir(iconset):
            shutil.rmtree(iconset)
        os.makedirs(iconset)
        for size in (16, 32, 64, 128, 256, 512, 1024):
            master.resize((size, size), Image.LANCZOS).save(
                os.path.join(iconset, f"icon_{size}x{size}.png")
            )
            if size <= 512:
                master.resize((size * 2, size * 2), Image.LANCZOS).save(
                    os.path.join(iconset, f"icon_{size}x{size}@2x.png")
                )
        subprocess.run(
            ["iconutil", "-c", "icns", iconset, "-o", os.path.join(ICON_DIR, "icon.icns")],
            check=True,
        )
        shutil.rmtree(iconset)
        print("icon.icns written")
    else:
        print("iconutil not found — skipping .icns (macOS only)", file=sys.stderr)

    print(f"icons written to {ICON_DIR}")


if __name__ == "__main__":
    main()
