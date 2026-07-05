//! Vendored from `rust-tcl` (locus sister repo) — see `VENDOR.md`.
//! `crate::` rewritten to `crate::vendor::rust_tcl::`; otherwise byte-identical to upstream `src/cfg.rs`.

use crate::vendor::rust_tcl::sema::{BoundCommand, BoundCommandTarget, BoundScript, BoundWord};
use crate::vendor::rust_tcl::span::Span;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cfg {
    pub blocks: Vec<BasicBlock>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BasicBlock {
    pub id: BlockId,
    pub commands: Vec<CfgCommand>,
    pub terminator: Terminator,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BlockId(pub usize);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CfgCommand {
    pub target: BoundCommandTarget,
    pub args: Vec<BoundWord>,
    pub span: Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Terminator {
    Return,
}

pub fn build(script: &BoundScript) -> Cfg {
    let commands = script.commands.iter().map(lower_command).collect();
    Cfg {
        blocks: vec![BasicBlock {
            id: BlockId(0),
            commands,
            terminator: Terminator::Return,
        }],
    }
}

fn lower_command(command: &BoundCommand) -> CfgCommand {
    CfgCommand {
        target: command.target.clone(),
        args: command.args.clone(),
        span: command.span,
    }
}
