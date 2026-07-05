//! Vendored from `rust-tcl` (locus sister repo) — see `VENDOR.md`.
//! `crate::` rewritten to `crate::vendor::rust_tcl::`; otherwise byte-identical to upstream `src/expr.rs`.

use crate::vendor::rust_tcl::error::{Error, Result};
use crate::vendor::rust_tcl::value::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Expr {
    Int(i64),
    Literal(String),
    Var(String),
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

pub fn parse_expr(source: &str) -> Result<Expr> {
    Parser::new(source).parse()
}

impl Expr {
    pub fn eval<F>(&self, get_var: &mut F) -> Result<Value>
    where
        F: FnMut(&str) -> Option<Value>,
    {
        match self {
            Expr::Int(n) => Ok(Value::new(n.to_string())),
            Expr::Literal(text) => Ok(Value::new(text.clone())),
            Expr::Var(name) => get_var(name)
                .ok_or_else(|| Error::runtime(format!("unknown variable `{name}` in expression"))),
            Expr::Unary { op, expr } => {
                let value = expr.eval(get_var)?;
                match op {
                    UnaryOp::Neg => Ok(Value::new((-as_i64(&value)?).to_string())),
                    UnaryOp::Not => Ok(bool_value(!truthy(&value))),
                }
            }
            Expr::Binary { op, left, right } => {
                if *op == BinaryOp::And {
                    let left = left.eval(get_var)?;
                    if !truthy(&left) {
                        return Ok(bool_value(false));
                    }
                    let right = right.eval(get_var)?;
                    return Ok(bool_value(truthy(&right)));
                }
                if *op == BinaryOp::Or {
                    let left = left.eval(get_var)?;
                    if truthy(&left) {
                        return Ok(bool_value(true));
                    }
                    let right = right.eval(get_var)?;
                    return Ok(bool_value(truthy(&right)));
                }

                let left = left.eval(get_var)?;
                let right = right.eval(get_var)?;
                match op {
                    BinaryOp::Add => Ok(Value::new((as_i64(&left)? + as_i64(&right)?).to_string())),
                    BinaryOp::Sub => Ok(Value::new((as_i64(&left)? - as_i64(&right)?).to_string())),
                    BinaryOp::Mul => Ok(Value::new((as_i64(&left)? * as_i64(&right)?).to_string())),
                    BinaryOp::Div => {
                        let divisor = as_i64(&right)?;
                        if divisor == 0 {
                            return Err(Error::runtime("division by zero in expression"));
                        }
                        Ok(Value::new((as_i64(&left)? / divisor).to_string()))
                    }
                    BinaryOp::Eq => Ok(bool_value(left == right)),
                    BinaryOp::Ne => Ok(bool_value(left != right)),
                    BinaryOp::Lt => Ok(bool_value(as_i64(&left)? < as_i64(&right)?)),
                    BinaryOp::Le => Ok(bool_value(as_i64(&left)? <= as_i64(&right)?)),
                    BinaryOp::Gt => Ok(bool_value(as_i64(&left)? > as_i64(&right)?)),
                    BinaryOp::Ge => Ok(bool_value(as_i64(&left)? >= as_i64(&right)?)),
                    BinaryOp::And | BinaryOp::Or => unreachable!(),
                }
            }
        }
    }
}

pub fn truthy(value: &Value) -> bool {
    let s = value.as_str().trim();
    if s.is_empty() {
        return false;
    }
    !matches!(
        s.to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

fn bool_value(value: bool) -> Value {
    Value::new(if value { "1" } else { "0" })
}

fn as_i64(value: &Value) -> Result<i64> {
    value
        .as_str()
        .trim()
        .parse::<i64>()
        .map_err(|_| Error::runtime(format!("expected integer, got `{value}`")))
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Token {
    Int(i64),
    Ident(String),
    Var(String),
    Str(String),
    Op(&'static str),
    LParen,
    RParen,
    Eof,
}

struct Parser<'a> {
    lexer: ExprLexer<'a>,
    current: Token,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str) -> Self {
        let mut lexer = ExprLexer::new(source);
        let current = lexer.next_token();
        Self { lexer, current }
    }

    fn parse(mut self) -> Result<Expr> {
        let expr = self.parse_or()?;
        if self.current != Token::Eof {
            return Err(Error::runtime("trailing input in expression"));
        }
        Ok(expr)
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut left = self.parse_and()?;
        while self.take_op("||") {
            left = Expr::Binary {
                op: BinaryOp::Or,
                left: Box::new(left),
                right: Box::new(self.parse_and()?),
            };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut left = self.parse_eq()?;
        while self.take_op("&&") {
            left = Expr::Binary {
                op: BinaryOp::And,
                left: Box::new(left),
                right: Box::new(self.parse_eq()?),
            };
        }
        Ok(left)
    }

    fn parse_eq(&mut self) -> Result<Expr> {
        let mut left = self.parse_cmp()?;
        loop {
            let op = if self.take_op("==") {
                BinaryOp::Eq
            } else if self.take_op("!=") {
                BinaryOp::Ne
            } else {
                return Ok(left);
            };
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(self.parse_cmp()?),
            };
        }
    }

    fn parse_cmp(&mut self) -> Result<Expr> {
        let mut left = self.parse_add()?;
        loop {
            let op = if self.take_op("<=") {
                BinaryOp::Le
            } else if self.take_op(">=") {
                BinaryOp::Ge
            } else if self.take_op("<") {
                BinaryOp::Lt
            } else if self.take_op(">") {
                BinaryOp::Gt
            } else {
                return Ok(left);
            };
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(self.parse_add()?),
            };
        }
    }

    fn parse_add(&mut self) -> Result<Expr> {
        let mut left = self.parse_mul()?;
        loop {
            let op = if self.take_op("+") {
                BinaryOp::Add
            } else if self.take_op("-") {
                BinaryOp::Sub
            } else {
                return Ok(left);
            };
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(self.parse_mul()?),
            };
        }
    }

    fn parse_mul(&mut self) -> Result<Expr> {
        let mut left = self.parse_unary()?;
        loop {
            let op = if self.take_op("*") {
                BinaryOp::Mul
            } else if self.take_op("/") {
                BinaryOp::Div
            } else {
                return Ok(left);
            };
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(self.parse_unary()?),
            };
        }
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if self.take_op("-") {
            return Ok(Expr::Unary {
                op: UnaryOp::Neg,
                expr: Box::new(self.parse_unary()?),
            });
        }
        if self.take_op("!") {
            return Ok(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(self.parse_unary()?),
            });
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        match std::mem::replace(&mut self.current, Token::Eof) {
            Token::Int(n) => {
                self.bump();
                Ok(Expr::Int(n))
            }
            Token::Ident(text) | Token::Str(text) => {
                self.bump();
                Ok(Expr::Literal(text))
            }
            Token::Var(name) => {
                self.bump();
                Ok(Expr::Var(name))
            }
            Token::LParen => {
                self.bump();
                let expr = self.parse_or()?;
                if self.current != Token::RParen {
                    return Err(Error::runtime("expected `)` in expression"));
                }
                self.bump();
                Ok(expr)
            }
            _ => Err(Error::runtime("expected expression")),
        }
    }

    fn take_op(&mut self, expected: &str) -> bool {
        if matches!(&self.current, Token::Op(op) if *op == expected) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn bump(&mut self) {
        self.current = self.lexer.next_token();
    }
}

struct ExprLexer<'a> {
    source: &'a str,
    pos: usize,
}

