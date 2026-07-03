# QBE-based JIT as a MACVM codegen backend — review

Written against `docs/DESIGN.md` §5 (open decision D6, currently JASM-leaning)
and `docs/arm64.md` (`MAP_JIT`/W^X, AAPCS64, PAC). Two extractions happened in
this session, in order — the second corrects the first.

## Correction: my first pass answered the wrong question

Asked for "the QBE JIT components," I first extracted
[FBCQBE](https://github.com/albanread/FBCQBE) into `aot/`. That's QBE used as
a batch AOT backend: IL text in, assembly text out, then an external `as`/`cc`
step to get an executable. No in-memory code buffer, no relocation API, one
documented arm64 miscompilation. Useful as a small, readable reference for
QBE's optimizer pipeline; not a JIT by any definition, and not what "lighter
weight JIT than LLVM" was asking about. `aot/README.md` and this file's
predecessor said as much in more detail — kept for the record, but `jit/` is
the one that matters.

The actual JIT is in a sibling repo,
[FasterBASIC](https://github.com/albanread/FasterBASIC)'s `zig_compiler/`,
extracted into `jit/`. This review is about that.

## What `jit/` actually is

```
QBE IL text
  → qbe_compile_il_jit()      [jit/c/qbe_bridge.c — replaces QBE's main()]
  → parse → 25-pass optimizer → regalloc → isel   [jit/c/*.c, same QBE core]
  → jit_collect_fn(Fn*)       [jit/c/jit_collect.c — walks post-regalloc IR]
  → JitInst[]                 [flat, pointer-free, C-ABI struct array]
  → jitEncode()                [jit/zig/jit_encode.zig — Zig reads JitInst[]]
  → JitModule (code+data+fixups)
  → JitMemoryRegion.allocate() [jit/zig/jit_memory.zig — mmap + MAP_JIT]
  → JitLinker.link()           [jit/zig/jit_linker.zig — ADRP/ADD + trampolines]
  → makeExecutable()           [W^X toggle + icache invalidate]
  → call function pointer      [execution, in-process, no subprocess anywhere]
```

The key move, compared to the AOT path: QBE's own `emit.c` (which formats
final assembly as text) is bypassed after register allocation. `jit_collect.c`
reads the same `Fn*`/`Ins*` structures `emit.c` would otherwise stringify, and
produces `JitInst` records instead — plain structs (`kind`, `cls`, `rd/rn/rm/ra`
register ids, `imm`/`imm2`, `sym_name`), no text anywhere in the pipeline from
that point on. The Zig side (`arm64_encoder.zig`) turns each `JitInst` into a
raw `u32` machine word directly into `mmap`'d memory. This is what "lighter
weight JIT than LLVM, higher-level than a raw assembler" actually looks like:
QBE's SSA optimizer (mem2reg, GVN, global code motion, linear-scan-ish
regalloc — the "higher than assembler" part) feeding a small, direct-to-memory
encoder (the "lighter than LLVM" part, no MCJIT, no ORC, no bitcode).

## Verified, not just read

I didn't take the design docs' word for it. On this machine (Darwin arm64,
Zig 0.15.2), from the extracted `jit/zig/` files exactly as they sit in this
repo:

| Command | Result |
|---|---|
| `zig test arm64_encoder.zig` | **344/344 pass** — standalone, `std`-only |
| `zig test jit_encode.zig` | **371/371 pass** — encoder + JitInst→JitModule |
| `zig test jit_memory.zig` | **19/19 pass** — real `mmap`/`MAP_JIT`, W^X toggling, trampoline stubs |
| `zig test jit_linker.zig` | **391/391 pass** — ADRP/ADD relocation math, trampoline island |

`jit_capstone.zig` (needs vendored Capstone) and `qbe.zig`/`jit/c/*`
(needs the C objects linked in) weren't independently re-verified — that's a
full-build exercise (fetching `capstone/` as a sibling directory per
`build.zig`), out of scope for an extraction pass. Everything that *could* be
checked standalone, was, and passed.

## The part that actually matters for D6: patchable code, not just generated code

`docs/DESIGN.md` §5 asks for more than "produce machine code" — it wants a
code buffer **plus a relocation stream**, and reloc kinds that distinguish
embedded oops, **inline-cache call sites** (for later patching), runtime
calls, and internal words, because Self/Strongtalk-style adaptive
optimization needs to go back and rewrite call sites after the fact (PIC
transitions, deopt).

