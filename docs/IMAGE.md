# MACVM Image Format — a versioned SQLite source database

A persistent, queryable store for classes and methods, designed so the GUI
class browser ([`gui/PLAN.md`](../gui/PLAN.md) G3, mocked today per
[`gui/smappl.md`](../gui/smappl.md)) has something real to browse and edit
before core eval (S5/S6) or the mirror layer (Phase W2) exist, and so
interactive edits — made by *clicking around in the browser*, not by hand-
editing `.mst` files in a text editor — have somewhere durable, versioned,
and undo-able to live.

**Status (S22):** image_store built; DB→VM boot loader and GUI live-compile+persist wiring landed.

## 0. This is not the S16 snapshot

Worth being precise about up front, since both are "persistence for the
world" and it would be easy to conflate them: [`SPEC.md`](SPEC.md) §3.2
commits to **no heap/memory image** — MACVM always boots by compiling
`world/*.mst` source text, and S16 (Phase E, stretch) is a *deferred*, fully
separate mechanism for freezing/restoring live VM heap state (oops, the
Universe root schema) — "no Rust vtables in-heap objects" is what makes that
possible at all.

The format below stores **text** (class/method source, as `String`s) plus
small metadata (load order, categories, version numbers) — no oops, no heap
pointers, no compiled-code-as-the-source-of-truth. Booting from it is still
"boot from source," exactly per §3.2 — just from SQLite rows instead of
flat files. It does not compete with S16 and does not need S16 to exist
first. Think of it as **`world/*.mst` plus `world.list`, reimplemented as a
queryable, versioned database** instead of flat files + a hand-maintained
manifest — a change of *container*, not a change of *what's stored or how
MACVM boots*.

## 1. Why not just keep editing `.mst` files?

[`WORLD.md`](WORLD.md) §11 is right that one-file-per-class flat text scales
fine for **hand-authoring and porting** (git diffs cleanly, greppable, is
what `tools/dlt2mst` will emit). It's a poor fit for exactly one thing this
project didn't need until now: **interactive editing from a running GUI**.
Three concrete gaps drove this design:

- **Reordering/inserting needs a stable, sparse index.** `world.list` is a
  hand-maintained text file in filename order (`01_object.mst`,
  `02_nil_boolean.mst`, …). Inserting a class between two others today means
  renumbering files. A gap-numbered integer column doesn't have that
  problem (§3).
