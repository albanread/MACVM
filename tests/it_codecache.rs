//! Sprint S9 integration/golden tests (`tests_s09.md` §Integration/golden
//! tests, acceptance gate items 2-4). Unit tests in `compiler::jasm_assembler`
//! and `codecache` (+ `codecache::guard`) cover encoding and allocator
//! mechanics in isolation; these exercise the whole emit -> publish -> run,
//! and publish -> patch -> rerun, pipelines end to end, entirely through the
//! crate's public API — same spirit as `it_gc_full.rs`'s S8 coverage.
//!
//! This file is allowed `unsafe` (it lives in `tests/`, a separate crate
//! from `macvm` itself, so `src/lib.rs`'s `#![deny(unsafe_code)]` doesn't
//! reach here) — every call into published code goes through the one
//! [`call1`] helper.
//!
//! All tests run on the main test thread, never inside `std::thread::spawn`
//! — the write-protect toggle is per-thread (S9 P3); a helper-thread version
//! would pass or fail for reasons unrelated to what's being tested.

use macvm::codecache::guard::JitWriteGuard;
use macvm::codecache::CodeCache;
use macvm::compiler::assembler::{imm, mem_post, mem_pre, sp, x, xr, Assembler, Cond, RelocKind};
use macvm::compiler::jasm_assembler::JasmAssembler;
use macvm::vendor::wfasm::relocpatch::{abs_veneer, VENEER_LEN};

/// Pinned execution helper (tests_s09.md's own worked example) — every
/// smoke test's only path into published code.
unsafe fn call1(entry: *const u8, a: u64) -> u64 {
    let f: extern "C" fn(u64) -> u64 = std::mem::transmute(entry);
    f(a)
}

/// `mov x0, #ret_val; ret` — publishes a leaf that ignores its argument and
/// always returns `ret_val`. Shared by every test that just needs *some*
/// distinguishable callable target.
fn build_leaf(cc: &mut CodeCache, ret_val: i64) -> *const u8 {
    let mut a = JasmAssembler::new();
    a.emit("mov", &[x(0), imm(ret_val)]);
    a.emit("ret", &[]);
    let blob = a.finish();
    let h = cc.alloc(blob.code.len()).unwrap();
    cc.publish(h, &blob)
}

/// `stp x29,x30,[sp,#-16]!; bl .; ldp x29,x30,[sp],#16; ret` — a
/// `bl_patchable` call site wrapped in proper frame save/restore, so the
/// callee's own `ret` doesn't clobber the caller's return address (a leaf
/// `bl_patchable; ret` with no frame would infinite-loop: `bl` overwrites
/// x30 with *this* function's own return point, so the callee returns
/// straight back into the still-unexecuted tail instead of past it).
/// Returns `(entry, site_offset)` — `site_offset` is blob-relative, per
/// `reloc_offsets_blob_relative`'s convention; add it to `entry` once
/// published to get the absolute patch site.
fn build_caller(cc: &mut CodeCache) -> (*const u8, u32) {
    let mut a = JasmAssembler::new();
    a.emit("stp", &[x(29), x(30), mem_pre(31, -16)]);
    let site_off = a.bl_patchable(RelocKind::InlineCache);
    a.emit("ldp", &[x(29), x(30), mem_post(31, 16)]);
    a.emit("ret", &[]);
    let blob = a.finish();
    let h = cc.alloc(blob.code.len()).unwrap();
    let entry = cc.publish(h, &blob);
    (entry, site_off)
}

fn read_u32(addr: *const u8) -> u32 {
    let s = unsafe { std::slice::from_raw_parts(addr, 4) };
    u32::from_le_bytes(s.try_into().unwrap())
}

/// Gate item 2(a): `(x+1)*2`.
#[test]
fn smoke_arith() {
    let mut a = JasmAssembler::new();
    a.emit("add", &[x(0), x(0), imm(1)]);
    a.emit("lsl", &[x(0), x(0), imm(1)]);
    a.emit("ret", &[]);
    let blob = a.finish();

    let mut cc = CodeCache::new(1 << 16).unwrap();
    let h = cc.alloc(blob.code.len()).unwrap();
    let entry = cc.publish(h, &blob);

    assert_eq!(unsafe { call1(entry, 20) }, 42);
    assert_eq!(unsafe { call1(entry, 0) }, 2);
}

