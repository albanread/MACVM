# Package-aware browsing and editing — design

**Status: design, not built.** Written in response to a direct requirement:
the image database must contain the Cocoa GUI's own Smalltalk source (not
just the base world), the main browser must be able to show and edit all of
it, and accepting an edit must never push a live recompile into a VM that
doesn't have that source's package loaded. Revised once, in response to
further direction: every VM should boot from the database rather than raw
`.mst` files, each loading only the world plus the packages it actually
needs — which means package-list membership (today: which `.mst` files
`world.list`/`cocoaui.list` name) has to become a database concept in its
own right, not just a fact about which files got imported (§4.5). This
document investigates the current architecture against both — with exact
file/line citations, not assumptions — and proposes a solution. No code
changes yet.

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
filter. This matters for §4.5.

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

### 4.5 Resolved: package lists move into the database; every VM boots from it, selectively

Revision, superseding this section's earlier draft (which left DB-boot's
selectivity as an open question and recommended deferring it). Direction
from the user: **everything should boot from the database, each VM loading
only the world plus the packages it actually needs — and package-list
membership itself should be a database concept, not just a fact about which
`.list` file happened to get imported.**

This is a better answer than the live-existence check in §4.4 alone would
give, because it makes "does this VM have package X" a fact that's true **by
construction** (a VM that boots from package list `"world"` structurally
cannot have any `cocoaui`-package class defined — nothing ever asked the
compiler to define one) rather than incidentally true (today's Cocoa GUI
primary just happens not to load `cocoaui.list`, which is one specific
binary's boot code doing the right thing, not a property the database
itself understands or enforces). §4.4's mechanism (fold the check into the
live-compile doit, evaluated on the target VM) stays correct and stays the
right implementation — it's now grounded in a guaranteed invariant instead
of an incidental one.

**Schema addition** — two small tables, no change to anything existing:

```sql
CREATE TABLE IF NOT EXISTS package_lists (
    list_id  INTEGER PRIMARY KEY,
    name     TEXT NOT NULL UNIQUE          -- "world", "cocoaui"
);
CREATE TABLE IF NOT EXISTS package_list_members (
    list_id  INTEGER NOT NULL REFERENCES package_lists(list_id),
    package  TEXT NOT NULL,                -- matches class_versions.category
    UNIQUE(list_id, package)
);
```

No per-member ordering column: `classes.load_order` (already the historical,
dependency-safe import order — `world_boot.rs`'s own doc comment establishes
this: "`all_classes()` returns classes in dependency-safe `load_order`") is
already the correct sort key across *any* subset of packages, since it's a
single global sequence assigned once at import time regardless of which file
or package a class came from. A "list" is genuinely just a **named set of
packages** — boot order for any combination of lists falls out of the
existing column for free.

**Population, at import time.** `import_list_file` (§4.1) already knows which
`.list` file it's processing and already derives each file's package name
via `package_name_from_filename`. Extend it to also record, per file, that
the list being imported (its own name derived the same way — `"world.list"`
→ `"world"`, `"cocoaui.list"` → `"cocoaui"`) contains that package —
`Image::ensure_package_list_member(list_name, package)`, an idempotent
insert. The `.list` files on disk remain the checked-in source of truth for
the *initial* membership (exactly as they already are for classes and
methods); after import, the database rows are what boot actually reads, and
they can evolve independently from there — a browser feature to move a
package between lists, should anyone want one, is just another edit to
`package_list_members`, versioned the same casual way everything else in the
image already is.

**Boot becomes list-driven.** A new `Image::classes_for_lists(&[&str]) ->
Vec<FullClass>` — join `package_list_members` (by list name, unioned across
however many list names the caller asks for) against
`classes`/`latest_class_versions` on `category = package`, ordered by
`classes.load_order`, same shape as `all_classes()` today just filtered.
`load_world_from_image` (`gui/src/world_boot.rs:126`) takes a `&[&str]` of
requested list names instead of unconditionally calling `all_classes()`.

**Who requests which lists** (each VM boots "the world plus what it needs,"
per the direction — nothing here is forced to request everything just
because everything now exists in the database):

| VM | Requested lists | Why |
|---|---|---|
| `macvm-cocoa` primary | `["world"]` | persistent environment; never runs AppKit UI code, so `cocoaui`'s classes would be dead weight there — same role split as today, just now via the DB |
| `macvm-cocoa` UI worker | `["world", "cocoaui"]` | needs its own implementation live to run at all |
| `macvm-gui` | `["world"]` | no Cocoa-GUI-specific role; open to revisiting if a reason turns up to want `cocoaui` visible there too |
| compute workers (`Worker spawn:`) | whatever the spawning VM specifies (defaults to its own list set) | unchanged in spirit from today — a worker inherits its parent's boot shape |

