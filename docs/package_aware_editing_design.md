# Package-aware browsing and editing — design

**Status: design, not built.** Written in response to a direct requirement:
the image database must contain the Cocoa GUI's own Smalltalk source (not
just the base world), the main browser must be able to show and edit all of
it, and accepting an edit must never push a live recompile into a VM that
doesn't have that source's package loaded. This document investigates the
current architecture against that requirement — with exact file/line
citations, not assumptions — and proposes a solution. No code changes yet.

## 1. The problem, precisely

MACVM has more than one Smalltalk source *tree* today:

- **The base world** — `world/world.list` (files 01–62), loaded by every VM:
  the CLI, `macvm-gui`'s single VM, and both VMs `macvm-cocoa` boots (primary
  and UI worker).
- **The Cocoa GUI's own implementation** — `world/cocoaui.list` (files 63+):
  `CocoaUI`, `CocoaBrowser`, `CocoaFind`, `CocoaEditor`, `CocoaOutliner`,
  `CocoaCanvas`, `CocoaHelp`, `MacvmDelegate`, `ObjcMainProxy`. Loaded **only**
  by `macvm-cocoa`'s **UI worker** — never the primary, never `macvm-gui`,
  never the CLI (`world/cocoaui.list`'s own header comment is explicit about
  this).

Both are real Smalltalk source someone may want to browse, fix a bug in, or
extend — `CocoaHelp` is exactly as legitimate a target for the class browser
as `OrderedCollection`. Today it is invisible to it, and — as §3 shows —
attempting to edit it anyway is actively unsafe, not merely unsupported.

## 2. Current architecture (verified against source)

**Import is `world.list`-only.** `image_store::import::import_world_dir`
(`image_store/src/import.rs:27`) hardcodes `world_dir.join("world.list")` —
there is no path by which `cocoaui.list`'s files ever reach
`world/image.sqlite3`. `cmd_seed` (`gui/src/main.rs:379`) and
`reseed-world.sh` both call exactly this. **The database the browser reads
source from does not contain the Cocoa GUI's own classes at all.**

**Browser rows come from live VM reflection, not the database, in *both*
GUIs.** `UiBrowserService class >> browseSnapshot` (`world/34_tools.mst:585`)
walks `(ClassMirror on: Object) subclassMirrors` — a live sweep of whatever
classes the **primary** VM has loaded right now. The WKWebView GUI's own
`ClassHierarchyOutliner` (`world/34_tools.mst:170`) does the identical
`subclassMirrors` walk against its own VM — `UiBrowserService`'s own comment
calls this out explicitly: "the SAME projection the WKWebView outliner
renders... so the two GUIs browse identical rows." Since `cocoaui.list` is
never loaded into a primary, its classes can never appear as rows, in either
GUI, regardless of what the database contains.

**Accepting an edit always live-compiles, unconditionally, in both GUIs.**
Three save paths, all the same shape:

- `CocoaEditor class >> save` (`world/68_cocoaeditor.mst:157`): `Worker
  uiDoit: text onReply: [...]` runs first, unconditionally, then the database
  write.
- `CocoaBrowser class >> acceptMethod` (`world/66_cocoabrowser.mst:507`):
  saves to the image, then unconditionally builds `ShellSuper , '
  subclass: ' , cls , ' [' , text , ']'` and ships it as a live-compile
  `uiDoit`.
- The WKWebView GUI's own `BrowserSaveSource` handling
  (`gui/src/vm_host.rs`, `live_compile` at line 1128) follows the identical
  pattern — `image_store::flows`'s own doc comment (`flows.rs:8`) states the
  boundary explicitly: *"LIVE-compile is deliberately NOT here: each GUI owns
  its path to its running VM."* Nobody, in either GUI, currently asks "does
  the target VM even have this class" before compiling into it.

**DB-boot (`load_world_from_image`, `gui/src/world_boot.rs:126`) loads every
class in the database, unconditionally** — `Image::all_classes()`, no
filter. This matters for §5.

## 3. Why this is a real bug, not just a missing feature

Trace `CocoaBrowser class >> acceptMethod` for a method on a class the
primary has never loaded (true of every `cocoaui.list` class today, and true
of any class in a database extended per §4 whose package a given VM skipped).
Line 520:

```smalltalk
doit := ShellSuper , ' subclass: ' , cls , ' [' , String lf , text , String lf , ']'.
```