- **Versioning a text file means shelling out to git.** That's fine for a
  human developer; it's not something the browser's Cmd+S handler
  (`gui/src/vm_host.rs`'s `BrowserSaveSource`, today writing into
  `macvm-mock-vm`'s in-memory `MockWorld`) can reasonably drive
  programmatically per keystroke-accept. A database table is the right
  tool for "keep the last 100 edits, show the latest, support undo" (§4).
- **The browser needs a query surface, not a text scanner.** "List every
  class in package X," "list this class's categories," "list methods in
  category Y" are exactly what SQL indexes are for; re-parsing `.mst` text
  on every click doesn't scale past a toy world.

None of this changes `WORLD.md`'s adoption strategy, the `.dlt→.mst`
converter, or the S6 seed world — see §7 for exactly how the two coexist.

## 2. Schema

```sql
CREATE TABLE classes (
    class_id    INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    load_order  INTEGER NOT NULL UNIQUE
);

-- One row per edit. "Latest" is never a stored pointer — always
-- MAX(version_number) for that class_id — so there is exactly one source
-- of truth for "what's current," not two things that can drift apart.
CREATE TABLE class_versions (
    version_id      INTEGER PRIMARY KEY,
    class_id        INTEGER NOT NULL REFERENCES classes(class_id),
    version_number  INTEGER NOT NULL,
    superclass_name TEXT,                    -- NULL only for Object
    category        TEXT NOT NULL DEFAULT '',-- package/group, e.g. "Collections"
    comment         TEXT NOT NULL DEFAULT '',
    instance_vars   TEXT NOT NULL DEFAULT '',-- space-separated, matches .mst's "| a b c |"
    class_vars      TEXT NOT NULL DEFAULT '',
    edited_at       INTEGER NOT NULL,        -- unix epoch seconds
    UNIQUE(class_id, version_number)
);
CREATE INDEX idx_class_versions_latest ON class_versions(class_id, version_number DESC);

CREATE TABLE methods (
    method_id  INTEGER PRIMARY KEY,
    class_id   INTEGER NOT NULL REFERENCES classes(class_id),
    selector   TEXT NOT NULL,
    side       TEXT NOT NULL CHECK(side IN ('instance','class')),
    UNIQUE(class_id, selector, side)
);

CREATE TABLE method_versions (
    version_id     INTEGER PRIMARY KEY,
    method_id      INTEGER NOT NULL REFERENCES methods(method_id),
    version_number INTEGER NOT NULL,
    category       TEXT NOT NULL DEFAULT 'as yet unclassified',
    source         TEXT NOT NULL,
    edited_at      INTEGER NOT NULL,
    UNIQUE(method_id, version_number)
);
CREATE INDEX idx_method_versions_latest ON method_versions(method_id, version_number DESC);

-- Optional, and never authoritative — see §5.
CREATE TABLE method_bytecode (
    method_version_id INTEGER PRIMARY KEY REFERENCES method_versions(version_id),
    bytecode           BLOB NOT NULL,   -- SPEC.md §4.4 CompiledMethod format
    literals_json      TEXT,            -- literal frame; JSON is fine, not perf path
    compiler_tag       TEXT NOT NULL,   -- invalidation key, see §5
    compiled_at        INTEGER NOT NULL
);
```

No separate "instance variable version" table: a class's instance-variable
list is one field of `class_versions`, so editing it (like editing the
comment, or the superclass) just creates a new whole-row class version —
matching how `.mst` already treats ivars as part of one class *definition*,
never independently edited. This is deliberately the smallest schema that
satisfies "classes, methods, **and members** are versioned": members version
*with* their owning class, not on their own axis.

## 3. Load order — sparse gap numbering

`load_order` starts at 1000 and steps by 100 (1000, 1100, 1200, …), same
idea as line numbers in old BASIC, or `LexoRank`-style ordering fields:
**insert** a class between two existing ones by picking the midpoint of
their `load_order` values (insert between 1000 and 1100 → 1050; between
1000 and 1050 → 1025; …). **Reorder** by updating one row's `load_order` in
place — no renumbering of anything else. **Rebalance** (a maintenance
operation, not needed per-edit) when two adjacent classes' gap has been
halved down to nothing (`load_order` values differing by 1): renumber the
whole table back to clean multiples of 100 in current order. `Universe::
genesis()`-provided classes (SPEC.md §3.2 step 1 — `Object`, `Metaclass`,
etc.) aren't rows here at all; they exist before any `.mst`/image loading
happens, exactly as today.

## 4. Versioning, retention, undo

- **Every save is an INSERT, never an UPDATE.** `BrowserSaveSource`
  (`vm_host.rs`) becomes: look up the next `version_number` for that
  `method_id`/`class_id`, insert a new `*_versions` row, done.
- **"Latest" is `ORDER BY version_number DESC LIMIT 1`.** The browser always
  reads this — never anything else — so there's no "did I remember to
  update the latest-pointer" bug class to have in the first place.
- **Retention: keep the last 100 versions per method/class.** After each
  insert, delete rows with `version_number <= (current_max - 100)` for that
  `method_id`/`class_id`. Simple sliding-window pruning, no separate GC pass.
- **Undo is revert-as-a-new-version, not delete-the-latest.** "Undo"
  restores an older version's content by inserting *another* new version
  copying it — never destructively removes the version the user is undoing
  *away from*. Two reasons: (a) it's what "always see the latest version"
  already implies — there is no separate mutable "head pointer" to move
  backward, just the one `MAX(version_number)` rule from above, so undo
  doesn't need special-case read logic; (b) an accidental undo is itself
  undo-able (undo the undo), which delete-the-latest can't offer once the
  deleted row is gone. This does mean undo consumes one of the 100 retained
  slots — an explicit, worthwhile trade for never silently losing an edit.

## 5. Bytecode — optional, never authoritative

`method_bytecode` is a **cache**, keyed to one exact `method_version_id`,
tagged with a `compiler_tag` identifying the compiler build that produced
it. On boot: if a row exists for the current method version *and* its
`compiler_tag` matches the running compiler's, load the bytecode directly
(skip recompiling that method); otherwise (row missing, or `compiler_tag`
stale — e.g. after a compiler change) recompile from `source` and, if
`MACVM_CACHE_BYTECODE` is enabled, write a fresh row. Source is **always**
sufficient on its own; the cache only ever saves recompile time, never
carries information `source` doesn't. This keeps faith with SPEC.md §3.2's
"v1 always boots from source" — a bytecode cache that's allowed to be wrong
and silently regenerated isn't an image in the S16 sense, just a compile-
avoidance table (analogous to Rust's own incremental-compilation cache).
Not needed for the browser to work at all; build it only once boot time on
a large world actually matters.

## 6. Where this lives, and the import step

New workspace crate `image_store` (root-level, alongside `gui/` — both the
real `macvm` binary and `macvm-gui` will eventually depend on it, so it
doesn't belong nested under `gui/` the way `gui/mock_vm` does, since that
one really is GUI-only test scaffolding). Owns:

- Schema creation/migration (`schema.sql`, applied on first open).
- A versioned CRUD API mirroring `macvm-mock-vm`'s shape (`packages()`,
  `package_roots()`, `subclasses_of()`, `categories()`, `methods_in()`,
  `method_source()`, `set_method_source()`, `set_class_comment()`, plus
  `undo_method()`/`undo_class()` and `insert_class_at()`/`reorder_class()`
  for load order) so the GUI browser can point at *either* the mock or a
  real image with minimal glue — see §8.
- A `.mst` importer (`world/*.mst` → rows, `import::import_world_dir`), used to
  (re)seed an image from the hand-authored world. **The `.mst` files stay the
  checked-in source of truth** — the importer is repeatable. Re-seeding an
  existing image is incremental: methods are re-imported latest-wins, and a
  reopened class's declared vars are **merged** (`reimport_class_shell` unions
  instance/class vars — a method-only reopen never wipes an existing
  `<classVars: …>`). Removing a var, renaming, or changing a superclass needs a
  **fresh** rebuild (delete the image first). The `./reseed-world.sh` script and
  [`managingtheworld.md`](managingtheworld.md) wrap this workflow (build + fresh
  reseed + boot-check); reach for a fresh reseed after any class-shell change.
