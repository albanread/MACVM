# MACVM CI contract. See docs/sprints/CONVENTIONS.md and
# docs/sprints/sprint_s00_detail.md — this file IS the CI contract until a
# hosted CI is set up.

test:
    cargo test

test-release:
    cargo test --release

lint:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

ci: lint test

# Sprint acceptance gates. Later sprints append stress runs to their gate
# (e.g. `MACVM_GC_STRESS=1 just test` from S7 on).
gate-s00: ci
gate-s01: ci
gate-s02: ci
gate-s03: ci
gate-s04: ci
gate-s05: ci
gate-s06: ci

# S7: young-gen scavenger. Full suite green under MACVM_GC_STRESS=1
# (scavenge before every allocation) as well as stress off (via `ci`).
gate-s07: ci
    MACVM_GC_STRESS=1 cargo test

# S8: full mark-slide-compact GC (tests_s08.md's acceptance gate). Full
# suite green under stress off and =1 (via gate-s07), under =full (a full
# GC every 100 allocations), and the in-language suite specifically under
# the maximally aggressive =full:1 in debug. --test-threads=1 for the last
# step: a full GC on every single allocation is expensive per-call by
# design (it's the whole point of =full:1), and cargo test's default
# parallelism runs it_world's 6 tests concurrently — several of them
# CPU-heavy under this setting, including one that spawns a subprocess
# which ALSO loads the world under it — so contention alone turns a
# ~65s test into 4+ minutes with nothing actually wrong.
gate-s08: gate-s07
    MACVM_GC_STRESS=full cargo test
    MACVM_GC_STRESS=full:1 cargo test --test it_world -- --test-threads=1

# S9: vendored JASM wfasm + Assembler/JasmAssembler/CodeCache (tests_s09.md's
# acceptance gate). The no-LLVM check is warn-only (documents the corpus-
# replay-without-an-oracle claim; CI images without llvm make a hard fail
# impractical, and this dev machine has llvm via homebrew regardless). The
# P1 lint is a hard fail: a literal, comment-blind grep, so it also catches
# an explanatory comment that quotes its own trigger strings, not just a
# real re-introduced oracle dependency. it_codecache runs under --release
# specifically (not just via `ci`'s debug-mode `cargo test`) because W^X/
# icache bugs can hide in debug — this sprint found one exactly that way
# before this gate existed (patch_branch26's guard-ordering bug, only
# caught by actually running the integration tests, not by review).
gate-s09: gate-s08
    -command -v llvm-mc && echo "note: llvm-mc present -- no-LLVM claim not exercised this run"
    ! grep -rn 'crate::oracle\|feature = "llvm"' src/vendor/
    cargo test -p macvm
    cargo test -p macvm --release --test it_codecache
    cargo clippy --all-targets -- -D warnings

# S8 gate item 4: soak the full GC under sustained realistic churn with a
# continuous shadow-model integrity check (world/bench/soak.mst). The
# 2-minute CI variant runs routinely; the 1-hour variant is executed once
# per sprint sign-off with its numbers recorded in docs/PERF.md (both
# substitute the cycle count into the script's last line via sed, per
# world/bench's own hardcoded-literal convention — see soak.mst's own
# doc comment). Both run --release: debug-mode's unoptimized bytecode
# interpretation plus always-on verify_heap_at made even 10 cycles take
# 30+ seconds (0.6s under --release) — an interpretation-speed fact, not
# a GC one (confirmed by profiling before reaching for this fix).
soak-s08-ci:
    sed '$s/.*/Soak run: 400./' world/bench/soak.mst > /tmp/macvm_soak_ci.mst
    cargo run --release --quiet -- run /tmp/macvm_soak_ci.mst --world world

# S10 gate item 1 (differential): concatenate world/tests/tests.list's
# files (in order) into one temp .mst — TestRunner's SUnit-lite state
# (start/run:/report) must accumulate across them within ONE VM session,
# which `macvm run <one-file>` gives for free but N separate CLI
# invocations wouldn't. Plain concatenation is sound because each listed
# file is already independently well-formed top-level Smalltalk source
# (same reasoning `it_world.rs`'s own `load_tests_list` loop relies on,
# just done in the shell instead of in Rust so this is CLI/stdout-diffable
# under different MACVM_JIT values, not only assertable in-process).
run-world-tests:
    grep -v '^#' world/tests/tests.list | grep -v '^$' | sed 's|^|world/tests/|' | xargs cat > /tmp/macvm_world_tests.mst
    cargo run --quiet -- run /tmp/macvm_world_tests.mst --world world