This is a **reopen** shape — no instance-variable declaration, because a
reopen assumes the class already exists with its real shape and is only
adding one method. If `cls` does **not** already exist in the primary,
`subclass:` does not fail — it **defines a new class** under that name, with
`ShellSuper` as its superclass and **zero of its real instance variables**,
holding just the one edited method. That shell now lives, silently, in the
wrong VM, under the same name real code elsewhere expects to have its actual
shape. The existing `r = 'nil' ifFalse: [...]` error path (line 526) does not
catch this, because nothing raised — the doit *succeeded*, just not at
defining what the user thinks they edited.

This is the concrete failure mode the design in §4 closes, not a
hypothetical.

## 4. Design

### 4.1 Import: read every `.list` file, not just `world.list`

Generalize the importer to a list-file parameter, and add an "import
everything" entry point:

```rust
// image_store/src/import.rs
pub fn import_list_file(image: &Image, world_dir: &Path, list_file: &str) -> Result<ImportStats, String>;
// import_world_dir(image, dir) becomes a thin wrapper: import_list_file(image, dir, "world.list")

pub fn import_all_lists(image: &Image, world_dir: &Path) -> Result<ImportStats, String> {
    // "world.list" first (cocoaui.list's classes may reference base-world
    // globals), then every other *.list file in world_dir, sorted — so a
    // future third app-specific list needs no code change here.
}
```

`cmd_seed` switches to `import_all_lists`; `reseed-world.sh`'s output message
updates to say so. **No schema change is needed for package identity** — it
already exists: `import_list_file` calls the existing
`package_name_from_filename` (`import.rs:145`) per file (`"64_cocoaui.mst"`
→ `"cocoaui"`), which already lands in `class_versions.category`, which the
already-existing `Image::packages()` / `Image::package_roots()`
(`lib.rs:286`, `:305`) already group by. Importing `cocoaui.list` makes
`packages()` start answering `"cocoaui"` alongside the base world's package
names, for free.

### 4.2 Provenance for classes created interactively

`image_store::flows::new_class_from_source` / `save_method` (`flows.rs:42`,
`:77`) never call `set_method_home_file` — a class created through the
browser today gets no `source_file` on any of its methods, hence no
inferable package (`class_home_file`, `lib.rs:670`, returns `None`). Add an
optional `default_source_file: Option<&str>` parameter to both, set by the
method's own `set_method_home_file` call exactly as the importer already
does. The caller passes whichever `.mst`-equivalent identity fits the
context — a real filename if the browser is scoped to one, or a synthetic
marker like `"interactive.mst"` (its own one-file "package," category
`"interactive"`) if not. Either way every method gets *some* home, so §4.4's
existence check has something to reason about even for brand-new content.

### 4.3 Browser rows: merge live reflection with database-only entries

