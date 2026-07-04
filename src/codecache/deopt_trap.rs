//! S13 step 5 — the `brk`-based uncommon-trap mechanism: `brk` emission, the
//! macOS SIGTRAP handler, and the startup-generated trampolines that escape
//! from signal context into ordinary Rust (`sprint_s13_detail.md` D3/D4/D6).
//!
//! This is `codecache`'s single unsafe island for signal/ucontext work
//! (layer-boundary table: `deopt_trap` "may touch sigaction/ucontext,
//! code-cache bounds, stub generation, JIT toggle" and "must not touch heap
//! allocation, Universe, interpreter"). The handler itself (D3) is
//! async-signal-safe by doing almost nothing: it inspects the fault pc,
//! namespace-checks the `brk` imm, and — for one of *our* traps — rewrites
//! the ucontext to resume in a generated trampoline. Every observable action
//! (allocation, GC, `VmState` mutation, printing) happens *after* sigreturn,
//! in the Rust reached through that trampoline. The handler never allocates,
//! locks, formats a string, unwinds, or touches `VmState`.
//!
//! PAC note (D3, `arm64.md` §5 baseline): PAC is off for VM-internal control
//! flow, so rewriting `__pc`/`__lr` and the trampoline's later frame surgery
//! need no `pacia`/`autia`. That is the reason this whole design is legal as
//! written — stated here so it is not silently assumed.
//!
//! **Step 5 boundary.** This module resolves a trapping pc to its owning
//! nmethod + `DeoptState` and hands off *toward* materialization; the
//! materializer / interpreter-frame reconstruction (D5 M0–M8) is S13 step 6,
//! living in `runtime/deopt.rs`. The handoff seam here is
//! [`rt_uncommon_trap`], which resolves the `DeoptState` and then
//! deliberately aborts with a "step 6" marker (see its body).

#![allow(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
use crate::compiler::assembler::xr;
use crate::compiler::assembler::{mem, mem_post, mem_pre, sp, x, Assembler, RelocKind};
use crate::compiler::jasm_assembler::JasmAssembler;
use crate::oops::Oop;

use super::{CodeCache, CodeHandle};

// ── The `brk #imm` uncommon-trap namespace (D3 / sprint doc §"Uncommon trap
//    sites") ─────────────────────────────────────────────────────────────

/// MACVM claims the `brk #0xDExx` immediate namespace. The trap **site**
/// (faulting pc → nmethod → `PcDesc::find`) identifies the deopt state; the
/// imm only namespaces our `brk`s against foreign ones.
///
/// `0xDE00` — uncommon trap (guard failure / cold path): deopt + count.
pub const TRAP_UNCOMMON: u16 = 0xDE00;
/// `0xDE01` — forced trap from `MACVM_DEOPT_STRESS`: deopt, counted
/// separately, does NOT count toward `UncommonTrapLimit` (S13 step 11 wires
/// the distinction; step 5 only needs the imm reserved and recognised).
pub const TRAP_STRESS: u16 = 0xDE01;
/// `0xDE02` — compiled-code assertion ("should not reach"): a VM bug; panic
/// with the pc (handled off-signal via a tiny assert stub, D3 step 4).
pub const TRAP_ASSERT: u16 = 0xDE02;

/// AArch64 `brk #imm16` base opcode: `0xD420_0000 | (imm16 << 5)`.
const BRK_BASE: u32 = 0xD420_0000;
/// Mask isolating the opcode bits of a `brk` (everything except the imm16
/// field, which sits in bits 20:5).
const BRK_OPCODE_MASK: u32 = 0xFFE0_001F;
const BRK_IMM_SHIFT: u32 = 5;

/// Encode `brk #imm16`.
pub const fn brk_word(imm16: u16) -> u32 {
    BRK_BASE | ((imm16 as u32) << BRK_IMM_SHIFT)
}

/// Decode a 32-bit instruction word as one of *our* deopt `brk`s. Returns the
/// imm16 only when the word is a `brk` **and** the imm is in our reserved
/// `0xDE00..=0xDE02` range; every other `brk` (notably Rust's `abort()`, which
/// lowers to `brk #1` on arm64, and `unreachable`/`0xDE03..` we never emit) is
/// **not ours** — `None` here drives the handler's SIG_DFL re-raise so those
/// stay fatal (Pitfalls: "Foreign `brk`s").
pub fn decode_deopt_brk(word: u32) -> Option<u16> {
    if word & BRK_OPCODE_MASK != BRK_BASE {
        return None;
    }
    let imm = ((word >> BRK_IMM_SHIFT) & 0xFFFF) as u16;
    match imm {
        TRAP_UNCOMMON | TRAP_STRESS | TRAP_ASSERT => Some(imm),
        _ => None,
    }
}

