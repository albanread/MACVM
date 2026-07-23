//! A parser for MACVM's own `.mst` world-file syntax (`../../world/*.mst`,
//! `../../docs/WORLD.md` §10-11) — **not** Strongtalk's original bang-chunk
//! `.dlt` format (that's what the separate, not-yet-built `tools/dlt2mst`
//! converter reads). `.mst` files use MACVM's own bracket syntax:
//!
//! ```text
//! SuperclassName subclass: NewClassName [
//!     | instanceVar1 instanceVar2 |
//!     NewClassName class >> selectorName [ ... ]   "class-side method"
//!     unarySelector [ ... ]                        "instance-side method"
//!     binarySelector aParam [ ... ]
//!     keyword: a part: b [ ... ]
//! ]
//! ```
//!
//! Confirmed against the real 21-file corpus before writing this (not
//! assumed): class-side methods always repeat the full `ClassName class >>`
//! prefix per method — there is no block-grouping form (an earlier reading
//! of `01_object.mst`'s `class [ <primitive: 21> ]` looked like one at
//! first glance, but that's just the ordinary unary method `Object>>class`,
//! whose body happens to be a bare primitive pragma, same shape as
//! `identityHash [ <primitive: 20> ]` right next to it).
//!
//! Deliberately scoped to what the real corpus actually contains, not a
//! general Smalltalk grammar — this only has to import MACVM's own 21
//! `.mst` files correctly, not parse arbitrary Smalltalk source.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedMethod {
    pub selector: String,
    pub is_class_side: bool,
    /// The method's full source text, `selector [ ... ]` to the matching
    /// `]`, comment-and-string-aware so embedded `]` characters (inside a
    /// nested block literal, a comment, or a string) don't end it early —
    /// stored verbatim (including any leading doc comment inside the
    /// brackets), not split into separate "comment" and "code" fields; a
    /// `MethodMirror`'s source (`docs/APPS.md` §5.2) is the whole thing.
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedClass {
    pub name: String,
    pub superclass: Option<String>, // None only for `nil subclass: Object`
    pub instance_vars: String,      // space-separated, as written
    pub class_vars: String,         // space-separated, from `<classVars: …>`
    pub comment: String,            // the nearest preceding file-level comment
    pub methods: Vec<ParsedMethod>,
}

