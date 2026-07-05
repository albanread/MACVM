#!/bin/sh
# perf.sh — the S15 A7 measurement procedure (sprint_s15_detail.md).
# Runs every benchmark in world/bench/bench.list under three modes and
# prints PERF.md-ready table rows. The harness (Bench.mst) already does
# 3 discarded warmups + median-of-outer inside the process, timed with
# millisecondClock (excludes genesis + world load); this script only
# orchestrates modes and formats.
#
# Refuses to run under stress instrumentation — stressed numbers are not
# performance numbers (A7's own rule).
set -eu
cd "$(dirname "$0")/.."

if [ -n "${MACVM_GC_STRESS:-}" ] || [ -n "${MACVM_DEOPT_STRESS:-}" ]; then
    echo "perf.sh: refusing to measure under MACVM_GC_STRESS/MACVM_DEOPT_STRESS" >&2
    exit 2
fi

BIN=./target/release/macvm
[ -x "$BIN" ] || { echo "perf.sh: build first (cargo build --release)" >&2; exit 2; }
LIST=world/bench/bench.list
[ -f "$LIST" ] || { echo "perf.sh: missing $LIST" >&2; exit 2; }

echo "| benchmark | interp (ms) | jit t=1 | jit t=1000 | best/interp |"
echo "|---|---|---|---|---|"
while IFS= read -r f; do
    case "$f" in ''|'#'*) continue ;; esac
    name=$(basename "$f" .mst)
    interp=$(MACVM_JIT=off        "$BIN" run "world/bench/$f" --world world 2>/dev/null | awk 'END{print $NF}')
    t1=$(MACVM_JIT=threshold=1    "$BIN" run "world/bench/$f" --world world 2>/dev/null | awk 'END{print $NF}')
    t1000=$(MACVM_JIT=threshold=1000 "$BIN" run "world/bench/$f" --world world 2>/dev/null | awk 'END{print $NF}')
    best=$t1; [ "$t1000" -lt "$best" ] 2>/dev/null && best=$t1000
    if [ "${best:-0}" -gt 0 ] 2>/dev/null && [ "${interp:-0}" -gt 0 ] 2>/dev/null; then
        ratio=$(awk "BEGIN{printf \"%.1f\", $interp/$best}")
    else
        ratio="n/a"
    fi
    echo "| $name | $interp | $t1 | $t1000 | ${ratio}x |"
done < "$LIST"
