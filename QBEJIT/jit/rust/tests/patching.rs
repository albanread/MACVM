//! End-to-end proof of the two patching mechanisms added in `src/patch.rs`
//! for MACVM's needs (see that module's doc comment for the design):
//! moving-GC-safe oop slot updates, and inline-cache call-site re-patching.
//! Both are exercised against real JIT'd, executing code — not just unit
//! tests of the plumbing.

use qbejit::linker::{JumpTableEntry, RuntimeContext};
use qbejit::{compile, encode, linker, memory, patch};
use std::sync::atomic::{AtomicI32, Ordering};

/// A JIT'd function reads an oop through a `data $__macvm_oop$...` slot;
/// `update_oop_slot` changes it in place; the *same already-linked,
/// already-executable* function sees the new value on its very next call,
/// with no re-link and no W^X toggle (data is always writable).
#[test]
fn oop_slot_updates_without_relink() {
    let il = "\
export function l $get_oop() {
@start
\t%v =l loadl $__macvm_oop$widget
\tret %v
}
data $__macvm_oop$widget = { l 111 }
";

    let collector = compile::compile_il_jit(il, Some("arm64_apple")).expect("QBE compile failed");
    let module = encode::encode(collector.instructions(), 4096, 4096);
    assert!(module.errors.is_empty(), "encode() reported errors: {:?}", module.errors);

    // The convention was recognized during encoding, before linking even runs.
    assert!(
        module.oop_slots.contains_key("__macvm_oop$widget"),
        "oop_slots should contain \"__macvm_oop$widget\", got {:?}",
        module.oop_slots.keys().collect::<Vec<_>>()
    );

    let sym = module.symbols.get("get_oop").expect("no \"get_oop\" symbol");
    let fn_offset = sym.offset;

    let mut region = memory::JitMemoryRegion::allocate(4096, 4096).expect("allocate JIT memory failed");
    let (result, executable) = linker::link_and_finalize(&module, &mut region, None);
    assert!(executable, "link failed: stats={:?} diagnostics={:?}", result.stats, result.diagnostics);

    let ptr = region.get_function_ptr(fn_offset as usize).expect("get_function_ptr failed");
    // SAFETY: `get_oop` takes no arguments and returns `l` (i64/pointer-width) per the IL above.
    let get_oop: extern "C" fn() -> i64 = unsafe { std::mem::transmute(ptr) };

    assert_eq!(get_oop(), 111, "initial data-section value should read back unchanged");

    // Simulate a GC move: the object this oop pointed to relocated to a
    // new address. No re-link, no make_writable/make_executable dance —
    // patch.rs handles that internally (in this case, none is needed at
    // all, since the data region was never non-writable to begin with).
    patch::update_oop_slot(&module, &mut region, "__macvm_oop$widget", 0xDEADBEEF).expect("update_oop_slot failed");

    assert_eq!(
        get_oop(),
        0xDEADBEEFu64 as i64,
        "already-linked, already-executable code should observe the moved oop on its next call"
    );

    // And the raw address lookup agrees with what get_oop() is reading through.
    let addr = patch::oop_slot_address(&module, &region, "__macvm_oop$widget").expect("oop_slot_address failed");
    assert_eq!(region.read_data_u64((module.oop_slots["__macvm_oop$widget"]) as usize), Some(0xDEADBEEF));
    assert_ne!(addr, 0);
}

static LAST_CALLED: AtomicI32 = AtomicI32::new(0);

extern "C" fn ic_target_a() {
    LAST_CALLED.store(1, Ordering::SeqCst);
}
extern "C" fn ic_target_b() {
    LAST_CALLED.store(2, Ordering::SeqCst);
}

/// A JIT'd function calls through a `$__macvm_ic$...`-named site, resolved
/// initially via the jump table to one target; `repatch_ic_site` redirects
/// it to a different target in place, and the same already-linked function
/// observes the new target on its next call — the inline-cache
/// monomorphic-to-polymorphic transition MACVM's adaptive tier needs.
#[test]
fn ic_site_repatches_to_a_new_target() {
    let il = "\
export function w $dispatch() {
@start
\tcall $__macvm_ic$send1()
\tret 0
}
";

    let collector = compile::compile_il_jit(il, Some("arm64_apple")).expect("QBE compile failed");
    let module = encode::encode(collector.instructions(), 4096, 4096);
    assert!(module.errors.is_empty(), "encode() reported errors: {:?}", module.errors);

    assert!(
        module.ic_sites.contains_key("__macvm_ic$send1"),
        "ic_sites should contain \"__macvm_ic$send1\", got {:?}",
        module.ic_sites.keys().collect::<Vec<_>>()
    );

    let sym = module.symbols.get("dispatch").expect("no \"dispatch\" symbol");
    let fn_offset = sym.offset;

    // Give the trampoline island room for a second stub — repatch_ic_site
    // may need to write a *new* trampoline if the new target falls out of
    // direct-BL range (see patch::repatch_ic_site's doc comment).
    let mut region = memory::JitMemoryRegion::allocate_with_trampoline(4096, 4096, 4096).expect("allocate JIT memory failed");

    let entries = [JumpTableEntry {
        name: "_ic_target_a".to_string(), // QBE arm64 ABI-prefixes CALL_EXT names with '_'
        address: ic_target_a as *const () as u64,
    }];
    let ctx = RuntimeContext { entries: &entries, dlsym_handle: None };

    // The IL above calls `$__macvm_ic$send1`, not `$ic_target_a` directly —
    // in a real system MACVM's own inline-cache miss handler would emit
    // the QBE IL for a fresh call site pointing at whatever the first
    // observed receiver's method is. For this test, prove the *re-patch*
    // mechanism specifically: link once (site starts pointing at whatever
    // resolves under the `__macvm_ic$send1` name — here, nothing does, so
    // it becomes a trap stub), then immediately repatch it to
    // `ic_target_a`, call, repatch to `ic_target_b`, call again.
    let (result, executable) = linker::link_and_finalize(&module, &mut region, Some(&ctx));
    assert!(executable, "link failed: stats={:?} diagnostics={:?}", result.stats, result.diagnostics);

    let ptr = region.get_function_ptr(fn_offset as usize).expect("get_function_ptr failed");
    // SAFETY: `dispatch` takes no arguments and returns `w` (i32) per the IL above.
    let dispatch: extern "C" fn() -> i32 = unsafe { std::mem::transmute(ptr) };

    patch::repatch_ic_site(&module, &mut region, "__macvm_ic$send1", ic_target_a as *const () as u64)
        .expect("repatch_ic_site to target_a failed");
    dispatch();
    assert_eq!(LAST_CALLED.load(Ordering::SeqCst), 1, "should have called ic_target_a");

    patch::repatch_ic_site(&module, &mut region, "__macvm_ic$send1", ic_target_b as *const () as u64)
        .expect("repatch_ic_site to target_b failed");
    dispatch();
    assert_eq!(
        LAST_CALLED.load(Ordering::SeqCst),
        2,
        "already-linked, already-executable code should observe the re-patched target on its next call"
    );
}
