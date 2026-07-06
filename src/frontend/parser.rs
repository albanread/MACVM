//! Recursive-descent parser (`sprint_s05_detail.md` §Design "Grammar",
//! verbatim EBNF referenced in comments below). Fail-fast: the first error
//! aborts the whole parse (Pitfalls P10) — no resync, no multiple
//! diagnostics.

use crate::oops::layout::{SMI_MAX, SMI_MIN};

use super::ast::{
    BlockNode, ClassDefNode, Expr, FfiPragma, Indexable, Literal, MethodNode, TopItem,
};
use super::lexer::{int_lit_magnitude, Lexer, Span, Tok};
use super::CompileError;

enum ClassPragma {
    Indexable(Indexable),
    ClassVars(Vec<String>),
    Ignored,
}

const RESERVED: &[&str] = &["self", "super", "nil", "true", "false"];

/// `(primitive id, FFI pragma, temporaries, statements)` — a parsed
/// `method_body`. `primitive`/`ffi` are never both `Some` — S20's
/// `<primitive: FFI …>` (`ast::FfiPragma`) is parsed as a DISTINCT pragma
/// shape from the bare `<primitive: N>` integer form, not a variant of it.
type MethodBody = (Option<u16>, Option<FfiPragma>, Vec<String>, Vec<Expr>);

fn is_reserved(name: &str) -> bool {
    RESERVED.contains(&name)
}

