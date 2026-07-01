//! Object memory: allocation and garbage collection.
//!
//! Starting assumption (from Self and Strongtalk): a generational, moving
//! collector. Moving GC interacts with the JIT — compiled frames must expose
//! oop maps so the collector can find and relocate pointers. See
//! `docs/DESIGN.md` §5.
