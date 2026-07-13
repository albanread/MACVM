# MACVM Instruction Set Reference — bytecodes and primitives

Exhaustive reference tables for every bytecode (`src/bytecode/opcode.rs`) and
every primitive (`src/runtime/primitives.rs`) MACVM actually implements, as
of this writing. `docs/SPEC.md` §4/§10 describe the *design* of each
mechanism (encoding rules, the IC side-table, the `PrimResult` contract);
this document is the flat, complete listing SPEC.md's own prose doesn't
spell out row-by-row. Primitive ids/names are cross-checked directly against
`primitives.rs`'s own `prim_ids_frozen` regression test, which pins this
exact table — if the two ever disagree, the source (and its test) win.

## Bytecodes

31 opcodes, one instruction stream, no self-modifying bytecode (unlike
Strongtalk, which encodes inline-cache state directly in the opcode and
patches it at runtime — MACVM keeps bytecode immutable and routes every
send through a per-method IC side table instead, SPEC §4.3). Encoding is one
opcode byte followed by fixed-width little-endian operands. All jump/branch
offsets are byte distances relative to the program counter immediately
*after* the whole instruction (offset 0 = fall through to the next
instruction).

| Hex | Mnemonic | Operands | Description |
|---|---|---|---|
| `0x00` | `push_self` | — | Push the receiver of the current activation. |
| `0x01` | `push_nil` | — | Push `nil`. |
| `0x02` | `push_true` | — | Push `true`. |
| `0x03` | `push_false` | — | Push `false`. |
| `0x04` | `push_smi_i8` | `i8` | Push a small-integer constant in −128..127, encoded directly in the instruction (no literal-frame lookup). |
| `0x05` | `push_literal` | `u8` | Push literal-frame entry `u8` (a constant from the method's literal array). |
| `0x06` | `push_temp` | `u8` | Push argument/temporary slot `u8` — arguments and temporaries share one unified slot space. |
| `0x07` | `store_temp` | `u8` | Store TOS into temp slot `u8`, leaving the value on the stack. |
| `0x08` | `store_temp_pop` | `u8` | Store TOS into temp slot `u8` and pop it. |
| `0x09` | `push_instvar` | `u8` | Push instance variable `u8` of the receiver. |
| `0x0A` | `store_instvar_pop` | `u8` | Store TOS into instance variable `u8` of the receiver and pop it. |
| `0x0B` | `push_global` | `u8` | Push the value half of literal-frame Association `u8` (a global-variable read). |
| `0x0C` | `store_global_pop` | `u8` | Store TOS into the value half of literal-frame Association `u8` and pop it. |
| `0x0D` | `pop` | — | Discard TOS. |
| `0x0E` | `dup` | — | Duplicate TOS. |
| `0x0F` | `push_ctx_temp` | `u8 depth`, `u8 idx` | Push slot `idx` of the heap `Context` `depth` enclosing scopes up — a variable captured by/shared with a nested block. |
| `0x10` | `store_ctx_temp_pop` | `u8 depth`, `u8 idx` | Store TOS into that heap `Context` slot and pop it. |
| `0x11` | `push_closure` | `u8 lit`, `u8 ncopied` | Build a `BlockClosure` from literal-frame `CompiledBlock` `lit`, popping `ncopied` values off the stack to become its captured copies. |
| `0x12` | `push_literal_w` | `u16` | Wide form of `push_literal`, for methods with more than 256 literals. |
| `0x20` | `send` | `u8 ic` | Ordinary message send through inline-cache slot `ic` of the method's IC table (§4.3). |
| `0x21` | `send_super` | `u8 ic` | Super-send — lookup starts above the sending method's holder class — through IC slot `ic`. |
| `0x22` | `send_w` | `u16 ic` | Wide form of `send`, for methods with more than 256 send sites. |
| `0x23` | `send_super_w` | `u16 ic` | Wide form of `send_super`. |
| `0x30` | `jump_fwd` | `u16` | Unconditional relative jump forward by `u16` bytes. |
| `0x31` | `jump_back` | `u16` | Unconditional relative jump backward by `u16` bytes; also runs the loop-poll check (the safepoint/deopt trigger for hot loops, SPEC §5.5). |
| `0x32` | `br_true_fwd` | `u16` | Pop TOS: jump forward by `u16` if `true`; fall through if `false`; anything else sends `#mustBeBoolean`. |
| `0x33` | `br_false_fwd` | `u16` | Pop TOS: jump forward by `u16` if `false`; fall through if `true`; anything else sends `#mustBeBoolean`. |
| `0x40` | `return_tos` | — | Return TOS from the current *method* activation to its sender (unwinds the frame). |
| `0x41` | `return_self` | — | Return the receiver from the current method activation to its sender. |
| `0x42` | `block_return_tos` | — | Return TOS as the value of a *block* activation (to whoever sent `value`/`value:`/etc.) — not a method return. |
| `0x43` | `nlr_tos` | — | Non-local return: return TOS from the block's *home method* activation, unwinding any frames in between (SPEC §5.4). |

Notably absent (by deliberate design, SPEC §4 "Δ from Strongtalk"):
Strongtalk's dozens of frequency-specialized opcodes (`push_temp_0`,
`push_temp_1`, …) — MACVM leans on the optimizing tier for performance
rather than bytecode-level micro-specialization, keeping the set small
(~32 vs. Strongtalk's 256).

## Primitives

> **Snapshot — not exhaustive.** The id-by-id table below covers the original
> core set (ids ≤ ~110). Later passes added many more groups — unboxed float
> (`float_fastpath_design.md`), SIMD (`SIMD.md`), FFI (`FFI.md`), and the game
> group at ids 200–215 (`gamepane_design.md`) — so the live `PRIMITIVES` table
> is now ~124 entries. **The authoritative current list is the source itself:**
> `src/runtime/primitives.rs` and its `prim_ids_frozen` regression test (and the
> generated GUI reference page `gui/reference/pages/macvm-help/primitives.html`).
> This document remains the design-level reference for how a primitive behaves.

Grouped by receiver type, each bound to Smalltalk source via
`<primitive: N>`. Every primitive validates its own receiver/argument
shapes; a validation failure is always `PrimResult::Fail` (the interpreter
falls through to the method's ordinary bytecode body as a fallback), never
a Rust panic. `Argc` excludes the receiver. `Allocates` marks primitives
that can trigger a GC-safepoint-relevant allocation (handle-safety
relevant to anyone adding new ones); `Can fail` marks whether `Fail` is a
real, reachable outcome for that primitive (a few, like `class` or `==`,
always succeed once dispatched).

| Id | Selector | Group | Argc | Allocates | Can fail | Description |
|---|---|---|---|---|---|---|
| 1 | `+` | smi | 1 | No | Yes | Small-integer addition; fails (falls back to the method body, which promotes to LargeInteger) on overflow or a non-smi argument. |
| 2 | `-` | smi | 1 | No | Yes | Small-integer subtraction; fails on overflow or a non-smi argument. |
| 3 | `*` | smi | 1 | No | Yes | Small-integer multiplication; fails on overflow or a non-smi argument. |
| 4 | `//` | smi | 1 | No | Yes | Small-integer floor division; fails on overflow, a non-smi argument, or division by zero. |
| 5 | `\\` | smi | 1 | No | Yes | Small-integer modulo; fails on overflow, a non-smi argument, or division by zero. |
| 6 | `bitAnd:` | smi | 1 | No | Yes | Bitwise AND of two small integers; fails on a non-smi argument. |
| 7 | `bitOr:` | smi | 1 | No | Yes | Bitwise OR of two small integers; fails on a non-smi argument. |
| 8 | `bitXor:` | smi | 1 | No | Yes | Bitwise XOR of two small integers; fails on a non-smi argument. |
| 9 | `bitShift:` | smi | 1 | No | Yes | Bitwise shift: positive count shifts left (fails on overflow past the smi range), negative shifts right (arithmetic, never overflows); `\|count\| >= 62` always fails regardless of the receiver's value. |
| 10 | `<` | smi | 1 | No | Yes | Small-integer less-than; fails on a non-smi argument. |
| 11 | `<=` | smi | 1 | No | Yes | Small-integer less-or-equal; fails on a non-smi argument. |
| 12 | `>` | smi | 1 | No | Yes | Small-integer greater-than; fails on a non-smi argument. |
| 13 | `>=` | smi | 1 | No | Yes | Small-integer greater-or-equal; fails on a non-smi argument. |
| 14 | `=` | smi | 1 | No | Yes | Small-integer numeric equality; fails on a non-smi argument. |
| 15 | `~=` | smi | 1 | No | Yes | Small-integer numeric inequality; fails on a non-smi argument. |
| 20 | `identityHash` | oops | 0 | No | No | Answers the receiver's identity hash — the smi itself if the receiver already is one, else a hash derived from its heap identity. |
| 21 | `class` | oops | 0 | No | No | Answers the receiver's class. |
| 22 | `==` | oops | 1 | No | No | Identity comparison (same oop bits) — always succeeds, unlike `=`. |
| 23 | `basicNew` | oops | 0 | Yes | Yes | Allocates a new zero-sized instance of the receiver (a class); fails if the class's format requires an explicit size (indexable) or isn't instantiable this way. |
| 24 | `basicNew:` | oops | 1 | Yes | Yes | Allocates a new indexable (Array/ByteArray-shaped) instance of the receiver with the given element count; fails on a negative size or a non-indexable class format. |
| 25 | `instVarAt:` | oops | 1 | No | Yes | Answers named instance variable `n` (1-based) of the receiver; fails if `n` is out of range. |
| 26 | `at:` | oops | 1 | No | Yes | Answers indexable element `n` (1-based) of an Array-format object; fails if out of range or the receiver isn't Array-format. |
| 27 | `at:put:` | oops | 2 | No | Yes | Stores into indexable element `n` (1-based) of an Array-format object, answering the stored value; fails if out of range. |
| 28 | `size` | oops | 0 | No | Yes | Answers the number of indexable elements (Array or ByteArray format); fails on a fixed-shape (Slots-format) receiver. |
| 40 | `byteAt:` | ByteArray/String | 1 | No | Yes | Answers byte `n` (1-based) of a ByteArray/String/Symbol as a smi 0–255; fails if out of range. |
| 41 | `byteAt:put:` | ByteArray/String | 2 | No | Yes | Stores a byte 0–255 at position `n` (1-based); fails if out of range, the value isn't 0–255, or the receiver is a Symbol (interned, immutable). |
| 42 | `byteSize` | ByteArray/String | 0 | No | Yes | Answers the byte length of a ByteArray/String/Symbol. |
| 43 | `replaceFrom:to:with:` | ByteArray/String | 3 | No | Yes | Copies bytes `1..(to−from+1)` of the argument into the receiver's `from..to` range (1-based, inclusive; a no-op if `to < from`); fails if the range doesn't fit either object or the receiver is a Symbol. |
| 44 | `hashBytes` | ByteArray/String | 0 | No | Yes | Answers an FNV-1a hash of the receiver's raw bytes, masked to 30 bits (fits a smi). |
| 45 | `compare:` | ByteArray/String | 1 | No | Yes | Lexicographic byte-by-byte comparison of two byte-format objects, answering −1/0/1 (shorter-is-less on a common prefix). |
| 50 | `value` | BlockClosure | 0 | No† | Yes | Activates a zero-argument block; fails if the receiver doesn't take exactly 0 arguments. Succeeds via `PrimResult::Activated` — it pushes a real guest frame itself rather than returning a value directly (†the activation it pushes may itself allocate, e.g. a Context). |
| 51 | `value:` | BlockClosure | 1 | No† | Yes | Activates a one-argument block with the given argument. |
| 52 | `value:value:` | BlockClosure | 2 | No† | Yes | Activates a two-argument block. |
| 53 | `value:value:value:` | BlockClosure | 3 | No† | Yes | Activates a three-argument block. |
| 54 | `valueWithArguments:` | BlockClosure | 1 | No | Yes | Activates a block whose argument count matches the given Array's size, spreading the Array's elements onto the stack as the block's arguments; fails on an argument-count mismatch. |
| 60 | `ensure:` | control | 1 | No | Yes | Evaluates the receiver (a zero-arg block); once its activation completes — normally, or via a non-local return unwinding through it — evaluates the argument block. Fails if either block isn't zero-argument. |
| 61 | `ifCurtailed:` | control | 1 | No | Yes | Like `ensure:`, but the handler only runs if the protected block's activation is unwound *abnormally* (a non-local return passing through it), not on normal completion. |
| 90 | `quit` | system | 0 | No | No | Requests VM shutdown (the interpreter loop notices `exit_requested` after the current dispatch); answers the receiver. |
| 91 | `printOnStdout:` | system | 1 | No | Yes | Writes the argument's raw bytes directly to the VM's stdout stream; answers the receiver. |
| 92 | `millisecondClock` | system | 0 | No | No | Answers milliseconds elapsed since VM startup, as a smi. |
| 93 | `gcScavenge` | system | 0 | Yes | No | Forces one young-generation (scavenge) collection; answers the receiver. |
| 94 | `gcFull` | system | 0 | Yes | No | Forces a full mark-slide-compact collection; answers the receiver. |
| 95 | `error:` | system | 1 | No | No\* | Prints the argument message plus a full Smalltalk stack trace to stdout and terminates the process — a hard, unrecoverable error report, not a signalable/catchable exception (\*never returns, so "fails" doesn't really apply). |
| 96 | `quit:` | system | 1 | No | No | Like `quit`, but sets an explicit process exit code from the smi argument (0 if the argument isn't a smi). |
| 97 | `gcStats` | system | 0 | Yes | No | Answers an 8-element Array of counters in a fixed, pinned order — scavenge count, full-GC count, eden bytes used, old-gen bytes used, old-gen bytes committed, bytes promoted, marked bytes (last full GC), context-allocation count — read by the soak-test harness and the compiler's Context-elision gate. |
| 100 | `+` | Double | 1 | Yes | Yes | Double-precision addition; fails if the argument isn't a Double. |
| 101 | `-` | Double | 1 | Yes | Yes | Double-precision subtraction. |
| 102 | `*` | Double | 1 | Yes | Yes | Double-precision multiplication. |
| 103 | `/` | Double | 1 | Yes | Yes | Double-precision division; IEEE semantics (division by zero yields `inf`/`nan`, never fails) — only a non-Double argument fails. |
| 104 | `<` | Double | 1 | No | Yes | Double-precision less-than. |
| 105 | `=` | Double | 1 | No | Yes | Double-precision equality. |
| 106 | `sqrt` | Double | 0 | Yes | Yes | Square root; fails only if the receiver isn't a Double. |
| 107 | `floor` | Double | 0 | No | Yes | Floors to the nearest integer, answering a smi; fails if the result isn't finite or doesn't fit in the smi range. |
| 108 | `asDouble` | Double | 0 | Yes | Yes | Converts a smi receiver to a Double. |
| 109 | `printDigits` | Double | 0 | Yes | Yes | Answers a shortest-round-trip decimal string for the receiver (`"nan"`/`"inf"`/`"-inf"` for non-finite values; always includes a decimal point for finite ones). |
| 110 | `asSymbol` | Symbol | 0 | Yes | Yes | Interns the receiver's bytes (a String) as a Symbol, answering the canonical, unique instance. |

Id ranges are deliberately non-contiguous (15/9/6/5/2/8/10/1 used, with gaps
at e.g. 16–19, 29–39, 46–49, 55–59, 62–89, 98–99) — headroom for each group
to grow without renumbering its neighbours.