/// Gate item 2(b): a function with an internal conditional branch, forward-
/// referenced (`b.lt` to a label bound after it).
#[test]
fn smoke_internal_branch() {
    let mut a = JasmAssembler::new();
    let l = a.new_label();
    a.emit("cmp", &[x(0), imm(10)]);
    a.b_cond(Cond::Lt, l);
    a.emit("mov", &[x(0), imm(1)]);
    a.emit("ret", &[]);
    a.bind(l);
    a.emit("mov", &[x(0), imm(0)]);
    a.emit("ret", &[]);
    let blob = a.finish();

    let mut cc = CodeCache::new(1 << 16).unwrap();
    let h = cc.alloc(blob.code.len()).unwrap();
    let entry = cc.publish(h, &blob);

    assert_eq!(unsafe { call1(entry, 3) }, 0, "3 < 10: branch taken");
    assert_eq!(unsafe { call1(entry, 30) }, 1, "30 >= 10: branch not taken");
}

pub extern "C" fn add3(a: u64) -> u64 {
    a + 3
}

/// Gate item 2(c): a call to a Rust `extern "C"` function, via
/// `call_far`'s `ldr x16, =lit; blr x16` path. Rust code is routinely far
/// outside the `MAP_JIT` region's `mmap` placement, but ASLR makes "far" a
/// property to check, not assume — skip-with-message (never a silent pass)
/// on the astronomically unlikely case they land close.
#[test]
fn smoke_call_rust_extern() {
    let target = add3 as *const () as u64;

    let mut a = JasmAssembler::new();
    a.emit("stp", &[x(29), x(30), mem_pre(31, -16)]);
    a.emit("mov", &[x(29), sp()]);
    let lit = a.literal_u64(target, Some(RelocKind::RuntimeAddr));
    a.call_far(lit);
    a.emit("ldp", &[x(29), x(30), mem_post(31, 16)]);
    a.emit("ret", &[]);
    let blob = a.finish();

    let mut cc = CodeCache::new(1 << 16).unwrap();
    let h = cc.alloc(blob.code.len()).unwrap();

    let distance = (target as i64 - h.base as i64).unsigned_abs();
    if distance < (1 << 27) {
        eprintln!(
            "smoke_call_rust_extern: skipping -- add3 (0x{target:x}) landed within \
             Branch26 range of the code cache ({distance} bytes); far-call path not \
             exercised this run"
        );
        return;
    }

    let entry = cc.publish(h, &blob);
    assert_eq!(unsafe { call1(entry, 39) }, 42);
}

/// Gate item 3 (write-protect round trip on live code): a caller's
/// `bl_patchable` site retargeted from one leaf to another, executed
/// correctly both times.
#[test]
fn patch_and_rerun_branch26() {
    let mut cc = CodeCache::new(1 << 16).unwrap();
    let ret_1 = build_leaf(&mut cc, 1);
    let ret_2 = build_leaf(&mut cc, 2);
    let (caller, site_off) = build_caller(&mut cc);
    let site = unsafe { caller.add(site_off as usize) };

    cc.patch_branch26(site, ret_1 as u64);
    assert_eq!(unsafe { call1(caller, 0) }, 1);

    cc.patch_branch26(site, ret_2 as u64);
    assert_eq!(unsafe { call1(caller, 0) }, 2);
}

