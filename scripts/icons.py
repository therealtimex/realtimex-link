#!/usr/bin/env python3
"""Make RealTimeX Link's icon files: RealTimeX's icon for each platform with Link's badge
(assets/badge.svg) in its bottom-right corner, so Link has RealTimeX's shape everywhere, and the
tray icon: the Windows icon without its navy square.

  RealTimeX's icons (from realtimex-ai-app's frontend/assets), the bases:
  assets/realtimex-icon.png        Windows and the tray: a full-bleed square (icon.png)
  assets/realtimex-icon-macos.png  macOS: an 824 px rounded square in 1024 px with a transparent
                                   margin, the Big Sur grid (the 1024 px image in icon.icns)
  assets/realtimex-icon-linux.png  Linux: a rounded square inset 47/512 (linux-icons/512x512.png)

  assets/icon-<size>.png   Linux: 16 to 512 px, installed into the hicolor icon theme (1024: docs)
  assets/icon.ico          Windows (16 to 256 px, with 20 and 40 for 125% scaling)
  assets/icon.icns         macOS (iconutil)
  crates/link/icons/*.rgba raw RGBA for rtx-link: its window icon (256 px; Windows and Linux X11,
                           macOS uses the .app's icon.icns), the tray icon on Windows and Linux
                           (the app icon in colour without its navy square, 32 px) and the macOS
                           menu bar template (the same shape in black on transparent with the
                           badge's chain cut out, 44 px: 22 pt at 2x)
  crates/link/icons/*-other.rgba  the two tray icons of another Link instance (one a RealTimeX
                           runs with a data folder of its own): a dot at the top left, opposite
                           the badge, with a transparent gap around it

Renders the SVGs with Chrome (headless) and scales with Pillow. Run it after changing an SVG:
  python3 scripts/icons.py
Only the other instance's tray icons, from the tray icons already in crates/link/icons:
  python3 scripts/icons.py other
"""
import os
import shutil
import subprocess
import sys
import tempfile

from PIL import Image, ImageDraw

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


# Each base and where its body (the opaque square) lies in 1024 px: the badge is a share of the
# body, in its bottom-right corner. The full-bleed one keeps the badge clear of the edge.
BASES = {
    "windows": ("realtimex-icon.png", 0, 1024, 20),
    "macos": ("realtimex-icon-macos.png", 100, 924, 0),
    "linux": ("realtimex-icon-linux.png", 94, 930, 0),
}
# 452 px on the full-bleed 1024 (as before), 364 on the macOS body, 369 on the Linux one.
BADGE_SHARE = 452 / 1024
# At 32 px and below the badge is about 7 px at that share, and its chain can't be read: larger there.
SMALL_BADGE_SHARE = 0.55
SMALL = 32


def compose(platform, share, tmp):
    """RealTimeX's icon for the platform with Link's badge in the body's bottom-right corner, 1024 px."""
    name, start, end, margin = BASES[platform]
    base = Image.open(os.path.join(ASSETS, name)).convert("RGBA").resize((1024, 1024), Image.LANCZOS)
    size = round((end - start) * share)
    badge = render(os.path.join(ASSETS, "badge.svg"), size, os.path.join(tmp, f"badge-{size}.png"))
    at = end - margin - size
    base.alpha_composite(badge, (at, at))
    return base


def scaled(masters, size):
    """The icon at size px: from the small-size master at SMALL and below."""
    master = masters[1] if size <= SMALL else masters[0]
    return master.resize((size, size), Image.LANCZOS)


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


def write_rgba(image, size, name, resample=Image.LANCZOS, inner=None):
    os.makedirs(RGBA_DIR, exist_ok=True)
    # Premultiplied while scaling, so the transparent pixels' colour doesn't bleed into the edges.
    resized = image.convert("RGBa").resize((inner or size,) * 2, resample).convert("RGBA")
    if inner:
        # Drawn smaller, centred on the full canvas (transparent around it).
        canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
        offset = (size - inner) // 2
        canvas.paste(resized, (offset, offset))
        resized = canvas
    with open(os.path.join(RGBA_DIR, name), "wb") as handle:
        handle.write(resized.tobytes())


