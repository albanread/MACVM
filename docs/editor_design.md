# A Smalltalk text editor for MACVM

**Status:** design only — nothing here is built. Written 2026-07-16.

There is a `texteditor` icon in the toolbar that does nothing. It is declared in
`TOOLBAR_BUTTONS` as `("texteditor", "editor", "Text editor")`
(`gui/src/preprocess.rs:500`), so it renders and it posts
`{kind:"toolbar", button:"editor"}` — and `navigate_toolbar`
(`gui/src/main.rs:1261`) has no `"editor"` arm, so the message lands nowhere.
This document designs what should be behind it.

Strongtalk shipped a text editor. Ours are all JavaScript, which is the right
call for the *chrome* and stays that way — the class browser's source pane
(`.st-code-input` + `.st-code-highlight`, `gui/assets/smtk.js:280`) is hardened
and keeps its job. This is a **second, first-class implementation**, not a
migration ([[feedback-dual-placement-not-migration]]): the interesting thing is
a text editor whose **model is Smalltalk** — the buffer, the cursor, the edit
operations and the undo history all live in a Smalltalk heap, in its own VM.

## 1. What it is for

Two goals, in this order.

**A real tool.** Select a class, edit the *whole class* as text — not the
browser's method-at-a-time view — syntax-check it, and update the image. That is
a genuinely different way to work: the browser is good at navigating and
surgical edits, and bad at "rename this ivar and fix the six methods that use
it". A whole-class text buffer is good at exactly that.

**A real workload.** Every benchmark in `world/bench/` is a throughput sprint
that finishes in milliseconds — Richards (6 ms), DeltaBlue (4–5 ms), arith
(9 ms). They came out of the Self lineage, V8 carried them in as its legacy
suite, and they are the two the JS world eventually singled out as too small to
steer by. MACVM has no workload that is *long-running*, *latency-shaped*, or
that maintains a *large, long-lived, incrementally-mutated object graph*. An
editor is all three. See §7 — and read it with §8's warning about strawmen.

The second goal must never drive the first. If the editor is designed to be a
benchmark it will be a bad editor and a lying benchmark. Build it because a
Smalltalk editor is worth having; the profile is a side effect.

## 2. Architecture: who owns what

The split follows the house rule — *the purpose of the compiler is to run
Smalltalk, not to be Smalltalk*:

| layer | owns | language |
|---|---|---|
| WKWebView | pixels, scrolling, selection painting, key capture | JS/HTML |
| `vm_host` / `main.rs` | fetch, syntax check, install, persist | Rust |
| **editor worker VM** | **the rope, cursor, edit ops, undo history** | **Smalltalk** |
| `image_store` (SQLite) | the durable truth | Rust |

**One worker VM per open class.** `Worker class >> spawn: initSource`
(`world/47_worker.mst:53`) already exists, with `send:onReply:` and
`call:on:args:onReply:onError:`. Opening a class spawns a VM seeded with that
class's text; closing it drops the VM.

That last point is the architectural payoff and it is not just tidiness:

- **Closing a document frees its heap outright** — no tracing, no collection.
  The VM goes away and the memory is gone. A shared-image system has to
  *collect* your closed document.
- **GC isolation.** A full GC in the Workspace VM cannot stall your typing, and
  a 200 KB class's promotion churn cannot stall anyone else. This is the thing a
  single-image Smalltalk structurally cannot offer, and it is the *better half*
  of the Workers story — M0–M4's only exemplar today is ParallelMandel, a
  four-VM compute fan-out, which is the half everyone already believes in.
- **Idle by construction.** Event-driven on keys and menus means ~100 ms of dead
  time between keystrokes at fast typing. That is an eternity to a JIT: compile
  latency stops mattering, and a per-document collection can be scheduled into
  the gap where nobody is waiting.

