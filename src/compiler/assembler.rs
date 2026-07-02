//! Backend-neutral code-emission interface.
//!
//! The optimizing compiler emits native code through this trait *only*. The
//! concrete backend (JASM AArch64 encoder, LLVM, or none/interpreter-first) is
//! an OPEN DECISION — see `docs/DESIGN.md` §4. Keep this trait narrow so any
//! backend can implement it without the compiler front end knowing which won.
//! Nothing here commits to an instruction set; it is the seam, not the codegen.

/// A position in the code buffer that can be bound and branched to.
#[derive(Default)]
pub struct Label {
    /// Byte offset from buffer start once bound; `None` while unbound.
    pos: Option<usize>,
}

impl Label {
    pub fn new() -> Self {
        Self { pos: None }
    }
    pub fn is_bound(&self) -> bool {
        self.pos.is_some()
    }
    pub fn pos(&self) -> Option<usize> {
        self.pos
    }
    // Unused until a concrete Assembler backend calls it (S9 rewrites this
    // trait's shape entirely — see docs/sprints/sprint_s09_detail.md).
    #[allow(dead_code)]
    pub(crate) fn bind_at(&mut self, offset: usize) {
        self.pos = Some(offset);
    }
}

/// A relocation the runtime must fix up. Moving GC and inline-cache patching
/// depend on these being enumerable, so the seam surfaces them from the start.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelocKind {
    /// Embedded object pointer; GC must trace/relocate it.
    Oop,
    /// Patchable call site for a (polymorphic) inline cache.
    InlineCache,
    /// Call into a VM runtime routine.
    RuntimeCall,
    /// Absolute internal address, patched if the code moves.
    InternalWord,
}

/// Backend-neutral emitter. A concrete implementor owns a growable code buffer
/// and knows the target ISA; the compiler only ever holds a `&mut dyn Assembler`.
pub trait Assembler {
    /// Current write position, in bytes.
    fn offset(&self) -> usize;

    /// Start of the emitted code buffer.
    fn buffer(&self) -> &[u8];

    /// Fix `label` at the current offset.
    fn bind(&mut self, label: &mut Label);

    /// Unconditional branch to `label`.
    fn jump(&mut self, label: &mut Label);

    /// Record that the machine word most recently emitted needs runtime fixup.
    fn record_reloc(&mut self, kind: RelocKind, value: isize);

    /// Copy the buffer into executable memory and return the entry point.
    ///
    /// On Apple Silicon the implementor handles `MAP_JIT` / W^X
    /// (`pthread_jit_write_protect_np`) and `sys_icache_invalidate`. Returns
    /// `None` for an interpreter-first backend that emits no native code.
    fn finalize(&mut self) -> Option<*const u8>;
}
