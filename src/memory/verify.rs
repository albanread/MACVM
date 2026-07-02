//! The heap verifier: walks a committed region object-by-object (headers
//! make the heap parsable — size comes from the klass's format plus, for
//! indexable formats, the object's own size slot) and checks the
//! invariants genesis and every later allocator must preserve. Used by
//! genesis itself (SPEC §3.2 step 12) and by tests (CONVENTIONS §4).
//!
//! Format-dispatched from the first line: a `Double`'s body is raw bits,
//! not oops, and must never be read as if it were (SPEC §2.3 — the walker
//! is the first heap-scanning code in the tree, so getting this right here
//! sets the pattern S7's GC follows).

use crate::oops::layout::{HEADER_WORDS, MARK_TAG, MEM_TAG, TAG_MASK};
use crate::oops::wrappers::{KlassOop, MemOop};
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;

use super::universe::Universe;

#[derive(Debug, PartialEq, Eq)]
pub struct VerifyError(pub String);

/// `MACVM_DBG_OOP=<addr>` (S7, lesson 11: build this hook EARLY, before
/// the scavenger works). No-op unless `vm.dbg_oop` is set. Logs the traced
/// address's space, mark decode, and containing-card dirty state (once
/// old gen actually holds live objects) to stderr, tagged with `phase` —
/// callers pass a short phase name (`"scavenge-entry"`, `"boot"`, …) at
/// every phase boundary worth tracing. The scavenger (S7-7) additionally
/// calls this after moving the traced object, retargeting `vm.dbg_oop` to
/// its new address first — this function only ever reports on the CURRENT
/// address, it never chases forwarding itself.
pub fn dbg_oop_trace(vm: &VmState, phase: &str) {
    let Some(addr) = vm.dbg_oop else { return };
    let u = &vm.universe;
    let space = if u.layout.eden.contains(addr) {
        "eden"
    } else if u.layout.from.contains(addr) {
        "from"
    } else if u.layout.to.contains(addr) {
        "to"
    } else if u.layout.old.contains(addr) {
        "old"
    } else {
        eprintln!("[dbg_oop {phase}] {addr:#x}: out of the heap reservation entirely");
        return;
    };
    let Some(raw) = (addr as u64).checked_add(MEM_TAG) else {
        eprintln!("[dbg_oop {phase}] {addr:#x}: address too large to tag ({space})");
        return;
    };
    let oop = Oop::from_raw_unchecked(raw); // may be garbage memory; never assert on it here
    let Some(obj) = MemOop::try_from(oop) else {
        eprintln!("[dbg_oop {phase}] {addr:#x}: not a mem-tagged address ({space})");
        return;
    };
    if obj.is_forwarded() {
        eprintln!(
            "[dbg_oop {phase}] {addr:#x} ({space}): forwarded -> {:#x}",
            obj.forwardee().raw()
        );
        return;
    }
    // A traced address inside a committed-but-not-yet-allocated region
    // (e.g. old gen beyond `top`) holds arbitrary/zero-filled bytes, not
    // necessarily a valid mark word — check the tag before decoding
    // rather than going through the panicking `MemOop::mark()`.
    let raw_mark = obj.mark_word_raw();
    if raw_mark & TAG_MASK != MARK_TAG {
        eprintln!(
            "[dbg_oop {phase}] {addr:#x} ({space}): header {raw_mark:#x} is not a valid mark \
             (unallocated or corrupt)"
        );
        return;
    }
    let m = obj.mark();
    eprintln!(
        "[dbg_oop {phase}] {addr:#x} ({space}): age={} hash={} near_death={} tagged_contents={}",
        m.age(),
        m.hash(),
        m.near_death(),
        m.tagged_contents()
    );
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Checks one object's mark/klass invariants (shared by the S1 eden-only
/// `verify_heap` and S7's full `verify_heap_at`) and returns its size in
/// words. Never follows forwarding — a forwarded header failing the mark
/// check IS the point outside an in-progress scavenge (invariant 7).
fn check_object(addr: usize) -> Result<usize, VerifyError> {
    let candidate = Oop::from_raw(addr as u64 + MEM_TAG);
    let obj = MemOop::try_from(candidate)
        .ok_or_else(|| VerifyError(format!("object at {addr:#x} is not mem-tagged")))?;

    let word = obj.mark_word_raw();
    if word & TAG_MASK != MARK_TAG || word & 0b100 == 0 {
        return Err(VerifyError(format!(
            "object at {addr:#x} has a malformed mark word {word:#x} (forwarding pointer \
             outside a scavenge, or corruption)"
        )));
    }

    let klass_oop = obj.klass_oop();
    if !klass_oop.is_mem() {
        return Err(VerifyError(format!(
            "object at {addr:#x} has a non-mem klass field {:#x} (unpatched placeholder?)",
            klass_oop.raw()
        )));
    }
    // `KlassOop::try_from` is itself the correct invariant check here: it
    // succeeds iff `klass_oop`'s OWN klass (this object's metaclass) has
    // format Klass — meaning `klass_oop` is validly klass-shaped. Do NOT
    // additionally assert `klass_oop`'s own `.format()` is `Klass`: that
    // field describes the shape of THIS object's instances (e.g.
    // `Format::Slots` for `undefined_object_klass`, correctly), not of
    // `klass_oop` itself — conflating the two was a real S1 bug caught by
    // this check.
    KlassOop::try_from(klass_oop).ok_or_else(|| {
        VerifyError(format!(
            "object at {addr:#x}'s klass {:#x} is not itself klass-shaped",
            klass_oop.raw()
        ))
    })?;

    let words = obj.instance_size_words();
    if words < HEADER_WORDS {
        return Err(VerifyError(format!(
            "object at {addr:#x} claims {words} words, smaller than a header"
        )));
    }
    Ok(words)
}

/// Walks every object in `universe.eden` from `start` to `top`, checking:
/// every mark word has MARK_TAG + sentinel; every klass word is mem-tagged
/// and points to a validly klass-shaped object (i.e. `KlassOop::try_from`
/// succeeds on it — no klass field anywhere holds a smi, a surviving
/// genesis placeholder); the walk lands exactly on `eden.top` when done (no
/// object claims a size that overruns or undershoots the next header).
pub fn verify_heap(universe: &Universe) -> Result<usize, VerifyError> {
    let mut addr = universe.eden.start;
    let mut count = 0usize;

    while addr < universe.eden.top {
        let words = check_object(addr)?;
        addr += words * crate::oops::layout::WORD_SIZE;
        count += 1;
    }

    if addr != universe.eden.top {
        return Err(VerifyError(format!(
            "heap walk ended at {addr:#x}, expected exactly eden.top {:#x}",
            universe.eden.top
        )));
    }

    Ok(count)
}

/// Phase boundaries `verify_heap_at` is called from (SPEC §12.3-12.4 A9).
/// `Manual` is for ad-hoc calls (tests, a future `Smalltalk verifyHeap`
/// dev hook) not tied to a specific collector phase.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum VerifyPoint {
    ScavengeEntry,
    ScavengeExit,
    FullGcEntry,
    FullGcExit,
    Manual,
}

