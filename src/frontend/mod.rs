//! The MACVM source compiler (SPEC §1, §4.5, S5): lexer, parser, AST,
//! capture analysis, codegen, class-definition execution, and the
//! `world/world.list` loader. May use `oops` wrappers, `memory` allocation +
//! interning, and `bytecode::BytecodeBuilder`; must NOT call into
//! `interpreter/` directly — executing a doIt goes through the single
//! `runtime::execute_doit` entry point (`sprint_s05_detail.md` §Layer
//! boundaries).

pub mod ast;
pub mod capture;
pub mod classdef;
pub mod codegen;
pub mod lexer;
pub mod parser;
pub mod world;

use std::path::PathBuf;

use lexer::{LexError, Span};

/// The single error type spanning lex/parse/codegen/classdef/world failures
/// (`sprint_s05_detail.md` §Interfaces for later sprints). `path` is `None`
/// until a file-level caller (`load_file`) attaches it; `Display` renders
/// the pinned `file:line:col: error: msg` format (Pitfalls P10).
#[derive(Clone, Debug)]
pub struct CompileError {
    pub path: Option<PathBuf>,
    pub span: Span,
    pub msg: String,
    /// Set when the failure is "ran out of input" rather than a genuine
    /// error — the REPL keeps buffering more lines instead of reporting.
    pub eof: bool,
}

impl CompileError {
    pub fn with_path(mut self, path: PathBuf) -> CompileError {
        self.path = Some(path);
        self
    }
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path = self
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<input>".to_string());
        write!(
            f,
            "{path}:{}:{}: error: {}",
            self.span.line, self.span.col, self.msg
        )
    }
}

impl From<LexError> for CompileError {
    fn from(e: LexError) -> CompileError {
        CompileError {
            path: None,
            span: e.span,
            msg: e.msg,
            eof: e.eof,
        }
    }
}
