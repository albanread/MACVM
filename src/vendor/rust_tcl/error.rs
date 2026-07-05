//! Vendored from `rust-tcl` (locus sister repo) — see `VENDOR.md`.
//! `crate::` rewritten to `crate::vendor::rust_tcl::`; otherwise byte-identical to upstream `src/error.rs`.

use std::fmt;

use crate::vendor::rust_tcl::span::Span;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    Lex,
    Parse,
    Sema,
    Runtime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    pub kind: ErrorKind,
    pub span: Option<Span>,
    pub message: String,
}

impl Error {
    pub fn lex(span: Span, message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Lex,
            span: Some(span),
            message: message.into(),
        }
    }

    pub fn parse(span: Span, message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Parse,
            span: Some(span),
            message: message.into(),
        }
    }

    pub fn sema(span: Span, message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Sema,
            span: Some(span),
            message: message.into(),
        }
    }

    pub fn runtime(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Runtime,
            span: None,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.span {
            Some(span) => write!(
                f,
                "{:?} error at {}..{}: {}",
                self.kind, span.start, span.end, self.message
            ),
            None => write!(f, "{:?} error: {}", self.kind, self.message),
        }
    }
}

impl std::error::Error for Error {}
