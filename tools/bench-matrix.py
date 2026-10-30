"""Runs the redraw bench several times per variant (interleaved) and prints the best of each metric.

    python tools/bench-matrix.py REPEATS NAME=ENV ...     e.g. off=MSWIDGETS_NO_FP16=1 full= fx=MSWIDGETS_FP16=fx
Needs the test binary of `cargo test --release bench_card_redraw --no-run` (path printed by cargo) in BENCH_EXE and MSW_BENCH_BD.
"""
import os
import re
import subprocess
import sys

exe = os.environ["BENCH_EXE"]
reps = int(sys.argv[1])
variants = []
for a in sys.argv[2:]:
    name, _, kv = a.partition("=")
    env = {}
    if kv:
        k, _, v = kv.partition("=")
        env[k] = v
    variants.append((name, env))
best = {n: {} for n, _ in variants}
for r in range(reps):
    for name, env in variants:
        e = dict(os.environ)
        e.update(env)
        out = subprocess.run([exe, "bench_card_redraw", "--ignored", "--nocapture"], env=e, capture_output=True, text=True).stdout
        for line in out.splitlines():
            m = re.match(r"(.{40})\s+cache ON\s+best\s+([\d.]+) ms .*cache OFF best\s+([\d.]+) ms", line)
            if m:
                lab = m.group(1).strip()
                on, off = float(m.group(2)), float(m.group(3))
                b = best[name].setdefault(lab, [1e9, 1e9])
                b[0], b[1] = min(b[0], on), min(b[1], off)
labs = list(best[variants[0][0]].keys())
print(f"{'':42}" + "".join(f"{n:>18}" for n, _ in variants) + "   (cache ON / cache OFF, ms, best of %d)" % reps)
for lab in labs:
    print(f"{lab:42}" + "".join(f"{best[n][lab][0]:9.3f}/{best[n][lab][1]:7.3f}" for n, _ in variants))