**What a worker actually costs.** Less than it looks. The VM's own machine code
is *shared*: `macvm` is a **2.4 MB** binary and `macvm-gui` — that plus the
entire Cocoa app — is **4.5 MB**, mapped once into one process no matter how
many VMs run in it. (The GUI process is ~133 MB RSS; ~97% of that is Apple's
WebKit, not ours. The VM is closer in size to Smalltalk-80's whole system than
to V8's 30–50 MB engine binary.) So per-VM cost is only: the heap, the
CodeCache of JIT'd Smalltalk, and the world's object graph. Duplicated rope
nmethods across ten open documents are kilobytes, not megabytes.

The knob that *does* matter is **heap size**. Workers currently boot with
`heap_mib: 64` (`gui/src/vm_host.rs:2410`), which an editor has no use for — and
shrinking it is a latency feature, not just a footprint one: **a small heap
scavenges fast**. Sized to its document, an editor VM's own collections finish
in microseconds. Isolation then means both "someone else's GC can't stall my
typing" *and* "my own GC is over before the next keystroke".

### 2.1 World + package

The remaining per-VM cost is the world's object graph, and that is worth not
paying in full. The rule:

> **The world loads always. A package is a named set of classes loaded on top.**

The world is small — 91 classes, 1067 methods, **246 KB of source**, ~1.0 s to
boot — so it is not worth splitting into layers. Packages are the **VMApps**,
and the image already half-models them: `Image::packages()` and `package_roots()`
group by the class `category`, and the app categories in the image today
literally are the app names — `smappl` (14 classes), `tools` (8), `gamepane`
(4), `cocoa` (4), `mandelbrot`, `breakout`, `cocoapad`… The editor's classes
join as package **`editor`**.

What is missing is only the ability to *not* load one. Minimal shape:

- a `package` on a class — empty/NULL means world, a name means a VMApp;
- `load_world_from_image` keeps its meaning (the world);
- `load_package(vm, image, "editor")` loads one package's classes on top,
  reusing the same two-phase shells-then-methods pass (already forward-ref
  safe, S22).

**v1 changes no existing behaviour:** the primary VM keeps loading world + every
package exactly as it does now, and only *worker* VMs boot selectively. The
editor worker gets **world + `editor`** — no smappl, no gamepane, no Cocoa
bridge, no demos. Making the primary VM's packages lazy (so the Demos menu
loads its package on click) is a separate, later question.

Do **not** key the boundary on `load_order` or on the `.mst` numbering. That
numbering is historical, not architectural: `40_seqcltn_search` and
`41_string_text` are base library sitting between `39_floatarray` and
`42_benchdash`, and `51`–`59` are all base extensions loading *last*, after
every app. Category is the right key precisely because it is independent of when
a file happened to get written. (A happy accident helps here: `51`–`59`
*reopen* base classes rather than defining new ones, so the ~119 library methods
already file under their base class's category — the library wave is correctly
assigned to the world without anyone having intended it.)

## 3. The buffer: a persistent rope

A rope is a balanced tree of string fragments: `RopeLeaf` holding a short
`String`, `RopeBranch` holding `left`, `right`, and a `weight` (the character
count of the left subtree). Insert/delete/split/concat are all O(log n).

**Persistent, not ephemeral.** An edit rebuilds only the path from the root to
the touched leaf and shares everything else. This is chosen for a product
reason, not an aesthetic one: **undo is free**. Keep the previous root and you
have the previous document, exactly, at no copying cost. Undo/redo becomes a
list of roots. An ephemeral rope would need an explicit undo log and would get
write barriers instead of allocation; persistent is simpler *and* the more
interesting shape.

**Carry a newline count per node**, alongside `weight`. Without it there is no
O(log n) `lineAt:` / `offsetOfLine:`, and without those there is no way to
answer "which lines changed?" — which is the whole of §4. This is the one
non-obvious structural requirement and it must be in from the start; retrofitting
it means touching every node type.

Core protocol:

```smalltalk
Rope >> size                      "characters"
Rope >> lines                     "newline count + 1"
Rope >> at: i
Rope >> insert: aString at: i     "→ a NEW rope"
Rope >> delete: from to: to       "→ a NEW rope"
Rope >> lineAt: i                 "→ line number containing offset i"
Rope >> offsetOfLine: n
Rope >> linesFrom: a to: b        "→ String, for the view"
Rope >> asString
```

Leaves cap at ~64–128 characters; rebalance when depth exceeds ~2·log(size).
`RopeLeaf`/`RopeBranch` means every tree walk is a **bimorphic send at each
node** — real PIC territory, which is the shape the send-path work (O0–O4) has
only had Richards and DeltaBlue to judge against.

`TextBuffer` wraps a rope root plus cursor/selection/undo stack; the rope itself
stays a pure value.

## 4. The GUI protocol: keys in, damage out

The JS side becomes a dumb terminal: it captures keys, it paints lines, it owns
no text. Modelled directly on `VmRequest::GameStep { keys }`
(`gui/src/vm_host.rs:106`) — a GUI event submitted to an idle worker as a fresh
top-level request, single-outstanding.

```rust
VmRequest::EditorKey     { key: String, mods: u8 }
VmRequest::EditorCommand { name: String }          // accept, undo, redo, close
VmResponse::EditorDamage {
    first_line: u32, last_line: u32,   // inclusive range that changed
    text: String,                      // the new content of exactly those lines
    total_lines: u32,                  // so the view can size the scrollbar
    cursor_line: u32, cursor_col: u32,
}
```

The editor VM applies the edit, computes the damaged line range from the rope's
newline index, and answers **only those lines**. A single character typed mid-line
is a one-line damage record regardless of document size. That is what "send the
right part of the buffer efficiently to the view" means concretely, and it is
why §3's newline count is mandatory.

