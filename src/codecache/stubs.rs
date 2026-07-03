//! `call_stub` (Rust↔compiled trampoline) and `stub_poll` (S10 D5.6) —
//! hand-assembled once at startup, not derived from any `Ir`. Both are
//! published into the same [`CodeCache`] real compiled methods live in.

use crate::compiler::assembler::{
    imm, mem, mem_post, mem_pre, sp, x, Assembler, CodeBlob, Cond, RelocKind,
};
use crate::compiler::jasm_assembler::JasmAssembler;
use crate::runtime::vm_state::VmState;

use super::{CodeCache, CodeHandle};

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
}

impl Stubs {
    pub fn call_stub_entry(&self) -> *const u8 {
        self.call_stub.base
    }
    pub fn stub_poll_addr(&self) -> u64 {
        self.stub_poll.base as u64
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

    Stubs {
        call_stub: h1,
        stub_poll: h2,
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
    }
}
