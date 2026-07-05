//! Vendored from `rust-tcl` (locus sister repo) — see `VENDOR.md`.
//! `crate::` rewritten to `crate::vendor::rust_tcl::`; otherwise byte-identical to upstream `src/list_value.rs`.

use crate::vendor::rust_tcl::error::{Error, Result};

pub fn parse_list(source: &str) -> Result<Vec<String>> {
    let mut parser = ListParser::new(source);
    parser.parse()
}

pub fn format_list(values: &[String]) -> String {
    values
        .iter()
        .map(|value| format_element(value))
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_element(value: &str) -> String {
    if value.is_empty() {
        return "{}".to_string();
    }
    if value
        .chars()
        .all(|ch| !ch.is_whitespace() && !matches!(ch, '{' | '}' | '"' | ';' | '$' | '[' | ']'))
        && !value.starts_with('#')
    {
        return value.to_string();
    }
    let mut out = String::from("{");
    for ch in value.chars() {
        match ch {
            '\\' | '{' | '}' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out.push('}');
    out
}

struct ListParser<'a> {
    source: &'a str,
    pos: usize,
}

impl<'a> ListParser<'a> {
    fn new(source: &'a str) -> Self {
        Self { source, pos: 0 }
    }

    fn parse(&mut self) -> Result<Vec<String>> {
        let mut values = Vec::new();
        loop {
            self.skip_space();
            if self.peek().is_none() {
                return Ok(values);
            }
            values.push(self.read_element()?);
        }
    }

    fn read_element(&mut self) -> Result<String> {
        match self.peek() {
            Some('{') => self.read_braced(),
            Some('"') => self.read_quoted(),
            Some(_) => self.read_bare(),
            None => Ok(String::new()),
        }
    }

    fn read_braced(&mut self) -> Result<String> {
        self.bump();
        let mut depth = 1usize;
        let mut out = String::new();
        while let Some(ch) = self.bump() {
            match ch {
                '\\' => match self.bump() {
                    Some(next @ ('\\' | '{' | '}')) => out.push(next),
                    Some(next) => {
                        out.push('\\');
                        out.push(next);
                    }
                    None => out.push('\\'),
                },
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
        Err(Error::runtime("unterminated braced list element"))
    }

    fn read_quoted(&mut self) -> Result<String> {
        self.bump();
        let mut out = String::new();
        while let Some(ch) = self.bump() {
            match ch {
                '\\' => out.push(read_escape(self.bump())),
                '"' => return Ok(out),
                _ => out.push(ch),
            }
        }
        Err(Error::runtime("unterminated quoted list element"))
    }

    fn read_bare(&mut self) -> Result<String> {
        let mut out = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_whitespace() {
                break;
            }
            self.bump();
            match ch {
                '\\' => out.push(read_escape(self.bump())),
                _ => out.push(ch),
            }
        }
        Ok(out)
    }

    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(ch) if ch.is_whitespace()) {
            self.bump();
        }
    }

    fn peek(&self) -> Option<char> {
        self.source[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }
}

fn read_escape(ch: Option<char>) -> char {
    match ch {
        Some('n') => '\n',
        Some('r') => '\r',
        Some('t') => '\t',
        Some(ch) => ch,
        None => '\\',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_spaced_elements() {
        let values = vec!["alpha".to_string(), "two words".to_string()];
        let encoded = format_list(&values);
        assert_eq!(parse_list(&encoded).unwrap(), values);
    }
}
