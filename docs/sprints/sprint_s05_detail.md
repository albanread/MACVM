# Sprint S05 — Source compiler

Objective: replace hand-assembled bytecode with a full `.mst` pipeline — lexer,
parser, AST, capture analysis, and a single-pass code generator producing
`CompiledMethod` objects — plus the class-definition loader, `world.list`
processing, and the `macvm run` / `macvm repl` CLI. Implements SPEC §1
(language), §4.5 (source compiler), §3.2 step 2 (world load), and the codegen
contracts of §4.1–§4.4.

## Prerequisites

From prior sprints (all must exist and be green):
- `src/oops/` — `Oop`, typed wrappers, `Mark`, `Format`, `layout.rs` (S0).
- `src/memory/` — `Universe` with genesis klasses, allocator, symbol interning
  (S1). Instance creation for `Slots`/`IndexableOops`/`IndexableBytes`/
  `Method`/`Klass`/`Double` formats.
- `src/bytecode/` — opcode enum, operand decode, `BytecodeBuilder`,
  disassembler (S2). The codegen emits through `BytecodeBuilder` (promoted
  from test-only to library code this sprint).
- `src/interpreter/` — full send protocol, ICs, primitives (S3); closures,
  Contexts, NLR, `ensure:` (S4).
- `src/runtime/` — `VmState`, lookup, primitive table, `PrimResult`.

## Deliverables

- `src/frontend/lexer.rs` — token enum + hand-written lexer with spans.
- `src/frontend/ast.rs` — AST node types (below, verbatim).
- `src/frontend/parser.rs` — recursive-descent parser, fail-fast errors.
- `src/frontend/capture.rs` — scope tree + capture analysis.
- `src/frontend/codegen.rs` — AST → `CompiledMethod`/`CompiledBlock`,
  literal frames, IC tables, inlined control flow.
- `src/frontend/classdef.rs` — class-definition execution (create/reopen
  klass, install methods) + top-level "doIt" execution.
- `src/frontend/world.rs` — `world/world.list` loader.
- `src/main.rs` — `macvm run <file.mst>`, `macvm repl`, `--world <dir>` flag.
- `tests/golden/*.bc.expected` corpus; every S2–S4 golden program re-expressed
  as `.mst` source.

## Design

### Data structures

#### Tokens (`lexer.rs`)

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span { pub line: u32, pub col: u32 }   // 1-based, col in bytes

#[derive(Clone, PartialEq, Debug)]
pub enum Tok {
    Ident(String),        // foo, Foo123, _x
    Keyword(String),      // "at:" — WITH the trailing colon
    BinarySel(String),    // 1–2 chars from BINARY_CHARS
    IntLit { negative: bool, radix: u8, digits: String }, // raw digits, no sign
    FloatLit(f64),        // sign already applied
    CharLit(char),
    StrLit(String),
    SymLit(String),       // #foo / #at:put: / #+ / #'quoted' — text only
    LitArrayOpen,         // #(
    ByteArrayOpen,        // #[
    LParen, RParen, LBracket, RBracket,
    VBar,                 // | when NOT lexed as BinarySel (see rule L7)
    Semi, Period, Caret, Assign /* := */, Colon,  // bare : (block params)
    Eof,
}
pub struct Lexer { /* input: &str, pos, line, col, prev_kind: PrevKind */ }
```

`BINARY_CHARS` (pinned): `+ - * / \ ~ < > = & | @ % , ? !`
A binary selector is 1 or 2 characters, each from this set, longest-match,
except the reserved sequences `:=` (never a selector; `:` is not in the set
anyway), and the lexer never merges across whitespace.

#### Lexer rules (numbered; the implementation must follow these exactly)

- **L1 Whitespace/comments**: skip spaces, tabs, newlines. Comment = `"` …
  `"`; an embedded double-quote is written `""`. Unterminated comment at EOF
  = lex error. Comments may appear anywhere whitespace may.
- **L2 Identifiers/keywords**: `[A-Za-z_][A-Za-z0-9_]*`. If immediately
  followed by `:` and NOT by `:=`, consume the colon and emit
  `Keyword("name:")`. Otherwise `Ident`.
- **L3 Integers**: `[0-9]+` optionally `r` + radix digits: radix 2–36,
  radix digits are `0-9A-Z` (uppercase only), each `< radix` (else lex
  error). Emit `IntLit { radix, digits }`. No sign after `r` (`16r-FF` is
  `16rFF`?? no — it is `16r` with no digits: lex error).