struct Scanner<'a> {
    s: &'a str,
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn new(s: &'a str) -> Self {
        Self { s, pos: 0 }
    }

    fn peek(&self) -> Option<char> {
        self.s[self.pos..].chars().next()
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.advance();
        }
    }

    /// Consume a `"..."` comment (doubled `""` = a literal embedded quote,
    /// same escaping rule as Smalltalk string literals below), returning
    /// its text. Assumes the caller already confirmed `peek() == Some('"')`.
    fn skip_comment(&mut self) -> String {
        self.advance(); // opening quote
        let mut text = String::new();
        loop {
            match self.advance() {
                None => break,
                Some('"') => {
                    if self.peek() == Some('"') {
                        text.push('"');
                        self.advance();
                    } else {
                        break;
                    }
                }
                Some(c) => text.push(c),
            }
        }
        text
    }

    fn skip_string_literal(&mut self) {
        self.advance(); // opening quote
        loop {
            match self.advance() {
                None => break,
                Some('\'') => {
                    if self.peek() == Some('\'') {
                        self.advance();
                    } else {
                        break;
                    }
                }
                Some(_) => {}
            }
        }
    }

    /// Whitespace and comments both skipped, comment text discarded — for
    /// skipping *between* tokens where a comment could legally appear but
    /// isn't being captured (inside a selector, between class defs when a
    /// caller already grabbed the one it wants, etc).
    fn skip_ws_and_comments(&mut self) {
        loop {
            self.skip_ws();
            if self.peek() == Some('"') {
                self.skip_comment();
            } else {
                break;
            }
        }
    }

    fn read_identifier(&mut self) -> Option<String> {
        self.skip_ws_and_comments();
        let start = self.pos;
        if !matches!(self.peek(), Some(c) if c.is_alphabetic() || c == '_') {
            return None;
        }
        while matches!(self.peek(), Some(c) if c.is_alphanumeric() || c == '_') {
            self.advance();
        }
        Some(self.s[start..self.pos].to_string())
    }

    fn eat_str(&mut self, lit: &str) -> bool {
        self.skip_ws_and_comments();
        if self.s[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            true
        } else {
            false
        }
    }

    fn is_binary_char(c: char) -> bool {
        "+-*/~<>=&|@%,?!\\".contains(c)
    }

    /// Reads one full selector (unary, binary, or keyword) starting right
    /// after whatever preceded it (a class-def opener or the previous
    /// method's closing `]`). Returns `None` at the end of a class body
    /// (next non-ws/comment token is `]`, not the start of a selector).
    fn read_selector(&mut self) -> Option<String> {
        self.skip_ws_and_comments();
        let sel = match self.peek() {
            None | Some(']') => None,
            Some(c) if Self::is_binary_char(c) => {
                let start = self.pos;
                while matches!(self.peek(), Some(c) if Self::is_binary_char(c)) {
                    self.advance();
                }
                let sel = self.s[start..self.pos].to_string();
                self.read_identifier(); // the one binary parameter — discarded, not needed for the selector string
                self.skip_optional_type_annotation(); // T0′: `< other <Magnitude>` (D11)
                Some(sel)
            }
            Some(_) => {
                let first = self.read_identifier()?;
                self.skip_ws_and_comments();
                if self.peek() == Some(':') {
                    // Keyword selector: one or more "part: paramName" groups.
                    let mut sel = String::new();
                    let mut part = Some(first);
                    loop {
                        let Some(p) = part.take() else { break };
                        sel.push_str(&p);
                        sel.push(':');
                        self.advance(); // the ':'
                        self.read_identifier(); // the parameter name — discarded
                        self.skip_optional_type_annotation(); // T0′: `add: n <Integer>` (D11)
                        self.skip_ws_and_comments();
                        // Another "identifier:" continues the keyword selector;
                        // anything else (typically '[') ends it.
                        let save = self.pos;
                        if let Some(next_ident) = self.read_identifier() {
                            self.skip_ws_and_comments();
                            if self.peek() == Some(':') {
                                part = Some(next_ident);
                                continue;
                            }
                        }
                        self.pos = save;
                        break;
                    }
                    Some(sel)
                } else {
                    Some(first) // unary
                }
            }
        };
        // T0′ (docs/typechecker_design.md §3): `^<ReturnType>` can follow ANY
        // pattern shape (unary/binary/keyword) — one common tail rather than
        // three duplicated calls at each `Some(...)` above.
        if sel.is_some() {
            self.skip_optional_return_type_annotation();
        }
        sel
    }

    /// T0′: skips ONE optional Strongtalk-style type annotation `< ... >` at
    /// the current position — a balanced-bracket scan (nested `<...>` counts,
    /// matching `parser::capture_pragma_body`'s discipline), but simpler than
    /// [`Self::peek_is_class_pragma`]/[`Self::read_angle_pragma`]: no
    /// keyword/pragma disambiguation is needed here, since a `<` right after
    /// a parameter name or a whole method pattern can ONLY be a type
    /// annotation in this grammar (never a class pragma or another
    /// selector). No-op if the current char (after whitespace/comments)
    /// isn't `<`. Skip-only: this scanner already captures each method's
    /// FULL raw text separately (`read_bracketed_body`'s span includes any
    /// annotation verbatim); interpreting the annotation is `capture_
    /// type_signatures`'s job, over the SAME source re-parsed by the real
    /// frontend parser, not this lightweight scanner's.
    fn skip_optional_type_annotation(&mut self) {
        self.skip_ws_and_comments();
        if self.peek() != Some('<') {
            return;
        }
        self.advance(); // '<'
        let mut depth = 1u32;
        while depth > 0 {
            match self.peek() {
                Some('<') => {
                    depth += 1;
                    self.advance();
                }
                Some('>') => {
                    depth -= 1;
                    self.advance();
                }
                None => break, // unterminated -- let the caller's own EOF handling take over
                _ => {
                    self.advance();
                }
            }
        }
    }

    /// The `^<ReturnType>` half of a method pattern, between the last
    /// parameter and the body's `[`. No-op if there's no `^` here.
    fn skip_optional_return_type_annotation(&mut self) {
        self.skip_ws_and_comments();
        if self.peek() == Some('^') {
            self.advance();
            self.skip_optional_type_annotation();
        }
    }

    /// After a selector, reads `[ ... ]` — comment/string-aware bracket
    /// matching so nested block literals, comments, and strings inside the
    /// method body can't end it early. Returns the *whole* `selector [ ... ]`
    /// span (selector_start..closing bracket inclusive) as the method's
    /// stored source text.
    /// True iff a `<` at the current position begins a class-level pragma
    /// (`<classVars: …>`, `<indexable: …>`) rather than a binary-selector
    /// method (`< aMagnitude [ … ]`, `<= other [ … ]`). A pragma is `<`
    /// immediately followed by a keyword — an identifier then `:`; a binary
    /// method is `<`/`<=` followed by a parameter and `[`. Non-consuming
    /// (saves and restores position).
    fn peek_is_class_pragma(&mut self) -> bool {
        let save = self.pos;
        self.advance(); // the `<`
        let is_pragma = self.read_identifier().is_some() && {
            self.skip_ws_and_comments();
            self.peek() == Some(':')
        };
        self.pos = save;
        is_pragma
    }

    /// Consumes a class-level pragma `< ... >` and returns its inner text
    /// (e.g. `"classVars: Table"`). Assumes the next char is `<`. Stops at the
    /// first `>` (class pragmas don't nest angle brackets); a missing `>`
    /// consumes to EOF, which just ends the class scan cleanly.
    fn read_angle_pragma(&mut self) -> String {
        self.advance(); // the `<`
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == '>' {
                let inner = self.s[start..self.pos].to_string();
                self.advance(); // the `>`
                return inner;
            }
            self.advance();
        }
        self.s[start..self.pos].to_string()
    }

    /// Attempts to read a `| ident1 ident2 ... |` instance-variable list at
    /// the current position. On success (a run of identifiers closed by a
    /// second `|`), returns them space-joined, having consumed through the
    /// closing `|`. On failure — `|` actually opens a binary-selector method
    /// named `|` (`02_nil_boolean.mst`'s `| aBoolean [ ... ]`): one
    /// identifier then `[`, not another `|` — the position is rewound and
    /// `None` is returned, so the caller's normal selector/method parsing
    /// picks it up unchanged. Assumes the caller already confirmed
    /// `peek() == Some('|')`.
    fn try_read_ivar_list(&mut self) -> Option<String> {
        let save = self.pos;
        self.advance(); // the opening `|`
        let mut vars = Vec::new();
        loop {
            self.skip_ws_and_comments();
            if self.peek() == Some('|') {
                self.advance();
                return Some(vars.join(" "));
            }
            match self.read_identifier() {
                Some(v) => {
                    vars.push(v);
                    self.skip_optional_type_annotation(); // T0′: `| count <Integer> |` (D11)
                }
                None => {
                    self.pos = save; // not actually an ivar list — rewind
                    return None;
                }
            }
        }
    }

    fn read_bracketed_body(&mut self) -> Option<&'a str> {
        self.skip_ws_and_comments();
        if self.peek() != Some('[') {
            return None;
        }
        let body_start = self.pos;
        self.advance(); // opening [
        let mut depth = 1;
        loop {
            match self.peek() {
                None => break,
                Some('[') => {
                    depth += 1;
                    self.advance();
                }
                Some(']') => {
                    depth -= 1;
                    self.advance();
                    if depth == 0 {
                        return Some(&self.s[body_start..self.pos]);
                    }
                }
                Some('$') => {
                    // A character literal `$c` — the char AFTER `$` is data,
                    // not a delimiter, even when it is `[`, `]`, `'`, or `"`.
                    // Without this, `String>>printOn:`'s `$'` reads as a
                    // string-literal start and desyncs the bracket depth,
                    // truncating the captured source (dropping the method's
                    // closing `]`). Skip both chars as one token.
                    self.advance(); // the `$`
                    self.advance(); // the literal character (any char)
                }
                Some('"') => {
                    self.skip_comment();
                }
                Some('\'') => {
                    self.skip_string_literal();
                }
                Some(_) => {
                    self.advance();
                }
            }
        }
        Some(&self.s[body_start..self.pos])
    }
}

