//! Hand-written lexer (SPEC §1, `sprint_s05_detail.md` §Design "Tokens"/
//! "Lexer rules L1–L10", verbatim rule numbers referenced in comments below).
//! `col` is 1-based and counted in **bytes**, not chars (multi-byte UTF-8 in
//! comments/strings/chars does not skew later columns). Two lexer modes are
//! driven externally by the parser: `in_literal_seq` (L5a, inside `#( … )`/
//! `#[ … ]`) and `pragma_mode` (L10, inside `< … >`).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub line: u32,
    pub col: u32,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Tok {
    Ident(String),
    Keyword(String),
    BinarySel(String),
    /// Raw digit text, no sign — L5's negative-literal rule lives at the
    /// lexer's dispatch point, not inside this variant.
    IntLit {
        negative: bool,
        radix: u8,
        digits: String,
    },
    FloatLit(f64),
    CharLit(char),
    StrLit(String),
    SymLit(String),
    LitArrayOpen,
    ByteArrayOpen,
    LParen,
    RParen,
    LBracket,
    RBracket,
    VBar,
    Semi,
    Period,
    Caret,
    Assign,
    Colon,
    Eof,
}

#[derive(Clone, Debug)]
pub struct LexError {
    pub span: Span,
    pub msg: String,
    /// Set when the failure is fundamentally "ran out of input" (an
    /// unterminated string/comment/char at EOF) rather than a malformed
    /// token — the REPL (`main.rs`) uses this to decide whether to keep
    /// buffering more lines instead of reporting an error.
    pub eof: bool,
}

const BINARY_CHARS: &str = "+-*/\\~<>=&|@%,?!";

fn is_binary_char(c: char) -> bool {
    BINARY_CHARS.contains(c)
}

fn tok_ends_expr(t: &Tok) -> bool {
    matches!(
        t,
        Tok::Ident(_)
            | Tok::IntLit { .. }
            | Tok::FloatLit(_)
            | Tok::CharLit(_)
            | Tok::StrLit(_)
            | Tok::SymLit(_)
            | Tok::RParen
            | Tok::RBracket
    )
}

