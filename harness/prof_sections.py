#!/usr/bin/env python3
"""Side-by-side section profile: prof_sections.py [-r FROM:TO] RUN_DIR...
Averages the windows of every client's sections_<id>.txt: by default windows 2.. (the first
window is skipped); with -r only the windows inside frames [FROM, TO) (window length from the
file, NOITA_ONLINE_PROF_WINDOW)."""
import argparse, glob, os, re

ap = argparse.ArgumentParser()
ap.add_argument("-r", help="frame range FROM:TO")
ap.add_argument("runs", nargs="+")
args = ap.parse_args()
rng = tuple(int(x) for x in args.r.split(":")) if args.r else None

def load(d):
    tot, n = {}, 0
    order = []
    for f in sorted(glob.glob(os.path.join(d, "sections_*.txt"))):
        win, end, use = -1, 0, False
        for line in open(f):
            m = re.match(r"(\d+) frames: wall ([\d.]+) ms/frame, tick wait ([\d.]+), entities .*? ([\d.]+)$", line)
            if m:
                win += 1
                start, end = end, end + int(m[1])
                use = (rng[0] <= start and end <= rng[1]) if rng else win >= 1
                if use:
                    n += 1
                    for k, v in (("WALL", m[2]), ("tick_wait", m[3])):
                        tot[k] = tot.get(k, 0) + float(v)
                        if k not in order: order.append(k)
                continue
            m = re.match(r"(\s*)(\S.*?)\s+([\d.]+) ms/frame", line)
            if m and use:
                k = m[1] + m[2]
                tot[k] = tot.get(k, 0) + float(m[3])
                if k not in order: order.append(k)
    return {k: v / n for k, v in tot.items()} if n else {}, order

runs = [(os.path.basename(d.rstrip("/")), *load(d)) for d in args.runs]
order = []
for _, _, o in runs:
    for k in o:
        if k not in order: order.append(k)
print("%-46s" % "section (ms/frame)" + "".join("%14s" % r[0][:13] for r in runs))
for k in order:
    print("%-46s" % k[:46] + "".join("%14.2f" % r[1].get(k, float("nan")) for r in runs))
