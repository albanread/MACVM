# Sprint S03 — Sends, inline caches, primitives

Objective: implement real message dispatch end-to-end — MethodDictionary and
hierarchy lookup backed by the global LookupCache, the `send`/`send_super`
bytecodes with the full IC side-table state lattice (empty→mono→poly(4)→mega,
guard self-heal), `doesNotUnderstand:` with `Message` construction, the
primitive mechanism with the smi/oops/ByteArray groups, and invocation-counter
bumping (count only, no compile trigger). Implements SPEC §2.4
(MethodDictionary), §4.3, §5.3, §6.1–6.3, §10 (partial: smi, oops,
bytes, minimal system group).

## Prerequisites

- **S0** (`src/oops.rs`): `Oop`, tag helpers, `Mark`, `SmallInt` with
  overflow-checked arithmetic, typed wrappers (`MemOop`, `KlassOop`,
  `ArrayOop`, `ByteArrayOop`, `SymbolOop`, `MethodOop`), `src/oops/layout.rs`
  constants (or the flat-file equivalent to be moved there this sprint).
- **S1** (`src/memory.rs`): reservation + eden bump allocator (aborts when
  full — no GC yet), object formats (SPEC §2.3), klass objects (§2.4),
  `Universe::genesis()`, symbol table with interning, identity-hash counter,
  `print_oop` debug printer.
- **S2** (`src/bytecode/`, `src/interpreter.rs`): opcode enum + decode,
  `BytecodeBuilder`, disassembler, dispatch loop and frame layout (SPEC §5.1)
  for opcodes 00–12 / 30–33 / 40–41 **excluding** sends (20–23) and closures
  (0F/10/11 stubbed or trapping); frame push/pop with exact stack discipline.
- This sprint converts flat files to directories per CONVENTIONS §1:
  `src/interpreter.rs` → `src/interpreter/`, and `src/runtime.rs` +
  `src/lookup.rs` → `src/runtime/` (fold the existing `lookup.rs` content into
  `src/runtime/lookup.rs`).

## Deliverables

- `src/oops/layout.rs`: all new constants (frame slots, IC stride/slot
  indices, POLY/MEGA guard markers, meta-smi packing, primitive ids).
- `src/oops/method_dict.rs`: `MethodDictOop` wrapper — open-addressed
  (Symbol, CompiledMethod) table, growth by rehash-into-new.
- `src/runtime/lookup.rs`: `lookup()` hierarchy walk + `LookupCache`
  (probe/insert/flush) + flush triggers.
- `src/runtime/primitives.rs`: `PrimResult`, `PrimFn`, `PrimDesc`, static
  primitive table, smi/oops/bytes/system groups.
- `src/runtime/error.rs`: DNU fallback printer, `print_stack_trace`.
- `src/runtime/vm_state.rs` (or `mod.rs`): `VmState` additions — `InterpRegs`,
  `ic_epoch`, `out: Box<dyn Write>`, `exit_requested`, `VmOptions`
  (`MACVM_TRACE=ic` honored).
- `src/interpreter/send.rs`: `send_generic`, `send_super_generic`,
  `activate_method`, DNU path.
- `src/interpreter/ic.rs`: `InterpreterIc` view + transition function +
  `ic_state` readout for tests/tier-1.
- Universe extensions: `message_klass` (2 named slots: `selector`,
  `arguments`), well-known interned selectors `#doesNotUnderstand:`,
  `#mustBeBoolean`, `#cannotReturn:`.
- `tests/it_sends.rs`, `tests/it_ic.rs` (see `tests_s03.md`).

## Design

### Data structures

**Stack-pointer convention (pinned for the whole VM).** `sp` is the index of
the **next free slot** (one past top-of-stack). A send with `argc` arguments
therefore sees: args at `stack[sp-argc .. sp-1]`, receiver at
`stack[sp-argc-1]`. Named constant helpers in `interpreter/frame.rs`:
`fn receiver_index(sp: usize, argc: u8) -> usize { sp - argc as usize - 1 }`.

> **SPEC-QUESTION:** SPEC §5.3 writes `rcvr = stack[sp - argc]`, which is only
> correct if `sp` is the index *of* the top element. SPEC never pins the `sp`
> convention. This sprint pins `sp = next free slot`, receiver at
> `sp - argc - 1`; SPEC §5.3 should be amended to say so.

