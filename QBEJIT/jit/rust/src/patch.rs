//! Post-link patching for MACVM's adaptive-tier needs: moving-GC-safe oop
//! updates and inline-cache call-site re-patching.
//!
//! Added on top of the ported FasterBASIC JIT (which has neither concept —
//! see `../README.md` and `../../REVIEW.md`'s "reloc-kind vocabulary"
//! gap). Both mechanisms are built entirely on primitives that already
//! existed in [`crate::memory`] and a symbol-naming convention recognized
//! in [`crate::encode`] ([`crate::module::OOP_SYMBOL_PREFIX`] /
//! [`crate::module::IC_SITE_PREFIX`]) — no change to the vendored `jit/c`
//! QBE fork was needed.
//!
//! # Oops: routed through a data slot, not a code immediate
//!
//! `docs/DESIGN.md` §5 asks for a reloc kind that distinguishes "embedded
//! oops (for GC)" from other relocations, because on arm64 a 64-bit oop
//! can't be one immediate — it's materialized via `ADRP`/`ADD` or
//! `MOVZ`/`MOVK` pairs, each leg individually relocatable. That's the
//! right description of a *compile-time-constant* address baked into code
//! (a runtime function, a fixed string) — but MACVM's oops are heap
//! pointers a **moving GC relocates**, and repeatedly re-encoding a
//! multi-instruction immediate load in the *code* stream every time an
//! object moves is exactly the kind of code-patching MACVM's own GC design
//! (mark-slide-compact, per the project's memory/ sources) would rather
//! avoid — it means flipping W^X, patching instructions, and flushing the
//! icache for every moved oop referenced by every JIT'd method that holds
//! it.
//!
//! QBE already gives a better mechanism for free: a `data $sym = { l 0 }`
//! definition, loaded in function bodies the normal way (`%v =l loadl
//! $sym`), resolves through the *existing* `JIT_LOAD_ADDR`/data-relocation
//! machinery in [`crate::linker`] — no new QBE IL semantics needed. And
//! critically, [`crate::memory::JitMemoryRegion`]'s data region is **always
//! RW**, never subject to the code region's W^X toggle (see
//! `JitMemoryRegion::copy_data`'s own safety note) — so updating an oop
//! after a GC move is [`update_oop_slot`]: one bounds-checked 8-byte memory
//! write, no W^X flip, no instruction re-encoding, no icache invalidation
//! (data isn't instruction-fetched).
//!
//! The tradeoff this makes: an extra indirection (one `LDR` through the
//! data pool) per oop *read* in JIT'd code, in exchange for O(1),
//! code-patch-free oop *updates*. For an adaptive tier where methods get
//! deoptimized/recompiled far more often than any single oop constant is
//! read, this is the right side of that tradeoff — and it's how mainstream
//! JITs with moving GCs (HotSpot's oop-map-driven constant pools, V8's
//! constant pool relative to the code object) generally do this too,
//! rather than patching code for every move.
//!
//! # Inline caches: named, re-patchable call sites
//!
//! [`crate::linker::patch_external_calls`] already resolves every
//! `CALL_EXT` site exactly once, at link time. What's missing for a PIC
//! (monomorphic → polymorphic → megamorphic) is a way to find *that
//! specific send site's* code offset again later, without re-running the
//! whole link pass — [`crate::module::JitModule::ic_sites`] is that index
//! (site name → `BL` offset), and [`repatch_ic_site`] is the safe
//! make-writable → patch → make-executable sequence around the
//! already-existing [`crate::memory::JitMemoryRegion::patch_bl_direct`]/
//! [`patch_bl_to_trampoline`](crate::memory::JitMemoryRegion::patch_bl_to_trampoline).

use crate::memory::{JitMemoryRegion, MemoryError};
use crate::module::JitModule;

#[derive(Debug)]
pub enum PatchError {
    /// No `data $__macvm_oop$<name>` slot with this name was found in the
    /// module (check the name matches what the QBE IL codegen emitted,
    /// including the `__macvm_oop$` prefix).
    UnknownOopSlot(String),
    /// No `call $__macvm_ic$<name>(...)` site with this name was found.
    UnknownIcSite(String),
    Memory(MemoryError),
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownOopSlot(name) => write!(f, "unknown oop slot: \"{name}\""),
            Self::UnknownIcSite(name) => write!(f, "unknown IC site: \"{name}\""),
            Self::Memory(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for PatchError {}
impl From<MemoryError> for PatchError {
    fn from(e: MemoryError) -> Self {
        Self::Memory(e)
    }
}

/// Absolute, live address of an oop slot — e.g. to register it as a GC
/// root up front, before any move happens.
pub fn oop_slot_address(module: &JitModule, region: &JitMemoryRegion, name: &str) -> Result<u64, PatchError> {
    let offset = *module.oop_slots.get(name).ok_or_else(|| PatchError::UnknownOopSlot(name.to_string()))?;
    region.data_address(offset as usize).ok_or_else(|| PatchError::Memory(MemoryError::AlreadyFreed))
}

/// Overwrite an oop slot with a new address — the moving-GC update path.
/// No W^X toggle: the data region is always writable (see this module's
/// doc comment). Every JIT'd `loadl $__macvm_oop$<name>` site sees the new
/// value on its very next execution, with nothing else to patch.
pub fn update_oop_slot(module: &JitModule, region: &mut JitMemoryRegion, name: &str, new_value: u64) -> Result<(), PatchError> {
    let offset = *module.oop_slots.get(name).ok_or_else(|| PatchError::UnknownOopSlot(name.to_string()))?;
    region.write_data_u64(offset as usize, new_value)?;
    Ok(())
}

/// Re-patch an inline-cache call site to a new target — e.g. a PIC
/// transition, or attaching the first real target to a site that started
/// out pointing at a "not yet linked" trap stub.
///
/// Tries a direct `BL` first (in range for ±128MB, the common case for a
/// call within the same JIT'd module or to a nearby runtime function);
/// falls back to writing a *new* trampoline stub and redirecting the `BL`
/// to it if the target is out of range. Returns
/// [`MemoryError::TrampolineOverflow`] (wrapped in [`PatchError::Memory`])
/// if the fallback is needed but the trampoline island has no room left —
/// there is no further fallback; size `region`'s trampoline capacity with
/// expected re-patch volume in mind, not just the initial link's needs.
///
/// Handles the full W^X sequence itself (`make_writable` →
/// patch → `make_executable`, which also re-flushes the icache) — callers
/// don't need to touch region protection state.
pub fn repatch_ic_site(module: &JitModule, region: &mut JitMemoryRegion, name: &str, new_target: u64) -> Result<(), PatchError> {
    let bl_offset = *module.ic_sites.get(name).ok_or_else(|| PatchError::UnknownIcSite(name.to_string()))?;

    region.make_writable()?;

    let patch_result = match region.patch_bl_direct(bl_offset as usize, new_target) {
        Ok(()) => Ok(()),
        Err(MemoryError::BLOutOfRange) => region
            .write_trampoline(new_target)
            .and_then(|stub_offset| region.patch_bl_to_trampoline(bl_offset as usize, stub_offset)),
        Err(e) => Err(e),
    };

    // Always try to restore Executable/W^X state, even on a patch
    // failure — leaving the region stuck Writable would defeat W^X for
    // every other already-linked, otherwise-untouched call site in it.
    let restore_result = region.make_executable();

    patch_result?;
    restore_result?;
    Ok(())
}
