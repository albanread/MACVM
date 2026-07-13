# Managing the world

How to change MACVM's Smalltalk world (`world/`) without breaking the GUI, plus
the scripts and tests that keep it honest. Read this before editing a world
class — one gotcha here (a stale image) is what makes edits "not show up," or
the GUI come up with blank editors and no VM.

## The one rule

**The GUI boots from `world/image.sqlite3`, NOT from `world/*.mst`.**

So there are two representations of the world:

| | | |
|---|---|---|
| `world/*.mst` | the **source of truth** | committed to git; edited by hand |
| `world/image.sqlite3` | the **image** the GUI boots from | git-ignored, a build artifact; rebuilt from the `.mst` by `seed` |

Editing a `.mst` does nothing to a running GUI until you **reseed** the image.
(The CLI `macvm run --world world` reads the `.mst` directly, so a headless run
can look fine while the GUI is stale — don't be fooled.)

## Golden rules

1. **After changing any world class, reseed** — `./reseed-world.sh`.
2. **Boot-check the image** — `reseed-world.sh` does this for you. Never assume a
   reseed produced a bootable image; a single class that won't compile makes the
   whole VM boot fail, and then the GUI has no VM (blank browser, blank editors).
3. **Prefer a fresh rebuild** (delete + reseed) over an in-place merge when you
   change a class's *shell* (superclass, instance vars, class vars) — see below.
4. **`.mst` is the source of truth.** GUI edits live in the image; `export` them
   back to `.mst` before committing, or they're lost on the next fresh reseed.

## Everyday workflows

### Change a method body, or add/remove a method
Edit the `.mst`, then:
```sh
./reseed-world.sh          # rebuild + boot-check
./run-gui.sh               # Demos, browser, etc. now see the change
```
An incremental reseed (`./reseed-world.sh --keep`) is fine for method-only
changes — methods are always re-imported latest-wins.

### Add a new class
Add the class to a `world/NN_name.mst`, add that file to `world/world.list`
(load order matters — a file may only reference globals/classes bound earlier),
then `./reseed-world.sh`.

### Change a class's shell — superclass, instance vars, or **class vars**
This is the sharp edge. Do a **fresh** rebuild:
```sh
./reseed-world.sh          # (fresh is the default — it deletes the image first)
```
Why: an *incremental* reseed **merges** shell vars — it can *add* a
`<classVars: …>` you introduced, but it can't *remove* one, rename a class, or
change a superclass, because a class is often reopened in the same file to add
methods **without** restating its vars, and a reopen must not wipe them. A fresh
rebuild reconstructs the shell exactly from source. (This exact bug — a class var
that got wiped on reopen, so image boot failed with `undeclared variable` and the
GUI came up blank — is why this document exists.)

### Add a top-level doit or global (e.g. `Foo := Bar new.`)
Top-level doits are **not yet captured in the image** (S22-B is pending). The GUI
boot runs a small hardcoded list (`WORLD_DOITS` in `gui/src/vm_host.rs`:
`Transcript := TranscriptStream new.`, `Character initTable.`). If your class
needs a new global set at boot, add it to that list too, or hold the state in a
class variable instead (which the image *does* persist).

### Round-trip GUI edits back to source
The class browser writes edits into the image (and the live VM). To get them
into `.mst` so they can be committed:
```sh
./target/release/macvm-gui export --world world   # image -> world/*.mst
git diff world/                                    # review, then commit
```

## The commands

```sh
macvm-gui seed   --world world   # world/*.mst  -> world/image.sqlite3 (incremental/merge)
macvm-gui export --world world   # world/image.sqlite3 -> world/*.mst  (round-trip GUI edits)
```
`seed` on an existing image is incremental (merges); on a missing image it does a
full import. `reseed-world.sh` deletes first for a clean full import by default.

## The scripts

- **`./reseed-world.sh`** — build, rebuild `world/image.sqlite3` from `world/`
  *from scratch*, and boot-check it. The everyday "I changed the world" command.
  - `--keep` — incremental reseed (merge into the existing image) instead of a
    clean rebuild. Fine for method-only changes.
  - `--no-verify` — skip the boot check (faster; not recommended).
- **`./run-gui.sh`** — build and launch the GUI (which boots from the image).

## The guardrails (tests)

Run the whole gui suite after any `image_store` or `world_boot` change — a
filtered `grep` can hide a real failure:
```sh
cargo test -p macvm-gui
```
Two tests specifically protect the world image; both must stay green:

- `world_boot::tests::world_image_installs_all_classes_without_error` — seeds a
  fresh image from `world/` and boots it. Fails loudly if any class won't
  install (the boot-guard `reseed-world.sh` also runs).
- `world_boot::tests::db_booted_world_matches_mst_booted_world` — the
  **differential**: a world booted from the image must behave identically to one
  booted from the `.mst`. This is the test that catches a shell/var round-trip
  bug; it would have caught the blank-editors regression immediately.

## Troubleshooting

**GUI editors are blank / the browser shows classes but no source.**
The VM didn't boot. Look at the GUI's transcript (bottom pane) or reproduce
headlessly:
```sh
cargo test -p macvm-gui --bin macvm-gui world_image_installs_all_classes_without_error -- --nocapture
```
A message like `installing methods of X: … undeclared variable 'Y'` means class
`X` references `Y` that isn't declared — almost always a class/instance var that
didn't survive into the image. Fix: `./reseed-world.sh` (fresh). If it persists,
check the stored shell:
```sh
sqlite3 world/image.sqlite3 \
  "SELECT c.name, lcv.instance_vars, lcv.class_vars \
   FROM latest_class_versions lcv JOIN classes c ON lcv.class_id=c.class_id \
   WHERE c.name='X';"
```

**"undeclared variable" for a class defined twice in one file.**
A class can appear as a base definition (`Magnitude subclass: Character [ <classVars: Table> … ]`)
and later a method-adding reopen (`Magnitude subclass: Character [ … ]`) in the
same file. The reopen must not restate the vars, and the importer must not wipe
them (it merges). A fresh reseed rebuilds the correct union.

## Reference

- `world/*.mst` — the world source; `world/world.list` — load order.
- `world/image.sqlite3` — the image (git-ignored); rebuilt by `seed`.
- `image_store/` — the SQLite image: `import` (`.mst`→image), `export`
  (image→`.mst`), `reimport_class_shell` (merge a reopened class's vars).
- `gui/src/world_boot.rs` — reconstructs + boots the world from the image.
- `gui/src/vm_host.rs` — `WORLD_DOITS`, the boot path, and the browser's
  write-through to the image.
- [`docs/IMAGE.md`](IMAGE.md) — the image-store design.
