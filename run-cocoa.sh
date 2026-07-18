#!/usr/bin/env bash
#
# Launch the native Cocoa GUI (macvm-cocoa, cocoa_gui/ — the AppKit second
# mode, docs/cocoa_gui_design.md). Builds the macvm-cocoa binary, then runs it
# from the repo root so the VM finds world/ (and, when MACVM_IMAGE_PATH is
# set, the versioned SQLite image the browser/editor point at).
#
# Usage:
#   ./run-cocoa.sh                          # release build (default), JIT ON
#   ./run-cocoa.sh --debug                  # unoptimized build — ONLY for
#                                            # chasing a crash; ~20-45x slower
#                                            # on compute-heavy work (the
#                                            # MandelZoom/Breakout demos)
#   MACVM_COCOA_CTL=7644 ./run-cocoa.sh     # open the rusttcl control channel
#   MACVM_IMAGE_PATH=world/image.sqlite3 ./run-cocoa.sh   # point at an image
#
# Release is the default for the SAME reason run-gui.sh defaults to release:
# debug is unoptimized Rust, so the JIT compiler, GC, and dispatch runtime
# helpers all run tens of times slower — measured directly (Time
# millisecondsToRun:, matched JIT threshold): a 320x240 Mandelbrot frame is
# ~36ms release vs ~800ms debug. `cargo run -p cocoa_gui` builds `dev` by
# default — reach for THIS script instead when you want to see real
# performance (MandelZoom, ParallelMandel, Breakout), and reserve
# `cargo run`/`--debug` for chasing a crash with debug symbols.
#
# Any extra arguments after --debug pass through to the app.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

profile_flag=(--release)
bin_dir="release"
if [[ "${1:-}" == "--debug" ]]; then
	profile_flag=()
	bin_dir="debug"
	shift
fi

echo "▸ building macvm-cocoa (${bin_dir})…"
cargo build -p cocoa_gui ${profile_flag[@]+"${profile_flag[@]}"}

echo "▸ launching MACVM Cocoa GUI  (MACVM_JIT=${MACVM_JIT:-threshold=10 (default)})"
exec "./target/${bin_dir}/macvm-cocoa" "$@"
