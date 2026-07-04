# MACVM ASM Methods — Hand-Written Native Method Bodies

Design for **S23** (next free sprint number — `SPRINTS.md`'s Phase-E stretch
list currently ends at S22). Companion to [`docs/FFI.md`](FFI.md) (S20) but a
different problem: FFI calls **out** to existing native code; this designs a
method whose entire body **is** native code, hand-written by the Smalltalk
programmer directly in `.mst` source. Same posture as the FFI work: a
side/parallel track that touches neither `src/compiler` nor `src/interpreter`,
plus one standalone, non-disruptive deliverable (§7) built now.

## 0. Scope and non-goals

- **Designed now**: the whole feature (§§1–6, 8) — syntax, the calling-
  convention contract, and exactly where it would plug into the real
  compiler pipeline.
- **Built now**: `asm_preview` (§7), a new standalone binary that extracts a
  `<asm: '...'>` method's text and runs it through JASM's real, already-
  tested assembler — proving the mechanism, catching syntax errors, without
  installing anything into a running VM or touching any in-progress
  compiler file.
- **Not built now**: the frontend parser recognizing `<asm: ...>`, and the
  method-installer that would route it to JASM instead of the normal
  bytecode compiler. That's the real S23 sprint.
- **v1 is deliberately restricted to a leaf routine**: no allocation, no
  calls (primitives, other methods, blocks), no safepoints. §4 explains
  exactly why this is the safety boundary that makes the rest of the design
  simple, and why relaxing it is real, separately-scoped future work, not a
  detail to wave at.

## 1. Ground truth: what's already built

Three things this design leans on, all already real and tested:

**JASM's text assembler.** `src/vendor/wfasm/a64/parse.rs` is a genuine
AArch64 (LLVM/GAS-syntax) line parser — registers, SIMD/vector operands,
addressing modes, shifts — feeding `src/vendor/wfasm/a64/mod.rs`'s

```rust
/// Assemble a module's AArch64 text into an [`EncodedModule`].
pub fn assemble(text: &str) -> Result<EncodedModule>
```

which resolves labels, `.globl`, alignment directives, and relaxable
conditional branches, then calls `encode.rs` per instruction. This is tested
against a 1181-form corpus (`corpus/aarch64.tsv`, `corpus_replay.rs`) — real
assembly text in, real machine code out, already proven correct. Also
exposed as a trait: `Encoder::encode(&self, asm_text: &str) -> Result<EncodedModule>`.
`EncodedModule` itself:

```rust
pub struct EncodedModule {
    pub code: Vec<u8>,                      // encoded bytes, reloc fields left 0
    pub symbols: BTreeMap<String, usize>,   // name -> byte offset, every .globl/label
    pub relocs: Vec<Reloc>,                 // patches to apply at load time
    pub externs: Vec<String>,               // referenced-but-undefined host symbols
}
```

