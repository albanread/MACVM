//! `Nmethod`/`CodeTable` — the tier-1 compiled-method store
//! (`sprint_s10_detail.md` D2, D8). `CodeTable::oops_do`/`update_keys` are
//! this module's half of D8's GC integration; the other half (calling
//! them at the right point in each collector, under the right transform)
//! lives in `memory::scavenge`/`memory::fullgc` themselves, matching how
//! `runtime::lookup::gc_epilogue` (the closest existing precedent for a
//! Rust-side address-keyed cache that dangles across a moving GC) is
//! called FROM the collectors rather than the collectors being taught
//! about `LookupCache` internals.

use std::collections::HashMap;

use smallvec::SmallVec;

use crate::codecache::guard::JitWriteGuard;
use crate::codecache::CodeHandle;
use crate::compiler::assembler::{Reloc, RelocKind};
use crate::oops::wrappers::{KlassOop, SymbolOop};
use crate::oops::Oop;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NmethodId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NmState {
    Alive,
    /// S12/S13 use these — no S10 nmethod is ever constructed in either.
    NotEntrant,
    Zombie,
}

/// Which spill slots hold a LIVE oop at one specific safepoint (S12 D2) —
/// a packed bitmap over spill-slot indices, one bit per slot (`bit i set ⇔
/// frame slot i, at `[fp − 8·(i+1)]`, holds a live oop at the safepoint(s)
/// this map is attached to via `PcDesc.oopmap`). `SmallVec<[u64; 1]>`
/// (matching the project's existing `smallvec` use, `codecache::guard`'s
/// own precedent): `frame_slots <= 60` (S10/S11 eligibility's own frame
/// budget) means `ceil(60/64) == 1` word in every real case, so the inline
/// capacity avoids a heap allocation on the common path. Built by
/// `compiler::oopmap::build_for_position` (liveness ∩ is_oop at one exact
/// program position — NOT "every slot this method ever uses for an oop
/// anywhere", which is `Nmethod::slot_is_oop`'s job instead); multiple
/// safepoints with identical live sets share one entry in `Nmethod::
/// oopmaps` (deduplicated by content, D2).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OopMap {
    bits: SmallVec<[u64; 1]>,
}

impl OopMap {
    /// An empty map (every bit clear) — `oopmaps[0]`'s reserved slot for
    /// PcDescs that don't correspond to a real safepoint (S10's block-start
    /// descs, kept for `bci_at`'s trace-path lookup) and never consulted by
    /// the GC path (`oopmap_at` only ever reaches a REAL safepoint's own
    /// entry, D1: every compiled frame at GC time is suspended at one, by
    /// construction).
    pub fn empty() -> OopMap {
        OopMap {
            bits: SmallVec::new(),
        }
    }

    pub fn set(&mut self, slot: u16) {
        let word = slot as usize / 64;
        if word >= self.bits.len() {
            self.bits.resize(word + 1, 0);
        }
        self.bits[word] |= 1u64 << (slot as usize % 64);
    }

    pub fn is_oop(&self, slot: u16) -> bool {
        let word = slot as usize / 64;
        match self.bits.get(word) {
            Some(w) => (w >> (slot as usize % 64)) & 1 != 0,
            None => false,
        }
    }

    /// Every set bit's slot index, ascending — the GC root-scan's own
    /// iteration order (`each_code_root`, S12 D4.1); order doesn't matter
    /// for correctness, only for determinism (tests pin exact sequences).
    pub fn iter_slots(&self) -> impl Iterator<Item = u16> + '_ {
        self.bits.iter().enumerate().flat_map(|(w, &word)| {
            (0..64u16)
                .filter(move |&b| (word >> b) & 1 != 0)
                .map(move |b| (w * 64) as u16 + b)
        })
    }
}

/// One block-start (S10, trace-path only) or call-return-address (S11+,
/// GC safepoint) position, mapped back to a bytecode index for mixed-tier
/// stack traces (D3.6) and — for the latter kind only — an `oopmap` index
/// (`Nmethod::oopmaps`) the GC root-scan reads via `Nmethod::oopmap_at`'s
/// EXACT match. A block-start desc's own `oopmap` is always `0`
/// (`OopMap::empty()`, reserved — see that constructor's own doc): it
/// exists only so `bci_at`'s nearest-below trace lookup has full pc
/// coverage, never a real safepoint's own return address (S11's send/
/// alloc-slow sites always emit at least one more instruction — the NLR
/// check, or the fast-path merge — before any new block could start, so
/// the two kinds of entry never collide in practice; even if they did,
/// `oopmap_at`'s caller only ever passes a genuine suspended-frame return
/// address, D1).
#[derive(Clone, Copy, Debug)]
pub struct PcDesc {
    pub pc_off: u32,
    pub bci: usize,
    pub oopmap: u16,
}

