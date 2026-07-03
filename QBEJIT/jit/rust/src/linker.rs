//! Post-allocation relocation and trampoline generation. Port of
//! `jit_linker.zig`, driving [`crate::memory::JitMemoryRegion`] against a
//! completed [`crate::module::JitModule`].
//!
//! Resolves three kinds of relocation, in this order (mirroring `link()`):
//!
//! 1. **Data relocations (ADRP+ADD)**: [`JitModule::load_addr_relocs`]
//!    entries get their placeholder `ADRP`/`ADD` pair patched with the real
//!    page-delta/lo12 once code+data addresses are known
//!    ([`memory::JitMemoryRegion::patch_adrp_add`]).
//! 2. **Trampoline island (`CALL_EXT`)**: external calls beyond a `BL`'s
//!    ±128MB range get a 16-byte stub (`LDR X16,[PC,#8]; BR X16; .quad
//!    addr`) in the trailing trampoline region; in-range calls get a direct
//!    `BL` patch instead, skipping the stub.
//! 3. **Data symbol references (`DATA_SYMREF`)**: `.quad symbol+addend`
//!    slots in the data section (e.g. vtable entries) get the resolved
//!    absolute address written in directly.
//!
//! Symbol resolution order, for both (2) and (3): the module's own
//! internal symbol table (functions/data defined in this same
//! compilation), then a caller-supplied [`RuntimeContext`] jump table
//! (fast path), then `dlsym()` (slow-path fallback, with the macOS
//! leading-underscore retry upstream `dlsym` callers need).
//!
//! Dropped versus the Zig original (see `../README.md`): the
//! `LinkResult::dumpReport`/`analyzeDirectBLReach`/`BLSample`/
//! `DirectBLAnalysis` reporting and reach-statistics machinery — analysis
//! tooling, not part of the link contract. [`LinkResult::diagnostics`] here
//! is a flat `Vec<String>` (no severity tagging) for the same reason
//! `JitModule::errors` is — see that field's doc comment.

use crate::memory::JitMemoryRegion;
use crate::module::JitModule;
use libc::c_void;
use std::collections::HashSet;

// ============================================================================
// Section: Relocation records / results
// ============================================================================

/// Mirrors `DataRelocation`.
#[derive(Debug, Clone)]
pub struct DataRelocation {
    pub adrp_offset: u32,
    pub sym_name: String,
    pub inst_index: u32,
    pub addend: i64,
}

/// Mirrors `SymbolSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolSource {
    JumpTable,
    Dlsym,
    Internal,
    Unresolved,
}

/// Mirrors `ResolvedSymbol`.
#[derive(Debug, Clone)]
pub struct ResolvedSymbol {
    pub name: String,
    pub address: u64,
    pub source: SymbolSource,
}

/// Mirrors `TrampolineStub`.
#[derive(Debug, Clone)]
pub struct TrampolineStub {
    pub name: String,
    pub stub_offset: usize,
    pub target_addr: u64,
    pub resolved: bool,
}

/// Mirrors `LinkStats`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LinkStats {
    pub data_relocs_processed: u32,
    pub data_relocs_patched: u32,
    pub trampolines_generated: u32,
    pub ext_calls_patched: u32,
    pub ext_calls_direct: u32,
    pub symbols_from_jump_table: u32,
    pub symbols_from_dlsym: u32,
    pub symbols_internal: u32,
    pub symbols_unresolved: u32,
    pub errors: u32,
    pub warnings: u32,
}

/// Result of a full linking pass. Mirrors `LinkResult` (minus
/// `dumpReport`/`deinit` — the former is dropped reporting machinery, the
/// latter is automatic in Rust).
#[derive(Debug, Default)]
pub struct LinkResult {
    pub stats: LinkStats,
    pub resolved_symbols: Vec<ResolvedSymbol>,
    pub trampoline_stubs: Vec<TrampolineStub>,
    pub data_relocations: Vec<DataRelocation>,
    pub diagnostics: Vec<String>,
}

impl LinkResult {
    pub fn success(&self) -> bool {
        self.stats.errors == 0
    }
}

// ============================================================================
// Section: Runtime Context / Jump Table
// ============================================================================

