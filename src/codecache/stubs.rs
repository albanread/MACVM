//! `call_stub`/`stub_poll` (S10 D5.6) and, from S11, the runtime-stub
//! table's growing collection of anchor-and-call-into-Rust trampolines
//! (D5) — hand-assembled once at startup, not derived from any `Ir`. All
//! published into the same [`CodeCache`] real compiled methods live in.

use crate::compiler::assembler::{
    imm, mem, mem_post, mem_pre, sp, x, xr, Assembler, CodeBlob, Cond, RelocKind,
};
use crate::compiler::jasm_assembler::JasmAssembler;
use crate::oops::layout::{
    ROOTSPILL_BYTES, VMREG_LAST_COMPILED_FP_OFFSET, VMREG_LAST_COMPILED_KIND_OFFSET,
    VMREG_LAST_COMPILED_PC_OFFSET,
};
use crate::oops::wrappers::{KlassOop, MethodOop, SymbolOop};
use crate::oops::Oop;
use crate::runtime::lookup::{klass_of, lookup};
use crate::runtime::vm_state::{TierLink, VmState};

use super::nmethod::{IcState, NmethodId};
use super::pics::PIC_MAX_ENTRIES;
use super::{CodeCache, CodeHandle};

/// S12 D3: `runtime::frames::AdapterKind`'s raw wire values, written by
/// [`emit_stub_kind_tag`] and read back by the frame walker via
/// `VmRegBlock::last_compiled_kind`. Owned HERE (not in `runtime::frames`)
/// because this is the module that actually emits the `movz`/`str` writing
/// them — `frames.rs`'s `AdapterKind::from_raw`/`as_raw` just mirror these
/// same six numbers. `Poll` has no entry: `stub_poll` never calls
/// `emit_stub_prologue` at all (S10 D5.6: it never allocates/GCs, so it
/// never needs the anchor), so this kind is never actually written --
/// `frames.rs` keeps a `Poll` variant only for `AdapterKind`'s own
/// completeness, provably dead on this specific path.
pub const KIND_RESOLVE: u64 = 0;
pub const KIND_C2I: u64 = 1;
pub const KIND_MEGA: u64 = 2;
pub const KIND_DNU: u64 = 3;
pub const KIND_MUST_BE_BOOLEAN: u64 = 4;
pub const KIND_ALLOC_SLOW: u64 = 5;

/// D5's shared stub skeleton, part 1: anchor + AAPCS frame + RootSpill
/// (x0..x5). Every stub that calls into Rust starts with this; follow with
/// the stub's own `call_far`, then [`emit_stub_epilogue`]. `x28` must
/// already be `&VmState` (established once by `call_stub`, D5.1) — every
/// site this is used from is reached with that invariant already holding.
/// A free function taking `&mut dyn Assembler`, not a method, so it stays
/// reusable across every stub builder in this module without any of them
/// needing to share a common base type beyond the trait itself.
///
/// Deliberately does NOT also write `last_compiled_kind` (S12): an earlier
/// draft tried folding that in here too, using x16/x17 as scratch for the
/// immediate — but `build_c2i_shared`/`build_mega_shared` read x17/x16
/// (the method/selector oop carried through from the per-instance
/// adapter/mega trampoline's own tail-jump) as their OWN first instruction
/// after this prologue returns, so writing through either register here
/// would clobber it on every c2i/mega call in the system. Each of the 6
/// callers below calls [`emit_stub_kind_tag`] itself, immediately after
/// this, using a register IT has independently confirmed is free (x9 —
/// none of the 6 touch it this early; matches `build_call_stub`'s own
/// "caller-saved, x9-x11" scratch convention).
pub fn emit_stub_prologue(a: &mut dyn Assembler) {
    a.emit("stp", &[x(29), x(30), mem_pre(31, -16)]);
    a.emit("mov", &[x(29), sp()]);
    a.emit("sub", &[sp(), sp(), imm(ROOTSPILL_BYTES as i64)]);
    a.emit("stp", &[x(0), x(1), mem(31, 0)]);
    a.emit("stp", &[x(2), x(3), mem(31, 16)]);
    a.emit("stp", &[x(4), x(5), mem(31, 32)]);
    // Anchor (D4.1): last_compiled_fp/pc so a stack walker (S12) can find
    // this stub's own frame, and everything above it, while control is in
    // Rust.
    a.emit(
        "str",
        &[x(29), mem(28, VMREG_LAST_COMPILED_FP_OFFSET as i64)],
    );
    a.emit(
        "str",
        &[x(30), mem(28, VMREG_LAST_COMPILED_PC_OFFSET as i64)],
    );
}

/// S12 D3: writes `last_compiled_kind` — called by each of the 6
/// anchor-setting stubs' own preamble, immediately after
/// `emit_stub_prologue` returns, using x9 (`build_call_stub`'s own
/// "caller-saved, x9-x11" scratch precedent; independently confirmed free
/// at this exact point in all 6 callers — none of them reads x9 before
/// their own first `mov`/`ldr` off x0-x5/x16/x17/x28/x30). `kind` is one of
/// this module's own `KIND_*` constants, never `AdapterKind::Poll`'s
/// value (unwritten by construction — see `emit_stub_prologue`'s own doc).
fn emit_stub_kind_tag(a: &mut dyn Assembler, kind: u64) {
    a.emit("movz", &[x(9), imm(kind as i64)]);
    a.emit(
        "str",
        &[x(9), mem(28, VMREG_LAST_COMPILED_KIND_OFFSET as i64)],
    );
}

/// Part 2: clears the anchor (P9 — a stale anchor makes S12's walker walk
/// freed frames), reloads x0..x5 from RootSpill, tears down the frame.
/// Does NOT emit the final `ret`/`br`/`b` — callers differ on that (plain
/// `ret` for a callee-shaped stub like `dnu`, `br x16` tail-jump for
/// `resolve`/`mega`, P4) — and does not touch whatever scratch register
/// the caller is about to jump through (typically x16), matching the same
/// per-caller variation.
pub fn emit_stub_epilogue(a: &mut dyn Assembler) {
    // x(31) here means xzr, not sp: register 31 in a store's *data* (Rt)
    // position is unconditionally the zero register on this ISA — `is_sp`
    // only disambiguates ALU/move *operand* positions (`assembler.rs`'s
    // own `sp()` doc comment), which this is not.
    a.emit(
        "str",
        &[x(31), mem(28, VMREG_LAST_COMPILED_FP_OFFSET as i64)],
    );
    // S12: clear the kind tag alongside the fp (defense in depth, not load-
    // bearing — nothing ever reads it while `last_compiled_fp == 0` — but a
    // cleared-and-therefore-obviously-wrong value is strictly better than a
    // stale-but-plausible one if some future bug ever left `last_compiled_fp`
    // stale too).
    a.emit(
        "str",
        &[x(31), mem(28, VMREG_LAST_COMPILED_KIND_OFFSET as i64)],
    );
    a.emit("ldp", &[x(0), x(1), mem(31, 0)]);
    a.emit("ldp", &[x(2), x(3), mem(31, 16)]);
    a.emit("ldp", &[x(4), x(5), mem(31, 32)]);
    a.emit("add", &[sp(), sp(), imm(ROOTSPILL_BYTES as i64)]);
    a.emit("ldp", &[x(29), x(30), mem_post(31, 16)]);
}

/// `extern "C" fn(entry, vm, argv, argc) -> u64` (D4) — the shape every
/// call through `call_stub` must be transmuted to.
pub type CallStubFn =
    unsafe extern "C" fn(entry: u64, vm: *mut VmState, argv: *const u64, argc: u64) -> u64;

/// `Copy`: both fields are just a pointer+len (`CodeHandle` itself is
/// `Copy`) — copying a `Stubs` duplicates no real resource, and lets
/// `enter_compiled` read it out of `vm.stubs` before also needing `&mut
/// VmState` for the call itself (`vm.stubs.invoke(entry, vm, ...)` would
/// otherwise borrow `vm.stubs` and all of `vm` at once).
#[derive(Clone, Copy)]
pub struct Stubs {
    pub call_stub: CodeHandle,
    pub stub_poll: CodeHandle,
    /// D4.1: the shared `stub_resolve`/`stub_ic_miss` door — `emit.rs`
    /// embeds its address as every `IcSite`'s initial `bl` target (S11
    /// step 2).
    pub resolve: CodeHandle,
    /// D6.1: the shared c2i adapter tail — every per-method `c2i_<method>`
    /// trampoline (`codecache::adapters::build_c2i_adapter`) `br`s here
    /// with the target method's own oop already in x17 (S11 step 4).
    pub c2i_shared: CodeHandle,
    /// D4.4: the shared megamorphic-lookup tail — every per-selector
    /// `mega_<sel>` trampoline (`codecache::mega::build_mega_trampoline`)
    /// `br`s here with the selector's own oop already in x16 (S11 step 5).
    pub mega_shared: CodeHandle,
    /// D5.4: `rt_resolve_send`/`rt_mega_lookup`'s own lookup-failure
    /// return value — never patched into a site, just tail-jumped to for
    /// THIS one dispatch (S11 step 6).
    pub dnu: CodeHandle,
    /// D1/D5: the `BoolBr.not_bool` runtime helper — built and callable
    /// now (S11 step 6), but not yet reachable from any REAL compiled
    /// method: emitting a call to it from a `not_bool` block is
    /// `Ir::CallRuntime`'s own job, S11 step 7's eligibility relaxation.
    pub must_be_boolean: CodeHandle,
    /// D7: the inline-allocation slow-path helper — an `Ir::Alloc` fast
    /// path whose eden bump overflows `bl`s here with the class oop in x0
    /// and the size in bytes in x1 (S11 step 8). Callee-shaped (plain `ret`,
    /// result oop in x0), same contract as `must_be_boolean`.
    pub alloc_slow: CodeHandle,
    /// S13 D1 §2b: the shared `not_entrant_stub` — a now-`NotEntrant`
    /// nmethod's `entry`/`verified_entry` are patched (by
    /// `nmethod::make_not_entrant`) to `b not_entrant_stub`, so a future call
    /// through a compiled caller's `bl` (which still targets the old entry)
    /// lands here with receiver+args untouched in x0..x5 and x30 = the
    /// caller's own send-site return address. It re-dispatches EXACTLY like an
    /// IC miss (`stub_resolve`/[`rt_resolve_send`]): re-resolve
    /// (receiver klass, selector) for that site, patch the site's `bl` to the
    /// CURRENT method's target, and tail-jump there. See
    /// [`build_not_entrant_stub`] for why this shares the resolve tail
    /// verbatim rather than being its own runtime function.
    pub not_entrant: CodeHandle,
    /// S13 D6: the `deopt_return_trampoline` — the address §2c
    /// ([`crate::runtime::frames::redirect_returns_into_nm`]) writes into an
    /// in-flight `NotEntrant` activation's callee saved-LR slots, so a callee's
    /// `ret` deopts the caller lazily instead of resuming old compiled code.
    /// See [`build_deopt_return_trampoline`].
    pub deopt_return: CodeHandle,
}

impl Stubs {
    pub fn call_stub_entry(&self) -> *const u8 {
        self.call_stub.base
    }
    pub fn stub_poll_addr(&self) -> u64 {
        self.stub_poll.base as u64
    }
    pub fn resolve_addr(&self) -> u64 {
        self.resolve.base as u64
    }
    pub fn c2i_shared_addr(&self) -> u64 {
        self.c2i_shared.base as u64
    }
    pub fn mega_shared_addr(&self) -> u64 {
        self.mega_shared.base as u64
    }
    pub fn dnu_addr(&self) -> u64 {
        self.dnu.base as u64
    }
    pub fn must_be_boolean_addr(&self) -> u64 {
        self.must_be_boolean.base as u64
    }
    pub fn alloc_slow_addr(&self) -> u64 {
        self.alloc_slow.base as u64
    }
    /// S13 §2b: the address `make_not_entrant` patches an invalidated
    /// nmethod's `entry`/`verified_entry` to branch to.
    pub fn not_entrant_addr(&self) -> u64 {
        self.not_entrant.base as u64
    }
    /// S13 D6: the address §2c redirects an in-flight `NotEntrant` activation's
    /// callee saved-LR slots to (see [`build_deopt_return_trampoline`]).
    pub fn deopt_return_addr(&self) -> u64 {
        self.deopt_return.base as u64
    }

