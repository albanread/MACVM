# Primitive intrinsics — widening the fuse table (the Z arc)

*Written 2026-08-05, after a source-level survey of the Dart 1.24.3 VM's
intrinsifier + recognized-methods machinery (`../MACDARTV1/macdart/runtime/vm/`)
and a CPU-sample profile of MACVM's own runtime split. Companion measurements
in §2; the survey's citations are inline where a Dart precedent motivates a
slice.*

## 1. Motivation, and the honest scope

The three-way bench (2026-08-05, this machine, 7 interleaved rounds, µs/iter
warm — `scripts/xvm-bench.sh`) puts MACVM ahead of Cog on all seven workloads
and behind MACDART (the same Smalltalk on the ported Dart 1.24.3 JIT) on four:
arith 1.9×, fib 1.3×, richards 1.8×, alloc 1.4×. MACVM wins dict 2.3×,
deltablue 1.9×, sieve by a hair.

Two CPU-sample profiles split the remaining gap into two different problems:

| workload | top-of-stack in JIT code | in Rust runtime |
|---|---:|---:|
| richards (dispatch-heavy, 8s steady state) | ~100% | ~0% |
| library loop (OrderedCollection/Dictionary/Set/String/Symbol/Fraction) | 26% | **64%** |

So: **the richards/fib/arith gap is compiled-code quality** — inliner reach and
codegen, the `docs/regalloc_findings.md` / `docs/budgeted_inliner_design.md`
territory — and **no primitive work will move it**. What primitive work *can*
move is the library/collection side, where the named costs are:

```
rt_call_primitive            732 samples   12.5%   the shim trampoline itself
prim_replace_from_to_with    463            7.9%   bulk copy (real work, wrong door)
alloc_words                  348            5.9%   allocation helper
indexable_len + prim_size    412            7.0%   LENGTH READS taking a full call
intern_core                  228            3.9%   symbol interning (inherent)
rt_interpret_call + reentrant~385            6.6%   c2i fallbacks
prim_basic_new(_colon)       177            3.0%   allocation via shim
prim_byte_at_put + tail bytes105            1.8%   byte stores via shim
prim_class                    45            0.8%   `class` as a call-out
```

A length read costing a two-call round trip through a spill-everything stub is
exactly the category of cost Dart's intrinsifier exists to remove. This arc
removes it the MACVM way.

## 2. What Dart actually does (survey summary)

Dart 1.24.3 has a five-rung ladder, 265 recognized methods total
(`method_recognizer.h`):

1. **Parser folds** (`identical`) — the call never exists.
2. **Native-body → single IL node** (`flow_graph_builder.cc:3239` —
   length getters, `ClassID.getID`, `Object.==`): the *body* becomes one
   instruction; ordinary inlining then spreads it into callers.
3. **Graph intrinsics** (64: `_List.[]`, `codeUnitAt`, double rounding) — an
   IL fast path stamped at the callee's entry, falling through to the real
   body.
4. **Asm intrinsics** (90: every integer op, `GrowableArray.add`,
   `String.hashCode`, `OneByteString` allocate/`==`, the Bigint core) —
   hand-written arm64 at the callee's entry, before frame setup
   (`flow_graph_compiler_arm64.cc:999`).
5. **Recognized-inline at the caller** (`flow_graph_inliner.cc:3353`) — a
   monomorphic optimized call site is replaced by raw IL (`LoadIndexed`,
   `StoreIndexed`, `BinaryDoubleOp`) plus one class check.

The finding that shapes this arc: **rungs 3–4 exist mostly for Dart's
*unoptimized* tier and megamorphic sites.** In optimized Dart code, `a + b`
becomes a `BinarySmiOpInstr` from IC feedback (`jit_optimizer.cc:673`) and
never reaches the `Integer_add` assembly at all; a 40-entry blacklist
(`method_recognizer.h:444`) stops the inliner from replacing good intrinsics
with slow Dart bodies. The callee-entry intrinsics pay off because Dart's
unoptimized calls are expensive (usage counters, a 5×-unrolled IC linear scan,
frame setup, spill-slot nulling — `stub_code_arm64.cc:1442`).

MACVM has **no unoptimized compiled tier**: mono sends are patched direct
`bl`s, and the interpreter is the only other tier. So the right translation is
*not* 90 hand-written asm bodies — it is **widening rung 5**, which MACVM
already owns (the fuse table), plus **cheapening the door** for what stays in
Rust. Dart's *target list* is the useful import; its *mechanism* mostly is not.

## 3. What MACVM already has, in those terms