/// Emit an uncommon-trap `brk #imm` through the S9 `Assembler`. The vendored
/// encoder does not support `brk` (assembler.rs `emit_u32` doc names S13's
/// `brk #imm` traps as exactly the raw-word case), so this goes out as a raw
/// word. Recording the site's `SafepointState` (`kind = UncommonTrap`,
/// `reexecute = true`) is the *emit stage's* job (S13 step 3/7) — this helper
/// only lays down the instruction, so it is equally usable from a real emit
/// path and from this module's own hand-built test stubs.
pub fn emit_brk(a: &mut dyn Assembler, imm16: u16) {
    a.emit_u32(brk_word(imm16));
}

// ── Code-cache bounds: the ONE permitted global (D3 step 2) ───────────────
//
// The handler cannot safely reach a `&CodeCache` from signal context, so the
// region's `[lo, hi)` is cached in two write-once atomics at `install`. Two
// `u64` compares against these is the whole "is this pc ours?" test before we
// even look at the instruction word. `Relaxed` is sufficient: they are
// written once, before any compiled code (and therefore any trap) can run,
// and the SIGTRAP that reads them is delivered to a thread that has, by
// definition, already executed past that startup barrier.

static CODE_LO: AtomicU64 = AtomicU64::new(0);
static CODE_HI: AtomicU64 = AtomicU64::new(0);

/// Absolute address of the generated `deopt_uncommon_trampoline` (D4). The
/// handler sets `__pc` to this to resume there after sigreturn. Write-once at
/// `install`; a value of 0 means "not installed", which the handler treats as
/// "not ours" (so a stray trap before install cannot jump through a null).
static UNCOMMON_TRAMPOLINE: AtomicU64 = AtomicU64::new(0);

/// Absolute address of the tiny assert stub the handler redirects `0xDE02`
/// traps to (D3 step 4 — still "no work in-handler"; the stub calls
/// [`rt_compiled_assert_failed`], which panics off-signal). Write-once.
static ASSERT_STUB: AtomicU64 = AtomicU64::new(0);

/// `true` once [`install`] has run. Guards against a double `sigaction` and
/// lets the handler cheaply reject traps that predate installation.
static INSTALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// ── The macOS arm64 ucontext, laid out by hand (D3 step 1) ────────────────
//
// `libc` 0.2 does NOT expose `ucontext_t` / `__darwin_mcontext64` /
// `__darwin_arm_thread_state64` on aarch64-apple-darwin (verified against the
// vendored crate). These `#[repr(C)]` mirrors track Apple's
// `<sys/_types/_ucontext.h>` + `<mach/arm/_structs.h>` (the `_STRUCT_MCONTEXT64`
// / `_STRUCT_ARM_THREAD_STATE64` definitions). We only ever read/write fields
// up through the thread-state block, so the NEON/exception sub-structs are
// left as an opaque tail sized to match the real struct — the handler never
// touches them, and getting their *size* right keeps `mcontext64`'s own size
// honest should anything ever take `size_of` of it (nothing in step 5 does).

/// `_STRUCT_ARM_THREAD_STATE64` — the integer register file at a fault. `__x`
/// is x0..x28; `__fp`/`__lr`/`__sp`/`__pc` are the named specials; `__cpsr`
/// + `__pad` complete the 34-word block.
#[repr(C)]
struct ArmThreadState64 {
    __x: [u64; 29],
    __fp: u64,
    __lr: u64,
    __sp: u64,
    __pc: u64,
    __cpsr: u32,
    __pad: u32,
}

/// `_STRUCT_ARM_EXCEPTION_STATE64` — 3 words (far/esr/exception). Read never;
/// present only so `__ss` lands at the right offset within `mcontext64`.
#[repr(C)]
struct ArmExceptionState64 {
    __far: u64,
    __esr: u32,
    __exception: u32,
}

/// `_STRUCT_ARM_NEON_STATE64` — 32×128-bit V registers + fpsr/fpcr. Opaque
/// tail; present only for correct total size. `__v` is `[u128; 32]`; the two
/// trailing u32s are fpsr/fpcr.
#[repr(C)]
struct ArmNeonState64 {
    __v: [u128; 32],
    __fpsr: u32,
    __fpcr: u32,
}

/// `_STRUCT_MCONTEXT64` — exception state, then thread state (`__ss`, the one
/// we touch), then NEON state.
#[repr(C)]
struct Mcontext64 {
    __es: ArmExceptionState64,
    __ss: ArmThreadState64,
    __ns: ArmNeonState64,
}

/// `ucontext_t` — only `uc_mcontext` matters here; the leading fields are
/// laid out exactly so that pointer lands correctly. `uc_mcontext` is a
/// *pointer* to the `mcontext64` (Apple stores it out-of-line and points
/// `uc_mcontext` at `__mcontext_data`), which is why the handler double-
/// dereferences: `(*(*ctx).uc_mcontext).__ss.__pc`.
#[repr(C)]
struct Ucontext {
    uc_onstack: i32,
    uc_sigmask: u32,
    uc_stack: StackT,
    uc_link: *mut Ucontext,
    uc_mcsize: usize,
    uc_mcontext: *mut Mcontext64,
    // Apple appends the inline `__mcontext_data` after this; we never read it
    // (we go through `uc_mcontext`, which points at it), so it is omitted.
}

