#!/usr/bin/env python3
"""Evaluate Noita Online acceptance criteria from e2e run artifacts.

Usage:
  harness/evaluate.py --run run/e2e/full --netsim-run run/e2e/netsim \
                      --fps-run run/e2e/fps_mp1_{1,2,3} --baseline run/e2e/fps_solo1_{1,2,3} \
                      [--info-baseline run/e2e/solo4] [--frames 36000] [--players 4] [--skip-build] [--skip-tests]
  harness/evaluate.py --fps-only --fps-run ... --baseline ...    # the FPS criterion alone

FPS criterion (agreed with the user): every lockstep client simulates every player, so the
4-player workload is inherently larger than single-player. The criterion therefore measures
the mod's own overhead at equal workload: 1-player sessions (patches, networking, lockstep)
vs vanilla single-player baseline runs, same scenario, seed and CPU pinning, run alternately
(harness/fps_pairs.sh). Vanilla is not deterministic, so its runs follow different
trajectories; each side is the median over its runs. The 4-player session's FPS against
--info-baseline is reported for information only.

Exit status 0 only if every criterion passes. Prints one PASS/FAIL line per criterion.
"""
import argparse
import json
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
results = []


def record(name, ok, detail):
    results.append((name, ok))
    print(f"{'PASS' if ok else 'FAIL'}  {name}: {detail}")


def load_summary(run):
    with open(os.path.join(run, "summary.json")) as f:
        return json.load(f)


