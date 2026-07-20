//! Smalltalk syntax-colour spans (CG8) — the SAME six categories and the SAME
//! deliberately-simple rules as the web GUI's `smtk.js` highlighter
//! (`SMALLTALK_TOKEN_RE`, which its own comment blesses as "deliberately the
//! only tokenizer... upgrade to parser-driven highlighting once there's a real
//! one"): comments `"[^"]*"`, strings `'[^']*'`, symbols `#ident`, keywords
//! `ident:`, the pseudo-variables, and word-bounded numbers. One product, one
//! rule set, two renderers. (The compiler's `frontend::lexer` is value-
//! oriented — decoded strings, skipped comments, line/col spans — so it cannot
//! answer colour extents; this scanner exists for the same reason the JS one
//! does.)
//!
//! Offsets are UTF-16 code units — `NSRange` for attributed strings counts
//! UTF-16, and a byte-offset mismatch on a non-ASCII comment would misplace
//! colours at best and raise `NSRangeException` inside AppKit at worst.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Comment,
    Str,
    Symbol,
    Keyword,
    Pseudo,
    Number,
    /// A send whose selector the image does not know (workspace mode only —
    /// `spans_utf16_checked`): a keyword token that is no known selector's
    /// part, or a send-position identifier that is no known unary selector.
    /// Painted red — the typo/nonexistent-message flag.
    Unknown,
}

const PSEUDO: [&str; 6] = ["self", "super", "true", "false", "nil", "thisContext"];

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}
fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

/// Colour spans as `(utf16_start, utf16_len, kind)`, non-overlapping, in
/// order. Mirrors the web regex's alternation: at each position the first
/// matching category wins and the scan resumes after it.
pub fn spans_utf16(text: &str) -> Vec<(u64, u64, Kind)> {
    spans_utf16_checked(text, None)
}

