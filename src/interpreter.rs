//! Baseline threaded-code interpreter — the fast-to-start execution tier.
//!
//! Runs everything first with no optimization, gathering inline-cache feedback
//! for the adaptive compiler. Hot methods graduate to the optimizing tier; the
//! interpreter also serves as the deoptimization target. See `docs/DESIGN.md`
//! §3.
