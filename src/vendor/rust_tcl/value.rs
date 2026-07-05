//! Vendored from `rust-tcl` (locus sister repo) — see `VENDOR.md`.
//! `crate::` rewritten to `crate::vendor::rust_tcl::`; otherwise byte-identical to upstream `src/value.rs`.

use std::fmt;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Value(String);

impl Value {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn empty() -> Self {
        Self(String::new())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
