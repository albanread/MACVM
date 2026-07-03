# qbejit — feature guide

Usage-oriented reference for what this crate can do, as of this session's
additions (the text-free builder and the patching layer). `README.md` covers
provenance and module layout; `../REVIEW.md` covers the investigation
narrative (why each piece exists, what was checked, what's still open). This
document is the third thing: how to actually call it.

All examples below are lifted directly from this crate's own test files
(`tests/integration.rs`, `tests/ir_builder.rs`, `tests/patching.rs`) — copy
from the tests, not from this document, if you want a compiling starting
point; this file trims them for readability.

## The pipeline, either way in

Both entry points below converge on the same four steps:

```rust
let module = encode::encode(collector.instructions(), code_cap, data_cap);
let mut region = memory::JitMemoryRegion::allocate(code_cap, data_cap)?;
let (result, executable) = linker::link_and_finalize(&module, &mut region, runtime_ctx);
let f: extern "C" fn(...) -> ... = unsafe { std::mem::transmute(region.get_function_ptr(offset)?) };
```

`collector` is a [`compile::JitCollectorHandle`] — produced by *either* of
the two entry points in the next section. Nothing past this point cares
which one built it.

## 1. Text entry point (the original port)

```rust
let collector = compile::compile_il_jit(
    "export function w $add(w %a, w %b) {\n@start\n\t%c =w add %a, %b\n\tret %c\n}\n",
    Some("arm64_apple"),
)?;
```

QBE IL text in, `JitCollectorHandle` out. Runs through QBE's real parser,
inside the setjmp guard `csrc/rust_bridge.c` establishes (a malformed string
returns `Err(JitCompileError::ParseAborted)`, not a crash).

## 2. Text-free entry point (new)

```rust
use qbejit::ir::{build_function_jit, op, Cls, FnSpec};

let collector = build_function_jit(
    FnSpec { name: "add", ret_cls: Some(Cls::W), is_export: true },
    Some("arm64_apple"),
    |f| {
        // Parameters first, before any block — same requirement QBE IL
        // text has (a function's `Opar` instructions must lead the
        // entry block).
        let a = f.par(Cls::W);
        let b = f.par(Cls::W);

        let start = f.new_block("start");
        f.set_current_block(start);

        let c = f.ins(op::ADD, Cls::W, a, b);
        f.ret(Some(Cls::W), c);
    },
)?;
```

Same result as the text example above — same optimizer pipeline, same
`JitCollectorHandle` type — with no `.ssa` string formatting or parsing
anywhere. `build_function_jit` builds and compiles exactly **one** function
per call (matching an adaptive tier's actual unit of recompilation: one hot
method at a time).

### Building a CFG: allocate blocks before you fill them in

`new_block` allocates a block **without** making it current — get handles
for every block you'll need to jump to before you start filling any of them
in, then `set_current_block` each one when you're ready to emit into it.
This is what makes forward branches (loops, `if`/`else`) possible without a
name-based lookup table:

```rust
|f| {
    let n = f.par(Cls::W);

    let start = f.new_block("start");
    f.set_current_block(start);
    // Each `f.con_*`/`f.ins*` call takes `&mut self`, so — same rule as
    // any other Rust method chain — bind the constant to a local before
    // passing it to the next `f.*` call; you can't nest them.
    let four = f.con_int(4);
    let sum_slot = f.alloc(4, four); // mutable local #1
    let four = f.con_int(4);
    let i_slot = f.alloc(4, four);   // mutable local #2
    let zero = f.con_int(0);
    f.store(op::STOREW, zero, sum_slot);
    let one = f.con_int(1);
    f.store(op::STOREW, one, i_slot);

    // Allocate the rest of the CFG's blocks now, before finishing `start`.
    let loop_blk = f.new_block("loop");
    let body_blk = f.new_block("body");
    let done_blk = f.new_block("done");

    f.jmp(loop_blk); // closes `start`

    f.set_current_block(loop_blk);
    let i_val = f.ins1(op::LOAD, Cls::W, i_slot);
    let cond = f.ins(op::CSGTW, Cls::W, i_val, n);
    f.jnz(cond, done_blk, body_blk); // closes `loop`

    f.set_current_block(body_blk);
    let sum_val = f.ins1(op::LOAD, Cls::W, sum_slot);
    let i_val2 = f.ins1(op::LOAD, Cls::W, i_slot);
    let sum1 = f.ins(op::ADD, Cls::W, sum_val, i_val2);
    let one = f.con_int(1);
    let i1 = f.ins(op::ADD, Cls::W, i_val2, one);
    f.store(op::STOREW, sum1, sum_slot);
    f.store(op::STOREW, i1, i_slot);
    f.jmp(loop_blk); // closes `body` — back-edge to an already-closed block, which is fine

    f.set_current_block(done_blk);
    let result = f.ins1(op::LOAD, Cls::W, sum_slot);
    f.ret(Some(Cls::W), result);
}
```

A block, once made current and then left (by a terminator or the next
`set_current_block` call), is closed **permanently** — the same one-shot
constraint QBE IL text has (its parser rejects "multiple definitions of
block @name" the same way). Reopening a non-empty closed block is a caller
error the C side detects and rejects via a clean `err()`, not something it
silently corrupts.

### Mutable locals: `alloc` + load/store, not explicit Phi nodes

There's no Phi-node constructor in this API. Represent a mutable local the
way `sum_slot`/`i_slot` do above: `f.alloc(align, size)` once, then
`load`/`store` through it everywhere. `build_function_jit` runs the same
`promote()` optimizer pass the text path does, which lifts SSA-promotable
stack slots into real SSA values automatically — simpler to emit correctly
than hand-built Phi nodes, and no slower once `promote()` has run.

### Instruction reference

| Method | Shape | Notes |
|---|---|---|
| `f.con_int(i64)` | constant | any class — same constant can back a `w` or `l` use, matching a bare integer literal in QBE IL text |
| `f.con_double(f64)` / `f.con_single(f32)` | constant | |
| `f.con_addr(sym, addend)` | constant | address of a global symbol + byte offset; resolved later by `linker` — see "Patching", below, for the `__macvm_oop$`/`__macvm_ic$` naming convention this feeds |
| `f.ins(op, cls, a0, a1)` | 2-arg | arithmetic, compare, memory; allocates a fresh destination temp |
| `f.ins1(op, cls, a0)` | 1-arg | `neg`, `ext*`, `trunc*`, `*tosi`/`*tof`, `cast`, `copy`, `load*` — shorthand for `ins(op, cls, a0, Ref::NONE)` |
| `f.store(op, value, addr)` | store | `op` ∈ `{STOREB,STOREH,STOREW,STOREL,STORES,STORED}`; **value first, then address** (easy to get backwards) |
| `f.alloc(align, size)` | 1-arg | `align` ∈ `{4, 8, 16}`; returns a pointer-width (`l`) temp |
| `f.par(cls)` | — | next parameter, entry block only, before any other instruction |
| `f.arg(cls, value)` + `f.call(func, ret_cls)` | — | args declared immediately before the call they belong to, matching QBE IL text's own ordering; `ret_cls: None` for a void call |
| `f.jmp`/`f.jnz`/`f.ret`/`f.hlt` | terminator | close the current block |

`op::*` is a curated ~84-opcode subset — full integer/float arithmetic, all
four class (`w`/`l`/`s`/`d`) comparisons, memory access, type conversions,
stack allocation. **Not included**: aggregate (struct-by-value)
parameters/arguments/returns, vararg calls, NEON ops. See
`jit/c/ir_builder.h`'s top comment for the exact reasoning on each.

### Safety

A malformed function — bad types, a missing terminator, anything
`typecheck()` or the optimizer pipeline itself would reject — aborts the
*whole* `build_function_jit` call cleanly (`Err(JitCompileError::ParseAborted)`)
instead of crashing the process, and the C side's global state is fully
usable again for the very next call. Proven in
`tests/ir_builder.rs::malformed_function_aborts_cleanly_instead_of_crashing`:
an intentionally unterminated function is built, aborts, and a second,
valid function is built and compiled successfully immediately after in the
same test.

`build_function_jit` and `compile_il_jit` share one process-wide lock (both
touch QBE's same C globals) — calls from different threads serialize
automatically; you don't need your own mutex around either.

## 3. Patching (new): oops and inline caches

Neither of the above existed in the Zig original this crate was ported
from — added for MACVM's adaptive tier, closing the reloc-kind gap
`../REVIEW.md` identifies against `docs/DESIGN.md` §5. Full design
rationale is in `src/patch.rs`'s own module doc; this is the how-to.

### Oop slots (moving-GC-safe)

Give a `data` symbol a name starting with `__macvm_oop$`, and it's
automatically tracked. **Defining** the data object currently requires text
IL — the IR builder (`ir_builder.h`) has no `data`-definition API yet, only
function construction, so it can't create a new oop slot by itself.
*Reading* the slot works from either entry point equally, though: a
function built via `build_function_jit` can `con_addr("__macvm_oop$widget",
0)` to get the address of a slot that some other, text-compiled definition
created, exactly like `f.con_addr` would for any other external symbol —
it's specifically the slot's own definition that's text-only right now, not
the code that reads it.

```qbe
export function l $get_oop() {
@start
	%v =l loadl $__macvm_oop$widget
	ret %v
}
data $__macvm_oop$widget = { l 111 }
```

```rust
let module = encode::encode(collector.instructions(), 4096, 4096);
assert!(module.oop_slots.contains_key("__macvm_oop$widget"));

// ... allocate region, link_and_finalize ...

// A GC move: update the slot in place. No re-link, no W^X toggle (the
// data region is always writable) — the *already-executing* function
// observes the change on its very next call.
patch::update_oop_slot(&module, &mut region, "__macvm_oop$widget", new_address)?;

// Or just read the slot's live address (e.g. to register it as a root
// up front, before any move happens):
let addr = patch::oop_slot_address(&module, &region, "__macvm_oop$widget")?;
```

Why a data slot instead of a code-embedded constant: MACVM's GC *moves*
objects. Re-encoding a multi-instruction immediate load in the code stream
on every move means a W^X flip, an instruction patch, and an icache flush —
per moved object, per referencing method. Routing the oop through a data
slot instead means a GC move is one bounds-checked 8-byte memory write to a
region that was never subject to W^X in the first place.

### Inline-cache sites (code-patching)

Name a call's target symbol starting with `__macvm_ic$` and it's tracked
the same way, keyed by name instead of by symbol address:

```qbe
export function w $dispatch() {
@start
	call $__macvm_ic$send1()
	ret 0
}
```

```rust
assert!(module.ic_sites.contains_key("__macvm_ic$send1"));

// Redirect the site — a PIC transition, or attaching the first real
// target to a site that started out unresolved.
patch::repatch_ic_site(&module, &mut region, "__macvm_ic$send1", target_a_addr)?;
dispatch();

patch::repatch_ic_site(&module, &mut region, "__macvm_ic$send1", target_b_addr)?;
dispatch(); // now calls target_b
```

`repatch_ic_site` handles the whole W^X sequence itself (`make_writable` →
patch → `make_executable`, which also re-flushes the icache) — tries a
direct `BL` first, falls back to writing a new trampoline stub if the new
target is out of `BL`'s ±128MB range. Built on `memory::JitMemoryRegion`'s
`patch_bl_direct`/`patch_bl_to_trampoline`, which already existed before
this session — `patch.rs` is the missing "find this site again by name"
layer on top, not new low-level machinery.

Unlike oop slots, an IC site's *definition* isn't text-only: `f.arg(...)` +
`f.call(f.con_addr("__macvm_ic$send1", 0), ...)` emits the same `Ocall` +
`CAddr` shape a text `call $__macvm_ic$send1(...)` does, and
`crate::encode`'s recognition of the `__macvm_ic$` prefix runs on the
resulting instruction stream identically regardless of which entry point
produced it. That said, `tests/patching.rs`'s IC-site test only exercises
the text path — the IR-builder side of this specific combination
(`qbe_ir_call` feeding an IC-tracked site) is architecturally the same
mechanism but doesn't have its own dedicated test yet; see `README.md`'s
"still owed" list.

## Verification

`cargo test`: 225 passing, zero warnings (Darwin arm64). Coverage specific
to the features above: `tests/ir_builder.rs` (text-free path, 3 tests),
`tests/patching.rs` (both patch mechanisms, 2 tests) — see `README.md`'s
"What's verified, and what's still owed" for the precise, current state and
what's explicitly *not* covered yet (float/NEON, multi-function builds,
`ir_builder`-side call/trampoline paths).