/// A jump table entry mapping a symbol name to a function pointer. Mirrors
/// `JumpTableEntry`.
#[derive(Debug, Clone)]
pub struct JumpTableEntry {
    pub name: String,
    pub address: u64,
}

/// Function pointers for external functions JIT-compiled code needs to
/// call — MACVM's own runtime, in place of FasterBASIC's (which is not
/// ported here; see `../README.md`). Mirrors `RuntimeContext`.
///
/// This is the fast path for symbol resolution: checked before falling
/// back to `dlsym()` for every `CALL_EXT`/`DATA_SYMREF` symbol.
pub struct RuntimeContext<'a> {
    pub entries: &'a [JumpTableEntry],
    /// `dlsym` search scope. `None` means `RTLD_DEFAULT` (search all
    /// loaded images) — the common case.
    pub dlsym_handle: Option<*mut c_void>,
}

impl<'a> RuntimeContext<'a> {
    pub fn lookup(&self, name: &str) -> Option<u64> {
        self.entries.iter().find(|e| e.name == name).map(|e| e.address)
    }

    pub fn empty() -> RuntimeContext<'static> {
        RuntimeContext { entries: &[], dlsym_handle: None }
    }
}

// ============================================================================
// Section: dlsym
// ============================================================================

/// Mirrors `DlsymFn.resolve`: tries `name` as given, then (macOS only) with
/// a leading underscore — the C ABI's symbol-table prefix convention.
fn dlsym_resolve(handle: Option<*mut c_void>, name: &str) -> Option<u64> {
    let effective_handle = handle.unwrap_or(libc::RTLD_DEFAULT);

    let try_one = |s: &str| -> Option<u64> {
        let cname = std::ffi::CString::new(s).ok()?;
        // SAFETY: `cname` is a valid NUL-terminated C string for the
        // duration of this call; `effective_handle` is either a caller-
        // supplied valid dlopen handle or the RTLD_DEFAULT sentinel.
        let ptr = unsafe { libc::dlsym(effective_handle, cname.as_ptr()) };
        if ptr.is_null() { None } else { Some(ptr as u64) }
    };

    if let Some(addr) = try_one(name) {
        return Some(addr);
    }
    if cfg!(target_os = "macos") {
        let prefixed = format!("_{name}");
        if let Some(addr) = try_one(&prefixed) {
            return Some(addr);
        }
    }
    None
}

// ============================================================================
// Section: Data Relocation Collection
// ============================================================================

/// Mirrors `collectDataRelocations`'s primary path (the module's own
/// `load_addr_relocs`, always populated by [`crate::encode::encode`] — the
/// Zig source's JitInst-stream-rescan fallback is dropped as dead code
/// for this port, since `crate::encode` always populates the primary path).
pub fn collect_data_relocations(module: &JitModule) -> Vec<DataRelocation> {
    module
        .load_addr_relocs
        .iter()
        .map(|lar| DataRelocation {
            adrp_offset: lar.adrp_offset,
            sym_name: lar.sym_name.clone(),
            inst_index: lar.inst_index,
            addend: lar.addend,
        })
        .collect()
}

// ============================================================================
// Section: External Symbol Resolution
// ============================================================================

/// Mirrors `resolveExternalSymbols`: deduplicates `module.ext_calls` by
/// name, then resolves each via internal symbol table → jump table →
/// `dlsym()`, in that order.
pub fn resolve_external_symbols(
    module: &JitModule,
    context: Option<&RuntimeContext>,
    stats: &mut LinkStats,
    diagnostics: &mut Vec<String>,
) -> Vec<ResolvedSymbol> {
    let mut resolved = Vec::new();
    let mut seen = HashSet::new();

    for ext_call in &module.ext_calls {
        let name = ext_call.sym_name.as_str();
        if name.is_empty() || !seen.insert(name.to_string()) {
            continue;
        }

        let mut source = SymbolSource::Unresolved;
        let mut address = 0u64;

        // Step 1: internal symbol table. On macOS, CALL_EXT names may
        // carry a leading '_' (ABI prefix) that FUNC_BEGIN names don't —
        // try both.
        let internal_lookup_name = name.strip_prefix('_').unwrap_or(name);
        let internal_sym = module.symbols.get(name).or_else(|| module.symbols.get(internal_lookup_name));
        if let Some(sym) = internal_sym {
            if sym.is_code {
                address = sym.offset as u64;
                source = SymbolSource::Internal;
                stats.symbols_internal += 1;
            }
        }

        // Step 2: RuntimeContext jump table.
        if source == SymbolSource::Unresolved {
            if let Some(ctx) = context {
                if let Some(addr) = ctx.lookup(name) {
                    address = addr;
                    source = SymbolSource::JumpTable;
                    stats.symbols_from_jump_table += 1;
                }
            }
        }

        // Step 3: dlsym fallback.
        if source == SymbolSource::Unresolved {
            let handle = context.and_then(|c| c.dlsym_handle);
            if let Some(addr) = dlsym_resolve(handle, name) {
                address = addr;
                source = SymbolSource::Dlsym;
                stats.symbols_from_dlsym += 1;
            } else {
                stats.symbols_unresolved += 1;
                diagnostics.push(format!("unresolved external: \"{name}\""));
            }
        }

        resolved.push(ResolvedSymbol { name: name.to_string(), address, source });
    }

    resolved
}

