//! Vendored from `rust-tcl` (locus sister repo) — see `VENDOR.md`.
//! `crate::` rewritten to `crate::vendor::rust_tcl::`; otherwise byte-identical to upstream `src/parser.rs`.

use crate::vendor::rust_tcl::ast::{Command, Script, Word, WordPart};
use crate::vendor::rust_tcl::error::{Error, Result};
use crate::vendor::rust_tcl::lexer::{Token, TokenKind, WordKind};
use crate::vendor::rust_tcl::span::Span;

pub fn parse(tokens: &[Token]) -> Result<Script> {
    Parser::new(tokens).parse()
}

pub fn parse_source(source: &str) -> Result<Script> {
    let tokens = crate::vendor::rust_tcl::lexer::lex(source)?;
    parse(&tokens)
}

struct Parser<'a> {
    tokens: &'a [Token],
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self { tokens }
    }

    fn parse(&self) -> Result<Script> {
        let mut commands = Vec::new();
        let mut words = Vec::new();
        let mut command_span: Option<Span> = None;

        for token in self.tokens {
            match &token.kind {
                TokenKind::Word(word) => {
                    let parsed = match word.kind {
                        WordKind::Braced => Word::literal(&word.text, token.span),
                        WordKind::Bare | WordKind::Quoted => {
                            parse_word_parts(&word.text, token.span)?
                        }
                    };
                    command_span = Some(match command_span {
                        Some(span) => span.join(token.span),
                        None => token.span,
                    });
                    words.push(parsed);
                }
                TokenKind::CommandSep | TokenKind::Eof => {
                    if !words.is_empty() {
                        commands.push(Command {
                            words: std::mem::take(&mut words),
                            span: command_span.take().expect("words imply a span"),
                        });
                    }
                    if matches!(token.kind, TokenKind::Eof) {
                        break;
                    }
                }
            }
        }

        Ok(Script { commands })
    }
}

fn parse_word_parts(text: &str, span: Span) -> Result<Word> {
    let mut scanner = PartScanner::new(text, span);
    let mut parts = Vec::new();
    let mut literal = String::new();

    while let Some(ch) = scanner.peek() {
        match ch {
            '\\' => {
                scanner.bump();
                literal.push(scanner.read_escape());
            }
            '$' => {
                scanner.bump();
                match scanner.read_variable()? {
                    Some(name) => {
                        flush_literal(&mut parts, &mut literal);
                        parts.push(WordPart::Var(name));
                    }
                    None => literal.push('$'),
                }
            }
            '[' => {
                flush_literal(&mut parts, &mut literal);
                let inner = scanner.read_command_substitution()?;
                let script = parse_source(&inner)?;
                parts.push(WordPart::Command(Box::new(script)));
            }
            _ => {
                scanner.bump();
                literal.push(ch);
            }
        }
    }

    flush_literal(&mut parts, &mut literal);
    if parts.is_empty() {
        parts.push(WordPart::Text(String::new()));
    }
    Ok(Word { parts, span })
}

fn flush_literal(parts: &mut Vec<WordPart>, literal: &mut String) {
    if !literal.is_empty() {
        parts.push(WordPart::Text(std::mem::take(literal)));
    }
}

struct PartScanner<'a> {
    source: &'a str,
    span: Span,
    pos: usize,
}

impl<'a> PartScanner<'a> {
    fn new(source: &'a str, span: Span) -> Self {
        Self {
            source,
            span,
            pos: 0,
        }
    }

    fn read_escape(&mut self) -> char {
        let Some((_, ch)) = self.bump() else {
            return '\\';
        };
        match ch {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            other => other,
        }
    }

    fn read_variable(&mut self) -> Result<Option<String>> {
        if self.peek() == Some('{') {
            self.bump();
            let start = self.pos;
            while let Some((end, ch)) = self.bump() {
                if ch == '}' {
                    let name = &self.source[start..end];
                    if name.is_empty() {
                        return Err(Error::parse(
                            self.local_span(start, end),
                            "empty variable name",
                        ));
                    }
                    return Ok(Some(name.to_string()));
                }
            }
            return Err(Error::parse(
                self.local_span(start, self.pos),
                "unterminated braced variable name",
            ));
        }

        let Some(ch) = self.peek() else {
            return Ok(None);
        };
        if !is_ident_start(ch) {
            return Ok(None);
        }
        let start = self.pos;
        self.bump();
        while matches!(self.peek(), Some(ch) if is_ident_continue(ch)) {
            self.bump();
        }
        Ok(Some(self.source[start..self.pos].to_string()))
    }

    fn read_command_substitution(&mut self) -> Result<String> {
        let open = self.pos;
        self.expect('[');
        let content_start = self.pos;
        let mut depth = 1usize;

        while let Some((ch_start, ch)) = self.bump() {
            match ch {
                '\\' => {
                    self.bump();
                }
                '{' => self.skip_brace(open)?,
                '"' => self.skip_quote(open)?,
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(self.source[content_start..ch_start].to_string());
                    }
                }
                _ => {}
            }
        }

        Err(Error::parse(
            self.local_span(open, self.pos),
            "unterminated command substitution",
        ))
    }

    fn skip_brace(&mut self, open: usize) -> Result<()> {
        let mut depth = 1usize;
        while let Some((_, ch)) = self.bump() {
            match ch {
                '\\' => {
                    self.bump();
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
        Err(Error::parse(
            self.local_span(open, self.pos),
            "unterminated braced sequence",
        ))
    }

    fn skip_quote(&mut self, open: usize) -> Result<()> {
        while let Some((_, ch)) = self.bump() {
            match ch {
                '\\' => {
                    self.bump();
                }
                '"' => return Ok(()),
                _ => {}
            }
        }
        Err(Error::parse(
            self.local_span(open, self.pos),
            "unterminated quoted sequence",
        ))
    }

    fn expect(&mut self, expected: char) {
        let (_, actual) = self.bump().expect("expected character");
        debug_assert_eq!(actual, expected);
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

    fn local_span(&self, start: usize, end: usize) -> Span {
        Span::new(self.span.start + start, self.span.start + end)
    }
}

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_ident_continue(ch: char) -> bool {
    is_ident_start(ch) || ch.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vendor::rust_tcl::lexer::lex;

    #[test]
    fn parses_var_and_command_parts() {
        let tokens = lex(r#"puts "x=$x y=[add 1 2]""#).unwrap();
        let script = parse(&tokens).unwrap();
        let word = &script.commands[0].words[1];
        assert!(matches!(word.parts[1], WordPart::Var(_)));
        assert!(matches!(word.parts[3], WordPart::Command(_)));
    }

    #[test]
    fn braces_disable_substitution() {
        let tokens = lex("puts {$x [add 1 2]}").unwrap();
        let script = parse(&tokens).unwrap();
        assert_eq!(
            script.commands[0].words[1].static_text(),
            Some("$x [add 1 2]")
        );
    }
}
