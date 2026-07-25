#!/bin/sh
# dart-bench.sh — MACVM vs the 2017 Dart 1.24.3 VM (V1, arm64 JIT), run
# natively in the Lima ubuntu VM. Mirrors cog-bench.sh's honesty protocol:
# 1-min load gate, commit-stamped header, DART+MACVM back-to-back pairs per
# round (same-thermal-state), best-of-rounds on the µs warm numbers.
#
# The Dart side runs scripts/dart-bench.dart — a checksum-asserting port of
# the SAME seven workloads — one PROCESS PER BENCH because two corners of
# the 2017 arm64 optimizing JIT SIGILL on modern hardware:
#   - the background-compiler install path (all benches: --no_background_compilation);
#   - the polymorphic-with-deopt/megamorphic path (deltablue only adds
#     --no_polymorphic_with_deopt; richards NEEDS poly-deopt ON — its poly
#     sites through the megamorphic route hit the other broken corner).
# Both workarounds can only SLOW Dart, so Dart's numbers are a floor.
#
# Usage: ./scripts/dart-bench.sh   (ROUNDS=n, THRESH=n, FORCE=1 to skip gate)

set -e
cd "$(dirname "$0")/.."

ROUNDS="${ROUNDS:-3}"
THRESH="${THRESH:-20}"
DART=/Users/oberon/claudeprojects/MACVM/.dart/dart-sdk/bin/dart
DARTSRC=/Users/oberon/claudeprojects/MACVM/scripts/dart-bench.dart

LOAD1=$(uptime | sed -E 's/.*load averages?: *([0-9.]+).*/\1/')
if [ "${FORCE:-0}" != "1" ] && [ "$(printf '%.0f' "$LOAD1")" -ge 4 ]; then
    echo "1-min load $LOAD1 is too high for a clean comparison; wait for it to settle (or FORCE=1)."; exit 3
fi
GITDESC="$(git rev-parse --short HEAD 2>/dev/null || echo '?')$(git diff --quiet 2>/dev/null || echo '+dirty')"
echo "load=$LOAD1  rounds=$ROUNDS  macvm-threshold=$THRESH  commit=$GITDESC  (Dart 1.24.3 linux-arm64 in Lima, per-bench flags)"

run_dart() {
    for b in arith fib sieve dict alloc richards; do
        limactl shell ubuntu -- "$DART" --no_background_compilation "$DARTSRC" "$b" </dev/null 2>/dev/null
    done
    limactl shell ubuntu -- "$DART" --no_background_compilation --no_polymorphic_with_deopt "$DARTSRC" deltablue </dev/null 2>/dev/null
}

RAW=/tmp/dartbench_raw.txt
: > "$RAW"
i=1
while [ "$i" -le "$ROUNDS" ]; do
    # DART then MACVM, back to back — a same-thermal-state pair.
    run_dart | grep 'warm_us=' | sed "s/^/dart /" >> "$RAW"
    MACVM_JIT=threshold="$THRESH" ./target/release/macvm run scripts/cog-bench.mst --world world </dev/null 2>/dev/null \
        | grep 'warm_us=' | sed "s/^/macvm /" >> "$RAW"
    echo "  round $i done"
    i=$((i + 1))
done

python3 - "$RAW" <<'PY'
import sys, re, collections
best = collections.defaultdict(lambda: float('inf'))
order = []
for line in open(sys.argv[1]):
    m = re.match(r'(\w+) (\S+) +cold_us=(\d+) warm_us=(\d+)', line)
    if not m: continue
    vm, bench, _, warm = m.group(1), m.group(2), m.group(3), int(m.group(4))
    if (vm, bench) not in best or warm < best[(vm, bench)]:
        best[(vm, bench)] = warm
    if bench not in order: order.append(bench)
print()
print("bench       MACVM ms   Dart ms   ratio  verdict")
print("-" * 50)
for b in order:
    mv, dv = best.get(('macvm', b)), best.get(('dart', b))
    if mv is None or dv is None: continue
    r = mv / dv
    verdict = ("Dart %.2fx faster" % r) if r > 1 else ("MACVM %.2fx faster" % (1 / r))
    print("%-10s %8.1f  %8.1f   %5.2f  %s" % (b, mv / 1000, dv / 1000, dv / mv, verdict))
print()
print("(best-of-rounds, warm = median of 6 x10-rep batches, microsecond clock;")
print(" Dart = one fresh process per bench with 2017-arm64 JIT workaround flags — a floor)")
PY