`jit_memory.zig` already has exactly the low-level primitives that requires:

- `patchBLDirect()`, `patchBLToTrampoline()`, `patchAdrpAdd()`,
  `patchCodeWord()` — overwrite an already-emitted instruction word in place.
- `makeWritable()` / `makeExecutable()` — the W^X flip around a patch,
  matching `docs/arm64.md`'s `pthread_jit_write_protect_np` plan exactly.
- `icacheInvalidate()` — required after any patch before re-execution.

This is real, tested machinery (see the "write protection state enforcement"
and "patchBLToTrampoline computes correct offset" cases in the 19/19 above),
and it's the same primitive an inline-cache patcher needs: flip to RW, rewrite
one call-site word, flip back to RX, flush icache.

What's **not** there: `JitInst.sym_type` (`jit/c/jit_collect.h`) only
distinguishes `GLOBAL` / `THREAD_LOCAL` / `DATA` / `FUNC` — there's no
`OOP` or `IC_SITE` category, and the relocation records in `jit_linker.zig`
carry a symbol name to resolve, not a GC/patch-semantics tag. So this doesn't
hand MACVM the reloc-kind vocabulary DESIGN.md §5 wants, but — unlike the AOT
path, where getting there meant forking `emit.c`'s text emission — here it
means **extending an already-structured, already-tested schema**: add
`JIT_SYM_OOP` / `JIT_SYM_IC_SITE` to the enum, thread the extra tag through
`jit_collect.c` and `jit_linker.zig`'s fixup records, done. That's a real but
bounded patch, not an architectural mismatch.

## Two open risks, carried over from the same lineage

1. **The arm64 register-allocation bug.** `aot/known_issues/BUG_REPORT.md`
   documents a real miscompilation (array store after a `WHILE`+nested-`IF`
   lands in the wrong physical register) in this same QBE-fork lineage.
   `jit/c/` is a further-evolved fork of the same arm64 backend (see
   `design/QBE_extensions.md` — it's grown NEON opcodes and more peepholes
   since). The FasterBASIC repo has a live regression test for the same shape
   of bug (`jit/known_issue_regression_test.bas`, copied from
   `tests/test_while_array_bug.bas`) — I could **not** confirm whether it
   currently passes, since running it needs a full `fbc` build (Capstone
   vendored, all runtime `.a` libs linked). Treat this as an open,
   unconfirmed risk, not a resolved one. In a GC'd VM a wrong-register bug is
   silent heap corruption, not a crash — this needs its own targeted
   adversarial test pass before any part of this backend is trusted, the same
   standard [[feedback_gc_verify_under_stress]] already holds MACVM's own GC
   work to.