    /// Invokes `entry` (a compiled method's own entry point — an `Nmethod`'s
    /// `code.base + entry_off`) through the real, published `call_stub`,
    /// exactly as if it were an ordinary `extern "C"` function taking
    /// `(vm, argv, argc)`. `argv[0]` is the receiver, `argv[1..]` the real
    /// Smalltalk arguments (`ir::convert`'s own `Param{index}` convention).
    ///
    /// The one place `interpreter::compiled_call::enter_compiled` needs
    /// unsafe FFI, tucked in here instead of allowed at that call site —
    /// this module (`codecache`) is `crate`'s one designated owner of every
    /// raw pointer call into `MAP_JIT` memory (this file's own module doc),
    /// so `interpreter` stays covered by the crate-root `#![deny(unsafe_code)]`.
    /// Takes `&mut VmState`, not a raw pointer, so the public signature
    /// itself stays safe (clippy's `not_unsafe_ptr_arg_deref`) — the
    /// reference-to-pointer conversion `call_stub`'s own FFI shape needs is
    /// done internally instead.
    pub fn invoke(&self, entry: u64, vm: &mut VmState, argv: &[u64]) -> u64 {
        let call: CallStubFn = unsafe { std::mem::transmute(self.call_stub_entry()) };
        let vm_ptr: *mut VmState = vm;
        unsafe { call(entry, vm_ptr, argv.as_ptr(), argv.len() as u64) }
    }
}

/// Build and publish both stubs into `cache`. Call once, before compiling
/// or running any real method — `emit.rs` needs `stub_poll`'s address
/// (`Stubs::stub_poll_addr`) to embed as a pool constant in any method
/// that emits `Ir::Poll`.
pub fn install(cache: &mut CodeCache) -> Stubs {
    let call_stub_blob = build_call_stub();
    let h1 = cache
        .alloc(call_stub_blob.code.len())
        .expect("stubs::install: code cache too small for call_stub");
    cache.publish(h1, &call_stub_blob);

    let stub_poll_blob = build_stub_poll();
    let h2 = cache
        .alloc(stub_poll_blob.code.len())
        .expect("stubs::install: code cache too small for stub_poll");
    cache.publish(h2, &stub_poll_blob);

    let stub_resolve_blob = build_stub_resolve();
    let h3 = cache
        .alloc(stub_resolve_blob.code.len())
        .expect("stubs::install: code cache too small for stub_resolve");
    cache.publish(h3, &stub_resolve_blob);

    let c2i_shared_blob = build_c2i_shared();
    let h4 = cache
        .alloc(c2i_shared_blob.code.len())
        .expect("stubs::install: code cache too small for c2i_shared");
    cache.publish(h4, &c2i_shared_blob);

    let mega_shared_blob = build_mega_shared();
    let h5 = cache
        .alloc(mega_shared_blob.code.len())
        .expect("stubs::install: code cache too small for mega_shared");
    cache.publish(h5, &mega_shared_blob);

    let dnu_blob = build_stub_dnu();
    let h6 = cache
        .alloc(dnu_blob.code.len())
        .expect("stubs::install: code cache too small for dnu");
    cache.publish(h6, &dnu_blob);

    let must_be_boolean_blob = build_stub_must_be_boolean();
    let h7 = cache
        .alloc(must_be_boolean_blob.code.len())
        .expect("stubs::install: code cache too small for must_be_boolean");
    cache.publish(h7, &must_be_boolean_blob);

    let alloc_slow_blob = build_stub_alloc_slow();
    let h8 = cache
        .alloc(alloc_slow_blob.code.len())
        .expect("stubs::install: code cache too small for alloc_slow");
    cache.publish(h8, &alloc_slow_blob);

    let not_entrant_blob = build_not_entrant_stub();
    let h9 = cache
        .alloc(not_entrant_blob.code.len())
        .expect("stubs::install: code cache too small for not_entrant");
    cache.publish(h9, &not_entrant_blob);

    let deopt_return_blob = build_deopt_return_trampoline();
    let h10 = cache
        .alloc(deopt_return_blob.code.len())
        .expect("stubs::install: code cache too small for deopt_return");
    cache.publish(h10, &deopt_return_blob);

    Stubs {
        call_stub: h1,
        stub_poll: h2,
        resolve: h3,
        c2i_shared: h4,
        mega_shared: h5,
        dnu: h6,
        must_be_boolean: h7,
        alloc_slow: h8,
        not_entrant: h9,
        deopt_return: h10,
    }
}

/// D4: saves x19..x28 + fp/lr (callee-saved, AAPCS64), moves the incoming
/// `entry`/`vm`/`argv`/`argc` (x0..x3) out of the way of the compiled
/// call's own x0..x5 argument registers *before* touching any of them
/// (x0..x3 alias two of the very registers this stub is about to
/// populate — `argv`/`argc` specifically would clobber themselves
/// mid-copy otherwise), then conditionally loads up to 6 words from
/// `argv` into x0..x5 depending on the runtime `argc` value, `blr`s into
/// the compiled entry, and restores. Its own `x29` is the anchor a stack
/// walker stops at (D4's own words) — a completely ordinary frame-pointer
/// chain, nothing extra required for that beyond the standard prologue.
fn build_call_stub() -> CodeBlob {
    let mut a = JasmAssembler::new();

    a.emit("stp", &[x(29), x(30), mem_pre(31, -96)]);
    a.emit("stp", &[x(19), x(20), mem(31, 16)]);
    a.emit("stp", &[x(21), x(22), mem(31, 32)]);
    a.emit("stp", &[x(23), x(24), mem(31, 48)]);
    a.emit("stp", &[x(25), x(26), mem(31, 64)]);
    a.emit("stp", &[x(27), x(28), mem(31, 80)]);
    a.emit("mov", &[x(29), sp()]);

    // Move incoming params to scratch (caller-saved, x9-x11) registers
    // before any of x0..x5 get overwritten below.
    a.emit("mov", &[x(9), x(0)]); // entry
    a.emit("mov", &[x(28), x(1)]); // vm -> x28 (D5.1's own convention)
    a.emit("mov", &[x(10), x(2)]); // argv
    a.emit("mov", &[x(11), x(3)]); // argc

    let args_done = a.new_label();
    for (i, off) in [(1, 0), (2, 8), (3, 16), (4, 24), (5, 32), (6, 40)] {
        a.emit("cmp", &[x(11), imm(i)]);
        a.b_cond(Cond::Lt, args_done);
        a.emit("ldr", &[x((i - 1) as u8), mem(10, off)]);
    }
    a.bind(args_done);

    a.emit("blr", &[x(9)]);

    a.emit("ldp", &[x(27), x(28), mem(31, 80)]);
    a.emit("ldp", &[x(25), x(26), mem(31, 64)]);
    a.emit("ldp", &[x(23), x(24), mem(31, 48)]);
    a.emit("ldp", &[x(21), x(22), mem(31, 32)]);
    a.emit("ldp", &[x(19), x(20), mem(31, 16)]);
    a.emit("ldp", &[x(29), x(30), mem_post(31, 96)]);
    a.emit("ret", &[]);

    a.finish()
}

/// D5.6: called via `bl` from compiled code with x28 already `&VmState`
/// (established once by `call_stub`, still live at any `Poll` site) —
/// saves all of x0..x15 (compiled code's own live values, which the
/// `Poll` site's caller expects untouched, S10 P6), calls
/// [`rt_poll`], restores, returns. x16/x17/flags are NOT saved: they're
/// documented assembler/veneer scratch (D3.5), never expected to survive
/// an Ir op boundary in the first place, so a Poll site never relies on
/// them either.
fn build_stub_poll() -> CodeBlob {
    let mut a = JasmAssembler::new();

    // Frame: [x29] = saved caller x29 = LOOP's fp, [x29+8] = saved x30 = the
    // return address INSIDE the loop right after `bl stub_poll` (== the poll's
    // ret_pc). `rt_poll` is a C call and preserves x29, so x29 still names THIS
    // frame after it returns — we reload loop_fp/ret_pc from [x29] not registers.
    a.emit("stp", &[x(29), x(30), mem_pre(31, -16)]);
    a.emit("mov", &[x(29), sp()]);
    a.emit("sub", &[sp(), sp(), imm(128)]);
    a.emit("stp", &[x(0), x(1), mem(31, 0)]);
    a.emit("stp", &[x(2), x(3), mem(31, 16)]);
    a.emit("stp", &[x(4), x(5), mem(31, 32)]);
    a.emit("stp", &[x(6), x(7), mem(31, 48)]);
    a.emit("stp", &[x(8), x(9), mem(31, 64)]);
    a.emit("stp", &[x(10), x(11), mem(31, 80)]);
    a.emit("stp", &[x(12), x(13), mem(31, 96)]);
    a.emit("stp", &[x(14), x(15), mem(31, 112)]);

    // rt_poll(vm, loop_fp, ret_pc) -> PollOutcome{result:x0, deopted:x1}.
    a.emit("mov", &[x(0), x(28)]); // x0 = &mut VmState
    a.emit("ldr", &[x(1), mem(29, 0)]); // x1 = loop_fp = saved x29
    a.emit("ldr", &[x(2), mem(29, 8)]); // x2 = ret_pc  = saved x30
    let rt_poll_lit = a.literal_u64(rt_poll as *const () as u64, Some(RelocKind::RuntimeAddr));
    a.call_far(rt_poll_lit);

    // deopted == 0 -> normal loop resume (restore x0-x15, ret to the loop).
    // deopted != 0 -> deopt teardown: x0 already holds the deoptee's result;
    // discard BOTH stub_poll's frame AND the loop frame and `ret` the result to
    // the loop's own native caller (the loop activation is gone after a deopt).
    let cont = a.new_label();
    a.cbz(xr(1), cont);

    // --- deopt teardown (mirrors deopt_trap::build_uncommon_trampoline) ---
    // x0 (result) is NOT restored — it must survive to the loop's caller.
    a.emit("ldr", &[x(2), mem(29, 0)]); // x2 = loop_fp (= saved x29)
    a.emit("mov", &[sp(), x(2)]); // sp := loop_fp: drop stub_poll frame + loop locals
    a.emit("ldp", &[x(29), x(30), mem_post(31, 16)]); // pop loop frame's own {caller_fp, caller_lr}
    a.emit("ret", &[]); // return result (x0) to the loop's caller

    // --- continue: restore the loop's live regs and resume it ---
    a.bind(cont);
    a.emit("ldp", &[x(0), x(1), mem(31, 0)]);
    a.emit("ldp", &[x(2), x(3), mem(31, 16)]);
    a.emit("ldp", &[x(4), x(5), mem(31, 32)]);
    a.emit("ldp", &[x(6), x(7), mem(31, 48)]);
    a.emit("ldp", &[x(8), x(9), mem(31, 64)]);
    a.emit("ldp", &[x(10), x(11), mem(31, 80)]);
    a.emit("ldp", &[x(12), x(13), mem(31, 96)]);
    a.emit("ldp", &[x(14), x(15), mem(31, 112)]);
    a.emit("add", &[sp(), sp(), imm(128)]);
    a.emit("ldp", &[x(29), x(30), mem_post(31, 16)]);
    a.emit("ret", &[]);

    a.finish()
}