**Interpreter registers.** The current activation state moves from S2 loop
locals into `VmState` so that send/primitive/DNU helpers can mutate it:

```rust
pub struct InterpRegs {
    pub fp: usize,        // frame base index into ProcessStack
    pub sp: usize,        // next free slot
    pub bci: u32,         // next bytecode to fetch
    pub method: MethodOop // currently executing CompiledMethod/CompiledBlock
}
// in VmState: pub regs: InterpRegs
```

Tier-0 performance is fine with this (SPEC §13 interpreter target); the
dispatch loop may shadow them in locals and write back before any call into
`runtime/`.

**Frame layout** — unchanged from S2 (SPEC §5.1). Use only the named constants
`FRAME_METHOD=0, FRAME_SAVED_FP=1, FRAME_SAVED_BCI=2, FRAME_CONTEXT=3,
FRAME_RECEIVER=4, FRAME_TEMPS_BASE=5` from `layout.rs`. **S4 will insert two
slots (serial, marker) and move `FRAME_TEMPS_BASE` to 7** — never hard-code 5.

**MethodDictionary** (`MethodDictOop`, format `IndexableOops`). One named
field `tally: smi` (live entry count), then indexable part
`[size][k0, v0, k1, v1, …]` — `capacity = size/2`, always a power of two.
Keys are `SymbolOop` or `nil` (empty). No tombstones: v1 has **no method
removal** (out of scope). Probing: linear, step 1, on the symbol's identity
hash: `h = sym.identity_hash() & (capacity-1)`; slot *i* occupies indexable
elements `2*i` and `2*i+1`. Growth: on insert, if `4*(tally+1) > 3*capacity`,
allocate a new dictionary of `2*capacity`, reinsert all pairs, store the new
dictionary into the klass's `methods` slot. Symbols get their identity hash
assigned **eagerly at intern time** (S1 must already do this; if it is lazy,
make it eager now) so probing never mutates mark words.

**LookupCache** (`src/runtime/lookup.rs`) — Rust-side, SPEC §6.1:

```rust
pub const LOOKUP_CACHE_SIZE: usize = 4096;              // (tunable)

#[derive(Copy, Clone)]
struct CacheEntry { klass: Oop, selector: Oop, method: Oop } // klass = Oop(0) => empty

pub struct LookupCache {
    // 2 entries per index: [2*h] = primary, [2*h+1] = victim
    entries: Box<[CacheEntry]>,   // len = 2 * LOOKUP_CACHE_SIZE
}

impl LookupCache {
    pub fn probe(&mut self, klass: KlassOop, selector: SymbolOop) -> Option<MethodOop>;
    pub fn insert(&mut self, klass: KlassOop, selector: SymbolOop, method: MethodOop);
    pub fn flush(&mut self);
    fn hash(klass: KlassOop, selector: SymbolOop) -> usize {
        (((klass.raw() >> 3) ^ (selector.raw() >> 3)) as usize) & (LOOKUP_CACHE_SIZE - 1)
    }
}
```

- `probe`: check primary; on victim hit, **swap primary/victim** (promote) and
  return; else `None`. (`&mut self` because of the promotion swap.)
- `insert`: demote current primary into the victim slot, write the new entry
  as primary.
- `flush`: zero everything. Flush triggers (wire all three now):
  1. method (re)definition — `install_method` (below);
  2. class-hierarchy change — v1 has none after a class is created, but the
     entry point `Universe::set_superclass` (if ever added) must call it;
  3. full GC — klass/selector fields are raw addresses; S8's `gc_epilogue()`
     calls `flush()`. Add the empty `gc_epilogue(&mut VmState)` hook **now**
     so S8 is a one-line change.

**IC side table** (SPEC §4.3). Each CompiledMethod's `ics` is a plain heap
`Array`, stride 4 per site:

| slot | name | contents |
|---|---|---|
| base+0 | `sel` | SymbolOop |
| base+1 | `meta` | smi: bits 7:0 = argc, bits 31:8 = install epoch |
| base+2 | `guard` | `nil` (empty) \| KlassOop (mono) \| smi `IC_GUARD_POLY=1` \| smi `IC_GUARD_MEGA=2` |
| base+3 | `target` | `nil` \| MethodOop \| smi nmethod-id (S10+) \| ArrayOop poly pairs |