- **L4 Floats**: `digits . digits` (both sides mandatory: `.5` and `5.` are
  not floats — `5.` lexes as IntLit then Period). Optional exponent
  `e[-]?digits` on either an integer or a float mantissa (`1e10`, `3.14e-2`);
  `d` is accepted as an alias for `e`. Any exponent or decimal point makes it
  `FloatLit` (→ boxed Double, SPEC §1.1). Parse the text with Rust
  `str::parse::<f64>` after normalizing `d`→`e`.
- **L5 Negative literals** (the *only* place `-` is not a selector):
  a `-` immediately followed by a digit (no whitespace) begins a negative
  number **iff the previous token cannot end an expression**. "Can end an
  expression" = `Ident`, any literal token, `RParen`, `RBracket`. So:
  `-3` at start → IntLit(neg). `x - 3` → BinarySel("-") because `x` ends an
  expression (note the space before `3` is irrelevant: the deciding factor is
  the *previous* token). `3 - -2` → IntLit(3), BinarySel("-") (prev=IntLit),
  then `-2`: prev is now BinarySel → IntLit(neg 2). `x-3` → same as `x - 3`.
  `#(-1 -2)` → both negative (prev = LitArrayOpen / IntLit… careful: after
  `-1`, prev=IntLit which "ends an expression" — **exception L5a**: inside
  literal-array/byte-array lexing the parser sets a lexer flag
  `in_literal_seq`; while set, `-`+digit is always a negative literal).
- **L6 Characters**: `$` followed by exactly one Unicode scalar (any scalar,
  including space, `'`, `"`, `$`). `$` at EOF = lex error.
- **L7 `|`**: emitted as `VBar` token always; the *parser* treats `VBar` as
  the binary selector `|` when it occurs in binary-operator position, and as
  a declaration/param delimiter at declaration positions. (`||` lexes as one
  `BinarySel("||")`? No — pinned: `|` never merges; two adjacent bars are two
  `VBar` tokens. The only 2-char selectors containing `|` are none.)
- **L8 Strings**: `'` … `'`, embedded quote written `''`. Unterminated = lex
  error. Contents are raw bytes (UTF-8 passthrough), no escapes.
- **L9 `#`**: dispatch on the next char: `(` → `LitArrayOpen`; `[` →
  `ByteArrayOpen`; letter/underscore → symbol: greedy run of
  identifier-or-keyword parts (`#at:put:` = ident+colon repeated; `#foo`);
  binary char(s) → 1–2 char binary symbol (`#+`, `#>>`); `'` → quoted symbol
  (string rules, interned as-is). Anything else = lex error.
- **L10 Punctuation**: `:=`→`Assign`, `^`→`Caret`, `.`→`Period`, `;`→`Semi`,
  `(`/`)`/`[`/`]`, bare `:`→`Colon`. `<` `>` `>=` `<=` `>>` `==` `~=` `~~`
  etc. are ordinary `BinarySel`s — pragma delimiters are recognized by the
  *parser* in pragma position, with a lexer mode flag `pragma_mode` that
  suppresses 2-char merging of selectors *beginning with* `>` (so `>` always
  closes; `>>` inside a pragma is two closes — irrelevant in practice since
  we only nest on explicit `<`).

#### Grammar (EBNF — normative)

Terminals are token names; `ident`, `keyword`, `binsel` abbreviate them.

```ebnf
file            = { top_item } Eof ;
top_item        = class_def | do_it ;
do_it           = statement "." ;                (* executed immediately *)
class_def       = ident keyword("subclass:") ident "[" { class_item } "]" ;
class_item      = class_pragma | instvar_decl | class_method | method ;
class_pragma    = "<" keyword("indexable:") ident ">"          (* oops|bytes *)
                | "<" keyword("classVars:") { ident } ">" ;
instvar_decl    = VBar { ident } VBar ;
class_method    = ident ident("class") binsel(">>") pattern "[" method_body "]" ;
method          = pattern "[" method_body "]" ;
pattern         = ident                                (* unary *)
                | binsel ident | VBar ident            (* binary; VBar = `|` *)
                | { keyword ident }+ ;                 (* keyword *)
method_body     = { pragma } [ temporaries ] statements ;
pragma          = "<" pragma_tokens ">" ;              (* see Pragmas below *)
temporaries     = VBar { ident } VBar ;
statements      = [ statement { "." statement } [ "." ] ] ;
statement       = return | expression ;
return          = "^" expression ;
expression      = ident ":=" expression                (* right-assoc chain *)
                | cascade ;
cascade         = keyword_expr { ";" message_segment } ;
message_segment = { ident }-unary { binsel_arg } [ keyword_msg ]   (* ≥1 msg *)
keyword_expr    = binary_expr [ keyword_msg ] ;
keyword_msg     = { keyword binary_expr }+ ;
binary_expr     = unary_expr { (binsel | VBar) unary_expr } ;
unary_expr      = primary { ident } ;                  (* unary sends *)
primary         = ident | "self" | "super" | "nil" | "true" | "false"
                | literal | block | "(" expression ")" ;
block           = "[" { ":" ident } [ VBar-if-params ] [ temporaries ]
                  statements "]" ;
literal         = IntLit | FloatLit | CharLit | StrLit | SymLit
                | lit_array | byte_array ;
lit_array       = "#(" { array_elem } ")" ;
array_elem      = IntLit | FloatLit | CharLit | StrLit | SymLit
                | ident            (* → Symbol, except nil/true/false *)
                | keyword_run      (* `foo:bar:` bare → Symbol *)
                | binsel | VBar    (* → Symbol *)
                | "(" { array_elem } ")"  | "#(" { array_elem } ")"
                | "#[" { byte_elem } "]" ;
byte_array      = "#[" { byte_elem } "]" ;
byte_elem       = IntLit ;         (* value must be 0..255 after radix *)
```