2. **Text still exists at the front of the pipeline.** `qbe_compile_il_jit()`
   still takes QBE IL as *text* (`design/direct_qbe_ir_jit.md` — the
   FasterBASIC authors measured ~0.5ms of parse overhead per JIT compile on a
   trivial function and explicitly decided **not** to remove it, calling a
   direct-IR-construction API "high risk... 3,000–3,500 lines... 3–4 weeks").
   For MACVM's adaptive tier — recompiling hot methods repeatedly, inheriting
   PIC state, resuming at a specific safepoint — that per-compile text
   round-trip is real, recurring latency, on top of whatever cost MACVM's own
   IR-to-QBE-IL serialization would add. It's not disqualifying (0.5ms is
   small next to a full recompile), but it's a permanent tax this
   architecture carries that a direct-to-memory encoder (JASM's approach)
   doesn't.

## Bottom line

This is a materially different, much stronger candidate than my first
extraction. It's a real, W^X-correct, in-process ARM64 JIT with tested
patch/relocate/trampoline machinery that overlaps meaningfully with what
`docs/DESIGN.md` §5 and `docs/arm64.md` already ask for — not a rejected
alternative, closer to "worth prototyping against." The case for it over the
JASM-leaning D6 default:

- **For**: gets QBE's mem2reg/GVN/regalloc pipeline for free instead of
  building MACVM's own optimizer from scratch; the encoder is proven across
  ~300 instruction forms with clang-verified round-trip tests (aot's design
  doc claims 828 cases — not independently re-verified here, but the 344
  unit tests I ran are a real subset); the memory/patch/W^X layer is directly
  reusable almost as-is.
- **Against**: an unresolved arm64 correctness bug in the same lineage; a
  text-IL front door with measured (if small) recurring cost; and a reloc
  schema that needs real, if bounded, extension work before it carries
  MACVM's oop/IC semantics.

Recommendation: don't switch D6 on this alone, but this is worth a spike —
specifically, feed one real MACVM optimizing-tier method through
`qbe_compile_il_jit()` → `jitEncode()` → `JitMemoryRegion` end to end, and
separately, triage the arm64 regalloc bug against current QBE HEAD before
relying on this backend for anything that touches the GC'd heap.

## Addendum: `jit/rust/` — the same pipeline, ported to Rust

Per follow-up direction, `jit/{c,zig}` above has since been ported to a
standalone Rust crate at [`jit/rust/`](jit/rust/) (`qbejit`) — same
architecture, same vendored C QBE-fork, driven over FFI from Rust instead of
Zig, so MACVM could prototype against it without a second toolchain. Every
module got independent test verification during the port (216 tests: 214
unit, plus 2 end-to-end integration tests that compile real QBE IL, encode
it, `mmap`/`MAP_JIT`-allocate, link, and execute the result — including a
loop with a backward branch and a conditional exit, which exercises
`resolve_fixups`'s two real risk paths). That's a stronger evidence base
than this review had when it recommended "worth a spike" above — the
synthetic version of that spike has now happened. It does not, on its own,
resolve either open risk this review flagged: the arm64 regalloc bug is
still unconfirmed either way, and the reloc-kind extension DESIGN.md §5
needs (oop/IC-site tagging) still doesn't exist. See `jit/rust/README.md`'s
own "What's verified, and what's still owed" for the current, precise
state.

## Addendum 2: both open risks from Addendum 1, now addressed

### The arm64 regalloc bug: did not reproduce

`aot/known_issues/BUG_REPORT.md`'s own root-cause section is explicit that
this is **not** the MADD-fusion peephole ("Disabling the MADD peephole
optimization does NOT fix this bug... indicating the problem is in register
assignment, not instruction selection") — I'd mis-attributed it to MADD
fusion in Addendum 1's framing; correcting that here. The actual documented
shape: a value computed before a branch (there, an array data pointer)
gets corrupted by the time a post-merge block uses it, specifically when
one predecessor path makes an external call.

Three tests built to reproduce that exact shape against `jit/rust`
(`jit/rust/tests/register_bug_investigation.rs`) — a WHILE-loop-then-branch-
merge array re-read transliterated directly from the report's own BASIC
reproducer, an asymmetric-register-pressure branch merge, and a
branch-merge where one predecessor makes a real external call (closest
match to the report's actual failing assembly, which showed a value
clobbered specifically across a `bl _basic_array_bounds_error`) — **all
three pass**. This doesn't prove the bug can never occur (three hand-built
cases aren't exhaustive, and the original bug involved a real BASIC
runtime's array-descriptor bounds-check IL shape this doesn't fully
replicate), but it's real negative evidence on the exact failure mode the
report describes, on the exact `jit/c` fork this crate drives, and it's a
meaningfully stronger basis for confidence than "unconfirmed."

**A different, real bug turned up while investigating**, though: the trap
stub `write_trap_stub()` writes for unresolved trampoline targets — meant
to be `BRK #0xF001` so an unresolved-symbol call faults cleanly and
identifiably (`SIGTRAP`) instead of following a null pointer — was encoding
the wrong opcode base (`0xD4000000` instead of `0xD4200000`), landing on an
*unallocated* ARM64 instruction instead of `BRK`. Executing it raises
`SIGILL`, not `SIGTRAP` — still a clean, identifiable trap rather than
silent corruption, but not the one the design intended, and one whose exact
signal a crash handler would need to match on. This was wrong in the
original `jit_memory.zig` too (its own doc comment states the correct
`0xD43E0020` result directly above code computing `0xD41E0020` — a
self-contradicting comment that should have been a tell). Fixed in
`jit/rust/src/memory.rs`, with a regression test
(`write_trap_stub_encodes_a_real_brk_not_svc`) checked against
`crate::encoder::emit_brk`'s independently clang/llvm-mc-verified encoding.

### The reloc-kind gap: closed, via a data-section indirection rather than a code reloc kind

DESIGN.md §5 asks for reloc kinds distinguishing embedded oops and
IC-patchable call sites. Built both, in [`jit/rust/src/patch.rs`](jit/rust/src/patch.rs) —
but the oop mechanism isn't a new *code* reloc kind, because on reflection
that's the wrong shape for a **moving** GC: MACVM's mark-slide-compact
collector relocates objects, and re-encoding a multi-instruction immediate
load in the code stream on every move (flip W^X, patch, flush icache) is
exactly the cost DESIGN.md's own reloc-kind ask would otherwise commit
MACVM to paying per moved object per referencing method. QBE already has a
better mechanism available for free: a `data $sym = { l 0 }` slot, loaded
in function bodies the normal way (`loadl $sym`), resolves through the
*existing* `LOAD_ADDR`/data-relocation machinery — and the data region is
**always RW**, never subject to the code region's W^X toggle (true in the
original `jit_memory.zig` too, not something this port added). So a GC move
becomes one bounds-checked 8-byte write, no W^X flip, no icache
invalidation, no code patch at all. `patch::update_oop_slot()` is that
write; `patch::repatch_ic_site()` is the IC-site mechanism DESIGN.md
actually did ask for as code-patching (a PIC needs to redirect a `BL`, not
route through a data slot) — built directly on `patch_bl_direct`/
`patch_bl_to_trampoline`, which already existed and already worked. Both
are proven end-to-end in `jit/rust/tests/patching.rs`: a JIT'd function
observes a moving-GC-style oop update on its very next call with no
re-link, and a JIT'd call site observes a monomorphic→different-target PIC
transition the same way.

Neither mechanism required touching the vendored `jit/c` QBE fork — both
are a symbol-naming convention (`__macvm_oop$name` / `__macvm_ic$name`)
recognized purely on the Rust side, since QBE already carries whatever
symbol name a frontend chooses all the way through to the collected
`JitInst` stream.

### The text-IL question: it was true, and per follow-up direction, now isn't

Addendum 1 found that every public entry point into the pipeline
(`qbe_compile_il`, `qbe_compile_il_jit`, ...) took text, but that
`jit_collect_fn(JitCollector*, Fn*)` already existed as a real, unused hook
that would let a direct-IR-construction API skip `parse()`'s tokenize/parse
step (not the optimizer pipeline, which still has to run either way) —
exactly what `design/direct_qbe_ir_jit.md` proposed and explicitly deferred,
not because it was infeasible, but because FasterBASIC's own authors
estimated 3,000–3,500 lines / 3–4 weeks for their use case's cost/benefit.

That estimate turned out to be pessimistic for what MACVM actually needs
(not because the original authors were wrong about their own scope — they
were reimplementing Ref/Tmp/Con bookkeeping from scratch; it turns out
`util.c` already exports `newtmp()`/`newcon()`/`intern()`/`newblk()`, doing
exactly that bookkeeping, unused outside `parse.c` only because nothing else
had called them yet). Built as [`jit/c/ir_builder.c`](jit/c/ir_builder.c) +
[`ir_builder.h`](jit/c/ir_builder.h) (~500 lines total) plus a Rust wrapper
([`jit/rust/src/ir.rs`](jit/rust/src/ir.rs) + `ir_ffi.rs`) — [`ir::build_function_jit`](jit/rust/README.md)
constructs a function directly (blocks, instructions, calls, terminators)
with no QBE IL text anywhere, through the identical optimizer pipeline and
`JitInst` collection the text path uses. One deliberate, minimal deviation
from "vendored, unmodified" was needed: `typecheck()` in `parse.c` was
`static`; it's now exported (linkage-only change, declared in `all.h`) so
the builder can run the same validation `parse()` itself already runs on
every function, catching a malformed builder-constructed function via the
existing `err()`/setjmp path instead of an uncaught assertion deeper in the
pipeline. Documented at both edit sites in the source.

Proven end-to-end in `jit/rust/tests/ir_builder.rs`: the same `add` and
`sum_to(n)` programs `tests/integration.rs` builds from text, built instead
via pure Rust builder calls, executing identically — including a mutable
local (`alloc4`+load/store, lifted to SSA by the same `promote()` pass the
text path relies on) and a backward branch. Also proven: a malformed
builder-constructed function (missing terminator) aborts cleanly through the
setjmp guard and leaves QBE's global state usable for the next build —
`jit_collect.c`'s existing safety story, now confirmed to cover the
text-free path too, not just text.

Scope of what's covered: a curated ~84-opcode subset (arithmetic, all-class
comparisons, memory, conversions, stack allocation) — not aggregate
(struct-by-value) parameters, vararg calls, or NEON. See
`ir_builder.h`'s own top comment for the exact boundary and reasoning.
