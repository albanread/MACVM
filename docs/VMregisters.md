# MACVM AArch64 register usage & argument-passing conventions (as implemented)

This documents the **as-shipped** register/calling conventions, current as of
S15, gathered by reading the real compiler/codecache/runtime source (every
claim below is cited to a file:line). It complements
[`arm64.md`](arm64.md) §3, which is the **original, pre-implementation
design proposal** — written before the interpreter and compiler existed.
Where reality diverged from that proposal (it did, substantially), this doc
says so explicitly rather than silently duplicating or contradicting it.

**Scope**: everything below applies to **compiled (tier-1/JIT) code and the
runtime stubs that bridge to/from it**. The interpreter (tier-0) has none of
this — see §6.

---

## 1. Fixed-role registers

| Reg | Role | Established / enforced at |
|---|---|---|
| `x28` | `&VmState` (the "one god-struct", `vm_state.rs:2`) — pinned for the whole compiled-code execution | `build_call_stub`, `src/codecache/stubs.rs:368`: `mov x28, x1` — "`vm -> x28` (D5.1's own convention)". Compiled code and stubs then do fixed-offset `[x28, #N]` loads/stores for `VmRegBlock` fields (`eden_top_addr`, `eden_end`, `poll_flag`, `last_compiled_fp/pc/kind` — full struct in `src/runtime/vm_state.rs:24-70`); offsets defined in `oops::layout`'s `VMREG_*_OFFSET` consts, asserted never to drift from the struct via `vmregblock_offsets_pinned` |
| `x29` / `x30` | FP / LR, standard AAPCS64 | Every compiled method and stub prologue: `stp x29,x30,[sp,#-N]!; mov x29,sp` (e.g. `emit_stub_prologue`, `src/codecache/stubs.rs:71-72`). `x29` doubles as the anchor the S12 GC frame-walker chains through (`walk_frames`, `src/memory/roots.rs`) |
| `x19` | Inline-alloc fast path: object base pointer (callee-saved scratch) | `src/compiler/emit.rs:2089-2094` — asserted by name in tests: `"obj base must be x19 (callee-saved scratch), never x16/x18"` |
| `x20` | Inline-alloc fast path: the **address** of `eden.top` (not its value — S12 step 7 single-source-of-truth), live until the commit `str` | `src/compiler/emit.rs:2090-2091` |
| `x21`–`x23` | S14 perf-recovery **register residency** pool: call-free spilled intervals get a real register instead of a memory round-trip, longest-lived first | `src/compiler/regalloc.rs:627-660`, `assign_residents` — "disjoint from emit's x16/x17/x19/x20 scratches" |
| `x18` | **Darwin platform register — must never appear in emitted code** | AAPCS64/Apple ABI rule (`arm64.md:99`); enforced by an actual assertion in the test suite: `src/compiler/emit.rs:2101-2103`, `"x18 is the Darwin platform register -- must never appear"` |
| `x16` / `x17` | IP0/IP1 — assembler/veneer scratch (far branches); **also** carries payload across a c2i-adapter tail-jump: `x17` = method oop, `x16` = shared-stub address/branch target | `arm64.md:100-101` (IP0/IP1); `src/codecache/adapters.rs:168-170` (`ldr x17,=method; ldr x16,=c2i_shared; br x16`); explicitly documented as never expected to survive an IR-op boundary, `src/codecache/stubs.rs:397-399` |
| `x0`–`x15` | General linear-scan-allocatable pool (`NUM_ALLOCATABLE_REGS = 16`) | `src/compiler/regalloc.rs:608-611`. **Caveat**: `x0`–`x5` are excluded from the S14 residency pool specifically — "ABI argument/result/alloc-slow paths write them mid-body" (`regalloc.rs:632`) |