pub struct Lexer<'a> {
    input: &'a str,
    pos: usize,
    line: u32,
    col: u32,
    prev_ends_expr: bool,
    pragma_mode: bool,
    in_literal_seq: bool,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Lexer<'a> {
        Lexer {
            input,
            pos: 0,
            line: 1,
            col: 1,
            prev_ends_expr: false,
            pragma_mode: false,
            in_literal_seq: false,
        }
    }

    /// L10: while set, a run of binary chars starting with `>` never merges
    /// a second char — every `>` in a pragma closes it, never `>=`/`>>`.
    pub fn set_pragma_mode(&mut self, v: bool) {
        self.pragma_mode = v;
    }

    /// L5a: while set, `-` immediately followed by a digit is ALWAYS a
    /// negative literal, regardless of the previous token.
    pub fn set_in_literal_seq(&mut self, v: bool) {
        self.in_literal_seq = v;
    }

    fn rest(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn peek_char(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn peek_char_at(&self, n: usize) -> Option<char> {
        self.rest().chars().nth(n)
    }

    fn here(&self) -> Span {
        Span {
            line: self.line,
            col: self.col,
        }
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek_char()?;
        self.pos += c.len_utf8();
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += c.len_utf8() as u32;
        }
        Some(c)
    }

    fn err(&self, span: Span, msg: impl Into<String>) -> LexError {
        LexError {
            span,
            msg: msg.into(),
            eof: false,
        }
    }

    fn err_eof(&self, span: Span, msg: impl Into<String>) -> LexError {
        LexError {
            span,
            msg: msg.into(),
            eof: true,
        }
    }

    /// L1: whitespace and `"…"` comments (embedded `""` = literal `"`).
    fn skip_trivia(&mut self) -> Result<(), LexError> {
        loop {
            match self.peek_char() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                Some('"') => {
                    let start = self.here();
                    self.bump();
                    loop {
                        match self.bump() {
                            None => {
                                return Err(self.err_eof(start, "unterminated comment"));
                            }
                            Some('"') => {
                                if self.peek_char() == Some('"') {
                                    self.bump(); // escaped "" -> literal "
                                } else {
                                    break;
                                }
                            }
                            Some(_) => {}
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn consume_while(&mut self, pred: impl Fn(char) -> bool) -> String {
        let mut s = String::new();
        while let Some(c) = self.peek_char() {
            if pred(c) {
                s.push(c);
                self.bump();
            } else {
                break;
            }
        }
        s
    }

    fn is_ident_start(c: char) -> bool {
        c.is_ascii_alphabetic() || c == '_'
    }
    fn is_ident_cont(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '_'
    }

    /// L2.
    fn lex_ident_or_keyword(&mut self) -> Tok {
        let ident = self.consume_while(Self::is_ident_cont);
        if self.peek_char() == Some(':') && self.peek_char_at(1) != Some('=') {
            self.bump();
            Tok::Keyword(format!("{ident}:"))
        } else {
            Tok::Ident(ident)
        }
    }

    fn consume_radix_digit_run(&mut self) -> String {
        self.consume_while(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
    }

    /// Consumes an optional `e|d [-] digits` exponent suffix, returning it
    /// normalized to `"e[-]digits"`, or `None` (consuming nothing) if the
    /// lookahead doesn't confirm a valid exponent.
    fn try_consume_exponent(&mut self) -> Result<Option<String>, LexError> {
        let marker = match self.peek_char() {
            Some(c @ ('e' | 'E' | 'd' | 'D')) => c,
            _ => return Ok(None),
        };
        let neg = self.peek_char_at(1) == Some('-');
        let first_digit_offset = if neg { 2 } else { 1 };
        if !self
            .peek_char_at(first_digit_offset)
            .is_some_and(|c| c.is_ascii_digit())
        {
            return Ok(None);
        }
        let _ = marker;
        self.bump(); // marker
        if neg {
            self.bump();
        }
        let digits = self.consume_while(|c| c.is_ascii_digit());
        Ok(Some(format!("e{}{digits}", if neg { "-" } else { "" })))
    }

    /// L3/L4: caller has already established the token starts with a digit
    /// (`self.peek_char()` is ascii digit) or a confirmed negative-literal
    /// `-`. `negative` is applied to the produced literal.
    fn lex_number(&mut self, negative: bool, start: Span) -> Result<Tok, LexError> {
        let lead = self.consume_while(|c| c.is_ascii_digit());

        if self.peek_char() == Some('r') {
            let radix_val: u32 = lead.parse().unwrap_or(0);
            if !(2..=36).contains(&radix_val) {
                return Err(self.err(start, format!("radix {radix_val} out of range 2..=36")));
            }
            self.bump(); // 'r'
            let rdigits = self.consume_radix_digit_run();
            if rdigits.is_empty() {
                return Err(self.err(start, "radix literal has no digits after 'r'"));
            }
            for c in rdigits.chars() {
                let d = c.to_digit(36).expect("consume_radix_digit_run: bad char");
                if d >= radix_val {
                    return Err(self.err(
                        start,
                        format!("digit '{c}' is not valid in radix {radix_val}"),
                    ));
                }
            }
            return Ok(Tok::IntLit {
                negative,
                radix: radix_val as u8,
                digits: rdigits,
            });
        }

        if self.peek_char() == Some('.') && self.peek_char_at(1).is_some_and(|c| c.is_ascii_digit())
        {
            self.bump(); // '.'
            let frac = self.consume_while(|c| c.is_ascii_digit());
            let mut text = format!("{lead}.{frac}");
            if let Some(exp) = self.try_consume_exponent()? {
                text.push_str(&exp);
            }
            let v: f64 = text
                .parse()
                .map_err(|_| self.err(start, format!("malformed float literal '{text}'")))?;
            return Ok(Tok::FloatLit(if negative { -v } else { v }));
        }

        if let Some(exp) = self.try_consume_exponent()? {
            let text = format!("{lead}{exp}");
            let v: f64 = text
                .parse()
                .map_err(|_| self.err(start, format!("malformed float literal '{text}'")))?;
            return Ok(Tok::FloatLit(if negative { -v } else { v }));
        }

        Ok(Tok::IntLit {
            negative,
            radix: 10,
            digits: lead,
        })
    }

    /// L6: `$` + exactly one Unicode scalar.
    fn lex_char_literal(&mut self, start: Span) -> Result<Tok, LexError> {
        self.bump(); // '$'
        match self.bump() {
            Some(c) => Ok(Tok::CharLit(c)),
            None => Err(self.err_eof(start, "'$' at end of input")),
        }
    }

    /// L8: `'…'`, embedded quote `''`.
    fn lex_string_literal(&mut self, start: Span) -> Result<Tok, LexError> {
        self.bump(); // opening '
        let mut s = String::new();
        loop {
            match self.bump() {
                None => return Err(self.err_eof(start, "unterminated string")),
                Some('\'') => {
                    if self.peek_char() == Some('\'') {
                        self.bump();
                        s.push('\'');
                    } else {
                        break;
                    }
                }
                Some(c) => s.push(c),
            }
        }
        Ok(Tok::StrLit(s))
    }

    /// Shared by string and quoted-symbol lexing (L8 rules).
    fn lex_quoted_text(&mut self, start: Span) -> Result<String, LexError> {
        match self.lex_string_literal(start)? {
            Tok::StrLit(s) => Ok(s),
            _ => unreachable!(),
        }
    }

    /// A run of up to 2 [`BINARY_CHARS`] (excluding `|`, which never
    /// merges — L7), starting with `first` (already peeked, not consumed).
    /// Never consumes a trailing `-` that is itself immediately followed by
    /// a digit: that `-` must stay available to independently resolve as a
    /// negative literal or a standalone operator (Pitfalls P1/P3 — see the
    /// module-level derivation in the sprint's own worked examples).
    fn lex_binary_sel(&mut self, first: char) -> Tok {
        self.bump();
        let mut s = String::new();
        s.push(first);
        let allow_merge = !(self.pragma_mode && first == '>');
        if allow_merge {
            if let Some(c2) = self.peek_char() {
                let mergeable = is_binary_char(c2) && c2 != '|';
                let steals_negative_literal =
                    c2 == '-' && self.peek_char_at(1).is_some_and(|c3| c3.is_ascii_digit());
                if mergeable && !steals_negative_literal {
                    s.push(c2);
                    self.bump();
                }
            }
        }
        Tok::BinarySel(s)
    }

    /// L9: `#` dispatch.
    fn lex_hash(&mut self, start: Span) -> Result<Tok, LexError> {
        self.bump(); // '#'
        match self.peek_char() {
            Some('(') => {
                self.bump();
                Ok(Tok::LitArrayOpen)
            }
            Some('[') => {
                self.bump();
                Ok(Tok::ByteArrayOpen)
            }
            Some('\'') => {
                let text = self.lex_quoted_text(start)?;
                Ok(Tok::SymLit(text))
            }
            Some(c) if Self::is_ident_start(c) => {
                let mut text = String::new();
                loop {
                    let part = self.consume_while(Self::is_ident_cont);
                    text.push_str(&part);
                    if self.peek_char() == Some(':') {
                        self.bump();
                        text.push(':');
                        if self.peek_char().is_some_and(Self::is_ident_start) {
                            continue;
                        }
                    }
                    break;
                }
                Ok(Tok::SymLit(text))
            }
            Some('|') => {
                self.bump();
                Ok(Tok::SymLit("|".to_string()))
            }
            Some(c) if is_binary_char(c) => match self.lex_binary_sel(c) {
                Tok::BinarySel(s) => Ok(Tok::SymLit(s)),
                _ => unreachable!(),
            },
            _ => Err(self.err(start, "invalid '#' sequence")),
        }
    }

    pub fn next_token(&mut self) -> Result<(Tok, Span), LexError> {
        self.skip_trivia()?;
        let start = self.here();
        let Some(c) = self.peek_char() else {
            return Ok((Tok::Eof, start));
        };

        let tok = if Self::is_ident_start(c) {
            self.lex_ident_or_keyword()
        } else if c.is_ascii_digit() {
            self.lex_number(false, start)?
        } else if c == '-' {
            let next_is_digit = self.peek_char_at(1).is_some_and(|c| c.is_ascii_digit());
            let starts_negative = next_is_digit && (self.in_literal_seq || !self.prev_ends_expr);
            if starts_negative {
                self.bump();
                self.lex_number(true, start)?
            } else {
                self.lex_binary_sel('-')
            }
        } else if c == '$' {
            self.lex_char_literal(start)?
        } else if c == '\'' {
            self.lex_string_literal(start)?
        } else if c == '#' {
            self.lex_hash(start)?
        } else if c == '|' {
            self.bump();
            Tok::VBar
        } else if c == '(' {
            self.bump();
            Tok::LParen
        } else if c == ')' {
            self.bump();
            Tok::RParen
        } else if c == '[' {
            self.bump();
            Tok::LBracket
        } else if c == ']' {
            self.bump();
            Tok::RBracket
        } else if c == ';' {
            self.bump();
            Tok::Semi
        } else if c == '.' {
            self.bump();
            Tok::Period
        } else if c == '^' {
            self.bump();
            Tok::Caret
        } else if c == ':' {
            self.bump();
            if self.peek_char() == Some('=') {
                self.bump();
                Tok::Assign
            } else {
                Tok::Colon
            }
        } else if is_binary_char(c) {
            self.lex_binary_sel(c)
        } else {
            return Err(self.err(start, format!("unexpected character '{c}'")));
        };

        self.prev_ends_expr = tok_ends_expr(&tok);
        Ok((tok, start))
    }
}

/// The base-256 little-endian magnitude of `digits` interpreted in `radix`
/// (both already lexer-validated: `radix` in `2..=36`, every char of
/// `digits` a valid base-`radix` digit). Empty result = zero. Used by the
/// parser to decide `Literal::Int` vs `Literal::BigInt` (SPEC-QUESTION,
/// `sprint_s05_detail.md` §Literal frame construction).
pub fn int_lit_magnitude(radix: u32, digits: &str) -> Vec<u8> {
    let mut mag: Vec<u8> = Vec::new();
    for c in digits.chars() {
        let d = c.to_digit(36).expect("int_lit_magnitude: invalid digit");
        let mut carry = d;
        for byte in mag.iter_mut() {
            let v = (*byte as u32) * radix + carry;
            *byte = (v & 0xFF) as u8;
            carry = v >> 8;
        }
        while carry > 0 {
            mag.push((carry & 0xFF) as u8);
            carry >>= 8;
        }
    }
    mag
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex_all(input: &str) -> Result<Vec<Tok>, LexError> {
        let mut lx = Lexer::new(input);
        let mut out = Vec::new();
        loop {
            let (t, _) = lx.next_token()?;
            if t == Tok::Eof {
                break;
            }
            out.push(t);
        }
        Ok(out)
    }

    #[test]
    fn lex_idents_keywords() {
        let toks = lex_all("foo at: x:=").unwrap();
        assert_eq!(
            toks,
            vec![
                Tok::Ident("foo".into()),
                Tok::Keyword("at:".into()),
                Tok::Ident("x".into()),
                Tok::Assign,
            ]
        );
    }

    #[test]
    fn lex_radix() {
        let toks = lex_all("16rFF 2r1010 36rZZ").unwrap();
        assert_eq!(
            toks,
            vec![
                Tok::IntLit {
                    negative: false,
                    radix: 16,
                    digits: "FF".into()
                },
                Tok::IntLit {
                    negative: false,
                    radix: 2,
                    digits: "1010".into()
                },
                Tok::IntLit {
                    negative: false,
                    radix: 36,
                    digits: "ZZ".into()
                },
            ]
        );
        assert_eq!(int_lit_magnitude(16, "FF"), vec![0xFF]);
        assert_eq!(int_lit_magnitude(2, "1010"), vec![0x0A]);
        assert_eq!(
            int_lit_magnitude(36, "ZZ"),
            vec![(1295 & 0xFF) as u8, (1295 >> 8) as u8]
        );
    }

    #[test]
    fn lex_radix_bad() {
        let e1 = lex_all("16r").unwrap_err();
        assert_eq!(e1.span, Span { line: 1, col: 1 });
        let e2 = lex_all("8r9").unwrap_err();
        assert_eq!(e2.span, Span { line: 1, col: 1 });
    }

    #[test]
    fn lex_float_forms() {
        let toks = lex_all("3.25 1e10 2.5e-3 1d2").unwrap();
        assert_eq!(
            toks,
            vec![
                Tok::FloatLit(3.25),
                Tok::FloatLit(1e10),
                Tok::FloatLit(2.5e-3),
                Tok::FloatLit(1e2),
            ]
        );
        let toks2 = lex_all("5.").unwrap();
        assert_eq!(
            toks2,
            vec![
                Tok::IntLit {
                    negative: false,
                    radix: 10,
                    digits: "5".into()
                },
                Tok::Period,
            ]
        );
    }

    #[test]
    fn lex_neg_table() {
        fn int(n: &str, neg: bool) -> Tok {
            Tok::IntLit {
                negative: neg,
                radix: 10,
                digits: n.into(),
            }
        }
        assert_eq!(lex_all("-3").unwrap(), vec![int("3", true)]);
        assert_eq!(
            lex_all("x - 3").unwrap(),
            vec![
                Tok::Ident("x".into()),
                Tok::BinarySel("-".into()),
                int("3", false)
            ]
        );
        assert_eq!(
            lex_all("x-3").unwrap(),
            vec![
                Tok::Ident("x".into()),
                Tok::BinarySel("-".into()),
                int("3", false)
            ]
        );
        assert_eq!(
            lex_all("3 - -2").unwrap(),
            vec![int("3", false), Tok::BinarySel("-".into()), int("2", true)]
        );
        assert_eq!(
            lex_all("3--2").unwrap(),
            vec![int("3", false), Tok::BinarySel("-".into()), int("2", true)]
        );
        assert_eq!(
            lex_all("a -2").unwrap(),
            vec![
                Tok::Ident("a".into()),
                Tok::BinarySel("-".into()),
                int("2", false)
            ]
        );
        assert_eq!(
            lex_all("(-3)").unwrap(),
            vec![Tok::LParen, int("3", true), Tok::RParen]
        );
        assert_eq!(
            lex_all("[-3]").unwrap(),
            vec![Tok::LBracket, int("3", true), Tok::RBracket]
        );
        // Pitfall P3: "a --> b" merges the first two dashes (neither is
        // followed by a digit) then leaves '>' standalone.
        assert_eq!(
            lex_all("a --> b").unwrap(),
            vec![
                Tok::Ident("a".into()),
                Tok::BinarySel("--".into()),
                Tok::BinarySel(">".into()),
                Tok::Ident("b".into()),
            ]
        );
    }

    #[test]
    fn lex_neg_in_litarray() {
        let mut lx = Lexer::new("-1 -2");
        lx.set_in_literal_seq(true);
        let mut out = Vec::new();
        loop {
            let (t, _) = lx.next_token().unwrap();
            if t == Tok::Eof {
                break;
            }
            out.push(t);
        }
        assert_eq!(
            out,
            vec![
                Tok::IntLit {
                    negative: true,
                    radix: 10,
                    digits: "1".into()
                },
                Tok::IntLit {
                    negative: true,
                    radix: 10,
                    digits: "2".into()
                },
            ]
        );
    }

    #[test]
    fn lex_char() {
        let toks = lex_all("$a $  $' $\" $$").unwrap();
        assert_eq!(
            toks,
            vec![
                Tok::CharLit('a'),
                Tok::CharLit(' '),
                Tok::CharLit('\''),
                Tok::CharLit('"'),
                Tok::CharLit('$'),
            ]
        );
    }

    #[test]
    fn lex_string_quotes() {
        let toks = lex_all("'it''s'").unwrap();
        assert_eq!(toks, vec![Tok::StrLit("it's".into())]);
        assert!(lex_all("'unterminated").is_err());
    }

    #[test]
    fn lex_comment_quotes() {
        let toks = lex_all("\"a \"\"b\"\" c\" 42").unwrap();
        assert_eq!(
            toks,
            vec![Tok::IntLit {
                negative: false,
                radix: 10,
                digits: "42".into()
            }]
        );
        assert!(lex_all("\"unterminated").is_err());
    }

    #[test]
    fn lex_symbols() {
        let toks = lex_all("#foo #at:put: #+ #>> #'hi there'").unwrap();
        assert_eq!(
            toks,
            vec![
                Tok::SymLit("foo".into()),
                Tok::SymLit("at:put:".into()),
                Tok::SymLit("+".into()),
                Tok::SymLit(">>".into()),
                Tok::SymLit("hi there".into()),
            ]
        );
    }

    #[test]
    fn lex_binary_two_char() {
        let toks = lex_all("a >= b").unwrap();
        assert_eq!(
            toks,
            vec![
                Tok::Ident("a".into()),
                Tok::BinarySel(">=".into()),
                Tok::Ident("b".into())
            ]
        );
        let toks2 = lex_all("a --> b").unwrap();
        assert_eq!(
            toks2,
            vec![
                Tok::Ident("a".into()),
                Tok::BinarySel("--".into()),
                Tok::BinarySel(">".into()),
                Tok::Ident("b".into()),
            ]
        );
    }

    #[test]
    fn lex_vbar_never_merges() {
        let toks = lex_all("||").unwrap();
        assert_eq!(toks, vec![Tok::VBar, Tok::VBar]);
    }

    #[test]
    fn lex_pragma_mode_gt_never_merges() {
        let mut lx = Lexer::new(">>= >");
        lx.set_pragma_mode(true);
        let mut out = Vec::new();
        loop {
            let (t, _) = lx.next_token().unwrap();
            if t == Tok::Eof {
                break;
            }
            out.push(t);
        }
        assert_eq!(
            out,
            vec![
                Tok::BinarySel(">".into()),
                Tok::BinarySel(">".into()),
                Tok::BinarySel("=".into()),
                Tok::BinarySel(">".into()),
            ]
        );
    }

    #[test]
    fn lex_span_tracks_line_col_bytes() {
        let mut lx = Lexer::new("foo\n  bar");
        let (_, s1) = lx.next_token().unwrap();
        assert_eq!(s1, Span { line: 1, col: 1 });
        let (_, s2) = lx.next_token().unwrap();
        assert_eq!(s2, Span { line: 2, col: 3 });
    }
}
