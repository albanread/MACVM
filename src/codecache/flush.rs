//! S12 D6: flushing an nmethod (also the S13 zombie path's own substrate,
//! per that section's own text). A free function taking `vm: &mut
//! VmState` — NOT a `CodeTable` method — because it genuinely needs THREE
//! of `VmState`'s own tables at once (`code_table`, `pic_table`,
//! `code_cache`); the same reasoning `memory::roots::each_code_root`'s own
//! doc gives for its own signature (a method receiver borrows `self` for
//! the call, so `code_table.flush(vm, id)` would try to borrow `vm.
//! code_table` and `vm` simultaneously — rejected regardless of what the
//! body does).
//!
//! Called by `memory::fullgc::full_gc`, right after its own weak sweep
//! (`CodeTable::weak_sweep`, S12 D5 point 2) — one call per member of
//! `flush_set`, strictly BEFORE the update/rewrite phase (D5 point 3: "no
//! updater touches freed memory"). The caller's own precondition (D5
//! point 2's invariant: a dead key klass implies no live activation of
//! this nmethod exists) is checked THERE (`fullgc::debug_assert_weak_
//! sweep_invariant`), not re-checked here.

use crate::runtime::vm_state::VmState;

use super::nmethod::{IcState, NmethodId};
use super::CodeHandle;

