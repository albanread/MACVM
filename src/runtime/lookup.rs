//! Hierarchy method lookup and the `LookupCache` (SPEC §6.1). `lookup()` is
//! the single entry point every send path (`interpreter::send`, DNU's
//! `#doesNotUnderstand:` resolution) goes through; it is oblivious to
//! frames/`InterpRegs` — callers resolve the starting klass (receiver's own
//! klass for a normal send, `method.holder().superclass()` for a super
//! send) before calling in.

use crate::oops::method_dict::MethodDictOop;
use crate::oops::wrappers::{KlassOop, MemOop, MethodOop, SymbolOop};
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;

/// `klass_of` (SPEC §5.3 step 3): smis have no header, so they resolve to
/// the fixed `smi_klass`; every other tag dereferences the object's own
/// klass field. Tag-checks first — the Pitfalls section is explicit that
/// this must never dereference a smi as if it had a header.
pub fn klass_of(vm: &VmState, o: Oop) -> KlassOop {
    if o.is_smi() {
        vm.universe.smi_klass
    } else {
        MemOop::try_from(o)
            .expect("klass_of: oop is neither a smi nor a mem oop")
            .klass()
    }
}

/// Cache slot count *(tunable)* — 2 entries (primary + victim) per index.
pub const LOOKUP_CACHE_SIZE: usize = 4096;

#[derive(Copy, Clone)]
struct CacheEntry {
    klass: Oop,
    selector: Oop,
    method: Oop,
}

impl CacheEntry {
    const EMPTY: CacheEntry = CacheEntry {
        klass: Oop::from_raw_unchecked(0),
        selector: Oop::from_raw_unchecked(0),
        method: Oop::from_raw_unchecked(0),
    };

    fn is_empty(&self) -> bool {
        self.klass.raw() == 0
    }
}

/// SPEC §6.1: a 2-way (primary/victim) address-keyed cache from
/// `(receiver klass, selector)` to the resolved `MethodOop`. Address-keyed
/// means a moving GC invalidates every entry — see `gc_epilogue`.
pub struct LookupCache {
    entries: Box<[CacheEntry]>,
}

impl LookupCache {
    pub fn new() -> LookupCache {
        LookupCache {
            entries: vec![CacheEntry::EMPTY; 2 * LOOKUP_CACHE_SIZE].into_boxed_slice(),
        }
    }

    fn hash(klass: KlassOop, selector: SymbolOop) -> usize {
        (((klass.oop().raw() >> 3) ^ (selector.oop().raw() >> 3)) as usize)
            & (LOOKUP_CACHE_SIZE - 1)
    }

    /// Checks the primary slot, then the victim slot. A victim hit is
    /// promoted to primary (swapped) before returning — the "2-way victim"
    /// design keeps a recently-demoted entry alive one probe longer.
    pub fn probe(&mut self, klass: KlassOop, selector: SymbolOop) -> Option<MethodOop> {
        let h = Self::hash(klass, selector);
        let (primary, victim) = (2 * h, 2 * h + 1);

        let p = self.entries[primary];
        if !p.is_empty()
            && p.klass.raw() == klass.oop().raw()
            && p.selector.raw() == selector.oop().raw()
        {
            return MethodOop::try_from(p.method);
        }
        let v = self.entries[victim];
        if !v.is_empty()
            && v.klass.raw() == klass.oop().raw()
            && v.selector.raw() == selector.oop().raw()
        {
            self.entries.swap(primary, victim);
            return MethodOop::try_from(v.method);
        }
        None
    }

    /// Demotes the current primary into the victim slot, writes the new
    /// entry as primary.
    pub fn insert(&mut self, klass: KlassOop, selector: SymbolOop, method: MethodOop) {
        let h = Self::hash(klass, selector);
        let (primary, victim) = (2 * h, 2 * h + 1);
        self.entries[victim] = self.entries[primary];
        self.entries[primary] = CacheEntry {
            klass: klass.oop(),
            selector: selector.oop(),
            method: method.oop(),
        };
    }

