//! T1 (`docs/typechecker_design.md` §3): the `TypeExpr` grammar, parsed from
//! a captured annotation's TEXT — NOT the original token stream. T0′
//! (`frontend::parser::capture_pragma_body`) already consumed that during
//! compilation; this is a separate, later pass over the reconstructed
//! string (`frontend::ast::TypeAnnotation::text`), so it re-tokenizes on
//! its own, minimal terms (identifiers, `[`, `]`, `,`, `^`, `|`, and the
//! `def` keyword).
//!
//! Grammar (design doc §3, `TypeList := TypeExpr (',' TypeExpr)*`):
//! ```text
//! TypeExpr   := UnionExpr ('def')?
//! UnionExpr  := Atom ('|' Atom)*
//! Atom       := Ident ('[' TypeList ']')?          -- Named, or Generic
//!             | '[' BlockBody ']'                  -- Block
//! BlockBody  := (TypeExpr (',' TypeExpr)*)? (',' '^' TypeExpr | '^' TypeExpr)?
//! ```
//! `def` binds loosest (applies to the whole preceding union) — a
//! judgment call absent a real corpus example to check against; the
//! inference-clause engine itself is SKIP for v1 (design doc §2), so this
//! only needs to be STORED without breaking well-formedness, never acted on.

/// One parsed type expression. Generics and unions are STORED, not
/// interpreted — v1 has no declared generic formal-parameter list to check
/// an application against (that's the optional T5's job, docs/
/// typechecker_design.md §2/§6); even here, `Generic::args` is kept only
/// for the use-site arity tally (`check::record_arity`), never substitution.
#[derive(Clone, Debug, PartialEq)]
pub enum TypeExpr {
    /// A bare name: `Integer`, `Self`, or an unapplied generic head.
    Named(String),
    /// `head[args…]` — a generic application. `args` is never empty (a
    /// bare `Cltn[]` is rejected by the parser as malformed).
    Generic { head: String, args: Vec<TypeExpr> },
    /// `[arg, arg, …, ^ret]` — a block type; either half may be absent
    /// (`[]`, `[^Boolean]`, `[Integer]`).
    Block {
        arg_types: Vec<TypeExpr>,
        return_type: Option<Box<TypeExpr>>,
    },
    /// `A | B | …`, written exactly as Strongtalk sources write it.
    /// `variants.len() >= 2`.
    Union(Vec<TypeExpr>),
    /// `X def` — the inference-clause sugar (`InferenceClause.str`/
    /// `DeltaInferenceSignature.dlt`). Stored and inert.
    InferenceDef(Box<TypeExpr>),
}

/// A malformed annotation, with a short human-readable reason —
/// `check::TypeError::MalformedTypeExpr`'s payload.
#[derive(Clone, Debug, PartialEq)]
pub struct ParseError(pub String);

impl std::fmt::Display for TypeExpr {
    /// Canonical re-rendering (T2's error messages: `subtype::ResolvedType`
    /// wraps this for "declared X, got Y"-style text) — not necessarily
    /// byte-identical to however the original annotation was WRITTEN
    /// (spacing, and any token `capture_pragma_body` couldn't faithfully
    /// preserve), but round-trips through [`parse`] to the same `TypeExpr`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeExpr::Named(n) => write!(f, "{n}"),
            TypeExpr::Generic { head, args } => {
                write!(f, "{head}[")?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, "{a}")?;
                }
                write!(f, "]")
            }
            TypeExpr::Block {
                arg_types,
                return_type,
            } => {
                write!(f, "[")?;
                for (i, a) in arg_types.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, "{a}")?;
                }
                if let Some(r) = return_type {
                    if !arg_types.is_empty() {
                        write!(f, ",")?;
                    }
                    write!(f, "^{r}")?;
                }
                write!(f, "]")
            }
            TypeExpr::Union(variants) => {
                for (i, v) in variants.iter().enumerate() {
                    if i > 0 {
                        write!(f, "|")?;
                    }
                    write!(f, "{v}")?;
                }
                Ok(())
            }
            TypeExpr::InferenceDef(inner) => write!(f, "{inner} def"),
        }
    }
}

