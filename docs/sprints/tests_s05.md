# Sprint S05 — Test Plan

## Acceptance gate

Restated from SPRINTS.md S5, made checkable:
1. Golden AST→bytecode listings exist for a corpus in `tests/golden/`
   (`*.mst` + `*.bc.expected`) in which **every opcode 00–12, 20–23, 30–33,
   40–43 appears at least once** (checked by a meta-test that greps the
   expected files for each mnemonic).
2. Parse/compile errors carry `file:line:col` (asserted on ≥ 10 deliberate
   error inputs).
3. End-to-end: `macvm run tests/golden/point_demo.mst` prints the expected
   transcript (Point creation, `+`, `printOn:` via `printOnStdout:`).
4. Every S2–S4 golden program re-expressed as `.mst` source produces the
   same transcript as its hand-assembled original (originals stay in-tree).
5. All prior sprints' tests still green. `just gate-s05` runs all of the
   above.

## Unit tests

Format: test name | module | assertion | rationale.

| test | module | assertion | rationale |
|---|---|---|---|
| lex_idents_keywords | lexer | `foo at: x:=` → Ident, Keyword("at:"), Ident, Assign | L2/L10 split of `:` vs `:=` |
| lex_radix | lexer | `16rFF 2r1010 36rZZ` values 255, 10, 1295 | L3 radix range |
| lex_radix_bad | lexer | `16r` and `8r9` are lex errors with col | L3 digit<radix |
| lex_float_forms | lexer | `3.14 1e10 2.5e-3 1d2` all FloatLit; `5.` = Int,Period | L4 boundaries |
| lex_neg_table | lexer | the L5 table verbatim: `-3`, `x - 3`, `x-3`, `3 - -2`, `3--2`, `a -2`, `(-3)`, `[-3]` | the negative-literal rule |
| lex_neg_in_litarray | lexer/parser | `#(-1 -2)` → both negative | L5a in_literal_seq |
| lex_char | lexer | `$a $  $' $" $$` each one CharLit | L6 any scalar |
| lex_string_quotes | lexer | `'it''s'` → `it's`; unterminated errors | L8 embedded quote |
| lex_comment_quotes | lexer | `"a ""b"" c"` skipped entirely | L1 embedded dquote |
| lex_symbols | lexer | `#foo #at:put: #+ #>> #'hi there'` | L9 all four forms |
| lex_binary_two_char | lexer | `a >= b` one selector; `a --> b` → `--`,`>` | 2-char max |
| lex_vbar_never_merges | lexer | `||` → VBar,VBar | L7 |
| parse_precedence | parser | `1 + 2 * 3 max: 4 foo` AST shape: keyword(binary(binary(1+2)*3), max:, unary(4 foo)) | unary>binary>keyword |
| parse_assign_chain | parser | `a := b := 3` right-assoc nesting | expression rule |
| parse_cascade_rcvr | parser | `x add: 1; add: 2` receiver = x | cascade rule |
| parse_cascade_paren | parser | `(d at: 1) foo; bar` receiver = the paren send result | edge case (P7) |
| parse_cascade_segment_chain | parser | `x foo; bar baz: 1` segment2 = keyword send on unary on CascadeRcvr | segment grammar |
| parse_cascade_nonsend | parser | `3; foo` is a parse error | cascade requires Send |
| parse_caret_positions | parser | `^ x foo; bar` OK; `x foo: ^y` error; `[:a | ^a]` OK | ^ statement-only |
| parse_super | parser | `super foo` OK, is_super on send; `^super` error; `super foo; bar` all segments super | super rules |
| parse_block_headers | parser | `[]`, `[ | t | ]`, `[:x | x]`, `[:x :y | | t | t]` | header disambiguation |
| parse_litarray_nested | parser | `#(1 (2 3) #(4) #[5] foo bar: nil true)` → Literal tree with symbols, nested arrays, byte array, nil/true as values | P2 + nesting |
| parse_keyword_symbol_glue | parser | `#(bar:baz:)` one symbol; `#(bar: baz:)` two | P2 adjacency |
| parse_bytearray_range | parser | `#[0 255 16rFF]` OK; `#[256]`, `#[-1]`, `#[foo]` errors | byte_elem rule |
| parse_pragma_prim | parser | `<primitive: 7>` sets primitive=7; second one errors | pragma rule 1 |
| parse_pragma_skip | parser | `<:T>`, `<pre: x > 0 :: bla>`, `<Array[Int]>` all skipped; method compiles | pragma rule 2 depth counting |
| parse_classdef_items | parser | Point example from SPEC §1.2 verbatim → ClassDefNode with 5 methods, 1 class-side | brace grammar |
| parse_vbar_method | parser | class body `\| arg [ ^arg ]` = binary method `\|`; `\| a b \|` = instvars | P4 lookahead |
| parse_reserved | parser | `self := 3`, temps named `nil` → errors | reserved words |
| capture_basic | capture | method temp read in block → ctx slot 0, ref depth 1 from block | rule 2/4 |
| capture_none | capture | temp used only in method body → frame slot, has_ctx=false | negative case |
| capture_param | capture | captured method param → ctx slot + prologue move emitted | rule 5 |
| capture_depth3 | capture | 3-deep nested blocks each with own captured temp: depths 0/1/2 at innermost refs | rule 4 walk |
| capture_skip_level | capture | middle block owns no ctx: inner ref to method temp has depth 1 (not 2) | context-owning count only |
| capture_slot_order | capture | params-then-temps declaration order regardless of reference order | rule 3 determinism |
| resolve_order | codegen | temp shadows instvar shadows classvar shadows global (4 fixtures) | resolution order |
| resolve_unbound | codegen | method using `Zork` → compile error with span | fail-fast |
| resolve_doit_global | codegen | top-level `Foo := 3.` creates Association; `foo := 3.` (lowercase) errors | declare-on-assign |
| litframe_dedupe | codegen | two `#foo` + two `'hi'` + two 1000s → 3 literals | dedupe rules |
| ic_site_indices | codegen | method with 3 sends: ics length 12, operands 0,1,2 | P12 |
| flags_word | codegen | argc/ntemps/has_ctx/is_block/prim_fails packing for 5 fixture methods | SPEC §4.4 |
| bigint_literal | codegen | `16rFFFFFFFFFFFFFFFFFF` → LargePositiveInteger, digits little-endian | BigInt path |
| worldlist_parse | world | comments/blanks skipped; duplicate path errors | loader rules |

