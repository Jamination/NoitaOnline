#!/usr/bin/env python3
"""Side-by-side section profile: prof_sections.py RUN_DIR... (averages windows 2.. of every client)."""
import glob, os, re, sys

def load(d):
    tot, n = {}, 0
    order = []
    for f in sorted(glob.glob(os.path.join(d, "sections_*.txt"))):
        win = -1
        for line in open(f):
            m = re.match(r"(\d+) frames: wall ([\d.]+) ms/frame, tick wait ([\d.]+), entities .*? ([\d.]+)$", line)
            if m:
                win += 1
                if win >= 1:
                    n += 1
                    for k, v in (("WALL", m[2]), ("tick_wait", m[3])):
                        tot[k] = tot.get(k, 0) + float(v)
                        if k not in order: order.append(k)
                continue
            m = re.match(r"(\s*)(\S.*?)\s+([\d.]+) ms/frame", line)
            if m and win >= 1:
                k = m[1] + m[2]
                tot[k] = tot.get(k, 0) + float(m[3])
                if k not in order: order.append(k)
    return {k: v / n for k, v in tot.items()} if n else {}, order

runs = [(os.path.basename(d.rstrip("/")), *load(d)) for d in sys.argv[1:]]
order = []
for _, _, o in runs:
    for k in o:
        if k not in order: order.append(k)
print("%-46s" % "section (ms/frame)" + "".join("%14s" % r[0][:13] for r in runs))
for k in order:
    print("%-46s" % k[:46] + "".join("%14.2f" % r[1].get(k, float("nan")) for r in runs))
for name, t, _ in runs:
    if t:
        g = lambda name: next((v for k, v in t.items() if k.strip().startswith(name)), 0)
        acc = g("dm_update") + g("dm_draw") + g("swap_window")