Poly pairs array: heap `Array` of 8 elements `[k1,m1,k2,m2,k3,m3,k4,m4]`,
unused pairs `nil`; arity = index of first `nil` key; max 4 pairs *(tunable)*.
Because both the IC table and poly arrays are ordinary heap objects, **GC
scans them for free** — same property Strongtalk gets from PICs being heap
objArrays (reference-vm-analysis §3.3).

**Dependency versioning — the `ic_epoch`.** `VmState.ic_epoch: u32` (24 bits
used). Every `install_method` (and any future hierarchy change) bumps it.
An IC entry is *current* iff `meta.epoch == vm.ic_epoch`. A stale entry with a
matching guard **self-heals**: re-lookup, rewrite target, stamp new epoch
(SPEC §6.2's lazy flush). Comparison is equality-only; wrap at 2^24 is an
accepted ABA risk (`debug_assert!(vm.ic_epoch < 1 << 24)` — 16M definitions
is unreachable in v1).

> **SPEC-QUESTION:** SPEC §6.2 specifies a *per-klass* dependency version, but
> the pinned klass layout (§2.4) has no version slot and the IC stride (§4.3)
> is pinned at 4, so there is nowhere to store a per-klass stamp. This sprint
> coarsens to one global epoch packed into the IC `argc` slot
> (bits 31:8) — semantically identical (stale entries self-heal on next use),
> merely causing one extra lookup per site after any definition. A per-klass
> Rust-side map can restore precision later without changing heap layouts.
> Also: SPEC §4.3 calls slot 1 plain `argc: smi`; the epoch packing is an
> extension of that slot's encoding.

**`InterpreterIc`** (`src/interpreter/ic.rs`) — the 4-slot view:

```rust
pub struct InterpreterIc { pub ics: ArrayOop, pub base: usize } // base = ic_idx * 4
impl InterpreterIc {
    pub fn at(method: MethodOop, ic_idx: u16) -> InterpreterIc;
    pub fn selector(&self) -> SymbolOop;
    pub fn argc(&self) -> u8;
    pub fn epoch(&self) -> u32;
    pub fn guard(&self) -> Oop;
    pub fn target(&self) -> Oop;
    pub fn set_mono(&self, k: KlassOop, m: MethodOop, epoch: u32);
    pub fn set_poly(&self, pairs: ArrayOop, epoch: u32);
    pub fn set_mega(&self);
    pub fn set_empty(&self);
}

#[derive(PartialEq, Debug)]
pub enum IcState { Empty, Mono, Poly(u8 /*arity*/), Mega }
pub fn ic_state(method: MethodOop, ic_idx: u16) -> IcState;   // test + S14 feedback readout
```

**Primitives** (`src/runtime/primitives.rs`), SPEC §10 signature:

```rust
pub enum PrimResult { Ok(Oop), Fail }        // S4 adds an Activated variant
pub type PrimFn = fn(vm: &mut VmState, args: &[Oop]) -> PrimResult;

pub struct PrimDesc {
    pub id: u16,
    pub name: &'static str,   // for tracing/disassembly
    pub f: PrimFn,
    pub argc: u8,             // argument count EXCLUDING receiver
    pub can_allocate: bool,
    pub can_fail: bool,
}
pub static PRIMITIVES: &[PrimDesc];           // sorted by id; lookup via binary search
pub fn prim_by_id(id: u16) -> Option<&'static PrimDesc>;
```

Calling convention: `args[0]` is the receiver, `args[1..=argc]` the
arguments. The interpreter copies receiver+args from the stack into a
stack-local `[Oop; 6]` buffer before the call (avoids `&mut VmState` /
stack-slice aliasing). **Additionally**, before the call it records
`vm.prim_arg_base = sp - argc - 1`; `VmState::prim_arg(i)` re-reads the live
stack slot. Rule (binding from S3 onward): a primitive with
`can_allocate = true` must not read the copied `args` slice **after** its
first allocating call — it must re-fetch via `vm.prim_arg(i)`. The process
stack is a GC root, so re-fetched oops are always current; this is the exact
choke-point pattern S7's Handle retrofit builds on (see Pitfalls).

**VmState additions.** `regs: InterpRegs`, `ic_epoch: u32`,
`prim_arg_base: usize`, `out: Box<dyn std::io::Write>` (stdout by default;
tests substitute a buffer — this is how transcript assertions work before the
S5 golden runner exists), `exit_requested: bool`, `start_instant:
std::time::Instant` (for `millisecondClock`).

### Algorithms

**1. `send` bytecode, end-to-end** (`send_generic(vm, argc, ic_idx)`):

```rust
pub fn send_generic(vm: &mut VmState, argc: u8, ic_idx: u16);
pub fn send_super_generic(vm: &mut VmState, argc: u8, ic_idx: u16);
```

The dispatch loop decodes `send <u8 ic>` / `send_w <u16 ic>`, reads `argc`
from the IC meta slot, and calls `send_generic` (debug_assert that the passed
argc equals the IC's). Steps:

1. `ic = InterpreterIc::at(vm.regs.method, ic_idx)`.
2. `rcvr = stack[vm.regs.sp - argc - 1]`.
3. `k = klass_of(rcvr)`: smi tag → `universe.smi_klass`; heap tag → header
   klass word. (Tag 10 is reserved-unused; debug_assert it never appears.)
4. **Fast path**: `ic.guard() == k && ic.epoch() == vm.ic_epoch` →
   `activate(vm, ic.target(), argc)`; done.
5. `guard == IC_GUARD_POLY`: if epoch stale → *reverify* (row 7 below), then
   re-probe. Scan pairs for `k`; hit → activate. Miss → transition table.
6. `guard == IC_GUARD_MEGA`: `lookup(vm, k, sel)` (which probes the
   LookupCache first); found → activate, **no IC write**; not found → DNU.
7. Otherwise (empty, or mono with wrong/stale guard): full `lookup`, then the
   transition table below decides the IC write; then activate or DNU.

**2. IC transition table** — the heart of this sprint. `E` = current
`vm.ic_epoch`; `L(k)` = full lookup of the site's selector starting at `k`
(for super sites: starting at `site_holder.superclass`, see step 7 below).

| # | State (guard) | Event | Action | New state |
|---|---|---|---|---|
| 1 | empty (`nil`) | send, `L(k)`=m | `set_mono(k, m, E)`; activate m | mono |
| 2 | empty | send, `L(k)`=∅ | DNU; **IC untouched** | empty |
| 3 | mono(k0,m0) | `k==k0`, epoch==E | activate m0 (fast path) | mono |
| 4 | mono(k0,m0) | `k==k0`, epoch≠E | *self-heal*: `L(k0)`=m′ → `set_mono(k0,m′,E)`, activate; `L(k0)`=∅ → `set_empty()`, DNU | mono / empty |
| 5 | mono(k0,m0) | `k≠k0`, `L(k)`=m | alloc pairs `[k0,m0,k,m,nil×4]`; `set_poly(pairs, E)`; activate m | poly(2) |
| 6 | mono(k0,m0) | `k≠k0`, `L(k)`=∅ | DNU; IC untouched | mono |
| 7 | poly(n) | pair hit, epoch==E | activate m_i | poly(n) |
| 8 | poly(n) | epoch≠E (before any probe) | **reverify all pairs**: each (k_i,m_i) → `L(k_i)`; found → rewrite m_i; ∅ → delete pair, compact left; stamp epoch:=E; then re-probe (→ row 7/9/10/11) | poly(≤n) |
| 9 | poly(n<4) | miss, `L(k)`=m | append (k,m) at first nil pair | poly(n+1) |
| 10 | poly(4) | miss, `L(k)`=m | `set_mega()`; activate m | mega |
| 11 | poly(n) | miss, `L(k)`=∅ | DNU; IC untouched | poly(n) |
| 12 | mega | send, lookup found | activate; **never writes IC** | mega |
| 13 | mega | send, lookup ∅ | DNU | mega |

Row 8 must reverify *all* pairs at once before restamping the epoch —
restamping after healing only the hit pair would let the other, still-stale
pairs pass the fast path. Mega is a sink: no transition back (no IC flush op
in v1). If reverification in row 8 empties the pair array, `set_empty()` and
fall through to row 1.

Provide the transition logic as one function so tests can drive it directly:

```rust
/// Handles every non-fast-path case. Returns the method to activate,
/// or None => caller runs the DNU path. Never activates itself.
pub fn ic_transition(
    vm: &mut VmState,
    caller: MethodOop,       // method owning the IC table
    ic_idx: u16,
    rcvr_klass: KlassOop,
    is_super: bool,
) -> Option<MethodOop>;
```

**3. `lookup`** (`src/runtime/lookup.rs`):

```rust
pub fn lookup(vm: &mut VmState, klass: KlassOop, selector: SymbolOop) -> Option<MethodOop>
```
1. `vm.lookup_cache.probe(klass, selector)` → hit returns immediately.
2. Walk: `k = klass; while k != nil { k.methods().probe(selector)? ; k = k.superclass() }`.
3. On success `insert` into the cache (keyed by the *original* receiver
   klass, not the defining klass — that is Strongtalk's LookupKey shape).
4. `None` on failure — **no negative caching** in v1 (note: DNU-heavy code
   pays full walks; acceptable).

**4. `install_method`** (Universe / runtime):

```rust
pub fn install_method(vm: &mut VmState, klass: KlassOop, selector: SymbolOop, m: MethodOop)
```
1. `MethodDictOop::insert` (grow-rehash if needed; growing allocates — park
   the new dictionary reference only in the klass slot, never across a second
   allocation in a Rust local).
2. `m.set_holder(klass)`.
3. `vm.lookup_cache.flush()`.
4. `vm.ic_epoch += 1` (this is the lazy IC flush — SPEC §6.2).

**5. `activate_method`** (`src/interpreter/send.rs`) — order per SPEC §5.3:

```rust
pub fn activate_method(vm: &mut VmState, m: MethodOop, argc: u8)
```
1. Bump invocation counter: low 16 bits of `m.counters()`, **saturating** at
   0xFFFF (a wrap would spuriously re-trigger in S10). Bump happens for every
   arrival, including primitive-success short-circuits (tier-1 wants to see
   those invocations too).
2. If `m.primitive() != 0`: copy receiver+args to the local buffer, set
   `prim_arg_base`, call the primitive.
   - `Ok(v)`: `sp -= argc+1; push v`; return to dispatch (no frame).
   - `Fail`: `debug_assert!(m.flags().prim_fails)`; fall through to step 3 —
     the method's bytecode is the fallback (SPEC §10).
3. Push frame: `[method=m][saved_fp][saved_bci=vm.regs.bci][context=nil]
   [receiver=stack[rcvr_idx]][temps... = nil]`; set
   `regs = { fp: new_fp, sp: new_fp + FRAME_TEMPS_BASE + ntemps, bci: 0, method: m }`.
   Stack-overflow check against the process-stack capacity here (VM error,
   not a Smalltalk send, in v1).
4. `has_ctx` methods: Context allocation is **S4**; until then
   `debug_assert!(!m.flags().has_ctx)`.

Return (`return_tos`, S2, re-validated now that argc>0 callees exist): result
= `stack[sp-1]`; `stack[fp - argc - 1] = result; sp = fp - argc;` restore
`fp/bci/method` from saved slots (argc read from the *returning* method's
flags).

**6. DNU path** (`send_generic` lookup failure):
1. Allocate `Array` of `argc`, fill from `stack[sp-argc .. sp-1]` (stack is
   the source — live slots, not stale Rust copies).
2. **Push the array onto the operand stack** (parks it as a GC root), then
   allocate the `Message` (klass `message_klass`, slots `selector`,
   `arguments`); pop the array into `message.arguments`. This ordering is the
   S7-safe pattern — see Pitfalls.
3. Rewrite the stack: `sp -= argc; stack[sp-1]` stays the receiver; push
   message ⇒ receiver + 1 arg.
4. Full lookup of `#doesNotUnderstand:` from `klass_of(rcvr)` (do **not** go
   through the site's IC — the site keeps its own selector's state).
   Found → `activate_method(vm, dnu_method, 1)`.
5. Not found (no library yet, or broken image): VM fallback — print to
   `vm.out`, exact format (pinned for golden tests):

   ```
   DNU #<selector> (receiver class <KlassName>)
     <Holder>>><selector> @<bci>
     ...
   ```
   (top frame first, one line per frame via `print_stack_trace`), then
   terminate the process with exit code 1. No recursion possible: the
   fallback never re-sends.

**7. Super sends** (`send_super_generic`): identical to `send_generic` except
every `L(k)` in the transition table is replaced by
`lookup(vm, vm.regs.method.holder().superclass(), sel)` — the lookup start
is **static per site** (the *executing* method's holder), while the IC guard
is still the receiver klass (SPEC §5.3). Consequence: all pairs of a poly
super site share one target; harmless. The holder must be read from the
currently executing method, not from the receiver's hierarchy (see Pitfalls).

**8. Counters**: `fn bump_invocation(m: MethodOop)` — smi field `counters`,
invocation = bits 15:0, saturate; reserved bits untouched. No trigger, no
loop-counter share this sprint (S10).

### Layer boundaries

- `oops/` may not call into `runtime/` or `interpreter/`. `method_dict.rs`
  takes the allocator as a parameter (`&mut VmState` or a narrower alloc
  trait) for growth, but contains no dispatch logic.
- `runtime/lookup.rs` knows MethodDictOop and KlassOop; it never touches
  frames or `InterpRegs`.
- `runtime/primitives.rs` may read/write the operand stack **only** through
  `VmState::prim_arg`/push helpers; it never pushes frames (that changes in
  S4 with `Activated`).
- `interpreter/` owns all frame and `InterpRegs` mutation; it is the only
  caller of `ic_transition`, `lookup` (via send), and primitives.
- `bytecode/` remains pure data + builder; the builder gains
  `add_send(selector, argc)` which appends an IC site and returns its index.

## Implementation order

1. `layout.rs`: IC slot constants, `IC_GUARD_POLY/MEGA`, meta-smi pack/unpack,
   counter masks, primitive-id constants. Compiles standalone.
2. `MethodDictOop`: probe/insert/grow + unit tests (no interpreter needed).
3. Universe: `message_klass`, well-known selectors, `install_method` (with
   flush hooks calling stubs).
4. `LookupCache` + `lookup()` + `gc_epilogue` stub + unit tests.
5. `PrimResult`/`PrimFn`/`PrimDesc`/table skeleton + smi group + unit tests.
6. oops group, bytes group, system group (`quit`, `printOnStdout:`,
   `millisecondClock`) + `vm.out` plumbing.
7. Interpreter restructure: `InterpRegs` into VmState, directory split,
   `Frame` accessor cleanup; S2 tests still green.
8. `activate_method` (counter bump + primitive step + frame push) and the
   revised return path.
9. `send_generic` fast path + `ic_transition` + `ic_state`; `send`/`send_w`
   opcodes live.
10. `send_super_generic`; `send_super`/`send_super_w` opcodes.
11. DNU path + `print_stack_trace` + fallback printer.
12. Integration tests + gate script `just gate-s03`.

Each step leaves `cargo test` green.

## Pitfalls

- **Do not imitate Strongtalk's self-modifying sends.** Strongtalk encodes IC
  state in the opcode and rewrites bytecode on miss
  (reference-vm-analysis §3.3). MACVM bytecode is immutable (SPEC §4, Δ); all
  state lives in the IC side table. If you find yourself wanting to patch
  bytecode, you are off-spec.
- **PIC storage must be GC-visible.** Strongtalk's interpreter PICs are heap
  objArrays precisely so GC scans them for free. Ours likewise: poly pair
  arrays and the IC table itself are ordinary heap Arrays. Never mirror
  klass/method oops into Rust-side structs — the single exception is the
  LookupCache, which has an explicit flush contract.
- **LookupCache is address-keyed** (`raw() >> 3` hashing): a moving full GC
  (S8) invalidates every entry. The `gc_epilogue → flush()` hook must exist
  *now*, dead code or not, so S8 cannot forget it. The same applies to any
  future Rust-side table keyed by oop bits.
- **No `become:`-like operations.** Nothing in v1 changes an object's identity
  or klass in place. If such an operation is ever added, every cache in this
  sprint (LookupCache, ICs via epoch, poly arrays) must be flushed — leave a
  comment at the flush sites saying so.
- **Raw oops across allocation — the S7 choke-point pattern.** GC does not
  exist yet, but S7 will collect on *every* allocation under
  `MACVM_GC_STRESS=1`, and Handles only arrive in S7. Write S3 code so the
  retrofit is cheap: (a) all allocation already goes through the single
  `VmState` alloc choke point (SPEC §7.2); (b) any oop needed *across* an
  allocation is parked in a heap-visible root — an operand-stack slot (push
  before alloc, pop after) or an already-reachable object field — never a
  plain Rust local. Concrete instances this sprint: DNU (array parked on the
  stack before Message alloc), MethodDictionary growth (new dict installed
  into the klass slot immediately; reinsert reads pairs from the *old* dict,
  which is reachable until replaced), poly-array creation (read k0/m0 from
  the IC slots *after* allocating the array, not before). S7 then only adds
  `HandleScope` where the stack-parking idiom is awkward; nothing needs
  re-architecting.
- **Primitive arg aliasing/staleness:** the copied `args` buffer is raw oops.
  Pre-GC this is safe everywhere; the binding rule (re-fetch via
  `vm.prim_arg(i)` after any allocation in `can_allocate` primitives) must be
  followed *now* so S7 doesn't have to audit every primitive.
- **Row 8 reverification must be all-pairs-then-restamp** (rationale in the
  table). Partial healing + restamp is the subtle stale-dispatch bug.
- **Super lookup starts at `method.holder.superclass`**, never at
  `klass_of(receiver).superclass` — the classic bug that only appears with
  hierarchies ≥ 3 deep when the receiver is an instance of a subclass of the
  method's holder.
- **`klass_of` must tag-check before dereferencing** — smis have no header.
- **Floored division:** Rust `/`/`%` truncate; Smalltalk `//`/`\\` floor.
  Use `i64::div_euclid`/`rem_euclid` — note Smalltalk `\\` takes the sign of
  the *divisor*: `-7 \\ 2 = 1`, `7 \\ -2 = -1`. `div_euclid` is NOT floored
  for negative divisors (`7 // -2` must be `-4`, `div_euclid` gives `-3`);
  implement floored explicitly: `q = a.wrapping_div(b); if (a % b != 0) &&
  ((a < 0) != (b < 0)) { q -= 1 }`.
- **`bitShift:` overflow:** left shifts must fail (not wrap) when the result
  leaves the 62-bit smi range; check via `leading_zeros` on the absolute
  value before shifting.
- **Counter saturation, not wrap** (S10 trigger correctness).
- **DNU must not recurse**: the missing-`doesNotUnderstand:` fallback prints
  and terminates; it may not send anything.

## Interfaces for later sprints

- `lookup(vm, klass, selector)` — S4 (cannotReturn:/mustBeBoolean sends),
  S5 (compiler installing + resolving), S11 (megamorphic stub calls it).
- `install_method` — S5 world loading; its flush discipline is the model for
  S13's dependency invalidation.
- `ic_state` / `InterpreterIc` iteration — S14 reads type feedback from
  exactly these slots (SPEC §8.4); keep them `pub`.
- IC `target` slot semantics: `MethodOop` **or smi nmethod-id** — S10 makes
  the smi case live (`activate` matches on tag; leave a `todo!` arm now).
- `bump_invocation` + `counters` accessors — S10's trigger reads them.
- `gc_epilogue` — S7/S8 flush hook.
- `PrimDesc.can_allocate` — S7 uses it to audit handle discipline.
- `vm.out` — S5's golden-transcript runner writes through it.

## Out of scope

- Loop-counter share in `counters` and any compile trigger → S10.
- Blocks: `push_closure`, ctx temps, `value` primitives, `ensure:`,
  `mustBeBoolean`, `cannotReturn:` → S4 (the selectors are interned now; the
  sends are wired there).
- Double group, LargeInteger/Double overflow *fallback code* (the primitives
  Fail correctly now; the Smalltalk fallback methods arrive with the S6
  library), Character, `Symbol intern` primitive → S6.
- `sourceCompile:`, `gcScavenge`/`gcFull` primitives → S5 / S7 / S8.
- Negative lookup caching, method removal from dictionaries, IC flush op,
  weak symbol table → not in v1 (S22 for weakness).
- nmethod ids in IC targets → S10.

---

## Appendix — primitive set (pinned ids and exact semantics)

All primitives validate receiver/arg *tags and formats* themselves; any
violation → `Fail` (never a Rust panic). `args[0]` = receiver. "smi range" =
62-bit signed (SPEC §2.1).

**smi group** — both operands must be smis, else Fail. Results out of smi
range → Fail (SmallInteger fallback code will build LargeIntegers in S6).

| id | selector | argc | semantics | fail conditions |
|---|---|---|---|---|
| 1 | `+` | 1 | checked add | non-smi arg; overflow |
| 2 | `-` | 1 | checked sub | non-smi; overflow |
| 3 | `*` | 1 | checked mul | non-smi; overflow |
| 4 | `//` | 1 | **floored** quotient | non-smi; divisor = 0 |
| 5 | `\\` | 1 | floored remainder (sign of divisor) | non-smi; divisor = 0 |
| 6 | `bitAnd:` | 1 | bitwise and | non-smi |
| 7 | `bitOr:` | 1 | bitwise or | non-smi |
| 8 | `bitXor:` | 1 | bitwise xor | non-smi |
| 9 | `bitShift:` | 1 | count>0 left, count<0 arithmetic right | non-smi; \|count\| ≥ 62; left-shift overflow |
| 10–13 | `<` `<=` `>` `>=` | 1 | comparison → true/false oop | non-smi arg |
| 14 | `=` | 1 | equality → true/false | non-smi arg (fallback compares Double etc.) |
| 15 | `~=` | 1 | inequality | non-smi arg |

**oops group**

| id | selector | argc | semantics | fail conditions |
|---|---|---|---|---|
| 20 | `identityHash` | 0 | heap: lazily-assigned mark hash as smi; smi receiver: itself | never |
| 21 | `class` | 0 | `klass_of(receiver)` | never |
| 22 | `==` | 1 | raw oop bit equality → true/false | never |
| 23 | `basicNew` | 0 | receiver is a klass: `Slots` → all-nil instance; indexable formats → size-0 instance | receiver not a klass; format ∈ {Method, Klass, Double, Closure, Context, Process} |
| 24 | `basicNew:` | 1 | indexable instance of size n (oops→nil-filled, bytes→zeroed) | receiver not a klass; format not indexable; n not smi or n < 0 |
| 25 | `instVarAt:` | 1 | 1-based named-slot read | receiver smi; index not smi; index < 1 or > named-slot count |
| 26 | `at:` | 1 | 1-based indexable-oops element read | receiver format ≠ IndexableOops; bad index |
| 27 | `at:put:` | 2 | element write; returns the value | as 26 |
| 28 | `size` | 0 | indexable element count as smi | receiver not indexable (either format) |

**bytes group** — receiver format must be `IndexableBytes`.

| id | selector | argc | semantics | fail conditions |
|---|---|---|---|---|
| 40 | `byteAt:` | 1 | 1-based byte read → smi 0..255 | format; bad index |
| 41 | `byteAt:put:` | 2 | byte write; value smi 0..255; returns value | format; bad index; value range; **receiver is a Symbol** (immutable) |
| 42 | `byteSize` | 0 | byte count as smi | format |
| 43 | `replaceFrom:to:with:` | 3 | copy `arg3[1 .. to-from+1]` into `receiver[from..to]`; overlap-safe (`copy_within` when same object); `to < from` → no-op Ok | formats; bounds on either side; non-smi indices; receiver Symbol |
| 44 | `hashBytes` | 0 | FNV-1a 64 over bytes, `& 0x3FFF_FFFF`, as smi | format |
| 45 | `compare:` | 1 | lexicographic byte compare → smi -1/0/1 (prefix rule: shorter < longer) | arg format |

**system group (dev hooks)**

| id | selector | argc | semantics | fail conditions |
|---|---|---|---|---|
| 90 | `quit` | 0 | set `vm.exit_requested`; dispatch loop exits after this activation returns | never |
| 91 | `printOnStdout:` | 1 | write `args[1]` bytes to `vm.out`; returns receiver | arg not IndexableBytes |
| 92 | `millisecondClock` | 0 | millis since VM start as smi | never |

Reserved: 50–54 block `value` family, 60–61 `ensure:`/`ifCurtailed:` (S4);
70+ Double group (S6); 93+ `gcScavenge`/`gcFull`/`sourceCompile:` (S7/S8/S5).
