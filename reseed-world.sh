#!/usr/bin/env bash
#
# Rebuild the GUI's world image (world/image.sqlite3) from the .mst source of
# truth (world/*.mst) and verify it boots. Run this after ANY change to a world
# class — the GUI boots from the image, NOT the .mst, so a stale image is what
# makes edits "not show up" or the VM fail to boot. See docs/managingtheworld.md.
#
# Seeds from EVERY world/*.list file, not just world.list — world.list first,
# then every other *.list file found (e.g. cocoaui.list), each recorded as its
# own package list in the image (docs/package_aware_editing_design.md §4.5).
#
# A FRESH rebuild (the default) is the safe choice: it sidesteps the incremental
# reseed's edge cases (a removed class var, a renamed/removed class, a changed
# superclass) that an in-place merge can't express.
#
#   ./reseed-world.sh            # fresh rebuild of world/image.sqlite3 + boot check
#   ./reseed-world.sh --keep     # incremental reseed (merge into the existing image)
#   ./reseed-world.sh --no-verify# skip the boot check (faster; not recommended)
#
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

keep=0
verify=1
for arg in "$@"; do
	case "$arg" in
		--keep) keep=1 ;;
		--no-verify) verify=0 ;;
		*) echo "reseed-world.sh: unknown arg '$arg'"; exit 2 ;;
	esac
done

echo "▸ building macvm-gui (release)…"
cargo build -p macvm-gui --release

if [[ $keep -eq 0 ]]; then
	echo "▸ removing world/image.sqlite3 for a clean rebuild…"
	rm -f world/image.sqlite3
fi

echo "▸ seeding world/image.sqlite3 from world/*.mst (every world/*.list)…"
./target/release/macvm-gui seed --world world

if [[ $verify -eq 1 ]]; then
	echo "▸ verifying the world image installs every class and boots…"
	# Seeds a throwaway image from world/ and boots it — fails loudly if any
	# class won't install (e.g. a class var that didn't round-trip).
	cargo test -p macvm-gui --bin macvm-gui \
		world_image_installs_all_classes_without_error -- --quiet
fi

echo "✓ world image rebuilt and boots — launch it with ./run-gui.sh"