struct Parser<'a> {
    lx: Lexer<'a>,
    cur: (Tok, Span),
    peeked: Option<(Tok, Span)>,
    literal_seq_depth: u32,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Result<Parser<'a>, CompileError> {
        let mut lx = Lexer::new(input);
        let cur = lx.next_token()?;
        Ok(Parser {
            lx,
            cur,
            peeked: None,
            literal_seq_depth: 0,
        })
    }

    fn bump(&mut self) -> Result<(), CompileError> {
        self.cur = match self.peeked.take() {
            Some(t) => t,
            None => self.lx.next_token()?,
        };
        Ok(())
    }

    fn peek2(&mut self) -> Result<&Tok, CompileError> {
        if self.peeked.is_none() {
            self.peeked = Some(self.lx.next_token()?);
        }
        Ok(&self.peeked.as_ref().unwrap().0)
    }

    fn error(&self, span: Span, msg: impl Into<String>) -> CompileError {
        CompileError {
            path: None,
            span,
            msg: msg.into(),
            eof: matches!(self.cur.0, Tok::Eof),
        }
    }

    fn check_not_reserved(&self, span: Span, name: &str) -> Result<(), CompileError> {
        if is_reserved(name) {
            Err(self.error(span, format!("'{name}' is a reserved word")))
        } else {
            Ok(())
        }
    }

    fn expect(&mut self, want: &Tok, msg: &str) -> Result<(), CompileError> {
        if std::mem::discriminant(&self.cur.0) == std::mem::discriminant(want) {
            self.bump()
        } else {
            Err(self.error(self.cur.1, msg))
        }
    }

    fn expect_ident(&mut self, msg: &str) -> Result<String, CompileError> {
        match self.cur.0.clone() {
            Tok::Ident(n) => {
                self.bump()?;
                Ok(n)
            }
            _ => Err(self.error(self.cur.1, msg)),
        }
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<(), CompileError> {
        match &self.cur.0 {
            Tok::Keyword(k) if k == kw => self.bump(),
            _ => Err(self.error(self.cur.1, format!("expected '{kw}'"))),
        }
    }

    fn enter_literal_seq(&mut self) {
        self.literal_seq_depth += 1;
        if self.literal_seq_depth == 1 {
            self.lx.set_in_literal_seq(true);
        }
    }

    fn exit_literal_seq(&mut self) {
        self.literal_seq_depth -= 1;
        if self.literal_seq_depth == 0 {
            self.lx.set_in_literal_seq(false);
        }
    }

    // --- literals ---------------------------------------------------------

    fn intlit_to_literal(negative: bool, radix: u8, digits: &str) -> Literal {
        let mag = int_lit_magnitude(radix as u32, digits);
        if mag.len() <= 8 {
            let mut v: u64 = 0;
            for (i, &b) in mag.iter().enumerate() {
                v |= (b as u64) << (8 * i);
            }
            let fits = if !negative {
                v <= SMI_MAX as u64
            } else {
                v <= (SMI_MIN as i128).unsigned_abs() as u64
            };
            if fits {
                let val = if negative { -(v as i64) } else { v as i64 };
                return Literal::Int(val);
            }
        }
        Literal::BigInt {
            negative,
            digits: mag,
        }
    }

    fn parse_array_elems(&mut self) -> Result<Vec<Literal>, CompileError> {
        let mut out = Vec::new();
        while !matches!(self.cur.0, Tok::RParen) {
            out.push(self.parse_array_elem()?);
        }
        Ok(out)
    }

    fn parse_array_elem(&mut self) -> Result<Literal, CompileError> {
        let (tok, span) = self.cur.clone();
        match tok {
            Tok::IntLit {
                negative,
                radix,
                digits,
            } => {
                self.bump()?;
                Ok(Self::intlit_to_literal(negative, radix, &digits))
            }
            Tok::FloatLit(v) => {
                self.bump()?;
                Ok(Literal::Float(v))
            }
            Tok::CharLit(c) => {
                self.bump()?;
                Ok(Literal::Char(c))
            }
            Tok::StrLit(s) => {
                self.bump()?;
                Ok(Literal::Str(s))
            }
            Tok::SymLit(s) => {
                self.bump()?;
                Ok(Literal::Symbol(s))
            }
            Tok::Ident(name) => {
                self.bump()?;
                Ok(match name.as_str() {
                    "nil" => Literal::Nil,
                    "true" => Literal::True,
                    "false" => Literal::False,
                    _ => Literal::Symbol(name),
                })
            }
            Tok::Keyword(_) => {
                // P2: adjacent Keyword tokens glue into one symbol iff
                // textually adjacent (no whitespace) — tracked via spans.
                let mut sel = String::new();
                let mut expect_col = None;
                while let Tok::Keyword(k) = self.cur.0.clone() {
                    if let Some(col) = expect_col {
                        if self.cur.1.col != col {
                            break;
                        }
                    }
                    expect_col = Some(self.cur.1.col + k.len() as u32);
                    sel.push_str(&k);
                    self.bump()?;
                }
                Ok(Literal::Symbol(sel))
            }
            Tok::BinarySel(s) => {
                self.bump()?;
                Ok(Literal::Symbol(s))
            }
            Tok::VBar => {
                self.bump()?;
                Ok(Literal::Symbol("|".to_string()))
            }
            Tok::LParen => {
                self.bump()?;
                let elems = self.parse_array_elems()?;
                self.close_paren("expected ')'")?;
                Ok(Literal::Array(elems))
            }
            Tok::LitArrayOpen => self.parse_array_literal(),
            Tok::ByteArrayOpen => self.parse_byte_array_literal(),
            _ => Err(self.error(span, "invalid literal array element")),
        }
    }

    /// Consumes the closing `)` of a `(…)`/`#(…)` literal group. Distinct
    /// from `expect(RParen,…)` so the flag-restoring order is explicit at
    /// every call site: literal-seq mode must be corrected (via
    /// `exit_literal_seq` at the outer two call sites) BEFORE the `bump()`
    /// that fetches whatever token follows the closing delimiter, else that
    /// next token is lexed under the stale (about-to-be-wrong) mode.
    fn close_paren(&mut self, msg: &str) -> Result<(), CompileError> {
        if !matches!(self.cur.0, Tok::RParen) {
            return Err(self.error(self.cur.1, msg));
        }
        self.bump()
    }

    fn parse_array_literal(&mut self) -> Result<Literal, CompileError> {
        // self.cur == LitArrayOpen
        self.enter_literal_seq();
        self.bump()?;
        let elems = self.parse_array_elems()?;
        if !matches!(self.cur.0, Tok::RParen) {
            return Err(self.error(self.cur.1, "expected ')' to close array literal"));
        }
        self.exit_literal_seq();
        self.bump()?;
        Ok(Literal::Array(elems))
    }

    fn parse_byte_array_literal(&mut self) -> Result<Literal, CompileError> {
        // self.cur == ByteArrayOpen
        self.enter_literal_seq();
        self.bump()?;
        let mut bytes = Vec::new();
        loop {
            if matches!(self.cur.0, Tok::RBracket) {
                break;
            }
            let (tok, span) = self.cur.clone();
            match tok {
                Tok::IntLit {
                    negative,
                    radix,
                    digits,
                } => {
                    self.bump()?;
                    let mag = int_lit_magnitude(radix as u32, &digits);
                    if negative || mag.len() > 1 {
                        return Err(self.error(span, "byte array element out of range 0..255"));
                    }
                    bytes.push(mag.first().copied().unwrap_or(0));
                }
                _ => return Err(self.error(span, "byte array elements must be integers")),
            }
        }
        if !matches!(self.cur.0, Tok::RBracket) {
            return Err(self.error(self.cur.1, "expected ']' to close byte array"));
        }
        self.exit_literal_seq();
        self.bump()?;
        Ok(Literal::ByteArray(bytes))
    }

    // --- expressions --------------------------------------------------------

    /// Returns the parsed primary and whether it was literally the `super`
    /// token (the only send built directly atop it may set `is_super`).
    fn parse_primary(&mut self) -> Result<(Expr, bool), CompileError> {
        let (tok, span) = self.cur.clone();
        match tok {
            Tok::Ident(name) => {
                self.bump()?;
                Ok(match name.as_str() {
                    "self" => (Expr::SelfRef(span), false),
                    "super" => (Expr::SelfRef(span), true),
                    "nil" => (
                        Expr::Lit {
                            value: Literal::Nil,
                            span,
                        },
                        false,
                    ),
                    "true" => (
                        Expr::Lit {
                            value: Literal::True,
                            span,
                        },
                        false,
                    ),
                    "false" => (
                        Expr::Lit {
                            value: Literal::False,
                            span,
                        },
                        false,
                    ),
                    _ => (Expr::Var { name, span }, false),
                })
            }
            Tok::LParen => {
                self.bump()?;
                let e = self.parse_expression()?;
                self.close_paren("expected ')'")?;
                Ok((e, false))
            }
            Tok::LBracket => Ok((Expr::Block(Box::new(self.parse_block()?)), false)),
            Tok::IntLit {
                negative,
                radix,
                digits,
            } => {
                self.bump()?;
                Ok((
                    Expr::Lit {
                        value: Self::intlit_to_literal(negative, radix, &digits),
                        span,
                    },
                    false,
                ))
            }
            Tok::FloatLit(v) => {
                self.bump()?;
                Ok((
                    Expr::Lit {
                        value: Literal::Float(v),
                        span,
                    },
                    false,
                ))
            }
            Tok::CharLit(c) => {
                self.bump()?;
                Ok((
                    Expr::Lit {
                        value: Literal::Char(c),
                        span,
                    },
                    false,
                ))
            }
            Tok::StrLit(s) => {
                self.bump()?;
                Ok((
                    Expr::Lit {
                        value: Literal::Str(s),
                        span,
                    },
                    false,
                ))
            }
            Tok::SymLit(s) => {
                self.bump()?;
                Ok((
                    Expr::Lit {
                        value: Literal::Symbol(s),
                        span,
                    },
                    false,
                ))
            }
            Tok::LitArrayOpen => {
                let lit = self.parse_array_literal()?;
                Ok((Expr::Lit { value: lit, span }, false))
            }
            Tok::ByteArrayOpen => {
                let lit = self.parse_byte_array_literal()?;
                Ok((Expr::Lit { value: lit, span }, false))
            }
            _ => Err(self.error(span, "expected an expression")),
        }
    }

    fn parse_unary_chain(
        &mut self,
        mut e: Expr,
        is_super: &mut bool,
    ) -> Result<Expr, CompileError> {
        while let Tok::Ident(name) = self.cur.0.clone() {
            let span = self.cur.1;
            self.bump()?;
            e = Expr::Send {
                receiver: Box::new(e),
                selector: name,
                args: vec![],
                is_super: *is_super,
                span,
            };
            *is_super = false;
        }
        Ok(e)
    }

    fn parse_binary_chain(
        &mut self,
        base: Expr,
        is_super: &mut bool,
    ) -> Result<Expr, CompileError> {
        let mut e = self.parse_unary_chain(base, is_super)?;
        loop {
            let sel = match &self.cur.0 {
                Tok::BinarySel(s) => s.clone(),
                Tok::VBar => "|".to_string(),
                _ => break,
            };
            let span = self.cur.1;
            self.bump()?;
            let (rhs_primary, mut rhs_is_super) = self.parse_primary()?;
            let rhs = self.parse_unary_chain(rhs_primary, &mut rhs_is_super)?;
            e = Expr::Send {
                receiver: Box::new(e),
                selector: sel,
                args: vec![rhs],
                is_super: *is_super,
                span,
            };
            *is_super = false;
        }
        Ok(e)
    }

    /// `keyword_expr`/`message_segment`'s shared tail: binary chain then an
    /// optional single keyword message.
    fn parse_keyword_chain(
        &mut self,
        base: Expr,
        is_super: &mut bool,
    ) -> Result<Expr, CompileError> {
        let mut e = self.parse_binary_chain(base, is_super)?;
        if let Tok::Keyword(_) = &self.cur.0 {
            let span = self.cur.1;
            let mut sel = String::new();
            let mut args = Vec::new();
            while let Tok::Keyword(k) = self.cur.0.clone() {
                sel.push_str(&k);
                self.bump()?;
                let (arg_primary, mut arg_is_super) = self.parse_primary()?;
                let arg = self.parse_binary_chain(arg_primary, &mut arg_is_super)?;
                args.push(arg);
            }
            e = Expr::Send {
                receiver: Box::new(e),
                selector: sel,
                args,
                is_super: *is_super,
                span,
            };
            *is_super = false;
        }
        Ok(e)
    }

    fn parse_keyword_expr(&mut self) -> Result<Expr, CompileError> {
        let start = self.cur.1;
        let (base, mut is_super) = self.parse_primary()?;
        let e = self.parse_keyword_chain(base, &mut is_super)?;
        if is_super {
            // `is_super` only starts true for a `super` primary, and only
            // clears once some message is applied atop it — still true
            // here means `super` appeared with no message at all (bare
            // `^super` / `x := super`, both parse errors per the grammar).
            return Err(self.error(start, "'super' must be the receiver of a message send"));
        }
        Ok(e)
    }

    fn parse_cascade(&mut self) -> Result<Expr, CompileError> {
        let cascade_span = self.cur.1;
        let first = self.parse_keyword_expr()?;
        if !matches!(self.cur.0, Tok::Semi) {
            return Ok(first);
        }
        let (receiver, is_super, selector, args, send_span) = match first {
            Expr::Send {
                receiver,
                is_super,
                selector,
                args,
                span,
            } => (*receiver, is_super, selector, args, span),
            other => {
                return Err(self.error(other.span(), "cascade requires a message send"));
            }
        };
        let rcvr_span = receiver.span();
        let mut segments = vec![Expr::Send {
            receiver: Box::new(Expr::CascadeRcvr(rcvr_span)),
            selector,
            args,
            is_super,
            span: send_span,
        }];
        while matches!(self.cur.0, Tok::Semi) {
            self.bump()?;
            let mut seg_is_super = is_super;
            let base = Expr::CascadeRcvr(rcvr_span);
            let seg_start = self.cur.1;
            let seg = self.parse_keyword_chain(base, &mut seg_is_super)?;
            if matches!(seg, Expr::CascadeRcvr(_)) {
                return Err(self.error(seg_start, "cascade segment has no message"));
            }
            segments.push(seg);
        }
        Ok(Expr::Cascade {
            receiver: Box::new(receiver),
            segments,
            span: cascade_span,
        })
    }

    fn parse_expression(&mut self) -> Result<Expr, CompileError> {
        if let Tok::Ident(name) = self.cur.0.clone() {
            if matches!(self.peek2()?, Tok::Assign) {
                let span = self.cur.1;
                self.check_not_reserved(span, &name)?;
                self.bump()?; // ident
                self.bump()?; // :=
                let value = self.parse_expression()?;
                return Ok(Expr::Assign {
                    name,
                    value: Box::new(value),
                    span,
                });
            }
        }
        self.parse_cascade()
    }

    fn parse_statement(&mut self) -> Result<Expr, CompileError> {
        if matches!(self.cur.0, Tok::Caret) {
            let span = self.cur.1;
            self.bump()?;
            let value = self.parse_expression()?;
            Ok(Expr::Return {
                value: Box::new(value),
                span,
            })
        } else {
            self.parse_expression()
        }
    }

    fn parse_statements(&mut self) -> Result<Vec<Expr>, CompileError> {
        let mut stmts = Vec::new();
        if matches!(self.cur.0, Tok::RBracket | Tok::Eof) {
            return Ok(stmts);
        }
        loop {
            stmts.push(self.parse_statement()?);
            if matches!(self.cur.0, Tok::Period) {
                self.bump()?;
                if matches!(self.cur.0, Tok::RBracket | Tok::Eof) {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(stmts)
    }

    // --- blocks -------------------------------------------------------------

    fn parse_block(&mut self) -> Result<BlockNode, CompileError> {
        let span = self.cur.1; // '['
        self.bump()?;
        let mut params = Vec::new();
        if matches!(self.cur.0, Tok::Colon) {
            while matches!(self.cur.0, Tok::Colon) {
                self.bump()?;
                let pspan = self.cur.1;
                let name = self.expect_ident("expected a parameter name after ':'")?;
                self.check_not_reserved(pspan, &name)?;
                params.push(name);
            }
            self.expect(&Tok::VBar, "expected '|' after block parameters")?;
        }
        let temps = self.parse_optional_temps()?;
        let body = self.parse_statements()?;
        self.expect(&Tok::RBracket, "expected ']' to close block")?;
        Ok(BlockNode {
            params,
            temps,
            body,
            span,
            scope_id: 0,
        })
    }

    fn parse_optional_temps(&mut self) -> Result<Vec<String>, CompileError> {
        let mut temps = Vec::new();
        if matches!(self.cur.0, Tok::VBar) {
            self.bump()?;
            while let Tok::Ident(n) = self.cur.0.clone() {
                self.check_not_reserved(self.cur.1, &n)?;
                temps.push(n);
                self.bump()?;
            }
            self.expect(&Tok::VBar, "expected closing '|' after temporaries")?;
        }
        Ok(temps)
    }

    // --- pragmas --------------------------------------------------------------

    fn skip_pragma_body(&mut self, mut depth: u32) -> Result<(), CompileError> {
        loop {
            match &self.cur.0 {
                Tok::BinarySel(s) if s == "<" => {
                    depth += 1;
                    self.bump()?;
                }
                Tok::BinarySel(s) if s == ">" => {
                    depth -= 1;
                    if depth == 0 {
                        self.lx.set_pragma_mode(false);
                    }
                    self.bump()?;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                Tok::Eof => return Err(self.error(self.cur.1, "unterminated pragma")),
                _ => self.bump()?,
            }
        }
    }

    /// Method-body pragma (`sprint_s05_detail.md` §Algorithms "Pragmas"
    /// rule 1/2). Returns `(Some(id), None)` for a recognized
    /// `<primitive: N>`, or `(None, Some(ffi))` for S20's `<primitive: FFI
    /// …>` (docs/FFI.md §6.3) — a keyword-bodied pragma, structurally
    /// distinct from the bare-integer form, recognized the instant the
    /// token right after `primitive:` is the identifier `FFI` rather than
    /// an integer literal.
    fn parse_method_pragma(&mut self) -> Result<(Option<u16>, Option<FfiPragma>), CompileError> {
        let start = self.cur.1;
        self.lx.set_pragma_mode(true);
        self.bump()?; // '<'
        if let Tok::Keyword(k) = self.cur.0.clone() {
            if k == "primitive:" {
                self.bump()?;
                if matches!(&self.cur.0, Tok::Ident(id) if id == "FFI") {
                    self.bump()?;
                    let ffi = self.parse_ffi_pragma_body(start)?;
                    return Ok((None, Some(ffi)));
                }
                let (negative, radix, digits) = match self.cur.0.clone() {
                    Tok::IntLit {
                        negative,
                        radix,
                        digits,
                    } => (negative, radix, digits),
                    _ => return Err(self.error(start, "expected an integer after <primitive:")),
                };
                self.bump()?;
                if !matches!(&self.cur.0, Tok::BinarySel(s) if s == ">") {
                    return Err(self.error(start, "expected '>' to close <primitive: N>"));
                }
                self.lx.set_pragma_mode(false);
                self.bump()?;
                if negative {
                    return Err(self.error(start, "primitive id must not be negative"));
                }
                let mag = int_lit_magnitude(radix as u32, &digits);
                let mut v: u64 = 0;
                for (i, &b) in mag.iter().enumerate().take(8) {
                    v |= (b as u64) << (8 * i);
                }
                if mag.len() > 8 || v == 0 || v > u16::MAX as u64 {
                    return Err(self.error(start, "primitive id out of range 1..=65535"));
                }
                return Ok((Some(v as u16), None));
            }
        }
        self.skip_pragma_body(1)?;
        Ok((None, None))
    }

    /// S20 FFI: the body of `<primitive: FFI …>`, cursor already past the
    /// `FFI` identifier, still in pragma mode. A flat run of keyword:value
    /// pairs (`function:`/`selector:`/`class:`/`classSide:`/`ret:`/
    /// `args:`) until the closing `>` — order-independent, each recognized
    /// once (a repeat is a hard error, matching `parse_method_body`'s own
    /// "duplicate primitive pragma" posture one level up).
    fn parse_ffi_pragma_body(&mut self, start: Span) -> Result<FfiPragma, CompileError> {
        let mut function: Option<String> = None;
        let mut selector: Option<String> = None;
        let mut class: Option<String> = None;
        let mut class_side: Option<bool> = None;
        let mut ret: Option<String> = None;
        let mut args: Option<Vec<String>> = None;

        loop {
            if matches!(&self.cur.0, Tok::BinarySel(s) if s == ">") {
                break;
            }
            let Tok::Keyword(k) = self.cur.0.clone() else {
                return Err(self.error(
                    self.cur.1,
                    "expected a keyword part (function:/selector:/class:/classSide:/ret:/args:) \
                     in <primitive: FFI …>",
                ));
            };
            self.bump()?;
            macro_rules! dup_check {
                ($slot:expr, $name:literal) => {
                    if $slot.is_some() {
                        return Err(
                            self.error(start, concat!("duplicate '", $name, "' in FFI pragma"))
                        );
                    }
                };
            }
            match k.as_str() {
                "function:" => {
                    dup_check!(function, "function:");
                    function = Some(self.expect_ffi_sym("function:")?);
                }
                "selector:" => {
                    dup_check!(selector, "selector:");
                    selector = Some(self.expect_ffi_sym("selector:")?);
                }
                "class:" => {
                    dup_check!(class, "class:");
                    class = Some(self.expect_ffi_sym("class:")?);
                }
                "classSide:" => {
                    dup_check!(class_side, "classSide:");
                    class_side = Some(self.expect_ffi_bool("classSide:")?);
                }
                "ret:" => {
                    dup_check!(ret, "ret:");
                    ret = Some(self.expect_ffi_sym("ret:")?);
                }
                "args:" => {
                    dup_check!(args, "args:");
                    args = Some(self.expect_ffi_sym_array("args:")?);
                }
                other => {
                    return Err(self.error(
                        start,
                        format!("unknown keyword '{other}' in <primitive: FFI …>"),
                    ))
                }
            }
        }
        self.lx.set_pragma_mode(false);
        self.bump()?; // '>'

        let ret = ret.ok_or_else(|| self.error(start, "FFI pragma missing 'ret:'"))?;
        let args = args.unwrap_or_default();

        match (function, selector) {
            (Some(name), None) => Ok(FfiPragma::Function { name, ret, args }),
            (None, Some(selector)) => {
                let class = class
                    .ok_or_else(|| self.error(start, "FFI 'selector:' pragma missing 'class:'"))?;
                Ok(FfiPragma::Selector {
                    selector,
                    class,
                    class_side: class_side.unwrap_or(false),
                    ret,
                    args,
                })
            }
            (Some(_), Some(_)) => Err(self.error(
                start,
                "FFI pragma must not specify both 'function:' (Tier 1) and 'selector:' (Tier 2)",
            )),
            (None, None) => Err(self.error(
                start,
                "FFI pragma needs 'function:' (Tier 1, POSIX) or 'selector:' (Tier 2, Cocoa)",
            )),
        }
    }

    /// A bare symbol literal (`#foo`, `#foo:bar:`) — every FFI pragma value
    /// EXCEPT `classSide:`/`args:` is one of these.
    fn expect_ffi_sym(&mut self, ctx: &str) -> Result<String, CompileError> {
        match self.cur.0.clone() {
            Tok::SymLit(s) => {
                self.bump()?;
                Ok(s)
            }
            _ => Err(self.error(
                self.cur.1,
                format!("expected a symbol literal (#foo) after '{ctx}'"),
            )),
        }
    }

    fn expect_ffi_bool(&mut self, ctx: &str) -> Result<bool, CompileError> {
        match self.cur.0.clone() {
            Tok::Ident(s) if s == "true" => {
                self.bump()?;
                Ok(true)
            }
            Tok::Ident(s) if s == "false" => {
                self.bump()?;
                Ok(false)
            }
            _ => Err(self.error(
                self.cur.1,
                format!("expected 'true' or 'false' after '{ctx}'"),
            )),
        }
    }

    /// `#(g g g)`-shaped: a literal array of bare shape-token symbols.
    /// Reuses [`Self::parse_array_literal`] verbatim (the SAME grammar an
    /// ordinary method-body `#(…)` expression uses — pragma mode and
    /// literal-array mode are independent lexer flags, so nesting one
    /// inside the other is not a new lexer case) rather than re-deriving
    /// array-literal parsing here.
    fn expect_ffi_sym_array(&mut self, ctx: &str) -> Result<Vec<String>, CompileError> {
        if !matches!(self.cur.0, Tok::LitArrayOpen) {
            return Err(self.error(self.cur.1, format!("expected '#(' after '{ctx}'")));
        }
        let span = self.cur.1;
        let Literal::Array(elems) = self.parse_array_literal()? else {
            unreachable!("parse_array_literal always returns Literal::Array")
        };
        elems
            .into_iter()
            .map(|e| match e {
                Literal::Symbol(s) => Ok(s),
                _ => Err(self.error(
                    span,
                    format!("'{ctx}' array elements must be bare shape symbols (g, f, h4, …)"),
                )),
            })
            .collect()
    }

    fn parse_class_pragma(&mut self) -> Result<ClassPragma, CompileError> {
        let start = self.cur.1;
        self.lx.set_pragma_mode(true);
        self.bump()?; // '<'
        if let Tok::Keyword(k) = self.cur.0.clone() {
            if k == "indexable:" {
                self.bump()?;
                if let Tok::Ident(kind) = self.cur.0.clone() {
                    self.bump()?;
                    if matches!(&self.cur.0, Tok::BinarySel(s) if s == ">") {
                        self.lx.set_pragma_mode(false);
                        self.bump()?;
                        return match kind.as_str() {
                            "oops" => Ok(ClassPragma::Indexable(Indexable::Oops)),
                            "bytes" => Ok(ClassPragma::Indexable(Indexable::Bytes)),
                            _ => Err(self.error(
                                start,
                                format!(
                                    "<indexable:> argument must be 'oops' or 'bytes', got '{kind}'"
                                ),
                            )),
                        };
                    }
                }
                return Err(self.error(start, "malformed <indexable:> pragma"));
            } else if k == "classVars:" {
                self.bump()?;
                let mut names = Vec::new();
                while let Tok::Ident(n) = self.cur.0.clone() {
                    names.push(n);
                    self.bump()?;
                }
                if matches!(&self.cur.0, Tok::BinarySel(s) if s == ">") {
                    self.lx.set_pragma_mode(false);
                    self.bump()?;
                    return Ok(ClassPragma::ClassVars(names));
                }
                return Err(self.error(start, "malformed <classVars:> pragma"));
            }
        }
        self.skip_pragma_body(1)?;
        Ok(ClassPragma::Ignored)
    }

    // --- methods --------------------------------------------------------------

    fn parse_pattern(&mut self) -> Result<(String, Vec<String>), CompileError> {
        match self.cur.0.clone() {
            Tok::Ident(name) => {
                self.bump()?;
                Ok((name, vec![]))
            }
            Tok::BinarySel(sel) => {
                self.bump()?;
                let pspan = self.cur.1;
                let p = self.expect_ident("expected a parameter name after binary selector")?;
                self.check_not_reserved(pspan, &p)?;
                Ok((sel, vec![p]))
            }
            Tok::VBar => {
                self.bump()?;
                let pspan = self.cur.1;
                let p = self.expect_ident("expected a parameter name after '|'")?;
                self.check_not_reserved(pspan, &p)?;
                Ok(("|".to_string(), vec![p]))
            }
            Tok::Keyword(_) => {
                let mut sel = String::new();
                let mut params = Vec::new();
                while let Tok::Keyword(k) = self.cur.0.clone() {
                    sel.push_str(&k);
                    self.bump()?;
                    let pspan = self.cur.1;
                    let p = self.expect_ident("expected a parameter name after keyword")?;
                    self.check_not_reserved(pspan, &p)?;
                    params.push(p);
                }
                Ok((sel, params))
            }
            _ => Err(self.error(self.cur.1, "invalid method pattern")),
        }
    }

    fn parse_method_body(&mut self) -> Result<MethodBody, CompileError> {
        let mut primitive = None;
        let mut ffi = None;
        while matches!(&self.cur.0, Tok::BinarySel(s) if s == "<") {
            let prim_span = self.cur.1;
            let (id, pragma_ffi) = self.parse_method_pragma()?;
            if id.is_some() || pragma_ffi.is_some() {
                if primitive.is_some() || ffi.is_some() {
                    return Err(self.error(prim_span, "duplicate primitive pragma"));
                }
                primitive = id;
                ffi = pragma_ffi;
            }
        }
        let temps = self.parse_optional_temps()?;
        let body = self.parse_statements()?;
        Ok((primitive, ffi, temps, body))
    }

    fn parse_method(&mut self, class_side: bool) -> Result<MethodNode, CompileError> {
        let span = self.cur.1;
        let (pattern_selector, params) = self.parse_pattern()?;
        self.expect(&Tok::LBracket, "expected '[' to start method body")?;
        let (primitive, ffi, temps, body) = self.parse_method_body()?;
        self.expect(&Tok::RBracket, "expected ']' to close method body")?;
        Ok(MethodNode {
            pattern_selector,
            params,
            primitive,
            ffi,
            temps,
            body,
            class_side,
            span,
        })
    }

    // --- class definitions ------------------------------------------------

    fn parse_class_def(&mut self) -> Result<ClassDefNode, CompileError> {
        let span = self.cur.1;
        let superclass = self.expect_ident("expected superclass name")?;
        self.expect_keyword("subclass:")?;
        let name = self.expect_ident("expected new class name")?;
        self.expect(&Tok::LBracket, "expected '[' to start class body")?;

        let mut indexable = None;
        let mut inst_vars = Vec::new();
        let mut class_vars = Vec::new();
        let mut methods = Vec::new();

        loop {
            match self.cur.0.clone() {
                Tok::RBracket => break,
                Tok::Eof => {
                    return Err(self.error(self.cur.1, "unexpected end of input in class body"))
                }
                Tok::BinarySel(s) if s == "<" => {
                    // Disambiguate a class pragma (`<indexable: ...>`,
                    // `<classVars: ...>`) from a binary method named `<`
                    // (e.g. `Magnitude>><`): both real pragma forms open
                    // with a keyword, so an ident right after `<` means
                    // this is a method pattern, not a pragma.
                    let is_binary_method = matches!(self.peek2()?, Tok::Ident(_));
                    if is_binary_method {
                        methods.push(self.parse_method(false)?);
                    } else {
                        match self.parse_class_pragma()? {
                            ClassPragma::Indexable(ind) => indexable = Some(ind),
                            ClassPragma::ClassVars(names) => class_vars.extend(names),
                            ClassPragma::Ignored => {}
                        }
                    }
                }
                Tok::VBar => {
                    self.bump()?;
                    let is_binary_method = matches!(self.cur.0, Tok::Ident(_))
                        && matches!(self.peek2()?, Tok::LBracket);
                    if is_binary_method {
                        let mspan = self.cur.1;
                        let param = self.expect_ident("expected a parameter name")?;
                        self.check_not_reserved(mspan, &param)?;
                        self.expect(&Tok::LBracket, "expected '['")?;
                        let (primitive, ffi, temps, body) = self.parse_method_body()?;
                        self.expect(&Tok::RBracket, "expected ']'")?;
                        methods.push(MethodNode {
                            pattern_selector: "|".to_string(),
                            params: vec![param],
                            primitive,
                            ffi,
                            temps,
                            body,
                            class_side: false,
                            span: mspan,
                        });
                    } else {
                        let mut names = Vec::new();
                        while let Tok::Ident(n) = self.cur.0.clone() {
                            self.check_not_reserved(self.cur.1, &n)?;
                            names.push(n);
                            self.bump()?;
                        }
                        self.expect(&Tok::VBar, "expected closing '|' after instance variables")?;
                        inst_vars.extend(names);
                    }
                }
                Tok::Ident(ident_name) => {
                    let is_class_method = matches!(self.peek2()?, Tok::Ident(n) if n == "class");
                    if is_class_method {
                        let mspan = self.cur.1;
                        self.bump()?; // class name ident
                        self.bump()?; // "class"
                        if ident_name != name {
                            return Err(self.error(
                                mspan,
                                format!(
                                    "'{ident_name} class' does not match the class being defined ('{name}')"
                                ),
                            ));
                        }
                        if !matches!(&self.cur.0, Tok::BinarySel(s) if s == ">>") {
                            return Err(self.error(self.cur.1, "expected '>>' after 'class'"));
                        }
                        self.bump()?; // '>>'
                        methods.push(self.parse_method(true)?);
                    } else {
                        methods.push(self.parse_method(false)?);
                    }
                }
                Tok::BinarySel(_) | Tok::Keyword(_) => {
                    methods.push(self.parse_method(false)?);
                }
                _ => return Err(self.error(self.cur.1, "expected a class item")),
            }
        }
        self.bump()?; // ']'
        Ok(ClassDefNode {
            superclass,
            name,
            indexable,
            inst_vars,
            class_vars,
            methods,
            span,
        })
    }
}

/// Parses one whole `.mst` file into its top-level items (SPEC §1's
/// `file` production), in source order. `do_it` statements are NOT
/// executed here — `frontend::classdef`/`world` drive execution.
pub fn parse_file(input: &str) -> Result<Vec<TopItem>, CompileError> {
    let mut p = Parser::new(input)?;
    let mut items = Vec::new();
    while !matches!(p.cur.0, Tok::Eof) {
        let is_class_def = matches!(&p.cur.0, Tok::Ident(_))
            && matches!(p.peek2()?, Tok::Keyword(k) if k == "subclass:");
        if is_class_def {
            items.push(TopItem::ClassDef(p.parse_class_def()?));
        } else {
            let stmt = p.parse_statement()?;
            p.expect(&Tok::Period, "expected '.' after top-level statement")?;
            items.push(TopItem::DoIt(stmt));
        }
    }
    Ok(items)
}

/// Parses a single top-level item (one class def or one dot-terminated
/// statement) — the REPL's per-line/per-chunk entry point. Returns `None`
/// (input was ONLY trailing whitespace/comments) rather than erroring.
pub fn parse_one_top_item(input: &str) -> Result<Option<TopItem>, CompileError> {
    let mut p = Parser::new(input)?;
    if matches!(p.cur.0, Tok::Eof) {
        return Ok(None);
    }
    let is_class_def = matches!(&p.cur.0, Tok::Ident(_))
        && matches!(p.peek2()?, Tok::Keyword(k) if k == "subclass:");
    let item = if is_class_def {
        TopItem::ClassDef(p.parse_class_def()?)
    } else {
        let stmt = p.parse_statement()?;
        p.expect(&Tok::Period, "expected '.' after statement")?;
        TopItem::DoIt(stmt)
    };
    Ok(Some(item))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_expr(src: &str) -> Expr {
        let items = parse_file(&format!("{src}.")).expect("parse failed");
        match items.into_iter().next().unwrap() {
            TopItem::DoIt(e) => e,
            _ => panic!("expected a doIt"),
        }
    }

    fn sel_of(e: &Expr) -> &str {
        match e {
            Expr::Send { selector, .. } => selector,
            _ => panic!("not a Send: {e:?}"),
        }
    }

    #[test]
    fn parse_precedence() {
        // 1 + 2 * 3 max: 4 foo -> keyword(binary(binary(1+2)*3), max:, unary(4 foo))
        let e = parse_expr("1 + 2 * 3 max: 4 foo");
        assert_eq!(sel_of(&e), "max:");
        let Expr::Send { receiver, args, .. } = &e else {
            unreachable!()
        };
        assert_eq!(sel_of(receiver), "*");
        let Expr::Send {
            receiver: inner_r, ..
        } = receiver.as_ref()
        else {
            unreachable!()
        };
        assert_eq!(sel_of(inner_r), "+");
        assert_eq!(sel_of(&args[0]), "foo");
    }

    #[test]
    fn parse_assign_chain() {
        let e = parse_expr("a := b := 3");
        match e {
            Expr::Assign { name, value, .. } => {
                assert_eq!(name, "a");
                match *value {
                    Expr::Assign { name, value, .. } => {
                        assert_eq!(name, "b");
                        assert_eq!(
                            *value,
                            Expr::Lit {
                                value: Literal::Int(3),
                                span: value.span()
                            }
                        );
                    }
                    _ => panic!("expected nested assign"),
                }
            }
            _ => panic!("expected assign"),
        }
    }

    #[test]
    fn parse_cascade_rcvr() {
        let e = parse_expr("x add: 1; add: 2");
        let Expr::Cascade {
            receiver, segments, ..
        } = e
        else {
            panic!("expected cascade")
        };
        assert_eq!(
            *receiver,
            Expr::Var {
                name: "x".into(),
                span: receiver.span()
            }
        );
        assert_eq!(segments.len(), 2);
    }

    #[test]
    fn parse_cascade_paren() {
        let e = parse_expr("(d at: 1) foo; bar");
        let Expr::Cascade { receiver, .. } = e else {
            panic!("expected cascade")
        };
        assert_eq!(sel_of(&receiver), "at:");
    }

    #[test]
    fn parse_cascade_segment_chain() {
        let e = parse_expr("x foo; bar baz: 1");
        let Expr::Cascade { segments, .. } = e else {
            panic!("expected cascade")
        };
        assert_eq!(sel_of(&segments[1]), "baz:");
    }

    #[test]
    fn parse_cascade_nonsend() {
        let items = parse_file("3; foo.");
        assert!(items.is_err());
    }

    #[test]
    fn parse_caret_positions() {
        let _ = parse_expr("x foo; bar"); // sanity: cascades otherwise parse
        assert!(parse_file("x foo: ^y.").is_err());
        let e = parse_expr("[:a | ^a]");
        assert!(matches!(e, Expr::Block(_)));
    }

    #[test]
    fn parse_super() {
        let e = parse_expr("super foo");
        let Expr::Send { is_super, .. } = e else {
            panic!()
        };
        assert!(is_super);
        assert!(parse_file("^super.").is_err());
        let e2 = parse_expr("super foo; bar");
        let Expr::Cascade { segments, .. } = e2 else {
            panic!()
        };
        for s in &segments {
            assert!(matches!(s, Expr::Send { is_super: true, .. }));
        }
    }

    #[test]
    fn parse_block_headers() {
        let e = parse_expr("[ ]");
        assert!(matches!(e, Expr::Block(_)));
        let e = parse_expr("[ | t | t ]");
        let Expr::Block(b) = e else { panic!() };
        assert_eq!(b.temps, vec!["t".to_string()]);
        let e = parse_expr("[:x | x]");
        let Expr::Block(b) = e else { panic!() };
        assert_eq!(b.params, vec!["x".to_string()]);
        let e = parse_expr("[:x :y | | t | t]");
        let Expr::Block(b) = e else { panic!() };
        assert_eq!(b.params, vec!["x".to_string(), "y".to_string()]);
        assert_eq!(b.temps, vec!["t".to_string()]);
    }

    #[test]
    fn parse_litarray_nested() {
        let e = parse_expr("#(1 (2 3) #(4) #[5] foo bar: nil true)");
        let Expr::Lit {
            value: Literal::Array(items),
            ..
        } = e
        else {
            panic!()
        };
        assert_eq!(items[0], Literal::Int(1));
        assert_eq!(
            items[1],
            Literal::Array(vec![Literal::Int(2), Literal::Int(3)])
        );
        assert_eq!(items[2], Literal::Array(vec![Literal::Int(4)]));
        assert_eq!(items[3], Literal::ByteArray(vec![5]));
        assert_eq!(items[4], Literal::Symbol("foo".into()));
        assert_eq!(items[5], Literal::Symbol("bar:".into()));
        assert_eq!(items[6], Literal::Nil);
        assert_eq!(items[7], Literal::True);
    }

    #[test]
    fn parse_keyword_symbol_glue() {
        let e = parse_expr("#(bar:baz:)");
        let Expr::Lit {
            value: Literal::Array(items),
            ..
        } = e
        else {
            panic!()
        };
        assert_eq!(items, vec![Literal::Symbol("bar:baz:".into())]);

        let e2 = parse_expr("#(bar: baz:)");
        let Expr::Lit {
            value: Literal::Array(items2),
            ..
        } = e2
        else {
            panic!()
        };
        assert_eq!(
            items2,
            vec![
                Literal::Symbol("bar:".into()),
                Literal::Symbol("baz:".into())
            ]
        );
    }

    #[test]
    fn parse_bytearray_range() {
        let e = parse_expr("#[0 255 16rFF]");
        assert_eq!(
            e,
            Expr::Lit {
                value: Literal::ByteArray(vec![0, 255, 255]),
                span: e.span()
            }
        );
        assert!(parse_file("#[256].").is_err());
        assert!(parse_file("#[-1].").is_err());
        assert!(parse_file("#[foo].").is_err());
    }

    #[test]
    fn parse_pragma_prim() {
        let items = parse_file("Object subclass: X [ foo [ <primitive: 7> ^1 ] ]").unwrap();
        let TopItem::ClassDef(c) = &items[0] else {
            panic!()
        };
        assert_eq!(c.methods[0].primitive, Some(7));

        let dup = parse_file("Object subclass: X [ foo [ <primitive: 7> <primitive: 8> ^1 ] ]");
        assert!(dup.is_err());
    }

    // S20 FFI (docs/FFI.md §6.3) — `<primitive: FFI …>` parsing.

    #[test]
    fn parse_ffi_pragma_tier1_function() {
        // docs/FFI.md §6.3's real generated FFIPosix>>mmapAddr:... shape,
        // verbatim (6 `g` args, `g` return).
        let items = parse_file(
            "Object subclass: X [ \
                mmapAddr: a1 length: a2 prot: a3 flags: a4 fd: a5 offset: a6 [ \
                    <primitive: FFI function: #mmap ret: #g args: #(g g g g g g)> \
                ] \
            ]",
        )
        .unwrap();
        let TopItem::ClassDef(c) = &items[0] else {
            panic!()
        };
        let m = &c.methods[0];
        assert_eq!(m.primitive, None);
        match &m.ffi {
            Some(FfiPragma::Function { name, ret, args }) => {
                assert_eq!(name, "mmap");
                assert_eq!(ret, "g");
                assert_eq!(args, &vec!["g", "g", "g", "g", "g", "g"]);
            }
            other => panic!("expected FfiPragma::Function, got {other:?}"),
        }
        assert_eq!(m.body.len(), 0, "the pragma is the whole body — no ^expr");
    }

    #[test]
    fn parse_ffi_pragma_tier2_selector() {
        // docs/FFI.md §6.3's real generated NSColorAlien shape, verbatim
        // (4 `f` args, `g` return, classSide: true).
        let items = parse_file(
            "Object subclass: X [ \
                X class >> colorWithRed: a1 green: a2 blue: a3 alpha: a4 [ \
                    <primitive: FFI selector: #colorWithRed:green:blue:alpha: \
                        class: #NSColor classSide: true ret: #g args: #(f f f f)> \
                ] \
            ]",
        )
        .unwrap();
        let TopItem::ClassDef(c) = &items[0] else {
            panic!()
        };
        let m = &c.methods[0];
        match &m.ffi {
            Some(FfiPragma::Selector {
                selector,
                class,
                class_side,
                ret,
                args,
            }) => {
                assert_eq!(selector, "colorWithRed:green:blue:alpha:");
                assert_eq!(class, "NSColor");
                assert!(*class_side);
                assert_eq!(ret, "g");
                assert_eq!(args, &vec!["f", "f", "f", "f"]);
            }
            other => panic!("expected FfiPragma::Selector, got {other:?}"),
        }
    }

    /// `frame`'s real shape (docs/FFI.md §6.3): no `args:` at all, `ret: #h4`
    /// — `args` must default to empty rather than erroring, and a Tier 2
    /// pragma's own `classSide:` must default to `false` when omitted.
    #[test]
    fn parse_ffi_pragma_defaults_no_args_no_classside() {
        let items = parse_file(
            "Object subclass: X [ \
                frame [ <primitive: FFI selector: #frame class: #NSView ret: #h4> ] \
            ]",
        )
        .unwrap();
        let TopItem::ClassDef(c) = &items[0] else {
            panic!()
        };
        match &c.methods[0].ffi {
            Some(FfiPragma::Selector {
                class_side, args, ..
            }) => {
                assert!(!*class_side);
                assert!(args.is_empty());
            }
            other => panic!("expected FfiPragma::Selector, got {other:?}"),
        }
    }

    #[test]
    fn parse_ffi_pragma_missing_ret_is_an_error() {
        let err = parse_file(
            "Object subclass: X [ foo [ <primitive: FFI function: #getpid args: #()> ^1 ] ]",
        );
        assert!(err.is_err());
    }

    #[test]
    fn parse_ffi_pragma_both_function_and_selector_is_an_error() {
        let err = parse_file(
            "Object subclass: X [ foo [ \
                <primitive: FFI function: #getpid selector: #foo class: #Bar ret: #g> ^1 \
            ] ]",
        );
        assert!(err.is_err());
    }

    #[test]
    fn parse_ffi_pragma_neither_function_nor_selector_is_an_error() {
        let err = parse_file("Object subclass: X [ foo [ <primitive: FFI ret: #g> ^1 ] ]");
        assert!(err.is_err());
    }

    #[test]
    fn parse_ffi_pragma_selector_without_class_is_an_error() {
        let err =
            parse_file("Object subclass: X [ foo [ <primitive: FFI selector: #foo ret: #g> ^1 ] ]");
        assert!(err.is_err());
    }

    #[test]
    fn parse_ffi_pragma_args_element_must_be_a_symbol() {
        let err = parse_file(
            "Object subclass: X [ foo [ \
                <primitive: FFI function: #mmap ret: #g args: #(1 2)> ^1 \
            ] ]",
        );
        assert!(err.is_err());
    }

    #[test]
    fn parse_ffi_pragma_never_collides_with_plain_primitive_pragma() {
        // Two DIFFERENT method bodies, one of each kind, in the SAME class —
        // proves parse_method_pragma's two return arms stay independent.
        let items = parse_file(
            "Object subclass: X [ \
                plain [ <primitive: 7> ^1 ] \
                ffi [ <primitive: FFI function: #getpid ret: #g> ] \
            ]",
        )
        .unwrap();
        let TopItem::ClassDef(c) = &items[0] else {
            panic!()
        };
        assert_eq!(c.methods[0].primitive, Some(7));
        assert!(c.methods[0].ffi.is_none());
        assert!(c.methods[1].primitive.is_none());
        assert!(matches!(c.methods[1].ffi, Some(FfiPragma::Function { .. })));
    }

    #[test]
    fn parse_pragma_skip() {
        let items =
            parse_file("Object subclass: X [ foo [ <:T> <pre: x foo bla> <Array[Int]> ^1 ] ]")
                .unwrap();
        let TopItem::ClassDef(c) = &items[0] else {
            panic!()
        };
        assert_eq!(c.methods[0].primitive, None);
        assert_eq!(c.methods[0].body.len(), 1);
    }

    #[test]
    fn parse_classdef_items() {
        let src = "Object subclass: Point [ \
            <indexable: bytes> \
            | x y | \
            Point class >> x: ax y: ay [ ^self new ] \
            x [ ^x ] \
            y [ ^y ] \
            + p [ ^x ] \
            printOn: aStream [ ^self ] \
            ]";
        let items = parse_file(src).unwrap();
        let TopItem::ClassDef(c) = &items[0] else {
            panic!()
        };
        assert_eq!(c.superclass, "Object");
        assert_eq!(c.name, "Point");
        assert_eq!(c.indexable, Some(Indexable::Bytes));
        assert_eq!(c.inst_vars, vec!["x".to_string(), "y".to_string()]);
        assert_eq!(c.methods.len(), 5);
        assert!(c.methods[0].class_side);
    }

    #[test]
    fn parse_vbar_method() {
        let items = parse_file("Object subclass: X [ | arg [ ^arg ] ]").unwrap();
        let TopItem::ClassDef(c) = &items[0] else {
            panic!()
        };
        assert_eq!(c.methods[0].pattern_selector, "|");
        assert_eq!(c.methods[0].params, vec!["arg".to_string()]);

        let items2 = parse_file("Object subclass: X [ | a b | ]").unwrap();
        let TopItem::ClassDef(c2) = &items2[0] else {
            panic!()
        };
        assert_eq!(c2.inst_vars, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn parse_reserved() {
        assert!(parse_file("self := 3.").is_err());
        assert!(parse_file("Object subclass: X [ foo [ | nil | ^1 ] ]").is_err());
    }
}
