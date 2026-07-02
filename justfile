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