**The calling convention (`docs/arm64.md` §3), already decided.** Apple's
AAPCS64: `x18` platform-reserved, never use it; `x16`/`x17` are IP0/IP1
scratch (what JASM's own far-call veneers use); `x29`=FP/`x30`=LR/`sp` per
AAPCS64. MACVM's own layer on top: "Compiled code uses the general
allocatable set (`x0`–`x17` minus IP scratch)... the callee-saved VM
registers [`x24`-`x28`] are preserved across compiled↔runtime transitions so
the runtime (GC, lookup, deopt) can always find VM state." This is already
the contract ordinary tier-1-JIT-compiled code follows — an ASM method is
just another producer of code that has to follow it too.

**The actual SmallInteger tag scheme** (`src/oops/layout.rs`) — note this
*resolves* `docs/arm64.md` §1.1's "open: 1-bit vs. 3-bit tag" note; the real
answer, as implemented, is 2 bits:

```rust
pub const TAG_BITS: u32 = 2;
pub const TAG_MASK: u64 = 0b11;
pub const INT_TAG: u64 = 0b00;   // smi
pub const MEM_TAG: u64 = 0b01;   // heap oop; address = word - MEM_TAG
```

`src/oops/smi.rs`'s own comment: "`INT_TAG == 0`, so no OR is needed: the
tagged word is exactly `v * 4`" — and this is an *exact arithmetic identity*,
not an approximation: `SMI_MAX`/`SMI_MIN` restrict the payload to 62 signed
bits specifically so `v << 2` never truncates. §4 leans on this precisely.

**The Nmethod/CodeTable decoupling** (`src/codecache/nmethod.rs`) — "is this
method's implementation native code" is *already* a side-table fact
(`MethodOop → Nmethod` via a separate `CodeTable`), not a field on
`MethodOop` itself. This means an ASM method's installed code can be
represented **exactly like a tier-1-JIT-compiled method's** — the same
`Nmethod`/`CodeTable`/publish machinery, just fed bytes from
`wfasm::a64::assemble()` instead of the compiler's own decode → convert →
regalloc → emit pipeline. No new VM-level representation is needed; only a
new *source* for the bytes.

## 2. Precedent check: none found

Searched `strongtalk-repo/StrongtalkSource/` directly (not from memory) for
any mechanism to embed raw assembly in a method body. There isn't one — real
Strongtalk exposes exactly two method-body forms: ordinary bytecode, and
`<primitive: N>` pragmas that call **into** a VM-implemented primitive,
never raw user-authored machine code. MACVM's own existing use of assembly
(stub routines, the tier-1 compiler's output) has always been VM-authored,
never Smalltalk-image-authored. **This is a genuinely novel feature for this
lineage**, not a port of something Strongtalk already had.

The nearer prior art is systems-language inline assembly — GCC's
`asm volatile(...)`, Rust's `asm!` macro — both of which, notably, also
auto-generate the surrounding register-save/frame machinery around a
hand-written instruction sequence rather than requiring a fully freestanding
function. §4's leaf-routine contract follows the same shape, just pushed
further: v1 needs *no* generated wrapper at all (see below).

## 3. The syntax

A new pragma, `<asm: 'text'>` — deliberately distinct from `<primitive: N>`,
since these are different mechanisms (call into Rust vs. *be* the machine
code):

```smalltalk
SmallInteger extend [
    bitAnd: aSmallInteger [
        <asm: 'and x0, x0, x1
ret'>
    ]
]
```

**Why a quoted string, not bare text like `<primitive: N>`'s bare integer.**
`image_store::mst`'s bracket-matcher (and the real frontend parser will need
the same care) tracks `[`/`]` depth character-by-character outside of
comments/strings — and ARM64 memory operands use bracket syntax
(`ldr x0, [x1, #8]`). Bare, unquoted asm text risks a stray `[`/`]`
desynchronizing the *method's own* body-bracket matching. Wrapping the text
in a Smalltalk string literal puts it inside a span every `.mst`-aware parser
already treats as opaque (`skip_string_literal`), removing the hazard by
construction instead of relying on addressing-mode brackets happening to
stay balanced.

**No Smalltalk fallback body** — contrast `<primitive: N>`, which keeps a
bytecode body as the "primitive failed at runtime" fallback
(`frontend/codegen.rs`: `prim_fails = method.primitive.is_some() &&
!method.body.is_empty()`). There's no equivalent runtime failure trigger for
hand-assembled code — it either assembles at install time or it's a compile
error, reported through the same `(message, character position)` contract
every other `.mst` compile error already uses (A20).

## 4. The calling-convention contract

This is the load-bearing section — get it wrong and you corrupt the VM, not
just one computation.

| Registers | Rule |
|---|---|
| `x0` | Receiver on entry; **must** hold the return value on exit |
| `x1`, `x2`, … | Arguments on entry, in selector order (matches `emit.rs`'s existing "marshal receiver+args into `x0..x5`" convention) |
| `x0`–`x15` | Free to use for scratch/computation |
| `x16`/`x17` | **Reserved** — IP0/IP1, veneer scratch |
| `x18` | **Reserved** — platform (`docs/arm64.md` §3, verbatim: "never use it") |
| `x19`–`x23` | **Reserved** — outside compiled code's own general-allocatable set |
| `x24`–`x28` | **Reserved** — VM/interpreter state, must survive compiled↔runtime transitions |
| `x29`, `x30`, `sp` | **Untouched** — no call means no frame is needed; `ret` uses whatever `x30` held on entry (the send site's own return address) |

**The exit contract**: end with one `ret`. Nothing else touches control flow.

**The v1 restriction, spelled out**: an ASM method must not allocate, call
anything (a primitive, another method, a block), or otherwise reach a
safepoint. This is what licenses skipping oop-map generation entirely
(`docs/arm64.md` §6: the collector needs oop-maps to find live oops "in
registers and stack slots of **optimized frames**" — but only at a
safepoint; a routine that never allocates and never polls never *hits* one,
so there is no moment during its execution the collector could observe its
registers at all). Relaxing this later needs the full oop-map/safepoint
story `docs/arm64.md` §6 already describes for ordinary compiled methods —
real, deferred work, not assumed away.

**Why `bitAnd:` works as a single instruction — and why this is an exact
proof, not a hopeful guess.** Both `x0` and `x1` arrive tagged: `raw = v << 2`
(§1's exact identity). Bitwise AND commutes with a common left shift of
zero-fill bits: `(a<<2) & (b<<2) = (a & b)<<2`. The low 2 bits of *both*
operands are 0, so the low 2 bits of the raw AND are *also* 0 — the result
is already a correctly-tagged smi, with no untagging or re-tagging step at
all. The identical argument holds for `bitOr:`/`bitXor:` (`orr`/`eor`) — the
same shift-distributes-over-bitwise-op reasoning, not a separate
coincidence.

**A cautionary counter-example, worth stating explicitly.** A tempting next
example is a leading-zero-count (`clz`) or population-count "fast `highBit`"
method. This is *not* a trivial one-instruction port the way `bitAnd:` is:
`clz` on the raw tagged word does **not** equal `clz` on the untagged value
minus a fixed correction for negative operands, because two's-complement
sign extension behaves differently under a left-shift-by-2 depending on
sign (a positive tagged value has exactly 2 more leading zeros than its
untagged self; a negative one, being already all-1s in its sign-extended
high bits, does not). Getting this wrong doesn't crash — it silently returns
a plausible-looking wrong answer for negative receivers, precisely the kind
of bug this whole project's practice is built around catching before it
ships, not after. Any ASM method touching a signed, sign-sensitive operation
needs its own explicit correctness argument, not a pattern-matched
assumption from `bitAnd:`'s simplicity.

## 5. Install-time integration (designed, not built)

Where this plugs into the real pipeline, for whenever S23 actually happens
(not attempted here — this is exactly the "compiler work" this side project
stays out of):

- `frontend`'s method AST (`MethodNode` in `frontend/codegen.rs`) gains an
  `asm_body: Option<String>` field alongside the existing `primitive`/`body`
  fields, populated by the parser recognizing `<asm: '...'>`.
- `compile_method_inner` branches before `emit_prologue`/`emit_body`: if
  `asm_body` is set, skip Smalltalk bytecode generation entirely, call
  `wfasm::a64::assemble` on the text, and on success install the result as
  an `Nmethod` via the same `CodeTable`/publish path a tier-1-JIT-compiled
  method already uses (§1) — but **unconditionally at install time**, not
  lazily on a hotness counter the way tier-1 promotion works today, since
  there's no bytecode-interpreted stage to be hot or cold *from*.
- On an `assemble` error, report it as a compile error through the existing
  `(message, character position)` contract (A20) — `assemble`'s own errors
  are already line-numbered (`"line {}: `{raw}`"`, from `mod.rs`'s error
  context), so mapping that back to a `.mst` source position is direct.

## 6. Composability with FFI (S20)

`EncodedModule`'s `externs`/`relocs` fields are the exact mechanism FFI's
Tier 1 (direct POSIX calls) would use — a hand-written ASM method could in
principle reference an external symbol (`_objc_msgSend`, a libc function)
directly. This is explicitly **out of scope** for v1's no-calls restriction
(§4) — referencing an extern means *calling* it, which reopens the
oop-map/safepoint question — but it's a natural, worth-noting future
integration point: FFI's own shape-keyed trampolines (`docs/FFI.md` §5)
could plausibly themselves be authored as ASM methods once both features are
real.

## 7. What's built now: `asm_preview`

A new, standalone binary — `src/bin/asm_preview.rs`, one new file plus one
new `[[bin]]` `Cargo.toml` entry in the existing `macvm` crate, touching no
in-progress compiler file. It:

1. Reads a file: either a whole `<asm: '...'>` method (extracts the quoted
   text, un-escaping `''` → `'`) or a bare `.s`-style file of raw assembly
   text (used as-is) — so it doubles as a plain "does this assemble"
   checker independent of any Smalltalk wrapping.
2. Runs the text through the real `wfasm::a64::assemble` — the exact
   function the real S23 sprint would eventually call, not a stand-in.
3. On success: reports byte count, a hex listing, the symbol table, and any
   relocations/externs. On failure: prints `assemble`'s own line-numbered
   error and exits non-zero.
4. **Best-effort v1-contract linting**: scans instruction operands for any
   of the reserved registers (§4's table) and warns if one appears — not a
   formal verifier (it can't know if a reserved register is genuinely
   touched vs. just mentioned in a comment-adjacent way), but a cheap,
   real check that catches the obvious mistake.

Installs nothing; never touches a running VM. Worked, tested examples:
`SmallInteger>>bitAnd:`/`bitOr:`/`bitXor:` (proven correct in §4) — plus a
deliberately-reserved-register example confirming the linter actually
fires, and a deliberately-malformed example confirming `assemble`'s real
parse errors surface cleanly through this tool.

## 8. Deferred

- **Allocation/call support** — needs oop-maps, safepoint polling, and the
  compiled↔runtime transition convention `docs/arm64.md` §6 already
  describes for ordinary compiled methods. Not attempted here.
- **Non-oop (float/HFA) calling conventions** — a possible future reuse of
  FFI's `g`/`f`/`h`/`i`/`s`/`v` vocabulary (`docs/FFI.md` §1) for an ASM
  method's own declared return/argument shape, if a method wants to work
  with unboxed floats directly. Not designed here.
- **An explicit "trap back to interpreter" escape hatch** — would let an ASM
  method bail into ordinary Smalltalk execution mid-routine; genuinely
  harder than everything else here (needs a synchronous deopt-like
  transition), not attempted.
- **PIC/customization-key integration** — v1 ASM methods are monomorphic by
  construction (always this exact code for this exact selector); no guard,
  no polymorphic redirection. Revisit only if a real use case needs it.

## 9. Cross-references

- `SPRINTS.md` gains **S23** (Phase-E stretch, alongside S20/S21/S22).
- `SPEC.md` decision log gains **A24**, following A17–A23's format.
