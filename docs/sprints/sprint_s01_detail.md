# Sprint S1 — Heap arena, allocation, genesis

Objective: allocate real heap objects. Reserve the address space, implement
the eden bump allocator (no GC — controlled abort when full), implement the
object formats and instance creation, klass objects, `Universe::genesis()`
(the metaclass knot), the interning symbol table, lazy identity hash, and a
debug object printer. After this sprint `nil.klass.name == #UndefinedObject`
holds in a running process.

SPEC sections implemented: §2.2 (headers in memory), §2.3 (formats), §2.4
(klass layout), §2.5 (format-checked wrappers), §3.1 (Universe, symbol
table), §3.2 step 1 (genesis), §7.1 (reservation, S1 subset), §7.2
(allocation fast path only).

## Prerequisites

From S0: `src/oops/` with `Oop`, `SmallInt`, `Mark`, `layout.rs`, tag-level
typed wrappers; `justfile`; crate-wide `#![deny(unsafe_code)]` with
`oops` opted back in. Nothing else exists: `src/memory.rs` and
`src/runtime.rs` are one-line stubs and are converted to directories now
(CONVENTIONS §1).

## Deliverables

- `src/memory/` (replaces `src/memory.rs`) `[unsafe OK — add
  #![allow(unsafe_code)]]`:
  - `reservation.rs` — mmap reserve/commit (via a new `libc = "0.2"`
    dependency in Cargo.toml).
  - `space.rs` — `Eden` bump space.
  - `alloc.rs` — the one allocation choke point (SPEC §7.2).
  - `universe.rs` — `Universe`, well-known oops, `genesis()`.
  - `symbols.rs` — interning symbol table.
  - `verify.rs` — heap walker used by tests (CONVENTIONS §4).
- `src/oops/` additions: `heap.rs` (raw header/body access — the ONLY place
  that dereferences heap memory), `klass.rs` (`Format`, klass field
  accessors), format-checking upgrades to `wrappers.rs`, `print.rs`
  (`print_oop`), klass/method/etc. layout constants appended to `layout.rs`.
- `src/runtime/` (replaces `src/runtime.rs`): `vm_state.rs` — minimal
  `VmState` + `VmOptions` (parses `MACVM_HEAP`, `MACVM_TRACE`; other flags
  parsed but inert).
- `tests/it_memory.rs` integration tests.

## Design

### Data structures

**Layout constants** appended to `src/oops/layout.rs` (single source of
truth). All `*_INDEX` constants are body-word indexes (word 0 = byte offset
`BODY_OFFSET`); `byte offset = BODY_OFFSET + 8*index`.

```rust
// Klass body — SPEC §2.4, in declaration order.
pub const KLASS_FORMAT_INDEX: usize = 0;          // smi: format enum + flags
pub const KLASS_NON_INDEXABLE_SIZE_INDEX: usize = 1; // smi: words incl. header
pub const KLASS_SUPERCLASS_INDEX: usize = 2;      // klassOop | nil
pub const KLASS_METHODS_INDEX: usize = 3;         // MethodDictionary | nil (nil until S3)
pub const KLASS_NAME_INDEX: usize = 4;            // Symbol | nil (during genesis)
pub const KLASS_INST_VAR_NAMES_INDEX: usize = 5;  // Array | nil
pub const KLASS_CLASS_VARS_INDEX: usize = 6;      // Array | nil
pub const KLASS_MIXIN_INDEX: usize = 7;           // always nil in v1 (SPEC §2.4 Δ)
pub const KLASS_BODY_WORDS: usize = 8;
pub const KLASS_SIZE_WORDS: usize = HEADER_WORDS + KLASS_BODY_WORDS; // 10

// Format smi encoding: bits 7:0 = Format discriminant,
// bit 8 = has_untagged_contents (SPEC §2.4 "format enum + flags").
pub const FORMAT_KIND_MASK: i64 = 0xFF;
pub const FORMAT_UNTAGGED_BIT: i64 = 1 << 8;

// Indexable body: [size: smi][elements…] appended after the named part.
// The size slot's body index = klass.non_indexable_size - HEADER_WORDS.
```

