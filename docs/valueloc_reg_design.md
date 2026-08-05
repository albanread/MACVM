# `ValueLoc::Reg` — let non-oops stay resident across safepoints

*2026-08-05. Design only; not built. The analysis below is what makes it
buildable, and the scoping rule is the whole point.*

## The observation

`sieveOnce` spends 158 of 352 instructions on frame spill/reload (109 stores,
49 loads) against one useful array store per iteration. That is not register
pressure — arm64 has 31 GP registers and the allocator is not running out. It
is a **policy**, enforced always-on, release included
(`regalloc::verify_spill_all`, regalloc.rs:993):

```rust
assert!(!(iv.crosses_safepoint && matches!(iv.assignment, Some(Assignment::Reg(_)))),
        "crosses a safepoint but holds a register (S12's oop-map invariant)")
```

Any interval crossing a safepoint must live in a frame slot. With a safepoint
at every send, every allocation and every loop poll, that is most of a hot
loop.

Two metadata formats force it, and neither is about the hardware:

1. **GC oop maps** describe frame slots only — a live *oop* in a register
   would be a root the collector cannot find.
2. **`ValueLoc`** (scopes.rs:290) is `ConstPool | ConstSmi | FrameSlot | Nil |
   …` — no register variant. Since any safepoint may deopt, *every* live
   value must be in a slot, oop or not.

So a tagged `SmallInteger` induction variable — which the GC has no interest in
— still gets `stur x8, [x29,#-112]` every iteration, on the loop's critical
path. `LiveInterval` already carries `is_oop` (regalloc.rs:40) and
`crosses_call` (:56); the information needed to do better is present and
unused.

## The hard edge that scopes it

The trap handler *does* have the registers: `ArmThreadState64.__x: [u64; 29]`
plus `__fp/__lr/__sp/__pc`, straight out of `uc_mcontext`
(codecache/deopt_trap.rs). So a SIGTRAP deopt could read `x23` directly.

**But not every deopt is a trap.** `FrameView` (deopt.rs:77) carries only
`fp`, `pc`, `nm`, `result` — no register snapshot — and its own doc says `pc`
is the *"Trap pc (uncommon-trap path) or `orig_ret_pc` (**return-path
deopt**)"*. On the return path a frame is unwinding into an invalidated
nmethod; there is no `ucontext`, and a register-only value is unrecoverable.

Return-path deopt targets **call return addresses**. Therefore an interval that
never crosses a call can never be caught by it.

## The rule (refined — `Alloc` is the trap for the naive version)

A first cut said "`!is_oop && !crosses_call`". That is **wrong**, and the
counterexample is allocation. `emit_alloc` is a genuine inline eden bump —
*"addr = &eden.top; obj = *addr; new_top = obj + size; if new_top > end ->
slow"* — but it keeps an **internal `bl`** for the eden-exhausted case, and
regalloc.rs:225 records that live-across vregs *"spill before the internal
`bl`"*. Meanwhile `crosses_call` counts only `CallSend`/`CallRuntime`
(regalloc.rs:52), so an interval spanning an `Alloc` is NOT flagged. The naive
rule would hand it a caller-saved register that the slow path then clobbers —
silently, and only under memory pressure, which is the worst possible way to
find a bug.

Rather than enumerate call-like ops (a list that rots the moment someone adds
one), make clobbering structurally impossible:

> Permit `Assignment::Reg` across a safepoint iff
> **`!is_oop`** — the GC never scans it — **and** the register is
> **callee-saved (x19–x28)**, which the AAPCS64 ABI requires every callee to
> preserve, including the alloc slow path and any runtime helper.

That leaves exactly one open hazard rather than a family of them: the
**return-path deopt**, which has no `ucontext` to read the register from even
though the value is still architecturally there. Two ways to close it, in
increasing order of work:

- **(a) keep `!crosses_call` as well** — no call inside the interval means no
  return address inside it, so return-path deopt cannot land there. Narrow,
  provably safe, and enough for sieve's loop-carried induction variable, which
  is the case the critical-path law says actually costs.
- **(b) have the return-path stub spill x19–x28** into a known frame area and
  point `FrameView.regs` at it. Wider, but it is real work in the unwinder and
  should follow (a) proving the payoff exists.