// ============================================================================
// Section: Trampoline Island
// ============================================================================

/// Mirrors `buildTrampolineIsland`: one 16-byte stub per non-internal
/// resolved symbol (a real trampoline for resolved ones, a `BRK #0xF001`
/// trap stub for unresolved ones — so a call to an unresolved symbol
/// faults cleanly with `SIGTRAP` instead of jumping through a null
/// pointer).
pub fn build_trampoline_island(
    region: &mut JitMemoryRegion,
    resolved_symbols: &[ResolvedSymbol],
    stats: &mut LinkStats,
    diagnostics: &mut Vec<String>,
) -> Vec<TrampolineStub> {
    let mut stubs = Vec::new();

    for sym in resolved_symbols {
        if sym.source == SymbolSource::Internal {
            continue;
        }
        let is_resolved = sym.source != SymbolSource::Unresolved;

        if !is_resolved {
            match region.write_trap_stub() {
                Ok(offset) => {
                    stats.trampolines_generated += 1;
                    stubs.push(TrampolineStub {
                        name: sym.name.clone(),
                        stub_offset: offset,
                        target_addr: sym.address,
                        resolved: false,
                    });
                }
                Err(e) => {
                    diagnostics.push(format!("trap stub write failed for \"{}\": {e}", sym.name));
                    stats.errors += 1;
                }
            }
            continue;
        }

        match region.write_trampoline(sym.address) {
            Ok(offset) => {
                stats.trampolines_generated += 1;
                stubs.push(TrampolineStub {
                    name: sym.name.clone(),
                    stub_offset: offset,
                    target_addr: sym.address,
                    resolved: true,
                });
            }
            Err(e) => {
                diagnostics.push(format!("trampoline write failed for \"{}\": {e}", sym.name));
                stats.errors += 1;
            }
        }
    }

    stubs
}

// ============================================================================
// Section: External Call Patching
// ============================================================================

/// Mirrors `patchExternalCalls`: for each `CALL_EXT` site, patches its
/// placeholder `BL` to branch directly to an internal function, directly
/// to an external target within ±128MB, or to that target's trampoline
/// stub — in that preference order (skipping a trampoline whenever a
/// direct `BL` reaches).
pub fn patch_external_calls(
    region: &mut JitMemoryRegion,
    module: &JitModule,
    trampoline_stubs: &[TrampolineStub],
    resolved_symbols: &[ResolvedSymbol],
    stats: &mut LinkStats,
    diagnostics: &mut Vec<String>,
) {
    for ext_call in &module.ext_calls {
        let name = ext_call.sym_name.as_str();
        if name.is_empty() {
            continue;
        }

        if let Some(sym) = resolved_symbols.iter().find(|s| s.source == SymbolSource::Internal && s.name == name) {
            if let Err(e) = region.patch_bl_to_trampoline(ext_call.code_offset as usize, sym.address as usize) {
                diagnostics.push(format!(
                    "BL patch (internal) failed at 0x{:x} for \"{name}\": {e}",
                    ext_call.code_offset
                ));
                stats.errors += 1;
                continue;
            }
            stats.ext_calls_patched += 1;
            continue;
        }

        let target_addr = resolved_symbols
            .iter()
            .find(|s| s.source != SymbolSource::Internal && s.name == name)
            .map(|s| s.address)
            .unwrap_or(0);

        if target_addr != 0 && region.patch_bl_direct(ext_call.code_offset as usize, target_addr).is_ok() {
            stats.ext_calls_patched += 1;
            stats.ext_calls_direct += 1;
            continue;
        }

        if let Some(stub) = trampoline_stubs.iter().find(|s| s.name == name) {
            if let Err(e) = region.patch_bl_to_trampoline(ext_call.code_offset as usize, stub.stub_offset) {
                diagnostics.push(format!("BL patch failed at 0x{:x} for \"{name}\": {e}", ext_call.code_offset));
                stats.errors += 1;
                continue;
            }
            stats.ext_calls_patched += 1;
        } else {
            diagnostics.push(format!("no trampoline for \"{name}\" at 0x{:x}", ext_call.code_offset));
            stats.warnings += 1;
        }
    }
}