/// Parse just the message pattern (selector) from a standalone method
/// source string's first line — for the class browser's "New Method"
/// template (`../../gui/src/browser_render.rs`), where there's no
/// surrounding class body to scan, just the raw accepted text. Reuses the
/// exact same selector grammar the class-body parser below uses (unary,
/// binary, or keyword), so a method saved this way is written in real
/// `.mst` syntax and would round-trip identically if ever re-parsed as
/// part of a full class body (e.g. a future image→`.mst` exporter,
/// `../../docs/IMAGE.md` §9). Returns `None` if the text doesn't start
/// with a parseable pattern at all (empty, or starts with something that
/// isn't an identifier or binary-selector character).
pub fn parse_method_selector(source: &str) -> Option<String> {
    Scanner::new(source).read_selector()
}

/// Every DISTINCT selector *sent* by a method's body — the data behind an
/// accurate, VM-free "senders" query (`Image::senders_of`). A compact
/// recursive-descent walk of the stored `pattern [ ... ]` source that:
///   - skips the method's own defining pattern (its keyword parts aren't sends);
///   - groups keyword message parts back into whole selectors (`at:put:`), which
///     a flat text search can't do because they're split across arguments;
///   - is comment / string / char / symbol-literal aware, so `"a comment"`,
///     `'at: put:'`, or the literal `#at:put:` never masquerade as sends.
/// Deliberately scoped to the real corpus's Smalltalk, same as the rest of this
/// module — not a general compiler front end.
pub fn sent_selectors(method_source: &str) -> Vec<String> {
    let mut s = SendScan {
        sc: Scanner::new(method_source),
        out: Vec::new(),
    };
    s.method();
    s.out.sort();
    s.out.dedup();
    s.out
}

struct SendScan<'a> {
    sc: Scanner<'a>,
    out: Vec<String>,
}

impl<'a> SendScan<'a> {
    fn method(&mut self) {
        // Skip the defining message pattern — its keyword parts define the
        // method, they are not sends.
        self.sc.read_selector();
        self.sc.skip_ws_and_comments();
        // Body is usually `pattern [ ... ]` (imported form); a freshly-typed
        // template may be bare (`pattern\n ...`). Consume an opening body `[`
        // if present, then parse the statement sequence either way.
        if self.sc.peek() == Some('[') {
            self.sc.advance();
        }
        self.sequence();
    }

    /// temps? pragma* ( statement ('.' statement?)* )? — stops at a closer.
    fn sequence(&mut self) {
        self.temps();
        loop {
            self.sc.skip_ws_and_comments();
            match self.sc.peek() {
                None | Some(']') | Some('}') | Some(')') => break,
                Some('.') => {
                    self.sc.advance();
                }
                Some('<') => self.skip_pragma(),
                Some('^') => {
                    self.sc.advance();
                    self.statement_expr();
                }
                _ => self.statement_expr(),
            }
        }
    }

    /// One statement's expression, with a progress guard so a token the grammar
    /// doesn't model can never spin the sequence loop forever.
    fn statement_expr(&mut self) {
        let before = self.sc.pos;
        self.expression();
        if self.sc.pos == before {
            self.sc.advance(); // unmodelled char — skip it and keep going
        }
    }

    /// Optional `| a b |` temp declaration at the head of a sequence. Rolls back
    /// if what follows isn't actually a temp list (so a leading binary-or, which
    /// can't really occur with no receiver, is never mistaken for one).
    fn temps(&mut self) {
        self.sc.skip_ws_and_comments();
        if self.sc.peek() != Some('|') {
            return;
        }
        let save = self.sc.pos;
        self.sc.advance();
        loop {
            self.sc.skip_ws_and_comments();
            match self.sc.peek() {
                Some('|') => {
                    self.sc.advance();
                    return;
                }
                Some(c) if c.is_alphabetic() || c == '_' => {
                    self.sc.read_identifier();
                }
                _ => {
                    self.sc.pos = save;
                    return;
                }
            }
        }
    }

    fn skip_pragma(&mut self) {
        self.sc.advance(); // '<'
        loop {
            match self.sc.peek() {
                None | Some('>') => {
                    self.sc.advance();
                    return;
                }
                Some('\'') => self.sc.skip_string_literal(),
                Some(_) => {
                    self.sc.advance();
                }
            }
        }
    }

