"""Draws the installer artwork (needs Pillow): installer/icon.ico, installer/sidebar.bmp (164x314), installer/header.bmp (150x57).

    python tools/installer_art.py

Everything is drawn from scratch with code, no third-party images. The output files are committed; run this only to change the look.
"""
import os
from PIL import Image, ImageDraw, ImageFilter, ImageFont

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "installer")
FONTS = os.path.join(ROOT, "assets", "fonts")
os.makedirs(OUT, exist_ok=True)


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(len(a)))


def gradient(w, h, stops):
    """Diagonal (top-left to bottom-right) multi-stop gradient."""
    g = Image.new("RGB", (w, h))
    px = g.load()
    for y in range(h):
        for x in range(w):
            t = x / max(w - 1, 1) * 0.35 + y / max(h - 1, 1) * 0.65
            for i in range(len(stops) - 1):
                (t0, c0), (t1, c1) = stops[i], stops[i + 1]
                if t <= t1 or i == len(stops) - 2:
                    px[x, y] = lerp(c0, c1, min(max((t - t0) / (t1 - t0), 0), 1))
                    break
    return g


def blob(img, cx, cy, r, color, alpha):
    layer = Image.new("RGBA", img.size, (0, 0, 0, 0))
    ImageDraw.Draw(layer).ellipse((cx - r, cy - r, cx + r, cy + r), fill=color + (alpha,))
    img.alpha_composite(layer.filter(ImageFilter.GaussianBlur(r * 0.55)))


def glass_card(img, box, radius, alpha=46, rim=110):
    """A frosted card: translucent white fill, brighter upper half, thin rim."""
    x0, y0, x1, y1 = box
    layer = Image.new("RGBA", img.size, (0, 0, 0, 0))
    ImageDraw.Draw(layer).rounded_rectangle(box, radius, fill=(255, 255, 255, alpha), outline=(255, 255, 255, rim), width=max(1, img.size[0] // 600))
    hi = Image.new("RGBA", img.size, (0, 0, 0, 0))
    ImageDraw.Draw(hi).rounded_rectangle((x0, y0, x1, y0 + (y1 - y0) * 0.5), radius, fill=(255, 255, 255, 38))
    mask = Image.new("L", img.size, 0)
    ImageDraw.Draw(mask).rounded_rectangle(box, radius, fill=255)
    hi.putalpha(Image.composite(hi.getchannel("A"), Image.new("L", img.size, 0), mask))
    img.alpha_composite(layer)
    img.alpha_composite(hi)


def tile_icon(size=1024):
    """App icon: a rounded tile with a blue-violet gradient and a 2x2 grid of glass cards."""
    img = gradient(size, size, [(0, (122, 170, 255)), (0.55, (118, 92, 235)), (1, (86, 40, 190))]).convert("RGBA")
    blob(img, size * 0.25, size * 0.2, size * 0.32, (255, 255, 255), 90)
    blob(img, size * 0.85, size * 0.95, size * 0.35, (255, 90, 190), 110)
    pad, gap = size * 0.17, size * 0.055
    cw = (size - 2 * pad - gap) / 2
    for r in range(2):
        for c in range(2):
            x0 = pad + c * (cw + gap)
            y0 = pad + r * (cw + gap)
            glass_card(img, (x0, y0, x0 + cw, y0 + cw), size * 0.075, alpha=96 if (r, c) == (0, 1) else 64, rim=150)
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, size - 1, size - 1), size * 0.225, fill=255)
    out = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    out.paste(img, (0, 0), mask)
    return out


def font(name, px):
    for f in (os.path.join(FONTS, name), r"C:\Windows\Fonts\segoeui.ttf"):
        try:
            return ImageFont.truetype(f, px)
        except OSError:
            pass
    return ImageFont.load_default()


def sidebar():
    s = 3  # supersample
    w, h = 164 * s, 314 * s
    img = gradient(w, h, [(0, (22, 26, 70)), (0.5, (70, 52, 160)), (1, (36, 92, 200))]).convert("RGBA")
    blob(img, w * 0.15, h * 0.1, w * 0.7, (120, 160, 255), 110)
    blob(img, w * 0.95, h * 0.62, w * 0.6, (255, 90, 190), 120)
    blob(img, w * 0.1, h * 0.95, w * 0.6, (70, 210, 190), 90)
    # a loose stack of mini widgets, like the desktop the product draws
    glass_card(img, (14 * s, 120 * s, 150 * s, 166 * s), 12 * s, 52, 120)
    glass_card(img, (14 * s, 174 * s, 78 * s, 238 * s), 12 * s, 52, 120)
    glass_card(img, (86 * s, 174 * s, 150 * s, 238 * s), 12 * s, 52, 120)
    d = ImageDraw.Draw(img)
    d.text((26 * s, 124 * s), "24", font=font("Nunito-Light.ttf", 26 * s), fill=(255, 255, 255, 240))
    d.text((70 * s, 138 * s), "OCT", font=font("Nunito-Bold.ttf", 9 * s), fill=(190, 215, 255, 230))
    d.arc((26 * s, 186 * s, 66 * s, 226 * s), 0, 360, fill=(255, 255, 255, 70), width=3 * s)
    d.arc((26 * s, 186 * s, 66 * s, 226 * s), -90, 150, fill=(80, 230, 140, 255), width=3 * s)
    d.text((100 * s, 190 * s), "7°", font=font("Nunito-Light.ttf", 24 * s), fill=(255, 255, 255, 240))
    d.text((101 * s, 220 * s), "sunny", font=font("Nunito-Medium.ttf", 9 * s), fill=(210, 225, 255, 220))
    img.alpha_composite(tile_icon(512).resize((64 * s, 64 * s), Image.LANCZOS), (14 * s, 22 * s))
    d.text((14 * s, 254 * s), "Glass", font=font("Nunito-Bold.ttf", 22 * s), fill=(255, 255, 255, 255))
    d.text((14 * s, 254 * s + 25 * s), "Widgets", font=font("Nunito-Light.ttf", 22 * s), fill=(255, 255, 255, 235))
    return img.convert("RGB").resize((164, 314), Image.LANCZOS)


def header():
    s = 4
    img = Image.new("RGBA", (150 * s, 57 * s), (255, 255, 255, 255))
    img.alpha_composite(tile_icon(512).resize((40 * s, 40 * s), Image.LANCZOS), (100 * s, 8 * s))
    return img.convert("RGB").resize((150, 57), Image.LANCZOS)


if __name__ == "__main__":
    icon = tile_icon(1024)
    icon.save(os.path.join(OUT, "icon.ico"), sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
    sidebar().save(os.path.join(OUT, "sidebar.bmp"))
    header().save(os.path.join(OUT, "header.bmp"))
    print("wrote", sorted(os.listdir(OUT)))