```rust
// src/oops/klass.rs
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Format {            // SPEC §2.3, discriminants pinned
    Slots = 0, IndexableOops = 1, IndexableBytes = 2, Method = 3,
    Klass = 4, Double = 5, Closure = 6, Context = 7, Process = 8,
}

impl KlassOop {
    pub fn format(self) -> Format;
    pub fn has_untagged_contents(self) -> bool;
    pub fn non_indexable_size(self) -> usize;       // words incl. header
    pub fn superclass(self) -> Oop;                 // KlassOop or nil
    pub fn name(self) -> Oop;                       // SymbolOop or nil
    pub fn methods(self) -> Oop;
    // setters mirror getters; genesis + S3 use them
    pub fn set_name(self, u: &Universe, s: SymbolOop);
    // ...
}
```

**`non_indexable_size` semantics (pinned here; SPEC §2.3 states it only for
`Slots`):** for every format, `non_indexable_size` = header words + named
fields, i.e. the fixed prefix. Total instance size in words:

| Format | total words |
|---|---|
| Slots, Klass | `nis` |
| IndexableOops | `nis + 1 + nelems` |
| IndexableBytes, Method | `nis + 1 + ceil(nbytes/8)` |
| Double | `nis` = 3 (header + one raw f64 word) |
| Closure | `nis + 1 + ncopied` (nis = 4: method, home) |
| Context | `nis + 1 + nslots` (nis = 3: home hint) |

> **SPEC-QUESTION:** SPEC §2.3 defines the instance-size rule only for
> `Slots` objects; the table above pins the obvious extension for the other
> formats. If a different convention was intended (e.g. `non_indexable_size`
> excluding the header for indexables), SPEC §2.3 should say so.

```rust
// src/oops/heap.rs — the ONLY module that dereferences heap addresses.
impl MemOop {
    pub fn mark(self) -> Mark;
    pub fn set_mark(self, m: Mark);
    pub fn klass(self) -> KlassOop;                 // header word 1
    pub fn set_klass(self, k: KlassOop);
    pub fn body_oop(self, index: usize) -> Oop;     // named-part word
    pub fn set_body_oop(self, index: usize, v: Oop);
    pub fn body_word_raw(self, index: usize) -> u64;   // Double payload etc.
    pub fn set_body_word_raw(self, index: usize, w: u64);
}
```
Internally each is a `read_volatile`-free plain `ptr::read`/`ptr::write` on
`(self.addr() + BODY_OFFSET + 8*index) as *mut u64`, in `unsafe` blocks with
`debug_assert!` bounds checks against the object's computed size. All return
values are **by-value copies** — no function anywhere returns `&Oop`,
`&mut Oop`, `&[Oop]`, or `&[u8]` pointing into the heap (see Pitfalls; this
is the S7-moving-GC accessor discipline, designed now).

Typed-wrapper upgrades: `ArrayOop::try_from(o)` now = mem tag ∧
`o.klass().format() == Format::IndexableOops`; `ByteArrayOop`/`SymbolOop` →
`IndexableBytes` (Symbol additionally `klass == universe.symbol_klass`);
`MethodOop` → `Method`; `KlassOop` → `Klass`; `DoubleOop` → `Double`;
`ClosureOop` → `Closure`; `ContextOop` → `Context`. Indexable accessors:

```rust
impl ArrayOop  { pub fn len(self) -> usize;  pub fn at(self, i: usize) -> Oop;
                 pub fn at_put(self, i: usize, v: Oop); }   // 0-based, debug bounds-checked
impl ByteArrayOop { pub fn len(self) -> usize; pub fn byte_at(self, i: usize) -> u8;
                 pub fn byte_at_put(self, i: usize, b: u8);
                 pub fn copy_bytes_out(self, buf: &mut Vec<u8>); }
impl DoubleOop { pub fn value(self) -> f64; }
impl SymbolOop { pub fn as_string(self) -> String; }        // copies out; debug/printing only
```