/// The workspace variant: with `sets`, sends whose selector the image doesn't
/// know become [`Kind::Unknown`] spans. Send position is tracked LEXICALLY —
/// Smalltalk's grammar makes that sound: an identifier FOLLOWING a primary
/// (ident, literal, `)`, `]`, pseudo, `;`-cascade) is a unary send; the first
/// identifier of an expression is a variable/global and is never flagged.
/// Keyword tokens are checked against the PARTS of known keyword selectors
/// (so `at: 1 put: 2` is clean but a typo'd `prinString:` flags) — per-token,
/// so a novel combination of individually-known parts passes: this catches
/// typos and nonexistent messages, not type errors. Inside `#(…)`/`#[…]`
/// literal arrays nothing is a send, so nothing flags; `x:= 3` (no space) is
/// exempted as an assignment, not a keyword send.
pub fn spans_utf16_checked(
    text: &str,
    sets: Option<&crate::symbols::SendSets>,
) -> Vec<(u64, u64, Kind)> {
    let chars: Vec<char> = text.chars().collect();
    // utf16 offset of chars[i]; one extra entry for end-of-text.
    let mut u16_at = Vec::with_capacity(chars.len() + 1);
    let mut acc = 0u64;
    for &c in &chars {
        u16_at.push(acc);
        acc += c.len_utf16() as u64;
    }
    u16_at.push(acc);

    let mut out = Vec::new();
    let mut i = 0usize;
    // Send-position state (only consulted when `sets` is given): true when the
    // PREVIOUS token could end a primary expression — an ident that follows is
    // then a unary send. `;` re-opens send position (cascade). Inside a
    // `#(…)`/`#[…]` literal array nothing is a send.
    let mut prev_primary = false;
    let mut literal_depth = 0usize;
    while i < chars.len() {
        let c = chars[i];
        let prev_word = i > 0 && is_word(chars[i - 1]);
        // "comment" — to the next double quote (no escape, as the web rule).
        // Transparent: does not change send-position state.
        if c == '"' {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '"' {
                j += 1;
            }
            if j < chars.len() {
                out.push((u16_at[i], u16_at[j + 1] - u16_at[i], Kind::Comment));
                i = j + 1;
                continue;
            }
            // unterminated: colour to EOF (the web leaves it uncoloured; a
            // comment being typed reads better coloured — same category).
            out.push((u16_at[i], u16_at[chars.len()] - u16_at[i], Kind::Comment));
            break;
        }
        // 'string'
        if c == '\'' {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '\'' {
                j += 1;
            }
            if j < chars.len() {
                out.push((u16_at[i], u16_at[j + 1] - u16_at[i], Kind::Str));
                i = j + 1;
                prev_primary = true;
                continue;
            }
            out.push((u16_at[i], u16_at[chars.len()] - u16_at[i], Kind::Str));
            break;
        }
        // #( / #[ — a literal array: suppress send checks until it closes.
        if c == '#' && i + 1 < chars.len() && (chars[i + 1] == '(' || chars[i + 1] == '[') {
            literal_depth += 1;
            i += 2;
            prev_primary = false;
            continue;
        }
        // #symbol
        if c == '#' && i + 1 < chars.len() && is_ident_start(chars[i + 1]) {
            let mut j = i + 1;
            while j < chars.len() && is_word(chars[j]) {
                j += 1;
            }
            // Keyword symbols: swallow the colon runs (`#at:put:`) so the
            // colons can't disturb the state machine.
            while j < chars.len() && chars[j] == ':' {
                j += 1;
                while j < chars.len() && is_word(chars[j]) {
                    j += 1;
                }
            }
            out.push((u16_at[i], u16_at[j] - u16_at[i], Kind::Symbol));
            i = j;
            prev_primary = true;
            continue;
        }
        // $x — a character literal: the next char is CONTENT ($a, $", $[).
        if c == '$' && i + 1 < chars.len() {
            i += 2;
            prev_primary = true;
            continue;
        }
        // ident… — keyword (trailing ':') or pseudo, both word-bounded left.
        if is_ident_start(c) && !prev_word {
            let mut j = i + 1;
            while j < chars.len() && is_word(chars[j]) {
                j += 1;
            }
            if j < chars.len() && chars[j] == ':' {
                // `x:= 3` (no space before :=) is an assignment, not a send.
                let is_assign = j + 1 < chars.len() && chars[j + 1] == '=';
                let word: String = chars[i..=j].iter().collect();
                let kind = match sets {
                    Some(s)
                        if literal_depth == 0
                            && !is_assign
                            && !s.keyword_parts.contains(&word) =>
                    {
                        Kind::Unknown
                    }
                    _ => Kind::Keyword,
                };
                out.push((u16_at[i], u16_at[j + 1] - u16_at[i], kind));
                i = j + 1;
                prev_primary = false;
                continue;
            }
            let word: String = chars[i..j].iter().collect();
            if PSEUDO.contains(&word.as_str()) {
                out.push((u16_at[i], u16_at[j] - u16_at[i], Kind::Pseudo));
            } else if let Some(s) = sets {
                // A lowercase ident right after a primary is a unary SEND;
                // uppercase idents are globals/classes (never send-position).
                if literal_depth == 0
                    && prev_primary
                    && c.is_ascii_lowercase()
                    && !s.unary.contains(&word)
                {
                    out.push((u16_at[i], u16_at[j] - u16_at[i], Kind::Unknown));
                }
            }
            i = j;
            prev_primary = true;
            continue;
        }
        // number — \b\d+(\.\d+)?\b (so `16r1F`'s parts stay uncoloured,
        // exactly as the web regex leaves them).
        if c.is_ascii_digit() && !prev_word {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j + 1 < chars.len() && chars[j] == '.' && chars[j + 1].is_ascii_digit() {
                j += 2;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
            }
            if j >= chars.len() || !is_word(chars[j]) {
                out.push((u16_at[i], u16_at[j] - u16_at[i], Kind::Number));
            } else {
                // trailing word char (e.g. `16r`): no colour, skip the run.
                while j < chars.len() && is_word(chars[j]) {
                    j += 1;
                }
            }
            i = j;
            prev_primary = true;
            continue;
        }
        // any other word run (e.g. plain identifiers) is skipped whole so its
        // interior can't start a false token; everything else advances by one.
        if is_word(c) {
            let mut j = i + 1;
            while j < chars.len() && is_word(chars[j]) {
                j += 1;
            }
            i = j;
            prev_primary = true;
        } else {
            match c {
                // whitespace is transparent — `3 zorkle` keeps `3` as the
                // primary the ident follows.
                c if c.is_whitespace() => {}
                ')' | ']' => {
                    if literal_depth > 0 {
                        literal_depth -= 1;
                    }
                    prev_primary = true;
                }
                '}' => prev_primary = true,
                ';' => prev_primary = true, // cascade: next selector is a send
                '(' | '[' | '{' => {
                    if literal_depth > 0 {
                        literal_depth += 1;
                    }
                    prev_primary = false;
                }
                _ => prev_primary = false,
            }
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<(u64, u64, Kind)> {
        spans_utf16(text)
    }

    #[test]
    fn all_six_categories_scan() {
        let spans = kinds("\"note\" 'txt' #sym at: self 42");
        assert_eq!(
            spans,
            vec![
                (0, 6, Kind::Comment),
                (7, 5, Kind::Str),
                (13, 4, Kind::Symbol),
                (18, 3, Kind::Keyword),
                (22, 4, Kind::Pseudo),
                (27, 2, Kind::Number),
            ]
        );
    }

    #[test]
    fn word_boundaries_match_the_web_rules() {
        // `selfish` is not a pseudo; `x16` is not a number; `16r1F`'s pieces
        // stay uncoloured (the \b rule); `add:` includes its colon.
        assert!(kinds("selfish x16 16r1F").is_empty());
        assert_eq!(kinds("add: 3"), vec![(0, 4, Kind::Keyword), (5, 1, Kind::Number)]);
        assert_eq!(kinds("3.14"), vec![(0, 4, Kind::Number)]);
    }

    #[test]
    fn utf16_offsets_survive_non_ascii() {
        // '𝕏' is an astral char: 2 UTF-16 units — the string after it must
        // start at 3 (1 for the space having followed 2 units), not byte-based.
        let spans = kinds("𝕏 'a'");
        assert_eq!(spans, vec![(3, 3, Kind::Str)]);
        // an em-dash inside a comment (1 unit) keeps later spans aligned.
        let spans = kinds("\"—\" 7");
        assert_eq!(spans, vec![(0, 3, Kind::Comment), (4, 1, Kind::Number)]);
    }

    #[test]
    fn unterminated_comment_colours_to_eof() {
        assert_eq!(kinds("\"open 42"), vec![(0, 8, Kind::Comment)]);
    }
}

#[cfg(test)]
mod unknown_tests {
    use super::*;
    use crate::symbols::SendSets;
    use std::collections::HashSet;

    fn sets() -> SendSets {
        let mut keyword_parts = HashSet::new();
        for p in ["at:", "put:", "ifTrue:", "show:"] {
            keyword_parts.insert(p.to_string());
        }
        let mut unary = HashSet::new();
        for u in ["printString", "size", "new", "foo"] {
            unary.insert(u.to_string());
        }
        SendSets { keyword_parts, unary }
    }

    fn unknowns(text: &str) -> Vec<String> {
        let s = sets();
        let chars: Vec<char> = text.chars().collect();
        // Recover the flagged text from utf16 offsets (ASCII in these tests).
        spans_utf16_checked(text, Some(&s))
            .into_iter()
            .filter(|(_, _, k)| *k == Kind::Unknown)
            .map(|(a, l, _)| chars[a as usize..(a + l) as usize].iter().collect())
            .collect()
    }

    #[test]
    fn unknown_unary_and_keyword_sends_flag() {
        assert_eq!(unknowns("3 zorkle"), ["zorkle"]);
        assert_eq!(unknowns("x prinString"), ["prinString"]);
        assert_eq!(unknowns("d att: 1 put: 2"), ["att:"]);
        assert_eq!(unknowns("self zork: 1"), ["zork:"]);
    }

    #[test]
    fn known_sends_variables_and_globals_stay_clean() {
        assert!(unknowns("x printString size").is_empty());
        assert!(unknowns("d at: 1 put: 2").is_empty());
        assert!(unknowns("Transcript show: 'hi'").is_empty(), "global receiver + known keyword");
        assert!(unknowns("| zorp | zorp := 3. zorp printString").is_empty(), "declared temp never flags");
        assert!(unknowns("x foo; foo").is_empty(), "cascade selector known");
        assert_eq!(unknowns("x foo; zork"), ["zork"], "cascade selector unknown");
    }

    #[test]
    fn literal_arrays_char_literals_and_assignments_are_exempt() {
        assert!(unknowns("#(zork blah at:then:)").is_empty(), "literal array contents are data");
        assert!(unknowns("#(a #(b zork) c)").is_empty(), "nested literal array");
        assert!(unknowns("x := $a").is_empty());
        assert!(unknowns("x:= 3").is_empty(), "no-space assignment is not a keyword send");
        assert!(unknowns("[ :each | each printString ]").is_empty(), "block arg is not a send");
        assert_eq!(unknowns("[ :each | each zork ]"), ["zork"], "send inside a block still checks");
    }

    #[test]
    fn plain_mode_never_flags() {
        assert!(
            spans_utf16("3 zorkle qux: 9")
                .into_iter()
                .all(|(_, _, k)| k != Kind::Unknown),
            "spans_utf16 (no sets) must behave exactly as before"
        );
    }
}
