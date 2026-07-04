//! `call_stub`/`stub_poll` (S10 D5.6) and, from S11, the runtime-stub
//! table's growing collection of anchor-and-call-into-Rust trampolines
//! (D5) — hand-assembled once at startup, not derived from any `Ir`. All
//! published into the same [`CodeCache`] real compiled methods live in.

use crate::compiler::assembler::{
    imm, mem, mem_post, mem_pre, sp, x, Assembler, CodeBlob, Cond, RelocKind,
};
use crate::compiler::jasm_assembler::JasmAssembler;
use crate::oops::layout::{
    ROOTSPILL_BYTES, VMREG_LAST_COMPILED_FP_OFFSET, VMREG_LAST_COMPILED_PC_OFFSET,
};
use crate::oops::wrappers::{KlassOop, MethodOop, SymbolOop};
use crate::oops::Oop;
use crate::runtime::lookup::{klass_of, lookup};
use crate::runtime::vm_state::{TierLink, VmState};

use super::nmethod::{IcState, NmethodId};
use super::pics::PIC_MAX_ENTRIES;
use super::{CodeCache, CodeHandle};

/// D5's shared stub skeleton, part 1: anchor + AAPCS frame + RootSpill
/// (x0..x5). Every stub that calls into Rust starts with this; follow with
/// the stub's own `call_far`, then [`emit_stub_epilogue`]. `x28` must
/// already be `&VmState` (established once by `call_stub`, D5.1) — every
/// site this is used from is reached with that invariant already holding.
/// A free function taking `&mut dyn Assembler`, not a method, so it stays
/// reusable across every stub builder in this module without any of them
/// needing to share a common base type beyond the trait itself.
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

    Stubs {
        call_stub: h1,
        stub_poll: h2,
        resolve: h3,
        c2i_shared: h4,
        mega_shared: h5,
        dnu: h6,
        must_be_boolean: h7,
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

    a.emit("mov", &[x(0), x(28)]); // vm -> rt_poll's one argument
    let rt_poll_lit = a.literal_u64(rt_poll as *const () as u64, Some(RelocKind::RuntimeAddr));
    a.call_far(rt_poll_lit);

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

    let (caller_id, caller_code, site_off, site_idx, selector, _argc) =
        find_caller_site(vm, ret_addr);
    let prev_state = vm.code_table.get(caller_id).unwrap().ic_sites[site_idx].state;

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
                (caller_id, site_off),
                vec![(old_k, old_t), (k, new_t)],
            );
            (stub.base as u64, new_t, IcState::Pic { stub })
        }
        IcState::Pic { stub } => {
            let mut pairs = vm.pic_table.pairs_of(stub).to_vec();
            let new_t = resolve_target_entry(vm, k, selector, method, true);
            if pairs.len() < PIC_MAX_ENTRIES {
                pairs.push((k, new_t));
                let resolve_addr = vm.stubs.resolve_addr();
                let smi_klass_bits = vm.universe.smi_klass.oop().raw();
                let new_stub = vm.pic_table.build(
                    &mut vm.code_cache,
                    smi_klass_bits,
                    resolve_addr,
                    (caller_id, site_off),
                    pairs,
                );
                vm.pic_table.free(&mut vm.code_cache, stub);
                (new_stub.base as u64, new_t, IcState::Pic { stub: new_stub })
            } else {
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

    vm.tier_links.push(TierLink::IntoInterpreter {
        compiled_fp: vm.reg_block.last_compiled_fp,
        compiled_ret_pc: vm.reg_block.last_compiled_pc,
    });
    let result = crate::interpreter::run_method_reentrant(vm, method, receiver, &args);
    vm.tier_links.pop();

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

    let (_caller_id, _caller_code, _site_off, _site_idx, selector, argc_total) =
        find_caller_site(vm, ret_addr);
    let real_argc = argc_total as usize - 1;

    // SAFETY: this function's own contract above -- argv[0] is the
    // receiver, argv[1..=real_argc] the real Smalltalk args, D4.1's
    // register protocol.
    let receiver = Oop::from_raw(unsafe { *argv });
    let k = klass_of(vm, receiver);

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

/// S10: nothing sets `VmRegBlock::poll_flag` nonzero yet (mirrors
/// `interpreter::poll`'s own S2-era status), so this is rarely even
/// reached in a normal run — a real interrupt/trace producer is a later
/// sprint's job (D5.6). The one thing wired up now is
/// `VmState::trace_on_poll`, tests_s10.md's `mixed_trace_golden` (gate
/// item 4) hook: a test sets it before calling into compiled code whose
/// own loop will reach a `Poll`, since compiled code is send-free (D1)
/// and so has no other way to invoke a `printStackTrace` primitive from
/// the inside. One-shot: cleared immediately, so a loop with several
/// back-edge crossings prints exactly once.
///
/// # Safety
/// Only ever reached via `bl rt_poll` from `stub_poll`'s own hand-assembled
/// listing above, never called directly from Rust — `vm` must be `x28`,
/// established once by `call_stub` and never null for the lifetime of any
/// compiled call (D4's own invariant), the same pointer `Stubs::invoke`'s
/// own `call` already trusts.
pub unsafe extern "C" fn rt_poll(vm: *mut VmState) {
    // SAFETY: this function's own contract, guaranteed by every caller
    // (`stub_poll`'s assembly, the only one there is).
    let vm = unsafe { &mut *vm };
    if vm.trace_on_poll {
        vm.trace_on_poll = false;
        crate::runtime::error::print_stack_trace(vm);
    }
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
