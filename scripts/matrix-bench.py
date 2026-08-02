#!/usr/bin/env python3
"""Feature-matrix INDICATOR sweep for MACVM (docs/regalloc_findings.md).

MACVM against itself — no Cog/MACDART interleave. Sweeps codegen feature gates
x inline-budget level over the responsive benchmark subset, briefly: each
configuration is REPS short runs (25 warmup + 9 samples each, ~0.5s), scored by
the best median per bench, and reported as a % delta against the DEFAULT
configuration.

This is a screen, not a verdict. It answers "does this combination move
anything, and which way" cheaply enough to cover a dozen configurations;
anything it flags is re-measured with the full 41-sample cog-bench protocol
(alternating arm order) before it is believed.

Thermal discipline between configurations, since that is what invented a
phantom regression earlier in this arc: wait for the 1-minute load average to
fall below IDLE_LOAD, then sleep COOL seconds, before every configuration.
"""
import os, re, subprocess, sys, time

REPS = int(os.environ.get("REPS", 5))
COOL = float(os.environ.get("COOL", 20))
IDLE_LOAD = float(os.environ.get("IDLE_LOAD", 2.5))
BENCHES = ["fib", "richards", "deltablue", "sieve", "dict"]
LINE = re.compile(r"(\w+)\s+warm_us=(\d+) min_us=(\d+)")

# (label, env overrides). None value = leave unset (the default).
CONFIGS = [
    ("default",              {}),
    ("no-resident-calls",    {"MACVM_RESIDENT_CALLS": "0"}),          # Stage 1 control
    ("no-prologue-stp",      {"MACVM_PROLOGUE_STP": "0"}),            # Stage 2 control
    ("peep-imm",             {"MACVM_PEEP_IMM": "1"}),                # parked peephole
    ("inline-2",             {"MACVM_INLINE_LEVEL": "2"}),
    ("inline-3",             {"MACVM_INLINE_LEVEL": "3"}),
    ("inline-4",             {"MACVM_INLINE_LEVEL": "4"}),
    ("inline-2+peep",        {"MACVM_INLINE_LEVEL": "2", "MACVM_PEEP_IMM": "1"}),
    ("inline-3+peep",        {"MACVM_INLINE_LEVEL": "3", "MACVM_PEEP_IMM": "1"}),
    ("inline-4+peep",        {"MACVM_INLINE_LEVEL": "4", "MACVM_PEEP_IMM": "1"}),
    ("inline-4+no-stp",      {"MACVM_INLINE_LEVEL": "4", "MACVM_PROLOGUE_STP": "0"}),
]

def load1():
    return os.getloadavg()[0]

def settle():
    waited = 0
    while load1() > IDLE_LOAD and waited < 180:
        time.sleep(10); waited += 10
    time.sleep(COOL)

def run_once(env_over):
    env = dict(os.environ)
    env["MACVM_JIT"] = "threshold=20"
    env.update(env_over)
    out = subprocess.run(
        ["./target/release/macvm", "run", "scripts/matrix-bench.mst", "--world", "world"],
        capture_output=True, text=True, env=env, timeout=300)
    res = {}
    for m in LINE.finditer(out.stdout):
        res[m.group(1)] = (int(m.group(2)), int(m.group(3)))
    if len(res) != len(BENCHES):
        sys.stderr.write(f"  !! incomplete run: {out.stdout[-300:]}{out.stderr[-300:]}\n")
    return res

def main():
    results = {}
    for label, env_over in CONFIGS:
        settle()
        best = {}
        for _ in range(REPS):
            for b, (warm, mn) in run_once(env_over).items():
                w, m = best.get(b, (10**9, 10**9))
                best[b] = (min(w, warm), min(m, mn))
        results[label] = best
        print(f"  ran {label:18} load1={load1():.2f}  " +
              " ".join(f"{b}={best.get(b,('?',))[0]}" for b in BENCHES), flush=True)

    base = results["default"]
    print("\n=== INDICATOR MATRIX — % vs default (warm median, best of "
          f"{REPS}); negative = faster ===")
    print(f"{'config':20}" + "".join(f"{b:>11}" for b in BENCHES) + f"{'mean':>9}")
    print("-" * (20 + 11 * len(BENCHES) + 9))
    for label, _ in CONFIGS:
        r = results[label]; cells = []; deltas = []
        for b in BENCHES:
            if b in r and b in base and base[b][0]:
                d = 100 * (r[b][0] - base[b][0]) / base[b][0]
                deltas.append(d); cells.append(f"{d:+10.1f}%")
            else:
                cells.append(f"{'--':>11}")
        mean = sum(deltas) / len(deltas) if deltas else 0
        print(f"{label:20}" + "".join(cells) + f"{mean:+8.1f}%")
    print("\nIndicators only (brief protocol). Re-measure anything promising with"
          "\nthe full cog-bench protocol + alternating arm order before believing it.")

main()