Note the thread boundary is **not new**: the GUI already talks to the VM across
the language-thread channel, so routing keys to a *different* VM adds no hop to
the critical path. Only accept/save needs to reach the primary VM.

## 5. The class round-trip

### Fetch — image → text

`image_store` already has every piece:

- `Image::all_methods_of(class) -> Vec<FullMethod{selector, side, category, source}>`
- `Image::superclass_of`, `class_comment`
- `FullClass { name, superclass, category, comment, instance_vars, class_vars, .. }`
- `export::class_block(superclass, name, ivars, cvars, method_sources, with_header)`
  — **already renders a class to `.mst` text**, and is deliberately
  round-trip-idempotent: "a re-parse equals the stored source and re-export
  stays idempotent (no churn)" (`image_store/src/export.rs:69`).

So fetch is `class_block(...)` and nothing new. That idempotence is load-bearing:
it means opening a class and accepting it unchanged must produce a zero-diff.
**That is the first test to write** (§9 M1) — if it churns, everything below
inherits the churn.

### Check — before updating, and with the real compiler

The requirement is a syntax check *before* the update. The check must use
**`frontend::parser::parse_file(input) -> Result<Vec<TopItem>, CompileError>`**
(`src/frontend/parser.rs:1093`) — the actual compiler's parser, the same one
that will install the text, reporting exact `line:col`
(`tests/it_frontend_errors.rs` asserts precise positions).

**It must not use `image_store::mst::parse_mst_source`.** That is a *second*
parser, used for import and sender extraction. Checking with a parser other than
the one that installs is a lie by construction: any divergence between them
produces either a false green (accepted here, rejected on install) or a false
red. One parser is the authority.

`parse_file` is a pure function needing no VM, so the check is cheap and
host-side. Be honest about its reach: **it catches syntax, not semantics.**
Undeclared variables and the like surface in `codegen`, which needs a VM. So:

- **keystroke time** — no check (too expensive, too noisy)
- **on pause / on demand** — `parse_file`, underline `line:col`
- **on accept** — `parse_file` gates, then the install is the real authority

### Update — install, then persist, and diff rather than rewrite

```
parse_file(text)                      → error? show line:col, STOP
live_compile(vm, text)                → vm.exec(...), the running VM is authority
image_store writes                    → persist only what changed
```

Order matters. The image is the boot source
([[reference-gui-boots-from-image]]), so persisting text the VM rejected would
poison the next boot. Install first; if the persist then fails, say so — the
`SaveOk { image_error: Option<String> }` pattern (`gui/src/vm_host.rs`) exists
for exactly this honesty and should be reused rather than claiming success
unconditionally.

**The coarse-grained trap, and the mitigation.** A whole-class accept is not a
whole-class rewrite. Naively re-installing every method would:

- invalidate every nmethod of that class → a deopt storm on a hot class, and
- bump `method_version_count` for every method → history polluted with
  no-op versions on every accept.

So the accept path must **diff**: parse the buffer, compare each method's source
against the stored `FullMethod.source`, and write only the ones that actually
changed (plus removals for methods that disappeared, and
`set_class_definition` if the shell changed). `create_or_reopen_method` /
`create_or_reopen_class` already exist for the write side. This is the single
most important correctness detail in the design — whole-class *editing* with
method-granular *committing*.

## 6. Durability

Standard editor rules; the process is not the durability story.

- The **image is the truth**, and it must never contain text that does not
  parse — that is what §5's ordering guarantees.
- **Autosave writes a draft**, not the class. A draft is unchecked text by
  definition, so it cannot go through `set_method_source`. It belongs in a
  separate drafts table (or a file) keyed by class name, restored on reopen with
  an "unsaved changes" marker.
- A worker VM dying therefore costs at most the draft interval — the same
  contract every editor has had for fifty years. `WorkerIdle`'s respawn
  supervisor is strictly better than a crash dialog, provided the draft is
  outside the VM heap.

## 7. What it is worth as a workload

Recorded because it motivated the idea, and because [[reference-instructions-are-not-time]]
says the reasoning matters more than the number:

- **Latency, not throughput.** Keystroke→paint under ~16 ms, *including* the GC
  that decides to run mid-keystroke. Nothing in `world/bench/` measures latency.
- **Real generational behaviour.** Mandelbrot allocates garbage that all dies
  immediately — pure eden churn, zero survivors. A persistent rope allocates a
  path per edit *into a long-lived tree*: survivors, promotion, old-to-young
  pointers, fragmentation over hours. S8 and S12 built that machinery and it has
  never had a workload that behaves like a program.
