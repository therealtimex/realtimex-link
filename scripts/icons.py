#!/usr/bin/env python3
"""Make RealTimeX Link's icon files from assets/icon.svg and assets/tray.svg.

  assets/icon-<size>.png   16 to 1024 px (Linux, docs)
  assets/icon.ico          Windows (16 to 256 px)
  assets/icon.icns         macOS (iconutil)
  crates/link/icons/*.rgba raw RGBA for rtx-link: its window icon (256 px), the tray icon on
                           Windows and Linux (the app icon, 32 px) and the macOS menu bar template
                           (black on transparent, 44 px: 22 pt at 2x)

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


def write_rgba(image, size, name):
    os.makedirs(RGBA_DIR, exist_ok=True)
    resized = image.resize((size, size), Image.LANCZOS)
    with open(os.path.join(RGBA_DIR, name), "wb") as handle:
        handle.write(resized.tobytes())


def main():
    with tempfile.TemporaryDirectory() as tmp:
        master = render(os.path.join(ASSETS, "icon.svg"), 1024, os.path.join(tmp, "icon.png"))
        tray = render(os.path.join(ASSETS, "tray.svg"), 176, os.path.join(tmp, "tray.png"))

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
    write_rgba(master, 32, "tray-32.rgba")
    write_rgba(tray, 44, "tray-template-44.rgba")
    print("icons written to assets/ and crates/link/icons/")


if __name__ == "__main__":
    main()