/// A compiled call site's own IC lattice state (S11 D3/D4) — mirrors the
/// interpreter IC's mono→poly→mega shape (SPEC §5.3) but lives entirely on
/// the Rust side (compiled code itself only ever sees "call whatever the
/// patched `bl` currently targets"). `Mono` records the klass/target pair
/// it was resolved for — D4.1's own state table needs the OLD pair on a
/// later "different klass" resolve, to seed a fresh PIC alongside the new
/// one. `Pic`/`Mega` carry only the generated stub's own handle — NOT a
/// pair count or the pairs themselves: `codecache::pics::PicTable`/
/// `codecache::mega::MegaTable` are the single source of truth for both
/// (keyed by the stub's own handle/selector respectively), so there is
/// nothing here that could drift out of sync with them. A later
/// transition (grow, or promote to mega) reads the OLD stub's own pairs
/// from there, then frees it there too (P1/P2: rebuild-and-swing, never
/// an in-place edit).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IcState {
    Unresolved,
    Mono { klass: KlassOop, target: u64 },
    Pic { stub: CodeHandle },
    Mega { stub: CodeHandle },
}

/// A compiled inline-cache call site (S11 D3): one per `Ir::CallSend` in a
/// compiled method's body. `off` is the `bl` instruction's own byte offset
/// within `Nmethod::code` (matches `Reloc::offset`'s convention for
/// `InlineCache` relocs — `bl_patchable`'s own doc). `selector`/`argc`
/// identify the send; `state` starts `Unresolved` at publish (every fresh
/// site's `bl` targets `stub_resolve`, D3) and evolves as real receivers
/// arrive (D4.1's state machine).
#[derive(Clone, Copy, Debug)]
pub struct IcSite {
    pub off: u32,
    pub selector: SymbolOop,
    pub argc: u8,
    pub state: IcState,
}

pub struct Nmethod {
    pub id: NmethodId,
    /// Customization key — the receiver klass this nmethod was compiled
    /// for. Strong until S12 (D8's own SPEC-QUESTION: weak treatment is
    /// S12's job; S10 just needs the mechanism in place).
    pub key_klass: KlassOop,
    pub key_selector: SymbolOop,
    pub code: CodeHandle,
    /// `== verified_entry_off` until S11 gives a customized nmethod a
    /// separate unverified entry.
    pub entry_off: u32,
    pub verified_entry_off: u32,
    pub state: NmState,
    pub level: u8,
    pub version: u8,
    pub literal_off: u32,
    /// Blob-relative, exactly as `CodeBlob` produced them — add
    /// `code.base` for an absolute address (matches S9's own convention).
    pub relocs: Vec<Reloc>,
    pub frame_slots: u16,
    /// S12 D2's independent ground truth, one bool per spill slot: could
    /// THIS slot EVER hold an oop, anywhere in the method (copied verbatim
    /// from `regalloc::RegallocResult::slot_is_oop` at compile time — slots
    /// are allocated monotonically, one per interval, never reused across
    /// intervals of different `is_oop`-ness, D3.5 point 3, so this is a
    /// per-slot, not per-safepoint, fact). `compiler::oopmap::verify`'s own
    /// cross-check against every `oopmaps[i]`'s bits — independent of
    /// `oopmaps` itself, so a builder bug that sets a bit for a slot that's
    /// never actually oop-typed is still caught, not just self-consistent
    /// by construction.
    pub slot_is_oop: Vec<bool>,
    pub pcdescs: Vec<PcDesc>,
    pub oopmaps: Vec<OopMap>,
    pub ic_sites: Vec<IcSite>,
    /// The bci of the block containing this method's `Ir::Poll`, if any
    /// (D5.3: at most one loop back-edge site matters for S10 — a method
    /// with none has no loop at all). Block-bci granularity, matching
    /// `pcdescs`' own precision — a mixed-tier stack-trace walker
    /// (`runtime::error::print_stack_trace`) has no exact native pc to
    /// work from at a poll callback (S11's `last_compiled_pc` anchor isn't
    /// wired up yet), so this is computed once at compile time from the IR
    /// directly instead, purely as Rust-side bookkeeping alongside the
    /// unchanged emitted code.
    pub poll_bci: Option<usize>,
}

impl Nmethod {
    /// Every `Reloc` this nmethod's literal pool actually needs the GC to
    /// visit (`Oop`/`KeyKlassOop`; `RuntimeAddr`/`InternalWord`/
    /// `InlineCache` are never oops).
    fn oop_relocs(&self) -> impl Iterator<Item = &Reloc> {
        self.relocs
            .iter()
            .filter(|r| matches!(r.kind, RelocKind::Oop | RelocKind::KeyKlassOop))
    }