#[repr(C)]
struct StackT {
    ss_sp: *mut core::ffi::c_void,
    ss_size: usize,
    ss_flags: i32,
}

// ── The SIGTRAP handler (D3) ──────────────────────────────────────────────

/// SA_SIGINFO handler for SIGTRAP. Async-signal-safe: no allocation, no lock,
/// no formatting, no unwinding across the signal boundary. Its whole job:
///
/// 1. Read the fault pc from `(*(*ctx).uc_mcontext).__ss.__pc`.
/// 2. Bounds-check the pc against the cached code-cache range (two `u64`
///    compares), and decode `*(pc as *const u32)` as one of our deopt `brk`s.
///    The code cache is always readable, so the load is safe.
/// 3. Not ours → restore `SIG_DFL` and return; re-execution kills the process
///    with the default disposition (this keeps Rust `abort()` fatal).
/// 4. `0xDE02` (compiled assert) → resume in the assert stub (which panics
///    off-signal). Still no work in-handler.
/// 5. Otherwise (`0xDE00`/`0xDE01`) → **redirect**: stash the trap pc in x16
///    (IP0 — scratch, never live at a safepoint, `arm64.md` §3) and set
///    `__pc` to the uncommon trampoline. All real work happens after
///    sigreturn, in Rust reached via that trampoline.
///
/// # Safety
/// Installed only via [`install`] as SIGTRAP's `sa_sigaction`; the kernel
/// guarantees `info`/`ctx` are valid for a SIGTRAP delivery. Never called
/// from Rust.
extern "C" fn sigtrap_handler(_sig: i32, _info: *mut libc::siginfo_t, ctx: *mut core::ffi::c_void) {
    // SAFETY: `ctx` is the kernel-provided `ucontext_t*` for this SIGTRAP; the
    // field layout mirrors Apple's headers (see the struct docs above). We
    // only read/write `__ss` scalars, all valid for the delivery's lifetime.
    unsafe {
        let uc = ctx as *mut Ucontext;
        if uc.is_null() {
            return; // defensive; the kernel never delivers a null context.
        }
        let mc = (*uc).uc_mcontext;
        if mc.is_null() {
            return;
        }
        let ss = &mut (*mc).__ss;
        let pc = ss.__pc;

        // (2) bounds + our-trap decode. Reject if not installed, out of the
        // code cache, or not one of our reserved brk imms.
        if !INSTALLED.load(Ordering::Relaxed) {
            restore_default_and_return();
            return;
        }
        let lo = CODE_LO.load(Ordering::Relaxed);
        let hi = CODE_HI.load(Ordering::Relaxed);
        if pc < lo || pc >= hi {
            // Not in our code cache — a foreign brk (e.g. a Rust abort in
            // ordinary Rust text). (3) Make it fatal.
            restore_default_and_return();
            return;
        }
        // The code cache is always readable — this load cannot fault.
        let word = core::ptr::read(pc as *const u32);
        let imm = match decode_deopt_brk(word) {
            Some(imm) => imm,
            None => {
                // A brk inside the cache we don't recognise (should not
                // happen, but treat as foreign — (3), stay honest/fatal).
                restore_default_and_return();
                return;
            }
        };

        if imm == TRAP_ASSERT {
            // (4) Redirect to the assert stub, which panics off-signal.
            let stub = ASSERT_STUB.load(Ordering::Relaxed);
            if stub != 0 {
                ss.__x[16] = pc;
                ss.__pc = stub;
            } else {
                restore_default_and_return();
            }
            return;
        }

        // (5) 0xDE00 / 0xDE01 — redirect into the uncommon trampoline with the
        // trap pc stashed in x16 (IP0). PAC is off, so no signing needed.
        let tramp = UNCOMMON_TRAMPOLINE.load(Ordering::Relaxed);
        if tramp == 0 {
            restore_default_and_return();
            return;
        }
        ss.__x[16] = pc;
        ss.__pc = tramp;
    }
}

/// D3 step 3: reset SIGTRAP to its default disposition, so that returning from
/// the handler re-executes the faulting instruction and the process dies with
/// the default SIGTRAP behavior. Async-signal-safe (`sigaction` is on the
/// signal-safe list). Kept as a named helper so every "not ours" arm above
/// reads as one intention.
///
/// # Safety
/// Only reached from within [`sigtrap_handler`].
unsafe fn restore_default_and_return() {
    let mut sa: libc::sigaction = core::mem::zeroed();
    sa.sa_sigaction = libc::SIG_DFL;
    // Best-effort: a failure here cannot be reported from signal context, and
    // the re-executed brk will still trap (just possibly through this handler
    // again — which will loop at most until sigaction succeeds; in practice it
    // never fails for SIG_DFL).
    let _ = libc::sigaction(libc::SIGTRAP, &sa, core::ptr::null_mut());
}