- **Dispatch-bound, not loop-bound.** Bimorphic `RopeLeaf`/`RopeBranch` at every
  node, deep recursion, blocks with early exit (`detect:`-shaped conditional
  NLR — the S24 B5 shapes). Complementary to Mandelbrot's float/loop profile.
- **A clean per-VM profile.** The metrics dashboard samples a **per-VM**
  `Arc<VmLiveStats>` rather than a global, specifically for multi-VM safety
  ([[project-metrics-dashboard]]). That decision has had no real reason to exist
  until now: an editor in its own VM means the toolbar can show *that document's*
  alloc rate, GC count and live interp/compiled ratio while you type.

## 8. Risks and open questions

1. **The strawman trap — the real one.** If the rope is chosen because it makes
   a good VM workload rather than because the editor needs it, the profile is a
   strawman. For documents under a megabyte a gap buffer or piece table is often
   the better structure; VS Code moved *to* a piece table deliberately. The
   justification here is undo-via-structural-sharing (§3), which is a product
   argument. If that stops being true, revisit the structure — do not keep the
   rope for its GC profile.
2. **The bridge could dominate.** If damage records are large or frequent, the
   thing being timed is `EditorDamage` marshalling, not the VM. Instrument the
   two separately from the start or the first profile will be worthless.
3. **N VMs × N CodeCaches.** §2 measures this as small — the VM binary is
   shared, so only JIT'd Smalltalk duplicates. Still the first scenario in which
   the I-cache argument used to justify the code-size work in `c495f2e` could
   actually be *tested* — one document open vs twenty. It remains a hypothesis
   until then, and §2 predicts the answer is "negligible".
4. **Syntax check reach.** `parse_file` is syntax only; the honest UI must not
   imply "it compiles" when it means "it parses".
5. **Where does the check run?** v1: host-side Rust, since `parse_file` is pure.
   If live error underlining wants it inside the editor VM, that needs a
   primitive (`syntaxCheck:` → nil or `line:col:message`), which is the cleaner
   seam but more surface. Not decided.
6. **Is whole-class the right grain?** Very large classes make a single buffer
   unwieldy and widen the accept diff. Possibly the answer is per-class for now
   and never worry about it, since our largest world classes are small.

## 9. Milestones

- **M0 — wire the icon.** `"editor" => open_editor()` in `navigate_toolbar`,
  mirroring `open_workspace()` (`gui/src/main.rs:565`). Opens a view on a class
  picked from `Image::class_names()`. Read-only, text from `class_block`.
  *Gate:* the icon does something.
- **M1 — round-trip fidelity.** Open every class in the image, accept unchanged,
  assert a zero diff and zero version bumps. *Gate:* `class_block` idempotence
  holds across the whole world (91 classes), headless.
- **M2 — the rope in Smalltalk.** `Rope`/`RopeLeaf`/`RopeBranch` + `TextBuffer`,
  with the newline index. Pure Smalltalk, no GUI, filed as package **`editor`**
  from the first line. *Gate:* a test suite in `world/tests/` —
  insert/delete/split/concat/lineAt: against a `String` oracle, plus a
  randomised differential test.
- **M3 — keys and damage.** `EditorKey`/`EditorDamage`, JS as a dumb terminal.
  *Gate:* typing works; damage is minimal (assert one-line damage for a
  mid-line insert).
- **M4 — check and accept.** `parse_file` gate, diffing accept, install →
  persist with honest `image_error`. *Gate:* a bad edit shows `line:col` and
  changes nothing; a good edit changes exactly the methods that differ.
- **M4.5 — packages (§2.1).** A `package` on a class, `load_package`, and the
  existing app categories backfilled as packages. Primary VM behaviour
  unchanged. *Gate:* a VM booted with world + `editor` has `Rope` and does
  **not** have `GamePane`; the primary VM still has both.
- **M5 — its own VM.** Move the buffer into a worker spawned with world +
  `editor`, sized to the document rather than `heap_mib: 64`; close drops it.
  *Gate:* a full GC in the Workspace VM does not stall typing (measure it —
  this is the claim the whole architecture rests on).
- **M6 — durability.** Draft autosave outside the image; restore on reopen.

M0–M1 are worth doing regardless of whether the rest ever happens: they turn a
dead icon into a working read-only class-text view and prove the fidelity the
browser's own accept path already depends on.

## Cross-references

- `docs/multi-smalltalk-worker.md` — the Worker channel this rides on
- `docs/gamepane_design.md` — `GameStep`'s single-outstanding event pattern,
  which §4 copies
- `docs/mandelbrot_walkthrough.md` §5.1 — why §7's claims must be measured, not
  reasoned about
- `image_store/src/export.rs`, `image_store/src/lib.rs` — the fetch/persist API
- `src/frontend/parser.rs`, `tests/it_frontend_errors.rs` — the one true parser
- `SPEC.md` — needs an entry if this is built
