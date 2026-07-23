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
    /// A Squeak/Pharo brace (dynamic) array `{ e1. e2. … }`. Unlike
    /// [`Literal::Array`] (a compile-time constant of literal elements), the
    /// elements are arbitrary EXPRESSIONS evaluated left-to-right at runtime,
    /// producing a fresh `Array` on each evaluation. Codegen desugars it to
    /// `(Array new: n)` + one `at:put:` per element (no new bytecode).
    DynArray {
        elems: Vec<Expr>,
        span: Span,
    },
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
            Expr::DynArray { span, .. } => *span,
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

/// T0′ (`docs/typechecker_design.md` §3): a captured Strongtalk-style type
/// annotation — the token text between `<` and `>` (exclusive), reconstructed
/// from the token stream rather than sliced from source (the lexer's `Span`
/// is line/col only, not a byte offset). `text` is NOT re-parsed here; T1
/// builds a real `TypeExpr` from it. Never consulted by codegen — see
/// `tests/it_frontend_golden.rs`'s annotated-vs-stripped differential, which
/// exists specifically to keep that true.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeAnnotation {
    pub text: String,
    pub span: Span,
}

/// T0′: a method's own captured signature — `None`/empty means unannotated
/// (the type system's `Dynamic`, §4). Parallel to `MethodNode::params`/
/// `temps` by index; never consulted outside `image_store`'s export and the
/// (not-yet-built) `macvm typecheck` subcommand.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MethodSignature {
    pub param_types: Vec<Option<TypeAnnotation>>,
    pub return_type: Option<TypeAnnotation>,
    pub temp_types: Vec<Option<TypeAnnotation>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MethodNode {
    pub pattern_selector: String,
    pub params: Vec<String>,
    pub primitive: Option<u16>,
    /// S20 FFI (docs/FFI.md §5/§6.3): `<primitive: FFI ...>` — a distinct,
    /// keyword-bodied pragma the ffi_gen generator already emits (Tier 1
    /// `function:`, Tier 2 `selector:`), parsed separately from the bare
    /// `<primitive: N>` integer form above (`primitive` and `ffi` are never
    /// both `Some` — `parser::parse_method_pragma`'s own two return arms).
    pub ffi: Option<FfiPragma>,
    pub temps: Vec<String>,
    pub body: Vec<Expr>,
    pub class_side: bool,
    pub span: Span,
    /// T0′: captured type annotations, if any (`docs/typechecker_design.md`).
    pub type_sig: MethodSignature,
}

/// S20 FFI: the parsed body of a `<primitive: FFI ...>` pragma (docs/FFI.md
/// §6.3's own generated syntax, verbatim) — `ret`/`args` are the raw ABI
/// shape-token strings (`"g"`/`"f"`/`"h4"`/…, docs/FFI.md §1), left as
/// `String` rather than resolved to an enum here: the AST layer has no
/// opinion on which ret/arg classes the runtime primitive actually
/// implements yet (v1: `g`/`f`/`v` only, `codecache::ffi_stubs`) — that
/// validation belongs at the SAME later stage that resolves the target
/// symbol, not here.
#[derive(Clone, Debug, PartialEq)]
pub enum FfiPragma {
    /// Tier 1 — `<primitive: FFI function: #mmap ret: #g args: #(g g g g g g)>`.
    Function {
        name: String,
        ret: String,
        args: Vec<String>,
    },
    /// Tier 2 — `<primitive: FFI selector: #colorWithRed:green:blue:alpha:
    /// class: #NSColor classSide: true ret: #g args: #(f f f f)>`. Parsed
    /// now for completeness (ffi_gen already emits this form for its Cocoa
    /// manifest) but not yet acted on by any runtime primitive — Tier 2
    /// dispatch is S20 step 7, deliberately deferred (docs/FFI.md §3).
    Selector {
        selector: String,
        class: String,
        class_side: bool,
        ret: String,
        args: Vec<String>,
    },
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
    /// T0′: captured type annotations, parallel to `inst_vars` by index
    /// (`docs/typechecker_design.md` §3's "typed instance variables").
    pub inst_var_types: Vec<Option<TypeAnnotation>>,
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
