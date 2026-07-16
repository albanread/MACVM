# Save-time class shape migration (a limited, VM-internal `become:`)

**Status:** design only — nothing built. Captures the design worked out in
conversation 2026-07-16.

## Goal

Lift the "cannot change shape of existing class" limitation — for the **paused**
VM, at **save** time only. Today editing a class's instance variables can only
take effect on the next boot (the image is the boot source; there is no
`become:` to migrate live instances — see the no-become fact in the project
memory). This adds a narrow, VM-internal migration so a shape edit applied
through the editor's Save can carry the running VM's existing instances across,
without losing the live session to a reboot.

Explicitly **out of scope** (and that is what makes this tractable):
- No guest-facing `become:`/`becomeForward:` primitive. This is never callable
  from Smalltalk; it is an internal operation the Save path invokes.
- No two-way identity swap. Migration is strictly one-way: each old-layout
  instance → a new-layout copy.
- Not while the VM is running guest code. Only at a request boundary, when the
  worker is between doits (see §2).

## Why this is tractable here (the reuse map)

MACVM's moving GC already solved the one genuinely hard problem — enumerating
**every** reference and rewriting it — and did so under `MACVM_GC_STRESS`. The
migration is that machinery pointed at a caller-supplied forwarding map instead
of the GC's own compaction forwarding. Concretely, all confirmed present:

| need | already exists |
|---|---|
| rewrite every oop field of every object | `MemOop::for_each_oop_field(klass, FnMut(Oop) -> Oop)` (`src/oops/heap.rs`) — already a *rewriting* visitor: it returns the new oop per slot |
| rewrite every root | `memory::roots::for_each_root(vm, FnMut(&mut VmState, Oop) -> Oop)` — same rewriting shape, "one fixed order": universe singletons, well-known selectors, symbol table, class table, class vars, globals, handle arena, and the process-stack scan |
| flush compiled code with stale field offsets | `codecache::flush::make_not_entrant` + `runtime::deps::invalidate_dependents` (the class-redefine path) |
| a walkable heap (for `allInstances`) | linear walk via `instance_size_words` (`fullgc.rs` phase B already does it) |
| write barrier / remembered set for new old→young edges | the GC's existing barrier (`memory::cards`/`store`) |

The new code is orchestration on top of these, not new GC.

## 2. The load-bearing invariant: a paused VM has no live frames

Save is a top-level worker request (`VmRequest::EditorAccept`), and the worker
services one request at a time. Between requests the VM stack is at its **clean
baseline** — `sp = 0`, no active frame — the exact invariant hardened in the
recover-clean-or-die work (`embed::restore_after_guest_fatal` / the idle
baseline). So when migration runs, driven from Rust:

- there is **no guest computation on the stack**, hence no live compiled frames
  holding oops in registers/spill slots. The hardest third of root completeness
  — catching oops inside in-flight JIT frames via oop-maps — **does not apply**,
  because nothing is running. `for_each_root`'s process-stack scan walks an
  empty stack.
- the roots that remain are all **static**: universe singletons, symbol table,
  class table, class vars, globals, and the handle arena. Enumerable and quiet.

This is the entire reason the "paused" restriction makes the feature small
rather than a sprint: it deletes the risk core by construction.

Caller discipline: the migration is one Rust function; it must not cache a raw
oop across the fixup (values move/are abandoned) — hold through the handle arena
or re-read.

## 3. The operation

`migrate_shape(vm, klass_C, new_ivar_names)`:

1. **Update the klass shape metadata** — new instance size + ivar-name array —
   keeping the **same klass oop** (so the `C` global, class-side refs, and
   identity-guarded ICs stay valid). Compute the old→new slot remap by ivar
   name.
2. **`all_instances_of(vm, C)`** — a linear heap walk collecting every object
   whose klass is `C`. (Subclasses migrate too: their layout includes inherited
   ivars, so recurse — their remap differs by the inherited prefix.)
3. For each instance: allocate a new-layout copy, copy surviving ivars by the
   remap, `nil` the additions → build the side map `{old → new}`.
4. **`become_fixup(vm, &map)`** — the fixup pass (§4). Rewrites every oop field
   in the heap and every static root through the map.
5. **Flush** nmethods that bake `C`'s (and subclasses') field offsets —
   `invalidate_dependents`, broadened from one selector to "any of this class's
   compiled methods." Clean here: nothing is executing, so it is just
   `make_not_entrant` (next call recompiles against the new layout).