Build (a) first. Everything else keeps today's behaviour, so the GC invariant
and the return-path contract hold by construction rather than by argument.

## It is a TYPE-BASED policy, and the direction is already proven here

This is not a new mechanism. `assign_residents` (regalloc.rs:823) already runs
a **write-through residency tier**, selected by type: `is_fp` intervals get
`d8`–`d15`, integers get `x21`–`x23` via `LiveInterval::resident_reg`, both
gated on `!crosses_call` and all callee-saved. But — its own comment —
*"The slot stays canonical (write-through)"*.

Write-through saves the **reloads** and not the **stores**. That is exactly the
asymmetry in `sieveOnce`: 109 frame stores against 49 frame loads. And the
store is the loop-carried one (`stur x8, [x29,#-112]`, every iteration), which
is the kind the critical-path law says actually costs.

So the change is narrow: **make the register canonical instead of
write-through, for the types the GC never scans.** The tiers, by what the
collector needs of each:

| type | GC needs | today | possible |
|---|---|---|---|
| constant / `ConstSmi` | nothing | already elided (F3) | — |
| double (`is_fp`) | nothing — never a root | resident `d8`–`d15`, write-through | **canonical register** |
| smi / proven non-oop | nothing — not a root | resident `x21`–`x23`, write-through | **canonical register** |
| heap oop | must FIND it, and a moving GC must UPDATE it | slot-canonical | slot-canonical (register-naming oop maps would also need write-BACK — out of scope) |

The oop row is why this stays type-selective rather than universal: finding a
root in a register needs only a richer oop map, but *relocating* it needs the
collector to write the register back through the trap frame. That is a
different and much larger change, and nothing here proposes it.

The two narrower versions of this same idea — F3's `ConstSmi` slot elision and
the float arc's `d8`–`d15` residency — both landed and both paid. This extends
the identical reasoning to the integer non-oop case, which is the one sieve's
induction variable falls into.

## Sketch

1. `ValueLoc::Reg(u8)` in scopes.rs, encoded/decoded alongside `FrameSlot`.
2. `FrameView` gains `regs: Option<[u64; 32]>` — `Some` on the trap path
   (copied from `ArmThreadState64`), `None` on the return path.
3. Materializer: `ValueLoc::Reg(n)` reads `regs[n]`; if `regs` is `None`, that
   is a **bug, not a fallback** — assert loudly. It should be unreachable
   given the rule above, and an assert is how we find out if the rule is
   wrong.
4. `verify_spill_all` relaxed to exempt `!is_oop && !crosses_call`, with its
   message updated so the invariant it still enforces stays legible.
5. Regalloc allows those intervals to keep a register; `build_deopt_metadata`
   emits `ValueLoc::Reg` for them.

## Gates

`DEOPT_STRESS=64` is the gate that matters — it forces deopt at every site, so
a value recorded in the wrong place surfaces as a wrong answer rather than a
rare corruption. Plus the 4-mode world differential (plain / `GC_STRESS=1` /
`full:64` / `DEOPT_STRESS=64`), byte-identical off-vs-threshold with correct
checksums, and the full release suite.

**Failure mode to respect: silent wrong answers, not crashes.** A value read
from the wrong register materialises as a plausible integer. The checksums are
the detector.

## Expected value — deliberately not promised

Unknown, and this file will not pretend otherwise. Today's card-barrier result
showed twelve instructions and three memory ops *off the critical path* cost
0%, so the ~158 bulk spills may well be absorbed by the core too. The spills
this could remove that plausibly DO cost are the **loop-carried** ones — the
induction variable stored every iteration — which the critical-path law says
are the kind that matter.

Two earlier attacks on this same problem (F3c: `freed=0 of 2047 crossing
intervals`; the deopt-liveness rework) both measured flat — but both worked
*within* the frame-slot-only format. This is the first version that changes the
format, which is why it is worth trying even though its predecessors were not.

Measure with the standard interleaved A/B **and quote the per-benchmark noise
floor** (`critical_path_findings.md`): arith ±0.1%, fib ±0.4%, sieve ±4.8%,
richards ±6.5%, dict ±13.4%. A sieve-only 3% is not a result.