impl<'a> ExprLexer<'a> {
    fn new(source: &'a str) -> Self {
        Self { source, pos: 0 }
    }

    fn next_token(&mut self) -> Token {
        self.skip_space();
        let Some(ch) = self.peek() else {
            return Token::Eof;
        };

        if ch.is_ascii_digit() {
            return self.read_int();
        }
        if ch == '$' {
            return self.read_var();
        }
        if ch == '"' {
            return self.read_str();
        }
        if is_ident_start(ch) {
            return self.read_ident();
        }
        if ch == '(' {
            self.bump();
            return Token::LParen;
        }
        if ch == ')' {
            self.bump();
            return Token::RParen;
        }

        for op in [
            "&&", "||", "==", "!=", "<=", ">=", "+", "-", "*", "/", "<", ">", "!",
        ] {
            if self.source[self.pos..].starts_with(op) {
                self.pos += op.len();
                return Token::Op(op);
            }
        }
        self.bump();
        Token::Eof
    }

    fn read_int(&mut self) -> Token {
        let start = self.pos;
        while matches!(self.peek(), Some(ch) if ch.is_ascii_digit()) {
            self.bump();
        }
        Token::Int(self.source[start..self.pos].parse().unwrap_or(0))
    }

    fn read_var(&mut self) -> Token {
        self.bump();
        if self.peek() == Some('{') {
            self.bump();
            let start = self.pos;
            while let Some(ch) = self.peek() {
                if ch == '}' {
                    let name = self.source[start..self.pos].to_string();
                    self.bump();
                    return Token::Var(name);
                }
                self.bump();
            }
            return Token::Var(self.source[start..self.pos].to_string());
        }
        let start = self.pos;
        while matches!(self.peek(), Some(ch) if is_ident_continue(ch)) {
            self.bump();
        }
        Token::Var(self.source[start..self.pos].to_string())
    }

    fn read_str(&mut self) -> Token {
        self.bump();
        let mut out = String::new();
        while let Some(ch) = self.bump() {
            match ch {
                '\\' => out.push(match self.bump() {
                    Some('n') => '\n',
                    Some('r') => '\r',
                    Some('t') => '\t',
                    Some(ch) => ch,
                    None => '\\',
                }),
                '"' => return Token::Str(out),
                _ => out.push(ch),
            }
        }
        Token::Str(out)
    }

    fn read_ident(&mut self) -> Token {
        let start = self.pos;
        while matches!(self.peek(), Some(ch) if is_ident_continue(ch)) {
            self.bump();
        }
        Token::Ident(self.source[start..self.pos].to_string())
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

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_ident_continue(ch: char) -> bool {
    is_ident_start(ch) || ch.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_evaluates_tcl_style_condition() {
        let expr = parse_expr("$x < 5 && $x != 3").unwrap();
        let value = expr
            .eval(&mut |name| {
                if name == "x" {
                    Some(Value::new("4"))
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(value.as_str(), "1");
    }
}