Keep `UiBrowserService`/`ClassHierarchyOutliner`'s live sweep exactly as it
is — it is correct and cheap for what it answers ("what does *this* VM
actually have loaded"), and other tooling may come to depend on that
property. Add a second, database-only source alongside it rather than
replacing it:

- A new host-side query (Cocoa GUI: `host_service.rs`, mirroring the
  existing read-only `Image::open_read_only` pattern CG7's source pane
  already uses; WKWebView GUI: the equivalent `vm_host.rs` handler) that
  answers `Image::packages()` grouped into `Image::package_roots(pkg)` /
  `methods_in(...)` — pure DB reads, no VM involved, matching how the source
  pane already reads host-side rather than through the VM.
- The browser tree gains a "Packages" grouping (or a mode toggle — the exact
  widget is a build-time decision, not a design one) built from that query,
  alongside the existing live class-hierarchy tree.
- Cross-reference the two: a row's class name present in the live sweep's
  name set is annotated live (editable-and-immediately-applied); a
  database-only row is annotated not-loaded-here (editable, saves to the
  database, per §4.4 skips the live-compile step honestly rather than
  attempting and silently misfiring per §3).

This is additive to the existing tree, not a rewrite of it — lower risk, and
the "do the two GUIs' browsers show identical rows" differential property
survives unchanged for the live-tree view; it simply doesn't apply to the new
database-only view (which is deliberately not live-VM-relative at all).

### 4.4 Gate live-compile on "does the target VM already have this class"

The check has to run **on the VM the doit is actually shipped to** (the
primary, for both `CocoaEditor` and `CocoaBrowser`), not on the UI worker
where this Smalltalk code happens to be executing — `Worker class >>
classNamed:` (`world/47_worker.mst:166`) resolves against "this worker's
world," i.e. whichever VM is running it *at the moment it runs*, so a local
call from UI-worker code answers the wrong VM's question. The fix is to fold
the check into the **same** doit string already being shipped over `Worker
uiDoit:onReply:`, so it evaluates on the primary as part of the one round
trip — no new RPC verb, no new primitive, no check/act race:

```smalltalk
"CocoaBrowser class >> acceptMethod, reworked:"
doit := '(Worker classNamed: #', cls, ') isNil ifTrue: [ ''<<not-loaded>>'' ] ifFalse: [ ',
        ShellSuper, ' subclass: ', cls, ' [', String lf, text, String lf, ']. nil ]'.
Worker uiDoit: doit onReply: [ :r |
    r = 'nil'
        ifTrue: [ CocoaUI appendTranscript: 'browser: saved ', cls, '>>', sel, ' (image + live)' ]
        ifFalse: [ r = '''<<not-loaded>>'''
            ifTrue: [ CocoaUI appendTranscript: 'browser: saved ', cls, '>>', sel,
                             ' to the image — primary does not have this package loaded, not applied live' ]
            ifFalse: [ CocoaUI appendTranscript: 'browser: saved ', cls, '>>', sel,
                             ' to the image, but live compile failed: ', r ] ] ].
```

The gate applies to **editing an existing class's method** — `CocoaEditor
save` and `CocoaBrowser acceptMethod` — because that's the shape §3's bug
lives in (a reopen assuming the class already has its real shape). It does
**not** apply to `CocoaBrowser acceptClass` (creating a genuinely new class):
a new class's superclass either resolves on the target VM (in which case
compiling it there is exactly what a user creating new code while connected
to that VM wants — unchanged from today) or it doesn't, which already
produces an honest compile error through the existing `r = 'nil' ifFalse:`
path — not the silent-wrong-shape failure §3 describes, so there's nothing
new to guard there.

### 4.5 Cross-GUI consistency, and an open question about DB-boot

The WKWebView GUI's own `BrowserSaveSource` handling should get the
equivalent gate for the same reason (§3's bug shape doesn't care which GUI
shipped the doit). But note an asymmetry worth deciding explicitly, not
silently: `load_world_from_image` (§2) loads **every** class in the database
unconditionally. Once §4.1 lands, the WKWebView GUI's single VM would
therefore always end up with `cocoaui`'s classes compiled in at boot — inert
(no run loop to invoke `CocoaUI startup` there) but always "loaded," which
means §4.4's gate would never actually trigger for that GUI. Only
`macvm-cocoa`'s primary — which deliberately boots from raw `.mst` +
`world.list` only, never the image — genuinely exercises the "not loaded"
case today.

Two honest options, not resolved here: leave DB-boot as "load everything"
(simplest; the extra classes are harmless dead weight, and the gate still
does real work for the Cocoa GUI's primary, which is the case that actually
motivated this) — or make DB-boot selective (a `--packages world,cocoaui`
style boot filter), which matters once a *third* list exists that some VM
deliberately should not carry. Recommend deferring the second until there's
a concrete third package to test it against, and shipping the gate
universally either way — it's correct and harmless even where it never
fires.

## 5. Non-goals for v1

- No UI for choosing which package a brand-new interactive class belongs to
  beyond the synthetic `"interactive.mst"` default (§4.2) — a real picker is
  a follow-up once the plumbing exists.
- No change to `export_world_dir` in this pass — writing `cocoaui.list`'s
  classes back out to their `.mst` files on disk is a separate, smaller
  extension of the same file-scoping idea once import is generalized, not
  bundled here.
- No attempt to make DB-boot selective (§4.5) — flagged, not built.

## 6. Milestone ladder

| | Deliverable | Proof |
|---|---|---|
| **M1** | `import_list_file` / `import_all_lists`; `cmd_seed` switched over; `reseed-world.sh` rebuilt and verified | `world/image.sqlite3` contains `CocoaHelp` etc.; `Image::packages()` lists `"cocoaui"`; existing importer tests still pass unchanged (`world.list`-only behavior is the same code path, just parameterized) |
| **M2** | `source_file` provenance for interactively-created classes/methods (§4.2) | a class created via the browser has a non-`NULL` derivable `class_home_file` |
| **M3** | The live-compile gate (§4.4) in `CocoaEditor`/`CocoaBrowser` | editing a `CocoaHelp` method while connected to the primary (which doesn't load `cocoaui.list`) saves to the database and reports "not applied live" — verified by driving the real app (`MACVM_COCOA_CTL`), not assumed; a normal base-world edit is unaffected |
| **M4** | Database-only rows in the browser (§4.3) | `CocoaHelp` is visible and its source opens for editing from a fresh Cocoa GUI session that never touched it any other way |
| **M5** | The WKWebView GUI's equivalent gate | same M3 proof, other GUI |

M1–M3 carry the real risk and the actual bug fix; M4–M5 are mostly mapping
over a proven base, matching this project's own convention for staging a
build (`cocoa_gui_design.md` §10's shape).