// ============================================================================
// Section: Data Relocations (ADRP + ADD)
// ============================================================================

/// Mirrors `resolveDataRelocations`: for each `LOAD_ADDR` relocation, looks
/// up the target symbol's offset, computes its real address (code or data
/// region, plus `addend`), and patches the `ADRP`/`ADD` pair.
pub fn resolve_data_relocations(
    region: &mut JitMemoryRegion,
    module: &JitModule,
    data_relocs: &[DataRelocation],
    stats: &mut LinkStats,
    diagnostics: &mut Vec<String>,
) {
    for reloc in data_relocs {
        stats.data_relocs_processed += 1;

        if reloc.sym_name.is_empty() {
            diagnostics.push(format!("data reloc at 0x{:x} has empty symbol name", reloc.adrp_offset));
            stats.errors += 1;
            continue;
        }

        let Some(sym_entry) = module.symbols.get(&reloc.sym_name) else {
            diagnostics.push(format!("data symbol \"{}\" not found in symbol table", reloc.sym_name));
            stats.warnings += 1;
            continue;
        };

        let base_addr = if sym_entry.is_code { region.code_address(sym_entry.offset as usize) } else { region.data_address(sym_entry.offset as usize) };
        let Some(base_addr) = base_addr else {
            diagnostics.push(format!("{} address null for \"{}\"", if sym_entry.is_code { "code" } else { "data" }, reloc.sym_name));
            stats.errors += 1;
            continue;
        };

        let target_addr = if reloc.addend != 0 { base_addr.wrapping_add(reloc.addend as u64) } else { base_addr };

        if let Err(e) = region.patch_adrp_add(reloc.adrp_offset as usize, target_addr) {
            diagnostics.push(format!("ADRP+ADD patch failed for \"{}\": {e}", reloc.sym_name));
            stats.errors += 1;
            continue;
        }
        stats.data_relocs_patched += 1;
    }
}

// ============================================================================
// Section: Data Symbol References (vtable-style pointers embedded in data)
// ============================================================================

/// Mirrors `resolveDataSymRefs`: patches each `DATA_SYMREF` 8-byte slot in
/// the data section with the resolved absolute address of its symbol
/// (same internal → jump-table → dlsym resolution order as external
/// calls).
pub fn resolve_data_sym_refs(
    region: &mut JitMemoryRegion,
    module: &JitModule,
    context: Option<&RuntimeContext>,
    stats: &mut LinkStats,
    diagnostics: &mut Vec<String>,
) {
    for reference in &module.data_sym_refs {
        let name = reference.sym_name.as_str();
        if name.is_empty() {
            continue;
        }

        let internal_name = name.strip_prefix('_').unwrap_or(name);
        let internal_sym = module.symbols.get(name).or_else(|| module.symbols.get(internal_name));
        let mut resolved_addr: Option<u64> = internal_sym.and_then(|sym_entry| {
            if sym_entry.is_code { region.code_address(sym_entry.offset as usize) } else { region.data_address(sym_entry.offset as usize) }
        });

        if resolved_addr.is_none() {
            if let Some(ctx) = context {
                resolved_addr = ctx.lookup(name);
            }
        }
        if resolved_addr.is_none() {
            let handle = context.and_then(|c| c.dlsym_handle);
            resolved_addr = dlsym_resolve(handle, name);
        }

        let Some(addr) = resolved_addr else {
            diagnostics.push(format!("DATA_SYMREF unresolved: \"{name}\" at data offset 0x{:x}", reference.data_offset));
            stats.warnings += 1;
            continue;
        };

        let final_addr = if reference.addend >= 0 {
            addr.wrapping_add(reference.addend as u64)
        } else {
            addr.wrapping_sub((-reference.addend) as u64)
        };

        let off = reference.data_offset as usize;
        let Some(data_slice) = region.data_slice() else { continue };
        if off + 8 <= data_slice.len() {
            // SAFETY: `region.data_slice()` returns the live, writable
            // data mapping (checked non-empty by the length check above);
            // `off + 8 <= len` was just verified, so this 8-byte write is
            // in-bounds. `data_slice()` gives `&[u8]` but the region is a
            // mutable RW mapping we own — a `&mut` reborrow through the
            // same pointer is sound here because no other reference to
            // this byte range is live for the duration of the write.
            unsafe {
                let ptr = data_slice.as_ptr().add(off) as *mut u8;
                std::ptr::copy_nonoverlapping(final_addr.to_le_bytes().as_ptr(), ptr, 8);
            }
            stats.data_relocs_patched += 1;
        } else {
            diagnostics.push(format!("DATA_SYMREF offset 0x{off:x} out of range for \"{name}\""));
            stats.errors += 1;
        }
    }
}

