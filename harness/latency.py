#!/usr/bin/env python3
"""Input latency from latency-probe logs (NOITA_ONLINE_LATPROBE=1, mod files/latprobe.lua).

Usage: harness/latency.py LAT_FILE...

For every press of the watched inputs (key D = walk right, left mouse = fire, number keys
1/2 = quick-bar item) it finds:
  P  the first frame whose input poll shows the press (OnWorldPreUpdate f),
  S  lockstep only: the frame whose captured input carries it, and the tick it was sent for,
  C  the first update whose controlled ControlsComponent shows it (mButtonDown*; not for
     number keys, which Noita's inventory GUI handles),
  R  the first update in which the character reacts (walks right: velocity x > 0.5; fires:
     the held wand's cast frame changes; number key: the active item changes).
Frames count world updates after the poll (vanilla: R = P, i.e. 0); ms = wall time between
the OnWorldPreUpdate calls of P and R (the frames in between, including any stalls).
"""
import re
import statistics
import sys

FIELD = re.compile(r"(\w+)=(-?[\d.]+)(?=\s|$)")


def load(path):
    rows = []
    for line in open(path):
        if not line.startswith("lat "):
            continue
        r = {k: float(v) for k, v in FIELD.findall(line)}
        for k in ("f", "tick", "pd", "pm", "pk", "sd", "sm", "cd", "cm", "cast", "item"):
            r[k] = int(r.get(k, 0 if k != "tick" else -1))
        rows.append(r)
    return rows


def first(rows, i0, pred, limit=120):
    f0 = rows[i0]["f"]
    for r in rows[i0:]:
        if r["f"] - f0 > limit:
            return None
        if pred(r):
            return r
    return None


KINDS = {
    # kind: (poll field, captured field, ControlsComponent field, reaction)
    "walk": ("pd", "sd", "cd", lambda r0: lambda x: x["vx"] > 0.5),
    "fire": ("pm", "sm", "cm", lambda r0: lambda x: x["cast"] != r0["cast"] and x["cast"] > 0),
    "item": ("pk", None, None, lambda r0: lambda x: x["item"] != r0["item"]),
}


def events(rows, kind):
    poll, cap, ctl, reaction = KINDS[kind]
    out = []
    prev_f, prev_on = -10, 0
    for i, r in enumerate(rows):
        on = r[poll]
        if on and not (prev_on and r["f"] == prev_f + 1):
            before = rows[i - 1] if i else r
            s = first(rows, i, lambda x: x[cap] == 1) if cap else None
            c = first(rows, i, lambda x: x[ctl] == 1) if ctl else None
            out.append(dict(P=r, S=s, C=c, R=first(rows, i, reaction(before))))
        prev_f, prev_on = r["f"], on
    return out


def dist(v):
    return "n/a" if not v else f"min {min(v)} median {statistics.median(v)} max {max(v)}"


def report(path):
    rows = load(path)
    print(f"== {path}: {len(rows)} lines")
    for kind in KINDS:
        ev = events(rows, kind)
        if not ev:
            continue
        cs, rs, ms, ss = [], [], [], []
        for e in ev:
            p = e["P"]
            if e["C"]:
                cs.append(e["C"]["f"] - p["f"])
            if e["R"]:
                rs.append(e["R"]["f"] - p["f"])
                ms.append((e["R"]["t"] - p["t"]) * 1000)
            if e["S"] and e["S"]["tick"] >= 0:
                ss.append((e["S"]["f"] - p["f"], e["S"]["tick"] - p["f"]))
        print(f"  {kind}: {len(ev)} presses")
        if ss:
            print(f"    captured at poll+{dist([a for a, _ in ss])}, sent for tick poll+{dist([b for _, b in ss])}")
        if cs:
            print(f"    ControlsComponent shows it: poll+{dist(cs)} updates")
        print(f"    character reacts:           poll+{dist(rs)} updates")
        if ms:
            print(f"    wall time poll->reaction frame: median {statistics.median(ms):.1f} ms (min {min(ms):.1f}, max {max(ms):.1f})")


if __name__ == "__main__":
    for p in sys.argv[1:]:
        report(p)