    /// S12 D2/P1: the GC root-scan's own lookup — `ret_pc` is a
    /// COMPILED FRAME's saved return address (absolute; converted to a
    /// blob-relative offset here), i.e. exactly where a safepoint's `bl`
    /// resumes. EXACT match only, never nearest-below: D1's own invariant
    /// is that every compiled frame at GC time is suspended at a real
    /// safepoint's return address, by construction (loop polls never
    /// allocate/GC — S10 D5.6 — so nothing else can be live on the stack
    /// when a collection starts). A miss means that invariant broke
    /// somewhere upstream (a safepoint got emitted without its own
    /// `PcDesc`, or the walker mis-stepped) — panicking here, rather than
    /// silently falling back to the nearest map, is P1's whole point: a
    /// near-miss would trace the WRONG slots as oops, corrupting the heap
    /// three tests later instead of failing loudly at the source.
    pub fn oopmap_at(&self, ret_pc: u64) -> &OopMap {
        let off = (ret_pc - self.code.base as u64) as u32;
        let idx = self
            .pcdescs
            .iter()
            .position(|d| d.pc_off == off)
            .unwrap_or_else(|| {
                panic!(
                    "oopmap_at: nmethod {:?} has no safepoint PcDesc at return-address offset \
                     {off:#x} (ret_pc {ret_pc:#x}) — D1's invariant broke: every compiled frame \
                     live at GC time must be suspended at a recorded safepoint",
                    self.id
                )
            });
        &self.oopmaps[self.pcdescs[idx].oopmap as usize]
    }

    /// Trace path (`runtime::error::print_stack_trace`, mixed-tier stack
    /// traces): nearest-below lookup over ALL descs (block-start AND real
    /// safepoint alike) — any pc inside the method resolves to whichever
    /// desc most recently preceded it, never panics (a trace is best-effort
    /// diagnostics, not a GC-correctness path — P1's exact-match
    /// requirement is `oopmap_at`'s alone). `pcdescs` is sorted by
    /// `pc_off` ascending (both `pcdescs_sorted_by_pc_off` and driver.rs's
    /// own construction maintain this).
    pub fn bci_at(&self, pc: u64) -> usize {
        let off = (pc - self.code.base as u64) as u32;
        let idx = self.pcdescs.partition_point(|d| d.pc_off <= off);
        if idx == 0 {
            self.pcdescs.first().map(|d| d.bci).unwrap_or(0)
        } else {
            self.pcdescs[idx - 1].bci
        }
    }

    /// Test-only, minimal fake — shared by this module's own tests and
    /// `compiler::oopmap`'s (neither cares about `key_klass`/`key_selector`
    /// /`code` beyond their tag-level shape; both DO care about
    /// `frame_slots`/`slot_is_oop`/`oopmaps`, which this takes explicitly).
    #[cfg(test)]
    pub(crate) fn test_fake(
        frame_slots: u16,
        slot_is_oop: Vec<bool>,
        oopmaps: Vec<OopMap>,
    ) -> Nmethod {
        use crate::oops::layout::MEM_TAG;
        use crate::oops::Oop;
        Nmethod {
            id: NmethodId(0),
            // SAFETY: test-only, tag-level shape is never dereferenced by
            // anything `test_fake`'s own callers exercise (same reasoning
            // as this module's own private `fake_klass`/`fake_selector`).
            key_klass: unsafe { KlassOop::from_oop_unchecked(Oop::from_raw(0x1000 + MEM_TAG)) },
            key_selector: unsafe { SymbolOop::from_oop_unchecked(Oop::from_raw(0x2000 + MEM_TAG)) },
            code: CodeHandle {
                base: 0x1000 as *const u8,
                len: 0x1000,
            },
            entry_off: 0,
            verified_entry_off: 0,
            state: NmState::Alive,
            level: 1,
            version: 0,
            literal_off: 0,
            relocs: Vec::new(),
            frame_slots,
            slot_is_oop,
            pcdescs: Vec::new(),
            oopmaps,
            ic_sites: Vec::new(),
            poll_bci: None,
        }
    }
}

#[derive(Default)]
pub struct CodeTable {
    slots: Vec<Option<Nmethod>>,
    by_key: HashMap<(u64, u64), NmethodId>,
    /// `(code base, id)`, sorted by base — `find_by_pc` binary-searches
    /// this; the code cache never moves a published block (S9/S12's own
    /// invariant), so this table is never invalidated by a GC the way
    /// `by_key` is.
    by_addr: Vec<(u64, NmethodId)>,
}

