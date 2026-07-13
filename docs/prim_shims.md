# Primitive Shims — compiling `<primitive: N>` methods

Status: implemented and committed (8402ef8). Design synthesised from a four-part read-only
review of the as-built code (walker/GC subsystem, shim mechanism, test harness,
primitive semantics). This document is the authority for *why* the mechanism is
shaped the way it is and *which* primitives it covers.

## 1. Motivation

Before this work, `driver::eligibility_detail` rejected **every** method carrying
a `<primitive: N>` pragma as `NoPermanent` (the `method.primitive() != 0` gate):
a primitive-bearing method was permanently interpreter-only. A compiled caller
sending to such a method therefore took a c2i bailout into the interpreter just
to run the primitive, then (usually) returned straight back to compiled code.

The user's ask: "create prim shims the compiler can call … for all the prims
that make sense and are not already compiled." This work lets a primitive-bearing
method acquire its own tier-1 nmethod whose entry prologue calls the primitive
directly through one shared runtime stub, so compiled→compiled dispatch to a
primitive method stays compiled.

## 2. Mechanism (as built)

A shimmable primitive method compiles to an ordinary tier-1 nmethod **plus a
one-time prologue prefix** emitted before the method's own bytecode body — because
Smalltalk always attempts the primitive before the fallback body runs.

- **Eligibility** (`driver.rs`): `eligibility_detail` accepts a primitive method
  iff `method.primitive() == 0 || is_shimmable_primitive(method.primitive())`.
- **Codegen** (`emit.rs::emit_prim_shim`, called once from `emit` after the frame
  prologue + nil-fill, before the block loop):
  ```
  movz x10, #prim_id
  movz x11, #argc_plus_recv          ; = method.argc() + 1 (receiver)
  bl   stub_call_primitive           ; x0..x5 still hold receiver+args (D5.1)
  <safepoint pc; empty oopmap>       ; see §4
  cmp  x16, #PRIM_FAIL_SENTINEL
  b.eq fail                          ; -> fall through to the method's own body
  mov  x0, x16                       ; Ok: return the primitive's result
  b    epilogue
  fail:                              ; body loop begins here (x0 = self, intact)
  ```
- **Runtime stub** (`stubs.rs::build_stub_call_primitive`, kind
  `KIND_CALL_PRIMITIVE = 7`): `emit_stub_prologue` archives x0..x7 into RootSpill
  and sets the anchor (`last_compiled_fp = stub fp`, `last_compiled_pc =` the
  shim's return pc, `last_compiled_kind = KIND_CALL_PRIMITIVE`); then it moves the
  scalar args into x0(vm)/x1(prim_id)/x2(argc) and calls `rt_call_primitive`;
  `mov x16, x0` preserves the tagged result across `emit_stub_epilogue` (which
  clears the anchor and reloads x0..x7 from RootSpill). It deliberately does **not**
  override x0 the way `stub_alloc_slow` does — on the Fail path x0..x5 must arrive
  back at the method exactly as the caller had them.
- **Runtime fn** (`stubs.rs::rt_call_primitive`): reads receiver+args back out of
  RootSpill (`[last_compiled_fp − ROOTSPILL_BYTES + 8·i]`, `i < argc_plus_recv`),
  looks up `prim_by_id`, calls `(desc.f)(vm, &buf[..n])` exactly as
  `interpreter::send::try_primitive` does, and returns the raw oop bits on `Ok`,
  `PRIM_FAIL_SENTINEL` on `Fail`, and `unreachable!()` on `Activated` (excluded by
  eligibility, §3).

Registers `x10`/`x11` carry the scalars specifically because x0..x5 are the
method's live receiver+args; they survive `emit_stub_prologue` (saves x0..x7 only)
and `emit_stub_kind_tag` (scratches x9). `PRIM_FAIL_SENTINEL = 0b1010` is a
`RESERVED_TAG`-family value distinct from `BAILOUT_SENTINEL` (0b10) and
`NLR_SENTINEL` (0b0110); a real oop can never collide with it.

## 3. Which primitives are shimmed — the classification

> As-built classification for the ~66-entry table at the time of this work. The
> table has since grown (SIMD, unboxed-float, FFI, and the game group at ids
> 200–215 were added in later passes); those groups are **not** shimmed —
> they're either fused/inlined (float/SIMD), the FFI dispatch path, or
> pane-independent effect primitives (the game group emits a `GameCommand` and
> returns `self`, so a normal interpreted call is fine). The mechanism and the
> exclusion *reasons* below are unchanged; only the counts have moved.

Of the (then) 66 entries in the `PRIMITIVES` table: **44 shimmed, 22 excluded**
(plus the FFI sentinel, which is not a table row). Three exclusion sets, each with
a distinct reason:

### `PRIM_ACTIVATES_FRAME` = {50,51,52,53,54,60,61}
`value`/`value:`/`value:value:`/`value:value:value:`/`valueWithArguments:`/
`ensure:`/`ifCurtailed:`. These return `PrimResult::Activated`: the primitive has
already pushed a *new interpreter frame* and reassigned `vm.regs`. The generic
"call it, branch on Ok-vs-Fail" shim cannot represent "control transferred to a
brand-new activation". *Proven complete*: `activate_block` (the only producer of
`Activated`) is called from exactly these seven primitives and no other.

