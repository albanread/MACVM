//! Vendored from `rust-tcl` (locus sister repo) — see `VENDOR.md`.
//! `crate::` rewritten to `crate::vendor::rust_tcl::`; otherwise byte-identical to upstream `src/ast.rs`.

use crate::vendor::rust_tcl::span::Span;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Script {
    pub commands: Vec<Command>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Command {
    pub words: Vec<Word>,
    pub span: Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Word {
    pub parts: Vec<WordPart>,
    pub span: Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WordPart {
    Text(String),
    Var(String),
    Command(Box<Script>),
}

impl Word {
    pub fn literal(value: impl Into<String>, span: Span) -> Self {
        Self {
            parts: vec![WordPart::Text(value.into())],
            span,
        }
    }

    pub fn static_text(&self) -> Option<&str> {
        match self.parts.as_slice() {
            [WordPart::Text(text)] => Some(text),
            _ => None,
        }
    }
}
