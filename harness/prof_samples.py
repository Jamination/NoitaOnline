#!/usr/bin/env python3
"""Main-thread sample profile: prof_samples.py RUN_DIR [top]  (windows 2.. of every client).
Exclusive = EIP, inclusive = any return address on the EBP chain; ms/frame scaled by the
wall time per frame of the sections file."""
import collections, glob, json, os, re, sys, urllib.request

CACHE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "run", "ghidra_funcs.json")
cache = json.load(open(CACHE)) if os.path.exists(CACHE) else {}

def func(a):
    if a < 0x401000 or a >= 0x0f00000:
        return "ext@%04x" % (a >> 16)
    k = "%08x" % a
    if k not in cache:
        try:
            r = urllib.request.urlopen("http://127.0.0.1:18489/func?addr=0x%s" % k, timeout=10).read().decode().strip()
        except Exception:
            r = ""
        cache[k] = r.split()[0] if r else "?%s" % k
    return cache[k]

def wall_ms(d):
    w = []
    for f in glob.glob(os.path.join(d, "sections_*.txt")):
        v = [float(m[1]) for m in re.finditer(r"frames: wall ([\d.]+)", open(f).read())]
        w += v[1:]
    return sum(w) / len(w) if w else 0

d = sys.argv[1]
top = int(sys.argv[2]) if len(sys.argv) > 2 else 40
ex, inc = collections.Counter(), collections.Counter()
total = 0
for f in glob.glob(os.path.join(d, "samples_*.txt")):
    win = -1
    for line in open(f):
        p = line.split()
        if p[0] == "W":
            win += 1
            if win >= 1:
                total += int(p[2])
            continue
        if win < 1 or p[0] == "D":
            continue
        a, c = int(p[1], 16), int(p[2])
        (ex if p[0] == "E" else inc)[a] += c
fx, fi = collections.Counter(), collections.Counter()
for a, c in ex.items():
    fx[func(a)] += c
for a, c in inc.items():
    fi[func(a)] += c
json.dump(cache, open(CACHE, "w"))
wm = wall_ms(d)
print("%s: %d samples, wall %.2f ms/frame" % (d, total, wm))
print("-- exclusive")
for k, c in fx.most_common(top):
    print("  %-22s %5.1f%% %6.2f ms/frame" % (k, 100 * c / total, wm * c / total))
print("-- inclusive (EBP chain)")
for k, c in fi.most_common(top):
    print("  %-22s %5.1f%% %6.2f ms/frame" % (k, 100 * c / total, wm * c / total))