### `PRIM_ALREADY_FUSED` = SMI_INLINE ∪ {23,26,27}
SMI_INLINE = {1,2,3,6,7,8,10,11,12,13,14,15} (smi arithmetic/comparison),
`basicNew` (23), Array `at:` (26) / `at:put:` (27). Each already has a *more
specialized* send-site fusion in `compiler::ir` (`is_smi_inlinable[_on]`,
`array_op_kind[_on]`, `classify_smi_send`, `alloc_site_klass`) that compiles the
send to inline code instead of a call. Every one of those detectors reads a mono
IC's target via `MethodOop::try_from(ic.target())` and *relies on it succeeding* —
which was always true only because no primitive-bearing method could be
independently compiled. Shimming a fused primitive would give it its own nmethod,
so a caller's mono IC target stops being a plain `MethodOop`, `try_from` returns
`None`, and the fused fast path silently downgrades (or, in `classify_smi_send`,
hits its hard `.expect`). *Proven exact*: this set is byte-for-byte the union of
the six detectors' fused ids — `basicNew:` (24) is **not** fused (only bare
`basicNew` 23 is), so it stays a shimmed generic primitive.

### `PRIM_READS_ARG_BASE` = {93,94}
`gcScavenge` (93), `gcFull` (94). Both return `vm.prim_arg(0)`, i.e.
`vm.stack[vm.prim_arg_base + 0]` — the *relocated* receiver re-read from the rooted
operand-stack slot after the collection they trigger (the S12-step-7 fix). The
compiled shim path **never sets `vm.prim_arg_base`** (only `send.rs`'s interpreted
dispatch does), so a shimmed 93/94 would return a stale, arbitrary oop from
whatever paused interpreter frame `prim_arg_base` last pointed into. They are the
*only* shimmable-by-the-other-rules primitives that read `prim_arg_base`
(`valueWithArguments:`/`ensure:`/`ifCurtailed:` also read/mutate the operand stack
but are already excluded as `Activated`). Kept interpreter-only.

### FFI (`PRIM_ID_FFI` = −1)
A distinct sentinel, not a table row; intercepted before `prim_by_id` because it
carries a variable-arity argument shape the fixed colon-counted `PrimDesc` model
was never built for. Its own later work.

`is_shimmable_primitive(id)` = `id != PRIM_ID_FFI && !PRIM_ACTIVATES_FRAME(id)
&& !PRIM_ALREADY_FUSED(id) && !PRIM_READS_ARG_BASE(id)`.

### Notes on the shimmed set
- **Allocating primitives** (24, 97, 100-103, 106, 108-111, 116, 119, 120) are
  GC-safe by construction — the CallPrimitive stub uses the same anchor+RootSpill
  machinery every Rust-reaching stub relies on; none reads `prim_arg_base`.
- **`quit` (90) / `quit:` (96)** set `vm.exit_requested` and return `Ok` normally
  (the flag is polled by the run loop) — deferred-exit semantics identical to
  interpreted.
- **`error:` (95)** diverges via `raise_guest_fatal` (process-exit, or the S21
  embedded `siglongjmp` recovery, which is safe across compiled frames). Safe to
  shim; the only difference from interpreted is that `print_stack_trace` (which
  walks `vm.stack`, not the native chain) yields a less complete dossier — a minor,
  pre-existing diagnostics gap, not a correctness issue.