impl std::fmt::Display for VerifyPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            VerifyPoint::ScavengeEntry => "scavenge-entry",
            VerifyPoint::ScavengeExit => "scavenge-exit",
            VerifyPoint::FullGcEntry => "full-gc-entry",
            VerifyPoint::FullGcExit => "full-gc-exit",
            VerifyPoint::Manual => "manual",
        };
        write!(f, "{s}")
    }
}

/// `true` when `verify_heap_at` should actually run: always in debug
/// builds, or when `MACVM_GC_VERIFY=1` overrides a release build.
pub fn verify_enabled() -> bool {
    cfg!(debug_assertions) || std::env::var("MACVM_GC_VERIFY").as_deref() == Ok("1")
}

/// A9: the full cross-check verifier (SPEC §12.3-12.4). Walks eden, the
/// live survivor (`from`), and old gen `[start, top)`; checks every
/// object's mark/klass invariants (`check_object`, shared with the S1
/// eden-only `verify_heap`) plus — the part that actually catches GC bugs
/// (lesson 8: bugs live in disagreements between separately-correct
/// structures) — that every body oop is a smi or a mem oop into a
/// COMMITTED, LIVE range (never into `to`-space, which is empty/being
/// built mid-scavenge and must never be pointed at from a supposedly
/// consistent heap at a phase boundary); that every old-gen slot holding a
/// new-gen oop lies in a dirty card; and that every process-stack slot is
/// a valid oop or smi.
pub fn verify_heap_at(vm: &VmState, at: VerifyPoint) -> Result<(), VerifyError> {
    verify_range(vm, vm.universe.eden.start, vm.universe.eden.top, at)?;
    verify_range(vm, vm.universe.from.start, vm.universe.from.top, at)?;
    verify_range(vm, vm.universe.old.bounds.start, vm.universe.old.top, at)?;
    verify_cards_vs_heap(vm, at)?;
    verify_process_stack(vm, at)?;
    Ok(())
}

