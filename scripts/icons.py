#!/usr/bin/env python3
"""Make RealTimeX Link's icon files: RealTimeX's icon (assets/realtimex-icon.png) with Link's badge
(assets/badge.svg) in its bottom-right corner, and the tray icon: that same icon without its navy
square.

  assets/icon-<size>.png   16 to 1024 px (Linux, docs)
  assets/icon.ico          Windows (16 to 256 px)
  assets/icon.icns         macOS (iconutil)
  crates/link/icons/*.rgba raw RGBA for rtx-link: its window icon (256 px), the tray icon on
                           Windows and Linux (the app icon in colour without its navy square,
                           32 px) and the macOS menu bar template (the same shape in black on
                           transparent with the badge's chain cut out, 44 px: 22 pt at 2x)

Renders the SVGs with Chrome (headless) and scales with Pillow. Run it after changing an SVG:
  python3 scripts/icons.py
"""
import os
import shutil
import subprocess
import sys
import tempfile

from PIL import Image

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ASSETS = os.path.join(ROOT, "assets")
RGBA_DIR = os.path.join(ROOT, "crates", "link", "icons")
CHROME_CANDIDATES = [
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "google-chrome",
    "chromium",
]


def chrome():
    for candidate in CHROME_CANDIDATES:
        path = shutil.which(candidate) or (candidate if os.path.exists(candidate) else None)
        if path:
            return path
    sys.exit("Chrome or Chromium is needed to render the SVGs")


def render(svg_path, size, out_png):
    """The SVG at size x size px, transparent where it is."""
    with open(svg_path, encoding="utf-8") as handle:
        svg = handle.read()
    svg = svg.replace("<svg ", f'<svg style="display:block" ', 1)
    svg = svg.replace('width="1024" height="1024"', f'width="{size}" height="{size}"', 1)
    svg = svg.replace('width="32" height="32"', f'width="{size}" height="{size}"', 1)
    with tempfile.TemporaryDirectory() as tmp:
        page = os.path.join(tmp, "page.html")
        with open(page, "w", encoding="utf-8") as handle:
            handle.write(f"<!doctype html><html><body style='margin:0;background:transparent'>{svg}</body></html>")
        subprocess.run(
            [
                chrome(),
                "--headless=new",
                "--disable-gpu",
                "--hide-scrollbars",
                "--default-background-color=00000000",
                f"--window-size={size},{size}",
                f"--screenshot={out_png}",
                "file://" + page,
            ],
            check=True,
            capture_output=True,
        )
    image = Image.open(out_png).convert("RGBA").crop((0, 0, size, size))
    image.save(out_png)
    return image


# The badge: about 44% of the icon, in its bottom-right corner, clear of the edge.
BADGE_SIZE = 452
BADGE_MARGIN = 20


def compose(badge):
    """RealTimeX's icon with Link's badge in the bottom-right corner, 1024 px."""
    base = Image.open(os.path.join(ASSETS, "realtimex-icon.png")).convert("RGBA").resize((1024, 1024), Image.LANCZOS)
    at = 1024 - BADGE_SIZE - BADGE_MARGIN
    base.alpha_composite(badge, (at, at))
    return base


# The tray icon: the app icon's navy square goes. A pixel's opacity follows its distance from the
# navy (the square is within 15 of it, the artwork at least 117 away), and its colour is the
# artwork's own with the navy blended into the edge taken back out. The badge's navy ring goes too,
# which leaves a gap between the badge and the ring under it.
NAVY = (3, 45, 110)
CLEAR_BELOW = 24
OPAQUE_FROM = 96


def ramp(value, low, high):
    return min(max((value - low) / (high - low), 0.0), 1.0)


def pixels(image):
    data = image.tobytes()
    return zip(data[0::4], data[1::4], data[2::4], data[3::4])


def from_pixels(size, out):
    return Image.frombytes("RGBA", size, bytes(c for pixel in out for c in pixel))


def without_background(image):
    out = []
    for r, g, b, _ in pixels(image):
        alpha = ramp(((r - NAVY[0]) ** 2 + (g - NAVY[1]) ** 2 + (b - NAVY[2]) ** 2) ** 0.5, CLEAR_BELOW, OPAQUE_FROM)
        if alpha == 0:
            out.append((0, 0, 0, 0))
            continue
        out.append(tuple(round(min(max((c - (1 - alpha) * n) / alpha, 0), 255)) for c, n in zip((r, g, b), NAVY)) + (round(alpha * 255),))
    return from_pixels(image.size, out)


# The macOS template: every pixel of the tray icon black, and the badge's white chain (the only
# near-white in the icon) a hole, so it shows through the black badge.
def template(tray):
    out = []
    for r, g, b, a in pixels(tray):
        out.append((0, 0, 0, round(a * (1 - ramp(min(r, g, b), 80, 220)))))
    return from_pixels(tray.size, out)


def write_rgba(image, size, name, resample=Image.LANCZOS):
    os.makedirs(RGBA_DIR, exist_ok=True)
    # Premultiplied while scaling, so the transparent pixels' colour doesn't bleed into the edges.
    resized = image.convert("RGBa").resize((size, size), resample).convert("RGBA")
    with open(os.path.join(RGBA_DIR, name), "wb") as handle:
        handle.write(resized.tobytes())


def main():
    with tempfile.TemporaryDirectory() as tmp:
        master = compose(render(os.path.join(ASSETS, "badge.svg"), BADGE_SIZE, os.path.join(tmp, "badge.png")))

    sizes = [16, 32, 48, 64, 128, 256, 512, 1024]
    for size in sizes:
        master.resize((size, size), Image.LANCZOS).save(os.path.join(ASSETS, f"icon-{size}.png"))

    master.save(os.path.join(ASSETS, "icon.ico"), sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)])

    if shutil.which("iconutil"):
        with tempfile.TemporaryDirectory() as tmp:
            iconset = os.path.join(tmp, "icon.iconset")
            os.makedirs(iconset)
            for size in (16, 32, 128, 256, 512):
                master.resize((size, size), Image.LANCZOS).save(os.path.join(iconset, f"icon_{size}x{size}.png"))
                master.resize((size * 2, size * 2), Image.LANCZOS).save(os.path.join(iconset, f"icon_{size}x{size}@2x.png"))
            subprocess.run(["iconutil", "-c", "icns", iconset, "-o", os.path.join(ASSETS, "icon.icns")], check=True)
    else:
        print("iconutil not found (macOS only): assets/icon.icns not updated")

    write_rgba(master, 256, "window-256.rgba")
    # The tray icons are scaled down by area (box): fine detail averages out instead of ringing.
    tray = without_background(master)
    write_rgba(tray, 32, "tray-32.rgba", Image.BOX)
    write_rgba(template(tray), 44, "tray-template-44.rgba", Image.BOX)
    print("icons written to assets/ and crates/link/icons/")


if __name__ == "__main__":
    main()