// ── Trampolines (D4 / D6) generated at startup ────────────────────────────

/// Handles to the generated deopt trampolines, published into the same
/// `CodeCache` real nmethods and the SPEC §9 stubs live in. Returned by
/// [`install`] and (in later steps) stashed alongside `Stubs`.
#[derive(Clone, Copy)]
pub struct DeoptTrampolines {
    /// D4: the stub the handler resumes into after a `0xDE00`/`0xDE01` trap.
    pub uncommon: CodeHandle,
    /// D3 step 4: the stub `0xDE02` (compiled assert) redirects to.
    pub assert: CodeHandle,
}

impl DeoptTrampolines {
    pub fn uncommon_addr(&self) -> u64 {
        self.uncommon.base as u64
    }
    pub fn assert_addr(&self) -> u64 {
        self.assert.base as u64
    }
}

/// D4: `deopt_uncommon_trampoline`. Runs right after sigreturn, with the
/// trapped compiled frame intact (FP = its frame) and the trap pc in x16.
///
/// The frame-record trick is load-bearing (D4): before calling into Rust it
/// builds a *walker-visible* frame record whose saved-lr = trap_pc, so while
/// `rt_uncommon_trap` runs (and may GC, S13 step 6+) the S12 stack walker sees
/// the trapped nmethod frame at pc = trap_pc and its oop maps cover it.
///
/// ```text
///   mov  lr, x16                 // saved-lr := trap_pc (lr is dead here)
///   stp  fp, lr, [sp, #-16]!     // push a normal frame record for the walker
///   mov  fp, sp                  //   -> [fp] = trapped_frame_fp, [fp+8] = trap_pc
///   ldr  x2, [fp]                // x2 := fp-of-trapped-frame (= the saved fp)
///   mov  x0, x28                 // &mut VmState (VM-state register, §3)
///   mov  x1, x16                 // trap pc
///   ldr  x16, <rt_uncommon_trap>; blr x16   // Rust; result oop -> x0
///   ldr  x2, [fp]                // reload trapped_frame_fp (call clobbers x2)
///   mov  sp, x2                  // tear down trampoline record AND trapped frame
///   ldp  fp, lr, [sp], #16       // pop the COMPILED frame's own saved {fp,lr}
///   ret                          // return the deopt result to the native caller
/// ```
///
/// "normative in effect, not exact instruction choice" (D4). The teardown
/// discards BOTH the trampoline record and the whole trapped compiled frame:
/// `sp := trapped_frame_fp` lands on the compiled frame's own saved `{fp,lr}`
/// pair (the native caller's fp + the real return address), which the `ldp`
/// pops so the final `ret` returns the deopt result to the compiled method's
/// original caller (the activation is gone after a deopt). `trapped_frame_fp`
/// is reloaded from the stable `[fp]` record *after* the call (x2 is caller-
/// saved and `rt_uncommon_trap` may clobber it), not carried in a register
/// across it.
///
/// NB step 5: `rt_uncommon_trap` `unimplemented!()`s (the step-6 seam), so
/// this teardown never executes yet — it is written correctly now so step 6
/// inherits a sound epilogue rather than a bug.
fn build_uncommon_trampoline() -> crate::compiler::assembler::CodeBlob {
    let mut a = JasmAssembler::new();

    // saved-lr := trap_pc (in x16), so the walker sees pc = trap_pc.
    a.emit("mov", &[x(30), x(16)]);
    // Push { fp, lr(=trap_pc) } and re-root fp — a normal frame record.
    a.emit("stp", &[x(29), x(30), mem_pre(31, -16)]);
    a.emit("mov", &[x(29), sp()]);
    // x2 := the trapped frame's fp (= saved fp we just pushed, = [fp]). This is
    // the FP `rt_uncommon_trap`/materialization reads slots against.
    a.emit("ldr", &[x(2), mem(29, 0)]);
    // Marshal (vm, trap_pc, fp) into x0/x1/x2.
    a.emit("mov", &[x(0), x(28)]); // &mut VmState
    a.emit("mov", &[x(1), x(16)]); // trap pc
                                   // (x2 already holds fp.)
    let lit = a.literal_u64(
        rt_uncommon_trap as *const () as u64,
        Some(RelocKind::RuntimeAddr),
    );
    a.call_far(lit);
    // Teardown (D4): reload the trapped frame's fp from the stable record,
    // then set sp to it so the `ldp` pops the COMPILED frame's own {fp,lr} —
    // discarding both the trampoline record and the trapped compiled frame.
    a.emit("ldr", &[x(2), mem(29, 0)]);
    a.emit("mov", &[sp(), x(2)]);
    a.emit("ldp", &[x(29), x(30), mem_post(31, 16)]);
    a.emit("ret", &[]);

    a.finish()
}