/// Single-object entry point (`verify_object` per the sprint doc) — the
/// same checks `verify_range` runs per object, usable standalone (a
/// future dev hook, or a test isolating one suspect object).
pub fn verify_object(vm: &VmState, obj: MemOop) -> Result<(), VerifyError> {
    check_object(obj.addr())?;
    check_body_oops(vm, obj, VerifyPoint::Manual)?;
    Ok(())
}

fn verify_range(
    vm: &VmState,
    start: usize,
    top: usize,
    at: VerifyPoint,
) -> Result<(), VerifyError> {
    let mut addr = start;
    while addr < top {
        let words = check_object(addr)?;
        let candidate = Oop::from_raw(addr as u64 + MEM_TAG);
        let obj = MemOop::try_from(candidate).expect("already validated by check_object");
        check_body_oops(vm, obj, at)?;
        addr += words * crate::oops::layout::WORD_SIZE;
    }
    if addr != top {
        return Err(VerifyError(format!(
            "[{at}] heap walk ended at {addr:#x}, expected exactly {top:#x}"
        )));
    }
    Ok(())
}

/// A slot is valid iff it's a smi, or a mem oop into a currently-live,
/// committed range: eden `[start, top)`, the live survivor `from [start,
/// top)`, or old gen `[start, top)` — NEVER into `to`-space (empty/being
/// built) or beyond any space's `top`.
fn valid_slot(vm: &VmState, v: Oop) -> bool {
    if !v.is_mem() {
        return true; // smi (or, in debug, would already have tripped is_smi's own tag checks)
    }
    let addr = v.mem_addr();
    let u = &vm.universe;
    (addr >= u.eden.start && addr < u.eden.top)
        || (addr >= u.from.start && addr < u.from.top)
        || (addr >= u.old.bounds.start && addr < u.old.top)
}

fn check_body_oops(vm: &VmState, obj: MemOop, at: VerifyPoint) -> Result<(), VerifyError> {
    use crate::oops::klass::Format;
    let klass = obj.klass();
    let nis = klass.non_indexable_size();
    let named = nis - HEADER_WORDS;
    let check_one = |v: Oop, where_: &str| -> Result<(), VerifyError> {
        if !valid_slot(vm, v) {
            return Err(VerifyError(format!(
                "[{at}] object at {:#x} {where_} holds {:#x}, not a smi or a live-range oop",
                obj.addr(),
                v.raw()
            )));
        }
        Ok(())
    };
    match klass.format() {
        Format::Slots | Format::Klass => {
            for i in 0..named {
                check_one(obj.body_oop(i), "named field")?;
            }
        }
        Format::Double | Format::Process => {}
        Format::IndexableOops | Format::Closure | Format::Context => {
            for i in 0..named {
                check_one(obj.body_oop(i), "named field")?;
            }
            for i in 0..obj.indexable_len() {
                check_one(obj.tail_oop_at(i), "tail element")?;
            }
        }
        Format::IndexableBytes | Format::Method => {
            for i in 0..named {
                check_one(obj.body_oop(i), "named field")?;
            }
        }
    }
    Ok(())
}

