#!/usr/bin/env bash
# Summarise an e2e run directory: first mismatch, verification totals, per-client FPS,
# chunk-hash cost and chunk-generation waits.
D="${1:?run dir}"
echo "status: $(cat "$D/done" 2>/dev/null || echo running)"
python3 - "$D" <<'EOF'
import json, os, sys, glob
d = sys.argv[1]
bad = None
n = 0
for l in open(os.path.join(d, "hashes.jsonl")) if os.path.exists(os.path.join(d, "hashes.jsonl")) else []:
    n += 1
    j = json.loads(l)
    if bad is None and (j["chunk_mismatches"] or j["position_mismatches"] or not j["entity_hash_equal"]):
        bad = j
print("checkpoints:", n, "first bad:", bad)
s = os.path.join(d, "summary.json")
if os.path.exists(s):
    js = json.load(open(s))
    print("verification:", js["verification"])
    for c in js["clients"]:
        print("client", c["player"], "out %.1f in %.1f KB/s" % (c["avg_out_Bps"] / 1000, c["avg_in_Bps"] / 1000))
for f in sorted(glob.glob(os.path.join(d, "frames_*.txt"))):
    v = [int(x) for x in open(f) if x.strip()][600:]
    if not v:
        continue
    s2 = sorted(v)
    print(os.path.basename(f), "avg_fps=%.1f low1=%.1f max_ms=%.0f frames=%d" % (1e6 * len(v) / sum(v), 1e6 / s2[int(len(s2) * .99)], s2[-1] / 1000, len(v)))
# Lockstep tick waits per client (perf.jsonl windows after the first 5, i.e. after loading):
# time blocked per frame, frames that waited over 1 ms, and the shortest arrival-to-use
# margin of the others.
p = os.path.join(d, "perf.jsonl")
per = {}
for l in open(p) if os.path.exists(p) else []:
    j = json.loads(l)
    if "stalls" in j:
        per.setdefault(j["player"], []).append(j)
for pl, w in sorted(per.items()):
    w = w[5:]
    fr = sum(x["frames"] for x in w)
    if not fr:
        continue
    m = [x["margin_min_us"] for x in w if x["margin_min_us"] >= 0]
    print("ticks player %d: wait %.0f us/frame, stalls %d/%d frames, min margin %s ms" % (
        pl, sum(x["wait_us"] for x in w) / fr, sum(x["stalls"] for x in w), fr, "%.1f" % (min(m) / 1000) if m else "n/a"))
ev = os.path.join(d, "events.log")
for l in open(ev) if os.path.exists(ev) else []:
    if "input delay" in l or "started:" in l:
        print(l.strip()[:160])
EOF
grep -h -E 'chunk-hash|chunk generation wait|stopped|FATAL' "$D"/client_*.log 2>/dev/null | tail -6