    pub fn flush(&mut self) {
        for e in self.entries.iter_mut() {
            *e = CacheEntry::EMPTY;
        }
    }
}

impl Default for LookupCache {
    fn default() -> Self {
        Self::new()
    }
}

/// SPEC §6.1: cache probe, then a superclass-chain walk of
/// `klass.methods()` (a `MethodDictionary`, or `nil` for a klass with no
/// methods installed yet — `MethodDictOop::try_from` returning `None`
/// transparently skips those levels). On a walk hit, caches the result
/// keyed by the ORIGINAL receiver klass (not the defining klass) — that is
/// Strongtalk's LookupKey shape, and what makes an inherited method's cache
/// entry correct for every subclass that inherits it. No negative caching:
/// a miss walks the full chain every time.
pub fn lookup(vm: &mut VmState, klass: KlassOop, selector: SymbolOop) -> Option<MethodOop> {
    if let Some(m) = vm.lookup_cache.probe(klass, selector) {
        return Some(m);
    }

    let nil = vm.universe.nil_obj;
    let mut k = klass;
    loop {
        if let Some(dict) = MethodDictOop::try_from(k.methods()) {
            if let Some(m) = dict.probe(vm, selector) {
                vm.lookup_cache.insert(klass, selector, m);
                return Some(m);
            }
        }
        let sc = k.superclass();
        if sc.raw() == nil.raw() {
            return None;
        }
        k = KlassOop::try_from(sc).expect("lookup: superclass field is not a klass");
    }
}

/// Installs `m` under `selector` in `klass`'s `MethodDictionary`, allocating
/// one on first install (SPEC §2.4, S3). Wires all three lazy-flush
/// triggers (SPEC §6.2): the lookup cache and every IC self-heal on their
/// next use via the bumped epoch.
pub fn install_method(vm: &mut VmState, klass: KlassOop, selector: SymbolOop, m: MethodOop) {
    // `klass`/`selector`/`m` are bare parameters held across `dict.insert`'s
    // possible growth allocation below — S7-9/S7-10 (found via
    // `MACVM_GC_STRESS=1`).
    let scope = crate::memory::handles::HandleScope::enter(vm);
    let klass_h = scope.handle(vm, klass);
    let selector_h = scope.handle(vm, selector);
    let m_h = scope.handle(vm, m);

    let dict = match MethodDictOop::try_from(klass.methods()) {
        Some(d) => d,
        None => crate::oops::method_dict::alloc_method_dict(vm, 8),
    };
    let dict = dict.insert(vm, selector_h.get(vm), m_h.get(vm));
    let klass = klass_h.get(vm);
    let m = m_h.get(vm);
    klass.set_methods(dict.oop());
    m.set_holder(klass.oop());
    patch_block_holders(klass.oop(), m);
    // `klass` is typically an early-promoted (old-gen) genesis klass and
    // `dict` a young object — the raw setters above bypass the per-slot
    // store barrier, so the old→new edge must be recorded here (S7-10).
    // `m` is young at install time (freshly compiled), but the barrier is
    // an old-check no-op then, so cover it uniformly.
    crate::memory::store::post_write_barrier(vm, klass.as_mem());
    crate::memory::store::post_write_barrier(vm, m.as_mem());

    vm.lookup_cache.flush();
    vm.ic_epoch += 1;
    debug_assert!(
        vm.ic_epoch < (1 << crate::oops::layout::IC_META_EPOCH_BITS),
        "install_method: ic_epoch wrapped past 24 bits"
    );
}

/// A `CompiledBlock` literal has no holder of its own at build time (its
/// enclosing method isn't installed yet) — `install_method` patches every
/// block transitively reachable through `m`'s literal pool (blocks can
/// nest) so that a super send *inside* a block resolves against the right
/// holder (`interpreter::ic::resolve` reads `caller.holder()`, and `caller`
/// is the executing `CompiledBlock` itself for a send inside a block).
fn patch_block_holders(klass_oop: Oop, m: MethodOop) {
    let literals = m.literals();
    for i in 0..literals.len() {
        if let Some(blk) = MethodOop::try_from(literals.at(i)) {
            if blk.is_block() {
                blk.set_holder(klass_oop);
                patch_block_holders(klass_oop, blk);
            }
        }
    }
}