/// D3 step 4: the assert stub `0xDE02` traps redirect to. Trap pc is in x16.
/// Sets up a minimal frame and calls [`rt_compiled_assert_failed`], which
/// panics — so this stub never actually returns. No RootSpill/anchor: the
/// panic tears the process down, so there is nothing for a walker to see.
///
/// ```text
///   stp  fp, lr, [sp, #-16]!
///   mov  fp, sp
///   mov  x0, x28          // &mut VmState
///   mov  x1, x16          // trap pc
///   ldr  x16, <rt_compiled_assert_failed>; blr x16   // -> !
///   brk  #0xDE02          // unreachable belt-and-braces (never executes)
/// ```
fn build_assert_stub() -> crate::compiler::assembler::CodeBlob {
    let mut a = JasmAssembler::new();

    a.emit("stp", &[x(29), x(30), mem_pre(31, -16)]);
    a.emit("mov", &[x(29), sp()]);
    a.emit("mov", &[x(0), x(28)]); // vm
    a.emit("mov", &[x(1), x(16)]); // trap pc
    let lit = a.literal_u64(
        rt_compiled_assert_failed as *const () as u64,
        Some(RelocKind::RuntimeAddr),
    );
    a.call_far(lit);
    // rt_compiled_assert_failed is `-> !`; this trap is never reached.
    emit_brk(&mut a, TRAP_ASSERT);

    a.finish()
}

// ── Runtime entry points reached from the trampolines ─────────────────────

/// **STEP 6 SEAM.** D4/D5: the `extern "C"` entry `deopt_uncommon_trampoline`
/// calls after sigreturn. Its full body (D5 M0–M8: materialize interpreter
/// frames from the resolved `DeoptState`, `Frame::verify` them, then nest an
/// interpreter run) is S13 step 6, in `runtime/deopt.rs`.
///
/// Step 5 goes exactly as far as the boundary allows and no further: it maps
/// `trap_pc → nmethod` ([`super::CodeTable::find_by_pc`]), resolves the
/// `DeoptState` for `pc - code_base` (proving the whole trap→state chain is
/// wired), then **stops** at the materialization handoff with a loud,
/// obvious "step 6" abort. Resolving here (rather than in step 6) keeps the
/// seam honest: the resolve must compile and be reachable now, so step 6
/// only has to fill in the `deoptimize_frame` call that consumes it.
///
/// # Safety
/// Only reached via `blr` from `deopt_uncommon_trampoline`; `vm` is `x28`
/// (`&mut VmState`), `trap_pc` the faulting pc, `fp` the trapped compiled
/// frame's FP. Never called directly from Rust.
pub unsafe extern "C" fn rt_uncommon_trap(
    vm: *mut crate::runtime::vm_state::VmState,
    trap_pc: u64,
    fp: u64,
) -> u64 {
    // SAFETY: contract above — `vm` is the `x28` &mut VmState the trampoline
    // forwarded from the compiled call.
    let vm = unsafe { &mut *vm };

    // trap_pc → owning nmethod. A miss is a VM bug (a deopt brk must live
    // inside a published nmethod), so this is a panic, not a graceful path.
    let nm_id = vm.code_table.find_by_pc(trap_pc).unwrap_or_else(|| {
        panic!(
            "rt_uncommon_trap: trap pc {trap_pc:#x} is not inside any published nmethod \
             (a deopt brk must belong to a compiled method)"
        )
    });
    let nm = vm
        .code_table
        .get(nm_id)
        .expect("rt_uncommon_trap: find_by_pc returned a live id");
    let code_off = (trap_pc - nm.code.base as u64) as u32;

    // Resolve the deopt state at this site (step 4's decode side). Panics on a
    // pc that was not recorded at compile time (P1) — every deopt-relevant pc
    // is, by construction.
    let deopt = crate::compiler::scopes::DeoptState::at(nm, code_off);

    // ─────────────────────────── STEP 6 SEAM ────────────────────────────
    // Everything above is step 5: trap → nmethod → resolved DeoptState. The
    // next line is where step 6 (`runtime::deopt::deoptimize_frame` + nested
    // `interpret_active`, D5 M0–M8) takes over, consuming `deopt`, `fp`, and
    // `nm_id`. Until then, stop loudly rather than fabricate a result.
    unimplemented!(
        "S13 step 6: materialize interpreter frames from this DeoptState and run them. \
         Resolved at nmethod {nm_id:?}, code_off {code_off:#x}, fp {fp:#x}, \
         site kind {:?}, reexecute {}, {} innermost-stack entries. \
         (Step 5 wires trap→state; step 6 fills in deoptimize_frame + interpret_active.)",
        deopt.site.kind,
        deopt.site.reexecute,
        deopt.site.stack.len(),
    );
}

