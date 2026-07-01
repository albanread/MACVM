//! Runtime support: activation stacks, frames, and deoptimization.
//!
//! Holds the machinery for switching a running activation between the optimized
//! and interpreted representations of a method (deoptimization / on-stack
//! replacement) when a speculative assumption made by the compiler is
//! invalidated. See `docs/DESIGN.md` §3.
