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
    """[name] in live registration order, read out of the world source."""
    ui = pathlib.Path(world_dir) / "64_cocoaui.mst"
    src = ui.read_text(encoding="utf-8")
    # `CocoaUI class >> startup` ends at the next class-side method definition.
    m = re.search(r"CocoaUI class >> startup\b", src)
    if not m:
        raise SystemExit(f"{ui}: no `CocoaUI class >> startup`")
    body = src[m.end():]
    end = re.search(r"\n    CocoaUI class >> \w", body)
    if end:
        body = body[: end.start()]

    # Map each view class to the symbol it registers, from its own file.
    registers = {}  # class name -> view symbol
    for path in sorted(pathlib.Path(world_dir).glob("*.mst")):
        text = path.read_text(encoding="utf-8")
        for cls, sym in re.findall(
            r"(\w+) class >> register\b.*?registerViewNamed: #(\w+)", text, re.S
        ):
            registers.setdefault(cls, sym)

    views = []
    # Walk `start` top to bottom; both shapes register, in the order written.
    for mm in re.finditer(
        r"self\s+registerViewNamed:\s*#(\w+)"
        r"|\(Worker classNamed: #(\w+)\)\s*ifNotNil:",
        body,
    ):
        direct, cls = mm.group(1), mm.group(2)
        if direct:
            views.append(direct)
        elif cls in registers:
            views.append(registers[cls])
    return views


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