/// D3 step 4: the off-signal landing for a `0xDE02` compiled-assertion trap —
/// a compiled-code "should not reach". This is always a VM bug, so it panics
/// with the trap pc. `-> !`: it never returns to the assert stub.
///
/// # Safety
/// Only reached via `blr` from `build_assert_stub`'s listing; `vm` is `x28`.
/// Never called directly from Rust.
pub unsafe extern "C" fn rt_compiled_assert_failed(
    _vm: *mut crate::runtime::vm_state::VmState,
    trap_pc: u64,
) -> ! {
    panic!(
        "compiled-code assertion (brk #0xDE02) at pc {trap_pc:#x} — a 'should not reach' \
         guard fired; this is a VM/compiler bug"
    );
}

// ── A checked native-stack slot read (layer-boundary helper) ──────────────

/// Read the oop at byte offset `off` from a compiled frame's FP. The single
/// door `runtime/deopt.rs` (step 6, a `#![deny(unsafe_code)]` module) uses to
/// touch a raw native-stack slot: the safety table pins that `runtime/deopt`
/// may reach native-stack pointers *only* through this re-exported helper.
/// Kept here (in `codecache`, the unsafe island) so the raw deref is licensed
/// where unsafe already lives.
///
/// # Safety
/// `fp` must be a live compiled frame's FP and `fp + off` a slot within that
/// frame (guaranteed by a `DeoptState`'s `FrameSlot` offsets, which the
/// compiler emitted for exactly this frame). The caller owns that invariant;
/// this only performs the aligned 8-byte load.
pub unsafe fn read_frame_slot(fp: usize, off: i32) -> Oop {
    // SAFETY: caller's contract — `fp + off` is an 8-byte-aligned live slot.
    let addr = (fp as isize + off as isize) as *const u64;
    Oop::from_raw(unsafe { core::ptr::read(addr) })
}

// ── install (D3): sigaction once at startup + trampoline generation ───────

/// Install the SIGTRAP handler and publish the deopt trampolines into `cache`.
/// Call once at startup, after the code cache exists but before running any
/// compiled code (the same window `stubs::install` runs in).
///
/// Order matters: the trampolines and their target statics MUST be published
/// and stored *before* `sigaction` arms the handler, so the very first trap
/// can never see a half-installed state (a `tramp == 0` handler falls through
/// to SIG_DFL, but publishing first removes even that transient).
///
/// Debugger caveat (D3, documented loudly): under lldb, Mach `EXC_BREAKPOINT`
/// is claimed *before* POSIX SIGTRAP conversion, so every deopt trap stops the
/// debugger. Mach exception ports are the v2 fix; v1 accepts the caveat
/// (README: `MACVM_JIT=off` for lldb sessions).
pub fn install(cache: &mut CodeCache) -> DeoptTrampolines {
    // 1. Cache the code-cache bounds for the handler's pc check.
    let (lo, hi) = cache.bounds();
    CODE_LO.store(lo, Ordering::Relaxed);
    CODE_HI.store(hi, Ordering::Relaxed);

    // 2. Generate + publish the trampolines.
    let uncommon_blob = build_uncommon_trampoline();
    let hu = cache
        .alloc(uncommon_blob.code.len())
        .expect("deopt_trap::install: code cache too small for uncommon trampoline");
    cache.publish(hu, &uncommon_blob);

    let assert_blob = build_assert_stub();
    let ha = cache
        .alloc(assert_blob.code.len())
        .expect("deopt_trap::install: code cache too small for assert stub");
    cache.publish(ha, &assert_blob);

    UNCOMMON_TRAMPOLINE.store(hu.base as u64, Ordering::Relaxed);
    ASSERT_STUB.store(ha.base as u64, Ordering::Relaxed);

    // 3. Arm the handler (last — see the doc above). SA_SIGINFO for the
    //    3-arg form; SA_ONSTACK so a trap on a nearly-exhausted stack still
    //    delivers on the alt stack if one is set up (harmless if not).
    // SAFETY: `sigtrap_handler` matches the SA_SIGINFO 3-arg ABI; we install
    // it exactly once (guarded by INSTALLED) with a zeroed, fully-initialized
    // `sigaction`.
    unsafe {
        let mut sa: libc::sigaction = core::mem::zeroed();
        sa.sa_sigaction = sigtrap_handler as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
        libc::sigemptyset(&mut sa.sa_mask);
        let rc = libc::sigaction(libc::SIGTRAP, &sa, core::ptr::null_mut());
        assert_eq!(rc, 0, "deopt_trap::install: sigaction(SIGTRAP) failed");
    }
    INSTALLED.store(true, Ordering::Relaxed);

    DeoptTrampolines {
        uncommon: hu,
        assert: ha,
    }
}

// ── Test-only hooks: point the handler's redirect at a capture stub ───────
//
// `handler_redirect_smoke` (tests_s13.md) needs to exercise the
// signal→ucontext-rewrite→trampoline path in ISOLATION, WITHOUT reaching the
// step-6-less `rt_uncommon_trap` (which `unimplemented!()`s). These hooks let
// a test arm the handler + point `UNCOMMON_TRAMPOLINE` at its OWN benign
// capture stub that records (pc, x16) and returns, then restore state.

