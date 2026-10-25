"""Typography lint for the widgets: how evenly is the text placed inside every card, and how many type sizes does it use?

    set MSW_TEXT_LINT=1 && mswidgets --sheet out.png --scale 1 2> lint.log
    python tools/text-lint.py lint.log

The host logs one `TXT` line per text run (card size, box x y w h, natural size nw nh, font size, weight, alignment, alpha, text) and a `CARD`
line before each card. The text block of a card is the union of the natural text extents (a middle-aligned run sits in the middle of its box).
Prints, per card, the insets of that block from the card edge (16 px is the design padding) and the type sizes in use; flags what looks off:
an inset below 12 px, a block much closer to one side than to the opposite one, more than 4 sizes, text wider than its box.
"""
import sys

CARD_W = {"S": 170.0, "M": 356.0, "L": 356.0}
CARD_H = {"S": 170.0, "M": 170.0, "L": 356.0}


def main(path):
    cards, cur = [], None
    for line in open(path, encoding="utf-8", errors="replace"):
        f = line.rstrip("\n").split("\t")
        if f[0] == "CARD" and len(f) >= 3:
            cur = {"id": f"{f[1]}:{f[2].lower()}", "size": f[2], "runs": []}
            cards.append(cur)
        elif f[0] == "TXT" and cur is not None and len(f) >= 13:
            x, y, w, h, nw, nh = map(float, f[2:8])
            fs, wt, al = float(f[8]), int(f[9]), f[10]
            top = y + max(0.0, (h - nh) / 2.0)  # middle aligned in its box (the common case)
            left = x if al == "L" else (x + (w - nw) / 2.0 if al == "C" else x + w - nw)
            cur["runs"].append(dict(top=top, bottom=top + nh, left=left, right=left + nw, fs=fs, wt=wt, w=w, nw=nw, text=f[12]))
    seen = set()
    print(f"{'card':16} {'top':>5} {'bottom':>6} {'left':>5} {'right':>5}  sizes / flags")
    for c in cards:
        if c["id"] in seen or not c["runs"]:
            continue
        seen.add(c["id"])
        r = c["runs"]
        W, H = CARD_W[c["size"]], CARD_H[c["size"]]
        top, bottom = min(t["top"] for t in r), H - max(t["bottom"] for t in r)
        left, right = min(t["left"] for t in r), W - max(t["right"] for t in r)
        sizes = sorted({t["fs"] for t in r})
        flags = []
        if min(top, bottom, left, right) < 12:
            flags.append("inset<12")
        if abs(top - bottom) > 18 and c["size"] != "L":
            flags.append("vertical imbalance")
        if abs(left - right) > 20:
            flags.append("horizontal imbalance")
        if len(sizes) > 4:
            flags.append(f"{len(sizes)} sizes")
        if any(t["nw"] > t["w"] + 0.5 for t in r):
            flags.append("clipped")
        print(f"{c['id']:16} {top:5.0f} {bottom:6.0f} {left:5.0f} {right:5.0f}  {','.join(f'{s:g}' for s in sizes)}  {'; '.join(flags)}")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "lint.log")