```rust
// src/memory/reservation.rs
pub struct Reservation { base: usize, len: usize }
impl Reservation {
    /// mmap(len, PROT_NONE, MAP_ANON|MAP_PRIVATE). SPEC §7.1: default 8 GiB
    /// *(tunable)*; overridden by MACVM_HEAP (MiB) for tests.
    pub fn reserve(len: usize) -> Reservation;
    /// mprotect(base+off, len, PROT_READ|PROT_WRITE), page-rounded.
    pub fn commit(&self, off: usize, len: usize);
}

// src/memory/space.rs
pub struct Eden { pub start: usize, pub top: usize, pub end: usize }

// src/memory/universe.rs
pub struct Universe {
    reservation: Reservation,
    pub eden: Eden,
    // well-known oops (SPEC §3.1) — generate with a macro like Strongtalk's
    // APPLY_TO_VM_OOPS so S7 can iterate them as GC roots:
    pub nil_obj: Oop, pub true_obj: Oop, pub false_obj: Oop,
    pub metaclass_klass: KlassOop, pub class_klass: KlassOop,
    pub object_klass: KlassOop, pub undefined_object_klass: KlassOop,
    pub boolean_klass: KlassOop, pub true_klass: KlassOop, pub false_klass: KlassOop,
    pub smi_klass: KlassOop, pub character_klass: KlassOop, pub double_klass: KlassOop,
    pub string_klass: KlassOop, pub symbol_klass: KlassOop,
    pub array_klass: KlassOop, pub bytearray_klass: KlassOop,
    pub association_klass: KlassOop, pub methoddict_klass: KlassOop,
    pub method_klass: KlassOop, pub closure_klass: KlassOop,
    pub context_klass: KlassOop, pub process_klass: KlassOop,
    pub smalltalk: Oop,               // nil until S5
    pub symbols: SymbolTable,
    hash_counter: u32,                // starts 1; identity-hash source
}

// src/memory/symbols.rs — SPEC §3.1: open-addressed hash set of Symbols.
pub struct SymbolTable { buckets: Vec<Oop> /* nil or SymbolOop */, count: usize }
impl SymbolTable {
    pub fn with_capacity(cap_pow2: usize) -> SymbolTable;   // default 1024
    fn content_hash(bytes: &[u8]) -> u64;                   // FNV-1a 64
}
impl Universe {
    /// Interning entry point. Probes by content; on miss allocates a Symbol
    /// (IndexableBytes, klass = symbol_klass) and inserts. Rehashes ×2 at
    /// 75 % load.
    pub fn intern(&mut self, bytes: &[u8]) -> SymbolOop;
    pub fn identity_hash(&mut self, o: Oop) -> u32;          // see Algorithms
}

// src/runtime/vm_state.rs
pub struct VmOptions { pub heap_mib: usize /* default 8192 */,
                       pub trace: TraceFlags /* MACVM_TRACE */ }
pub struct VmState { pub universe: Universe, pub options: VmOptions }
impl VmState { pub fn new() -> VmState; }   // parses env once (CONVENTIONS §3)
```

### Algorithms

**Allocation (SPEC §7.2, S1 subset).** One choke point:

```rust
// src/memory/alloc.rs — everything allocates through here, forever.
pub fn alloc_words(vm: &mut VmState, words: usize, klass: Oop, tagged: bool) -> MemOop
```
1. `debug_assert!(words >= HEADER_WORDS)`; `let size = words * WORD_SIZE`
   (already 8-aligned because it's whole words — byte allocators round up
   BEFORE calling).
2. `new_top = eden.top + size`; if `new_top > eden.end` → **S1 slow path =
   fatal**: print `"macvm: eden exhausted (N bytes requested)"` and
   `std::process::exit(70)`. (Not a Rust panic: heap exhaustion is an
   environment limit, not a VM bug — and S7 replaces this branch with the
   scavenger.)
3. `let addr = eden.top; eden.top = new_top;`
4. Write header: mark = `Mark::pristine().with_tagged_contents(tagged)`;
   klass word = `klass.raw()` (callers may pass a placeholder during genesis).
5. Zero/`nil`-fill the body: if `tagged`, fill body words with
   `vm.universe.nil_obj` (exception: genesis pre-nil, see below); else zero
   bytes. This keeps the heap parsable at every instant.
6. Return `MemOop::from_oop_unchecked(Oop::from_raw(addr as u64 + MEM_TAG))`.

