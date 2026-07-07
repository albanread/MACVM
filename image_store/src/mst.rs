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
        match self.peek() {
            None | Some(']') => None,
            Some(c) if Self::is_binary_char(c) => {
                let start = self.pos;
                while matches!(self.peek(), Some(c) if Self::is_binary_char(c)) {
                    self.advance();
                }
                let sel = self.s[start..self.pos].to_string();
                self.read_identifier(); // the one binary parameter — discarded, not needed for the selector string
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
            if pending_comment.is_none() {
                pending_comment = Some(c);
            }
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
        // Optional "| a b c |" instance-variable declaration, only legal
        // as the very first thing in the class body.
        sc.skip_ws_and_comments();
        if sc.peek() == Some('|') {
            let save = sc.pos;
            sc.advance();
            let mut vars = Vec::new();
            loop {
                sc.skip_ws_and_comments();
                if sc.peek() == Some('|') {
                    sc.advance();
                    instance_vars = vars.join(" ");
                    break;
                }
                match sc.read_identifier() {
                    Some(v) => vars.push(v),
                    None => {
                        sc.pos = save; // not actually an ivar list — rewind
                        break;
                    }
                }
            }
        }

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
}