/// D4.1: `stub_resolve`/`stub_ic_miss` — one shared stub, two doors, same
/// tail (S11 step 2 wires the `bl`-initial door as every fresh `IcSite`'s
/// target; the guard-miss `b` door lands S11 step 5 alongside PICs). x30 on
/// entry is the ORIGINAL send site's own return address either way (a `bl`
/// naturally sets it; a guard's `b` doesn't touch it) — captured into x1
/// (`rt_resolve_send`'s `ret_addr` argument) before `call_far`'s own `blr`
/// would otherwise clobber it.
fn build_stub_resolve() -> CodeBlob {
    let mut a = JasmAssembler::new();

    emit_stub_prologue(&mut a);
    emit_stub_kind_tag(&mut a, KIND_RESOLVE);
    a.emit("mov", &[x(0), x(28)]); // vm
    a.emit("mov", &[x(1), x(30)]); // ret_addr (captured before call_far clobbers x30)
    a.emit("sub", &[x(2), x(29), imm(ROOTSPILL_BYTES as i64)]); // argv = &RootSpill
    let rt_resolve_lit = a.literal_u64(
        rt_resolve_send as *const () as u64,
        Some(RelocKind::RuntimeAddr),
    );
    a.call_far(rt_resolve_lit);
    a.emit("mov", &[x(16), x(0)]); // result -> x16 (P4: survives the epilogue's own x0 reload)
    emit_stub_epilogue(&mut a);
    a.emit("br", &[x(16)]);

    a.finish()
}

/// D4.1/D4.3/D4.4's shared target-resolution step, used by `rt_resolve_send`
/// (both the mono path and every Pic/Mega transition), `rt_mega_lookup`,
/// and — `pub(crate)` specifically for this — `compiler::driver::
/// compile_method`'s own compile-time resolution of a `send_super` site
/// (D4.6): the same "real nmethod or c2i adapter" choice applies whether
/// the resolving call happens at runtime or at compile time.
///
/// `use_verified` picks between a real nmethod's `entry` and its
/// `verified_entry`: a MONO site's `bl` is a static patch that could later
/// see a DIFFERENT receiver klass through the SAME site, so its target
/// must re-verify (`entry`, `use_verified=false`) — but a PIC's own guard
/// chain, or `rt_mega_lookup`'s own fresh `klass_of`+`lookup`, has ALREADY
/// verified this exact (klass, method) pair by the time it's about to
/// jump, so re-checking would be pure redundant overhead (`verified_entry`,
/// `use_verified=true`, D4.3's own "the pair's klass was just compared").
/// A c2i adapter has no guard of its own either way (D6.1), so the
/// distinction is moot there.
pub(crate) fn resolve_target_entry(
    vm: &mut VmState,
    k: KlassOop,
    selector: SymbolOop,
    method: MethodOop,
    use_verified: bool,
) -> u64 {
    match vm.code_table.lookup(k, selector) {
        Some(target_id) => {
            let target_nm = vm
                .code_table
                .get(target_id)
                .expect("code_table.lookup just returned this id");
            let off = if use_verified {
                target_nm.verified_entry_off
            } else {
                target_nm.entry_off
            };
            target_nm.code.base as u64 + off as u64
        }
        // D6.1: no compiled nmethod yet -- a c2i adapter, callable exactly
        // like a real nmethod's own `entry` (`bl`, x0=result on return),
        // stands in until (if ever) `method` gets compiled for real.
        None => {
            let c2i_shared_addr = vm.stubs.c2i_shared_addr();
            vm.adapters
                .get_or_make(&mut vm.code_cache, c2i_shared_addr, method)
        }
    }
}

/// S14 step 5: `resolve_target_entry` for a SUPER-send site. A super site
/// links `verified_entry` directly (skipping the entry guard) and its
/// receivers are SUBCLASS instances — any klass below the holder — so it must
/// never enter an nmethod whose code was customized to its key klass
/// (`self_devirt`: guard-free self-send inlines baked against exactly that
/// klass). Such a target dispatches through the method's c2i adapter instead:
/// the interpreter re-dispatches its self-sends against the receiver's REAL
/// klass. Non-devirtualized targets keep the fast compiled link (their code is
/// receiver-klass-independent, the pre-step-5 status quo).
pub(crate) fn resolve_super_target_entry(
    vm: &mut VmState,
    super_klass: KlassOop,
    selector: SymbolOop,
    method: MethodOop,
) -> u64 {
    let devirt = vm
        .code_table
        .lookup(super_klass, selector)
        .and_then(|id| vm.code_table.get(id))
        .is_some_and(|nm| nm.self_devirt);
    if devirt {
        let c2i_shared_addr = vm.stubs.c2i_shared_addr();
        vm.adapters
            .get_or_make(&mut vm.code_cache, c2i_shared_addr, method)
    } else {
        resolve_target_entry(vm, super_klass, selector, method, true)
    }
}

/// Finds the caller nmethod and its own `IcSite` from a send site's return
/// address. `ret_addr - 4` is always the `bl`/guard-miss site itself —
/// D4.1's own invariant, true through every door a compiled send can be
/// re-entered from (a fresh `bl`, a guard's own `b`, a PIC's own indirect
/// tail-call): none of them ever touch x30 except the original `bl`
/// itself. Shared by [`rt_resolve_send`] and [`rt_dnu`] — the latter is
/// reached from EITHER `rt_resolve_send`'s own `stub_resolve` or
/// `rt_mega_lookup`'s own `stub_mega_shared`, both of which restore x30 to
/// the original send's own continuation before tail-jumping onward, so
/// this same derivation is valid starting from either one.
fn find_caller_site(
    vm: &VmState,
    ret_addr: u64,
) -> (NmethodId, CodeHandle, u32, usize, SymbolOop, u8) {
    let caller_id = vm
        .code_table
        .find_by_pc(ret_addr)
        .expect("ret_addr must fall inside a published nmethod (D4.1)");
    let caller_nm = vm
        .code_table
        .get(caller_id)
        .expect("find_by_pc just returned this id");
    let caller_code = caller_nm.code;
    let site_off = (ret_addr - 4 - caller_code.base as u64) as u32;
    let site_idx = caller_nm
        .ic_sites
        .iter()
        .position(|s| s.off == site_off)
        .unwrap_or_else(|| panic!("no IcSite at offset {site_off} in nmethod {caller_id:?}"));
    let site = &caller_nm.ic_sites[site_idx];
    (
        caller_id,
        caller_code,
        site_off,
        site_idx,
        site.selector,
        site.argc,
    )
}

/// # Safety
/// Only ever reached via `bl`/`b` from `stub_resolve`'s own hand-assembled
/// listing above, never called directly from Rust — same contract as
/// [`rt_poll`]. `argv` points at `stub_resolve`'s own RootSpill area (6
/// live `u64`s, receiver + up to 5 args); valid for the duration of this
/// call only (the stub reloads and may clobber that memory immediately
/// after, on its way to the tail-jump).
///
/// D4.1's full state machine: finds the caller nmethod and its `IcSite`
/// from `ret_addr` (`ret_addr - 4` is always the `bl`/guard-miss site
/// itself — D4.1's own invariant, true through EITHER door), resolves the
/// real target the SAME two-step way an interpreted send does (`klass_of`
/// + `runtime::lookup::lookup`), then transitions:
/// - `Unresolved` / `Mono` (same klass): patch directly, stay/become
///   `Mono{klass, target}` (`resolve_target_entry`'s own `entry`, not
///   `verified_entry` — see that function's doc).
/// - `Mono` (different klass): promote to a fresh 2-entry PIC (D4.3).
///   The OLD pair is re-resolved via `verified_entry` rather than reusing
///   its stored MONO-era target — consistent with the "same klass" arm
///   just above, which ALSO always re-resolves fresh rather than trusting
///   a stale stored value (a redefinition could have moved it since).
/// - `Pic`, room for one more: rebuild with `n+1` pairs (D3.4: PICs are
///   immutable once published, no in-place growth), free the old stub.
/// - `Pic`, already at `PIC_MAX_ENTRIES`: free the PIC, promote to the
///   selector's own (possibly newly-built, possibly shared with other
///   sites) megamorphic trampoline (D4.4) — which never patches anything
///   itself again, so `Mega` can never reach this function (D4.1: "unreachable").
///
/// An interpreted-only target (`code_table.lookup` finds no nmethod) falls
/// back to a c2i adapter (D6.1) at every one of the above — from
/// `IcState`'s own perspective this is no different from a real nmethod's
/// entry, just a different source for a target address. DNU (`lookup`
/// finds nothing) returns `stubs.dnu`'s own address WITHOUT patching the
/// site at all or touching its recorded state (D5.4) — a LATER call
/// through the same site, with a different receiver klass, might still
/// resolve successfully, so nothing here may assume this klass is
/// permanent the way a real resolution does.
pub unsafe extern "C" fn rt_resolve_send(vm: *mut VmState, ret_addr: u64, argv: *mut u64) -> u64 {
    // SAFETY: this function's own contract, guaranteed by `stub_resolve`.
    let vm = unsafe { &mut *vm };
    // P9, checked from the inside: any stub reaching Rust must have set
    // the anchor before the call (a bare `VmState::reg_block` starts at
    // 0, and `enter_compiled` debug-asserts it's clear on ENTRY — this is
    // the complementary check, that it's genuinely non-zero while a
    // runtime call is actually in flight, not just cleared again on the
    // way out).
    debug_assert_ne!(
        vm.reg_block.last_compiled_fp, 0,
        "rt_resolve_send: anchor must be set by stub_resolve's prologue before this call"
    );

    // S15 A8: every arrival here IS a compiled-IC miss (Unresolved first
    // touch, or a mono/pic guard rejecting the receiver) — hits never leave
    // compiled code, so misses are the whole observable.
    vm.stats.ic_misses += 1;
    let (caller_id, caller_code, site_off, site_idx, selector, _argc) =
        find_caller_site(vm, ret_addr);
    let site = &vm.code_table.get(caller_id).unwrap().ic_sites[site_idx];
    let prev_state = site.state;
    let super_klass = site.super_klass;

    // S13 step 10d: a `send_super` site re-dispatches from its STATIC
    // holder-superclass (D4.6), NEVER the receiver's klass — skipping subclass
    // overrides is the whole point of `super`. Its `bl` reached here only
    // because its compile-time-resolved target was invalidated (patched to
    // `not_entrant_stub`) or flushed to `Unresolved`; re-resolve the SAME static
    // super binding and re-point the `bl`, staying `Mono{super_klass, target}`.
    // This bypasses the receiver-klass dynamic lookup + poly/mega machinery
    // below entirely — a super site is monomorphic by construction. Without
    // this, the site would collapse into an ordinary dynamic send reaching the
    // very override `super` was meant to skip (the step-8-deferred blocker).
    if let Some(sk) = super_klass {
        let Some(method) = lookup(vm, sk, selector) else {
            return vm.stubs.dnu_addr();
        };
        let target = resolve_super_target_entry(vm, sk, selector, method);
        vm.code_cache
            .patch_branch26_at(caller_code, site_off, target);
        vm.code_table.get_mut(caller_id).unwrap().ic_sites[site_idx].state =
            IcState::Mono { klass: sk, target };
        return target;
    }

    // SAFETY: this function's own contract above -- `argv[0]` is always
    // the receiver, D4.1's register protocol.
    let receiver = Oop::from_raw(unsafe { *argv });
    let k = klass_of(vm, receiver);

    let Some(method) = lookup(vm, k, selector) else {
        return vm.stubs.dnu_addr();
    };

    // `patch_target`: what the SITE's own `bl` gets repointed to (governs
    // every FUTURE call through it). `ret_target`: the address THIS
    // dispatch tail-jumps to right now -- the two differ only on
    // promotion to Mega (the site is patched to the shared trampoline,
    // but this call already has (k, method) in hand, so it skips straight
    // to the real target instead of bouncing through the trampoline's own
    // fresh lookup).
    let (patch_target, ret_target, new_state) = match prev_state {
        IcState::Unresolved => {
            let t = resolve_target_entry(vm, k, selector, method, false);
            (
                t,
                t,
                IcState::Mono {
                    klass: k,
                    target: t,
                },
            )
        }
        IcState::Mono { klass, .. } if klass == k => {
            let t = resolve_target_entry(vm, k, selector, method, false);
            (
                t,
                t,
                IcState::Mono {
                    klass: k,
                    target: t,
                },
            )
        }
        IcState::Mono { klass: old_k, .. } => {
            let old_method = lookup(vm, old_k, selector)
                .expect("a klass that was already Mono-resolved must still resolve to a method");
            let old_t = resolve_target_entry(vm, old_k, selector, old_method, true);
            let new_t = resolve_target_entry(vm, k, selector, method, true);
            let resolve_addr = vm.stubs.resolve_addr();
            let smi_klass_bits = vm.universe.smi_klass.oop().raw();
            let stub = vm.pic_table.build(
                &mut vm.code_cache,
                smi_klass_bits,
                resolve_addr,
                vec![(old_k, old_t), (k, new_t)],
            );
            (stub.base as u64, new_t, IcState::Pic { stub })
        }
        IcState::Pic { stub } => {
            let mut pairs = vm.pic_table.pairs_of(stub).to_vec();
            let new_t = resolve_target_entry(vm, k, selector, method, true);
            if pairs.len() < PIC_MAX_ENTRIES {
                vm.stats.pic_extends += 1;
                pairs.push((k, new_t));
                let resolve_addr = vm.stubs.resolve_addr();
                let smi_klass_bits = vm.universe.smi_klass.oop().raw();
                let new_stub =
                    vm.pic_table
                        .build(&mut vm.code_cache, smi_klass_bits, resolve_addr, pairs);
                vm.pic_table.free(&mut vm.code_cache, stub);
                (new_stub.base as u64, new_t, IcState::Pic { stub: new_stub })
            } else {
                vm.stats.mega_transitions += 1;
                vm.pic_table.free(&mut vm.code_cache, stub);
                let mega_shared_addr = vm.stubs.mega_shared_addr();
                let mega_stub =
                    vm.mega_table
                        .get_or_make(&mut vm.code_cache, mega_shared_addr, selector);
                (
                    mega_stub.base as u64,
                    new_t,
                    IcState::Mega { stub: mega_stub },
                )
            }
        }
        IcState::Mega { .. } => unreachable!(
            "rt_resolve_send: a Mega site's own rt_mega_lookup never patches, so a site can \
             never return to stub_resolve once megamorphic (D4.4)"
        ),
    };

    vm.code_cache
        .patch_branch26_at(caller_code, site_off, patch_target);
    vm.code_table
        .get_mut(caller_id)
        .expect("caller nmethod is still installed -- this call is running ON it")
        .ic_sites[site_idx]
        .state = new_state;

    ret_target
}

