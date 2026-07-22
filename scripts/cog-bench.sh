#!/bin/sh
# cog-bench.sh — run the micro+macro benchmark suite under Pharo/Cog and
# MACVM, same workloads, same protocol (10 inner reps; cold then median of 6
# warm), MICROSECOND clock on BOTH sides, interleaved back-to-back for R
# rounds on the same machine. The standing target: at least as fast as Cog.
#
# WHY microsecond: Pharo's millisecond clock and MACVM's `.as_millis()` both
# truncate, which on the sub-5 ms benches (sieve, deltablue) hid the real
# gaps and manufactured phantom ones (the WINVM investigation, PERF.md
# 2026-07-22). Both sides now read `microsecondClock` (Pharo:
# Time microsecondClockValue; MACVM: prim 252, added for this).
#
# APPLE SILICON HONESTY: unlike the WINVM/Windows harness there is NO hard
# core pinning here — macOS/arm64 exposes no per-core affinity, and thread
# affinity tags are advisory (ignored on Apple Silicon). Foreground default-
# QoS work already stays on P-cores, so the residual noise is thermal drift,
# not the P/E lottery. We control it the only honest way: a quiet machine
# (this script refuses to start if 1-min load is high), interleaved A/B
# rounds so each Cog/MACVM pair sees the same thermal state, and best-of the
# rounds. Only same-round pairs are meaningful.
#
# Setup (once): install Pharo 13 headless into $COG_DIR (default ./.cog),
# so that "$COG_DIR/pharo" and "$COG_DIR/Pharo.image" exist:
#   curl -L https://get.pharo.org/64/130 | bash    # into $COG_DIR
set -eu
cd "$(dirname "$0")/.."

ROUNDS=${ROUNDS:-3}
THRESH=${MACVM_THRESHOLD:-20}
COG_DIR=${COG_DIR:-./.cog}
PHARO="$COG_DIR/pharo"
IMG="$COG_DIR/Pharo.image"

[ -x ./target/release/macvm ] || { echo "build first: cargo build --release"; exit 2; }
{ [ -x "$PHARO" ] && [ -f "$IMG" ]; } || {
    echo "no Pharo at COG_DIR=$COG_DIR (need ./pharo + Pharo.image) — see setup comment"; exit 2; }

# Quiet-machine gate: the user works on this box; a loaded machine makes the
# comparison meaningless. Refuse above 4.0 unless FORCE=1.
LOAD1=$(uptime | sed -E 's/.*load averages?: *([0-9.]+).*/\1/')
if [ "${FORCE:-0}" != "1" ] && [ "$(printf '%.0f' "$LOAD1")" -ge 4 ]; then
    echo "1-min load $LOAD1 is too high for a clean comparison; wait for it to settle (or FORCE=1)."; exit 3
fi
echo "load=$LOAD1  rounds=$ROUNDS  macvm-threshold=$THRESH  (microsecond clock, no hard pinning — Apple Silicon)"

# Richards + DeltaBlue are translated from world/41a on the fly so the .mst
# stays the single source of truth; the emitted fileIn carries the same
# checksums the MACVM driver asserts.
python3 scripts/mst2st.py /tmp/cog-all.st --assemble >/dev/null

RAW=/tmp/cogbench_raw.txt
: > "$RAW"
i=1
while [ "$i" -le "$ROUNDS" ]; do
    # COG then MACVM, back to back — a same-thermal-state pair.
    ( cd "$COG_DIR" && ./pharo Pharo.image st /tmp/cog-all.st </dev/null 2>/dev/null ) \
        | grep 'warm_us=' | sed "s/^/cog /" >> "$RAW"
    MACVM_JIT=threshold="$THRESH" ./target/release/macvm run scripts/cog-bench.mst --world world </dev/null 2>/dev/null \
        | grep 'warm_us=' | sed "s/^/macvm /" >> "$RAW"
    echo "  round $i done"
    i=$((i + 1))
done

# Reduce: best (min) warm_us per (vm,bench) across rounds, ms with one
# decimal, ratio and verdict. Best-of is the right summary — it strips the
# rounds that lost the core to something else.
python3 - "$RAW" <<'PY'
import sys, re, collections
best = collections.defaultdict(lambda: float('inf'))
order = []
for line in open(sys.argv[1]):
    m = re.match(r'(\w+)\s+(\S+)\s+.*warm_us=(\d+)', line)
    if not m: continue
    vm, bench, us = m.group(1), m.group(2), int(m.group(3))
    if bench not in order: order.append(bench)
    best[(vm, bench)] = min(best[(vm, bench)], us)
print(f"\n{'bench':10} {'MACVM ms':>9} {'Cog ms':>8} {'ratio':>7}  verdict")
print("-" * 48)
for b in order:
    mv, cg = best[('macvm', b)], best[('cog', b)]
    if mv == float('inf') or cg == float('inf'):
        print(f"{b:10} {'—':>9} {'—':>8}   (missing)"); continue
    r = mv / cg
    verdict = (f"MACVM {cg/mv:.2f}x faster" if r < 0.97 else
               f"Cog {r:.2f}x faster"       if r > 1.03 else "parity")
    print(f"{b:10} {mv/1000:>9.1f} {cg/1000:>8.1f} {r:>7.2f}  {verdict}")
print("\n(best-of-rounds, warm = median of 6 x10-rep batches, microsecond clock)")
PY
