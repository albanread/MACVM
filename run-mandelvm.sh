#!/usr/bin/env bash
#
# Launch the standalone MandelVM demo — a fresh MACVM VM instance in its own
# window that runs ONE Mandelbrot zoom dive and then quits itself when the demo
# ends (world/46_mandelvm.mst; the `mandelvm` run-mode of macvm-gui). This is the
# "spin up a new VM instance, run the demo, exit the instance" lifecycle made
# on-screen — the same window + Metal pane + frame loop the GUI uses, but opening
# straight into the demo instead of the browser, and terminating on StopLoop.
#
# Usage:
#   ./run-mandelvm.sh            # release build (default), JIT ON
#   ./run-mandelvm.sh --debug    # unoptimized build (~47x slower; crash-chasing only)
#
# Escape ends the dive early (and quits) too. Release is the default because the
# Double escape-time math is compute-heavy; see run-gui.sh for the why.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

profile_flag=(--release)
bin_dir="release"
if [[ "${1:-}" == "--debug" ]]; then
	profile_flag=()
	bin_dir="debug"
	shift
fi

echo "▸ building macvm-gui (${bin_dir})…"
cargo build -p macvm-gui ${profile_flag[@]+"${profile_flag[@]}"}

echo "▸ launching MandelVM — a throwaway VM instance renders one dive, then exits"
exec "./target/${bin_dir}/macvm-gui" mandelvm "$@"
