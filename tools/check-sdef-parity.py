#!/usr/bin/env python3
"""Gate: the sdef's `view` enumerators must match the world's registration order.

`docs/applescript_design.md` §4 — a compiled AppleScript stores the four-char
enumerator CODE, not the name, so if `MvV6` means `canvas` today and `help`
after someone reorders `CocoaUI class >> startup`, every saved script silently
does the wrong thing. Nothing in the running system couples the sdef (a static
resource in Contents/Resources) to the registry (built at boot), so this gate
is the coupling.

Order of truth is the *registration sequence* in `CocoaUI class >> startup`:
`registerViewNamed: #workspace` inline, then one `(Worker classNamed: #CocoaX)
ifNotNil: [ :v | v register ]` per view — each of which calls
`registerViewNamed:` in its own file. NOT `cocoaui.list` order (which is load
order, and differs: find loads before editor there, and registers after
browser2 here).

Run: tools/check-sdef-parity.py [--world world] [--sdef tools/macVM.sdef]
Exit 0 on parity, 1 on drift (with a diff), 2 if the sources can't be read.
"""

import argparse
import pathlib
import re
import sys
import xml.etree.ElementTree as ET


def sdef_views(sdef_path):
    """[(name, code)] for the `view` enumeration, in document order."""
    root = ET.parse(sdef_path).getroot()
    for enum in root.iter("enumeration"):
        if enum.get("name") == "view":
            return [(e.get("name"), e.get("code")) for e in enum.iter("enumerator")]
    raise SystemExit(f"{sdef_path}: no `view` enumeration")


def world_views(world_dir):
    """[name] in CODE order, read out of `CocoaScript class >> viewCodes`.

    THE MAPPING THE RUNTIME ACTUALLY USES. `scriptCurrentView` answers
    `codeFor: CocoaUI activeView in: self viewCodes prefix: 'MvV'` — it looks
    the active view up BY NAME in `viewCodes` and takes the code from its
    index THERE. So the invariant that keeps a compiled script correct is
    `viewCodes` order == the sdef's enumerator order, and nothing else.

    This gate used to read the REGISTRATION sequence in `CocoaUI class >>
    startup` instead. That is a different list for a legitimate reason — a
    view can be registered in any toolbar position, and CocoaBrowser V1 is
    deliberately registered as NO tab at all while keeping its `browser`
    enumerator — so the gate reported drift on a system that was correct, and
    blocked the app build. Registration order sets where a tab SITS; it has
    never set which code a view REPORTS.
    """
    cs = pathlib.Path(world_dir) / "74_cocoascript.mst"
    src = cs.read_text(encoding="utf-8")
    m = re.search(
        r"CocoaScript class >> viewCodes\s*\[\s*\^#\(([^)]*)\)", src, re.S
    )
    if not m:
        raise SystemExit(f"{cs}: no `CocoaScript class >> viewCodes` literal")
    return m.group(1).split()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--world", default="world")
    ap.add_argument("--sdef", default="tools/macVM.sdef")
    args = ap.parse_args()

    try:
        pairs = sdef_views(args.sdef)
        live = world_views(args.world)
    except (OSError, ET.ParseError) as exc:
        print(f"check-sdef-parity: {exc}", file=sys.stderr)
        return 2

    names = [n for n, _ in pairs]
    if names == live:
        print(f"sdef parity OK — {len(names)} views: {' '.join(names)}")
        return 0

    print("SDEF PARITY DRIFT", file=sys.stderr)
    print(f"  sdef  ({args.sdef}): {' '.join(names)}", file=sys.stderr)
    print(f"  world ({args.world}): {' '.join(live)}", file=sys.stderr)
    for i, (a, b) in enumerate(zip(names + [None] * len(live), live + [None] * len(names))):
        if a != b:
            code = pairs[i][1] if i < len(pairs) else "(none)"
            print(f"  first difference at index {i}: sdef {a!r} ({code}) vs world {b!r}",
                  file=sys.stderr)
            break
    print("\nCodes are append-only (design §2): a REORDER silently breaks every",
          file=sys.stderr)
    print("saved script. Append the new view with the next free code instead.",
          file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
