//! Source ▸ Format — a comment/string-preserving re-indenter for `.mst`
//! Smalltalk (the editor's Format menu item, host verb `formatSource:`).
//!
//! Deliberately NOT an AST pretty-printer: the parser drops comments, so a
//! parse → re-emit round trip would DELETE every comment — unacceptable for a
//! formatter. Instead this is a token-aware line re-indenter: every line's
//! CONTENT is preserved byte-for-byte (interior spacing, comments, strings);
//! only the LEADING indentation is normalized to 4 spaces per bracket depth
//! (`[`/`(`/`{`), with a line's leading run of closers dedenting itself — the
//! corpus's own convention (class body at 4, method bodies at 8). Lines that
//! START inside a multi-line string or comment are emitted verbatim (their
//! leading whitespace is CONTENT). Idempotent by construction.

/// Lexical state carried across lines.
#[derive(Clone, Copy, PartialEq)]
enum Lex {
    Code,
    Str,     // inside '…' ('' is an escaped quote)
    Comment, // inside "…" ("" is an escaped quote)
}

pub fn format_source(src: &str) -> String {
    let mut out = String::with_capacity(src.len() + 64);
    let mut state = Lex::Code;
    let mut depth: i32 = 0;

    for line in src.split('\n') {
        let state_at_start = state;
        let depth_at_start = depth;

        // Scan the line to update state/depth for the NEXT line, and note the
        // leading closer run (computed on the trimmed text below).
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match state {
                Lex::Str => {
                    if c == '\'' {
                        // '' = escaped quote (stay in string), lone ' = end.
                        if chars.peek() == Some(&'\'') {
                            chars.next();
                        } else {
                            state = Lex::Code;
                        }
                    }
                }
                Lex::Comment => {
                    if c == '"' {
                        if chars.peek() == Some(&'"') {
                            chars.next();
                        } else {
                            state = Lex::Code;
                        }
                    }
                }
                Lex::Code => match c {
                    // $x consumes the next char VERBATIM — $" $' $[ $] must
                    // not flip state or depth.
                    '$' => {
                        chars.next();
                    }
                    '\'' => state = Lex::Str,
                    '"' => state = Lex::Comment,
                    '[' | '(' | '{' => depth += 1,
                    ']' | ')' | '}' => depth = (depth - 1).max(0),
                    _ => {}
                },
            }
        }

        if state_at_start != Lex::Code {
            // Mid-string / mid-comment: leading whitespace is content.
            out.push_str(line.trim_end());
            out.push('\n');
            continue;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            out.push('\n');
            continue;
        }

        // A leading run of closers (possibly space-separated, e.g. "] ].")
        // dedents the line itself.
        let mut leading_closers = 0i32;
        for c in trimmed.chars() {
            match c {
                ']' | ')' | '}' => leading_closers += 1,
                ' ' | '\t' => {}
                _ => break,
            }
        }
        let level = (depth_at_start - leading_closers).max(0) as usize;
        for _ in 0..level {
            out.push_str("    ");
        }
        out.push_str(trimmed);
        out.push('\n');
    }

    // split('\n') yields a trailing "" for a newline-terminated source, which
    // the loop turned into its own '\n' — collapse to exactly one final '\n'.
    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::format_source;

    #[test]
    fn normalizes_indent_by_bracket_depth() {
        let messy = "Object subclass: Foo [\n| a |\n  bar [\n^a + 1\n]\n baz: x [\nx > 0 ifTrue: [\n^x\n].\n^0\n]\n]\n";
        let formatted = format_source(messy);
        let expect = "Object subclass: Foo [\n    | a |\n    bar [\n        ^a + 1\n    ]\n    baz: x [\n        x > 0 ifTrue: [\n            ^x\n        ].\n        ^0\n    ]\n]\n";
        assert_eq!(formatted, expect);
    }

    #[test]
    fn is_idempotent_and_preserves_multiline_strings_and_comments() {
        let src = "Object subclass: Foo [\n    \"a comment\n  with ODD leading spaces kept\n\"\n    greet [\n        ^'multi\n  line keeps\n    its spaces'\n    ]\n]\n";
        let once = format_source(src);
        let twice = format_source(&once);
        assert_eq!(once, twice, "formatting must be idempotent");
        // The mid-string/mid-comment lines survive verbatim.
        assert!(once.contains("\n  with ODD leading spaces kept"));
        assert!(once.contains("\n  line keeps"));
        assert!(once.contains("\n    its spaces'"));
    }

    #[test]
    fn char_literals_do_not_confuse_the_lexer() {
        // $" $' $[ $] as character literals — depth/state must not shift.
        let src = "Object subclass: Q [\n    m [\n        ^$[ = $] or: [ $\" = $' ]\n    ]\n]\n";
        assert_eq!(format_source(src), src, "already-canonical text is untouched");
    }
}