def frame_stats(path, skip):
    """avg FPS and 1%-low FPS (1 / 99th-percentile frame time) after `skip` warm-up frames."""
    with open(path) as f:
        v = [int(x) for x in f if x.strip()]
    v = v[skip:]
    if not v:
        return None
    s = sorted(v)
    avg_us = sum(v) / len(v)
    p99 = s[min(len(s) - 1, (len(s) * 99 + 99) // 100 - 1)]
    return {"frames": len(v), "avg_fps": 1e6 / avg_us, "low1_fps": 1e6 / p99}


def events(run):
    p = os.path.join(run, "events.log")
    return open(p).read() if os.path.exists(p) else ""


def check_session(tag, run, frames, players):
    s = load_summary(run)
    ev = events(run)
    sess = s.get("session", {})
    clients = s.get("clients", [])
    done = [c for c in clients if c.get("perf_frames", 0) >= frames - 120]
    timeouts = len(re.findall(r"disconnected: TimedOut", ev))
    kicked = len(re.findall(r"kicking|Kicked", ev))
    finished = all(
        os.path.exists(os.path.join(run, f"client_{i}.log")) and
        "finishing: duration reached" in open(os.path.join(run, f"client_{i}.log")).read()
        for i in client_ids(run)
    )
    ok = (sess.get("frame", 0) >= frames and len(clients) >= players and len(done) >= players
          and timeouts == 0 and kicked == 0 and finished)
    record(f"{tag}: {players} clients complete {frames}-frame scripted session without disconnects", ok,
           f"server frame={sess.get('frame')} clients={len(clients)} complete={len(done)} "
           f"timeouts={timeouts} kicks={kicked} all_clients_reached_end={finished}")
    v = s["verification"]
    expected = frames // 60
    ok = v["chunk_mismatches"] == 0 and v["chunk_comparisons"] > 0 and v["checkpoints"] >= expected - 1
    record(f"{tag}: loaded-chunk hashes identical every 60 frames", ok,
           f"checkpoints={v['checkpoints']} (expected ~{expected}) chunk_comparisons={v['chunk_comparisons']} "
           f"mismatches={v['chunk_mismatches']} first_mismatch_frame={v['first_mismatch_frame']}")
    ok = v["position_mismatches"] == 0 and v["position_comparisons"] > 0
    record(f"{tag}: player+enemy positions identical (0 px)", ok,
           f"position_comparisons={v['position_comparisons']} mismatches={v['position_mismatches']}")
    return s


def client_ids(run):
    ids = []
    for f in os.listdir(run):
        m = re.match(r"client_(\d+)\.log$", f)
        if m:
            ids.append(int(m.group(1)))
    return sorted(ids)


def median(v):
    s = sorted(v)
    return (s[(len(s) - 1) // 2] + s[len(s) // 2]) / 2


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--run")
    ap.add_argument("--baseline", required=True, nargs="+",
                    help="vanilla solo runs (--solo --players 1 --pin), interleaved with --fps-run")
    ap.add_argument("--fps-run", required=True, nargs="+",
                    help="1-player MP runs (--players 1 --pin), same seed, interleaved with --baseline")
    ap.add_argument("--info-baseline", help="solo run with as many instances as --run (informational)")
    ap.add_argument("--netsim-run")
    ap.add_argument("--frames", type=int, default=36000)
    ap.add_argument("--players", type=int, default=4)
    ap.add_argument("--warmup", type=int, default=600)
    ap.add_argument("--skip-build", action="store_true")
    ap.add_argument("--skip-tests", action="store_true")
    ap.add_argument("--fps-only", action="store_true", help="only the FPS overhead criterion")
    a = ap.parse_args()
    if a.fps_only:
        a.skip_build = a.skip_tests = True
    elif not a.run or not a.netsim_run:
        ap.error("--run and --netsim-run are required (unless --fps-only)")

    # 1. Builds.
    if not a.skip_build:
        r = subprocess.run([os.path.join(ROOT, "harness", "build.sh")], capture_output=True, text=True)
        files = ["target/release/noita-online-server", "target/x86_64-pc-windows-gnu/release/noita-online-server.exe",
                 "target/i686-pc-windows-gnu/release/noita_online.dll"]
        missing = [f for f in files if not os.path.exists(os.path.join(ROOT, f))]
        record("Linux server build + Windows cross-builds (server .exe, client .dll)", r.returncode == 0 and not missing,
               f"build.sh exit={r.returncode} missing={missing}")

    # 8. Unit tests.
    if not a.skip_tests:
        r = subprocess.run(["cargo", "test", "--release", "--workspace"], cwd=ROOT, capture_output=True, text=True)
        passed = sum(int(x) for x in re.findall(r"test result: ok\. (\d+) passed", r.stdout))
        failed = sum(int(x) for x in re.findall(r"(\d+) failed", r.stdout))
        have = {n: (n in r.stdout) for n in ["reliable_messages_arrive_once_in_order", "large_messages_fragment",
                                               "delivery_resumes_after_total_outage", "sequence_numbers_wrap"]}
        record("unit tests (incl. reliable-UDP ordering/retransmission/fragmentation) pass",
               r.returncode == 0 and failed == 0 and all(have.values()),
               f"cargo test exit={r.returncode} passed={passed} failed={failed} key_tests_present={have}")

    # 2-4. Main session.
    s = None if a.fps_only else check_session("session", a.run, a.frames, a.players)

    # 5. FPS: mod overhead at equal workload (1-player session vs vanilla solo).
    def fps_compare(run, base, n):
        run_ids, base_ids = client_ids(run), client_ids(base)
        ok = len(run_ids) >= n and len(base_ids) >= n
        lines = []
        for ri, bi in zip(run_ids, base_ids):
            m = frame_stats(os.path.join(run, f"frames_{ri}.txt"), a.warmup)
            b = frame_stats(os.path.join(base, f"frames_{bi}.txt"), a.warmup)
            if not m or not b:
                ok = False
                continue
            da = 1 - m["avg_fps"] / b["avg_fps"]
            dl = 1 - m["low1_fps"] / b["low1_fps"]
            lines.append(f"client{ri}: avg {m['avg_fps']:.1f} vs {b['avg_fps']:.1f} ({da*100:+.1f}% drop), "
                         f"1%low {m['low1_fps']:.1f} vs {b['low1_fps']:.1f} ({dl*100:+.1f}% drop)")
            if da > 0.05 or dl > 0.05:
                ok = False
        return ok, "; ".join(lines)

    # Vanilla's simulation is not deterministic (thread timing, wall-clock budgets), so
    # identical solo runs drift onto different trajectories with different workloads, while
    # the lockstep session replays one trajectory. Each side is therefore summarised by the
    # median over its runs (interleaved MP/solo on the same cores, see docs/testing.md).
    def side(runs):
        st = []
        for r in runs:
            ids = client_ids(r)
            x = frame_stats(os.path.join(r, f"frames_{ids[0]}.txt"), a.warmup) if ids else None
            if x:
                st.append(x)
        return st

    mp, solo = side(a.fps_run), side(a.baseline)
    if len(mp) == len(a.fps_run) and len(solo) == len(a.baseline):
        m_avg, m_low = median([x["avg_fps"] for x in mp]), median([x["low1_fps"] for x in mp])
        b_avg, b_low = median([x["avg_fps"] for x in solo]), median([x["low1_fps"] for x in solo])
        da, dl = 1 - m_avg / b_avg, 1 - m_low / b_low
        ok = da <= 0.05 and dl <= 0.05
        runs = lambda st: " ".join(f"{x['avg_fps']:.1f}/{x['low1_fps']:.1f}" for x in st)
        detail = (f"median of {len(mp)} MP / {len(solo)} solo runs: avg {m_avg:.1f} vs {b_avg:.1f} ({da*100:+.1f}% drop), "
                  f"1%low {m_low:.1f} vs {b_low:.1f} ({dl*100:+.1f}% drop); runs avg/1%low MP [{runs(mp)}] solo [{runs(solo)}]")
    else:
        ok, detail = False, "missing frames_<id>.txt in an --fps-run or --baseline run"
    record("FPS overhead <= 5% vs single-player baseline at equal workload (avg and 1% low)", ok, detail)

    if not a.fps_only:
        if a.info_baseline:
            _, detail = fps_compare(a.run, a.info_baseline, a.players)
            print(f"INFO  {a.players}-player session vs {a.players} solo instances: {detail}")

        # 6. Bandwidth and server CPU.
        bw = [(c["player"], c["avg_out_Bps"], c["avg_in_Bps"], c["peak_out_Bps"], c["peak_in_Bps"]) for c in s["clients"]]
        ok = bool(bw) and all(o < 100_000 and i < 100_000 for _, o, i, _, _ in bw)
        record("bandwidth < 100 KB/s per client", ok,
               ", ".join(f"p{p}: avg out {o/1000:.1f} in {i/1000:.1f} (peak {po/1000:.1f}/{pi/1000:.1f}) KB/s" for p, o, i, po, pi in bw))
        cpu = s["server_cpu"]
        record("headless server uses < 1 CPU core", cpu["avg_cores"] < 1.0 and cpu["peak_cores_1s"] < 1.0,
               f"avg {cpu['avg_cores']:.4f} cores, peak(1s) {cpu['peak_cores_1s']:.4f} cores")

        # 7. Under 5% loss + 100 ms latency.
        check_session("netsim 5% loss + 100 ms", a.netsim_run, a.frames, a.players)
        ns = events(a.netsim_run)
        m = re.search(r"netsim=Some\(LinkConditions \{ loss: ([0-9.]+), latency_us: (\d+)", ns)
        record("netsim run used >= 5% loss and >= 100 ms latency", bool(m) and float(m.group(1)) >= 0.05 and int(m.group(2)) >= 100_000,
               m.group(0) if m else "netsim settings not found in events.log")

    failed = [n for n, ok in results if not ok]
    print(f"\n{len(results) - len(failed)}/{len(results)} criteria passed")
    sys.exit(0 if not failed else 1)


if __name__ == "__main__":
    main()