soak-s08:
    sed '$s/.*/Soak run: 200000./' world/bench/soak.mst > /tmp/macvm_soak_1hr.mst
    MACVM_TRACE=gc cargo run --release --quiet -- run /tmp/macvm_soak_1hr.mst --world world

# S10 gate item 3 (perf marker, tracking not gating): world/bench/arith.mst's
# sumTo: 5_000_000 timed under MACVM_JIT=off vs threshold=1, --release (debug
# timing is noise, not signal). A shebang recipe, not just's default
# line-per-subprocess execution (each line of a plain recipe runs in its own
# shell, so a variable set on one line isn't visible on the next) -- needed
# here since interp_ms and jit_ms both have to survive to the same final
# append line.
bench-s10:
    #!/usr/bin/env bash
    set -euo pipefail
    interp_out=$(MACVM_JIT=off cargo run --release --quiet -- run world/bench/arith.mst --world world)
    jit_out=$(MACVM_JIT=threshold=1 cargo run --release --quiet -- run world/bench/arith.mst --world world)
    interp_ms=$(echo "$interp_out" | grep -o 'ms: [0-9]*' | grep -o '[0-9]*')
    jit_ms=$(echo "$jit_out" | grep -o 'ms: [0-9]*' | grep -o '[0-9]*')
    ratio=$(echo "scale=2; $interp_ms / $jit_ms" | bc)
    date_str=$(date +%Y-%m-%d)
    commit=$(git rev-parse --short HEAD)
    echo "| $date_str | $commit | $interp_ms | $jit_ms | ${ratio}x |" >> docs/PERF.md
    echo "arith bench: interp_ms=$interp_ms jit_ms=$jit_ms ratio=${ratio}x"
    below2=$(echo "$ratio < 2" | bc)
    below5=$(echo "$ratio < 5" | bc)
    if [ "$below2" = "1" ]; then
        echo "FAIL: compiled speedup ${ratio}x is below the 2x architectural-mistake tripwire" >&2
        exit 1
    fi
    if [ "$below5" = "1" ]; then
        echo "WARN: compiled speedup ${ratio}x is below the 5x target (tracking only, not gating)"
    fi

# S11 gate item 4 (perf marker, tracking not gating): world/bench/dispatch.mst's
# runLoop: 5_000_000 (a real per-iteration super-send dispatch, D4.6 -- see
# that file's own doc for why it isn't the literal 3-class polymorphic design
# tests_s11.md sketches) timed under MACVM_JIT=off vs threshold=1, --release.
# Same shebang-recipe shape as bench-s10, same warn<5x/fail<2x tripwire --
# expect a SMALLER ratio than arith.mst's ~130x, honestly (a real send still
# costs a real dispatch even compiled; this measures that cost, not erasing
# it), so the 5x warn line is more likely to actually fire here than on
# bench-s10 -- that is expected, not a regression.
bench-s11:
    #!/usr/bin/env bash
    set -euo pipefail
    interp_out=$(MACVM_JIT=off cargo run --release --quiet -- run world/bench/dispatch.mst --world world)
    jit_out=$(MACVM_JIT=threshold=1 cargo run --release --quiet -- run world/bench/dispatch.mst --world world)
    interp_ms=$(echo "$interp_out" | grep -o 'ms: [0-9]*' | grep -o '[0-9]*')
    jit_ms=$(echo "$jit_out" | grep -o 'ms: [0-9]*' | grep -o '[0-9]*')
    ratio=$(echo "scale=2; $interp_ms / $jit_ms" | bc)
    date_str=$(date +%Y-%m-%d)
    commit=$(git rev-parse --short HEAD)
    echo "| $date_str | $commit | $interp_ms | $jit_ms | ${ratio}x |" >> docs/PERF.md
    echo "dispatch bench: interp_ms=$interp_ms jit_ms=$jit_ms ratio=${ratio}x"
    below2=$(echo "$ratio < 2" | bc)
    below5=$(echo "$ratio < 5" | bc)
    if [ "$below2" = "1" ]; then
        echo "FAIL: compiled speedup ${ratio}x is below the 2x architectural-mistake tripwire" >&2
        exit 1
    fi
    if [ "$below5" = "1" ]; then
        echo "WARN: compiled speedup ${ratio}x is below the 5x target (tracking only, not gating)"
    fi