/// # Safety
/// Test-only. Overrides the handler's redirect target and code-cache bounds
/// so a test can drive the whole signal path against a hand-built stub.
/// Serialize test use (`#[serial]`-style single-threaded runner) — these are
/// process-global statics shared with the real handler.
#[cfg(test)]
unsafe fn test_arm_handler(lo: u64, hi: u64, uncommon_tramp: u64) {
    CODE_LO.store(lo, Ordering::Relaxed);
    CODE_HI.store(hi, Ordering::Relaxed);
    UNCOMMON_TRAMPOLINE.store(uncommon_tramp, Ordering::Relaxed);
    INSTALLED.store(true, Ordering::Relaxed);
    // SAFETY: same contract as `install`'s sigaction arm.
    unsafe {
        let mut sa: libc::sigaction = core::mem::zeroed();
        sa.sa_sigaction = sigtrap_handler as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigemptyset(&mut sa.sa_mask);
        let rc = libc::sigaction(libc::SIGTRAP, &sa, core::ptr::null_mut());
        assert_eq!(rc, 0, "test_arm_handler: sigaction failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `brk_imm_decode` (tests_s13.md): the encoder for `brk #0xDE00..02`
    /// round-trips through the handler's decode mask, and foreign brks —
    /// notably Rust's `abort()` (`brk #1`) — are rejected (so SIG_DFL keeps
    /// them fatal).
    #[test]
    fn brk_imm_decode() {
        for imm in [TRAP_UNCOMMON, TRAP_STRESS, TRAP_ASSERT] {
            let word = brk_word(imm);
            // Encoding matches the ISA form `0xD420_0000 | imm16 << 5`.
            assert_eq!(
                word,
                0xD420_0000 | ((imm as u32) << 5),
                "brk #{imm:#x} encoding"
            );
            assert_eq!(decode_deopt_brk(word), Some(imm), "our imm must decode");
        }
        // Rust `abort()` lowers to `brk #1`; must be rejected as foreign.
        assert_eq!(
            decode_deopt_brk(brk_word(1)),
            None,
            "brk #1 (Rust abort) is foreign"
        );
        assert_eq!(decode_deopt_brk(brk_word(0)), None, "brk #0 is foreign");
        // A brk imm adjacent to our range but outside it is foreign.
        assert_eq!(
            decode_deopt_brk(brk_word(0xDDFF)),
            None,
            "0xDDFF just below range"
        );
        assert_eq!(
            decode_deopt_brk(brk_word(0xDE03)),
            None,
            "0xDE03 just above range"
        );
        // A non-brk instruction (a nop) is not a brk at all.
        assert_eq!(decode_deopt_brk(0xD503_201F), None, "nop is not a brk");
        // A `bl` with bits that happen to fall in the imm slot is still not a
        // brk (opcode mask rejects it).
        assert_eq!(decode_deopt_brk(0x9400_0000), None, "bl is not a brk");
    }

    /// The emit helper lays down exactly the raw `brk` word.
    #[test]
    fn emit_brk_lays_raw_word() {
        let mut a = JasmAssembler::new();
        emit_brk(&mut a, TRAP_UNCOMMON);
        let blob = a.finish();
        assert_eq!(&blob.code[..4], &brk_word(TRAP_UNCOMMON).to_le_bytes());
    }

    /// `handler_redirect_smoke` (tests_s13.md): publish a stub that
    /// `brk #0xDE01`s; a test-mode trampoline records (pc, x16) and returns;
    /// assert the trap pc was stashed in x16 and the handler redirected into
    /// the trampoline — i.e. the full signal → ucontext-rewrite → trampoline
    /// path fires in isolation, without reaching the step-6 seam.
    ///
    /// Runs on the CURRENT thread and installs a process-global SIGTRAP
    /// handler for the duration; `#[ignore]` by default so the ordinary
    /// `cargo test` run (which may execute tests in parallel and must not have
    /// a live deopt handler swallowing unrelated traps) stays stable. Run
    /// explicitly with `cargo test -- --ignored handler_redirect_smoke`.
    #[test]
    #[ignore = "installs a process-global SIGTRAP handler; run single-threaded with --ignored"]
    fn handler_redirect_smoke() {
        use std::sync::atomic::AtomicU64;

        // A static the capture trampoline writes the observed x16 (trap pc)
        // into, and a flag proving it ran.
        static CAPTURED_X16: AtomicU64 = AtomicU64::new(0);
        static RAN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

        // The capture trampoline the handler will redirect to. It receives the
        // trap pc in x16 (the handler's stash) and, per the smoke test's
        // contract, "records (pc, x16) and returns". It records x16, sets a
        // flag, then unwinds cleanly back to the caller of the brk-stub.
        //
        // The brk-stub below is called as `extern "C" fn() -> ()` and its brk
        // is its very first instruction. When the handler redirects here, the
        // machine state is: lr = the return address into `run_stub`'s caller
        // frame, fp = that caller's fp, x16 = trap pc. So a plain `ret` (after
        // recording x16) returns straight out of the stub as if it had done
        // nothing — no frame was ever built by the brk-stub.
        extern "C" fn capture_and_return() {
            // These statics are plain atomics — the only "work" done, and safe
            // to touch here because control has already left signal context
            // (we are running normal code the handler resumed into).
            // We read x16 via inline read of the stashed value. Since we can't
            // name x16 from Rust portably, the stub was entered with x16 live;
            // we recover it below through a tiny asm shim instead.
            unsafe {
                let x16: u64;
                core::arch::asm!("mov {}, x16", out(reg) x16, options(nomem, nostack, preserves_flags));
                CAPTURED_X16.store(x16, Ordering::SeqCst);
            }
            RAN.store(true, Ordering::SeqCst);
        }

        // Build a stub whose first instruction is `brk #0xDE01`, followed by a
        // `ret` (only reached if the handler somehow returns without
        // redirecting — then the test's asserts fail cleanly rather than
        // executing garbage).
        let mut cache = CodeCache::new(1 << 16).unwrap();

        // Publish the brk-stub.
        let mut sa = JasmAssembler::new();
        emit_brk(&mut sa, TRAP_STRESS); // brk #0xDE01
        sa.emit("ret", &[]);
        let stub_blob = sa.finish();
        let hs = cache.alloc(stub_blob.code.len()).unwrap();
        cache.publish(hs, &stub_blob);

        // The capture trampoline: `mov x_scratch, x16` is done inside
        // `capture_and_return` itself; here we just need a tiny native shim
        // that calls it and then returns. But simplest: point the handler
        // directly at a stub that `b`s into `capture_and_return`. We build a
        // stub: `ldr x17,<addr>; br x17`. It preserves x16 (the trap pc) into
        // `capture_and_return`, which reads it.
        let mut ta = JasmAssembler::new();
        let lit = ta.literal_u64(
            capture_and_return as *const () as u64,
            Some(RelocKind::RuntimeAddr),
        );
        ta.ldr_literal(xr(17), lit);
        ta.emit("br", &[x(17)]);
        let tramp_blob = ta.finish();
        let ht = cache.alloc(tramp_blob.code.len()).unwrap();
        cache.publish(ht, &tramp_blob);

        // Arm the handler pointing at OUR capture trampoline, with THIS
        // cache's bounds.
        let (lo, hi) = cache.bounds();
        // SAFETY: test-only global override; this test process is
        // single-threaded for the duration of the trap.
        unsafe { test_arm_handler(lo, hi, ht.base as u64) };

        // Execute the brk-stub. Control: brk #0xDE01 -> SIGTRAP -> handler
        // stashes pc in x16, sets __pc = capture trampoline -> br into
        // capture_and_return -> records x16, returns -> ret unwinds out of the
        // stub call. Result: we return here normally.
        let stub: extern "C" fn() = unsafe { core::mem::transmute(hs.base) };
        stub();

        assert!(
            RAN.load(Ordering::SeqCst),
            "capture trampoline must have run"
        );
        let captured = CAPTURED_X16.load(Ordering::SeqCst);
        assert_eq!(
            captured, hs.base as u64,
            "x16 must hold the trap pc = the brk-stub's first (and only trapping) instruction"
        );

        // Restore SIG_DFL so no later test sees our handler.
        unsafe {
            let mut sa: libc::sigaction = core::mem::zeroed();
            sa.sa_sigaction = libc::SIG_DFL;
            libc::sigaction(libc::SIGTRAP, &sa, core::ptr::null_mut());
        }
        INSTALLED.store(false, Ordering::Relaxed);
    }

    /// `read_frame_slot` reads the 8-byte oop at `fp + off` (the one licensed
    /// native-stack door for step 6). Exercised against a stack-local array so
    /// the offsets are real.
    #[test]
    fn read_frame_slot_reads_offset() {
        // Low 2 bits = 0b00 (smi tag) so `Oop::from_raw`'s debug tag check is
        // satisfied — real spill slots hold real oops, never raw junk words.
        let slots: [u64; 4] = [0x1110, 0x2220, 0x3330, 0x4440];
        let fp = slots.as_ptr() as usize;
        // off 0, +8, +16 in bytes.
        assert_eq!(unsafe { read_frame_slot(fp, 0) }.raw(), 0x1110);
        assert_eq!(unsafe { read_frame_slot(fp, 8) }.raw(), 0x2220);
        assert_eq!(unsafe { read_frame_slot(fp, 16) }.raw(), 0x3330);
        // Negative offsets (below-FP spill slots, the S12 convention) work too
        // when fp points into the middle of the array.
        let mid = (&slots[2] as *const u64) as usize;
        assert_eq!(unsafe { read_frame_slot(mid, -8) }.raw(), 0x2220);
        assert_eq!(unsafe { read_frame_slot(mid, -16) }.raw(), 0x1110);
    }
}