/// Cross-check (a): every old-gen slot holding a new-gen oop lies in a
/// dirty card — the remembered set must be COMPLETE (it may be
/// conservatively over-dirty, never under-dirty).
fn verify_cards_vs_heap(vm: &VmState, at: VerifyPoint) -> Result<(), VerifyError> {
    use crate::oops::klass::Format;
    let u = &vm.universe;
    let mut addr = u.old.bounds.start;
    while addr < u.old.top {
        let words = check_object(addr)?;
        let candidate = Oop::from_raw(addr as u64 + MEM_TAG);
        let obj = MemOop::try_from(candidate).expect("already validated");
        let klass = obj.klass();
        let nis = klass.non_indexable_size();
        let named = nis - HEADER_WORDS;
        let check_slot = |slot_addr: usize, v: Oop| -> Result<(), VerifyError> {
            if v.is_mem() && u.layout.is_new(v.mem_addr()) {
                let card = u.cards.card_index(slot_addr);
                if !u.cards.is_dirty(card) {
                    return Err(VerifyError(format!(
                        "[{at}] old slot {slot_addr:#x} holds new-gen oop {:#x} but card {card} \
                         is clean",
                        v.raw()
                    )));
                }
            }
            Ok(())
        };
        match klass.format() {
            Format::Slots | Format::Klass => {
                for i in 0..named {
                    check_slot(obj.body_addr(i), obj.body_oop(i))?;
                }
            }
            Format::Double | Format::Process => {}
            Format::IndexableOops | Format::Closure | Format::Context => {
                for i in 0..named {
                    check_slot(obj.body_addr(i), obj.body_oop(i))?;
                }
                let tail0 = obj.tail_start_word();
                for i in 0..obj.indexable_len() {
                    check_slot(obj.body_addr(tail0 + i), obj.tail_oop_at(i))?;
                }
            }
            Format::IndexableBytes | Format::Method => {
                for i in 0..named {
                    check_slot(obj.body_addr(i), obj.body_oop(i))?;
                }
            }
        }
        addr += words * crate::oops::layout::WORD_SIZE;
    }
    Ok(())
}