/// S7/S8's full-GC hook (Pitfalls: "LookupCache is address-keyed"). A moving
/// collection invalidates every entry; adding this now, unused until S8,
/// means the flush-on-GC contract can never be forgotten later.
pub fn gc_epilogue(vm: &mut VmState) {
    vm.lookup_cache.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oops::layout::HEADER_WORDS;
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

    fn trivial_method(vm: &mut VmState, name: &[u8]) -> MethodOop {
        let mut b = crate::bytecode::BytecodeBuilder::new();
        b.ret_self();
        let sel = vm.universe.intern(name);
        b.finish(vm, sel, 0, 0)
    }

    fn three_deep_hierarchy(vm: &mut VmState) -> (KlassOop, KlassOop, KlassOop) {
        let object_klass = vm.universe.object_klass;
        let root = vm.universe.new_klass(
            object_klass,
            "Root",
            crate::oops::Format::Slots,
            false,
            HEADER_WORDS,
        );
        let mid =
            vm.universe
                .new_klass(root, "Mid", crate::oops::Format::Slots, false, HEADER_WORDS);
        let leaf =
            vm.universe
                .new_klass(mid, "Leaf", crate::oops::Format::Slots, false, HEADER_WORDS);
        (root, mid, leaf)
    }

    #[test]
    fn lookup_walks_chain() {
        let mut vm = test_vm();
        let (root, _mid, leaf) = three_deep_hierarchy(&mut vm);
        let sel = vm.universe.intern(b"foo");
        let m = trivial_method(&mut vm, b"foo");
        install_method(&mut vm, root, sel, m);

        assert_eq!(lookup(&mut vm, leaf, sel), Some(m));
    }

    #[test]
    fn lookup_shadowing() {
        let mut vm = test_vm();
        let (root, mid, leaf) = three_deep_hierarchy(&mut vm);
        let sel = vm.universe.intern(b"foo");
        let m_root = trivial_method(&mut vm, b"foo_root");
        let m_mid = trivial_method(&mut vm, b"foo_mid");
        install_method(&mut vm, root, sel, m_root);
        install_method(&mut vm, mid, sel, m_mid);

        assert_eq!(lookup(&mut vm, leaf, sel), Some(m_mid));
    }

    #[test]
    fn lookup_miss() {
        let mut vm = test_vm();
        let (root, mid, leaf) = three_deep_hierarchy(&mut vm);
        let sel = vm.universe.intern(b"absent");

        assert_eq!(lookup(&mut vm, root, sel), None);
        assert_eq!(lookup(&mut vm, mid, sel), None);
        assert_eq!(lookup(&mut vm, leaf, sel), None);
    }

    #[test]
    fn cache_probe_insert() {
        let mut vm = test_vm();
        let (root, _mid, leaf) = three_deep_hierarchy(&mut vm);
        let sel = vm.universe.intern(b"foo");
        let other_sel = vm.universe.intern(b"bar");
        let m = trivial_method(&mut vm, b"foo");
        vm.lookup_cache.insert(leaf, sel, m);

        assert_eq!(vm.lookup_cache.probe(leaf, sel), Some(m));
        assert_eq!(vm.lookup_cache.probe(leaf, other_sel), None);
        assert_eq!(vm.lookup_cache.probe(root, sel), None);
    }

    #[test]
    fn cache_victim_promote() {
        let mut vm = test_vm();
        let (root, mid, _leaf) = three_deep_hierarchy(&mut vm);

        // Two distinct (klass, selector) pairs forced to collide at the
        // same cache slot: `LookupCache::hash` depends only on the low
        // `log2(LOOKUP_CACHE_SIZE)` bits of each oop's raw address (never
        // dereferenced), so construct a selector oop whose raw bits solve
        // `hash(mid, sel_b) == hash(root, sel_a)` directly — hunting for a
        // real interned symbol at a colliding address is not reliable
        // (allocation addresses advance by a fixed stride, so a brute-force
        // search over sequential names can systematically miss every
        // residue class the target hash falls in).
        let sel_a = vm.universe.intern(b"a");
        let m_a = trivial_method(&mut vm, b"a");
        let h_a = LookupCache::hash(root, sel_a);

        let mask = (LOOKUP_CACHE_SIZE - 1) as u64;
        let target_low = (h_a as u64) ^ ((mid.oop().raw() >> 3) & mask);
        let sel_b_x = (0x9999_0000u64 & !mask) | target_low;
        let sel_b = crate::oops::wrappers::fake_symbol_for_test(sel_b_x);
        assert_eq!(LookupCache::hash(mid, sel_b), h_a);
        let m_b = trivial_method(&mut vm, b"b");

        vm.lookup_cache.insert(root, sel_a, m_a);
        vm.lookup_cache.insert(mid, sel_b, m_b); // demotes (root,sel_a) to victim

        // Both entries stay retrievable through victim promotion — every
        // probe of the currently-demoted entry swaps it back to primary,
        // demoting whichever was primary before it.
        assert_eq!(vm.lookup_cache.probe(root, sel_a), Some(m_a)); // victim hit: (root,sel_a) <-> primary
        assert_eq!(vm.lookup_cache.probe(mid, sel_b), Some(m_b)); // victim hit again: swaps back

        // The most recent probe (of (mid,sel_b), now the victim before that
        // call) promoted it to primary — verify the swap actually happened
        // by checking raw slot contents.
        let primary = vm.lookup_cache.entries[2 * h_a];
        assert_eq!(primary.klass.raw(), mid.oop().raw());
        assert_eq!(primary.selector.raw(), sel_b.oop().raw());
    }

    #[test]
    fn cache_flush() {
        let mut vm = test_vm();
        let (root, _mid, leaf) = three_deep_hierarchy(&mut vm);
        let sel = vm.universe.intern(b"foo");
        let m = trivial_method(&mut vm, b"foo");
        install_method(&mut vm, root, sel, m);

        assert_eq!(lookup(&mut vm, leaf, sel), Some(m));
        assert!(vm.lookup_cache.probe(leaf, sel).is_some());

        vm.lookup_cache.flush();
        assert!(vm.lookup_cache.probe(leaf, sel).is_none());
        // The walk still succeeds and refills the cache.
        assert_eq!(lookup(&mut vm, leaf, sel), Some(m));
        assert!(vm.lookup_cache.probe(leaf, sel).is_some());
    }

    #[test]
    fn cache_keyed_by_receiver_klass() {
        let mut vm = test_vm();
        let (root, _mid, leaf) = three_deep_hierarchy(&mut vm);
        let sel = vm.universe.intern(b"foo");
        let m = trivial_method(&mut vm, b"foo");
        install_method(&mut vm, root, sel, m);

        // Looked up starting from `leaf` (the receiver klass), found on
        // `root` (the defining klass) — the cache entry must be keyed by
        // `leaf`, not `root`.
        assert_eq!(lookup(&mut vm, leaf, sel), Some(m));
        assert!(vm.lookup_cache.probe(leaf, sel).is_some());
    }

    #[test]
    fn install_flushes_cache_and_bumps_epoch() {
        let mut vm = test_vm();
        let (root, _mid, leaf) = three_deep_hierarchy(&mut vm);
        let sel = vm.universe.intern(b"foo");
        let m1 = trivial_method(&mut vm, b"foo1");
        install_method(&mut vm, root, sel, m1);
        assert_eq!(lookup(&mut vm, leaf, sel), Some(m1));
        assert!(vm.lookup_cache.probe(leaf, sel).is_some());

        let epoch_before = vm.ic_epoch;
        let m2 = trivial_method(&mut vm, b"foo2");
        install_method(&mut vm, root, sel, m2);

        assert!(vm.lookup_cache.probe(leaf, sel).is_none());
        assert_eq!(vm.ic_epoch, epoch_before + 1);
        assert_eq!(lookup(&mut vm, leaf, sel), Some(m2));
    }
}