/// Parses one annotation's captured text into a [`TypeExpr`]. `Err` for
/// anything that doesn't fully parse — including trailing text after an
/// otherwise-complete parse, which is why this consumes to end-of-input
/// rather than stopping at the first successful `Atom`.
pub fn parse(text: &str) -> Result<TypeExpr, ParseError> {
    let mut p = Parser { s: text, pos: 0 };
    let expr = p.parse_type_expr()?;
    p.skip_ws();
    if p.pos != p.s.len() {
        return Err(ParseError(format!(
            "trailing text after a complete type expression: {:?}",
            &p.s[p.pos..]
        )));
    }
    Ok(expr)
}

struct Parser<'a> {
    s: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.s[self.pos..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
        {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<char> {
        self.skip_ws();
        self.s[self.pos..].chars().next()
    }

    /// Peeks without skipping whitespace first — only used right after an
    /// identifier read, where "next char glued on with no space" matters
    /// (a generic application's `[` must immediately follow its head; a
    /// space before `[` would make `Cltn [x]` a bare `Named("Cltn")`
    /// followed by trailing-text failure, which is the right outcome —
    /// Strongtalk sources never space a generic application this way).
    fn peek_immediate(&self) -> Option<char> {
        self.s[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.s[self.pos..].chars().next()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn read_ident(&mut self) -> Option<&'a str> {
        self.skip_ws();
        let start = self.pos;
        let mut chars = self.s[self.pos..].chars();
        match chars.next() {
            Some(c) if c.is_alphabetic() || c == '_' => self.pos += c.len_utf8(),
            _ => return None,
        }
        while let Some(c) = self.s[self.pos..].chars().next() {
            if c.is_alphanumeric() || c == '_' {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
        Some(&self.s[start..self.pos])
    }

    fn parse_type_expr(&mut self) -> Result<TypeExpr, ParseError> {
        let e = self.parse_union()?;
        // `def` is a keyword suffix, not a generic identifier reference —
        // only consume it here (never let `read_ident` inside `parse_atom`
        // swallow it as if it were a type name of its own).
        let save = self.pos;
        if self.read_ident() == Some("def") {
            return Ok(TypeExpr::InferenceDef(Box::new(e)));
        }
        self.pos = save;
        Ok(e)
    }

    fn parse_union(&mut self) -> Result<TypeExpr, ParseError> {
        let mut variants = vec![self.parse_atom()?];
        while self.peek() == Some('|') {
            self.bump();
            variants.push(self.parse_atom()?);
        }
        Ok(if variants.len() == 1 {
            variants.pop().expect("just pushed one")
        } else {
            TypeExpr::Union(variants)
        })
    }

    fn parse_atom(&mut self) -> Result<TypeExpr, ParseError> {
        if self.peek() == Some('[') {
            self.bump();
            return self.parse_block_body();
        }
        let Some(name) = self.read_ident() else {
            return Err(ParseError(format!(
                "expected a type name or '[', found {:?}",
                self.peek().map_or(String::from("end of input"), String::from)
            )));
        };
        let name = name.to_string();
        if self.peek_immediate() == Some('[') {
            self.bump();
            let args = self.parse_type_list()?;
            if args.is_empty() {
                return Err(ParseError(format!(
                    "generic application '{name}[]' has no type arguments"
                )));
            }
            self.expect(']')?;
            return Ok(TypeExpr::Generic { head: name, args });
        }
        Ok(TypeExpr::Named(name))
    }

    /// Comma-separated `TypeExpr`s with no `^` — a generic application's
    /// argument list. (Block types use [`Self::parse_block_body`] instead,
    /// which additionally allows a trailing `^ReturnType`.)
    fn parse_type_list(&mut self) -> Result<Vec<TypeExpr>, ParseError> {
        let mut args = Vec::new();
        if self.peek() == Some(']') {
            return Ok(args);
        }
        loop {
            args.push(self.parse_type_expr()?);
            if self.peek() == Some(',') {
                self.bump();
                continue;
            }
            break;
        }
        Ok(args)
    }

    /// Inside a block type's `[...]`: zero or more comma-separated arg
    /// types, then an optional `^ReturnType` (which must be last).
    fn parse_block_body(&mut self) -> Result<TypeExpr, ParseError> {
        let mut arg_types = Vec::new();
        let mut return_type = None;
        if self.peek() != Some(']') {
            loop {
                if self.peek() == Some('^') {
                    self.bump();
                    return_type = Some(Box::new(self.parse_type_expr()?));
                    break; // '^' must be the last element
                }
                arg_types.push(self.parse_type_expr()?);
                if self.peek() == Some(',') {
                    self.bump();
                    continue;
                }
                break;
            }
        }
        self.expect(']')?;
        Ok(TypeExpr::Block {
            arg_types,
            return_type,
        })
    }

    fn expect(&mut self, want: char) -> Result<(), ParseError> {
        if self.peek() == Some(want) {
            self.bump();
            Ok(())
        } else {
            Err(ParseError(format!(
                "expected '{want}', found {:?}",
                self.peek().map_or(String::from("end of input"), String::from)
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(s: &str) -> TypeExpr {
        TypeExpr::Named(s.to_string())
    }

    #[test]
    fn plain_name() {
        assert_eq!(parse("Integer"), Ok(named("Integer")));
        assert_eq!(parse("Self"), Ok(named("Self")));
    }

    #[test]
    fn generic_application() {
        assert_eq!(
            parse("Cltn[Integer]"),
            Ok(TypeExpr::Generic {
                head: "Cltn".to_string(),
                args: vec![named("Integer")]
            })
        );
        assert_eq!(
            parse("OrdCltn[EX|X]"),
            Ok(TypeExpr::Generic {
                head: "OrdCltn".to_string(),
                args: vec![TypeExpr::Union(vec![named("EX"), named("X")])]
            })
        );
    }

    #[test]
    fn empty_generic_args_is_malformed() {
        assert!(parse("Cltn[]").is_err());
    }

    #[test]
    fn block_types() {
        assert_eq!(
            parse("[]"),
            Ok(TypeExpr::Block {
                arg_types: vec![],
                return_type: None
            })
        );
        assert_eq!(
            parse("[^Boolean]"),
            Ok(TypeExpr::Block {
                arg_types: vec![],
                return_type: Some(Box::new(named("Boolean")))
            })
        );
        assert_eq!(
            parse("[Integer,^Boolean]"),
            Ok(TypeExpr::Block {
                arg_types: vec![named("Integer")],
                return_type: Some(Box::new(named("Boolean")))
            })
        );
        assert_eq!(
            parse("[Integer, Integer, ^Boolean]"),
            Ok(TypeExpr::Block {
                arg_types: vec![named("Integer"), named("Integer")],
                return_type: Some(Box::new(named("Boolean")))
            })
        );
    }

    #[test]
    fn union_type() {
        assert_eq!(
            parse("EX | X"),
            Ok(TypeExpr::Union(vec![named("EX"), named("X")]))
        );
    }

    #[test]
    fn inference_def_sugar() {
        assert_eq!(
            parse("R def"),
            Ok(TypeExpr::InferenceDef(Box::new(named("R"))))
        );
    }

    #[test]
    fn trailing_garbage_is_malformed() {
        assert!(parse("Integer extra").is_err());
    }

    #[test]
    fn unbalanced_block_is_malformed() {
        assert!(parse("[Integer").is_err());
    }

    #[test]
    fn empty_text_is_malformed() {
        assert!(parse("").is_err());
    }

    #[test]
    fn nested_generic_and_block() {
        assert_eq!(
            parse("Cltn[[Integer,^Boolean]]"),
            Ok(TypeExpr::Generic {
                head: "Cltn".to_string(),
                args: vec![TypeExpr::Block {
                    arg_types: vec![named("Integer")],
                    return_type: Some(Box::new(named("Boolean")))
                }]
            })
        );
    }
}
