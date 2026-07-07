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
#   ./run-gui.sh                          # debug build, JIT off (fastest boot)
#   MACVM_JIT=threshold=1 ./run-gui.sh    # exercise the tier-1 JIT
#   MACVM_IMAGE_PATH=world/image.sqlite3 ./run-gui.sh   # point browser at an image
#   ./run-gui.sh --release                # optimized build (snappier UI)
#
# Any extra arguments after --release pass through to the app.
set -euo pipefail

# Repo root — so relative paths (world/, assets, image) resolve regardless of
# where the script is invoked from.
cd "$(dirname "${BASH_SOURCE[0]}")"

profile_flag=()
bin_dir="debug"
if [[ "${1:-}" == "--release" ]]; then
	profile_flag=(--release)
	bin_dir="release"
	shift
fi

echo "▸ building macvm-gui (${bin_dir})…"
cargo build -p macvm-gui "${profile_flag[@]}"

echo "▸ launching MACVM GUI  (MACVM_JIT=${MACVM_JIT:-off})"
# exec so the app takes over this process — the script blocks until you quit
# the app, and Ctrl-C / the window's close button end it cleanly.
exec "./target/${bin_dir}/macvm-gui" "$@"