6. Old instances are now unreferenced → reclaimed at the next GC.

**Degenerate case, common and nearly free:** if `all_instances_of(C)` is empty
(the class was never instantiated in this session), skip 2–4 entirely — just
update metadata (1) and flush (5). Many everyday shape edits cost almost
nothing.

## 4. `become_fixup` — a standalone side-map pass (NOT the compactor)

Do **not** piggyback the compacting GC's header forwarding: the compactor wants
each object's header for its *own* move, and pre-installing a become-forwarding
there fights it. Instead a self-contained pass, reusing the visitors:

```
fn become_fixup(vm, map: &HashMap<Oop, Oop>) {
    let redirect = |o: Oop| map.get(&o).copied().unwrap_or(o);
    // every static root
    roots::for_each_root(vm, |_vm, o| redirect(o));
    // every oop field of every live heap object
    for obj in heap_objects(vm) {                 // the linear walk
        obj.for_each_oop_field(obj.klass(), |o| redirect(o));
    }
    // record new old→young edges the fixup created (or promote copies)
    maintain_write_barrier(vm, map);
}
```

Independently testable with no editor, no Save, no shape logic — it is pure
"rewrite these references." That isolation is the whole point given the failure
mode (a missed slot = a dangling oop = the S12–S15 corruption class).

## 5. Build order (each layer GC-stress-verified before the next)

- **L1 — `become_fixup` alone.** The side-map rewrite over heap + static roots +
  the barrier. Test: build objects that reference target objects from **every
  static root kind** (a global, a class var, the symbol/class table, a handle,
  nested inside a collection, an indexable-oops tail), run `become_fixup` with a
  hand-built map, assert every reference now points to the replacement and
  **zero** still point to the old — under `MACVM_GC_STRESS`, and re-run a full
  GC afterward asserting no dangling oop / no verify failure.
- **L2 — `all_instances_of` + the migration driver.** Heap walk, new-shape copy,
  ivar remap, metadata update, nmethod flush, subclass recursion. Test: migrate
  a class held live from each root kind; assert instances carried their
  surviving ivar values, additions are `nil`, count preserved, and a method that
  reads a moved ivar returns the right value after recompile.
- **L3 — Save wiring.** `editor_accept_response`, after the DB+world persist,
  detects a shape change vs. the running klass and calls `migrate_shape`; report
  "migrated N live instances" on the transcript. The DB/world write stays; this
  only adds the live half.

## 6. Test matrix (the risk is entirely root completeness)

Under `MACVM_GC_STRESS`, an instance to be migrated must be reachable — and end
up correctly repointed — from each of: a global/`Smalltalk` binding; a class
variable; the symbol or class table; the handle arena; a live interpreter temp
(the paused stack is empty, but the Save doit's own frame is a boundary case to
pin); a named slot of another object; the indexable tail of an
`IndexableOops` object; a key/value inside a `Dictionary`/`Set` (identity-hashed
collections may need rehash — flag). A verify pass + full GC after each asserts
no survivor points at an abandoned instance.

## 7. Honest caveats

- This escalates what Save does: today Save is safe precisely because it never
  touches the running heap (DB + world only, effect on next boot). Live
  migration adds heap mutation — the risky new surface. **The reboot path stays
  the safe default;** live migration is the "don't lose my session" upgrade,
  and can be a flag.
- Value vs. cost: over save-then-relaunch (≈1 s from a few-KB image), the only
  thing gained is keeping live objects/session state across a shape edit — a
  genuine interactive-Smalltalk nicety, not a correctness need.
- Identity-hashed collections holding migrated instances as keys may need a
  rehash (identity hash should be stable across the copy if it is derived from a
  stored field, not the address — verify which MACVM uses).

## Cross-references

- `src/oops/heap.rs` (`for_each_oop_field`), `src/memory/roots.rs`
  (`for_each_root`), `src/memory/fullgc.rs` (the heap walk + forwarding it
  mirrors), `codecache::flush` / `runtime::deps` (nmethod invalidation).
- `docs/editor_design.md` (the Save path this hangs off), `docs/SPEC.md` §7 (GC).
- Project memory: the no-`become:`/static-shape fact (why this exists), and the
  GC-verify-under-stress rule (how it must be proven).