impl CodeTable {
    pub fn new() -> CodeTable {
        CodeTable::default()
    }

    /// Installs `nm`, reusing a freed slot if one exists (S12 flushing
    /// will create them; S10 never does, so this always appends for now —
    /// still correct either way, and means S12 doesn't have to touch this
    /// method at all).
    pub fn install(&mut self, mut nm: Nmethod) -> NmethodId {
        let key = (nm.key_klass.oop().raw(), nm.key_selector.oop().raw());
        let base = nm.code.base as u64;

        let id = match self.slots.iter().position(|s| s.is_none()) {
            Some(idx) => NmethodId(idx as u32),
            None => {
                let idx = self.slots.len();
                self.slots.push(None);
                NmethodId(idx as u32)
            }
        };
        nm.id = id;
        self.slots[id.0 as usize] = Some(nm);
        self.by_key.insert(key, id);

        let pos = self.by_addr.partition_point(|&(b, _)| b < base);
        self.by_addr.insert(pos, (base, id));

        id
    }

    pub fn get(&self, id: NmethodId) -> Option<&Nmethod> {
        self.slots.get(id.0 as usize)?.as_ref()
    }

    /// D4.1: `rt_resolve_send` needs this to patch the CALLER's own
    /// `ic_sites[i].state` after `patch_branch26_at` repoints its `bl` —
    /// `get`'s `&self` can't do that, and the two are never live at once
    /// (every caller reads what it needs from `get`'s borrow into owned
    /// locals before reaching for this one).
    pub fn get_mut(&mut self, id: NmethodId) -> Option<&mut Nmethod> {
        self.slots.get_mut(id.0 as usize)?.as_mut()
    }

    /// Binary-searches `by_addr` for the nmethod whose published range
    /// `[base, base+len)` contains `pc`.
    pub fn find_by_pc(&self, pc: u64) -> Option<NmethodId> {
        let idx = self.by_addr.partition_point(|&(base, _)| base <= pc);
        if idx == 0 {
            return None;
        }
        let (base, id) = self.by_addr[idx - 1];
        let nm = self.get(id)?;
        if pc >= base && pc < base + nm.code.len as u64 {
            Some(id)
        } else {
            None
        }
    }

    pub fn lookup(&self, k: KlassOop, sel: SymbolOop) -> Option<NmethodId> {
        self.by_key.get(&(k.oop().raw(), sel.oop().raw())).copied()
    }

    /// D8: visit every embedded-oop pool word in every nmethod (S10 has
    /// only `Alive` ones), wrapped in one `JitWriteGuard` covering every
    /// nmethod's own range — the guard's flush is harmless even when `f`
    /// leaves a word unchanged (pool words are data, not code, but the
    /// guard doesn't know or care which).
    ///
    /// D8's own text writes this as `code_table.oops_do(cache, &mut f)`,
    /// taking the owning `CodeCache` as a second parameter. That signature
    /// cannot be satisfied at the scavenge.rs call site: `f` there closes
    /// over `&mut VmState` (`scavenge_oop` needs the whole state, not just
    /// a field), and a `cache: &vm.code_cache` argument evaluated in the
    /// same call would borrow `vm` immutably while the closure holds it
    /// mutably — `CodeCache` has no cheap `Default` to `std::mem::take` it
    /// out of the way the way [`CodeTable`] itself is taken out (a real
    /// `Default` would mean a second `mmap` per scavenge, not a placeholder
    /// swap). Dropping the parameter sidesteps the conflict entirely, and
    /// the containment check it would have enabled is available more
    /// precisely from data this method already has: `reloc.offset` against
    /// *this nmethod's own* `code.len`, tighter than "is this address
    /// anywhere in the whole cache" (which would miss a reloc that drifted
    /// into a *neighboring* nmethod's bytes).
    pub fn oops_do(&mut self, f: &mut dyn FnMut(&mut u64)) {
        let mut guard = JitWriteGuard::new();
        for nm in self.slots.iter().flatten() {
            if !matches!(nm.state, NmState::Alive) {
                continue;
            }
            guard.note(nm.code.base, nm.code.len);
            let base = nm.code.base as usize;
            for reloc in nm.oop_relocs() {
                debug_assert!(
                    (reloc.offset as usize) + 8 <= nm.code.len,
                    "oops_do: reloc offset {} + 8 exceeds nmethod {:?}'s own code length {}",
                    reloc.offset,
                    nm.id,
                    nm.code.len
                );
                let addr = (base + reloc.offset as usize) as *mut u64;
                // SAFETY: `addr` is `code.base + reloc.offset`, and
                // `CodeBlob`'s own contract (S9) guarantees every `Reloc`
                // offset lands inside `[0, code.len)` of the blob it came
                // from, which `publish` copied byte-for-byte into this
                // handle — so `addr` is inside `[nm.code.base,
                // nm.code.base + nm.code.len)`, live MAP_JIT memory, and
                // 8-byte aligned (D3.3: the pool starts 8-aligned and
                // every entry is one `u64`). The write is guarded (this
                // function's own `guard`, noted for this exact range).
                unsafe { f(&mut *addr) };
            }
        }
    }