    fn expression(&mut self) {
        // assignment: `id := expr` (the `:=` isn't a send)
        self.sc.skip_ws_and_comments();
        let save = self.sc.pos;
        if matches!(self.sc.peek(), Some(c) if c.is_alphabetic() || c == '_') {
            self.sc.read_identifier();
            self.sc.skip_ws_and_comments();
            if self.sc.s[self.sc.pos..].starts_with(":=") {
                self.sc.pos += 2;
                self.expression();
                return;
            }
        }
        self.sc.pos = save;
        self.cascade();
    }

    // cascade := keywordExpr (';' message)*
    fn cascade(&mut self) {
        self.keyword_expr();
        loop {
            self.sc.skip_ws_and_comments();
            if self.sc.peek() == Some(';') {
                self.sc.advance();
                self.cascade_message();
            } else {
                break;
            }
        }
    }

    /// A message after `;` in a cascade — unary / binary / keyword on the
    /// (implicit) cascade receiver.
    fn cascade_message(&mut self) {
        self.sc.skip_ws_and_comments();
        if let Some(kw) = self.try_keyword_selector() {
            self.out.push(kw);
        } else if let Some(op) = self.try_binary_op() {
            self.out.push(op);
            self.unary_expr();
        } else {
            self.unary_chain_only();
        }
    }

    // keywordExpr := binaryExpr (keyword binaryExpr)*
    fn keyword_expr(&mut self) {
        self.binary_expr();
        let mut sel = String::new();
        loop {
            if let Some(part) = self.try_keyword_part() {
                sel.push_str(&part); // includes the ':'
                self.binary_expr();
            } else {
                break;
            }
        }
        if !sel.is_empty() {
            self.out.push(sel);
        }
    }

    // binaryExpr := unaryExpr (binOp unaryExpr)*
    fn binary_expr(&mut self) {
        self.unary_expr();
        loop {
            if let Some(op) = self.try_binary_op() {
                self.out.push(op);
                self.unary_expr();
            } else {
                break;
            }
        }
    }

    // unaryExpr := primary unarySelector*
    fn unary_expr(&mut self) {
        self.primary();
        self.unary_chain_only();
    }

    fn unary_chain_only(&mut self) {
        while let Some(u) = self.try_unary_selector() {
            self.out.push(u);
        }
    }

    fn primary(&mut self) {
        self.sc.skip_ws_and_comments();
        match self.sc.peek() {
            Some('(') => {
                self.sc.advance();
                self.expression();
                self.expect(')');
            }
            Some('[') => self.block(),
            Some('{') => {
                self.sc.advance();
                self.brace_array();
            }
            Some('\'') => self.sc.skip_string_literal(),
            Some('$') => {
                self.sc.advance();
                self.sc.advance(); // the quoted character
            }
            Some('#') => self.eat_hash_literal(),
            Some('-') if self.digit_follows_dash() => self.eat_number(),
            Some(c) if c.is_ascii_digit() => self.eat_number(),
            Some(c) if c.is_alphabetic() || c == '_' => {
                self.sc.read_identifier(); // a bare variable / pseudo / class read
            }
            _ => {} // caller's progress guard handles anything unmodelled
        }
    }

    fn block(&mut self) {
        self.sc.advance(); // '['
        // Optional block arguments: `:a :b |`
        self.sc.skip_ws_and_comments();
        if self.sc.peek() == Some(':') {
            loop {
                self.sc.skip_ws_and_comments();
                if self.sc.peek() == Some(':') {
                    self.sc.advance();
                    self.sc.read_identifier();
                } else {
                    break;
                }
            }
            self.sc.skip_ws_and_comments();
            if self.sc.peek() == Some('|') {
                self.sc.advance();
            }
        }
        self.sequence();
        self.expect(']');
    }

    /// Dynamic array `{ expr . expr }` — its elements are real expressions, so
    /// their sends count.
    fn brace_array(&mut self) {
        loop {
            self.sc.skip_ws_and_comments();
            match self.sc.peek() {
                None | Some('}') => {
                    self.sc.advance();
                    return;
                }
                Some('.') => {
                    self.sc.advance();
                }
                _ => self.statement_expr(),
            }
        }
    }

    /// `#foo` / `#at:put:` / `#+` symbols, `#(...)` literal arrays, `#[...]`
    /// byte arrays, `#'quoted'` — all literals, no sends inside.
    fn eat_hash_literal(&mut self) {
        self.sc.advance(); // '#'
        match self.sc.peek() {
            Some('(') => self.skip_balanced('(', ')'),
            Some('[') => self.skip_balanced('[', ']'),
            Some('\'') => self.sc.skip_string_literal(),
            Some(c) if Scanner::is_binary_char(c) => {
                while matches!(self.sc.peek(), Some(c) if Scanner::is_binary_char(c)) {
                    self.sc.advance();
                }
            }
            _ => {
                // #keyword:part: or #unary — identifier chars plus ':'
                while matches!(self.sc.peek(), Some(c) if c.is_alphanumeric() || c == '_' || c == ':')
                {
                    self.sc.advance();
                }
            }
        }
    }

    /// Skip a bracketed literal, string/comment-aware and balanced (a literal
    /// array can nest another).
    fn skip_balanced(&mut self, open: char, close: char) {
        self.sc.advance(); // the opener
        let mut depth = 1;
        while depth > 0 {
            match self.sc.peek() {
                None => return,
                Some('\'') => self.sc.skip_string_literal(),
                Some('"') => {
                    self.sc.skip_comment();
                }
                Some(c) => {
                    if c == open {
                        depth += 1;
                    } else if c == close {
                        depth -= 1;
                    }
                    self.sc.advance();
                }
            }
        }
    }