Notes:
- Block header: `[:x :y | body]`. The `|` after the last param is mandatory
  when params exist. Then optional block temps `| a b |` — yes, two bar
  groups may be adjacent: `[:x | | t | ...]`. Zero params → no leading bar:
  `[ | t | ... ]` is a paramless block with temps (disambiguation: after `[`,
  a `Colon` starts params; a `VBar` starts temps; anything else starts
  statements).
- `self`/`super`/`nil`/`true`/`false` are ordinary `Ident` tokens recognized
  by the parser (reserved: not assignable, not declarable as temps/params/
  instvars — parse error).
- `super` is legal **only** as the receiver of a send (unary/binary/keyword
  or cascade head). Bare `^super` or `x := super` = parse error.
- `^` is legal only as the first token of a statement (method or block).
  A `^` anywhere inside an expression (including inside a cascade segment or
  argument) = parse error. `^ x foo; bar` is fine: the return's expression is
  the whole cascade; the returned value is the **last** segment's result.
- `message_segment`: each `;` segment is `unary* binary* keyword?` re-rooted
  at the cascade receiver, and must contain at least one message.

**Cascade receiver rule (pinned, Blue-Book semantics):** parse `keyword_expr`;
on seeing `;` the parsed expression must be a `Send` (else error
"cascade requires a message send"). The cascade receiver is the receiver of
the **outermost** message of that first expression. Consequences:
- `coll add: 1; add: 2` — receiver `coll` (outermost msg is `add: 1`).
- `x foo: y bar; baz` — receiver `x` (arg `y bar` is inside).
- `(d at: 1) foo; bar` — outermost msg is unary `foo`, so the cascade
  receiver is the **result of the parenthesized keyword send** — this is the
  "cascade to the result of a keyword send" case and must have a golden test.

#### AST (`ast.rs` — verbatim)

```rust
pub enum Literal {
    Int(i64),                                  // fits 62-bit smi
    BigInt { negative: bool, digits: Vec<u8> },// base-256, little-endian, no leading 0
    Float(f64), Char(char), Str(String), Symbol(String),
    Array(Vec<Literal>), ByteArray(Vec<u8>),
    Nil, True, False,
}

pub enum Expr {
    SelfRef(Span),
    Var      { name: String, span: Span },
    Lit      { value: Literal, span: Span },
    Assign   { name: String, value: Box<Expr>, span: Span },
    Send     { receiver: Box<Expr>, selector: String,
               args: Vec<Expr>, is_super: bool, span: Span },
    Cascade  { receiver: Box<Expr>, segments: Vec<Expr>, span: Span },
    CascadeRcvr(Span),   // marker: innermost receiver inside each segment
    Block    (Box<BlockNode>),
    Return   { value: Box<Expr>, span: Span },  // statement position only
}

pub struct BlockNode {
    pub params: Vec<String>, pub temps: Vec<String>,
    pub body: Vec<Expr>, pub span: Span,
    pub scope_id: u32,           // filled by capture analysis
}
pub struct MethodNode {
    pub pattern_selector: String, pub params: Vec<String>,
    pub primitive: Option<u16>,  // from <primitive: N>
    pub temps: Vec<String>, pub body: Vec<Expr>,
    pub class_side: bool, pub span: Span,
}
pub enum Indexable { Oops, Bytes }
pub struct ClassDefNode {
    pub superclass: String, pub name: String,
    pub indexable: Option<Indexable>,
    pub inst_vars: Vec<String>, pub class_vars: Vec<String>,
    pub methods: Vec<MethodNode>, pub span: Span,
}
```

Cascade representation: the parser re-roots the first expression — its
innermost receiver `r` becomes `Cascade.receiver`, and the first segment is
the original `Send` tree with `r` replaced by `CascadeRcvr`. Every subsequent
segment is likewise a `Send` tree bottoming out at `CascadeRcvr`. If the
receiver is `super`, every segment send has `is_super = true`.

