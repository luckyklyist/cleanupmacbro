"""Draws the DMG window background (1x and 2x). Run: python3 assets/dmg/make_background.py (writes background.tiff)"""
import os
from PIL import Image, ImageDraw, ImageFont, ImageFilter

W, H = 660, 420
HERE = os.path.dirname(os.path.abspath(__file__))
FONT = "/System/Library/Fonts/SFNS.ttf"
if not os.path.exists(FONT):
    FONT = "/System/Library/Fonts/HelveticaNeue.ttc"


def draw(scale):
    w, h = W * scale, H * scale
    img = Image.new("RGB", (w, h))
    px = img.load()
    # Soft vertical gradient, dark like the app.
    top, bot = (30, 30, 36), (16, 16, 20)
    for y in range(h):
        t = y / (h - 1)
        c = tuple(int(top[i] + (bot[i] - top[i]) * t) for i in range(3))
        for x in range(w):
            px[x, y] = c
    # Blue glow behind the app icon.
    glow = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    g = ImageDraw.Draw(glow)
    cx, cy, r = 170 * scale, 200 * scale, 110 * scale
    g.ellipse((cx - r, cy - r, cx + r, cy + r), fill=(10, 132, 255, 60))
    glow = glow.filter(ImageFilter.GaussianBlur(40 * scale))
    img = Image.alpha_composite(img.convert("RGBA"), glow)

    d = ImageDraw.Draw(img)
    title = ImageFont.truetype(FONT, 26 * scale)
    sub = ImageFont.truetype(FONT, 14 * scale)
    d.text((w / 2, 52 * scale), "Install Clean You", font=title, fill=(255, 255, 255), anchor="mm")
    d.text((w / 2, 82 * scale), "Drag the app onto the Applications folder", font=sub, fill=(150, 150, 160), anchor="mm")

    # Dashed arrow from the app (x=170) to Applications (x=490).
    y = 200 * scale
    x0, x1 = 255 * scale, 395 * scale
    dash, gap, lw = 12 * scale, 8 * scale, 4 * scale
    x = x0
    while x < x1 - 18 * scale:
        d.line((x, y, min(x + dash, x1 - 18 * scale), y), fill=(10, 132, 255), width=lw)
        x += dash + gap
    d.polygon([(x1, y), (x1 - 22 * scale, y - 13 * scale), (x1 - 22 * scale, y + 13 * scale)], fill=(10, 132, 255))

    foot = ImageFont.truetype(FONT, 12 * scale)
    d.text((w / 2, 352 * scale), "Then open Clean You from Applications. First time: right-click → Open.",
           font=foot, fill=(120, 120, 130), anchor="mm")
    d.text((w / 2, 374 * scale), "Free and open source · github.com/luckyklyist/cleanupmacbro",
           font=foot, fill=(90, 90, 100), anchor="mm")
    return img.convert("RGB")


import subprocess, tempfile
tmp = tempfile.mkdtemp()
draw(1).save(os.path.join(tmp, "bg.png"))
draw(2).save(os.path.join(tmp, "bg@2x.png"))
subprocess.run(["tiffutil", "-cathidpicheck", os.path.join(tmp, "bg.png"), os.path.join(tmp, "bg@2x.png"),
                "-out", os.path.join(HERE, "background.tiff")], check=True)
print("wrote background.tiff")