- An **exporter** (image → `world/*.mst`, `export::export_world_dir`,
  `macvm-gui export --world world`) round-trips interactive edits back into
  git-diffable text.

## 7. Relationship to `WORLD.md` and `SPEC.md` §3.2 (updated below)

- `WORLD.md` §11's "one class per file… keep it" stands for **authoring and
  porting** — nothing here changes the `.dlt→.mst` converter, the layer-A/
  B/C adoption strategy, or how the S6 seed world is written and reviewed.
- `SPEC.md` §3.2 step 2 ("Load `world/*.mst` in a declared order") gets a
  second, equivalent option once this lands: load from an `image_store`
  SQLite file instead, in `load_order` order, two-pass exactly as `WORLD.md`
  §11 already specifies (declare all classes, then compile all methods) —
  the loader's *shape* doesn't change, only where the ordered (class,
  source) pairs come from.
- `docs/APPS.md` §5.2's `MethodNode` accept path (`mirror
  addMethod:category:ifFail:`) is the **live, in-VM** counterpart to
  `set_method_source` here — once G2/W3 land, a real accept both compiles
  into the live heap *and* should persist to the image, so a restart
  doesn't lose the edit. That mirror-layer wiring is still future work
  (needs the real mirror layer). The mock browser used to call
  `set_method_source` directly with no compiler involved, exactly like
  `macvm-mock-vm` did; as of S22 the GUI live-compiles edits via `vm.exec`
  and persists them to the `image_store` DB.
- `SPEC.md` A16 (`Smalltalk methodSources` IdentityDictionary, the **live**
  in-heap source registry) is populated *from* this image at boot, the same
  way it's populated from `.mst` text today — this format is upstream of
  A16, not a replacement for it.

## 8. GUI wiring (this pass)

`gui/src/vm_host.rs`'s worker thread currently owns a `macvm_mock_vm::
MockWorld` directly. To point the browser at a real image without ripping
out the mock (still useful: zero-setup UI testing, exactly its purpose per
its own doc comment):

- A small trait, `ClassWorld`, capturing exactly the query/mutation shape
  `browser_render.rs` calls today (`packages`, `package_roots`,
  `subclasses_of`, `categories`, `methods_in`, `method_source`,
  `set_method_source`, `set_class_comment`) — implemented by both
  `macvm_mock_vm::MockWorld` and `image_store`'s image type.
  `browser_render.rs`'s render functions become generic over `impl
  ClassWorld` instead of the concrete `MockWorld`.
- `vm_host::spawn` picks which backing store to use once at startup: an
  `image_store` file if `MACVM_IMAGE_PATH` is set (and openable), else the
  existing invented `MockWorld` seed — falling back rather than failing
  keeps the "just run it, no setup" property the mock was built for.

## 9. What this pass does *not* do

- No compiled-tier interaction (S13 deopt/dependency invalidation is
  unaffected — the bytecode cache in §5 is purely a boot-time convenience).
- No multi-user/concurrent-writer story — one image file, one GUI process,
  same as everything else in this project today.

*(Update, later passes: the GUI now boots the real VM from the image (S22, not
the mock), live-compiles browser edits and persists them, and the image →
`.mst` exporter is built (§6). See [`managingtheworld.md`](managingtheworld.md)
for the operational reseed/export workflow.)*
