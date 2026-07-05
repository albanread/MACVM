//! Vendored from `rust-tcl` (locus sister repo) — see `VENDOR.md`.
//! `crate::` rewritten to `crate::vendor::rust_tcl::`; otherwise byte-identical to upstream `src/registry.rs`.

use std::collections::HashMap;
use std::sync::Arc;

use crate::vendor::rust_tcl::error::Result;
use crate::vendor::rust_tcl::value::Value;
use crate::vendor::rust_tcl::vm::Flow;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct VerbId(pub usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Arity {
    pub min: usize,
    pub max: Option<usize>,
}

impl Arity {
    pub fn exact(n: usize) -> Self {
        Self {
            min: n,
            max: Some(n),
        }
    }

    pub fn at_least(n: usize) -> Self {
        Self { min: n, max: None }
    }

    pub fn range(min: usize, max: usize) -> Self {
        Self {
            min,
            max: Some(max),
        }
    }

    pub fn accepts(self, n: usize) -> bool {
        n >= self.min && self.max.map_or(true, |max| n <= max)
    }
}

pub type NativeVerb = dyn Fn(&mut crate::vendor::rust_tcl::vm::Vm<'_>, &[Value]) -> Result<Flow>
    + Send
    + Sync
    + 'static;

#[derive(Clone)]
pub struct VerbSpec {
    pub name: String,
    pub arity: Arity,
    pub handler: Arc<NativeVerb>,
}

#[derive(Clone, Default)]
pub struct Registry {
    names: HashMap<String, VerbId>,
    verbs: Vec<VerbSpec>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_core() -> Self {
        let mut registry = Self::new();
        crate::vendor::rust_tcl::verbs::register_core_verbs(&mut registry);
        registry
    }

    pub fn register<F>(&mut self, name: impl Into<String>, arity: Arity, handler: F) -> VerbId
    where
        F: Fn(&mut crate::vendor::rust_tcl::vm::Vm<'_>, &[Value]) -> Result<Value>
            + Send
            + Sync
            + 'static,
    {
        self.register_control(name, arity, move |vm, args| {
            handler(vm, args).map(Flow::Value)
        })
    }

    pub fn register_control<F>(
        &mut self,
        name: impl Into<String>,
        arity: Arity,
        handler: F,
    ) -> VerbId
    where
        F: Fn(&mut crate::vendor::rust_tcl::vm::Vm<'_>, &[Value]) -> Result<Flow>
            + Send
            + Sync
            + 'static,
    {
        let name = name.into();
        if let Some(id) = self.names.get(&name).copied() {
            self.verbs[id.0] = VerbSpec {
                name,
                arity,
                handler: Arc::new(handler),
            };
            return id;
        }

        let id = VerbId(self.verbs.len());
        self.names.insert(name.clone(), id);
        self.verbs.push(VerbSpec {
            name,
            arity,
            handler: Arc::new(handler),
        });
        id
    }

    pub fn resolve(&self, name: &str) -> Option<VerbId> {
        self.names.get(name).copied()
    }

    pub fn spec(&self, id: VerbId) -> Option<&VerbSpec> {
        self.verbs.get(id.0)
    }
}