/// S13 D1 §2b: `not_entrant_stub`. A now-`NotEntrant` nmethod's `entry` and
/// `verified_entry` are patched (`nmethod::make_not_entrant`) to `b
/// not_entrant_stub`. A future call reaches it through a compiled caller's
/// still-live `bl` (whose target is the old entry): at that instant x0..x5
/// hold the receiver+args exactly as the caller set them (the patched `b` is
/// the FIRST instruction at the entry — nothing has run yet), and x30 is the
/// caller's own send-site return address (a `b` doesn't touch x30, so it is
/// still whatever the caller's `bl` set — the SAME invariant `stub_resolve`'s
/// `bl`-door relies on).
///
/// The whole re-dispatch job — find the caller's `IcSite` from x30, re-resolve
/// (receiver klass, selector) via `code_table.lookup`, patch the site's `bl`
/// to the CURRENT method's target, tail-jump there — is EXACTLY
/// [`rt_resolve_send`]'s existing D4.1 machine, with nothing about it specific
/// to a "miss" versus an "invalidated entry": the MONO re-resolve arm
/// (`Mono {klass,..} if klass == k`) already re-runs a fresh `lookup`, so a
/// site whose `bl` had been patched straight to the OLD (now-NotEntrant) entry
/// gets repointed to whatever `code_table.lookup(k, selector)` returns TODAY —
/// the redefined method's fresh nmethod, or a c2i adapter if it's no longer
/// compiled. So this stub is byte-for-byte `stub_resolve`'s own listing rather
/// than a new runtime function: same prologue, same `rt_resolve_send`, same
/// tail-jump. A distinct handle (not just an alias of `resolve`) is kept only
/// so the invalidation path has its OWN stable branch target, decoupled from
/// any future divergence of the fresh-`bl` door.
///
/// **KNOWN LIMITATION — `send_super` re-dispatch (step-10 blocker).** This
/// reuse is correct for ORDINARY sends (Mono/PIC/Mega/Unresolved/c2i, all
/// adversarially verified S13 step 8). It is WRONG for a compiled `send_super`
/// site: `rt_resolve_send` re-resolves via `lookup(klass_of(self), selector)`,
/// but a super site must resolve from its STATIC holder-superclass — and the
/// runtime `IcSite` carries no super marker (`static_klass` lives only in the
/// discarded compile-time `CallSiteInfo`). So if a super-send's TARGET nmethod
/// is made `NotEntrant`, the super site silently collapses into a dynamic send
/// reaching a subclass override `super` was meant to skip. LATENT until a
/// super-target is invalidated (only the step-10 redefinition trigger does
/// that; no step-8 test hits it). The fix belongs with step 10: give the
/// runtime `IcSite` a super/static-klass marker so re-resolution stays
/// super-aware, exactly as `driver::compile_method`'s own super pre-resolution
/// already is at compile time.
fn build_not_entrant_stub() -> CodeBlob {
    let mut a = JasmAssembler::new();

    emit_stub_prologue(&mut a);
    emit_stub_kind_tag(&mut a, KIND_RESOLVE);
    a.emit("mov", &[x(0), x(28)]); // vm
    a.emit("mov", &[x(1), x(30)]); // ret_addr (captured before call_far clobbers x30)
    a.emit("sub", &[x(2), x(29), imm(ROOTSPILL_BYTES as i64)]); // argv = &RootSpill
    let rt_resolve_lit = a.literal_u64(
        rt_resolve_send as *const () as u64,
        Some(RelocKind::RuntimeAddr),
    );
    a.call_far(rt_resolve_lit);
    a.emit("mov", &[x(16), x(0)]); // result -> x16 (P4: survives the epilogue's own x0 reload)
    emit_stub_epilogue(&mut a);
    a.emit("br", &[x(16)]);

    a.finish()
}

/// S13 D6: `deopt_return_trampoline` — the RETURN-path sibling of
/// `codecache::deopt_trap::build_uncommon_trampoline`. A callee's `ret` lands
/// here when its saved-LR slot was redirected by §2c
/// ([`crate::runtime::frames::redirect_returns_into_nm`]) because its caller is
/// an in-flight activation of a now-`NotEntrant` nmethod.
///
/// **Entry state** (established by the returning callee's own epilogue
/// `ldp x29,x30,[sp],#16; ret`): `x0` = the callee's result oop; `x29` = the
/// VICTIM (nm-activation) FP — the `ldp` restored fp to the callee's caller,
/// which IS the victim; `[x29+8]` = the victim's OWN caller's return address
/// (untouched — §2c redirected the CALLEE's slot, never the victim's own).
/// `x28` = `&VmState` (preserved across the whole compiled activation, AAPCS64
/// callee-saved). This is exactly why `fp == victim_fp` holds here.
///
/// Shape vs [`crate::codecache::deopt_trap::build_uncommon_trampoline`] (D4):
/// both build a walker-visible frame record (`[fp]=victim_fp`,
/// `[fp+8]=<pc in victim nm>`) so a GC during the Rust call sees the victim
/// compiled frame at its safepoint, and both tear the record + victim frame
/// down and `ret` the deopt result to the victim's own caller. The RETURN path
/// differs in two ways: (1) the classifying pc for the record is `orig_ret_pc`
/// from `pending_deopts` (fetched via
/// [`crate::codecache::deopt_trap::rt_deopt_return_pc`], a pure lookup), where
/// D4 had the trap pc in `x16` from the signal handler; and (2) the callee's
/// result flows THROUGH — saved in a callee-saved reg across the helper call,
/// then handed to [`crate::codecache::deopt_trap::rt_deopt_on_return`] as
/// `incoming_result`.
///
/// ```text
///   mov  x19, x0                 // save callee result (x19 callee-saved)
///   mov  x20, x29                // save victim_fp     (x20 callee-saved)
///   mov  x0, x28                 // vm
///   mov  x1, x20                 // victim_fp
///   ldr  x16,<rt_deopt_return_pc>; blr x16   // x0 := orig_ret_pc (no alloc)
///   mov  x30, x0                 // saved-lr := orig_ret_pc
///   stp  x29, x30, [sp,#-16]!    // record: [fp]=victim_fp, [fp+8]=orig_ret_pc
///   mov  x29, sp                 //   re-root fp onto the record
///   mov  x0, x28                 // vm
///   mov  x1, x20                 // victim_fp
///   mov  x2, x19                 // result
///   ldr  x16,<rt_deopt_on_return>; blr x16   // x0 := deopt result oop bits
///   mov  sp, x20                 // tear down record + victim frame
///   ldp  x29, x30, [sp], #16     // pop the VICTIM's own saved {fp,lr}
///   ret                          // return the deopt result to the victim's caller
/// ```
///
/// The teardown (`sp := victim_fp`) discards BOTH the trampoline record and the
/// whole victim compiled frame, landing `sp` on the victim's own saved
/// `{fp,lr}` pair (its caller's fp + real return address), which the `ldp` pops
/// so the final `ret` returns the deopt result to the victim's ORIGINAL caller
/// — the victim activation is gone after a return-path deopt, exactly as D4's
/// trapped activation is gone after an uncommon-trap deopt. PAC is off
/// (`arm64.md` §5), so rewriting `x30`/frame surgery needs no signing.
fn build_deopt_return_trampoline() -> CodeBlob {
    let mut a = JasmAssembler::new();

    // Save the two live inputs into callee-saved regs so they survive both
    // calls: x19 = callee result, x20 = victim_fp.
    a.emit("mov", &[x(19), x(0)]);
    a.emit("mov", &[x(20), x(29)]);

    // Fetch orig_ret_pc (the victim's original return address into the nm) via
    // a pure map lookup — no allocation, so safe to call before the walker-
    // visible record exists.
    a.emit("mov", &[x(0), x(28)]); // vm
    a.emit("mov", &[x(1), x(20)]); // victim_fp
    let pc_lit = a.literal_u64(
        crate::codecache::deopt_trap::rt_deopt_return_pc as *const () as u64,
        Some(RelocKind::RuntimeAddr),
    );
    a.call_far(pc_lit);

    // Build the D4-shape walker-visible frame record: [fp]=victim_fp,
    // [fp+8]=orig_ret_pc, then re-root fp onto it so a GC inside
    // rt_deopt_on_return classifies the victim frame at its return safepoint.
    a.emit("mov", &[x(30), x(0)]); // saved-lr := orig_ret_pc
    a.emit("stp", &[x(29), x(30), mem_pre(31, -16)]);
    a.emit("mov", &[x(29), sp()]);

    // Marshal (vm, victim_fp, result) and run the return-path deopt.
    a.emit("mov", &[x(0), x(28)]); // vm
    a.emit("mov", &[x(1), x(20)]); // victim_fp (the pending_deopts key)
    a.emit("mov", &[x(2), x(19)]); // result (incoming_result)
    let rt_lit = a.literal_u64(
        crate::codecache::deopt_trap::rt_deopt_on_return as *const () as u64,
        Some(RelocKind::RuntimeAddr),
    );
    a.call_far(rt_lit);

    // Teardown (D6): sp := victim_fp discards the trampoline record AND the
    // whole victim frame; the ldp pops the victim's OWN saved {fp,lr}; ret
    // returns the deopt result (still in x0) to the victim's original caller.
    a.emit("mov", &[sp(), x(20)]);
    a.emit("ldp", &[x(29), x(30), mem_post(31, 16)]);
    a.emit("ret", &[]);

    a.finish()
}

