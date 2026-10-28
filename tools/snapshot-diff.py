"""Pixel diff of two directories of PNG renders (same file names): for a refactor that must not change a single pixel.

    python tools/snapshot-diff.py before_dir after_dir [--tol 0]

Per image prints the number of differing pixels and the largest channel delta; a missing file or a size change counts as a difference.
Exit 1 when any image differs by more than `--tol` (max channel delta, 0..255; default 0 = identical), else 0.
Uses numpy when installed, else plain Python (slow but identical numbers).
"""
import os
import sys

from PIL import Image

try:
    import numpy as np
except ImportError:
    np = None


def diff(a_path, b_path):
    """(differing pixels, max channel delta) or None when the sizes differ."""
    a, b = Image.open(a_path).convert("RGBA"), Image.open(b_path).convert("RGBA")
    if a.size != b.size:
        return None
    if np is not None:
        d = np.abs(np.asarray(a, dtype=np.int16) - np.asarray(b, dtype=np.int16))
        return int((d.max(axis=2) > 0).sum()), int(d.max())
    n = m = 0
    for p, q in zip(a.getdata(), b.getdata()):
        k = max(abs(x - y) for x, y in zip(p, q))
        if k:
            n += 1
            m = max(m, k)
    return n, m


def main(argv):
    tol = 0
    if "--tol" in argv:
        i = argv.index("--tol")
        tol = int(argv[i + 1])
        del argv[i : i + 2]
    if len(argv) != 2:
        print(__doc__)
        return 2
    da, db = argv
    names = sorted(set(os.listdir(da)) | set(os.listdir(db)))
    bad = 0
    for n in (n for n in names if n.lower().endswith(".png")):
        pa, pb = os.path.join(da, n), os.path.join(db, n)
        if not (os.path.exists(pa) and os.path.exists(pb)):
            print(f"{n}: MISSING in {'after' if os.path.exists(pa) else 'before'}")
            bad += 1
            continue
        r = diff(pa, pb)
        if r is None:
            print(f"{n}: SIZE CHANGED")
            bad += 1
            continue
        cnt, mx = r
        print(f"{n}: {cnt} px differ, max delta {mx}" + ("  <-- over tolerance" if mx > tol else ""))
        bad += mx > tol
    print(f"{len(names)} images, {bad} over tolerance {tol}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
