//! Method lookup, inline caches, and type feedback.
//!
//! Dispatch goes through monomorphic inline caches that promote to polymorphic
//! inline caches (PICs) on type diversity. The PICs are also the VM's type-
//! feedback source: the adaptive compiler reads them to specialize hot code.
//! This is the Self mechanism inherited by Strongtalk and HotSpot. See
//! `docs/DESIGN.md` §3.