/// D6.1: the shared c2i adapter tail. Reused unchanged from
/// [`emit_stub_prologue`]/[`emit_stub_epilogue`] rather than a hand-rolled
/// frame — "every stub that can reach Rust follows the same skeleton"
/// ([`Stubs`]'s own doc) holds for this one too, and RootSpill (x0..x5,
/// `sp`-relative, contiguous ascending once the prologue's `sub sp` runs)
/// is exactly the `argv` shape [`rt_interpret_call`] needs, for free. The
/// ONE thing that differs from `stub_resolve`'s shape: this is a
/// callee-shaped stub (a plain `ret` with x0=result, matching how the
/// ORIGINAL compiled call site's own `bl` expects an ordinary nmethod
/// `entry` to behave), not a tail-jumper — so the real result is parked in
/// x16 (spared by the epilogue, same reasoning as `stub_resolve`'s own use
/// of it, P4) across the epilogue's own x0..x5 reload, which would
/// otherwise clobber it right back to the pre-call receiver.
///
/// `c2i_<method>` (`codecache::adapters::build_c2i_adapter`) is what
/// carries the target method's own oop here, already loaded into x17 by
/// the time its own `br` lands — moved straight into x1
/// (`rt_interpret_call`'s `method_bits` argument) rather than spilled to a
/// stack slot: it only needs to survive the few instructions between
/// `c2i_<method>`'s own `ldr x17,<pool>` and this stub's `mov x1,x17`,
/// which nothing in between touches.
fn build_c2i_shared() -> CodeBlob {
    let mut a = JasmAssembler::new();

    emit_stub_prologue(&mut a);
    emit_stub_kind_tag(&mut a, KIND_C2I);
    a.emit("mov", &[x(0), x(28)]); // vm
    a.emit("mov", &[x(1), x(17)]); // method_bits, carried through from c2i_<method>
    a.emit("mov", &[x(2), sp()]); // argv = &RootSpill (x0..x5, contiguous)
    let rt_interpret_call_lit = a.literal_u64(
        rt_interpret_call as *const () as u64,
        Some(RelocKind::RuntimeAddr),
    );
    a.call_far(rt_interpret_call_lit);
    a.emit("mov", &[x(16), x(0)]); // result -> x16, survives the epilogue's own x0 reload
    emit_stub_epilogue(&mut a);
    a.emit("mov", &[x(0), x(16)]); // restore the real result
    a.emit("ret", &[]);

    a.finish()
}

/// # Safety
/// Only ever reached via `blr`/`bl` from `c2i_shared`'s own hand-assembled
/// listing above (through `call_far`), never called directly from Rust —
/// same contract as [`rt_poll`]/[`rt_resolve_send`]. `argv` points at
/// `c2i_shared`'s own RootSpill area (6 live `u64`s, receiver + up to 5
/// args, contiguous ascending); valid for the duration of this call only.
///
/// D6.1's C→I path: resolves `method_bits` to a real `MethodOop` (trusted
/// — it came from the adapter's own `RelocKind::Oop` pool word, which
/// `AdapterTable::get_or_make` only ever populates from a genuine
/// `MethodOop`), reads exactly `method.argc()` real args off `argv`
/// (deliberately NOT a separately-passed argc parameter — D5's own
/// pseudocode signature suggests one, but a method's own arity is a fixed
/// property of the method oop, always the SAME value this call's own
/// adapter was generated for, so there is nothing independent for a
/// second value to cross-check), pushes `TierLink::IntoInterpreter` (the
/// anchor D6.3's later NLR unwinder will need to find this boundary),
/// runs the method via `interpreter::run_method_reentrant` (NOT plain
/// `run_method` — this call may itself be nested inside an OUTER,
/// currently-paused interpreter activation reached via an earlier I→C
/// transition; `run_method_reentrant`'s own doc explains why that
/// distinction matters), pops the `TierLink`, and returns the raw result.
pub unsafe extern "C" fn rt_interpret_call(
    vm: *mut VmState,
    method_bits: u64,
    argv: *const u64,
) -> u64 {
    // SAFETY: this function's own contract, guaranteed by `c2i_shared`.
    let vm = unsafe { &mut *vm };
    debug_assert_ne!(
        vm.reg_block.last_compiled_fp, 0,
        "rt_interpret_call: anchor must be set by c2i_shared's prologue before this call"
    );

    let method = MethodOop::try_from(Oop::from_raw(method_bits)).expect(
        "rt_interpret_call: method_bits must be a genuine MethodOop -- AdapterTable::get_or_make's own RelocKind::Oop pool word",
    );
    let real_argc = method.argc();
    // SAFETY: this function's own contract above -- argv[0] is the
    // receiver, argv[1..=real_argc] the real Smalltalk args, D4.1's
    // register protocol (shared with `rt_resolve_send`'s own argv).
    let receiver = Oop::from_raw(unsafe { *argv });
    let args: Vec<Oop> = (0..real_argc)
        .map(|i| Oop::from_raw(unsafe { *argv.add(1 + i) }))
        .collect();

    // Captured BEFORE the nested run: the interpreted body may itself enter
    // and leave compiled code, clobbering the anchor the repatch below needs.
    let ret_addr = vm.reg_block.last_compiled_pc;
    // The nested run below can trigger a MOVING GC (scavenge or full), so the
    // raw `method`/`receiver` oops the escape hatch needs AFTERWARDS are held
    // through handles for the duration — the same discipline
    // `run_method_reentrant` itself applies to the saved outer method
    // (review finding: post-run reads through the pre-run raw copies would
    // dangle after a collection).
    let (result, method, receiver) = {
        let scope = crate::memory::handles::HandleScope::enter(vm);
        let method_h = scope.handle(vm, method.oop());
        let receiver_h = scope.handle(vm, receiver);
        vm.tier_links.push(TierLink::IntoInterpreter {
            compiled_fp: vm.reg_block.last_compiled_fp,
            compiled_ret_pc: vm.reg_block.last_compiled_pc,
        });
        let result = crate::interpreter::run_method_reentrant(vm, method, receiver, &args);
        vm.tier_links.pop();
        let method = MethodOop::try_from(method_h.get(vm))
            .expect("rt_interpret_call: the c2i adapter's method survived the nested run");
        (result, method, receiver_h.get(vm))
    };
    // S14 step 5 (the c2i escape hatch): a method reached ONLY through
    // compiled c2i calls never passes the interpreter's own compile trigger
    // (`activate_method`) — before step 5, some caller always trapped into
    // the interpreter eventually and warmed things up, but a fully
    // devirtualized caller never deopts, so without this the callee would
    // interpret FOREVER behind a frozen c2i site (the bench-smoke tripwire
    // caught exactly that: arith collapsing back to interpreter speed).
    // Mirror the trigger here: bump the invocation counter, compile (or
    // reuse) the (receiver-klass, selector) nmethod past the threshold, and
    // re-point the calling MONO site at the fresh compiled entry so every
    // FUTURE call dispatches compiled. This runs AFTER the interpreted run
    // below on purpose: that run just WARMED the method's own inner ICs, so
    // the compile sees real feedback (compiling before it would bake a cold
    // trap-laden v0 whose second trap re-executes an arbitrarily long call
    // interpreted — the arith bench's own 5M-iteration worst case).
    if let crate::runtime::vm_state::JitMode::Threshold(n) = vm.options.jit {
        let bumped = crate::interpreter::send::bump_invocation(method);
        if bumped >= n as i64 && !method.compile_disabled() {
            let k = klass_of(vm, receiver);
            let sel = SymbolOop::try_from(method.selector())
                .expect("a method's selector is always a Symbol");
            // DISPATCH-TRUTH guard (review finding): compile+install ONLY when
            // this method is what dynamic lookup for (k, sel) actually finds.
            // A SUPER-dispatched c2i callee is an ANCESTOR method — installing
            // it under the SUBCLASS key would make every future normal send of
            // sel to a k receiver execute the ancestor instead of the
            // override; likewise a method the nested run just REDEFINED must
            // not be resurrected under the fresh key.
            let is_dispatch_truth =
                lookup(vm, k, sel).is_some_and(|m2| m2.oop().raw() == method.oop().raw());
            if !is_dispatch_truth {
                return result.raw();
            }
            let existing = vm.code_table.lookup(k, sel).filter(|&id| {
                vm.code_table
                    .get(id)
                    .is_some_and(|nm| matches!(nm.state, crate::codecache::nmethod::NmState::Alive))
            });
            let compiled =
                existing.or_else(|| crate::compiler::driver::compile_method(vm, k, method));
            if let Some(id) = compiled {
                // Re-point the calling site's `bl` (its return address is the
                // anchored last_compiled_pc) — but ONLY a Mono site whose
                // recorded klass is this receiver's: a Pic/Mega site's shared
                // stub serves other klasses too and is left to its own
                // machinery. The patched target is the FULL `entry` (klass
                // guard included), same as rt_resolve_send's mono link.
                if vm.code_table.find_by_pc(ret_addr).is_some() {
                    let (caller_id, caller_code, site_off, site_idx, _sel2, _argc) =
                        find_caller_site(vm, ret_addr);
                    let site = &vm.code_table.get(caller_id).unwrap().ic_sites[site_idx];
                    let is_mono_k = matches!(site.state, IcState::Mono { klass, .. } if klass == k);
                    if is_mono_k && site.super_klass.is_none() {
                        let nm = vm.code_table.get(id).expect("just compiled/looked up");
                        let target = nm.code.base as u64 + nm.entry_off as u64;
                        vm.code_cache
                            .patch_branch26_at(caller_code, site_off, target);
                        vm.code_table.get_mut(caller_id).unwrap().ic_sites[site_idx].state =
                            IcState::Mono { klass: k, target };
                    }
                }
            }
        }
    }

    result.raw()
}

/// D4.4: the shared megamorphic-lookup tail. Tail-jump-shaped, like
/// `stub_resolve` (NOT callee-shaped like `c2i_shared`/`rt_interpret_call`)
/// — `rt_mega_lookup` never patches anything, so there is no "this call's
/// own result" to distinguish from "what a FUTURE call through the same
/// site should reach"; it's always the same address, tail-jumped to
/// directly with the ORIGINAL send site's own x30 still intact.
fn build_mega_shared() -> CodeBlob {
    let mut a = JasmAssembler::new();

    emit_stub_prologue(&mut a);
    emit_stub_kind_tag(&mut a, KIND_MEGA);
    a.emit("mov", &[x(0), x(28)]); // vm
    a.emit("mov", &[x(1), x(16)]); // selector_bits, carried through from mega_<sel>
    a.emit("mov", &[x(2), sp()]); // argv = &RootSpill (x0..x5, contiguous)
    let rt_mega_lookup_lit = a.literal_u64(
        rt_mega_lookup as *const () as u64,
        Some(RelocKind::RuntimeAddr),
    );
    a.call_far(rt_mega_lookup_lit);
    a.emit("mov", &[x(16), x(0)]); // result -> x16, survives the epilogue's own x0 reload
    emit_stub_epilogue(&mut a);
    a.emit("br", &[x(16)]);

    a.finish()
}