`tagged` (the mark's `tagged_contents` bit, SPEC §2.2) is true for
`Slots`, `Klass`, `IndexableOops`, `Closure`, `Context` (their bodies are
all oops — a size slot is a valid smi oop); false for `IndexableBytes`,
`Method` (byte tail), `Double`, `Process`.

Typed creation API (all in `alloc.rs`, all `&mut VmState`):

```rust
pub fn alloc_slots(vm: &mut VmState, klass: KlassOop) -> MemOop;              // nis words
pub fn alloc_indexable_oops(vm: &mut VmState, klass: KlassOop, n: usize) -> ArrayOop;
pub fn alloc_indexable_bytes(vm: &mut VmState, klass: KlassOop, nbytes: usize) -> ByteArrayOop;
pub fn alloc_double(vm: &mut VmState, v: f64) -> DoubleOop;
pub fn alloc_klass(vm: &mut VmState, meta: Oop) -> KlassOop;                  // genesis + S5
```
Indexables write their size slot (`SmallInt::new(n)`) at body index
`nis - HEADER_WORDS`, then elements after it. `alloc_indexable_bytes` stores
the TRUE byte count in the size slot and zero-fills up to the padded word
boundary (`ceil(nbytes/8)` words) — padding bytes are zero forever, so
byte-wise hashing/compare may safely read whole words later.

**Identity hash (SPEC §2.2).** `identity_hash(o)`: if `o.is_smi()` return a
mix of the value (pinned: `(v as u32) ^ ((v >> 32) as u32)`, so equal smis
hash equal without a mark word). Else read `mark.hash()`; if non-zero return
it; otherwise assign: `hash_counter` starts at 1, and on each assignment
`hash_counter = hash_counter.wrapping_add(1); if hash_counter == 0 {
hash_counter = 1 }` — **0 is never handed out** (0 = unassigned, SPEC §2.2).
Write back with `set_mark(mark.with_hash(h))`.

**Symbol interning.** `intern(bytes)`: hash = FNV-1a over `bytes`; probe
`buckets[(hash + i) & (cap-1)]` linearly; a slot is a hit when the stored
Symbol's length and bytes equal `bytes` (byte compare via `byte_at` copies,
not slices). On empty slot: allocate the Symbol first, THEN insert — note the
allocation cannot disturb the table in S1 (no GC), but keep the order
allocate-then-insert anyway; it is the order that stays correct when
allocation can collect (S7). Rehash: allocate new `Vec` (Rust-side, not heap)
and re-insert by stored content hash.

**`Universe::genesis()` — the metaclass knot, full order.** This is the
trickiest part of the sprint. The circularities: (a) every object needs a
klass, but the first klasses don't exist yet; (b) fields want `nil`, but
`nil` needs the `UndefinedObject` klass; (c) klass names are Symbols, but
Symbols need `symbol_klass` and the table. Resolution: allocate with
placeholder fields and patch, in exactly this order. `PLACEHOLDER` below =
`SmallInt::new(0).oop()` — a valid oop that can never be mistaken for a heap
reference; every PLACEHOLDER write is paired with a numbered patch step, and
`verify_genesis` proves none survive.

Helper used from step 6 on (after the knot exists):

```rust
/// Allocates the metaclass (a Klass-format object, instances are the class ⇒
/// meta.format = Klass, meta.non_indexable_size = KLASS_SIZE_WORDS,
/// meta.klass = metaclass_klass), then the class (klass = the new meta).
/// Superclass wiring: class.superclass = superclass;
/// meta.superclass = superclass.klass() (= the superclass's metaclass),
/// except Object whose meta.superclass = class_klass.
/// name fields = PLACEHOLDER until step 8 (or interned immediately from step 9).
fn new_klass(u: &mut Universe, superclass: Oop, format: Format,
             untagged: bool, nis_words: usize) -> KlassOop
```

Numbered genesis steps:

1. **Heap up**: `Reservation::reserve(heap_mib << 20)`; commit eden
   (4 MiB *(tunable)*, SPEC §7.1) at offset 0; init `Eden`.
2. **`nil` shell**: `nil_obj = alloc_words(2, PLACEHOLDER, /*tagged*/true)`
   — a Slots object with zero named fields. Its klass is PLACEHOLDER
   (patch step 6). From here on `alloc_*` nil-fills bodies with this real
   `nil_obj`.
3. **Metaclass knot**:
   a. `metaclass_klass = alloc_klass(u, PLACEHOLDER)`; body: format =
      `Klass` smi, nis = `KLASS_SIZE_WORDS`, superclass = PLACEHOLDER
      (patch 5c), methods/name/ivnames/classvars/mixin = nil.
   b. `metaclass_meta = alloc_klass(u, metaclass_klass.oop())` — the object
      "Metaclass class"; same body shape; superclass = PLACEHOLDER (patch 5d).
   c. **Patch**: `metaclass_klass.set_klass(metaclass_meta)`. The knot is
      closed: `Metaclass class class == Metaclass`.
