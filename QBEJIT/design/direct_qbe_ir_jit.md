# Direct QBE IR Construction for JIT Mode

**Status:** Future improvement — not scheduled  
**Impact:** Eliminates IL text serialization/deserialization overhead in the JIT compile pipeline  
**Risk:** High — tightly coupled to QBE internals; must exactly reproduce parser's IR construction  

## Problem

The current JIT pipeline has a wasteful text round-trip:

```
AST
 → codegen (Zig)          — formats IL as text via allocPrint / QBEBuilder
 → il_text: []const u8    — ~2–10 KB of formatted QBE IL string
 → qbe_compile_il_jit (C) — calls parse() which tokenizes and re-parses the text
 → Fn / Blk / Ins (C)     — QBE internal IR structs
 → optimizer pipeline      — 25 passes (SSA, GVN, GCM, regalloc, isel, …)
 → jit_collect_fn (C)     — walks final IR → JitInst[]
 → jitEncode (Zig)        — ARM64 machine code in memory
```

Steps 2–4 are redundant: the Zig codegen already knows exactly what
instructions, blocks, temporaries, and constants it wants. Formatting
them as text and immediately re-parsing that text wastes time on string
allocation, formatting, tokenization, and symbol lookup.

### Measured cost (factorial 18! benchmark, 10M reps)

| Phase               | Time     |
|----------------------|----------|
| QBE IL codegen       | 0.186 ms |
| JIT compile/encode   | 1.988 ms |
| — of which: parse()  | ~0.3 ms (estimated) |
| **Text overhead**    | **~0.5 ms** |

For this small program the overhead is modest (~20% of compile time).
For larger programs with thousands of IL lines it would be proportionally
larger, and the allocation pressure from hundreds of `allocPrint` calls
would be significant.

## Proposed Design

Replace the text-emitting `QBEBuilder` with a `QBEIRBuilder` that
constructs QBE's C-side IR structs directly via a new C builder API,
then hands the populated `Fn*` to the optimizer pipeline (skipping
`parse()` entirely).

```
AST
 → codegen (Zig)           — calls C builder API to construct IR directly
 → Fn / Blk / Ins (C)      — QBE internal IR structs (no text involved)
 → optimizer pipeline       — same 25 passes as today
 → jit_collect_fn (C)       — JitInst[]
 → jitEncode (Zig)          — ARM64 machine code
```

### C-side builder API (new file: `qbe/ir_builder.c`)

Thin wrapper functions that allocate and wire up QBE's internal structs
using QBE's own memory pools (`alloc()`, `vnew()`):

```c
// Lifecycle
IrBuilder *qbe_ir_new(const char *target_name);
void       qbe_ir_free(IrBuilder *b);

// Function construction
void  qbe_ir_func_begin(IrBuilder *b, const char *name, int retcls, int export);
Fn   *qbe_ir_func_end(IrBuilder *b);    // returns completed Fn* for optimizer

// Block construction
Blk  *qbe_ir_blk_new(IrBuilder *b, const char *name);
void  qbe_ir_blk_set_current(IrBuilder *b, Blk *blk);

// Temporaries and constants
Ref   qbe_ir_tmp_new(IrBuilder *b, const char *name, int cls);
Ref   qbe_ir_con_int(IrBuilder *b, int64_t val);
Ref   qbe_ir_con_flt(IrBuilder *b, double val);
Ref   qbe_ir_con_addr(IrBuilder *b, const char *sym_name);

// Instructions
void  qbe_ir_ins(IrBuilder *b, int op, int cls, Ref to, Ref arg0, Ref arg1);
void  qbe_ir_call(IrBuilder *b, Ref to, int cls, Ref fn, Ref *args, int narg);

// Terminators
void  qbe_ir_jmp(IrBuilder *b, Blk *target);
void  qbe_ir_jnz(IrBuilder *b, Ref cond, Blk *yes, Blk *no);
void  qbe_ir_ret(IrBuilder *b, int cls, Ref val);

// Parameters and phi
void  qbe_ir_par(IrBuilder *b, int cls, Ref to);
void  qbe_ir_phi(IrBuilder *b, Ref to, int cls, Blk **preds, Ref *vals, int n);

// Data section
void  qbe_ir_data_begin(IrBuilder *b, const char *name, int export);
void  qbe_ir_data_bytes(IrBuilder *b, const char *str, int len);
void  qbe_ir_data_zero(IrBuilder *b, int nbytes);
void  qbe_ir_data_ref(IrBuilder *b, const char *sym, int64_t off);
void  qbe_ir_data_end(IrBuilder *b);

// Pipeline entry (replaces qbe_compile_il_jit)
int   qbe_ir_compile_jit(IrBuilder *b, JitCollector *jc);
```

