//! AST node types (`sprint_s05_detail.md` §Design "AST" — verbatim).

use super::lexer::Span;

#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    Int(i64),
    /// Base-256, little-endian magnitude, no leading (high) zero byte.
    BigInt {
        negative: bool,
        digits: Vec<u8>,
    },
    Float(f64),
    Char(char),
    Str(String),
    Symbol(String),
    Array(Vec<Literal>),
    ByteArray(Vec<u8>),
    Nil,
    True,
    False,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    SelfRef(Span),
    Var {
        name: String,
        span: Span,
    },
    Lit {
        value: Literal,
        span: Span,
    },
    Assign {
        name: String,
        value: Box<Expr>,
        span: Span,
    },
    Send {
        receiver: Box<Expr>,
        selector: String,
        args: Vec<Expr>,
        is_super: bool,
        span: Span,
    },
    Cascade {
        receiver: Box<Expr>,
        segments: Vec<Expr>,
        span: Span,
    },
    /// Marker: the innermost receiver inside each cascade segment.
    CascadeRcvr(Span),
    Block(Box<BlockNode>),
    /// Statement position only (parser-guaranteed; codegen asserts it).
    Return {
        value: Box<Expr>,
        span: Span,
    },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::SelfRef(s) => *s,
            Expr::Var { span, .. } => *span,
            Expr::Lit { span, .. } => *span,
            Expr::Assign { span, .. } => *span,
            Expr::Send { span, .. } => *span,
            Expr::Cascade { span, .. } => *span,
            Expr::CascadeRcvr(s) => *s,
            Expr::Block(b) => b.span,
            Expr::Return { span, .. } => *span,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BlockNode {
    pub params: Vec<String>,
    pub temps: Vec<String>,
    pub body: Vec<Expr>,
    pub span: Span,
    /// Filled by capture analysis (`frontend::capture`).
    pub scope_id: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MethodNode {
    pub pattern_selector: String,
    pub params: Vec<String>,
    pub primitive: Option<u16>,
    pub temps: Vec<String>,
    pub body: Vec<Expr>,
    pub class_side: bool,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Indexable {
    Oops,
    Bytes,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClassDefNode {
    pub superclass: String,
    pub name: String,
    pub indexable: Option<Indexable>,
    pub inst_vars: Vec<String>,
    pub class_vars: Vec<String>,
    pub methods: Vec<MethodNode>,
    pub span: Span,
}

/// A top-level file item (`sprint_s05_detail.md` grammar: `top_item`).
#[derive(Clone, Debug, PartialEq)]
pub enum TopItem {
    ClassDef(ClassDefNode),
    /// `do_it = statement "."` — executed immediately, in file order.
    DoIt(Expr),
}