/// # Safety
/// Only ever reached via `blr`/`bl` from `stub_mega_shared`'s own
/// hand-assembled listing above (through `call_far`), never called
/// directly from Rust — same contract as [`rt_resolve_send`]. `argv`
/// points at `stub_mega_shared`'s own RootSpill area (6 live `u64`s,
/// receiver + up to 5 args, contiguous ascending); valid for the duration
/// of this call only.
///
/// D4.4's own words: "probes lookup cache (SPEC §6.1), full lookup on
/// miss, NEVER patches the site" -- `runtime::lookup::lookup` already IS
/// that probe-then-full-walk (`LookupCache` is its own first stop), so
/// this is a thin wrapper around the SAME `resolve_target_entry` helper
/// `rt_resolve_send` itself uses, with `use_verified=true` (this
/// function's own fresh `klass_of`+`lookup` IS the verification a PIC's
/// guard chain would otherwise have done, `resolve_target_entry`'s own
/// doc) -- and, critically, no patch call anywhere: once a site is
/// `Mega`, it stays `Mega` forever (D4.1's own state table has no exit
/// from it).
pub unsafe extern "C" fn rt_mega_lookup(
    vm: *mut VmState,
    selector_bits: u64,
    argv: *mut u64,
) -> u64 {
    // SAFETY: this function's own contract, guaranteed by `stub_mega_shared`.
    let vm = unsafe { &mut *vm };
    debug_assert_ne!(
        vm.reg_block.last_compiled_fp, 0,
        "rt_mega_lookup: anchor must be set by stub_mega_shared's prologue before this call"
    );

    let selector = SymbolOop::try_from(Oop::from_raw(selector_bits)).expect(
        "rt_mega_lookup: selector_bits must be a genuine SymbolOop -- MegaTable::get_or_make's \
         own RelocKind::Oop pool word",
    );
    // SAFETY: this function's own contract above -- argv[0] is the
    // receiver, D4.1's register protocol (shared with `rt_resolve_send`'s
    // own argv).
    let receiver = Oop::from_raw(unsafe { *argv });
    let k = klass_of(vm, receiver);

    let Some(method) = lookup(vm, k, selector) else {
        return vm.stubs.dnu_addr();
    };
    resolve_target_entry(vm, k, selector, method, true)
}

/// D5.4: the shared, callee-shaped DNU tail. Tail-jumped to (never
/// patched into any site — `rt_resolve_send`/`rt_mega_lookup`'s own doc)
/// from EITHER `stub_resolve`'s or `stub_mega_shared`'s own generic
/// epilogue, both of which restore x30 to the ORIGINAL send's own
/// continuation before getting here — same "capture ret_addr before
/// call_far clobbers it, callee-shaped return" pattern as `c2i_shared`.
fn build_stub_dnu() -> CodeBlob {
    let mut a = JasmAssembler::new();

    emit_stub_prologue(&mut a);
    emit_stub_kind_tag(&mut a, KIND_DNU);
    a.emit("mov", &[x(0), x(28)]); // vm
    a.emit("mov", &[x(1), x(30)]); // ret_addr, captured before call_far clobbers it
    a.emit("mov", &[x(2), sp()]); // argv = &RootSpill (x0..x5, contiguous)
    let rt_dnu_lit = a.literal_u64(rt_dnu as *const () as u64, Some(RelocKind::RuntimeAddr));
    a.call_far(rt_dnu_lit);
    a.emit("mov", &[x(16), x(0)]); // result -> x16, survives the epilogue's own x0 reload
    emit_stub_epilogue(&mut a);
    a.emit("mov", &[x(0), x(16)]); // restore the real result
    a.emit("ret", &[]);

    a.finish()
}

/// # Safety
/// Only ever reached via `br` from `stub_resolve`'s or `stub_mega_shared`'s
/// own epilogue (both tail-jumping to whatever `rt_resolve_send`/
/// `rt_mega_lookup` returned on a lookup miss), never called directly from
/// Rust. `argv` points at the ORIGINAL send site's own RootSpill area (6
/// live `u64`s, receiver + up to 5 args, contiguous ascending) — neither
/// intermediate stub's own epilogue touches it beyond the standard
/// reload, so it's exactly what the original send set up.
///
/// D5.4: builds a `Message` (selector + args array) mirroring
/// `interpreter::send::dnu`'s own recipe (that function reads its args off
/// `vm.stack`; this one off the RootSpill argv, matching every other S11
/// runtime stub) — `find_caller_site` (shared with `rt_resolve_send`)
/// re-derives the selector/argc this SITE was built with from `ret_addr`,
/// exactly the same way regardless of which door (`rt_resolve_send` or
/// `rt_mega_lookup`) led here, since an `IcSite`'s own `selector`/`argc`
/// never change, only its `state` does. Then sends `#doesNotUnderstand:`
/// via the same lookup+`run_method_reentrant` pattern `rt_interpret_call`
/// already uses (this call may be nested inside a paused interpreter
/// activation just like any other C→I path, for the exact same reason).
/// Falls back to `runtime::error::dnu_fallback` (prints + exits, `-> !`)
/// only if EVEN `#doesNotUnderstand:` itself isn't found on the
/// receiver's klass — unreachable in a real Smalltalk image (`Object`
/// always implements it), matching how the interpreter's own `dnu()`
/// treats the identical case.
pub unsafe extern "C" fn rt_dnu(vm: *mut VmState, ret_addr: u64, argv: *mut u64) -> u64 {
    // SAFETY: this function's own contract, guaranteed by `stub_dnu`.
    let vm = unsafe { &mut *vm };
    debug_assert_ne!(
        vm.reg_block.last_compiled_fp, 0,
        "rt_dnu: anchor must be set by stub_dnu's prologue before this call"
    );

    let (caller_id, _caller_code, _site_off, _site_idx, selector, argc_total) =
        find_caller_site(vm, ret_addr);
    let real_argc = argc_total as usize - 1;

    // SAFETY: this function's own contract above -- argv[0] is the
    // receiver, argv[1..=real_argc] the real Smalltalk args, D4.1's
    // register protocol.
    let receiver = Oop::from_raw(unsafe { *argv });
    let k = klass_of(vm, receiver);
    crate::runtime::error::trace_dnu(vm, &format!("compiled nm={}", caller_id.0), k, selector);

    // `selector`/`k`/`receiver` are bare parameters held across BOTH
    // allocations below (the interpreter's own `send::dnu` holds the
    // same values across the same two allocations, for the same reason —
    // S7-10 GC_STRESS audit).
    let scope = crate::memory::handles::HandleScope::enter(vm);
    let selector_h = scope.handle(vm, selector);
    let k_h = scope.handle(vm, k);
    let receiver_h = scope.handle(vm, receiver);

    let array_klass = vm.universe.array_klass;
    let args_array = crate::memory::alloc::alloc_indexable_oops(vm, array_klass, real_argc);
    for i in 0..real_argc {
        // SAFETY: this function's own contract above.
        args_array.at_put(i, Oop::from_raw(unsafe { *argv.add(1 + i) }));
    }
    let array_h = scope.handle(vm, args_array.oop());

    let message_klass = vm.universe.message_klass;
    let msg = crate::memory::alloc::alloc_slots(vm, message_klass);
    msg.set_body_oop(0, selector_h.get(vm).oop());
    msg.set_body_oop(1, array_h.get(vm));

    let sel_dnu = vm.universe.sel_does_not_understand;
    match lookup(vm, k_h.get(vm), sel_dnu) {
        Some(dnu_method) => {
            let result = crate::interpreter::run_method_reentrant(
                vm,
                dnu_method,
                receiver_h.get(vm),
                &[msg.oop()],
            );
            result.raw()
        }
        None => crate::runtime::error::dnu_fallback(vm, selector_h.get(vm), k_h.get(vm)),
    }
}

/// D1/D5: the shared `BoolBr.not_bool` runtime helper — callee-shaped
/// (plain `ret`, x0=result), same pattern as `c2i_shared`/`stub_dnu`. Only
/// one argument (`val`, arriving in x0), so it's copied to x1 (`rt_must_be
/// _boolean`'s own 2nd parameter) BEFORE x0 gets overwritten with `vm` —
/// `emit_stub_prologue`'s own `stp x0,x1,...` already spilled val's
/// original value to RootSpill by this point, so the register itself is
/// free to reuse.
///
/// Built and published now (S11 step 6, completing the `RuntimeStubs`
/// table's own roster), but not yet reachable from any REAL compiled
/// method — `Ir::CallRuntime{stub: MustBeBoolean}`'s own emission (a
/// `not_bool` block calling this instead of the deleted `Ir::Bailout`,
/// D1's own words) is S11 step 7's job, tied to that step's broader
/// eligibility relaxation. Testable in isolation now regardless, via a
/// direct `Stubs::invoke` the same way any nmethod entry would be.
fn build_stub_must_be_boolean() -> CodeBlob {
    let mut a = JasmAssembler::new();

    emit_stub_prologue(&mut a);
    emit_stub_kind_tag(&mut a, KIND_MUST_BE_BOOLEAN);
    a.emit("mov", &[x(1), x(0)]); // val -> x1, before x0 is overwritten below
    a.emit("mov", &[x(0), x(28)]); // vm
    let lit = a.literal_u64(
        rt_must_be_boolean as *const () as u64,
        Some(RelocKind::RuntimeAddr),
    );
    a.call_far(lit);
    a.emit("mov", &[x(16), x(0)]); // result -> x16, survives the epilogue's own x0 reload
    emit_stub_epilogue(&mut a);
    a.emit("mov", &[x(0), x(16)]); // restore the real result
    a.emit("ret", &[]);

    a.finish()
}

/// # Safety
/// Only ever reached via `blr` from `stub_must_be_boolean`'s own
/// hand-assembled listing above (through `call_far`), never called
/// directly from Rust.
///
/// Mirrors the interpreter's own `must_be_boolean_send` exactly (same
/// klass_of + lookup + activate/fallback shape; that function additionally
/// rolls `vm.regs.bci` back to the branch opcode itself first, which has
/// no compiled-code equivalent — compiled code has no bci at all): sends
/// `#mustBeBoolean` (unary) via lookup + `run_method_reentrant`, expected
/// to return a real boolean the caller's own re-tested branch will
/// consume (once S11 step 7 wires the caller side). If NOT found, goes
/// STRAIGHT to `runtime::error::dnu_fallback` (prints + exits, `-> !`) —
/// deliberately not a full `#doesNotUnderstand:` send-and-fallback the way
/// [`rt_dnu`] is: the interpreter's own `must_be_boolean_send` makes the
/// identical choice, since this isn't a real user-level message send in
/// the first place, just a boolean-coercion protocol step.
pub unsafe extern "C" fn rt_must_be_boolean(vm: *mut VmState, val: u64) -> u64 {
    // SAFETY: this function's own contract, guaranteed by `stub_must_be_boolean`.
    let vm = unsafe { &mut *vm };
    debug_assert_ne!(
        vm.reg_block.last_compiled_fp, 0,
        "rt_must_be_boolean: anchor must be set by stub_must_be_boolean's prologue before this call"
    );

    let t = Oop::from_raw(val);
    let k = klass_of(vm, t);
    let sel = vm.universe.sel_must_be_boolean;
    match lookup(vm, k, sel) {
        Some(m) => crate::interpreter::run_method_reentrant(vm, m, t, &[]).raw(),
        None => crate::runtime::error::dnu_fallback(vm, sel, k),
    }
}

