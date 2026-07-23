//! Optional Strongtalk-style type checker (`docs/typechecker_design.md`).
//! Advisory, off the run path — this module is reachable ONLY from the
//! `macvm typecheck` subcommand (`main.rs`); nothing in `interpreter`,
//! `compiler`, `memory`, or `frontend`'s own execution path (`classdef`,
//! `world`) may reference it. Reviewable by `grep -r "types::" src/` minus
//! `main.rs` — the gate is an audited convention, not a Rust-privacy wall
//! (this module must be `pub` for the separate `[[bin]]` target to reach
//! it at all).
//!
//! T1: a `TypeExpr` parser over T0′'s captured annotation text, a VM-free
//! per-class interface builder, and three annotation-syntax checks
//! (`MalformedTypeExpr`, `UndeclaredTypeName`, `GenericArityMismatch`).
//!
//! T2: `subtype_of` (nominal, `Self`-aware, block-variant) + expression-
//! type synthesis for the LOCAL rules — literals, plain variable
//! references (params/temps/ivars, with real lexical scoping for block
//! shadowing), assignments, and returns (including non-local returns
//! lexically inside a block, which target the ENCLOSING method's own
//! declared return type).
//!
//! T3 (this slice): the send rule (`send.rs`) — static-DNU + per-argument
//! subtype checks, wired into `expr_type`'s expression synthesis so a
//! send's type is now the target method's declared return (was: always
//! `Dynamic`). Still only checked when the RECEIVER's own type is known —
//! an unannotated receiver (the overwhelming majority of the real world
//! today) is never checked, matching the design's own "gradual typing
//! finds little until signatures exist."

pub mod check;
pub mod expr_type;
pub mod interface;
pub mod send;
pub mod subtype;
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