`Return` may appear only as a statement in `MethodNode.body`/`BlockNode.body`
(the parser guarantees this; codegen asserts it).

### Algorithms

#### Pragmas — exact skip rules

Pragmas appear only at the very start of a method body, before temporaries
(multiple allowed). On `<` in that position:
1. If the next tokens are exactly `Keyword("primitive:") IntLit BinarySel(">")`
   → recognized: set `MethodNode.primitive = N` (decimal or radix value;
   0 < N ≤ u16::MAX else compile error). At most one `<primitive:>` per
   method (second = error).
2. Otherwise **skip**: consume tokens, maintaining a nesting counter that
   `+1`s on `BinarySel("<")` and `−1`s on `BinarySel(">")` (initial depth 1),
   until depth 0. All token kinds are allowed inside (identifiers, keywords,
   literals, `[`/`]`, commas…); EOF before close = parse error. The lexer's
   `pragma_mode` flag (rule L10) is set for the duration so `>` never merges
   into `>=`/`>>`. This swallows every Strongtalk type annotation
   (`<:T>`, `<Array[Int]>`, `<pre: …>`…) without understanding them
   (SPEC §1, §1.1: parsed and ignored).

Class-level pragmas (`class_item` position): only the two forms in the
grammar are recognized; any other `<`…`>` at class-item position is skipped
by the same depth-counting rule. `<indexable:>` argument must be the ident
`oops` or `bytes` (else error).

#### Name resolution (codegen-time, per method)

Order (pinned; first hit wins; miss at the end = compile error
"undeclared variable 'x'" with span):
1. Block params/temps of the innermost enclosing scope, then outward through
   enclosing block scopes, then method params/temps (a unified slot
   namespace per scope; duplicate name within one scope = error; shadowing
   an outer scope is **allowed**).
2. Instance variables: the compiler flattens `inst_var_names` by walking the
   klass's superclass chain root-first (Object's vars first); the index into
   that flattened list is the `push_instvar` operand. Class-side methods see
   the *metaclass's* chain (empty in v1 — no class-instance vars).
3. Class variables: walk `class_vars` arrays up the superclass chain; found →
   the Association becomes a literal; compile as `push_global`/
   `store_global_pop` (same mechanism as globals, SPEC §4.1).
4. Globals: probe the `smalltalk` namespace; found → its Association as a
   literal, `push_global`. **Assignment to a global inside a method is
   allowed** (`store_global_pop`) but the Association must already exist.
5. Unresolvable → compile error, **except** in a *top-level doIt*: an
   `Assign` to an unresolvable name that begins with an uppercase letter
   creates a new global Association (value nil) in `smalltalk` first
   (declare-on-assign; this is how `Transcript := …` bootstraps globals).
   Reads of unresolvable names are errors even at top level.