/// D6.1/D6.2: flushes `id` — marks it `Zombie` and unhooks it from
/// `by_key` (step 1), sweeps every OTHER alive nmethod's own `IcSite`s and
/// resets any whose current target lands inside `id`'s own code range
/// (step 2, D6.2), then removes it from `by_addr` and returns its
/// `CodeCache` space (step 4). Step 3 (D6.3, interpreter ICs) is
/// deliberately a no-op here — S10 D4's own id-validation dispatch already
/// makes a stale smi id self-heal on next use; no eager sweep is needed or
/// wanted for it.
///
/// # Panics
/// If `id` is not currently installed (a double-flush, or a caller that
/// didn't check `weak_sweep`'s own output) — a VM-internal-consistency
/// bug, never a condition guest code can trigger.
pub fn flush_nmethod(vm: &mut VmState, id: NmethodId) {
    let flushed_range = {
        let nm = vm
            .code_table
            .get(id)
            .expect("flush_nmethod: id must be alive");
        let base = nm.code.base as u64;
        base..(base + nm.code.len as u64)
    };

    vm.code_table.mark_zombie(id);

    // --- D6.2: compiled-site invalidation sweep --------------------------
    // Collected into an owned `Vec` first, exactly like `memory::roots::
    // each_code_root`'s own frame-walk does and for the identical reason:
    // this loop only needs SHARED access to `code_table`/`pic_table` (both
    // fine simultaneously — disjoint fields of `vm`, and neither is a
    // capturing closure that would alias `vm` itself), but applying a
    // patch needs MUTABLE access to `code_cache`/`pic_table`/`code_table`
    // in turn — cleanly sequenced only if the "what needs patching" pass
    // has already finished and released its own borrows.
    struct PatchSite {
        caller_id: NmethodId,
        caller_code: CodeHandle,
        site_off: u32,
        site_idx: usize,
        free_pic: Option<CodeHandle>,
    }
    let mut to_patch: Vec<PatchSite> = Vec::new();
    for nm in vm.code_table.iter_alive() {
        for (site_idx, site) in nm.ic_sites.iter().enumerate() {
            match site.state {
                // `Unresolved` already points at `stub_resolve`;
                // `IcState::Mega` never encodes a specific nmethod's
                // entry at all (`rt_mega_lookup` re-derives its target
                // fresh on every call, D4.4 — `mega.rs`'s own doc), so
                // neither can ever reference a dying nmethod's range.
                IcState::Mono { target, .. } if flushed_range.contains(&target) => {
                    to_patch.push(PatchSite {
                        caller_id: nm.id,
                        caller_code: nm.code,
                        site_off: site.off,
                        site_idx,
                        free_pic: None,
                    });
                }
                IcState::Pic { stub } => {
                    let hits_flushed = vm
                        .pic_table
                        .pairs_of(stub)
                        .iter()
                        .any(|&(_, t)| flushed_range.contains(&t));
                    if hits_flushed {
                        to_patch.push(PatchSite {
                            caller_id: nm.id,
                            caller_code: nm.code,
                            site_off: site.off,
                            site_idx,
                            free_pic: Some(stub),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    let resolve_addr = vm.stubs.resolve_addr();
    for p in to_patch {
        if let Some(stub) = p.free_pic {
            vm.pic_table.free(&mut vm.code_cache, stub);
        }
        // `patch_branch26_at` guards its own write (a fresh `JitWriteGuard`
        // per call, `codecache::mod`'s own doc) — flushes are rare (klass
        // death, redefinition), so one guard per patched site costs
        // nothing worth batching against (this project's own "perf work
        // is out of scope before S15" stance, `memory::fullgc`'s doc).
        vm.code_cache
            .patch_branch26_at(p.caller_code, p.site_off, resolve_addr);
        vm.code_table
            .get_mut(p.caller_id)
            .expect("still installed -- we're mid-flush of a DIFFERENT nmethod entirely")
            .ic_sites[p.site_idx]
            .state = IcState::Unresolved;
    }

    // --- D6.1 step 4 ------------------------------------------------------
    let nm = vm.code_table.remove(id);
    vm.code_cache.free(nm.code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codecache::nmethod::{IcSite, NmState, Nmethod};
    use crate::compiler::assembler::CodeBlob;
    use crate::oops::layout::MEM_TAG;
    use crate::oops::wrappers::{KlassOop, SymbolOop};
    use crate::oops::Oop;
    use crate::runtime::vm_state::{VmOptions, VmState};

    fn test_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            gc_stress_full_period: None,
            eden_kb: None,
            jit: crate::runtime::JitMode::Off,
        })
    }

    fn fake_klass(vm: &mut VmState, addr: usize) -> KlassOop {
        let _ = vm;
        // SAFETY: test-only, tag-level shape never dereferenced (this
        // module's own tests never read a klass's body, only compare raw
        // bits) -- same reasoning as `nmethod.rs`'s own private
        // `fake_klass` helper.
        unsafe { KlassOop::from_oop_unchecked(Oop::from_raw(addr as u64 + MEM_TAG)) }
    }

    /// Publishes a tiny real blob, installs it as `nm`, returns its id
    /// alongside the handle (tests build a SEPARATE "caller" nmethod whose
    /// own `ic_sites` reference `nm`'s range, then flush `nm` and check the
    /// caller's site was reset).
    fn install_blob(vm: &mut VmState, len: usize) -> CodeHandle {
        let h = vm
            .code_cache
            .alloc(len)
            .expect("code cache alloc for test blob");
        let blob = CodeBlob {
            code: vec![0u8; len],
            literal_off: len as u32,
            relocs: Vec::new(),
            listing: Vec::new(),
        };
        vm.code_cache.publish(h, &blob);
        h
    }

    fn fake_nmethod(key_klass: KlassOop, key_selector: SymbolOop, code: CodeHandle) -> Nmethod {
        Nmethod {
            id: NmethodId(0),
            key_klass,
            key_selector,
            code,
            entry_off: 0,
            verified_entry_off: 0,
            state: NmState::Alive,
            level: 1,
            version: 0,
            literal_off: 0,
            relocs: Vec::new(),
            frame_slots: 0,
            slot_is_oop: Vec::new(),
            pcdescs: Vec::new(),
            oopmaps: Vec::new(),
            ic_sites: Vec::new(),
            poll_bci: None,
        }
    }

    /// `tests_s12.md`'s `flush_resets_dependent_sites`: nmethod A is
    /// called by compiled B (a Mono site whose own target is INSIDE A's
    /// own code range) and by a PIC in C (one of the PIC's own pairs
    /// targets A). Flushing A must: reset B's site to `Unresolved` (its
    /// own `bl` repatched to `stub_resolve`), free C's PIC AND reset its
    /// owning site the same way, and return A's own code-cache range to
    /// the free list.
    #[test]
    fn flush_resets_dependent_sites() {
        let mut vm = test_vm();
        let sel = vm.universe.intern(b"target:");
        let other_sel = vm.universe.intern(b"other:");
        let klass_a = fake_klass(&mut vm, 0x1000);
        let klass_b = fake_klass(&mut vm, 0x2000);
        let klass_c = fake_klass(&mut vm, 0x3000);
        let klass_x = fake_klass(&mut vm, 0x4000);
        let klass_y = fake_klass(&mut vm, 0x5000);

        // A: the nmethod about to be flushed. Its own entry is what B's
        // and C's PIC's sites will "call".
        let a_code = install_blob(&mut vm, 32);
        let a_entry = a_code.base as u64 + 4; // an arbitrary in-range "entry"
        let a_id = vm.code_table.install(fake_nmethod(klass_a, sel, a_code));

        // B: a caller with ONE Mono IcSite whose target is A's own entry.
        let b_code = install_blob(&mut vm, 16);
        let mut b_nm = fake_nmethod(klass_b, other_sel, b_code);
        b_nm.ic_sites.push(IcSite {
            off: 0,
            selector: sel,
            argc: 0,
            state: IcState::Mono {
                klass: klass_x,
                target: a_entry,
            },
        });
        let b_id = vm.code_table.install(b_nm);

        // C: a caller whose ONE IcSite is a Pic naming a stub built with a
        // pair that targets A's own entry (alongside an unrelated pair).
        let c_code = install_blob(&mut vm, 16);
        let smi_klass_bits = vm.universe.smi_klass.oop().raw();
        let resolve_addr = vm.stubs.resolve_addr();
        let pic_stub = vm.pic_table.build(
            &mut vm.code_cache,
            smi_klass_bits,
            resolve_addr,
            vec![(klass_x, 0xDEAD_0000), (klass_y, a_entry)],
        );
        let mut c_nm = fake_nmethod(klass_c, other_sel, c_code);
        c_nm.ic_sites.push(IcSite {
            off: 0,
            selector: sel,
            argc: 0,
            state: IcState::Pic { stub: pic_stub },
        });
        let c_id = vm.code_table.install(c_nm);

        flush_nmethod(&mut vm, a_id);

        assert!(
            vm.code_table.get(a_id).is_none(),
            "the flushed nmethod's own slot must be gone"
        );
        assert!(
            matches!(
                vm.code_table.get(b_id).unwrap().ic_sites[0].state,
                IcState::Unresolved
            ),
            "B's own site must be reset to Unresolved"
        );
        assert!(
            matches!(
                vm.code_table.get(c_id).unwrap().ic_sites[0].state,
                IcState::Unresolved
            ),
            "C's own site must ALSO be reset to Unresolved (its PIC referenced A)"
        );

        // The freed range must be reusable (`CodeCache::free`'s own
        // contract, exercised the same way `pics.rs`'s own
        // `pic_table_build_pairs_of_free_round_trip` checks it).
        let reused = vm
            .code_cache
            .alloc(a_code.len)
            .expect("A's own freed space must be reusable");
        assert_eq!(reused.base, a_code.base);
    }

    /// `tests_s12.md`'s `flush_id_reuse_safe`: after A is flushed, its own
    /// `NmethodId` slot is reusable by a completely unrelated nmethod
    /// WITHOUT resurrecting any stale reference to the flushed one — proof
    /// that step 4's "slot -> None, id reusable" is actually safe, not
    /// just documented as such.
    #[test]
    fn flush_id_reuse_safe() {
        let mut vm = test_vm();
        let sel = vm.universe.intern(b"target:");
        let klass_a = fake_klass(&mut vm, 0x1000);
        let klass_z = fake_klass(&mut vm, 0x6000);

        let a_code = install_blob(&mut vm, 16);
        let a_id = vm.code_table.install(fake_nmethod(klass_a, sel, a_code));
        flush_nmethod(&mut vm, a_id);

        let z_code = install_blob(&mut vm, 16);
        let z_sel = vm.universe.intern(b"unrelated:");
        let z_id = vm.code_table.install(fake_nmethod(klass_z, z_sel, z_code));

        assert_eq!(
            z_id, a_id,
            "the freed slot must be the one reused (CodeTable::install's own \
             'reuse a freed slot if one exists' doc)"
        );
        let z_nm = vm.code_table.get(z_id).unwrap();
        assert_eq!(z_nm.key_klass.oop().raw(), klass_z.oop().raw());
        assert_eq!(z_nm.code.base, z_code.base);
        assert_eq!(
            vm.code_table.lookup(klass_a, sel),
            None,
            "the flushed (klass_a, sel) key must stay gone -- install re-keys by \
             (klass_z, z_sel), never resurrects the old key"
        );
        assert_eq!(vm.code_table.lookup(klass_z, z_sel), Some(z_id));
    }
}