**Historical note — arm64.md §3 vs. reality**: the original design proposed
fixed roles for `x24`–`x27` (receiver/TOS cache, bytecode pointer,
method/frame descriptor — modeled on Strongtalk/Self's *hand-assembled*
interpreters). That never happened: MACVM's interpreter is ordinary Rust
(§6), not hand-assembled AArch64, so those roles were never realized. Only
`x28` survived into the real convention, and only for **compiled** code.
`x24`–`x27` today have no role of MACVM's own — confirmed by grep, they
appear nowhere except `build_call_stub`'s save/restore, and every register
in that save list is pure pass-through: stored, then reloaded unchanged
(`stubs.rs:357-362`/`382-387`), contrast `x28` right next to them getting a
genuinely *new* value written (`mov x28, x1`) for the callee to use. The
save/restore exists purely as AAPCS64 contract compliance at the one point
control actually crosses from Rust into hand-assembled/JIT code: Rust's own
compiled code may hold a live value in any callee-saved register across the
`extern "C"` call into this stub, and the stub in turn `blr`s into arbitrary
JIT-compiled code free to clobber any of `x19`-`x28`. The whole bank is
saved as one block rather than a hand-picked subset specifically so a
future claim on `x24`-`x27` (another residency register, a new fixed role)
needs zero changes here — unclaimed headroom, not dead weight, and one
fewer distributed invariant to keep in sync (the failure shape §3's
RootSpill history already shows the cost of). Being callee-saved is exactly
what would make a register *good* for a future persistent role (a value
placed there is guaranteed to survive any call, unlike the caller-saved
range `x0`-`x18`, which a callee may clobber with no obligation at all) —
`x21`-`x23` prove this: same callee-saved property, and it's precisely what
the S14 residency pool relies on. "No role" means unclaimed, not
unclaimable.

**If claimed, more likely for a wider residency pool than for more VM
state.** These two look like similar candidates but aren't equally likely:
a *new* VM-state value costs nothing register-wise today — `x28` is a
*pointer* to `VmRegBlock`, so any new state is just one more fixed offset
reached through the same one pinned register (§1); claiming a whole
register to hold one more state value directly would be a regression from
that already-scales-for-free pattern, not a natural extension of it.
Widening the S14 residency pool (`x21`-`x23` → up to `x21`-`x27`) is the
opposite: same mechanism, and already has a proven payoff — it's literally
what fixed a 135x regression (`regalloc.rs:52`, S14 perf recovery). Any hot
method with more than 3 call-free live intervals still spills to memory for
the overflow today; widening the pool directly addresses that, with no new
pattern to invent. A genuinely new `x28`-style pinned-pointer role would
only earn its own register if some other VM-wide value needed
direct-register access specifically to skip `x28`'s own indirection — no
evidence that's a real, measured need today, as opposed to a
plausible-sounding one.

Also stale: `regalloc.rs:610-612`'s own comment says
"`x19`–`x23` unused in v1" — true when written, superseded since by `x19/x20`
(alloc scratch) and `x21`-`x23` (S14 residency) above. Comments drift; this
doc reconciles against the actual code, not any single comment.

---

## 2. Argument-passing conventions — three distinct mechanisms

Easy to conflate; they're genuinely different code with different bounds.

### 2a. Send sites inside compiled code (the common case)
`emit_call_send` (S11), hand-generated **per call site**: `x0` = receiver,
`x1`..`x7` = up to 7 real arguments — a fresh, specific instruction sequence
every time, so it can freely use the whole `x0`-`x7` AAPCS64 argument-GPR
range. Bounded by the send-site check in `eligibility_detail`,
`src/compiler/driver.rs:146`: `InterpreterIc::at(method, ic).argc() > 7` →
`NoPermanent`. Origin story and the real bug that shaped this exact number:
§3.

### 2b. The outermost Rust → compiled bridge (`build_call_stub`)
**One single generic, shared stub** (not per-call-site generated) —
`src/codecache/stubs.rs:354-391`, D4/D5.1 — used whenever ordinary Rust code
(a Doit, the CLI, an external caller) invokes a compiled method for the
first time. It hardcodes an **unrolled loop of exactly 6 iterations**:
```rust
for (i, off) in [(1, 0), (2, 8), (3, 16), (4, 24), (5, 32), (6, 40)] {
    a.emit("cmp", &[x(11), imm(i)]);
    a.b_cond(Cond::Lt, args_done);
    a.emit("ldr", &[x((i - 1) as u8), mem(10, off)]);
}
```
loading up to 6 words (`argv[0..6]`) into `x0`..`x5` — i.e. receiver + 5
args, matching `method.argc() > 5` (`driver.rs:114`).

**Correction (2026-07-08): this is NOT where the "5" originates — the
causality runs the other way.** `docs/sprints/sprint_s10_detail.md` D5.1
states the convention as **pinned at S10 design time**, before this stub
existed: *"receiver in x0, args left-to-right in x1..x5 (argc ≤ 5 by
eligibility)"*. `build_call_stub`'s loop was written to match an
already-decided number, not the source of it. (An earlier version of this
doc claimed the reverse — corrected after checking git history and the
original design doc instead of inferring from the stub alone.)