- **Rung-5 fuses** (`ir.rs` translate classification, `driver.rs` id lists):
  `SMI_INLINE` (14 ops), `DOUBLE_INLINE` (6), `ArrayAt`/`ArrayAtPut`/
  `ArraySize`, literal-class `basicNew` inline allocation, `==`/`~~`,
  `BoolNot`, SIMD `VecArith`, float regions. Sometimes *better* than Dart's:
  `range_reduce` strips guards to a 3-instruction unchecked array access,
  where Dart keeps its bounds check.
- **Rung 1–2 analogue**: the frontend compiles `ifTrue:`/`whileTrue:`/
  `to:do:`/`ifNil:` to branch bytecode — they never exist as sends.
- **The door for everything else**: `emit_prim_shim` →
  `stub_call_primitive` → `rt_call_primitive` (`docs/prim_shims.md`) — two
  nested calls, a full stub frame, 8 RootSpill stores + reloads, GC-anchor
  writes, kind tag, NLR check. 137 of 157 primitives pay it on every call
  from compiled code.

**The coexistence model is already proven.** `Array>>size` (prim 28) is fused
at mono-Array sites via `resolve_method_ro(vm, guard_klass, selector)` — *not*
via the raw IC target — and is **not** in `PRIM_ALREADY_FUSED`, so the same
method keeps its shimmed nmethod for generic/megamorphic callers. Every new
fuse in this arc follows the prim-28 model: resolve by (guard klass,
selector), leave shimmability alone, fail edge = reexecute `UncommonTrap`.
(`is_smi_inlinable` is the one legacy gate still reading the raw IC target;
migrating it is Z6's prerequisite, not Z1's.)

## 4. Decisions of record

- **Z-D1 — caller-side fusion, not callee asm.** New special-casing lands as
  translate-stage fuses over existing or new `Ir` ops. No hand-written
  per-primitive machine bodies; `emit.rs` grows ops, not methods. Rationale:
  §2's finding, plus MACVM's differential gate is per-send-site semantics —
  a fuse is testable byte-identically, an asm body is a parallel
  implementation to keep honest forever.
- **Z-D2 — the prim-28 coexistence model everywhere.** No additions to
  `PRIM_ALREADY_FUSED` in this arc. A fused-and-shimmed primitive serves mono
  sites inline and everyone else through the existing shim.
- **Z-D3 — traps only where re-execution converges.** A fail edge may be an
  `UncommonTrap` only if one interpreted re-execution repairs the world
  (wrong klass → recompile; genuine fallback → Smalltalk body). A path that
  would trap *per object* (lazy `identityHash` install on fresh objects — a
  Set-insert storm) must use a call fallback instead, and waits for the slice
  that builds one (Z7).
- **Z-D4 — bulk work stays in Rust, behind a cheaper door.** `hashBytes`,
  `compare:`, `replaceFrom:to:with:` are memcmp/memcpy-bound; Dart leaves
  their equivalents to C too. The win is a leaf-call tier (Z5), not inlining.
- **Z-D5 — every slice is A/B-gated and reversible.** Byte-identical
  differential (off vs threshold=1) + full world tests under both GC-stress
  modes + before/after numbers on the library driver and `xvm-bench.sh`. A
  slice that doesn't pay gets the `regalloc_findings.md` treatment: recorded
  and reverted.

## 5. The slices

### Z1 — the pure-read fuses: `class` (21) and byte-`size` (42/28-on-bytes)

The smallest possible landing that retires a measured cost (~8% of the
library profile between them, `prim_class` + `indexable_len`/`prim_size`).

- **`class`**: at a mono site whose guard klass K resolves `#class` to prim
  21, the answer *is K*: `GuardKlass(recv, K)` + push pool-constant K. Smi
  guard uses `GuardShape::SmiTest` and pushes the SmallInteger klass.
  After fusion, `x class` feeds `RefCmpBr` for free in `= `-style bodies.
- **byte `size`**: `array_size_op` generalized — a guard klass of format
  `IndexableBytes` whose resolved target is prim 42 (`byteSize`) or 28
  (`size`) fuses to `GuardKlass` + `LoadField{size_slot}`, the size-slot
  byte offset computed from the guard klass's `non_indexable_size()` (Array
  hardcodes 16 today; the general formula subsumes it). Covers
  String/Symbol/ByteArray length reads — `scanFor:`, streams, copies.
- `_on` twins for both, so spliced/inlined bodies fuse identically (the
  `ir.rs:1290` lesson: a missing `_on` twin cost Mandelbrot 12× once).

### Z2 — byte element access: `byteAt:` (40) / `byteAt:put:` (41)