`self` compiles to `push_self`; in a block, `push_self` reads the frame
receiver, which S4's block activation set from the closure's home (no special
handling in codegen). Writes to instvars use `store_instvar_pop` (dup first
if the assignment's value is needed, see codegen patterns).

#### Capture analysis (`capture.rs`)

Pass over one `MethodNode` before codegen:
1. **Build the scope tree** by walking the body depth-first in source order;
   scope 0 = the method; each `BlockNode` gets the next `scope_id` (pre-order
   numbering). Each scope records its declared names (params then temps, in
   declaration order) and its parent.
2. **Mark captures**: at every `Var`/`Assign` resolution to a slot variable,
   if the referencing scope ≠ the defining scope (i.e., the reference crosses
   ≥1 block boundary), mark the variable *captured*. Per SPEC §4.5 the rule
   is read-**or**-write ⇒ captured (v1 has no read-only-copy optimization;
   the closure `copied[]` array is always empty, `ncopied = 0`).
3. **Slot assignment** per scope: captured variables get **Context slot**
   indices 0..k−1 in declaration order (params before temps); non-captured
   variables get frame temp slots 0..m−1 in declaration order. A scope with
   k > 0 is *context-owning* (`has_ctx`); k and m are recorded per scope.
4. **Depth numbering**: at a reference site in scope S to a captured var of
   scope D, `depth` = the number of context-owning scopes on the parent
   chain from S (inclusive) up to but excluding D, counting only
   context-owning ones… concretely: walk from S toward the root; count how
   many context-owning scopes you pass through **before** reaching D
   (counting S itself if it owns a context); that count is the operand.
   Depth 0 = "my own frame's context"; each `home_hint` hop adds 1
   (SPEC §5.4). A non-context-owning block's frame `context` slot holds the
   nearest enclosing Context (installed by S4's closure activation), so
   depth counting is over context-owning scopes only.
5. **Prologue moves**: for each captured *parameter*, codegen emits at scope
   entry `push_temp <argslot>` + `store_ctx_temp_pop 0 <ctxslot>` right
   after the (interpreter-performed) Context allocation. Captured temps need
   no move (they start nil, as Context slots do).

The closure captures its enclosing Context implicitly: `push_closure` copies
the creating frame's `context` slot into the new closure (S4 behavior;
no bytecode operand involved). Codegen therefore never pushes a Context.

> **SPEC-QUESTION:** SPEC §2.3/§5.4 describe a `copied[]` read-only-capture
> array on BlockClosure, but §4.5 pins v1 capture = "any temp referenced by
> a nested block is context-allocated". This sprint compiles with
> `ncopied = 0` always; `copied[]` stays dormant until a later optimization
> sprint. Confirm intended.

#### Codegen patterns (per construct)

General: single pass over statements; expression codegen leaves exactly one
value on the stack; statement position emits `pop` afterwards (except the
last statement of a block, whose value is the block value). Two-pass jump
fixup: forward jumps emit a u16 placeholder, patched when the label binds.
Jump encoding (pinned, must agree with S2's interpreter):
`jump_fwd/br_*_fwd <u16 d>`: target = bci_after_operands + d.
`jump_back <u16 d>`: target = bci_after_operands − d.

- **Variables**: slot var → `push_temp s` / captured → `push_ctx_temp depth s`
  / instvar → `push_instvar i` / global+classvar → `push_global lit`.
- **Assignment `x := e`** in value position: `<e>` `dup` `store_*_pop`; in
  statement position: `<e>` `store_*_pop` (no dup+pop pair — peephole done
  structurally, not by a peephole pass).
- **Literals**: smi in −128..=127 → `push_smi_i8`; nil/true/false →
  dedicated opcodes; everything else → literal frame + `push_literal`
  (`push_literal_w` if index > 255).
- **Send**: push receiver, push args left-to-right, `send <ic>` /
  `send_super <ic>` (`send_w` when ic index > 255).
- **Cascade**: `<receiver>`; for each segment except the last: `dup`,
  `<segment>` (the `CascadeRcvr` marker compiles to *nothing* — the dup'd
  value is already in receiver position), `pop`; last segment: `<segment>`
  without dup/pop — its result is the cascade's value.
- **Return `^e`**: `<e>` `return_tos` in a method; `<e>` `nlr_tos` in a
  block (any depth). Method falls off the end → `return_self`. Block falls
  off the end → `block_return_tos` (empty block body: `push_nil` first;
  a body whose last item is a statement uses that statement's value).
- **Blocks**: compile the `BlockNode` to its own `CompiledBlock` (own
  bytecode, own literal frame, own IC table; `is_block=1`,
  `holder` = the home *method* oop, argc = params, ntemps = its frame-slot
  count, `has_ctx` from capture analysis). Add it to the home literal frame;
  emit `push_closure <lit> 0`.

#### Inlined control-flow selectors

Inlining preconditions (else compile a **real send** — the library provides
the genuine methods in S6 so semantics match): the controlling argument(s)
must be *literal block expressions written directly at the call site*, with
the exact arity listed. Receiver blocks for `whileTrue:`/`whileFalse:` must
also be literal. Additional bail-out: for `to:do:`, if the loop-variable
param is captured by a block nested inside the body, emit a real send
(iteration-variable freshness — see Pitfalls P6). Blocks chosen for inlining
are *dissolved*: their temps are hoisted into the enclosing scope as fresh
slots (renamed internally; capture analysis runs on the *post-inlining
decision* shape, i.e. decide inlining first on the raw AST, then hoist, then
capture-analyze).

Patterns (labels are illustrative bci names):

```
e ifTrue: [b]                    e ifTrue: [b] ifFalse: [c]
    <e>                              <e>
    br_false_fwd L1                  br_false_fwd L1
    <b>                              <b>
    jump_fwd L2                      jump_fwd L2
L1: push_nil                     L1: <c>
L2: …                            L2: …
```
`ifFalse:` = same with `br_true_fwd`. `ifFalse:ifTrue:` swaps arms.
Empty inlined block ⇒ its `<b>` is `push_nil`.

```
a and: [b]                       a or: [b]
    <a>                              <a>
    br_false_fwd L1                  br_true_fwd L1
    <b>                              <b>
    jump_fwd L2                      jump_fwd L2
L1: push_false                   L1: push_true
L2: …                            L2: …
```

```
[c] whileTrue: [b]           ; value of the whole expression is nil
L0: <c inlined>              ; receiver block dissolved inline
    br_false_fwd L2
    <b inlined>
    pop                      ; discard body value
    jump_back L0             ; ← the GC/interrupt poll + loop counter (§5.5)
L2: push_nil
```
`whileFalse:` = `br_true_fwd`. Argless `[c] whileTrue` is **not** inlined
(real send; S6 defines it). The backward jump is mandatory even for
constant-false conditions — never "optimize" it away; it is the poll point.

```
a to: b do: [:i | body]      ; result of the expression is a (the receiver)
    <a>
    dup
    store_temp_pop t_i       ; t_i = hoisted loop var (or store_ctx_temp_pop)
    <b>
    store_temp_pop t_lim     ; t_lim = fresh hidden temp, never captured
L0: push_temp t_i
    push_temp t_lim
    send #<=                 ; REAL send — works for smi/Double/Large
    br_false_fwd L2
    <body>                   ; i resolves to t_i
    pop
    push_temp t_i
    push_smi_i8 1
    send #+                  ; real send (overflow → Large fallback works)
    store_temp_pop t_i
    jump_back L0
L2: …                        ; the dup'd receiver `a` is now TOS = result
```
Note the receiver copy rides the operand stack beneath the loop (legal:
jumps do not require an empty stack). `to:by:do:` is **not** inlined in v1.
`#<=` and `#+` here are ordinary IC-table send sites (they gather feedback
for tier 1 like any send).

Non-boolean conditions are handled by the interpreter's `br_*` opcodes
(`#mustBeBoolean` send, SPEC §4.2) — codegen does nothing special.

#### Literal frame & IC table construction

Per compiled unit (method or block):
- Literal frame: first-use order. Dedupe within the unit by value for:
  out-of-i8-range smis, Doubles (bit-exact), Symbols, Strings (byte-equal),
  Characters, BigInts. Literal Arrays/ByteArrays are not deduped. Globals/
  classvars contribute their **Association oop** (identity-deduped).
  `CompiledBlock` literals also go in the *home's* frame (the
  `push_closure` operand indexes the home frame).
- Literal `Array`s are built **recursively at compile time** as ordinary
  heap Arrays (elements: smis, Doubles, Characters, Strings, interned
  Symbols, nested Arrays/ByteArrays, nil/true/false). `BigInt` literals
  build `LargePositiveInteger`/`LargeNegativeInteger` byte objects
  (base-256 little-endian digits, SPEC §2.3 row IndexableBytes) — these two
  klasses must exist in genesis (see SPEC-QUESTION below).
- IC table: one 4-word group per send site, in bytecode order:
  `[selector, argc, nil, nil]` (empty state, SPEC §4.3). The `send` operand
  is the **site index** k (words 4k..4k+3). Inlined control flow consumes no
  IC entries except the real sends it emits (`#<=`, `#+` in `to:do:`).
- Flags word: `argc:4 | ntemps:8 | has_ctx:1 | is_block:1 | prim_fails:1`
  (SPEC §4.4). `ntemps` counts frame slots only (not Context slots).
  `prim_fails` = 1 iff the method has both a primitive and a non-empty body.
  Overflow of any field (argc > 15, ntemps > 255) = compile error.

> **SPEC-QUESTION:** SPEC §3.1's well-known-klass list shows `smi_klass
> double_klass string_klass …` with an ellipsis. S5 requires
> `large_pos_int_klass` / `large_neg_int_klass` (BigInt literals) and S6
> requires a genesis *skeleton* for the whole numeric/collection hierarchy
> (see sprint_s06_detail.md §"Genesis skeleton"). Propose amending §3.1 to
> enumerate the full list.

#### Class-definition execution (`classdef.rs`)

For `Super subclass: Name [ … ]`:
1. Resolve `Super` in `smalltalk` (must be a klass; error otherwise).
2. If `Name` is unbound → **create**: allocate metaclass (klass=`Metaclass`,
   superclass = Super's metaclass) then the class (klass = new metaclass,
   superclass = Super). `format`: from `<indexable:>` pragma if present,
   else inherit Super's format (Slots stays Slots). `non_indexable_size` =
   Super's + own instvar count (words). Set `name` (interned Symbol),
   `inst_var_names` (own names only, as Symbols), `class_vars` (Array of
   fresh Associations, value nil), `mixin_reserved` = nil, empty
   MethodDictionaries both sides. Bind `Name` in `smalltalk`.
3. If `Name` is bound to a klass → **reopen**: the declared superclass must
   equal the current one, and the definition must declare **no** instvars,
   no `<indexable:>` (error "cannot change shape of existing class Name" —
   schema change is out of scope v1). New `classVars:` entries are appended;
   re-declaring an existing class var is a no-op.
4. Compile and install each method into the (meta)class's MethodDictionary
   (replacing same-selector entries), flushing the lookup cache per
   SPEC §6.2. A `Name class >> pat [...]` item requires `Name` to equal the
   class being defined (error otherwise); its selector comes from `pat` and
   it installs in the metaclass.
5. Pattern selector assembly: unary = the ident; binary = the selector text
   (incl. `|`); keyword = concatenation of the keyword tokens in order
   (`at:put:`). Pattern params are the method's leading slot variables.

Top-level `do_it`: wrap the single statement as the body of an anonymous
method (selector `#doIt`, holder = `UndefinedObject`'s klass, receiver
`nil`), compile with the same pipeline (blocks, temps not allowed at top
level — a doIt has no `| |` section; use blocks for locals), execute via the
interpreter, discard the result (the REPL prints it instead).

#### world.list loader (`world.rs`)

`world/world.list`: UTF-8 text; one path per line, relative to the file's
own directory; blank lines and lines starting `#` ignored; duplicate entries
are an error. Load = for each path: read, parse whole file, execute items in
order. First error aborts the whole load with `path:line:col: message`.
`Universe` records `world_loaded = true` afterwards; then the VM sends
`startUp` to the `Smalltalk` global (SPEC §3.2 step 3) — if the selector is
not understood (early sprints), the DNU is suppressed for exactly this one
send (a bootstrap allowance, removed once S6 lands).

#### CLI (`main.rs`)

- `macvm run <file.mst> [--world <dir>]` — load world (default `./world`
  next to the CWD; `--world` overrides; missing world.list = warning, run
  file anyway), then load-and-execute `<file.mst>` the same way (its doIts
  are the program). Exit 0 unless a compile error / uncaught VM error.
- `macvm repl [--world <dir>]` — load world, then loop: prompt `mst> `, read
  lines, accumulate until the input parses (an "unexpected EOF" parse error
  ⇒ keep reading; any other error ⇒ report, reset buffer). Execute each
  complete statement as a doIt; print the result by sending `printString`
  if lookup finds it, else the Rust `print_oop` fallback (pre-S6 worlds).
- Env flags per CONVENTIONS §3 parsed into `VmOptions` before anything runs.

### Layer boundaries

- `frontend/` may use: `oops` wrappers, `memory` allocation + interning,
  `bytecode::BytecodeBuilder`, and read klass metadata. It may **not** call
  into `interpreter/` — execution of doIts goes through a single
  `runtime::execute_doit(&mut VmState, MethodOop) -> Result<Oop, VmError>`
  entry point.
- `classdef.rs` is the only module that creates/reopens klasses from source;
  it uses `memory`'s klass-allocation API (extend it if S1 only exposed
  genesis-internal creation).
