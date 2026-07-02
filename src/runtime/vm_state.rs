//! `VmState` — the one god-struct threaded as `&mut VmState` throughout the
//! VM (CONVENTIONS §2: no statics/globals). S1 gives it a `Universe` and the
//! options env parsing produces; later sprints add process/stack state.

use std::collections::HashSet;

use crate::interpreter::stack::{ProcessStack, DEFAULT_STACK_CAPACITY};
use crate::memory::Universe;

/// Which `MACVM_TRACE` channels are enabled. The channel set is open-ended
/// (CONVENTIONS §3); S1 only stores membership, nothing reads it yet.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct TraceFlags {
    channels: HashSet<String>,
}

impl TraceFlags {
    pub fn parse(s: &str) -> TraceFlags {
        TraceFlags {
            channels: s
                .split(',')
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect(),
        }
    }

    pub fn is_enabled(&self, channel: &str) -> bool {
        self.channels.contains(channel)
    }
}

pub struct VmOptions {
    /// Address-space reservation size, in MiB. Default 8192 (SPEC §7.1's
    /// tunable default), overridden by `MACVM_HEAP`.
    pub heap_mib: usize,
    pub trace: TraceFlags,
}

impl VmOptions {
    pub const DEFAULT_HEAP_MIB: usize = 8192;

    pub fn from_env() -> VmOptions {
        let heap_mib = std::env::var("MACVM_HEAP")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(Self::DEFAULT_HEAP_MIB);
        let trace = std::env::var("MACVM_TRACE")
            .ok()
            .map(|s| TraceFlags::parse(&s))
            .unwrap_or_default();
        VmOptions { heap_mib, trace }
    }
}

impl Default for VmOptions {
    fn default() -> Self {
        VmOptions {
            heap_mib: Self::DEFAULT_HEAP_MIB,
            trace: TraceFlags::default(),
        }
    }
}

pub struct VmState {
    pub universe: Universe,
    pub options: VmOptions,
    pub stack: ProcessStack,
    /// GC/interrupt poll flag, checked at `jump_back` (SPEC §5.5). A no-op
    /// hook in S2 (never set) so the dispatch loop's shape never changes
    /// once S7 wires a real poll behind it.
    pub pending: bool,
    /// Scratch: the bci the caller should resume at, set by
    /// `return_from_frame` on a non-entry return and read by
    /// `interpreter::resume_bci`. S2 never exercises the non-entry path
    /// (S2's activate/return discipline is single-frame only); this exists
    /// so the signature `return_from_frame(vm, result) -> Option<Oop>` the
    /// interfaces doc pins doesn't need to carry the resume bci itself.
    pub(crate) resume_bci: usize,
}

impl VmState {
    /// Parses options from the environment once and boots a fresh universe.
    pub fn new() -> VmState {
        Self::with_options(VmOptions::from_env())
    }

    /// Bypasses env parsing — required by tests (parallel test runners
    /// cannot race on process-wide env vars) and by any later embedder.
    pub fn with_options(options: VmOptions) -> VmState {
        let universe = Universe::genesis(&options);
        VmState {
            universe,
            options,
            stack: ProcessStack::with_capacity(DEFAULT_STACK_CAPACITY),
            pending: false,
            resume_bci: 0,
        }
    }
}

impl Default for VmState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_flags_parse() {
        let t = TraceFlags::parse("gc, jit,, bytecode");
        assert!(t.is_enabled("gc"));
        assert!(t.is_enabled("jit"));
        assert!(t.is_enabled("bytecode"));
        assert!(!t.is_enabled("ic"));
    }

    #[test]
    fn vm_options_default_heap() {
        let o = VmOptions::default();
        assert_eq!(o.heap_mib, VmOptions::DEFAULT_HEAP_MIB);
    }
}