`ArrayAt`'s shape one format over: new `Ir::ByteAt`/`ByteAtPut{,NC}` — klass
guard, smi index guard, bounds vs the size slot, then `ldrb` + smi-tag (at:)
or untag + range-0..255 guard + `strb` (at:put:; no card barrier — bytes are
not oops). `byteAt:put:` on a Symbol-klass guard must not fuse (the prim
fails there — immutability). Feeds `String>>at:`'s `basicByteAt:` leg,
WriteStream `nextPut:`, Symbol scanning. Follow-on measurement: whether
`Character value:` (the second leg of `String>>at:`) warrants a fuse of its
own via the char table, or whether inliner reach covers it.

### Z3 — `bitShift:` (9)

The one integer op outside `SMI_INLINE`. Its own gate (prim-28 model — *not*
added to `SMI_INLINE`, whose membership `PRIM_ALREADY_FUSED` copies and
`eligibility_detail` reads): `Ir::SmiShift` with Dart's exact bail set
(`Integer_shl/sar`, `intrinsifier_arm64.cc:515/:639`): negative or
≥-wordsize counts, and left-shift overflow via shift-back-and-compare.

### Z4 — allocation: `basicNew:` (24) and customized-`self basicNew`

Two halves of `docs/gc_alloc_gap.md`'s finding (~28 ns/object through the
shim vs 2–3 ns inline):
- `basicNew:` with a dynamic length: inline eden bump with a size clamp and
  slow-path call — Dart does exactly this shape as asm
  (`TYPED_ARRAY_ALLOCATION`, `intrinsifier_arm64.cc:182`); ours is the
  existing `Ir::Alloc` grown a dynamic-size operand for `IndexableOops`/
  `IndexableBytes`.
- `alloc_site_klass` extended from literal-class receivers to the customized
  `self` of a class-side method — the gap doc's own recommendation, and the
  reason every real `Klass new` constructor misses today's fuse.

### Z5 — the leaf door: a no-spill call tier for leaf primitives