## 4. GC safety

When a shimmed primitive allocates (or a stress collection fires) inside
`rt_call_primitive`, the collector's stack walker (`runtime::frames::walk_frames`)
sees the CallPrimitive stub as the innermost native frame (anchor kind
`KIND_CALL_PRIMITIVE`). The walk is a complete, correct peer of every other adapter
kind — verified by grepping every `AdapterKind`/`KIND_*` site:

- `AdapterKind::from_raw`: `KIND_CALL_PRIMITIVE => CallPrimitive`.
- `roots::real_oop_rootspill_slots`: the `CallPrimitive` arm resolves the *method's*
  nmethod by `caller_pc` (= the shim's return pc, inside the method) and reads
  `Nmethod::prim_call_argc_plus_recv` (= `method.argc() + 1`). Unlike the
  `Resolve|C2i|Mega|Dnu` arm there is no per-send `IcSite` — a primitive-call site
  is an unconditional call baked in at compile time, so the count is the method's
  own argc+1, recorded once. `argc ≤ 5` eligibility ⇒ count ≤ 6 ≤ ROOTSPILL_SLOTS
  (8). Immediates (smi args) in scanned slots are skipped by the ordinary root scan.
- Every generic `FrameView`-matching site (each_code_root's Adapter arm, the §2c
  redirect walk, the weak-sweep assert) handles CallPrimitive correctly without
  special-casing.

The empty oopmap the shim's own safepoint records is *correct, not a gap*: at
position 0 no regalloc-tracked vreg is live yet; the receiver+args live during the
call are accounted for by the RootSpill/CallPrimitive arm, not the method frame's
oopmap. The safepoint must still exist so the walk's `Compiled` step
(`oopmap_at(ret_pc)`, exact-match) doesn't panic. OSR entry bypasses the shim
(enters mid-loop via its own prologue), so it never re-runs the primitive.

## 5. Testing

1. **`compiled_prim_shim_ok_and_fail_paths`** (it_tier1.rs) — the two shim exits
   with no GC, so raw `stubs.invoke` at `verified_entry_off` is safe:
   - `class` (21, never fails): compiled result = `klass_of(recv)`, not the `^self`
     fallback → proves the shim ran, not the body.
   - `size` (28, fails on a smi): Array receiver → `Ok(count)`; smi receiver →
     `Fail` → falls through to the `^ -1` body. Both cross-checked against a fresh
     interpreted `run_method`. `size`'s method needs `prim_fails` set (interpreter
     debug-asserts it on the Fail branch).

2. **`compiled_prim_shim_basicnew_under_gc_stress`** (it_gc_jit.rs — the file whose
   helpers already establish a GC-walkable base frame) — GC-safety of the new
   compiled call site. `basicNew:` (24) compiled and driven via the real
   `enter_compiled` (NOT raw `invoke`, which leaves no `TierLink::IntoCompiled` and
   would panic the walker at the call_stub boundary), under `gc_stress: true` so
   every allocation inside the shim forces a scavenge. Setup follows the proven
   canonical pattern: push roots, run a throwaway `idleWarm` (`ret_self`) so a
   sentinel-terminated entry-frame remnant exists for the walk to bottom out on,
   then push the receiver. Compiled customized to `klass_of(receiver)` so the entry
   guard matches. Asserts n-element arrays across N collections and that a co-rooted
   young oop survives (the walker didn't corrupt the heap while walking the
   CallPrimitive frame). This is a count-correctness test: a wrong
   `prim_call_argc_plus_recv` would scan stale registers as oops and crash.

3. **driver.rs unit tests** — `eligible_accepts_shimmable_primitive_method` (21),
   `eligible_rejects_frame_activating_primitive_method` (50),
   `eligible_rejects_already_fused_primitive_method` (1),
   `eligible_rejects_arg_base_reading_primitive_method` (93), and
   `already_fused_covers_smi_inline_and_alloc_and_array_ops` (keeps
   `PRIM_ALREADY_FUSED` ⊇ SMI_INLINE and pins 23/26/27).

## 6. Verification plan

`cargo test --lib --tests` (unit + integration), then a targeted run of the two
new integration tests, then `cargo clippy` and `cargo fmt --check` on the touched
files. Committed as 8402ef8.