// ============================================================================
// Section: Top-Level Link
// ============================================================================

/// Runs the full linking pass: copies code+data into `region`, then
/// resolves every relocation kind in the order documented at the top of
/// this file. Mirrors `link`.
pub fn link(module: &JitModule, region: &mut JitMemoryRegion, context: Option<&RuntimeContext>) -> LinkResult {
    let mut result = LinkResult::default();

    if !module.code.is_empty() {
        if let Err(e) = region.copy_code(&module.code) {
            result.diagnostics.push(format!("code copy failed: {e}"));
            result.stats.errors += 1;
            return result;
        }
    }
    if !module.data.is_empty() {
        if let Err(e) = region.copy_data(&module.data) {
            result.diagnostics.push(format!("data copy failed: {e}"));
            result.stats.errors += 1;
            return result;
        }
    }

    result.data_relocations = collect_data_relocations(module);
    result.resolved_symbols = resolve_external_symbols(module, context, &mut result.stats, &mut result.diagnostics);
    result.trampoline_stubs = build_trampoline_island(region, &result.resolved_symbols, &mut result.stats, &mut result.diagnostics);

    // Split the borrow: patch_external_calls/resolve_* need `&result.stats`-adjacent
    // fields mutated while also reading `result.resolved_symbols`/`trampoline_stubs` —
    // take local copies of the Vec-typed fields to sidestep the aliasing rather than
    // fight the borrow checker over independent struct fields.
    let resolved_symbols = std::mem::take(&mut result.resolved_symbols);
    let trampoline_stubs = std::mem::take(&mut result.trampoline_stubs);

    patch_external_calls(region, module, &trampoline_stubs, &resolved_symbols, &mut result.stats, &mut result.diagnostics);
    resolve_data_relocations(region, module, &result.data_relocations, &mut result.stats, &mut result.diagnostics);
    resolve_data_sym_refs(region, module, context, &mut result.stats, &mut result.diagnostics);

    result.resolved_symbols = resolved_symbols;
    result.trampoline_stubs = trampoline_stubs;

    result.diagnostics.push(format!(
        "linking complete: {} trampolines, {} data relocs, {} ext calls patched",
        result.stats.trampolines_generated, result.stats.data_relocs_patched, result.stats.ext_calls_patched
    ));

    result
}

/// Convenience: link, then make the region executable in one step. Mirrors
/// `linkAndFinalize`. Returns `(result, executable)` — `executable` is
/// `false` if linking failed or `make_executable()` itself failed (check
/// `result.diagnostics` either way).
pub fn link_and_finalize(module: &JitModule, region: &mut JitMemoryRegion, context: Option<&RuntimeContext>) -> (LinkResult, bool) {
    let mut result = link(module, region, context);
    if !result.success() {
        return (result, false);
    }
    if let Err(e) = region.make_executable() {
        result.diagnostics.push(format!("makeExecutable failed: {e}"));
        result.stats.errors += 1;
        return (result, false);
    }
    (result, true)
}