### Zig-side IR builder (replace `QBEBuilder`)

The Zig `QBEIRBuilder` would hold a pointer to the C `IrBuilder` and
translate each current `QBEBuilder` method into a direct C FFI call:

| Current (text)                        | Proposed (direct IR)                         |
|---------------------------------------|----------------------------------------------|
| `emitBinary(dest, "w", "add", a, b)`  | `qbe_ir_ins(b, Oadd, Kw, dest, a, b)`       |
| `emitLoad(dest, "l", addr)`           | `qbe_ir_ins(b, Oload, Kl, dest, addr, R)`   |
| `emitStore("l", val, addr)`           | `qbe_ir_ins(b, Ostorel, Kl, R, val, addr)`  |
| `emitCall(dest, "l", "foo", "l %a")`  | `qbe_ir_call(b, dest, Kl, foo_ref, ...)`     |
| `emitBranch(cond, true_lbl, false_lbl)` | `qbe_ir_jnz(b, cond, true_blk, false_blk)` |
| `emitJump(target)`                    | `qbe_ir_jmp(b, target_blk)`                 |
| `newTemp()`                           | `qbe_ir_tmp_new(b, name, cls)`               |
| `emit("  %t =w copy 42\n", ...)`     | `qbe_ir_ins(b, Ocopy, Kw, t, INT(42), R)`   |

The codegen (`ExprEmitter`, `BlockEmitter`) would need its method calls
updated but the overall structure and logic would remain identical.

## Complexity Assessment

### What must be built

| Component                 | Estimated size    | Difficulty |
|---------------------------|-------------------|------------|
| C builder API             | 500–800 lines     | Medium     |
| Zig IR builder struct     | 1,000–1,500 lines | Medium     |
| Data section builder      | 200–300 lines     | Low        |
| Codegen call-site updates | ~600 call sites   | Tedious    |
| Tests / validation        | 500+ lines        | High       |
| **Total new/changed**     | **~3,000–3,500**  | —          |

### Estimated effort: 3–4 weeks

### What makes it hard

1. **`Ref` encoding.** QBE's `Ref` is a packed 32-bit bitfield
   (`type:3, val:29`). The `val` field indexes into `Fn.tmp[]` for
   `RTmp`, `Fn.con[]` for `RCon`, etc. The builder must assign indices
   identically to how `parse()` does it, or the optimizer will
   miscompile or crash.

2. **String interning.** QBE uses `intern()` / `str()` for symbol
   names. The optimizer compares names by interned ID, not string
   content. The builder must use the same interning table.

3. **Tmp/Con array management.** Temporaries and constants are stored
   in dynamically-grown arrays (`vnew()` / `vgrow()`). `Ref.val` must
   exactly match array positions. Off-by-one → wrong register
   allocation or data corruption.

4. **Block linking order.** The parser links blocks in source order
   via `Blk.link`. The optimizer's dominance computation, loop
   detection, and RPO numbering depend on this ordering. The builder
   must reproduce it.

5. **Parameter setup.** Function parameters use `Opar`/`Opare` pseudo-
   instructions at the start of the entry block. The ABI lowering
   passes expect these in a specific format.

6. **245 raw `emit()` calls.** Beyond the structured `QBEBuilder`
   methods, there are 245 places where the codegen formats arbitrary
   IL text directly. Each must be translated to the correct `Ins`
   construction with the right opcode enum, type class, and `Ref`
   arguments.

7. **Validation.** There's no easy way to diff "IR built by parser"
   against "IR built by builder" — QBE's IR structs aren't serializable
   for comparison. Testing requires running both paths and comparing
   final machine code output.

## Intermediate Optimisation (low-effort, do first)

Before tackling direct IR, a simpler change can recover 30–40% of the
text codegen cost with minimal risk:

**Replace `allocPrint` with `bufPrint` in `QBEBuilder.emit()`.**

Currently every IL instruction allocates a heap string via `allocPrint`.
Switching to a pre-sized stack buffer (`var buf: [512]u8`) and
`bufPrint` eliminates hundreds of small allocations per compile.
This is a half-day change that touches only `QBEBuilder` internals.

## Decision

**Not now.** The text round-trip adds ~0.5 ms on small programs and is
dwarfed by execution time. The risk of subtle IR construction bugs
breaking the optimizer is high, and the 600+ call-site migration is
tedious and error-prone.

**Revisit when:**
- Compile latency becomes a bottleneck (e.g., interactive REPL, hot-reload)
- Programs are large enough that IL text exceeds ~100 KB
- A QBE fork or rewrite is already in progress

**Do the `bufPrint` optimisation now** — it's low-risk and captures a
meaningful fraction of the formatting cost.