For prims that cannot allocate, cannot fail, never re-enter, and touch no
Smalltalk stack (`PrimDesc` already records `can_allocate`/`can_fail`):
call the Rust fn directly — args already sit in x0..x7, C ABI-compatible —
with no RootSpill traffic, no anchor writes, no kind tag, no NLR check.
`rt_call_primitive` today re-reads every arg from memory the stub just
stored; the leaf tier deletes both halves. Targets the 12.5%
`rt_call_primitive` line across *all* remaining shimmed prims at once —
`hashBytes`, `compare:`, `byteSize`-on-poly-sites, clocks. (A second flag,
`can_gc_unsafe`, may be needed for prims that read heap but never allocate;
the slice's design step settles the exact predicate set.)

### Z6 — the overflow fail edge: deopt → call

`SmiArith`'s overflow/fail edge is a full `UncommonTrap` today: 20-factorial
class workloads pay deopt + interpreted `SmallInteger>>+` + Smalltalk
LargeInteger fallback every iteration (`docs/PERF.md`'s 7.9× row vs >100×
for non-overflowing arith). `docs/next_architecture.md` already prescribes
the fix — *"a fused fast path falling through to an ordinary call isn't a
reconstruction, it's just a call."* Prerequisites, in order: (a) migrate
`is_smi_inlinable` to `resolve_method_ro` (the last raw-IC-target gate);
(b) lift the smi ids out of `PRIM_ALREADY_FUSED` so `SmallInteger>>+` may
own a shimmed nmethod; (c) point the fail edge at a plain `CallSend` to it.
Each step is independently differential-gated.

### Z7 — stretch: `identityHash` with a call fallback, float odds-and-ends

- `identityHash` (20): fast path = mark-word hash load, zero → *call* (per
  Z-D3, never trap). Wants either a two-outcome fuse shape (fast else
  CallSend in-line) or a tiny dedicated stub. Backs `Symbol>>hash`, i.e.
  Dictionary probes on symbol keys.
- `sqrt`/`floor` (106/107) and `asDouble` (108) into the float-region
  machinery — already named as follow-ons in
  `docs/float_fastpath_design.md`.

## 6. Gates (every slice)

```
cargo test
MACVM_JIT=off  just run-world-tests > /tmp/z_off.txt
MACVM_JIT=threshold=1 just run-world-tests > /tmp/z_t1.txt
diff /tmp/z_off.txt /tmp/z_t1.txt                      # byte-identical
MACVM_GC_STRESS=1        MACVM_JIT=threshold=1 just run-world-tests
MACVM_GC_STRESS=full:64  MACVM_JIT=threshold=1 just run-world-tests
```

plus the A/B record: the library-loop driver profile (Rust-side sample share
must fall, not shift) and a `xvm-bench.sh` spot run appended per slice to
this doc's measurement log. A slice with a flat or negative A/B is reverted
and recorded, per Z-D5.

## 7. Measurement log

### Z1 — `class` + byte-`size` fuses (landed 2026-08-05)

Implementation: `ir.rs` gates `class_const_op` / `byte_size_op` (prim-28
coexistence model, `resolve_method_ro` by guard klass) + two translate arms
(GuardKlass + pool-constant / GuardKlass + LoadField{computed size slot}).
No new `Ir` ops, no emit changes, no `PRIM_ALREADY_FUSED` changes. `_on`
twins for spliced bodies deliberately deferred to a Z1.1 follow-up — the
top-level arms alone were measurable, and the twin work should ride with
Z2's arms to touch the three splice sites once.

Gates: differential off-vs-threshold=1 **byte-identical** (6200 world tests,
0 failed); `MACVM_GC_STRESS=1` and `=full:64` with `threshold=1` both green;
`cargo test --release` green.

A/B (library composite, `runAll: 30000`, third warm pass, 3 process runs
each, this machine):

| row | base | Z1 | note |
|---|---:|---:|---|
| Random generation | 31–32 ms | **28 ms** | number-coercion `class` sends |
| String building (WriteStream) | 13 ms | **11–12 ms** | byte-`size` reads |
| total | 96–100 ms | **91–94 ms** | every other row ±1 ms |

Library-profile Rust-side share barely moved (63.7% → 64.8%, within noise —
the composite is dominated by `replaceFrom:to:with:`/alloc/intern, which are
Z4/Z5 targets); the *time* moved where the fused sends live. `prim_class`
and `prim_size` left the top-of-stack table entirely.

### Z2 — `byteAt:` / `byteAt:put:` fuses (landed 2026-08-05)

Implementation: `Ir::ByteAt`/`ByteAtPut` (klass-derived `len_off`/`tail_off`
fields), gate `byte_at_op` (declines `byteAt:put:` on a Symbol guard — the
primitive always fails there, and a fuse would trap-storm instead of taking
the Smalltalk error path), translate arm mirroring the Array at:/at:put:
arm, emit lowerings `emit_byte_guards`/`emit_byte_at`/`emit_byte_at_put`
(`ldurb`/`sturb`, smi-tag via `lsl/asr #2`, value guarded as tagged smi
0..=1020, no card barrier — bytes are not oops), plus the five exhaustive-
match sites a new op must join (`uses`/`defs`/`map_uses`/fail-edge alias
rewrites/`successors` — the last is the correctness-critical one: a trap
block unreachable through `successors()` falls out of layout entirely).

Gates: differential byte-identical (6200/0), GC-stress 1 and full:64 green.

A/B (same protocol as Z1; Z1 numbers as the base):

| row | Z1 | Z2 | note |
|---|---:|---:|---|
| Random generation | 28 ms | **10 ms** | see below |
| String building (WriteStream) | 11–12 ms | **9–10 ms** | `String>>at:put:` chain |
| Symbol interning | 14 ms | **11–12 ms** | byte scans |
| total | 91–94 ms | **67–69 ms** | vs 96–100 pre-arc |

The Random 2.8× was not in the design: **`LargeInteger` stores its digits
as bytes behind `byteAt:`/`byteAt:put:`** (`world/07_largeinteger.mst`),
and `Random`'s Lehmer step runs in exact integer arithmetic — so the byte
fuse compiled the digit loops of the whole bignum layer, not just strings.
Fraction arithmetic (7 ms) barely moved despite also being
LargeInteger-adjacent — its time is dominated by `gcd:` smi arithmetic,
already fused pre-arc.

### Z3 — `bitShift:` fuse (landed 2026-08-05)

`Ir::SmiShift` + `smi_shift_op` gate (own gate, NOT via `SMI_INLINE` —
adding 9 there would also add it to `PRIM_ALREADY_FUSED` and strip the
method's shim from poly/mega callers). Lowering: both-smi guards, the
primitive's own -61..=61 count window as one unsigned range check on the
tagged count (`count+244 <=u 488`), left shift on the tagged value with
shift-back-and-compare overflow bail (on tagged 64-bit values that IS the
smi-range check), right shift as `asrv` + `and ~3` to clear shifted-in tag
bits. The shift-back compare runs entirely in x19/x20 scratch: `dst` may
alias `a`'s register when `a` dies at this op, so `d` is written exactly
once, after every read of the sources.

Gates: differential byte-identical (6200/0), GC-stress 1 + full:64 green.
A/B: Random generation 10 -> 6-7 ms (the Lehmer step's LargeInteger
normalization shifts), library third-pass total 67-69 -> 64-65 ms.