**`world_boot.rs` has to move before `macvm-cocoa`'s primary can use it at
all — this isn't optional polish, it's a prerequisite.** It lives in
`gui/src/` today, and `gui` is a `[[bin]]`-only crate (confirmed:
`gui/Cargo.toml` declares no `[lib]` target, and `cocoa_gui/Cargo.toml` does
not depend on `gui`) — there is no `[lib]` target for `cocoa_gui` to depend
on even if it wanted to. So the M3 switch is not "point the primary's boot
call at a list-name slice instead of `all_classes()`" — that function is
structurally unreachable from `cocoa_gui` as things stand. Move the
"replay an image into a `VmHandle`" logic out of `gui/src/world_boot.rs`
and into `image_store` itself, as its own module. That direction is sound:
`image_store` may depend on `macvm`'s `VmHandle` freely — the constraint
`world_boot.rs`'s own header comment states only ever ran one way, *"the
[core] VM stays free of a SQLite dependency,"* not the reverse. Both `gui`
and `cocoa_gui` then call the same function. This is a pure extraction (no
behavior change) for `macvm-gui`, provable by its own existing boot/seed
tests passing unchanged against the moved code before anything new is built
on top of it — do it as its own early milestone (M2 below), not folded into
the bigger M3 change.

**The bare CLI (`macvm run`/`macvm repl`/`macvm rusttcl`) is a different,
smaller decision, and there's a real reason to leave its *default* alone.**
`VmHandle::boot` (`src/embed.rs:477`) direct-from-`.mst` is what makes
`world_image_installs_all_classes_without_error`-style tests possible at
all — an independent, DB-free "does the checked-in source itself still
boot" ground truth to catch DB-boot drifting from it, which the differential
philosophy this whole project runs on (`reference-instructions-are-not-time`,
the interpreter-is-the-oracle rule, etc.) actively wants to keep around, not
retire. Once M2's shared replay logic exists, giving the CLI an *additional*,
opt-in `--from-image` boot path is a small, low-risk add — but its default
should stay `.mst`-direct on purpose, not as leftover scope. Listed as a
**later, optional milestone** (M6 below).

The WKWebView GUI's own `BrowserSaveSource` handling gets the equivalent
§4.4 gate for the same reason as before (§3's bug shape doesn't care which
GUI shipped the doit) — now doubly justified, since `macvm-gui` requesting
only `["world"]` means it will *also* genuinely exercise the "not loaded"
path once `cocoaui`-package content exists in the database, which wasn't
true under the old "DB-boot loads everything" default.

## 5. Non-goals for v1

- No UI for choosing which package a brand-new interactive class belongs to
  beyond the synthetic `"interactive.mst"` default (§4.2) — a real picker is
  a follow-up once the plumbing exists.
- No change to `export_world_dir` in this pass — writing `cocoaui.list`'s
  classes back out to their `.mst` files on disk is a separate, smaller
  extension of the same file-scoping idea once import is generalized, not
  bundled here.
- No UI for editing package-list membership itself (moving a package between
  lists, defining a brand-new list) — the schema and the import-time
  population support it, but a browser affordance for it is a later ask.
- The bare CLI's *default* staying on `.mst`-direct boot (§4.5) — a
  deliberate choice (it's the DB-free ground truth other tests check
  against), not unresolved scope; an opt-in `--from-image` path is the
  small, later M7.

## 6. Milestone ladder

| | Deliverable | Proof |
|---|---|---|
| **M1** | `import_list_file` / `import_all_lists`; `package_lists` / `package_list_members` schema + population at import time; `cmd_seed` switched over; `reseed-world.sh` rebuilt and verified | `world/image.sqlite3` contains `CocoaHelp` etc.; `Image::packages()` lists `"cocoaui"`; `package_list_members` correctly maps `"cocoaui"` → the `cocoaui.list` package set; existing importer tests still pass unchanged (`world.list`-only behavior is the same code path, just parameterized) |
| **M2** | Extract `gui/src/world_boot.rs`'s replay logic into `image_store` (pure move, no behavior change) — the prerequisite §4.5 identifies | `macvm-gui`'s own existing boot/seed tests pass unchanged against the moved code; `cocoa_gui` can now reach it (compiles against it, not yet wired to anything) |
| **M3** | `source_file` provenance for interactively-created classes/methods (§4.2) | a class created via the browser has a non-`NULL` derivable `class_home_file` |
| **M4** | `Image::classes_for_lists`; the moved replay fn takes a list-name slice; `macvm-gui` and `macvm-cocoa`'s primary switch to DB-boot requesting `["world"]`; the UI worker requests `["world", "cocoaui"]` | each VM's live `ClassMirror allClasses` sweep matches exactly the classes in its requested lists — no more, no less; existing boot-time tests (`world_image_installs_all_classes_without_error` and friends) still pass against the now-selective boot |
| **M5** | The live-compile gate (§4.4) in `CocoaEditor`/`CocoaBrowser`, and the WKWebView GUI's equivalent | editing a `CocoaHelp` method while connected to the primary (which requests `["world"]` only) saves to the database and reports "not applied live" — verified by driving the real app (`MACVM_COCOA_CTL`), not assumed; a normal base-world edit is unaffected; same proof, other GUI |
| **M6** | Database-only rows in the browser (§4.3) | `CocoaHelp` is visible and its source opens for editing from a fresh Cocoa GUI session that never touched it any other way |
| **M7** | Optional, later: the bare CLI's opt-in `--from-image` boot path | `macvm run --world world --from-image` boots identically to the `.mst`-direct default |

M1–M5 carry the real risk and the actual bug fix (§3); M6 is mostly mapping
over a proven base; M7 is optional and explicitly deferred — matching this
project's own convention for staging a build (`cocoa_gui_design.md` §10's
shape).