4. **Object and Class** (cannot use `new_klass` yet — their metas'
   superclasses cross-reference):
   a. `object_meta = alloc_klass(u, metaclass_klass)`; `object_klass =
      alloc_klass(u, object_meta)`; object.superclass = nil; object.format =
      `Slots`, nis = 2. object_meta.superclass = PLACEHOLDER (patch 4d).
   b. `class_meta = alloc_klass(u, metaclass_klass)`; `class_klass =
      alloc_klass(u, class_meta)`; class.superclass = object_klass
      (**Δ, pinned here**: v1 has no Behavior/ClassDescription layer —
      `Class superclass == Object`); class.format = `Klass`,
      nis = `KLASS_SIZE_WORDS`.
   c. class_meta.superclass = object_meta ("Class class" inherits from
      "Object class").
   d. **Patch**: object_meta.superclass = class_klass (the Smalltalk-80 rule:
      `Object class superclass == Class`).
5. **Close Metaclass's superclasses**:
   c. metaclass_klass.superclass = class_klass (**Δ**: Smalltalk-80 has
      `Metaclass superclass == ClassDescription`; v1 pins `Class`).
   d. metaclass_meta.superclass = class_meta.
6. **UndefinedObject + nil patch**: `undefined_object_klass =
   new_klass(u, object_klass, Slots, false, 2)`.
   **Patch**: `nil_obj.set_klass(undefined_object_klass)`. Also backfill
   every nil-valued field written before `nil_obj` existed — in this order
   nothing needs it (steps 3–5 wrote nil via the live `nil_obj`), but
   `verify_genesis` re-checks.
7. **String, Symbol klasses**: `string_klass = new_klass(u, object_klass,
   IndexableBytes, true /*untagged*/, 2)`; `symbol_klass =
   new_klass(u, string_klass.oop(), IndexableBytes, true, 2)`.
8. **Symbol table + name patch pass**: `symbols =
   SymbolTable::with_capacity(1024)`. Now intern and patch `name` on every
   klass so far — pinned names: `#Object #Class #Metaclass #UndefinedObject
   #String #Symbol`; each metaclass gets the interned symbol `'<Name>
   class'` (e.g. `#'Object class'`) since the klass layout has no
   back-pointer from metaclass to class (SPEC §2.4) and the printer needs a
   name. (Metaclass-name convention pinned here; SPEC is silent.)