- Nothing in `frontend/` holds a raw `Oop` across an allocation without a
  `Handle` — the loader allocates constantly (S7 retrofits stress, but write
  it clean now).

## Implementation order

1. `lexer.rs` + unit tests (every rule L1–L10; the L5 negative-literal
   table verbatim as cases).
2. `ast.rs`; `parser.rs` expressions only (no class defs) + unit tests
   incl. cascades, blocks, literal arrays, pragma skipping.
3. Parser: class definitions, patterns, file items.
4. `capture.rs` + unit tests on synthetic ASTs (scope/slot/depth tables).
5. `codegen.rs` core (no inlining): variables, sends, cascades, returns,
   blocks, literal frames, IC tables — golden `.bc.expected` corpus starts.
6. Inlined control selectors + their goldens.
7. `classdef.rs` + `world.rs` + `runtime::execute_doit`.
8. `main.rs` CLI; convert all S2–S4 golden programs to `.mst`.

Each step compiles and tests independently; steps 1–6 need no live heap
changes beyond what S1–S4 provide.

## Pitfalls

- **P1 Negative literals** (rule L5): the prev-token rule, not whitespace,
  decides. `3 - -2` and `3--2` both mean `3 - (-2)`; `a -2` is a **send** of
  `-` with arg 2 (prev token Ident ends an expression) — surprising but
  correct Smalltalk; add a golden.