    /// `key_klass`/`key_selector` are plain Rust struct fields, not
    /// `MAP_JIT` memory — no guard needed, just `f`'s transform applied in
    /// place. Call [`Self::rehash`] afterward. Called by BOTH collectors:
    /// the full GC passes `forward_chase`, and the SCAVENGE passes a
    /// `|oop| scavenge_oop(vm, oop)` closure (so `f` may capture `&mut
    /// VmState` — that's fine, `code_table` is taken out via
    /// `std::mem::take` for the duration so the borrow doesn't conflict).
    /// An earlier draft treated these as full-GC-only "strong-until-S12"
    /// roots and had the scavenge skip them entirely — a real
    /// use-after-free: a scavenge that relocates a YOUNG `key_selector`
    /// symbol (or `key_klass`) updated the interning symbol/class table's
    /// own reference but left this SEPARATE nmethod copy dangling at the
    /// vacated address, which a later full GC would then chase into poisoned
    /// memory. Found via `MACVM_GC_STRESS=1 + MACVM_JIT=threshold=1`
    /// together (see `Justfile`'s `gate-s11`).
    pub fn update_keys(&mut self, f: &mut dyn FnMut(Oop) -> Oop) {
        for nm in self.slots.iter_mut().flatten() {
            let nk = f(nm.key_klass.oop());
            // SAFETY: a collector transform never changes an oop's shape,
            // only (at most) its address — this was klass-shaped before,
            // it's klass-shaped after (same reasoning as `roots.rs`'s own
            // `root_klass!`/`root_sel!` macros, which this mirrors).
            nm.key_klass = unsafe { KlassOop::from_oop_unchecked(nk) };
            let ns = f(nm.key_selector.oop());
            nm.key_selector = unsafe { SymbolOop::from_oop_unchecked(ns) };
            // S11 D3: each IcSite's own selector is the SAME kind of
            // Rust-side (not MAP_JIT) oop field as key_selector above —
            // the machine-code pool word carrying the same selector
            // (RelocKind::Oop, emitted alongside the InlineCache reloc)
            // is handled by `oops_do` already; this is purely the
            // Rust-struct-field half.
            for site in &mut nm.ic_sites {
                let ss = f(site.selector.oop());
                site.selector = unsafe { SymbolOop::from_oop_unchecked(ss) };
                // A resolved-mono site also mirrors its guard `klass` as a
                // Rust-side oop (the machine-code guard's own klass pool word
                // is `oops_do`'s job; this is the Rust-struct half). Same
                // relocation hazard as `key_selector`: `rt_resolve_send`'s
                // poly-transition path derefs this `klass`
                // (`lookup(vm, old_k, ..)`), so a stale one is a
                // use-after-free once a moving GC relocates that klass.
                if let IcState::Mono { klass, target } = site.state {
                    let nk = f(klass.oop());
                    site.state = IcState::Mono {
                        klass: unsafe { KlassOop::from_oop_unchecked(nk) },
                        target,
                    };
                }
            }
        }
    }

    /// Rebuilds `by_key` from every nmethod's current `key_klass`/
    /// `key_selector` — mandatory after [`Self::update_keys`] moves any of
    /// them (P7): the map is keyed on raw oop bits, which a moving full GC
    /// changes out from under it exactly like `LookupCache`'s own entries.
    pub fn rehash(&mut self) {
        self.by_key.clear();
        for nm in self.slots.iter().flatten() {
            let key = (nm.key_klass.oop().raw(), nm.key_selector.oop().raw());
            self.by_key.insert(key, nm.id);
        }
    }