Why 5 specifically: no comment states it explicitly, but `argc <= 5` sits
in `sprint_s10_detail.md:45-62`'s eligibility list right alongside
`is_block == 0`, `has_ctx == 0`, and a bytecode-length cap explicitly
annotated **"(tunable)"** — every one of those reads as a deliberately
conservative v1 starting bound (S10 was the *first* JIT tier), not a hard
technical wall, matching this project's repeated "cover the common case
first, extend later if measurement demands it" pattern. Most plausible
account: a practical bet that most real Smalltalk methods take ≤5 params,
not a register/ABI necessity — consistent with the surrounding v1-scoped
company it keeps, though not independently confirmed by a citation of its
own.

A method with more than 5 args can never be entered this way, so it's
permanently `NoPermanent` regardless of what `emit_call_send` (2a) could
otherwise support for a mid-compiled-code send. Raising this cap is a matter
of widening this one stub's unroll (or applying the rest-array design from
§2a's sibling discussion in `weekend_work.md`) — a bounded, mechanical
change, structurally simpler than it first appears, though see
`weekend_work.md`'s own caution about *why* care is still warranted here.

Also established here: `x28 = &VmState` (line 368, "D5.1's own convention")
and the full `x19`-`x28`+fp/lr callee-saved save (§4) — this stub is the
*single* place both of those boundary conventions begin.

### 2c. The interpreter's own convention
Stack-based, arity-unbounded, completely independent of §2a/§2b. See §6.

---

## 3. RootSpill — the adapter/stub-boundary argument shadow area