# Another instance's tray icons (the Link 0.2.1 UX contract, §3): the dot's centre, radius and gap
# in px of the icon, and its colour (black in the macOS template, violet elsewhere).
OTHER = {
    "tray-template-44": ((10.5, 10.5), 6, 2, (0, 0, 0, 255)),
    "tray-32": ((6, 6), 5, 1.5, (139, 92, 246, 255)),
}
# Drawn this many times larger, then scaled down, for smooth edges.
SUPERSAMPLE = 8


def with_dot(image, center, radius, gap, fill):
    """The icon with the dot: the icon cleared within radius + gap of the centre, then the dot."""
    size = image.width
    big = image.resize((size * SUPERSAMPLE,) * 2, Image.LANCZOS)
    x, y = center[0] * SUPERSAMPLE, center[1] * SUPERSAMPLE

    def disc(r):
        r *= SUPERSAMPLE
        return [x - r, y - r, x + r, y + r]

    hole = Image.new("L", big.size, 0)
    ImageDraw.Draw(hole).ellipse(disc(radius + gap), fill=255)
    big.putalpha(Image.composite(Image.new("L", big.size, 0), big.getchannel("A"), hole))
    ImageDraw.Draw(big).ellipse(disc(radius), fill=fill)
    return big.resize((size, size), Image.LANCZOS)


def write_other():
    for name, (center, radius, gap, fill) in OTHER.items():
        size = int(name.rsplit("-", 1)[1])
        with open(os.path.join(RGBA_DIR, f"{name}.rgba"), "rb") as handle:
            image = Image.frombytes("RGBA", (size, size), handle.read())
        with open(os.path.join(RGBA_DIR, f"{name}-other.rgba"), "wb") as handle:
            handle.write(with_dot(image, center, radius, gap, fill).tobytes())


def main():
    if sys.argv[1:] == ["other"]:
        write_other()
        print("other instance's tray icons written to crates/link/icons/")
        return
    with tempfile.TemporaryDirectory() as tmp:
        masters = {
            platform: (compose(platform, BADGE_SHARE, tmp), compose(platform, SMALL_BADGE_SHARE, tmp))
            for platform in BASES
        }
    windows, macos, linux = masters["windows"], masters["macos"], masters["linux"]

    for size in [16, 24, 32, 48, 64, 128, 256, 512, 1024]:
        scaled(linux, size).save(os.path.join(ASSETS, f"icon-{size}.png"))

    # Each size drawn from its own master (Pillow uses an appended image of a size as it is).
    ico_sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    ico = [scaled(windows, size) for size in ico_sizes]
    ico[-1].save(os.path.join(ASSETS, "icon.ico"), sizes=[(s, s) for s in ico_sizes], append_images=ico[:-1])

    if shutil.which("iconutil"):
        with tempfile.TemporaryDirectory() as tmp:
            iconset = os.path.join(tmp, "icon.iconset")
            os.makedirs(iconset)
            for size in (16, 32, 128, 256, 512):
                scaled(macos, size).save(os.path.join(iconset, f"icon_{size}x{size}.png"))
                scaled(macos, size * 2).save(os.path.join(iconset, f"icon_{size}x{size}@2x.png"))
            subprocess.run(["iconutil", "-c", "icns", iconset, "-o", os.path.join(ASSETS, "icon.icns")], check=True)
    else:
        print("iconutil not found (macOS only): assets/icon.icns not updated")

    write_rgba(windows[0], 256, "window-256.rgba")
    write_rgba(linux[0], 256, "window-linux-256.rgba")
    # The tray icons are scaled down by area (box): fine detail averages out instead of ringing.
    tray = without_background(windows[0])
    write_rgba(tray, 32, "tray-32.rgba", Image.BOX)
    # macOS: the menu bar's 22 pt at 2x, the icon drawn at 16 pt in it (32 px), as RealTimeX's
    # own tray icon is (frontend/assets/tray-icon.png shown at 16 pt), so the two side by side
    # are the same size and weight.
    write_rgba(template(tray), 44, "tray-template-44.rgba", Image.BOX, inner=32)
    write_other()
    print("icons written to assets/ and crates/link/icons/")


if __name__ == "__main__":
    main()