    /// Test-only hook standing in for S12 flushing (the real way a slot an
    /// IC still names can go away — S10 never frees one organically):
    /// simulates a stale id by clearing `id`'s slot out from under any IC
    /// that still holds it, so `send_generic`'s own re-validate-before-
    /// `enter_compiled` check (`code_table.get(nm_id).is_some_and(...)`)
    /// has a real gap to catch (tests_s10.md's `stale_id_self_heals`).
    #[cfg(test)]
    pub(crate) fn test_clear_slot(&mut self, id: NmethodId) {
        self.slots[id.0 as usize] = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::assembler::CodeBlob;
    use crate::memory::alloc;
    use crate::memory::{fullgc, scavenge};
    use crate::oops::layout::MEM_TAG;
    use crate::oops::smi::SmallInt;
    use crate::oops::wrappers::ArrayOop;
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

    /// A syntactically valid but never-allocated `KlassOop`/`SymbolOop`,
    /// for tests that only exercise `CodeTable`'s own bookkeeping (raw
    /// key bits, id/address arithmetic) — none of `install`/`get`/
    /// `lookup`/`find_by_pc` ever dereference `key_klass`/`key_selector`
    /// or `code.base`, only `MemOop::from_oop_unchecked`'s tag-level cast
    /// (`oops::wrappers` doc: "tag-level check only") and pointer/`u64`
    /// arithmetic, so a fake but correctly mem-tagged address is sound
    /// here — unlike [`CodeTable::oops_do`], which really does write
    /// through `code.base` and therefore needs a real `CodeCache`
    /// allocation (see the scavenge/full-GC tests below).
    fn fake_klass(addr: usize) -> KlassOop {
        unsafe { KlassOop::from_oop_unchecked(Oop::from_raw(addr as u64 + MEM_TAG)) }
    }
    fn fake_selector(addr: usize) -> SymbolOop {
        unsafe { SymbolOop::from_oop_unchecked(Oop::from_raw(addr as u64 + MEM_TAG)) }
    }
    fn fake_nmethod(
        key_klass: KlassOop,
        key_selector: SymbolOop,
        base: usize,
        len: usize,
    ) -> Nmethod {
        Nmethod {
            id: NmethodId(0),
            key_klass,
            key_selector,
            code: CodeHandle {
                base: base as *const u8,
                len,
            },
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

    /// `tests_s12.md`'s `oopmap_roundtrip`: slots {0, 5, 63, 64} on a
    /// 70-slot map — 63 and 64 straddle the word boundary (`SmallVec<[u64;
    /// 1]>` needs a second word once any bit >= 64 is set), the exact case
    /// a plain per-bit `Vec<bool>` would never exercise.
    #[test]
    fn oopmap_roundtrip() {
        let mut map = OopMap::empty();
        for &s in &[0u16, 5, 63, 64] {
            map.set(s);
        }
        let got: Vec<u16> = map.iter_slots().collect();
        assert_eq!(got, vec![0, 5, 63, 64]);
        for s in 0..70u16 {
            assert_eq!(map.is_oop(s), [0u16, 5, 63, 64].contains(&s), "slot {s}");
        }
    }

    /// `iter_slots`/`is_oop` on a genuinely empty map (no `set` calls at
    /// all) must never panic and never report anything live — the
    /// `oopmaps[0]` reserved-empty-map convention's own correctness
    /// depends on this.
    #[test]
    fn oopmap_empty_reports_nothing() {
        let map = OopMap::empty();
        assert_eq!(map.iter_slots().count(), 0);
        for s in 0..128u16 {
            assert!(!map.is_oop(s));
        }
    }

    /// `tests_s12.md`'s `pcdesc_exact_match_required` (P1): `oopmap_at`
    /// must hit EXACTLY, never nearest-below — `ret_pc ± 4` (one
    /// instruction either side of the real safepoint) must panic, not
    /// silently return a neighboring, WRONG map.
    #[test]
    fn pcdesc_exact_match_required() {
        let base = 0x10000usize;
        let mut map0 = OopMap::empty();
        map0.set(2);
        let nm = Nmethod {
            frame_slots: 4,
            slot_is_oop: vec![false, false, true, false],
            pcdescs: vec![PcDesc {
                pc_off: 0x20,
                bci: 3,
                oopmap: 0,
            }],
            oopmaps: vec![map0],
            ..fake_nmethod(fake_klass(0x1000), fake_selector(0x2000), base, 0x100)
        };

        let hit = nm.oopmap_at((base + 0x20) as u64);
        assert!(hit.is_oop(2));

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            nm.oopmap_at((base + 0x20 - 4) as u64)
        }));
        assert!(
            result.is_err(),
            "4 bytes before the real safepoint must miss"
        );

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            nm.oopmap_at((base + 0x20 + 4) as u64)
        }));
        assert!(
            result.is_err(),
            "4 bytes after the real safepoint must miss too"
        );
    }

    /// tests_s10.md: install -> get by id, lookup by key, find_by_pc
    /// inside the code range and at its last byte.
    #[test]
    fn codetable_install_get_find() {
        let mut table = CodeTable::new();
        let k = fake_klass(0x1000);
        let s = fake_selector(0x2000);
        let nm = fake_nmethod(k, s, 0x4000, 64);
        let id = table.install(nm);

        assert_eq!(table.get(id).unwrap().code.base as usize, 0x4000);
        assert_eq!(table.lookup(k, s), Some(id));
        assert_eq!(
            table.lookup(fake_klass(0x1000), fake_selector(0x9990)),
            None,
            "different selector must miss"
        );

        // Inside the range: [0x4000, 0x4040).
        assert_eq!(table.find_by_pc(0x4000), Some(id), "first byte");
        assert_eq!(table.find_by_pc(0x4030), Some(id), "mid-range byte");
        assert_eq!(table.find_by_pc(0x403f), Some(id), "last byte");
        assert_eq!(table.find_by_pc(0x4040), None, "one past the end");
        assert_eq!(table.find_by_pc(0x3fff), None, "one before the start");
    }

    /// tests_s10.md (P7): mutate key oop bits (simulated move), rehash,
    /// lookup by the new key works, the old key misses.
    #[test]
    fn codetable_rehash_after_move() {
        let mut table = CodeTable::new();
        let old_k = fake_klass(0x1000);
        let s = fake_selector(0x2000);
        let nm = fake_nmethod(old_k, s, 0x4000, 16);
        let id = table.install(nm);
        assert_eq!(table.lookup(old_k, s), Some(id));

        // Simulate a full-GC move: the klass's address changes, exactly
        // what `update_keys` would apply via a real `forward_chase`.
        let new_k = fake_klass(0x9000);
        table.update_keys(&mut |oop| {
            if oop.raw() == old_k.oop().raw() {
                new_k.oop()
            } else {
                oop
            }
        });

        assert_eq!(
            table.lookup(old_k, s),
            Some(id),
            "by_key is stale until rehash runs"
        );
        table.rehash();
        assert_eq!(table.lookup(old_k, s), None, "old key must now miss");
        assert_eq!(table.lookup(new_k, s), Some(id), "new key must hit");
    }

    /// Publishes a tiny real blob into a real `CodeCache`, patches one
    /// pool word to a live array's oop, installs an `Nmethod` referencing
    /// it via a `Reloc::Oop`, then runs a real scavenge and confirms
    /// `CodeTable::oops_do` (wired into `scavenge::scavenge`) updated the
    /// pool word to the array's new address with its contents intact —
    /// the scavenge half of D8, tested without waiting for `compile_method`
    /// (S10 step 7) to exist.
    #[test]
    fn oops_do_relocates_pool_word_across_scavenge() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        let arr = alloc::alloc_indexable_oops(&mut vm, klass, 1);
        arr.at_put(0, SmallInt::new(42).oop());
        let old_oop = arr.oop();

        let h = vm
            .code_cache
            .alloc(16)
            .expect("code cache alloc for test blob");
        let blob = CodeBlob {
            code: vec![0u8; 16],
            literal_off: 8,
            relocs: Vec::new(),
            listing: Vec::new(),
        };
        vm.code_cache.publish(h, &blob);
        let pool_addr = unsafe { h.base.add(8) } as *mut u64;
        vm.code_cache.patch_pool_word(pool_addr, old_oop.raw());

        let sel = vm.universe.intern(b"testSelector");
        let nm = fake_nmethod(klass, sel, h.base as usize, h.len);
        let nm = Nmethod {
            relocs: vec![crate::compiler::assembler::Reloc {
                offset: 8,
                kind: RelocKind::Oop,
            }],
            ..nm
        };
        let id = vm.code_table.install(nm);

        scavenge::scavenge(&mut vm).expect("scavenge must succeed");

        let nm2 = vm.code_table.get(id).unwrap();
        let new_bits = unsafe { *(nm2.code.base.add(8) as *const u64) };
        let new_oop = Oop::from_raw(new_bits);
        assert_ne!(
            new_oop.raw(),
            old_oop.raw(),
            "the pool word must have been updated to the array's new address"
        );
        let new_arr = ArrayOop::try_from(new_oop).expect("pool word must still be array-shaped");
        assert_eq!(new_arr.len(), 1);
        assert_eq!(new_arr.at(0), SmallInt::new(42).oop());
    }

    /// As above, but drives a real full GC — exercising `oops_do`'s
    /// full-GC call site AND `update_keys`/`rehash` (`key_klass` is the
    /// live `array_klass`, which a full GC's slide can itself relocate)
    /// in the same pass.
    #[test]
    fn full_gc_updates_pool_word_and_key_klass() {
        let mut vm = test_vm();
        let klass = vm.universe.array_klass;
        let arr = alloc::alloc_indexable_oops(&mut vm, klass, 1);
        arr.at_put(0, SmallInt::new(7).oop());
        let old_oop = arr.oop();

        let h = vm
            .code_cache
            .alloc(16)
            .expect("code cache alloc for test blob");
        let blob = CodeBlob {
            code: vec![0u8; 16],
            literal_off: 8,
            relocs: Vec::new(),
            listing: Vec::new(),
        };
        vm.code_cache.publish(h, &blob);
        let pool_addr = unsafe { h.base.add(8) } as *mut u64;
        vm.code_cache.patch_pool_word(pool_addr, old_oop.raw());

        let sel = vm.universe.intern(b"anotherTestSelector");
        let nm = fake_nmethod(klass, sel, h.base as usize, h.len);
        let nm = Nmethod {
            relocs: vec![crate::compiler::assembler::Reloc {
                offset: 8,
                kind: RelocKind::Oop,
            }],
            ..nm
        };
        let id = vm.code_table.install(nm);

        fullgc::full_gc(&mut vm).expect("full gc must succeed");

        let nm2 = vm.code_table.get(id).unwrap();
        let new_bits = unsafe { *(nm2.code.base.add(8) as *const u64) };
        let new_oop = Oop::from_raw(new_bits);
        let new_arr = ArrayOop::try_from(new_oop).expect("pool word must still be array-shaped");
        assert_eq!(new_arr.len(), 1);
        assert_eq!(new_arr.at(0), SmallInt::new(7).oop());

        // The installed nmethod's own key_klass must track wherever
        // array_klass ended up, and by_key must have been rehashed to
        // match — otherwise a post-GC `lookup` would silently miss an
        // nmethod that is, in fact, still installed and alive. Looked up
        // with a FRESH re-intern of the same selector bytes, not the
        // pre-GC `sel` local — that local is a plain Rust `Copy` value
        // the GC has no way to reach and update in place, so reusing it
        // post-GC would just be asserting a stale key correctly misses
        // (a different, less interesting claim than this test is for).
        let new_klass_bits = vm.universe.array_klass.oop().raw();
        assert_eq!(nm2.key_klass.oop().raw(), new_klass_bits);
        let sel2 = vm.universe.intern(b"anotherTestSelector");
        assert_eq!(
            vm.code_table.lookup(vm.universe.array_klass, sel2),
            Some(id),
            "by_key must be rehashed to the post-GC key"
        );
    }

    /// The SCAVENGE counterpart: a young `key_selector` symbol that a
    /// scavenge relocates must have the nmethod's own Rust-side copy updated
    /// too — not just the interning symbol table's reference. Regression for
    /// the use-after-free `gate-s11`'s combined gc-stress + JIT run exposed
    /// (the scavenge updated code-pool oops via `oops_do` but skipped
    /// `update_keys`, leaving this copy dangling at the vacated address).
    #[test]
    fn scavenge_updates_nmethod_key_selector() {
        let mut vm = test_vm();
        // The selector is freshly interned -> young -> relocated by the
        // scavenge. (`array_klass` itself is also young in this tiny genesis
        // heap and moves too -- so the lookup below re-reads
        // `vm.universe.array_klass` post-scavenge, never a stale local.)
        let klass = vm.universe.array_klass;
        let sel = vm.universe.intern(b"aFreshYoungKeySelectorForScavengeTest");
        let old_sel_bits = sel.oop().raw();

        let h = vm
            .code_cache
            .alloc(16)
            .expect("code cache alloc for test blob");
        let blob = CodeBlob {
            code: vec![0u8; 16],
            literal_off: 8,
            relocs: Vec::new(),
            listing: Vec::new(),
        };
        vm.code_cache.publish(h, &blob);
        let nm = fake_nmethod(klass, sel, h.base as usize, h.len);
        let id = vm.code_table.install(nm);

        scavenge::scavenge(&mut vm).expect("scavenge must succeed");

        let nm2 = vm.code_table.get(id).unwrap();
        assert_ne!(
            nm2.key_selector.oop().raw(),
            old_sel_bits,
            "scavenge must relocate the nmethod's Rust-side key_selector (the young symbol moved)"
        );
        // ...and it must still name the SAME interned symbol (an idempotent
        // re-intern returns it at its post-scavenge address).
        let sel2 = vm.universe.intern(b"aFreshYoungKeySelectorForScavengeTest");
        assert_eq!(
            nm2.key_selector.oop().raw(),
            sel2.oop().raw(),
            "the updated key_selector must resolve to the same symbol"
        );
        assert_eq!(
            vm.code_table.lookup(vm.universe.array_klass, sel2),
            Some(id),
            "by_key must be rehashed after the scavenge relocated key_selector (and key_klass)"
        );
    }
}
