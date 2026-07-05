//! Vendored from `rust-tcl` (locus sister repo) — see `VENDOR.md`.
//! `crate::` rewritten to `crate::vendor::rust_tcl::`; otherwise byte-identical to upstream `src/sema.rs`.

use crate::vendor::rust_tcl::ast::{Command, Script, Word, WordPart};
use crate::vendor::rust_tcl::error::{Error, Result};
use crate::vendor::rust_tcl::registry::{Registry, VerbId};
use crate::vendor::rust_tcl::span::Span;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundScript {
    pub commands: Vec<BoundCommand>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundCommand {
    pub target: BoundCommandTarget,
    pub args: Vec<BoundWord>,
    pub span: Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BoundCommandTarget {
    Native { verb: VerbId, name: String },
    Named(String),
    Dynamic(BoundWord),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundWord {
    pub parts: Vec<BoundWordPart>,
    pub span: Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BoundWordPart {
    Text(String),
    Var(String),
    Command(Box<BoundScript>),
}

pub fn analyze(script: &Script, registry: &Registry) -> Result<BoundScript> {
    Analyzer { registry }.script(script)
}

struct Analyzer<'a> {
    registry: &'a Registry,
}

impl<'a> Analyzer<'a> {
    fn script(&self, script: &Script) -> Result<BoundScript> {
        let mut commands = Vec::with_capacity(script.commands.len());
        for command in &script.commands {
            commands.push(self.command(command)?);
        }
        Ok(BoundScript { commands })
    }

    fn command(&self, command: &Command) -> Result<BoundCommand> {
        let name_word = command
            .words
            .first()
            .expect("parser does not emit empty commands");
        let arg_count = command.words.len() - 1;
        let target = match name_word.static_text() {
            Some(name) => {
                if name.is_empty() {
                    return Err(Error::sema(name_word.span, "empty command name"));
                }
                match self.registry.resolve(name) {
                    Some(verb) => {
                        let spec = self.registry.spec(verb).expect("resolved verb exists");
                        if !spec.arity.accepts(arg_count) {
                            return Err(Error::sema(
                                command.span,
                                format!(
                                    "verb `{name}` got {arg_count} arguments but expects {}",
                                    arity_text(spec.arity)
                                ),
                            ));
                        }
                        BoundCommandTarget::Native {
                            verb,
                            name: name.to_string(),
                        }
                    }
                    None => BoundCommandTarget::Named(name.to_string()),
                }
            }
            None => BoundCommandTarget::Dynamic(self.word(name_word)?),
        };

        let mut args = Vec::with_capacity(arg_count);
        for word in &command.words[1..] {
            args.push(self.word(word)?);
        }

        Ok(BoundCommand {
            target,
            args,
            span: command.span,
        })
    }

    fn word(&self, word: &Word) -> Result<BoundWord> {
        let mut parts = Vec::with_capacity(word.parts.len());
        for part in &word.parts {
            parts.push(match part {
                WordPart::Text(text) => BoundWordPart::Text(text.clone()),
                WordPart::Var(name) => BoundWordPart::Var(name.clone()),
                WordPart::Command(script) => BoundWordPart::Command(Box::new(self.script(script)?)),
            });
        }
        Ok(BoundWord {
            parts,
            span: word.span,
        })
    }
}

fn arity_text(arity: crate::vendor::rust_tcl::registry::Arity) -> String {
    match arity.max {
        Some(max) if max == arity.min => arity.min.to_string(),
        Some(max) => format!("{}..={max}", arity.min),
        None => format!("{} or more", arity.min),
    }
}
