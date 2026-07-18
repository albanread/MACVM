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
    while i < chars.len() {
        let c = chars[i];
        let prev_word = i > 0 && is_word(chars[i - 1]);
        // "comment" — to the next double quote (no escape, as the web rule).
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
                continue;
            }
            out.push((u16_at[i], u16_at[chars.len()] - u16_at[i], Kind::Str));
            break;
        }
        // #symbol
        if c == '#' && i + 1 < chars.len() && is_ident_start(chars[i + 1]) {
            let mut j = i + 1;
            while j < chars.len() && is_word(chars[j]) {
                j += 1;
            }
            out.push((u16_at[i], u16_at[j] - u16_at[i], Kind::Symbol));
            i = j;
            continue;
        }
        // ident… — keyword (trailing ':') or pseudo, both word-bounded left.
        if is_ident_start(c) && !prev_word {
            let mut j = i + 1;
            while j < chars.len() && is_word(chars[j]) {
                j += 1;
            }
            if j < chars.len() && chars[j] == ':' {
                out.push((u16_at[i], u16_at[j + 1] - u16_at[i], Kind::Keyword));
                i = j + 1;
                continue;
            }
            let word: String = chars[i..j].iter().collect();
            if PSEUDO.contains(&word.as_str()) {
                out.push((u16_at[i], u16_at[j] - u16_at[i], Kind::Pseudo));
            }
            i = j;
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
        } else {
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