## Integration / golden tests

Golden corpus `tests/golden/<name>.mst` + `.bc.expected` (disassembly of
every method compiled from the file, in definition order; disassembler
prints `send k <#selector argc>` per P12). Regenerate via `UPDATE_GOLDEN=1`.

| golden | contents / what it pins |
|---|---|
| g_arith.mst | smi literals incl. i8 boundary (−128, 127, 128), radix, float, char; push_smi_i8 vs push_literal choice |
| g_vars.mst | temps, instvars, class vars, globals; assignment in value vs statement position (dup presence) |
| g_sends.mst | unary/binary/keyword chains, super sends, keyword selector assembly `at:put:` |
| g_cascade.mst | plain cascade, cascade after keyword send, parenthesized-receiver cascade, `^` of a cascade, cascade on super |
| g_blocks.mst | empty block, params, block temps, nested 3 deep, NLR (`nlr_tos`), block falling off end (`block_return_tos`), ncopied=0 |
| g_capture.mst | shared mutable capture (counter), captured param prologue move, depth-skipping middle block |
| g_iftrue.mst | all four if-forms + non-literal-arg fallback to real send |
| g_andor.mst | and:/or: inlined + fallback when arg has a param |
| g_while.mst | whileTrue:/whileFalse:, body-value pop, jump_back present, constant-false condition still loops via jump_back |
| g_todo.mst | to:do: inlined (dup'd receiver as result), captured-loop-var fallback to real send |
| g_litarrays.mst | nested literal arrays, bare symbols, keyword symbols, negatives, byte arrays |
| g_pragmas.mst | `<primitive: N>` with fallback body (prim_fails=1), discarded type annotations |
| g_classdef.mst | SPEC §1.2 Point verbatim; reopen adding a method; `<indexable: bytes>` subclass |
| point_demo.mst | end-to-end transcript golden (`.expected` stdout) |
| s2_*/s3_*/s4_*.mst | every S2–S4 golden program re-expressed in source; `.expected` transcripts identical to the originals' |

Runner: `tests/it_golden.rs` (CONVENTIONS §5). CLI tests in
`tests/it_cli.rs`, driving the built binary with piped stdin/stdout:

| case | input | expected |
|---|---|---|
| run_ok | `macvm run point_demo.mst` | exit 0, transcript matches |
| run_compile_err | file with `a + + b` | exit ≠ 0, `line:col` on stderr |
| run_missing_file | `macvm run nope.mst` | exit ≠ 0, IO error message |
| world_override | `--world tests/fixtures/miniworld` | miniworld classes visible |
| world_missing | no world.list present | warning printed, file still runs |
| repl_arith | `3 + 4.` | prints `7` |
| repl_continuation | `[:x \|` then `x] value: 5.` | buffers, prints `5` |
| repl_error_reset | `a + + b.` then `1 + 1.` | error reported, then `2` |
| repl_global | `Foo := 3.` then `Foo * 2.` | declare-on-assign, prints `6` |
| repl_pre_s6_print | result with no `printString` | `print_oop` fallback used |

## In-language tests

None yet — SUnit-lite arrives in S6. The `point_demo.mst` transcript and the
re-expressed S2–S4 programs stand in as executable language tests.

## Stress / negative tests

Deliberate failures, each asserting the exact `line:col` and a message
substring (`tests/it_frontend_errors.rs`):

| input | expected error |
|---|---|
| unterminated string / comment / pragma | lex/parse error at opening position |
| `16r` , `8r9`, `$` at EOF | lex error |
| `x foo: ^y` | "unexpected ^" |
| `3; foo` | "cascade requires a message send" |
| `a + + b` | parse error at second `+` |
| `[:x | x` (EOF) | "unexpected end of input" (REPL uses this to continue) |
| method with 2 `<primitive:>` | "duplicate primitive pragma" |
| duplicate temp name; temp named `self` | declaration errors |
| unresolvable variable in method | "undeclared variable" |
| reopen with new instvars / changed superclass / `<indexable:>` | "cannot change shape" |
| `Zork subclass: Foo [...]` | "superclass not found" |
| `Foo class >> bar [...]` inside class Baz | class-name mismatch |
| 16 args, 256 temps, 70000-byte jump span | flags/operand overflow errors |
| `#[300]` | "byte array element out of range" |

Plus: compile the entire golden corpus under `debug_assertions` with the S2
frame/stack invariant checks enabled — any statement-value imbalance (P8)
must trip an assert, verified by one intentionally-miscompiled fixture in a
`#[should_panic]` unit test of the checker itself.

## Non-goals

- No GC-stress coverage of the loader (S7 retrofits Handles and runs the
  whole suite under `MACVM_GC_STRESS=1`; write the loader Handle-clean now,
  but the stress gate lands there).
- No in-language SUnit assertions (S6), no library semantics (S6 tests
  `to:do:` real-send equivalence with values, Large fallback, etc.).
- No performance measurements of parse/compile speed (tracked informally;
  PERF.md starts in S6 for the interpreter, S15 for tier 1).
- No REPL line-editing/history testing (plain stdin only in v1).
