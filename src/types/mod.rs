//! Optional Strongtalk-style type checker (`docs/typechecker_design.md`).
//! Advisory, off the run path — this module is reachable ONLY from the
//! `macvm typecheck` subcommand (`main.rs`); nothing in `interpreter`,
//! `compiler`, `memory`, or `frontend`'s own execution path (`classdef`,
//! `world`) may reference it. Reviewable by `grep -r "types::" src/` minus
//! `main.rs` — the gate is an audited convention, not a Rust-privacy wall
//! (this module must be `pub` for the separate `[[bin]]` target to reach
//! it at all).
//!
//! T1 (this slice): a `TypeExpr` parser over T0′'s captured annotation
//! text, a VM-free per-class interface builder, and three checks
//! (`MalformedTypeExpr`, `UndeclaredTypeName`, `GenericArityMismatch`) run
//! over the whole world. Send/method/assignment rules and the rest of the
//! v1 error catalog are T2/T3.

pub mod check;
pub mod interface;
pub mod type_expr;

use std::path::Path;

pub use check::TypeError;
pub use interface::WorldModel;

/// Builds the world model and runs every check currently implemented
/// (`check::check_world`). The one entry point `main.rs`'s `typecheck`
/// subcommand calls.
pub fn typecheck_world(world_dir: &Path) -> Result<(WorldModel, Vec<TypeError>), String> {
    let model = interface::build_world_model(world_dir)?;
    let errors = check::check_world(&model);
    Ok((model, errors))
}