/// Cross-check (d): every process-stack slot `0..sp` is a valid oop or
/// smi (SPEC §5.1's exact-stack invariant, invariant 12).
fn verify_process_stack(vm: &VmState, at: VerifyPoint) -> Result<(), VerifyError> {
    for i in 0..vm.stack.sp {
        let v = vm.stack.get(i);
        if !valid_slot(vm, v) {
            return Err(VerifyError(format!(
                "[{at}] process stack slot {i} holds {:#x}, not a smi or a live-range oop",
                v.raw()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::vm_state::VmOptions;

    #[test]
    fn verify_heap_post_genesis() {
        let u = Universe::genesis(&VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            eden_kb: None,
        });
        let count = verify_heap(&u).expect("post-genesis heap must verify");
        assert!(count > 0);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn verify_heap_detects_corruption() {
        let mut u = Universe::genesis(&VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            eden_kb: None,
        });
        // Corrupt object_klass's own klass field ("Object class"'s klass,
        // normally metaclass_klass) to a smi placeholder via the raw
        // (non-panicking) write path — never through `set_klass`, which
        // demands an actual `KlassOop`.
        let object_meta = u.object_klass.klass();
        object_meta
            .as_mem()
            .set_klass_raw(crate::oops::smi::SmallInt::new(0).oop());
        let result = verify_heap(&u);
        assert!(result.is_err());
        let _ = &mut u; // keep `u` mutably-bindable for symmetry with other tests
    }

    /// `dbg_oop_trace` must never panic, whether unset, pointed at a real
    /// object, or pointed at garbage — it's a diagnostic hook, not
    /// something that should ever be able to crash a run.
    #[test]
    fn dbg_oop_trace_never_panics() {
        let mut vm = crate::runtime::vm_state::VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            eden_kb: None,
        });
        dbg_oop_trace(&vm, "unset"); // vm.dbg_oop is None: no-op

        vm.dbg_oop = Some(vm.universe.nil_obj.mem_addr());
        dbg_oop_trace(&vm, "real-object");

        vm.dbg_oop = Some(vm.universe.layout.old.start + 4096); // unallocated old-gen address
        dbg_oop_trace(&vm, "unallocated");

        vm.dbg_oop = Some(usize::MAX - 8); // out of heap entirely
        dbg_oop_trace(&vm, "out-of-heap");
    }

    /// `verify_heap_at` on a freshly booted VM (before any scavenge has
    /// run) must succeed.
    #[test]
    fn verify_heap_at_fresh_boot() {
        let vm = crate::runtime::vm_state::VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            eden_kb: None,
        });
        verify_heap_at(&vm, VerifyPoint::Manual).expect("fresh boot must verify");
    }

    /// A guard against a vacuously-green verifier (mirrors
    /// `verify_heap_detects_corruption` for the S1 walker): a manually
    /// corrupted klass field must be CAUGHT, not silently accepted.
    #[test]
    #[cfg(debug_assertions)]
    fn verify_heap_at_detects_corruption() {
        let vm = crate::runtime::vm_state::VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            eden_kb: None,
        });
        let object_meta = vm.universe.object_klass.klass();
        object_meta
            .as_mem()
            .set_klass_raw(crate::oops::smi::SmallInt::new(0).oop());
        let result = verify_heap_at(&vm, VerifyPoint::Manual);
        assert!(result.is_err());
    }

    /// Cross-check (a) must catch a deliberately-cleared card hiding a
    /// real old->new reference — the exact class of bug lesson 8 warns
    /// about (two separately-correct structures disagreeing).
    #[test]
    fn verify_heap_at_detects_missing_dirty_card() {
        let mut vm = crate::runtime::vm_state::VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            eden_kb: None,
        });
        vm.universe.tenuring_threshold = 0; // promote immediately
        let klass = vm.universe.array_klass;
        let holder = crate::memory::alloc::alloc_indexable_oops(&mut vm, klass, 1);
        vm.stack.push(holder.oop());
        crate::memory::scavenge::scavenge(&mut vm).expect("scavenge must succeed");
        let holder_oop = vm.stack.get(vm.stack.sp - 1);
        let holder_mem = MemOop::try_from(holder_oop).unwrap();
        assert!(
            vm.universe.layout.is_old(holder_mem.addr()),
            "holder must have promoted"
        );

        // A genuine mutator write (through the real store choke point, not
        // the scavenger's own copy) of a fresh new-gen value into the
        // now-old holder — exactly the scenario the write barrier exists
        // for. This must correctly dirty holder's card.
        let referenced = crate::memory::alloc::alloc_indexable_oops(&mut vm, klass, 0);
        let tail0 = holder_mem.tail_start_word();
        crate::memory::store::store(&mut vm, holder_mem, tail0, referenced.oop());

        // The holder (old) now references something new-gen and its card
        // must be dirty — verify passes.
        verify_heap_at(&vm, VerifyPoint::Manual).expect("must verify before corruption");

        // Deliberately clear every card — the remembered set is now
        // incomplete; verify must catch it.
        for i in 0..vm.universe.cards.n_cards() {
            vm.universe.cards.set_clean(i);
        }
        let result = verify_heap_at(&vm, VerifyPoint::Manual);
        assert!(
            result.is_err(),
            "clearing all cards must be caught by the card-vs-heap cross-check"
        );
    }
}