/// A synthetic target 512 MiB away (never executed — nothing lives out
/// there) forces the veneer path: the `bl` field must end up pointing at a
/// freshly bump-allocated in-cache veneer whose `movz/movk` words
/// reconstruct the real target, not at the target directly (impossible in
/// 26 bits over that distance).
#[test]
fn veneer_fallback_forced() {
    let mut cc = CodeCache::new(1 << 16).unwrap();
    let (caller, site_off) = build_caller(&mut cc);
    let site = unsafe { caller.add(site_off as usize) };

    let far_target = site as u64 + (512 << 20);
    let used_before = cc.used_bytes();
    cc.patch_branch26(site, far_target);
    assert!(
        cc.used_bytes() >= used_before + VENEER_LEN,
        "a veneer must bump-allocate at least VENEER_LEN bytes"
    );

    let word = read_u32(site);
    assert_eq!(
        word & !0x03FF_FFFF,
        0x9400_0000,
        "still a bl, opcode bits untouched"
    );
    let imm26 = word & 0x03FF_FFFF;
    let signed = ((imm26 << 6) as i32 >> 6) as i64; // sign-extend the 26-bit field
    let veneer_addr = (site as i64 + (signed << 2)) as u64;
    assert_ne!(
        veneer_addr, far_target,
        "512 MiB away can't fit directly in imm26"
    );
    assert!(
        cc.contains(veneer_addr),
        "veneer must live inside the code cache"
    );

    let veneer_bytes = unsafe { std::slice::from_raw_parts(veneer_addr as *const u8, VENEER_LEN) };
    let mut got = [0u32; 5];
    for (i, word) in got.iter_mut().enumerate() {
        *word = u32::from_le_bytes(veneer_bytes[i * 4..i * 4 + 4].try_into().unwrap());
    }
    assert_eq!(
        got,
        abs_veneer(far_target),
        "veneer words must reconstruct the target"
    );
}

/// Gate item 4: a published function returning a pool constant;
/// `patch_pool_word` changes it; re-execution returns the new value with no
/// re-publish.
#[test]
fn literal_pool_patch_rerun() {
    let mut a = JasmAssembler::new();
    let lit = a.literal_u64(0x2A, None);
    a.ldr_literal(xr(0), lit);
    a.emit("ret", &[]);
    let blob = a.finish();
    let literal_off = blob.literal_off;

    let mut cc = CodeCache::new(1 << 16).unwrap();
    let h = cc.alloc(blob.code.len()).unwrap();
    let entry = cc.publish(h, &blob);
    assert_eq!(unsafe { call1(entry, 0) }, 42);

    let pool_addr = unsafe { entry.add(literal_off as usize) } as *mut u64;
    cc.patch_pool_word(pool_addr, 0x54);
    assert_eq!(unsafe { call1(entry, 0) }, 84);
}

/// Publishing the same blob at two different cache offsets must execute
/// identically both times — catches any accidental absolute intra-blob
/// reference (everything must be PC-relative or pool-relative).
#[test]
fn publish_is_position_independent() {
    let mut a = JasmAssembler::new();
    a.emit("add", &[x(0), x(0), imm(1)]);
    a.emit("ret", &[]);
    let blob = a.finish();

    let mut cc = CodeCache::new(1 << 16).unwrap();
    let h1 = cc.alloc(blob.code.len()).unwrap();
    let entry1 = cc.publish(h1, &blob);
    let h2 = cc.alloc(blob.code.len()).unwrap();
    let entry2 = cc.publish(h2, &blob);

    assert_ne!(entry1, entry2, "must actually land at different offsets");
    assert_eq!(unsafe { call1(entry1, 10) }, 11);
    assert_eq!(unsafe { call1(entry2, 10) }, 11);
}