    fn eat_number(&mut self) {
        // Lenient: an optional leading '-', then digits and the characters that
        // can appear in radix / float / exponent / scaled literals. Good enough
        // to consume the token so it isn't mis-read as sends.
        if self.sc.peek() == Some('-') {
            self.sc.advance();
        }
        while matches!(self.sc.peek(), Some(c) if c.is_alphanumeric() || c == 'r') {
            self.sc.advance();
        }
        // A decimal point only continues the number if a digit follows (else
        // it's a statement separator).
        if self.sc.peek() == Some('.') {
            let after = self.sc.s[self.sc.pos..].chars().nth(1);
            if matches!(after, Some(d) if d.is_ascii_digit()) {
                self.sc.advance(); // '.'
                while matches!(self.sc.peek(), Some(c) if c.is_alphanumeric() || c == '-') {
                    self.sc.advance();
                }
            }
        }
    }

    fn digit_follows_dash(&self) -> bool {
        matches!(self.sc.s[self.sc.pos..].chars().nth(1), Some(d) if d.is_ascii_digit())
    }

    /// A keyword message part `ident:` at the cursor (not `ident:=`). Consumes
    /// and returns `"ident:"`, or rolls back and returns `None`.
    fn try_keyword_part(&mut self) -> Option<String> {
        self.sc.skip_ws_and_comments();
        let save = self.sc.pos;
        let Some(id) = self.plain_identifier() else {
            return None;
        };
        if self.sc.peek() == Some(':') && self.sc.s[self.sc.pos..].chars().nth(1) != Some('=') {
            self.sc.advance(); // ':'
            Some(format!("{id}:"))
        } else {
            self.sc.pos = save;
            None
        }
    }

    /// A whole keyword selector starting at the cursor (`kw: arg kw2: arg…`) —
    /// used for cascade messages. Returns the combined selector, args consumed.
    fn try_keyword_selector(&mut self) -> Option<String> {
        let save = self.sc.pos;
        let Some(first) = self.try_keyword_part() else {
            self.sc.pos = save;
            return None;
        };
        let mut sel = first;
        self.binary_expr();
        while let Some(part) = self.try_keyword_part() {
            sel.push_str(&part);
            self.binary_expr();
        }
        Some(sel)
    }

    /// A binary operator at the cursor. `:=`/`:` never qualify (`:` isn't a
    /// binary char); a leading `-` before a digit is a negative literal, not an
    /// operator, and is left for `primary`.
    fn try_binary_op(&mut self) -> Option<String> {
        self.sc.skip_ws_and_comments();
        match self.sc.peek() {
            Some('-') if self.digit_follows_dash() => None,
            Some(c) if Scanner::is_binary_char(c) => {
                let start = self.sc.pos;
                while matches!(self.sc.peek(), Some(c) if Scanner::is_binary_char(c)) {
                    self.sc.advance();
                }
                Some(self.sc.s[start..self.sc.pos].to_string())
            }
            _ => None,
        }
    }

    /// A unary selector at the cursor: an identifier NOT followed by `:` (that
    /// would be a keyword part). Consumes and returns it, or rolls back.
    fn try_unary_selector(&mut self) -> Option<String> {
        self.sc.skip_ws_and_comments();
        let save = self.sc.pos;
        let Some(id) = self.plain_identifier() else {
            return None;
        };
        self.sc.skip_ws_and_comments();
        if self.sc.peek() == Some(':') {
            self.sc.pos = save; // keyword part — not a unary send
            None
        } else {
            Some(id)
        }
    }

    /// Read a bare identifier WITHOUT `read_identifier`'s leading trivia skip
    /// (callers control trivia so roll-backs are exact).
    fn plain_identifier(&mut self) -> Option<String> {
        let start = self.sc.pos;
        if !matches!(self.sc.peek(), Some(c) if c.is_alphabetic() || c == '_') {
            return None;
        }
        while matches!(self.sc.peek(), Some(c) if c.is_alphanumeric() || c == '_') {
            self.sc.advance();
        }
        Some(self.sc.s[start..self.sc.pos].to_string())
    }

    fn expect(&mut self, ch: char) {
        self.sc.skip_ws_and_comments();
        if self.sc.peek() == Some(ch) {
            self.sc.advance();
        }
    }
}