- **P2 Literal-array symbols need no `#`**: `#(foo bar: baz:qux: + nil)` is
  `{#foo. #bar:. #baz:qux:. #+. nil}`. Bare keyword *runs* must be glued:
  the lexer emits `Keyword("bar:")` then `Ident("baz")`… — the array parser
  must concatenate **adjacent** keyword tokens (no intervening whitespace?
  pinned: whitespace between keyword parts of one symbol is *not* allowed;
  consecutive Keyword tokens merge only if textually adjacent, which the
  lexer knows from spans).
- **P3 Binary selectors are 1–2 chars max**: `a --> b` is `-- >`? No: lex
  error is wrong too — it lexes as `BinarySel("--")` then `BinarySel(">")`,
  which then fails to parse (two operators in a row). Fail at parse, with a
  clear message.
- **P4 `|` triple duty**: binary selector vs temp-decl delimiter vs block
  param terminator. Handled by position (L7 + grammar); the nasty case is a
  binary *method named* `|` inside a class body: `| arg [ ^… ]` vs instvar
  decl `| a b |`. Disambiguate with lookahead: `VBar Ident VBar…` → decl;
  `VBar Ident LBracket` → binary method `|`.
- **P5 `whileTrue:` receiver-block inlining**: BOTH blocks dissolve inline
  and the loop MUST close with `jump_back` (SPEC §5.5 poll + counter).
  Compiling the condition once and jumping into the middle is wrong; the
  condition re-evaluates every iteration (pattern above).
