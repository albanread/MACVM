#!/usr/bin/env bash
#
# Launch the MACVM GUI — the native Cocoa/WKWebView programming environment
# (gui/, SPEC §16). Builds the macvm-gui binary, then runs it.
#
# Runs from the repo root so the VM finds world/ (and, when MACVM_IMAGE_PATH
# is set, the versioned SQLite image the class browser points at). The
# language thread runs the real embedded VM (src/embed.rs, VmHandle); the
# JIT is fully supported.
#
# Usage:
#   ./run-gui.sh                          # release build (default), JIT ON
#                                         # (gui_vm_options: threshold=10 when
#                                         #  MACVM_JIT is unset)
#   ./run-gui.sh --debug                  # unoptimized build — ONLY for chasing
#                                         # a crash; ~47x slower on compute-heavy
#                                         # work (e.g. the Mandelbrot canvas demo)
#   MACVM_JIT=off ./run-gui.sh            # force the pure interpreter
#   MACVM_JIT=threshold=1 ./run-gui.sh    # compile aggressively (first call)
#   MACVM_IMAGE_PATH=world/image.sqlite3 ./run-gui.sh   # point browser at an image
#
# Release is the default because debug is unoptimized Rust: the JIT compiler,
# GC, and the allocation/dispatch runtime helpers all run ~47x slower, which is
# very visible on compute-heavy Smalltalk (the Mandelbrot canvas demo is 0.7s
# release vs 33s debug). Use --debug only when a release crash needs debugging.
#
# Any extra arguments after --debug pass through to the app.
set -euo pipefail

# Repo root — so relative paths (world/, assets, image) resolve regardless of
# where the script is invoked from.
cd "$(dirname "${BASH_SOURCE[0]}")"

# Release by default; --debug opts into the unoptimized build.
profile_flag=(--release)
bin_dir="release"
if [[ "${1:-}" == "--debug" ]]; then
	profile_flag=()
	bin_dir="debug"
	shift
fi

echo "▸ building macvm-gui (${bin_dir})…"
# Bash 3.2 (macOS default) treats "${arr[@]}" on an empty array as an unbound
# variable under `set -u`; the `[@]+…` form expands to nothing when empty.
cargo build -p macvm-gui ${profile_flag[@]+"${profile_flag[@]}"}

echo "▸ launching MACVM GUI  (MACVM_JIT=${MACVM_JIT:-threshold=10 (GUI default)})"
# exec so the app takes over this process — the script blocks until you quit
# the app, and Ctrl-C / the window's close button end it cleanly.
exec "./target/${bin_dir}/macvm-gui" "$@"
