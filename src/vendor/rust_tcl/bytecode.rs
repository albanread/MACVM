//! Vendored from `rust-tcl` (locus sister repo) — see `VENDOR.md`.
//! `crate::` rewritten to `crate::vendor::rust_tcl::`; otherwise byte-identical to upstream `src/bytecode.rs`.

use crate::vendor::rust_tcl::registry::VerbId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Instr {
    PushConst(usize),
    LoadVar(usize),
    EvalExpr(usize),
    Concat(usize),
    Call(VerbId, usize),
    CallName(usize, usize),
    CallDynamic(usize),
    DefineProc { name: usize, proc: usize },
    JumpIfFalse(usize),
    Jump(usize),
    ForeachStart { var: usize, end: usize },
    ForeachNext { body: usize, end: usize },
    ForeachPop,
    Return,
    Pop,
    Halt,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Program {
    pub constants: Vec<String>,
    pub expressions: Vec<crate::vendor::rust_tcl::expr::Expr>,
    pub procedures: Vec<Procedure>,
    pub instructions: Vec<Instr>,
}

impl Program {
    pub fn constant(&self, index: usize) -> Option<&str> {
        self.constants.get(index).map(String::as_str)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Procedure {
    pub params: Vec<ProcedureParam>,
    pub body: Program,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcedureParam {
    pub name: String,
    pub default: Option<String>,
}