# S10: tier-1 JIT compiler (tests_s10.md's acceptance gate). gate-s09
# already covers "cargo test" + "cargo clippy -- -D warnings" (tests_s10.md
# gate script's own first/last lines) via the dependency chain, so this
# recipe is just the S10-specific middle: the off-vs-threshold=1
# differential byte-identical (gate item 1), GC-stress regression under the
# JIT's default (off) mode -- S10 never combines jit+stress, that's S12's
# flagship, not tested here -- and the perf marker recorded (gate item 3).
gate-s10: gate-s09
    MACVM_JIT=off just run-world-tests > /tmp/s10_off.txt
    MACVM_JIT=threshold=1 just run-world-tests > /tmp/s10_t1.txt
    diff /tmp/s10_off.txt /tmp/s10_t1.txt
    MACVM_GC_STRESS=1 just run-world-tests
    MACVM_GC_STRESS=full:64 just run-world-tests
    just bench-s10

# INVERTED by S12 step 7 (P10: "the S11 gc_under_compiled == 0 test must be
# inverted, not deleted — it becomes the proof the bridge is gone"). Same
# recipe name, same combined-stress run, OPPOSITE assertion: the exit-time
# "gc: gc_under_compiled=M" line (print_gc_bridge_stats, main.rs) must now
# show M > 0 — real collections genuinely ran with live compiled frames on
# the native stack (the hard case S12 exists for), release-mode-visible
# rather than debug-assert-only. bridge_old_allocs no longer exists (the
# field is deleted; anything referencing it fails to compile — tests_s12.md
# gate item 6).
bridge-stats-s11:
    #!/usr/bin/env bash
    set -euo pipefail
    grep -v '^#' world/tests/tests.list | grep -v '^$' | sed 's|^|world/tests/|' | xargs cat > /tmp/macvm_world_tests.mst
    # GC_STRESS=1 (scavenge on EVERY allocation), not full:64: the sampled
    # mode can legitimately land all of its collections outside compiled
    # windows (1-in-64 across a suite whose compiled stretches are short),
    # reading 0 without anything being wrong -- the every-allocation mode
    # is the one that guarantees collections inside compiled windows exist
    # to count.
    out=$(MACVM_GC_STRESS=1 MACVM_JIT=threshold=1 MACVM_TRACE=gc cargo run --quiet -- run /tmp/macvm_world_tests.mst --world world 2>&1 >/dev/null)
    line=$(echo "$out" | grep '^gc: gc_under_compiled=')
    echo "$line"
    under_compiled=$(echo "$line" | sed -n 's/.*gc_under_compiled=\([0-9]*\).*/\1/p')
    if [ "$under_compiled" = "0" ]; then
        echo "FAIL: gc_under_compiled=0 -- the bridge is gone, so combined stress MUST have run collections under live compiled frames (S12 P10)" >&2
        exit 1
    fi
    date_str=$(date +%Y-%m-%d)
    commit=$(git rev-parse --short HEAD)
    echo "| $date_str | $commit | (bridge deleted) | $under_compiled |" >> docs/PERF.md

# S11: compiled sends + inline alloc + the D8 GC bridge. UNLIKE gate-s10 (which
# deliberately kept GC-stress and the JIT apart, deferring the combo to "S12's
# flagship"), this gate COMBINES them: MACVM_GC_STRESS + MACVM_JIT=threshold=1
# TOGETHER. That is the only way to actually stress-test the collector against
# compiled code + inline allocation, and it is now sound because (a) the D8
# bridge suppresses moving GC while a compiled frame is live and (b) the
# scavenge updates nmethod/PIC/mega Rust-side keys (key_klass/key_selector),
# not just their code-pool oops. Running these two combined for the first time
# is exactly what surfaced that pre-existing scavenge-key use-after-free.
gate-s11: gate-s10
    MACVM_GC_STRESS=1 MACVM_JIT=threshold=1 just run-world-tests
    MACVM_GC_STRESS=full:64 MACVM_JIT=threshold=1 just run-world-tests
    just bridge-stats-s11
    just bench-s11