/// D7: `stub_alloc_slow` — the inline-allocation fast path's overflow tail.
/// The compiled `Ir::Alloc` sequence puts the class oop in x0 and the object
/// size in bytes in x1, then `bl`s here. Same callee-shaped skeleton as
/// `stub_must_be_boolean` (anchor → frame → RootSpill → `call_far` →
/// restore → clear anchor → plain `ret`, result oop in x0). Marshals into
/// `rt_alloc_slow(vm, klass_bits, size_bytes)`: `vm` from x28 into x0, and
/// x0/x1 shifted up to x1/x2 (size first, so the klass in x0 isn't clobbered
/// before it's read).
fn build_stub_alloc_slow() -> CodeBlob {
    let mut a = JasmAssembler::new();

    emit_stub_prologue(&mut a);
    emit_stub_kind_tag(&mut a, KIND_ALLOC_SLOW);
    a.emit("mov", &[x(2), x(1)]); // size_bytes -> x2 (read x1 before it's overwritten)
    a.emit("mov", &[x(1), x(0)]); // klass_bits -> x1 (read x0 before it's overwritten)
    a.emit("mov", &[x(0), x(28)]); // vm
    let lit = a.literal_u64(
        rt_alloc_slow as *const () as u64,
        Some(RelocKind::RuntimeAddr),
    );
    a.call_far(lit);
    a.emit("mov", &[x(16), x(0)]); // result -> x16, survives the epilogue's own x0 reload
    emit_stub_epilogue(&mut a);
    a.emit("mov", &[x(0), x(16)]); // restore the real result
    a.emit("ret", &[]);

    a.finish()
}

/// # Safety
/// Only ever reached via `blr` from `stub_alloc_slow`'s own hand-assembled
/// listing above (through `call_far`), never called directly from Rust —
/// `vm` must be `x28` (D4's own invariant, established by `call_stub`).
///
/// D7's slow edge: an inline eden bump overflowed, so allocate the object
/// the ordinary way. Because this is reached FROM compiled code,
/// `compiled_depth > 0`, so `alloc_slots` → `alloc_words` takes the D8
/// bridge's old-direct path (no moving GC — the outer compiled frame's
/// spill-slot oops are still invisible to the collector until S12). Returns
/// the freshly allocated object's tagged oop bits in x0. `klass_bits` came
/// from the `Alloc` site's own `RelocKind::Oop` pool word (a genuine
/// `KlassOop`, GC-tracked); `size_bytes` is redundant with the klass's own
/// `non_indexable_size` and cross-checked in debug.
pub unsafe extern "C" fn rt_alloc_slow(vm: *mut VmState, klass_bits: u64, size_bytes: u64) -> u64 {
    // SAFETY: this function's own contract, guaranteed by `stub_alloc_slow`.
    let vm = unsafe { &mut *vm };
    debug_assert_ne!(
        vm.reg_block.last_compiled_fp, 0,
        "rt_alloc_slow: anchor must be set by stub_alloc_slow's prologue before this call"
    );
    debug_assert!(
        vm.compiled_depth > 0,
        "rt_alloc_slow: only ever reached from compiled code (compiled_depth must be > 0)"
    );
    let klass = KlassOop::try_from(Oop::from_raw(klass_bits)).expect(
        "rt_alloc_slow: klass_bits must be a genuine KlassOop -- the Alloc site's own RelocKind::Oop pool word",
    );
    debug_assert!(
        matches!(klass.format(), crate::oops::klass::Format::Slots),
        "rt_alloc_slow: only Format::Slots basicNew is inlined (D7)"
    );
    debug_assert_eq!(
        klass.non_indexable_size() * crate::oops::layout::WORD_SIZE,
        size_bytes as usize,
        "rt_alloc_slow: the Alloc site's baked size must match the klass's own non_indexable_size"
    );
    // S12 D3 test hook (`VmState::test_walk_capture`'s own doc): this is
    // one of the six anchor-setting stubs, so it's a real place a test can
    // observe `runtime::frames::walk_frames` against a genuine in-flight
    // native chain rather than hand-faked memory. `catch_unwind` here (not
    // at the test level) is load-bearing, not defensive style: a panic
    // must never be allowed to unwind across this function's own
    // `extern "C"` boundary into the hand-assembled native frames below it
    // (UB) -- the walker's own panics (a real possibility this hook
    // exists specifically to exercise, `walker_terminates_on_torn_
    // tierlinks`) are caught and reported back through the Result instead.
    if vm.test_walk_capture.is_some() {
        let torn = if vm.test_tear_tier_links_before_walk {
            vm.test_tear_tier_links_before_walk = false;
            vm.tier_links.pop()
        } else {
            None
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut seen = Vec::new();
            crate::runtime::frames::walk_frames(vm, |fv| seen.push(fv));
            seen
        }))
        .map_err(|e| {
            e.downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "walk_frames panicked with a non-string payload".to_string())
        });
        // Restore what the tear took: the test's claim is only that the
        // WALKER fails loudly against torn links — the tear must not
        // outlive the observed walk, because (S12 step 7, no more D8
        // bridge) the ordinary `alloc_slots` below can genuinely
        // scavenge, and THAT walk needs the links intact (a panic there
        // would unwind across this fn's own `extern "C"` boundary — UB —
        // exactly what this hook's own `catch_unwind` exists to prevent
        // for the walk it actually wants to observe).
        vm.tier_links.extend(torn);
        vm.test_walk_capture = Some(result);
    }
    // S12 step 4 test hook (`VmState::test_scavenge_probe`'s own doc):
    // same rationale as `test_walk_capture` just above — this is the one
    // place a test can force a REAL `scavenge` to run against a REAL live
    // compiled frame's own spill slot, which is the only honest way to
    // prove `memory::roots::each_code_root`'s new frame-scanning code
    // actually relocates it correctly. `catch_unwind` here for the same
    // not-safe-to-unwind-across-`extern "C"` reason as `test_walk_capture`.
    if vm.test_force_scavenge_in_alloc_slow {
        vm.test_force_scavenge_in_alloc_slow = false;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut frames = Vec::new();
            crate::runtime::frames::walk_frames(vm, |fv| frames.push(fv));
            let (fp, ret_pc, nm) = frames
                .iter()
                .find_map(|fv| match fv {
                    crate::runtime::frames::FrameView::Compiled { fp, ret_pc, nm } => {
                        Some((*fp, *ret_pc, *nm))
                    }
                    _ => None,
                })
                .expect(
                    "test_force_scavenge_in_alloc_slow: caller must have a live compiled frame \
                     beneath rt_alloc_slow",
                );
            let slot = {
                let nmethod = vm.code_table.get(nm).expect("just walked through it");
                let mut slots = nmethod.oopmap_at(ret_pc).iter_slots();
                let slot = slots.next().expect(
                    "test_force_scavenge_in_alloc_slow: the caller's compiled frame must have \
                     exactly one live oop slot (its own receiver, kept alive across the Alloc \
                     safepoint) -- zero found",
                );
                assert!(
                    slots.next().is_none(),
                    "test_force_scavenge_in_alloc_slow: expected exactly one live oop slot, \
                     found more than one"
                );
                slot
            };
            let addr = (fp - 8 * (slot as u64 + 1)) as *mut u64;
            // SAFETY: `fp`/`slot` are exactly `memory::roots::
            // each_code_root`'s own compiled-frame formula, just derived
            // independently here so the test can read the slot BEFORE and
            // AFTER without going through that function itself.
            let before = unsafe { *addr };
            crate::memory::scavenge::scavenge(vm)
                .expect("test_force_scavenge_in_alloc_slow: scavenge must not stall");
            let after = unsafe { *addr };
            (before, after)
        }))
        .map_err(|e| {
            e.downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| {
                    "test_force_scavenge_in_alloc_slow panicked with a non-string payload"
                        .to_string()
                })
        });
        vm.test_scavenge_probe = Some(result);
    }
    crate::memory::alloc::alloc_slots(vm, klass).oop().raw()
}

/// S13 step 10b: `rt_poll`'s return, split across two registers by the
/// AArch64 C ABI so `stub_poll` can branch on it. A struct of two ≤8-byte
/// all-integer fields is ≤16 bytes and "fully integer", so the AAPCS64
/// "return in registers" rule (§B.2/§5.5) returns it in `x0:x1` with field 0
/// (`result`) in `x0` and field 1 (`deopted`) in `x1` — NOT via a hidden
/// x8 sret pointer (that path is only for aggregates > 16 bytes or containing
/// floats). `stub_poll`'s teardown depends on exactly this: after `bl
/// rt_poll`, `x0 = result` and `x1 = deopted`. The `#[repr(C)]` pins field
/// order = declaration order so `offset_of!(result) == 0` (see the ABI unit
/// test). `deopted` is 0 (continue the loop) or 1 (this frame was deopted;
/// `result` is the deoptee's own result, to hand to the loop's native caller).
#[repr(C)]
pub struct PollOutcome {
    pub result: u64,
    pub deopted: u64,
}

