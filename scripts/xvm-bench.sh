#!/bin/sh
# xvm-bench.sh — three-VM Smalltalk microbenchmark, headless / CLI only.
#
#   MACDART  Smalltalk on the ported Dart 1.24.3 VM  (macdart/build-st-rel/dart)
#   MACVM    our from-scratch Rust Smalltalk VM       (target/release/macvm)
#   Cog      Pharo 13 — the production Smalltalk JIT   (.cog/Pharo.image)
#
# Same BenchmarkDashboard workloads, checksum-verified body-for-body across all
# three, timed by ONE protocol (scripts/cog-bench.{mst,st} run:block:check:):
#   cold first run, 30 warmup iters (reach the JIT steady state), then 41
#   single-workload MICROSECOND samples — median + min + MAD reported so noise
#   is a number, not an RNG. Monotonic µs clock on every side
#   (MACDART stMicrosecondClock / MACVM prim 252 / Pharo Time microsecondClockValue).
#
# Rounds are INTERLEAVED (MACDART, MACVM, Cog back-to-back) so each triple sees
# the same thermal state; the report takes best-of-rounds median. No GUI.
#
#   ROUNDS=5 ./scripts/xvm-bench.sh          # more rounds = steadier best-of
#   COG_DIR=./.cog MACDART=~/claudeprojects/MACDART ./scripts/xvm-bench.sh
set -eu
cd "$(dirname "$0")/.."                       # MACVM root

ROUNDS=${ROUNDS:-5}
# MACVM's JIT for `run` is gated by MACVM_JIT — WITHOUT this it stays in the
# interpreter (cold==warm, ~50-170x slower), which silently makes every MACVM
# number meaningless. threshold=N tiers a method up after N sends; 20 < the 30
# warmup iters, so it is fully hot before the first timed sample.
THRESH=${MACVM_THRESHOLD:-20}
COG_DIR=${COG_DIR:-./.cog}
MACDART=${MACDART:-$HOME/claudeprojects/MACDART}
DART="$MACDART/macdart/build-st-rel/dart"
RUNMST="$MACDART/macdart/st/test/run_mst.dart"
PHARO="$COG_DIR/pharo"; IMG="$COG_DIR/Pharo.image"

[ -x ./target/release/macvm ] || { echo "build macvm: cargo build --release"; exit 2; }
[ -x "$DART" ] || { echo "no MACDART ST dart at $DART (build macdart/build-st-rel)"; exit 2; }
{ [ -x "$PHARO" ] && [ -f "$IMG" ]; } || { echo "no Pharo/Cog in $COG_DIR (get.pharo.org/64/130)"; exit 2; }

# Quiet-machine gate — a loaded box makes the comparison meaningless.
LOAD1=$(uptime | sed -E 's/.*load averages?: *([0-9.]+).*/\1/')
if [ "${FORCE:-0}" != "1" ] && [ "$(printf '%.0f' "$LOAD1")" -ge 4 ]; then
  echo "1-min load $LOAD1 too high for a clean run; wait for it to settle (or FORCE=1)."; exit 3; fi
GITDESC="$(git rev-parse --short HEAD 2>/dev/null || echo '?')$(git diff --quiet 2>/dev/null || echo '+dirty')"
echo "xvm-bench: load=$LOAD1 rounds=$ROUNDS macvm=$GITDESC MACVM_JIT=threshold=$THRESH (µs clock, interleaved, no GUI)"

python3 scripts/mst2st.py /tmp/cog-all.st --assemble >/dev/null   # Cog fileIn from the same .mst bodies

RAW=/tmp/xvm_raw.txt; : > "$RAW"
r=1
while [ "$r" -le "$ROUNDS" ]; do
  "$DART" --with-st "$RUNMST" scripts/cog-bench.mst </dev/null 2>/dev/null \
    | grep 'warm_us=' | sed 's/^/macdart /' >> "$RAW"
  MACVM_JIT=threshold="$THRESH" ./target/release/macvm run scripts/cog-bench.mst --world world </dev/null 2>/dev/null \
    | grep 'warm_us=' | sed 's/^/macvm /' >> "$RAW"
  ( cd "$COG_DIR" && ./pharo Pharo.image st /tmp/cog-all.st </dev/null 2>/dev/null ) \
    | grep 'warm_us=' | sed 's/^/cog /' >> "$RAW"
  echo "  round $r/$ROUNDS done"
  r=$((r + 1))
done

python3 - "$RAW" <<'PY'
import sys, re, collections
rows = collections.defaultdict(list); order = []
for line in open(sys.argv[1]):
    m = re.match(r'(\w+)\s+(\S+)\s+.*warm_us=(\d+)\s+min_us=(\d+)\s+mad_us=(\d+)', line)
    if not m: continue
    vm, b = m.group(1), m.group(2).strip()
    med, mn, mad = int(m.group(3)), int(m.group(4)), int(m.group(5))
    if b not in order: order.append(b)
    rows[(vm, b)].append((med, mad))
def best(vm, b):
    xs = rows.get((vm, b))
    if not xs: return None
    med = min(x[0] for x in xs)                       # best-of-rounds median
    cv  = 100.0 * (sorted(x[1] for x in xs)[len(xs)//2]) / med if med else 0
    return med, cv
h = f"{'bench':10} {'MACDART':>9} {'Cog':>8} {'MACVM':>9}   {'Dart vs Cog':>13} {'Dart vs MACVM':>13}  {'noise':>5}"
print("\n" + h); print("-" * len(h))
for b in order:
    d, c, m = best('macdart', b), best('cog', b), best('macvm', b)
    if not (d and c and m): print(f"{b:10} (missing)"); continue
    dc = c[0] / d[0]                                   # >1 = Dart faster
    vc = f"Dart {dc:.1f}x" if dc >= 1.03 else (f"Cog {1/dc:.1f}x" if dc <= 0.97 else "~parity")
    vm = f"{m[0]/d[0]:.0f}x"
    noise = max(d[1], c[1], m[1])
    print(f"{b:10} {d[0]:>8}µ {c[0]:>7}µ {m[0]:>8}µ   {vc:>13} {vm:>13}  {noise:>4.0f}%")
print("\nµs per iteration, warm, best-of-rounds median. 'Dart vs Cog' >1x = Dart faster.")
print("'noise' = worst per-round MAD/median across the three VMs (low = trustworthy).")
PY
