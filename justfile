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

soak-s08:
    sed '$s/.*/Soak run: 200000./' world/bench/soak.mst > /tmp/macvm_soak_1hr.mst
    MACVM_TRACE=gc cargo run --release --quiet -- run /tmp/macvm_soak_1hr.mst --world world
