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
EOF
grep -h -E 'chunk-hash|chunk generation wait|stopped|FATAL' "$D"/client_*.log 2>/dev/null | tail -6
