//! Vendored from `rust-tcl` (locus sister repo) — see `VENDOR.md`.
//! `crate::` rewritten to `crate::vendor::rust_tcl::`; otherwise byte-identical to upstream `src/lexer.rs`.

use crate::vendor::rust_tcl::error::{Error, Result};
use crate::vendor::rust_tcl::span::Span;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WordKind {
    Bare,
    Quoted,
    Braced,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WordToken {
    pub text: String,
    pub kind: WordKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TokenKind {
    Word(WordToken),
    CommandSep,
    Eof,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

pub fn lex(source: &str) -> Result<Vec<Token>> {
    Lexer::new(source).lex()
}

struct Lexer<'a> {
    source: &'a str,
    pos: usize,
    at_command_start: bool,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            pos: 0,
            at_command_start: true,
        }
    }

    fn lex(mut self) -> Result<Vec<Token>> {
        let mut tokens = Vec::new();
        loop {
            self.skip_inline_space();
            let Some(ch) = self.peek() else {
                tokens.push(Token {
                    kind: TokenKind::Eof,
                    span: Span::new(self.pos, self.pos),
                });
                return Ok(tokens);
            };

            if self.at_command_start && ch == '#' {
                self.skip_comment();
                continue;
            }

            if ch == '\n' || ch == ';' {
                let start = self.pos;
                self.bump();
                self.at_command_start = true;
                tokens.push(Token {
                    kind: TokenKind::CommandSep,
                    span: Span::new(start, self.pos),
                });
                continue;
            }

            let token = self.read_word()?;
            self.at_command_start = false;
            tokens.push(token);
        }
    }

    fn read_word(&mut self) -> Result<Token> {
        let start = self.pos;
        let ch = self.peek().expect("read_word requires a character");
        let (kind, text) = match ch {
            '{' => {
                self.bump();
                (WordKind::Braced, self.read_braced_word(start)?)
            }
            '"' => {
                self.bump();
                (WordKind::Quoted, self.read_quoted_word(start)?)
            }
            _ => (WordKind::Bare, self.read_bare_word()?),
        };
        Ok(Token {
            kind: TokenKind::Word(WordToken { text, kind }),
            span: Span::new(start, self.pos),
        })
    }

    fn read_braced_word(&mut self, start: usize) -> Result<String> {
        let mut depth = 1usize;
        let mut out = String::new();
        while let Some((_, ch)) = self.bump() {
            match ch {
                '\\' => {
                    out.push(ch);
                    if let Some((_, next)) = self.bump() {
                        out.push(next);
                    }
                }
                '{' => {
                    depth += 1;
                    out.push(ch);
                }
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(out);
                    }
                    out.push(ch);
                }
                _ => out.push(ch),
            }
        }
        Err(Error::lex(
            Span::new(start, self.pos),
            "unterminated braced word",
        ))
    }

    fn read_quoted_word(&mut self, start: usize) -> Result<String> {
        let mut out = String::new();
        while let Some((_, ch)) = self.bump() {
            match ch {
                '\\' => {
                    out.push(ch);
                    if let Some((_, next)) = self.bump() {
                        out.push(next);
                    }
                }
                '[' => out.push_str(&self.consume_bracket_after_open()?),
                '"' => return Ok(out),
                _ => out.push(ch),
            }
        }
        Err(Error::lex(
            Span::new(start, self.pos),
            "unterminated quoted word",
        ))
    }

    fn read_bare_word(&mut self) -> Result<String> {
        let mut out = String::new();
        while let Some(ch) = self.peek() {
            if is_inline_space(ch) || ch == '\n' || ch == ';' {
                break;
            }
            let (_, ch) = self.bump().expect("peeked character vanished");
            match ch {
                '\\' => {
                    out.push(ch);
                    if let Some((_, next)) = self.bump() {
                        out.push(next);
                    }
                }
                '[' => out.push_str(&self.consume_bracket_after_open()?),
                _ => out.push(ch),
            }
        }
        Ok(out)
    }

    fn consume_bracket_after_open(&mut self) -> Result<String> {
        let start = self.pos.saturating_sub(1);
        let mut out = String::from("[");
        let mut depth = 1usize;
        while let Some((_, ch)) = self.bump() {
            match ch {
                '\\' => {
                    out.push(ch);
                    if let Some((_, next)) = self.bump() {
                        out.push(next);
                    }
                }
                '{' => {
                    out.push(ch);
                    self.append_brace_tail(&mut out, start)?;
                }
                '"' => {
                    out.push(ch);
                    self.append_quote_tail(&mut out, start)?;
                }
                '[' => {
                    depth += 1;
                    out.push(ch);
                }
                ']' => {
                    depth -= 1;
                    out.push(ch);
                    if depth == 0 {
                        return Ok(out);
                    }
                }
                _ => out.push(ch),
            }
        }
        Err(Error::lex(
            Span::new(start, self.pos),
            "unterminated command substitution",
        ))
    }

    fn append_brace_tail(&mut self, out: &mut String, start: usize) -> Result<()> {
        let mut depth = 1usize;
        while let Some((_, ch)) = self.bump() {
            out.push(ch);
            match ch {
                '\\' => {
                    if let Some((_, next)) = self.bump() {
                        out.push(next);
                    }
                }
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
        Err(Error::lex(
            Span::new(start, self.pos),
            "unterminated braced sequence",
        ))
    }

    fn append_quote_tail(&mut self, out: &mut String, start: usize) -> Result<()> {
        while let Some((_, ch)) = self.bump() {
            match ch {
                '\\' => {
                    out.push(ch);
                    if let Some((_, next)) = self.bump() {
                        out.push(next);
                    }
                }
                '[' => out.push_str(&self.consume_bracket_after_open()?),
                '"' => {
                    out.push(ch);
                    return Ok(());
                }
                _ => out.push(ch),
            }
        }
        Err(Error::lex(
            Span::new(start, self.pos),
            "unterminated quoted sequence",
        ))
    }

    fn skip_inline_space(&mut self) {
        while matches!(self.peek(), Some(ch) if is_inline_space(ch)) {
            self.bump();
        }
    }

    fn skip_comment(&mut self) {
        while matches!(self.peek(), Some(ch) if ch != '\n') {
            self.bump();
        }
    }

    fn peek(&self) -> Option<char> {
        self.source[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<(usize, char)> {
        let ch = self.peek()?;
        let start = self.pos;
        self.pos += ch.len_utf8();
        Some((start, ch))
    }
}

fn is_inline_space(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\r')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_braced_words_literal() {
        let tokens = lex("set x {hello $name [ignored]}").unwrap();
        assert_eq!(
            tokens[2].kind,
            TokenKind::Word(WordToken {
                text: "hello $name [ignored]".into(),
                kind: WordKind::Braced,
            })
        );
    }

    #[test]
    fn command_substitution_may_contain_spaces() {
        let tokens = lex("set x [add 1 2]").unwrap();
        assert_eq!(
            tokens[2].kind,
            TokenKind::Word(WordToken {
                text: "[add 1 2]".into(),
                kind: WordKind::Bare,
            })
        );
    }
}