- **P6 `to:do:` variable freshness**: with a real send, each iteration binds
  a fresh block param; inlined, `t_i` is one shared slot. If `body` creates
  closures that capture `i`, they would all see the final value. Hence the
  bail-out rule (real send when the loop var is captured by a nested block).
  Test both shapes.
- **P7 Cascade + keyword send**: `x at: 1 put: 2; at: 3 put: 4` — receiver
  `x`, NOT the result of the first `at:put:`. But add parens and it flips
  (see Cascade receiver rule). Both directions in goldens.
- **P8 Statement-value discipline**: every statement except a block's last
  pops; forgetting the `pop` after an inlined `whileTrue:` body corrupts the
  stack invisibly until a golden's "final stack shape" assert catches it —
  keep S2's frame-teardown asserts on in debug.
- **P9 Empty things**: empty method body → `return_self` only. Empty block
  `[]` → `push_nil; block_return_tos`. Block with only temps `[ | a | ]` →
  same (temps don't produce values). Empty file / empty class body are
  valid. `#()` and `#[]` are valid literals.
- **P10 Error positions**: every Tok carries the Span of its FIRST byte;
  errors print `file:line:col: error: …` and abort (fail-fast, no resync —
  one error per run is fine in v1).
- **P11 Interning while parsing**: symbols/strings in literal frames are
  heap allocations — building a literal Array of 1000 symbols must not hold
  raw child oops across sibling allocations (Handles or build via a rooted
  Array, filling slots as you go).
- **P12 IC operand vs word index**: `send <k>` where k is the *site* index;
  the interpreter computes `4k`. Off-by-4 bugs read the selector of the
  wrong site and "work" alarmingly often — the disassembler must print both
  k and the selector so goldens pin it.

## Interfaces for later sprints

```rust
// frontend/mod.rs
pub fn compile_method(vm: &mut VmState, src: &str, holder: KlassOop,
                      class_side: bool) -> Result<MethodOop, CompileError>;
pub fn load_file(vm: &mut VmState, path: &Path) -> Result<(), CompileError>;
pub fn load_world(vm: &mut VmState, dir: &Path) -> Result<(), CompileError>;
pub struct CompileError { pub path: Option<PathBuf>, pub span: Span,
                          pub msg: String }
// runtime/
pub fn execute_doit(vm: &mut VmState, m: MethodOop) -> Result<Oop, VmError>;
```
- S6 consumes `load_world` and writes `.mst` against this exact grammar.
- S10+ (tier 1) reads the IC tables this sprint lays out; the site-index
  convention (P12) is load-bearing there.
- `sourceCompile:` primitive (SPEC §10) wraps `compile_method` — implement
  the primitive now (system group) since the plumbing is one call.

## Out of scope

- Class-shape changes on reopen (instvar add/remove) — no target sprint;
  revisit with image support (S16).
- `to:by:do:`, `timesRepeat:`, `ifNil:` inlining — real sends in v1
  (tier-1 inlining recovers the performance, S14).
- Read-only capture `copied[]` optimization and clean blocks (SPEC §4.5 Δ).
- Parser error recovery / multiple diagnostics per run.
- Mixins, namespaces, `.dlt` chunk format (SPEC §1.2 Δ).
- Unicode-aware `String at:` (byte-indexed per SPEC §1.3).

## Addendum — GUI-track obligation (SPEC §16.4, amendment A16)

The frontend must **record method source** in the world-side registry
`Smalltalk methodSources` (IdentityDictionary: CompiledMethod → String),
gated by `MACVM_KEEP_SOURCE` (default on). Record at install time in the
class-definition executor (and the REPL's doit path may skip it). No
CompiledMethod layout change — the registry is an ordinary heap object. The
GUI's G3/G4 browsers and the `VmHandle::mirrors` accept path (SPEC §16.3)
consume this; `eval` reuses this sprint's doit machinery.