9. **Remaining klasses** via `new_klass` with names interned immediately:

   | Universe field | name | superclass | format | untagged | nis |
   |---|---|---|---|---|---|
   | boolean_klass | Boolean | object | Slots | – | 2 |
   | true_klass | True | boolean | Slots | – | 2 |
   | false_klass | False | boolean | Slots | – | 2 |
   | smi_klass | SmallInteger | object | Slots | – | 2 (never instantiated on-heap) |
   | character_klass | Character | object | Slots | – | 3 (ivar: value) |
   | double_klass | Double | object | Double | yes | 3 |
   | array_klass | Array | object | IndexableOops | – | 2 |
   | bytearray_klass | ByteArray | object | IndexableBytes | yes | 2 |
   | association_klass | Association | object | Slots | – | 4 (key, value) |
   | methoddict_klass | MethodDictionary | object | IndexableOops | – | 2 |
   | method_klass | CompiledMethod | object | Method | yes | 9 (§4.4: 7 named) |
   | closure_klass | BlockClosure | object | Closure | – | 4 (method, home) |
   | context_klass | Context | object | Context | – | 3 (home hint) |
   | process_klass | Process | object | Process | yes | 2 |

   `inst_var_names` for Association = a 2-element Array of `#key #value`;
   Character = `#value`; all others nil (S5's class loader fills them).
10. **true/false**: `true_obj = alloc_slots(true_klass)`, `false_obj =
    alloc_slots(false_klass)`.
11. `smalltalk = nil` (populated in S5). `hash_counter = 1`.
12. **`verify_genesis()`** (in `memory/verify.rs`): walk eden object-by-object
    (headers make the heap parsable: size from klass format + size slots) and
    assert: every klass word has MEM_TAG and points to a `Format::Klass`
    object; no PLACEHOLDER (smi in an oop-only field) remains in any klass
    field except documented nils; the gate invariants below; every mark word
    has MARK_TAG + sentinel.

Gate invariants (SPRINTS S1) in terms of this structure:
`nil.klass().name() == intern("UndefinedObject")`;
`object_klass.klass().klass() == metaclass_klass` (`Object class class ==
Metaclass`); `metaclass_klass.klass().klass() == metaclass_klass`;
`object_klass.superclass() == nil`; `true_obj.klass() == true_klass`.

**Debug printer** (`src/oops/print.rs`):
`pub fn print_oop(u: &Universe, o: Oop) -> String` — smi → decimal; nil/
true/false by identity vs universe fields → `nil`/`true`/`false`; Symbol →
`#name`; String → `'contents'`; Double → shortest-roundtrip float; klass →
its name; Array → `#(e1 e2 …)` with recursion depth cap 3 and length cap 10
(`…` beyond); other mem oop → `a ClassName` / `an Array` (an before vowel,
match Smalltalk's convention loosely — pin: `a <name>` always, no vowel
logic, keeps goldens stable). Never allocates; never panics on malformed
input in release (prints `<bad oop 0x…>`).

### Layer boundaries

- `oops/heap.rs` is the only code with raw heap loads/stores; `memory/`
  computes addresses and sizes but reads/writes headers through `MemOop`
  accessors (exception: `alloc.rs` writing the fresh header may use one
  private unsafe write since the object doesn't exist yet).
- `memory/` may use `oops/`; `oops/` must NOT import `memory/` (printer takes
  `&Universe` — put `print_oop` in `oops/print.rs` with the `Universe`
  parameter typed via a small trait if a cycle appears; simplest: move
  `print_oop` to `memory/` if the import direction fights back. Pinned:
  `print_oop` may live in `memory/universe.rs` if needed; keep the name).
- `runtime/vm_state.rs` owns env parsing; `memory/` never reads env vars.
- No code outside `memory/alloc.rs` bumps `eden.top`.

## Implementation order

1. Add `libc` dependency; `reservation.rs` + unit test (reserve 64 MiB,
   commit 1 MiB, write/read a word, drop).
2. `space.rs` + `alloc_words` with a dummy klass word; `oops/heap.rs`
   accessors; test header round-trip through accessors.
3. `layout.rs` klass constants; `Format`; `klass.rs` accessors (against
   hand-built klass objects, no genesis yet).
4. Typed creation API (`alloc_slots` etc.) + wrapper `try_from` upgrades +
   indexable accessors; size-math unit tests.
5. `VmState`/`VmOptions` skeleton (env parsed; only `heap_mib` consumed).
6. `genesis()` steps 1–6; test the knot + nil invariants.
7. `symbols.rs`; genesis steps 7–8; interning tests.
8. Genesis steps 9–12 + `verify.rs`; full gate tests.
9. `print_oop`; printer unit tests; `tests/it_memory.rs`; `just gate-s01`
   (= `just ci` still).

## Pitfalls

- **Never hold Rust references into the heap.** Even though nothing moves in
  S1, S7's scavenger WILL move every object. The discipline is set NOW: all
  accessors take/return values (`Oop`, `u8`, `u64`, `String` copies); no
  `&[u8]` slices of Symbol bytes escape `oops/heap.rs`; comparisons loop over
  `byte_at`. When S7 lands, the only retrofit is `Handle`s around
  *oop-holding locals across allocation*, not a rewrite of every accessor
  signature. Code review rule: `unsafe` + lifetime `'a` on anything touching
  heap memory is an automatic reject.
- **Pre-biased offsets.** Strongtalk pre-biases field offsets by −Mem_Tag so
  compiled loads are `ldr xd, [xoop, #off-1]` (SPEC §2.1, analysis §3.1). In
  Rust accessors, do the equivalent once: `addr = raw - MEM_TAG` in
  `MemOop::addr()`, then plain `+ offset`. Do NOT also subtract the tag in
  offset constants — the constants in `layout.rs` are true (untagged)
  offsets; the bias lives in exactly one place (`addr()`), or S10's compiled
  loads will double-bias.
- **Placeholder klasses must be un-mistakable.** Using `Oop::from_raw(0)`
  (smi 0) as PLACEHOLDER means any traversal that treats it as a pointer
  fails fast in `KlassOop::try_from` (tag 00 ≠ 01) rather than reading wild
  memory. Never use a dangling MEM_TAG word as a placeholder.
- **`alloc` nil-fills, but nil doesn't exist for the first allocation.**
  Genesis step 2 allocates `nil_obj` itself: its body has zero fields, so
  nothing needs filling — but guard `alloc_words` against reading
  `u.nil_obj` before it's set (in genesis, pass an explicit fill value, or
  fill with PLACEHOLDER and rely on nil having no body). Pinned: `alloc_words`
  takes the fill value from `Universe.nil_obj` and genesis step 2 sets
  `nil_obj = PLACEHOLDER` *before* the first call, so the shell fills (zero
  words) trivially; step 2 then stores the real oop.
- **Metaclass `non_indexable_size` describes the CLASS object.** A
  metaclass's instances are class objects (10 words, format `Klass`);
  getting this wrong makes S5's `subclass:` allocate truncated klasses. The
  class's own `nis` describes ITS instances (e.g. Association = 4).
  Two different numbers on two adjacent objects — easy to swap.
- **`Class superclass` simplification is a Δ.** Smalltalk-80 inserts
  Behavior/ClassDescription between Object and Class, and
  `Metaclass superclass == ClassDescription`. v1 pins both to
  Object/Class (steps 4b, 5c). Record it in code comments; S6's library must
  not define Behavior expecting the VM chain to match.
- **Alignment is 8** (SPEC §2.1). `ceil(nbytes/8)` for byte objects; do not
  round to 16 (Self habit) — wastes eden and breaks size-math tests.
- **Odd-size byte objects**: the size slot stores the true byte length; the
  allocator zeroes the padding once; `byte_at` bounds-checks against the true
  length. Anyone reading padded words (S6 hash primitive) relies on
  padding-is-zero — never write the padding after allocation.
- **hash 0 / counter wrap.** The identity-hash counter skips 0 on wrap
  (Algorithms). Wrapping at 2^32 allocations of *hashed* objects is
  acceptable for a research VM (collisions legal, identity via `==` on oops).
- **`Double` body is NOT an oop.** `has_untagged_contents` + mark
  `tagged_contents = false`; the f64 bit pattern can look like any tag. Any
  heap walker (verify.rs, S7 GC) must dispatch on format BEFORE touching
  body words. `verify_genesis` is the first such walker — write it
  format-dispatched from day one.
- **Symbol table is a GC root set later.** Keep buckets as `Vec<Oop>`
  (S7 scans/updates it in place). Do not switch to
  `HashMap<Vec<u8>, Oop>` "temporarily" — duplicated key storage and a
  root-update headache.
- **`exit(70)` on eden exhaustion, not `panic!`** (CONVENTIONS §4: panics =
  VM bug). Tests that provoke exhaustion spawn a subprocess or use the
  documented tiny `MACVM_HEAP`; don't `#[should_panic]` on it.

## Interfaces for later sprints

```rust
// S2 (bytecode objects) and S3+ allocate through:
alloc_slots / alloc_indexable_oops / alloc_indexable_bytes / alloc_double
alloc_method(vm, nbytecode_bytes) -> MethodOop        // ADDED IN S2 in alloc.rs
Universe::intern(&mut self, &[u8]) -> SymbolOop
Universe::identity_hash(&mut self, Oop) -> u32
VmState::new(); vm.universe.<well_known>
print_oop(&Universe, Oop) -> String                    // disassembler, traces
memory::verify::verify_heap(&Universe)                 // test harness hook
KlassOop::{format, non_indexable_size, superclass, name, methods}
```
S3 will set `KLASS_METHODS_INDEX` fields (MethodDictionary objects, klass =
`methoddict_klass`). S7 replaces `alloc_words` step 2's abort with the
scavenge slow path and adds `Handle`; signatures above do not change (they
already thread `&mut VmState`).

## Out of scope

- Survivor spaces, old gen, card table, any GC — S7/S8 (`from`/`to`/old
  layout positions inside the reservation are S7's concern; S1 commits only
  eden at reservation offset 0).
- `Handle`/`HandleScope` — S7 (no collection can happen, so no oop moves).
- MethodDictionary population, lookup — S3.
- Character flyweight table (256 preallocated) — S6 library over the klass
  created here.
- `smalltalk` namespace population — S5 loader.
- Weak symbol table — S22.
- Process objects — S17 (klass allocated for format completeness only).