/// Parse one `.mst` file's full text into its class definitions. Malformed
/// input (anything not matching the grammar this module's doc comment
/// describes) stops parsing at the point of confusion and returns whatever
/// classes were successfully parsed before it, rather than erroring the
/// whole file out — matches this being an import *tool*, not a compiler:
/// partial, inspectable progress beats an all-or-nothing failure.
pub fn parse_mst_source(text: &str) -> Vec<ParsedClass> {
    let mut sc = Scanner::new(text);
    let mut classes = Vec::new();
    let mut pending_comment: Option<String> = None;

    loop {
        sc.skip_ws();
        if sc.peek() == Some('"') {
            let c = sc.skip_comment();
            // NEAREST preceding comment wins (the `ParsedClass::comment`
            // contract): when a file header / section banner AND a dedicated
            // class comment both precede a class, the one written directly
            // above the definition is the class comment. (This was
            // first-wins until 2026-07: every class whose dedicated comment
            // followed the file header silently got the header instead.)
            pending_comment = Some(c);
            continue;
        }
        if sc.peek().is_none() {
            break;
        }

        let Some(superclass_name) = sc.read_identifier() else {
            break;
        };
        if !sc.eat_str("subclass:") {
            break;
        }
        let Some(new_name) = sc.read_identifier() else {
            break;
        };
        sc.skip_ws_and_comments();
        if sc.peek() != Some('[') {
            break;
        }
        sc.advance(); // consume the class body's opening [

        let mut instance_vars = String::new();
        let mut class_vars = String::new();
        let mut methods = Vec::new();
        loop {
            sc.skip_ws_and_comments();
            if sc.peek() == Some(']') {
                sc.advance(); // close the class body
                break;
            }
            if sc.peek().is_none() {
                break;
            }

            // The "| a b c |" instance-variable declaration — usually the
            // very first thing in the class body, but a preceding pragma
            // pushes it later (`<classVars: nil>` before `| Block |` in
            // `64_cocoaui.mst`'s `CocoaToolbarAction` — a real corpus case
            // that silently truncated this file's whole remaining parse
            // before this fix, dropping `CocoaUI` and everything after it).
            // Tried on every iteration rather than once up front, so it's
            // recognized regardless of what precedes it.
            // `try_read_ivar_list` rewinds cleanly when `|` actually opens a
            // binary method instead (`02_nil_boolean.mst`'s
            // `| aBoolean [ ... ]`), so real `|` methods still parse as
            // methods.
            if sc.peek() == Some('|') {
                if let Some(vars) = sc.try_read_ivar_list() {
                    instance_vars = vars;
                    continue;
                }
            }

            // A class-level pragma, e.g. `<classVars: Table>` or
            // `<indexable: oops>`. The method scanner MUST skip these — left
            // unhandled, `<` reads as a binary selector, `read_bracketed_body`
            // then finds no `[` and the whole class's method scan breaks
            // (this dropped every one of `Character`'s methods). Method-level
            // pragmas like `<primitive: N>` live inside a body and are
            // captured by `read_bracketed_body`, not here.
            if sc.peek() == Some('<') && sc.peek_is_class_pragma() {
                let pragma = sc.read_angle_pragma();
                if let Some(rest) = pragma.trim().strip_prefix("classVars:") {
                    class_vars = rest.split_whitespace().collect::<Vec<_>>().join(" ");
                }
                continue;
            }

            // "NewClassName class >> selector [ body ]" vs a plain selector.
            let save = sc.pos;
            let mut is_class_side = false;
            if let Some(ident) = sc.read_identifier() {
                if ident == new_name && sc.eat_str("class") && sc.eat_str(">>") {
                    is_class_side = true;
                } else {
                    sc.pos = save;
                }
            }

            let Some(selector) = sc.read_selector() else {
                break;
            };
            if sc.read_bracketed_body().is_none() {
                break;
            }
            // Capture the FULL method text — header AND body — from `save`
            // (before any `Name class >>` prefix) to just past the matching
            // `]`. Storing only the bracketed body drops the parameter NAMES
            // from the header (`read_selector` discards them) even though the
            // body references those names, so a body-only record cannot be
            // recompiled — which the database must be able to do to serve as
            // the source of truth for booting the VM. This makes the code
            // match `ParsedMethod::source`'s own "full source text" contract.
            let source = sc.s[save..sc.pos].trim().to_string();
            methods.push(ParsedMethod {
                selector,
                is_class_side,
                source,
            });
        }

        classes.push(ParsedClass {
            name: new_name,
            superclass: if superclass_name == "nil" {
                None
            } else {
                Some(superclass_name)
            },
            instance_vars,
            class_vars,
            comment: pending_comment.take().unwrap_or_default(),
            methods,
        });
    }

    classes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_object_and_message_from_01_object_mst_shape() {
        let src = r#""01_object.mst — a file comment."

nil subclass: Object [
    class [
        <primitive: 21>
    ]
    == anObject [
        <primitive: 22>
    ]
    -> v [
        ^Association key: self value: v
    ]
    error: aString [
        <primitive: 95>
    ]
]

Object subclass: Message [
    selector [
        ^self instVarAt: 1
    ]
]
"#;
        let classes = parse_mst_source(src);
        assert_eq!(classes.len(), 2);

        let object = &classes[0];
        assert_eq!(object.name, "Object");
        assert_eq!(object.superclass, None);
        assert_eq!(object.comment, "01_object.mst — a file comment.");
        // "class" here is an ordinary unary method (Object>>class), not a
        // class-side-methods block — exactly the corpus finding this
        // module's doc comment describes.
        let class_method = object
            .methods
            .iter()
            .find(|m| m.selector == "class")
            .unwrap();
        assert!(!class_method.is_class_side);
        assert!(class_method.source.contains("primitive: 21"));

        let binary = object.methods.iter().find(|m| m.selector == "->").unwrap();
        assert!(binary.source.contains("Association key: self value: v"));

        let keyword = object
            .methods
            .iter()
            .find(|m| m.selector == "error:")
            .unwrap();
        assert!(keyword.source.contains("primitive: 95"));

        let message = &classes[1];
        assert_eq!(message.name, "Message");
        assert_eq!(message.superclass.as_deref(), Some("Object"));
    }

    #[test]
    fn parses_instance_vars_and_class_side_method() {
        let src = r#"Object subclass: Association [
    | key value |
    Association class >> key: k value: v [
        | a |
        a := self basicNew.
        ^a
    ]
    key [
        ^key
    ]
]
"#;
        let classes = parse_mst_source(src);
        assert_eq!(classes.len(), 1);
        let assoc = &classes[0];
        assert_eq!(assoc.instance_vars, "key value");
        let ctor = assoc
            .methods
            .iter()
            .find(|m| m.selector == "key:value:")
            .unwrap();
        assert!(ctor.is_class_side);
        let getter = assoc.methods.iter().find(|m| m.selector == "key").unwrap();
        assert!(!getter.is_class_side);
    }

    #[test]
    fn nested_block_literals_dont_confuse_bracket_matching() {
        let src = r#"Object subclass: OrderedCollection [
    do: aBlock [
        firstIndex to: lastIndex do: [:i | aBlock value: (self at: i)]
    ]
    size [
        ^lastIndex - firstIndex + 1
    ]
]
"#;
        let classes = parse_mst_source(src);
        let oc = &classes[0];
        assert_eq!(oc.methods.len(), 2);
        let do_method = oc.methods.iter().find(|m| m.selector == "do:").unwrap();
        assert!(do_method.source.contains("aBlock value: (self at: i)"));
        // Confirms the outer `]` wasn't mistaken for the method's own close.
        assert!(do_method.source.trim_end().ends_with(']'));
    }

    /// When a file header comment AND a dedicated class comment both precede
    /// a class definition, the NEAREST one is the class comment — the
    /// `ParsedClass::comment` contract. (Regression: this was first-wins,
    /// so every class whose dedicated comment sat below the file header
    /// silently imported the header as its browser comment instead.)
    #[test]
    fn nearest_preceding_comment_wins_over_the_file_header() {
        let src = r#""99_file.mst — the file header, tooling notes, not a class comment."
"I am the class comment a browser should show."
Object subclass: Commented [
    foo [ ^1 ]
]
"a stray comment between classes"
"I am the second class's own comment."
Object subclass: Commented2 [
    bar [ ^2 ]
]
"#;
        let classes = parse_mst_source(src);
        assert_eq!(
            classes[0].comment,
            "I am the class comment a browser should show."
        );
        assert_eq!(classes[1].comment, "I am the second class's own comment.");
    }

    #[test]
    fn comments_and_strings_containing_brackets_dont_confuse_matching() {
        let src = r#"Object subclass: Weird [
    "a comment with a ] bracket in it"
    foo [
        ^'a string with a ] bracket'
    ]
]
"#;
        let classes = parse_mst_source(src);
        let m = &classes[0].methods[0];
        assert_eq!(m.selector, "foo");
        assert!(m.source.contains("a string with a ] bracket"));
    }

    #[test]
    fn binary_selector_arrow_is_read_as_one_token() {
        let src = "Object subclass: Object [\n    -> v [\n        ^1\n    ]\n]\n";
        let classes = parse_mst_source(src);
        assert_eq!(classes[0].methods[0].selector, "->");
    }

    #[test]
    fn parse_method_selector_handles_unary_binary_and_keyword() {
        assert_eq!(
            parse_method_selector("printString\n\t^'x'"),
            Some("printString".to_string())
        );
        assert_eq!(
            parse_method_selector("+ aNumber\n\t^self add: aNumber"),
            Some("+".to_string())
        );
        assert_eq!(
            parse_method_selector("at: i put: v\n\t^self"),
            Some("at:put:".to_string())
        );
        assert_eq!(parse_method_selector(""), None);
    }

    #[test]
    fn sent_selectors_extracts_unary_binary_and_grouped_keyword_sends() {
        let sel = |src: &str| sent_selectors(src);
        let want = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<String>>();

        // The method's own pattern is NOT a send; the body's send is.
        assert_eq!(
            sel("printOn: aStream [ aStream nextPutAll: 'hi' ]"),
            want(&["nextPutAll:"])
        );
        // Keyword parts split across args are grouped back into one selector.
        assert_eq!(sel("store [ ^dict at: k put: v ]"), want(&["at:put:"]));
        // A nested keyword send inside an argument is its own selector.
        assert_eq!(
            sel("f [ ^self bar: (a baz: b) qux: c ]"),
            want(&["bar:qux:", "baz:"])
        );
        // Unary chain + binary operator.
        assert_eq!(
            sel("g [ ^self size + other length ]"),
            want(&["+", "length", "size"])
        );
        // Every cascade message counts (deduped).
        assert_eq!(sel("h [ s show: 'a'; nl; show: 'b' ]"), want(&["nl", "show:"]));
        // Comments, strings, symbol literals, temps, and `:=` are NOT sends.
        assert_eq!(
            sel("j [ | a | a := #at:put:. \"do: it\" ^'x , y' ]"),
            want(&[])
        );
        // A negative literal is not a binary '-'; a real subtraction is.
        assert_eq!(sel("k [ ^self clampTo: -3 ]"), want(&["clampTo:"]));
        assert_eq!(sel("m [ ^x - 3 ]"), want(&["-"]));
        // A block body's sends count; its args don't.
        assert_eq!(sel("n [ items do: [:e | e printNl ] ]"), want(&["do:", "printNl"]));
        // A primitive pragma is not a send.
        assert_eq!(
            sel("p [ <primitive: 60> ^self basicAt: i ]"),
            want(&["basicAt:"])
        );
    }

    // ── S22 regression tests: the DB must reproduce COMPILABLE source ──────

    #[test]
    fn captures_the_full_method_header_including_parameter_names() {
        // `at:put:`'s stored source must carry the `at: i put: v` header, not
        // just the `[ body ]` — the body references `i`/`v`, so a body-only
        // record could not be recompiled (the whole reason to store source).
        let src = "Object subclass: Foo [\n    at: i put: v [\n        ^i + v\n    ]\n]\n";
        let m = &parse_mst_source(src)[0].methods[0];
        assert_eq!(m.selector, "at:put:");
        assert!(m.source.starts_with("at: i put: v ["), "{:?}", m.source);
    }

    #[test]
    fn character_literal_quote_does_not_truncate_the_body() {
        // `$'` is the single-quote CHARACTER literal — its `'` must not read
        // as a string-literal start (which desyncs bracket depth and drops
        // the method's closing `]`). This is String>>printOn:'s exact shape.
        let src = "Object subclass: Str [\n    printOn: s [\n        s nextPut: $'.\n        s nextPut: $'\n    ]\n    size [ ^0 ]\n]\n";
        let c = &parse_mst_source(src)[0];
        assert_eq!(c.methods.len(), 2, "the $' method must not swallow `size`");
        let p = c.methods.iter().find(|m| m.selector == "printOn:").unwrap();
        assert!(
            p.source.trim_end().ends_with(']'),
            "not truncated: {:?}",
            p.source
        );
    }

    #[test]
    fn class_vars_pragma_is_captured_and_does_not_drop_later_methods() {
        // `<classVars: Table>` must be captured AND must not break the method
        // scan (it once dropped every one of Character's methods).
        let src = "Magnitude subclass: Character [\n    <classVars: Table>\n    Character class >> value: v [\n        ^Table at: v\n    ]\n    isVowel [ ^false ]\n]\n";
        let c = &parse_mst_source(src)[0];
        assert_eq!(c.class_vars, "Table");
        assert_eq!(c.methods.len(), 2, "pragma must not drop following methods");
        assert!(c
            .methods
            .iter()
            .any(|m| m.selector == "value:" && m.is_class_side));
        assert!(c.methods.iter().any(|m| m.selector == "isVowel"));
    }

    #[test]
    fn binary_comparison_selectors_are_not_eaten_as_class_pragmas() {
        // `<`, `<=`, `>=` are binary SELECTORS, not `<…>` pragmas — the
        // pragma check must not consume them (it once turned Magnitude's
        // `< aMagnitude` / `>=` into garbage selectors).
        let src = "Object subclass: Magnitude [\n    < aMagnitude [ ^self subclassResponsibility ]\n    <= aMagnitude [ ^(aMagnitude < self) not ]\n    >= aMagnitude [ ^(self < aMagnitude) not ]\n]\n";
        let c = &parse_mst_source(src)[0];
        assert!(c.class_vars.is_empty());
        let sels: Vec<&str> = c.methods.iter().map(|m| m.selector.as_str()).collect();
        assert!(sels.contains(&"<"), "got {sels:?}");
        assert!(sels.contains(&"<="), "got {sels:?}");
        assert!(sels.contains(&">="), "got {sels:?}");
    }

    #[test]
    fn binary_pipe_selector_still_parses_as_a_method_not_an_ivar_list() {
        // `|` is ALSO a binary selector (`02_nil_boolean.mst`'s `True | aBoolean`,
        // `False | aBoolean`) — `try_read_ivar_list`'s rewind-on-failure must not
        // swallow it. One identifier followed by `[` (not a second `|`) is a
        // method, not an ivar list.
        let src = "Object subclass: True [\n    | aBoolean [ ^true ]\n]\n";
        let c = &parse_mst_source(src)[0];
        assert!(
            c.instance_vars.is_empty(),
            "a `| param [` method must not be mistaken for an ivar list, got {:?}",
            c.instance_vars
        );
        let sels: Vec<&str> = c.methods.iter().map(|m| m.selector.as_str()).collect();
        assert!(sels.contains(&"|"), "got {sels:?}");
    }

    #[test]
    fn a_pragma_before_the_ivar_list_does_not_truncate_the_rest_of_the_class_or_file() {
        // The real bug (`64_cocoaui.mst`'s `CocoaToolbarAction`): a
        // `<classVars: …>` pragma before `| ivars |` used to leave the ivar
        // list unrecognized (the one-shot check only looked immediately after
        // `[`), so the orphaned `|` was misread as starting a binary-selector
        // method, its "parameter" ate the ivar name, no `[` followed, and the
        // whole per-member loop silently gave up — dropping that class's
        // methods AND every class after it in the file (the outer loop
        // couldn't resync either). Minimal shape reproducing it: a pragma,
        // THEN ivars, THEN a real method, THEN a second class.
        let src = "Object subclass: First [\n    <classVars: nil>\n    | Block |\n    setBlock: aBlock [ Block := aBlock ]\n]\nObject subclass: Second [\n    two [ ^2 ]\n]\n";
        let classes = parse_mst_source(src);
        assert_eq!(
            classes.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["First", "Second"],
            "both classes must survive — a pragma before the ivar list must not truncate the file"
        );
        assert_eq!(classes[0].instance_vars, "Block");
        assert_eq!(
            classes[0].methods.iter().map(|m| m.selector.as_str()).collect::<Vec<_>>(),
            vec!["setBlock:"]
        );
        assert_eq!(
            classes[1].methods.iter().map(|m| m.selector.as_str()).collect::<Vec<_>>(),
            vec!["two"]
        );
    }

    #[test]
    fn the_real_64_cocoaui_mst_file_yields_cocoaui_with_its_full_method_set() {
        // End-to-end regression guard on the actual corpus file, not just the
        // synthetic shape above: `CocoaUI` (and everything declared after it —
        // `CocoaToolbarAction` is first in the file) must come back, with a
        // real method count, not silently vanish.
        let text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../world/64_cocoaui.mst"),
        )
        .unwrap();
        let classes = parse_mst_source(&text);
        let names: Vec<&str> = classes.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["CocoaToolbarAction", "CocoaUI"],
            "both classes in 64_cocoaui.mst must parse"
        );
        let toolbar_action = &classes[0];
        assert_eq!(toolbar_action.instance_vars, "Block");
        assert_eq!(
            toolbar_action.methods.iter().map(|m| m.selector.as_str()).collect::<Vec<_>>(),
            vec!["on:", "setBlock:", "macvmAction:"]
        );
        let cocoa_ui = &classes[1];
        assert!(
            cocoa_ui.methods.len() > 20,
            "CocoaUI has dozens of real methods (CG2/CG4/CG5/CG6), got {}",
            cocoa_ui.methods.len()
        );
        assert!(cocoa_ui.methods.iter().any(|m| m.selector == "title" && m.is_class_side));
        assert!(cocoa_ui.methods.iter().any(|m| m.selector == "startup" && m.is_class_side));
    }
}
