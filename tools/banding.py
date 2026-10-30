"""Quantify 8-bit banding of the glass gradients in a `--dump` render
    python tools/banding.py img.png [--scale 1] [--cell 0,0] [--size s] [--json]

The card sits at the desk grid (origin 20,50 logical, cell 170 px, gap 16). Over a dark backdrop the soft parts of the glass (specular rim, inner glow,
sheen, tint, shadow) are gradients of a few levels. An 8-bit render turns them into plateaus of ONE value with 1-level steps between them (contours);
a dithered render shows the same average ramp as fine noise. Only SLOW gradient pixels are judged (local slope below 0.6 level/px after a 3 px blur,
all of the 9x9 neighbourhood slow, not a perfectly flat patch): text, rims and icons are excluded. Per channel, pooled over R, G, B, it reports
  flat   share of those pixels whose 4 neighbours all hold the same value (the inside of a plateau; banded ~0.9, dithered ~0.4)  <- headline
  run    mean length (px) of a horizontal run of equal values
  steps  1-level steps along rows per 100 px
  cerr   RMS of the low-frequency part of img - gauss(img, 5) (informational: it also contains the true curvature of the gradient)
Regions: "card" (the card rectangle) and "shadow" (the 45 px ring around it, backdrop included).
"""
import json
import sys

import numpy as np
from PIL import Image

SIZES = {"s": (1, 1), "m": (2, 1), "l": (2, 2)}
SLOW = 0.6  # level / px: steeper pixels are content (text, rims, icons), not a gradient that can band


def measure(img, card, ring):
    """img: HxWx3 uint8; card / ring: (x0, y0, x1, y1) px. Returns {region: {cerr, steps, flat, run, n}} pooled over the channels."""
    from scipy.ndimage import gaussian_filter

    out = {}
    h, w = img.shape[:2]
    cx0, cy0, cx1, cy1 = card
    rx0, ry0, rx1, ry1 = [max(0, ring[0]), max(0, ring[1]), min(w, ring[2]), min(h, ring[3])]
    yy, xx = np.mgrid[ry0:ry1, rx0:rx1]
    inside = (xx >= cx0) & (xx < cx1) & (yy >= cy0) & (yy < cy1)
    acc = {"card": [0.0, 0, 0, 0, 0, 0], "shadow": [0.0, 0, 0, 0, 0, 0]}  # e2, n, steps, flat, run sum, run count
    for c in range(3):
        a = img[ry0:ry1, rx0:rx1, c].astype(np.float64)
        lo = gaussian_filter(a, 5.0)
        e = gaussian_filter(a - lo, 2.0)
        sm = gaussian_filter(a, 3.0)
        gy, gx = np.gradient(sm)
        slow = np.hypot(gx, gy) < SLOW
        # keep away from steep neighbourhoods: a pixel counts only when the 9x9 box around it is slow too
        from scipy.ndimage import maximum_filter, minimum_filter

        slow = minimum_filter(slow.astype(np.uint8), 9).astype(bool) & (maximum_filter(a, 9) != minimum_filter(a, 9))  # not a flat patch
        core = np.zeros_like(slow)
        core[1:-1, 1:-1] = (a[1:-1, 1:-1] == a[:-2, 1:-1]) & (a[1:-1, 1:-1] == a[2:, 1:-1]) & (a[1:-1, 1:-1] == a[1:-1, :-2]) & (a[1:-1, 1:-1] == a[1:-1, 2:])
        step = np.zeros_like(slow)
        step[:, 1:] = np.abs(np.diff(a, axis=1)) == 1
        rl = np.zeros(a.shape)  # length of the horizontal run each pixel belongs to
        for y in range(a.shape[0]):
            row = a[y]
            b = np.flatnonzero(np.diff(row) != 0) + 1
            edges = np.concatenate(([0], b, [len(row)]))
            for s0, s1 in zip(edges[:-1], edges[1:]):
                rl[y, s0:s1] = s1 - s0
        for name, m in (("card", inside), ("shadow", ~inside)):
            k = m & slow
            r = acc[name]
            r[0] += float((e[k] ** 2).sum())
            r[1] += int(k.sum())
            r[2] += int(step[k].sum())
            r[3] += int(core[k].sum())
            r[4] += float((1.0 / rl[k]).sum())  # sum of 1/run length over pixels = number of runs touched
    for name, r in acc.items():
        n = max(r[1], 1)
        out[name] = {"cerr": (r[0] / n) ** 0.5, "steps": 100.0 * r[2] / n, "flat": r[3] / n, "run": n / max(r[4], 1e-9), "n": r[1] // 3}
    return out


def analyse(path, scale=1.0, cell=(0, 0), size="s"):
    img = np.asarray(Image.open(path).convert("RGB"))
    cw, ch = SIZES[size]
    w, h = 170 * cw + 16 * (cw - 1), 170 * ch + 16 * (ch - 1)
    ox, oy = 20 + 186 * cell[0], 50 + 186 * cell[1]
    f = lambda v: int(round(v * scale))
    card = (f(ox), f(oy), f(ox + w), f(oy + h))
    m = f(45)
    return measure(img, card, (card[0] - m, card[1] - m, card[2] + m, card[3] + m))


def main():
    argv = sys.argv[1:]
    if not argv:
        print(__doc__)
        return 2
    path, scale, cell, size = argv[0], 1.0, (0, 0), "s"
    for i, v in enumerate(argv):
        if v == "--scale":
            scale = float(argv[i + 1])
        if v == "--cell":
            cell = tuple(int(t) for t in argv[i + 1].split(","))
        if v == "--size":
            size = argv[i + 1]
    r = analyse(path, scale, cell, size)
    for k, m in r.items():
        print(f"{k:7s} flat {m['flat']:.3f}  run {m['run']:6.2f}  steps/100px {m['steps']:5.2f}  cerr {m['cerr']:.3f}  slow px {m['n']}")
    c, sh = r["card"], r["shadow"]
    print(f"SUMMARY flat card {c['flat']:.3f} shadow {sh['flat']:.3f} | run card {c['run']:.2f} shadow {sh['run']:.2f}")
    if "--json" in argv:
        print(json.dumps(r))
    return 0


if __name__ == "__main__":
    sys.exit(main())