/// Publish A, publish B, free A, publish C (A's exact size) into A's old
/// slot; B must still run correctly and C must run its own, fresh code —
/// not stale bytes left over from A. This is the case S12's flush protocol
/// will hit constantly: if a `publish`'s guard ever forgot to `note` the
/// reused range, C would deterministically execute A's leftover
/// instructions here instead of its own.
#[test]
fn freelist_reuse_executes() {
    let mut cc = CodeCache::new(1 << 16).unwrap();

    let mut aa = JasmAssembler::new();
    aa.emit("add", &[x(0), x(0), imm(1)]);
    aa.emit("ret", &[]);
    let blob_a = aa.finish();
    let ha = cc.alloc(blob_a.code.len()).unwrap();
    cc.publish(ha, &blob_a);

    let mut ab = JasmAssembler::new();
    ab.emit("add", &[x(0), x(0), imm(2)]);
    ab.emit("ret", &[]);
    let blob_b = ab.finish();
    let hb = cc.alloc(blob_b.code.len()).unwrap();
    let entry_b = cc.publish(hb, &blob_b);

    cc.free(ha);

    let mut ac = JasmAssembler::new();
    ac.emit("add", &[x(0), x(0), imm(3)]);
    ac.emit("ret", &[]);
    let blob_c = ac.finish();
    assert_eq!(
        blob_c.code.len(),
        blob_a.code.len(),
        "C must match A's size to land in its hole"
    );
    let hc = cc.alloc(blob_c.code.len()).unwrap();
    assert_eq!(hc.base, ha.base, "C must reuse A's freed slot");
    let entry_c = cc.publish(hc, &blob_c);

    assert_eq!(
        unsafe { call1(entry_b, 10) },
        12,
        "B unaffected by A's slot being reused"
    );
    assert_eq!(
        unsafe { call1(entry_c, 10) },
        13,
        "C's fresh code, not A's stale bytes"
    );
}

/// Two `bl` sites in different segments, both patched under ONE
/// `JitWriteGuard` with two noted ranges — both patches must be visible
/// after the single shared drop. Validates the multi-range flush path S12
/// will use to batch pool-word patches under one guard per GC pass (P5).
#[test]
fn two_blobs_one_guard() {
    let mut cc = CodeCache::new(1 << 16).unwrap();
    let (caller1, off1) = build_caller(&mut cc);
    let (caller2, off2) = build_caller(&mut cc);
    let site1 = unsafe { caller1.add(off1 as usize) };
    let site2 = unsafe { caller2.add(off2 as usize) };
    let target1 = build_leaf(&mut cc, 11);
    let target2 = build_leaf(&mut cc, 22);

    let bits_for = |site: *const u8, target: *const u8| -> u32 {
        let disp = target as i64 - site as i64;
        ((disp >> 2) as u32) & 0x03FF_FFFF
    };
    let bits1 = bits_for(site1, target1);
    let bits2 = bits_for(site2, target2);

    {
        let mut g = JitWriteGuard::new();
        g.note(site1, 4);
        g.note(site2, 4);
        unsafe {
            let s1 = std::slice::from_raw_parts_mut(site1 as *mut u8, 4);
            let w1 = (u32::from_le_bytes(s1.try_into().unwrap()) & !0x03FF_FFFF) | bits1;
            s1.copy_from_slice(&w1.to_le_bytes());

            let s2 = std::slice::from_raw_parts_mut(site2 as *mut u8, 4);
            let w2 = (u32::from_le_bytes(s2.try_into().unwrap()) & !0x03FF_FFFF) | bits2;
            s2.copy_from_slice(&w2.to_le_bytes());
        }
    } // one shared drop: exec mode, then icache flush over BOTH ranges

    assert_eq!(
        unsafe { call1(caller1, 0) },
        11,
        "site1's patch visible after the shared guard drops"
    );
    assert_eq!(
        unsafe { call1(caller2, 0) },
        22,
        "site2's patch visible after the shared guard drops"
    );
}

/// Manual only — crashes the process by design (proving W^X is actually
/// enforced on this machine, not silently a no-op). Run with `cargo test
/// --test it_codecache -- --ignored exec_without_publish_would_fault` once
/// per toolchain/OS bump; never in CI.
#[test]
#[ignore]
fn exec_without_publish_would_fault() {
    let mut cc = CodeCache::new(1 << 16).unwrap();
    let h = cc.alloc(16).unwrap();
    // No JitWriteGuard: this thread is in executable (protected) mode.
    // Writing to MAP_JIT memory in that mode must SIGBUS.
    unsafe {
        std::ptr::write(h.base as *mut u32, 0xD65F_03C0); // ret
    }
}
