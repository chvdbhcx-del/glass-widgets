"""Banding probe scene: 4 dark cards (disks, controls M, sysmon, network) over flat dark backdrops at scale 1 and 2, headless.

    python tools/banding-run.py EXE OUTDIR [NAME=VALUE ...]     (env overrides, e.g. MSWIDGETS_NO_FP16=1)

Needs out/bd/black.png and out/bd/dark.png (python tools/banding-bd.py). Prints, per backdrop and scale, tools/banding.py's flat share and mean run
length averaged over the four cards (card area / shadow ring); lower = smoother.
"""
import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import banding  # noqa: E402

CELLS = [((0, 0), "s"), ((1, 0), "m"), ((0, 1), "s"), ((1, 1), "s")]


def main():
    exe, out = sys.argv[1], sys.argv[2]
    env = dict(os.environ)
    for kv in sys.argv[3:]:
        k, v = kv.split("=", 1)
        env[k] = v
    os.makedirs(out, exist_ok=True)
    tot = {}
    for bd in ("black", "dark"):
        for sc in (1, 2):
            f = f"{out}/d_{bd}_{sc}.png"
            subprocess.run([exe, "--dump", f, "--backdrop", f"out/bd/{bd}.png", "--cards", "disks:s@0,0;controls:m@1,0;sysmon:s@0,1;network:s@1,1",
                            "--scale", str(sc), "--appearance", "dark", "--time", "2026-10-24 10:08:35"], env=env, check=True, capture_output=True)
            rs = [banding.analyse(f, sc, c, s) for c, s in CELLS]
            row = {}
            for reg in ("card", "shadow"):
                for k in ("flat", "run"):
                    row[f"{reg} {k}"] = sum(r[reg][k] for r in rs) / len(rs)
            print(f"{bd:5s} x{sc}: card flat {row['card flat']:.3f} run {row['card run']:6.2f} | shadow flat {row['shadow flat']:.3f} run {row['shadow run']:6.2f}")
            for k, v in row.items():
                tot.setdefault(k, []).append(v)
    print("MEAN : card flat {:.3f} run {:6.2f} | shadow flat {:.3f} run {:6.2f}".format(*[sum(tot[k]) / len(tot[k]) for k in ("card flat", "card run", "shadow flat", "shadow run")]))


if __name__ == "__main__":
    main()