`ROOTSPILL_SLOTS = 8` (`src/oops/layout.rs:397-398`) — an 8-slot,
64-byte stack area every "Rust-reaching" stub (`resolve`, `mega`, `dnu`,
`c2i`, plus others sharing `emit_stub_prologue`) opens in its own prologue:
```rust
a.emit("sub", &[sp(), sp(), imm(ROOTSPILL_BYTES as i64)]);
a.emit("stp", &[x(0), x(1), mem(31, 0)]);
a.emit("stp", &[x(2), x(3), mem(31, 16)]);
a.emit("stp", &[x(4), x(5), mem(31, 32)]);
a.emit("stp", &[x(6), x(7), mem(31, 48)]);
```
(`src/codecache/stubs.rs:70-84`). Purpose: spill the send's `x0`-`x7`
(receiver+args) to memory so (a) they survive the Rust call this stub is
about to make (`x6`/`x7` are ordinary caller-saved scratch in the C ABI —
without spilling them, any ≥6-arg send's tail gets clobbered), and (b) the
GC can find them as ordinary stack-resident oops rather than register
contents mid-call.

**Why 8, not 6 — real history, not an arbitrary constant.** `ROOTSPILL_SLOTS`
used to be 6. **Richards' own 7-arg task initializer** broke it: a c2i
adapter's read of args 6/7 walked *past* the 6-slot area into the stub
frame's own saved `fp`/`lr` — those values happen to satisfy the SmallInteger
tag check, so they flowed silently into the new object's last two ivars as
bogus-but-tag-valid integers. No crash — it surfaced *thousands of sends
later* as an unrelated `doesNotUnderstand:` on a SmallInteger. Fixed in
`f62a1e4` (S15, "BUG D root cause 4") by widening 6→8 and adding §2a's
`argc() > 7` gate as the backstop ensuring the area is never exceeded again.
Full account: `src/oops/layout.rs:383-396`.

**This is a genuinely different mechanism from an ordinary compiled frame's
own spill-slot oopmap (§4)** — don't conflate them. RootSpill exists only
for the *transient window inside stub/adapter code*, not for a compiled
method's own body. The invariant is actively checked, not just assumed:
`src/memory/roots.rs:274-281`, `each_code_root`'s `FrameView::Adapter`
branch, `debug_assert!(n <= ROOTSPILL_SLOTS, ...)` — silent in release
builds, which is exactly the class of gap that let the original 6-slot bug
through undetected for so long.

---

## 4. GC root discovery — three different "preserve registers" shapes

Three structurally distinct mechanisms exist for keeping values alive/found
across a boundary. Worth telling apart explicitly — they look superficially
similar (all "save some registers to the stack") but exist for different
reasons and cover different register sets:

| Mechanism | What it saves | Why | Where |
|---|---|---|---|
| **RootSpill** (§3) | `x0`-`x7` only | A send's own receiver+args, so the GC and the callee (interpreter, via c2i) can find/read them across a Rust call | `stubs.rs:70-84`, shared by `resolve`/`mega`/`dnu`/`c2i` |
| **`build_call_stub`'s full save** | `x19`-`x28` + fp/lr (all AAPCS64 callee-saved) | Generic Rust↔compiled boundary — the compiled callee may use *any* callee-saved register for its own vregs; Rust's view of them must survive regardless of which ones the callee actually touches | `stubs.rs:357-362, 382-387` |
| **Poll-site save** (D5.6) | `x0`-`x15` (the *entire* allocatable pool) | A safepoint poll can occur at literally any point in a compiled method body, with any subset of the allocatable pool live — must preserve all of it, unconditionally | `stubs.rs:393-399` |
| **Ordinary compiled-frame spill slots** | Whatever `regalloc` assigned, per-method | A ordinary compiled method's own locals/temps that didn't get a resident register (§1) | `compiler::emit::spill_offset` (`fp - 8*(slot+1)`), read by the S12 walker in `src/memory/roots.rs:255-268`, `FrameView::Compiled` |

The first three are fixed-shape, boundary-crossing conventions; the fourth
is per-method and described by the S12 oopmap machinery (`oopmap.rs`,
`PcDesc`), not a fixed register list at all.

---

## 5. Register file summary (quick reference)

```
x0 ......  general pool / receiver+args at a send / return value
x1-x5 ..   general pool / args 1-5 at a send
x6-x7 ..   general pool / args 6-7 at a send (2a only, never at the
           build_call_stub boundary — 2b caps at x0-x5)
x8-x15 .   general pool (linear-scan allocatable, no fixed role)
x16 ....   IP0 — veneer scratch; c2i adapter's shared-stub-address/branch
x17 ....   IP1 — veneer scratch; c2i adapter's method-oop payload
x18 ....   RESERVED (Darwin platform register) — never emit code using it
x19 ....   inline-alloc scratch: new object's base pointer
x20 ....   inline-alloc scratch: address of eden.top
x21-x23    S14 register-residency pool (spilled-but-resident intervals)
x24-x27    no persistent role (arm64.md's original proposal for these was
           superseded — see §1); appear only in build_call_stub's generic
           callee-saved save/restore
x28 ....   &VmState — pinned for all compiled-code execution
x29 ....   FP (frame pointer) — also the GC frame-walk chain anchor
x30 ....   LR (link register)
sp .....   16-byte aligned at every public boundary (AAPCS64)
```

---

## 6. The interpreter has none of this

Tier-0 execution (`src/interpreter/`) is **ordinary Rust** — a bytecode
dispatch loop over `ProcessStack` (`src/interpreter/stack.rs`), not
hand-assembled AArch64. None of §1-§5 applies to it: no fixed hardware
register roles, no `x0`-`x7` argument-count ceiling, arity-unbounded by
construction (an ordinary Rust loop pushing values onto the interpreter's
own stack, however many there are).

Concrete illustration — `BlockClosure>>valueWithArguments:`
(`world/04a_blockclosure.mst:21`, primitive 54), unpacking an Array back
into individual argument slots with no arity limit at all
(`src/runtime/primitives.rs:1005-1010`):
```rust
let array_slot = vm.prim_arg_base + 1;
vm.stack.sp = array_slot; // drop the Array argument itself
for i in 0..n {
    vm.stack.push(arr.at(i));
}
crate::interpreter::blocks::activate_block(vm, cl, n)
```
This is *why* every arity cap discussed in §2-§3 is specifically a
**compiled-code** limitation: `driver.rs:135`'s own words, "the
interpreter's stack-based sends handle any arity" — a method that can't be
compiled (any of §2's caps, or a primitive, or a block, or oversized
bytecode) simply keeps running here, correctly, just without a native
fast path.

---

## 7. See also

- [`arm64.md`](arm64.md) §3 — the original pre-implementation design
  proposal this doc supersedes for register-role questions (kept for
  history; §1 above notes exactly where reality diverged).
- [`weekend_work.md`](weekend_work.md) — Gap 2 discusses two concrete
  designs (stack-passed overflow args vs. a fixed-prefix-plus-rest-array
  convention) for raising the §2 caps, now informed by §2b's finding above
  about where the `argc() > 5` number actually comes from.
- `src/compiler/regalloc.rs` — linear-scan allocator, spill policy, S14
  residency post-pass (`assign_residents`).
- `src/memory/roots.rs` — the S12 GC root-walker; `each_code_root` is the
  single place all of §3/§4's mechanisms are consumed.