/// S13 step 10b: the loop back-edge poll's runtime side. Reached via `bl
/// rt_poll` from `stub_poll` whenever a compiled loop crosses a back-edge with
/// `reg_block.poll_flag` armed (which `flush::make_not_entrant` sets, §2d).
///
/// Two jobs:
///  1. the pre-existing one-shot `trace_on_poll` hook (fires regardless of any
///     deopt — `mixed_trace_golden`'s gate);
///  2. the loop-poll DEOPT: if `pending_deopt_flag` is set AND the polling
///     frame's OWN nmethod (the one `ret_pc` lands in) is `NotEntrant`, deopt
///     this frame exactly like `rt_uncommon_trap` does — materialize the
///     interpreter frame(s) at the LoopPoll scope and run them to completion,
///     returning the deoptee's result with `deopted = 1` so `stub_poll` tears
///     down to the loop's native caller. A poll in any OTHER (still-`Alive`)
///     frame, or with the flag unset, returns `{0, 0}` (continue the loop).
///
/// After a deopt, if no `NotEntrant` compiled frame remains on the stack, both
/// `pending_deopt_flag` and `poll_flag` are cleared (bounding polling to the
/// drain window). An escaping `NLR_SENTINEL` is propagated verbatim (never
/// treated as a normal result), exactly as `rt_deopt_on_return` does.
///
/// # Safety
/// Only ever reached via `bl rt_poll` from `stub_poll`'s own hand-assembled
/// listing above, never called directly from Rust (tests aside, which honor
/// the same contract) — `vm` must be `x28` (`&mut VmState`), `loop_fp` the
/// polling compiled frame's own FP (the saved x29 `stub_poll` reloads), and
/// `ret_pc` the return address of the `bl stub_poll` (inside the polling
/// nmethod, the LoopPoll scope's `PcDesc.code_off` key).
pub unsafe extern "C" fn rt_poll(vm: *mut VmState, loop_fp: u64, ret_pc: u64) -> PollOutcome {
    // SAFETY: this function's own contract, guaranteed by every caller
    // (`stub_poll`'s assembly, the only one there is).
    let vm = unsafe { &mut *vm };

    // (1) The one-shot trace hook — independent of any deopt.
    if vm.trace_on_poll {
        vm.trace_on_poll = false;
        crate::runtime::error::print_stack_trace(vm);
    }

    // (2) Fast continue: nothing is NotEntrant, so no loop needs deopting.
    if !vm.pending_deopt_flag {
        return PollOutcome {
            result: 0,
            deopted: 0,
        };
    }

    // Which nmethod owns this poll? A poll pc is always inside a published
    // nmethod (the poll instruction lives in compiled code); a miss should not
    // happen, so continue defensively (debug-assert to surface a VM bug).
    let nm_id = match vm.code_table.find_by_pc(ret_pc) {
        Some(id) => id,
        None => {
            debug_assert!(
                false,
                "rt_poll: ret_pc {ret_pc:#x} is not inside any published nmethod"
            );
            return PollOutcome {
                result: 0,
                deopted: 0,
            };
        }
    };

    // Is THIS frame's own nmethod the NotEntrant one? If not, the flag is for
    // some OTHER frame (or is stale) — this frame is fine, continue.
    let is_not_entrant = matches!(
        vm.code_table.get(nm_id).map(|nm| nm.state),
        Some(crate::codecache::nmethod::NmState::NotEntrant)
    );
    if !is_not_entrant {
        return PollOutcome {
            result: 0,
            deopted: 0,
        };
    }

    // Deopt this frame exactly like `rt_uncommon_trap`: a reexecute (LoopPoll)
    // site — `incoming_result: None`; the recorded loop-carried stack is read
    // from the frame, and the interpreter resumes at the loop-header bci and
    // re-executes the loop condition.
    let resume = crate::runtime::deopt::deoptimize_frame(
        vm,
        crate::runtime::deopt::FrameView {
            fp: loop_fp as usize,
            pc: ret_pc as usize,
            nm: nm_id,
            incoming_result: None,
        },
    );
    vm.note_deopt(crate::runtime::vm_state::DeoptReason::Poll); // D7 stats + trace
                                                                // S14 step 9: same walk bridge as `rt_uncommon_trap` (see
                                                                // `frames::deopt_bridge_link`).
    let bridge = crate::runtime::frames::deopt_bridge_link(vm, loop_fp);
    vm.tier_links.push(bridge);
    let result = crate::interpreter::interpret_active(vm, resume).raw();
    vm.tier_links.pop();

    // An NLR escaping through the deoptee: the nested interpreter run returned
    // the reserved-tag sentinel, NOT an oop. Hand it straight back (deopted=1
    // so `stub_poll` tears down and `ret`s it to the loop's caller, which then
    // propagates it via its own `emit_nlr_check`), never as a normal result.
    // (A call-free loop cannot itself originate an NLR — no sends — so this arm
    // is defensive symmetry with `rt_deopt_on_return`; MUST still precede any
    // `Oop::from_raw(result)`, though nothing here rebuilds an oop.)
    if result == crate::oops::layout::NLR_SENTINEL {
        return PollOutcome {
            result: crate::oops::layout::NLR_SENTINEL,
            deopted: 1,
        };
    }

    // The flags stay ARMED here. Disarming needs a native-stack walk to prove
    // "no NotEntrant frame is still running a loop", but `rt_poll` is NOT a
    // legal place to call `walk_frames`: it runs (via `bl stub_poll`) with the
    // enter_compiled `TierLink::IntoCompiled` still on `vm.tier_links` and NO
    // anchor set (`stub_poll` is not one of the six anchor-setting stubs), so
    // `walk_frames`' start rule would assert (`last_compiled_fp == 0` under an
    // IntoCompiled innermost) and — since this is `extern "C"` — that panic
    // aborts the VM. The just-deopted nmethod is still `NotEntrant` anyway (it
    // is freed only by the step-10c zombie sweep), so re-arming is idempotent
    // and correct; disarming is deferred to that sweep, which runs at a GC
    // safepoint where the walk IS legal. Until then every loop keeps polling
    // and `rt_poll` returns `{0,0}` fast for any still-`Alive` frame — a bounded
    // perf cost, never a correctness issue.
    PollOutcome { result, deopted: 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cache() -> CodeCache {
        CodeCache::new(1 << 20).unwrap()
    }

    /// S10 P6: the stub's own listing must save AND restore exactly the
    /// allocatable set (x0-x15) — no more, no less.
    #[test]
    fn poll_stub_preserves_x0_x15() {
        let blob = build_stub_poll();
        // Reg's derived Debug is `Reg { class: X, num: N, is_sp: false }` —
        // no listing line ever literally spells "x29", so the fp/lr save
        // (the one `stp`/`ldp` this function DIDN'T reuse for x0-x15) is
        // excluded by its `num: 29` field instead. The trailing comma on
        // `want` matters: `num: 1` is a substring of `num: 10`/`num: 11`.
        let is_x0_15_pair = |l: &&String, mnem: &str| {
            l.contains(mnem) && !l.contains("num: 29,") && !l.contains("num: 30,")
        };
        let saved: Vec<String> = blob
            .listing
            .iter()
            .filter(|l| is_x0_15_pair(l, "stp "))
            .cloned()
            .collect();
        let restored: Vec<String> = blob
            .listing
            .iter()
            .filter(|l| is_x0_15_pair(l, "ldp "))
            .cloned()
            .collect();
        assert_eq!(saved.len(), 8, "8 stp pairs cover x0..x15");
        assert_eq!(restored.len(), 8, "8 ldp pairs restore x0..x15");
        for i in 0..16u8 {
            let want = format!("num: {i},");
            assert!(
                saved.iter().any(|l| l.contains(&want)),
                "x{i} must appear in a saving stp"
            );
            assert!(
                restored.iter().any(|l| l.contains(&want)),
                "x{i} must appear in a restoring ldp"
            );
        }
    }

    /// S13 step 10b: the poll stub's CONTINUE path still restores x0-x15 (the
    /// loop's live regs), AND it now has a deopt-teardown branch. Structural
    /// check on the listing: a `cbz` on the `deopted` flag (x1), and — reached
    /// only when `deopted != 0` — the teardown that discards the loop frame
    /// (`mov sp, x2` after loading loop_fp, then `ldp x29,x30` popping the loop
    /// frame's own record). The x0-x15 save/restore invariant is covered by
    /// `poll_stub_preserves_x0_x15`; this asserts the NEW branch exists.
    #[test]
    fn poll_stub_has_deopt_teardown_branch() {
        let blob = build_stub_poll();
        // A `cbz` (conditional-branch-on-zero) must appear — the `deopted == 0`
        // continue test. `cbz` lowers to a `cbz`/`b`-shaped listing entry.
        assert!(
            blob.listing.iter().any(|l| l.contains("cbz")),
            "the poll stub must branch on rt_poll's `deopted` flag (cbz), got:\n{}",
            blob.listing.join("\n")
        );
        // The teardown's `mov sp, x2` — the SP-register move that discards the
        // stub_poll frame + loop locals down to loop_fp. `sp` is x31/is_sp, so
        // the listing spells the destination with `is_sp: true`.
        assert!(
            blob.listing
                .iter()
                .any(|l| l.contains("mov ") && l.contains("is_sp: true")),
            "the deopt teardown must `mov sp, <reg>` to discard down to loop_fp, got:\n{}",
            blob.listing.join("\n")
        );
        // TWO `ldp x29,x30` epilogues now (one per branch: teardown + continue),
        // vs. exactly one before this step.
        let fp_lr_pops = blob
            .listing
            .iter()
            .filter(|l| l.contains("ldp ") && l.contains("num: 29,"))
            .count();
        assert_eq!(
            fp_lr_pops, 2,
            "two `ldp x29,x30` frame-record pops (deopt-teardown branch + continue branch)"
        );
    }

    /// S13 step 10b: the AArch64 C ABI returns `PollOutcome{result, deopted}`
    /// (two ≤8-byte integer fields, ≤16 bytes total, all-integer) in `x0:x1`
    /// with field 0 in x0, field 1 in x1 — `stub_poll`'s teardown depends on
    /// exactly that. `#[repr(C)]` pins declaration order, so `result` is at
    /// offset 0 and `deopted` at offset 8; the struct is 16 bytes.
    #[test]
    fn poll_outcome_abi_layout() {
        use std::mem::{offset_of, size_of};
        assert_eq!(offset_of!(PollOutcome, result), 0, "result -> x0 (field 0)");
        assert_eq!(
            offset_of!(PollOutcome, deopted),
            8,
            "deopted -> x1 (field 1)"
        );
        assert_eq!(
            size_of::<PollOutcome>(),
            16,
            "a 16-byte all-integer struct returns in x0:x1, not via an x8 sret pointer"
        );
    }

    /// S13 step 10b: `rt_poll`'s decision logic, exercised directly (no native
    /// frame needed for the early-return arms). With `pending_deopt_flag` unset
    /// it returns `{0, 0}` (continue) regardless of any nmethod state — the
    /// fast path every non-deopting poll takes. This is the arm that keeps a
    /// normal run's polls cheap.
    #[test]
    fn rt_poll_continues_when_flag_unset() {
        let mut vm = test_vm();
        vm.pending_deopt_flag = false;
        vm.trace_on_poll = false;
        // ret_pc/loop_fp are irrelevant on this arm (returns before touching
        // code_table). SAFETY: honors rt_poll's contract — `vm` is a live &mut
        // VmState; the flag-unset arm returns before dereferencing loop_fp.
        let out = unsafe { rt_poll(&mut vm as *mut VmState, 0, 0) };
        assert_eq!(out.deopted, 0, "flag unset -> continue (no deopt)");
        assert_eq!(out.result, 0, "continue outcome carries no result");
    }

    #[test]
    fn stubs_install_and_publish() {
        let mut cache = test_cache();
        let stubs = install(&mut cache);
        assert!(cache.contains(stubs.call_stub_entry() as u64));
        assert!(cache.contains(stubs.stub_poll_addr()));
        assert!(cache.contains(stubs.resolve_addr()));
        assert!(cache.contains(stubs.c2i_shared_addr()));
        assert!(cache.contains(stubs.mega_shared_addr()));
        assert!(cache.contains(stubs.dnu_addr()));
        assert!(cache.contains(stubs.must_be_boolean_addr()));
        assert!(cache.contains(stubs.alloc_slow_addr()));
        assert!(cache.contains(stubs.not_entrant_addr()));
    }

    fn test_vm() -> VmState {
        VmState::with_options(crate::runtime::vm_state::VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            gc_stress_full_period: None,
            eden_kb: None,
            jit: crate::runtime::JitMode::Off,
        })
    }

    /// S11 step 6's own explicit scope for `must_be_boolean`: the STUB
    /// itself, testable in isolation via a direct `Stubs::invoke` (same
    /// shape any nmethod entry uses) -- NOT its wiring into
    /// `Ir::CallRuntime`, which is S11 step 7's job and has no emission
    /// path to test through yet.
    #[test]
    fn must_be_boolean_sends_and_returns_result() {
        let mut vm = test_vm();
        let smi_klass = vm.universe.smi_klass;
        let sel = vm.universe.sel_must_be_boolean;
        let mut b = crate::bytecode::builder::BytecodeBuilder::new();
        b.push_true();
        b.ret_tos();
        let handler = b.finish(&mut vm, sel, 0, 0);
        crate::runtime::lookup::install_method(&mut vm, smi_klass, sel, handler);

        let stubs = vm.stubs;
        let val = crate::oops::smi::SmallInt::new(42).oop();
        let true_obj = vm.universe.true_obj;
        let result = stubs.invoke(stubs.must_be_boolean_addr(), &mut vm, &[val.raw()]);
        assert_eq!(
            result,
            true_obj.raw(),
            "a non-boolean receiver with a real #mustBeBoolean handler must reach it"
        );
    }

    // S11 step 1 had a placeholder `rt_resolve_send` (echoed `ret_addr`
    // straight back) and two tests scoped to exactly that: a round trip
    // through real assembly, and a direct Rust-level call. S11 step 3
    // replaces the placeholder with the real D4.1 lookup+patch logic,
    // which requires `ret_addr` to fall inside a genuinely installed
    // `Nmethod` with a matching `IcSite` — neither placeholder test's
    // fabricated `ret_addr`/bare `VmState` satisfies that anymore, and
    // both are superseded by `tests/it_tier1.rs`'s
    // `mono_resolve_patches_call_site_and_dispatches`, a strictly stronger
    // claim (real dispatch reaches the right target and returns the right
    // value, not just "some bytes came back unchanged").
}
