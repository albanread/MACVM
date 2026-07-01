# Sprint S06 — Test Plan

## Acceptance gate

Restated from SPRINTS.md S6, made checkable:
1. `cargo test` (via `tests/it_world.rs`) boots the VM, loads
   `world/world.list` + `world/tests/tests.list`, runs SUnit-lite, and the
   process exits 0 with a report line matching `/^\d+ run, 0 failed$/`.
   Total assertions ≥ 200 (the report prints the assertion count; the Rust
   test asserts `run >=` the inventory total below).
2. `macvm run world/bench/fib.mst` prints `75025` (fib 25); the fib(30)
   variant prints `832040`; `macvm run world/bench/sieve.mst` prints
   `1899`.
3. Interpreter throughput measured per the procedure in
   sprint_s06_detail.md §Benchmarks and recorded in `docs/PERF.md`
   (SPEC §13 row 1 is tracked, not gated).
4. All S0–S5 tests still green. `just gate-s06` runs 1–3.

## Unit tests (Rust-side additions)

| test | module | assertion | rationale |
|---|---|---|---|
| genesis_skeleton | memory | every klass in the pinned tree exists, superclass links exact, formats per SPEC §2.3 | S6 reopens depend on it |
| genesis_metaclass_chain | memory | `Object class superclass == Class`, `Class superclass == Behavior` | class-side lookup path |
| instvar_layouts | memory | Character/Association/OC/Dictionary/Interval/WriteStream/TestResult/TestCase/Message instvar counts+names match the doc | .mst accessors index by these |
| prim_ids_frozen | runtime | registry table matches the pinned id map (one assert per id) | .mst `<primitive:>` bindings |
| prim_instvarat_klass | runtime | p45 on a klass oop reads the 8 named fields 1-based | Behavior accessors |
| prim_error_trace | runtime | p96 prints message + ≥1 frame line, terminates with nonzero | SPEC §6.3 |
| prim_double_print | runtime | p30 round-trips 0.1, 1e10, 1.5e-3, -0.0 shortest form | Double printOn: |
| trace_count_flag | interpreter | `MACVM_TRACE=count` prints a stable bytecode total for a fixed doIt | PERF procedure determinism |

## Integration / golden tests

- `tests/it_world.rs::boots_clean` — world.list alone (no tests) loads with
  zero output except nothing; then sends `startUp` successfully.
- `tests/it_world.rs::suite_green` — gate item 1.
- `tests/golden/point_demo.mst` re-golden: transcript now produced via
  `printString`/`<<` instead of primitive prints (updates the S5 golden —
  keep the S5 variant as `point_demo_prim.mst`).
- `bench/fib.mst`, `bench/sieve.mst` executed by `tests/it_bench_smoke.rs`
  with reduced sizes (fib 15 → `610`, sieve 1 iteration) so CI stays fast.

## In-language tests — inventory (world/tests/)

Each class lists its `runTest:do:` entries and approximate assertion count
(target total ≈ 215; the runner report must show ≥ 200).

**01 ObjectTests (14)** — `==`/`~~` on identical/distinct objects; `=`
default is identity; `hash == identityHash`; `isNil`/`notNil` on nil and
non-nil; `yourself`; `class` (3 cases incl. `3 class == SmallInteger`);
`isKindOf:`/`isMemberOf:` up the Integer chain; `->` builds Association.

**02 BooleanTests (16)** — truth tables for `and: or: & |` (8); `not` (2);
`ifTrue:` family answering nil on the untaken leg (4); non-literal blocks
via temp variable to force REAL sends and re-check `and:`/`ifTrue:` (2).

**03 IntegerTests (30)** — smi `+ - * // \\` incl. floored-division signs
(`-7 // 2 = -4`, `-7 \\ 2 = 1`) (8); comparisons + `max: min:
between:and:` (6); bit ops incl. `bitShift:` both directions (4);
`even odd gcd: timesRepeat:` (4); radix literals evaluate (`16rFF = 255`)
(2); printString of 0, 7, -7, minVal, maxVal (5); `hash = self` (1).

**04 LargeIntegerTests (28)** — `maxVal + 1` is LargePositiveInteger and
`> maxVal` (3); `minVal - 1`, `minVal negated`, `minVal abs` (3);
round-trip: `(maxVal + 1) - 1 == maxVal` demotes to smi (2); add/sub
mixed signs crossing digit boundaries (256^k ± 1 table) (6);
multiplication: `(2 raisedTo:)`-free — repeated doubling to 2^100, times
itself, compare against literal `16r…` 200-bit constant (4); comparison
matrix Large/Large, Large/smi both signs (6); printString of 2^64,
−(2^64), minVal (3); `=` and `hash` consistency across demotion (1).

**05 DoubleTests (16)** — arithmetic + literals (`3.14`, `1e10`) (4);
mixed smi/Double coercion both directions (`1 + 2.5`, `2.5 + 1`) (4);
`sqrt floor ceiling truncated abs negated` (5); comparisons incl.
`1 < 1.5` via fallback (2); printString round-trip `0.1` (1).

**06 CharacterTests (12)** — flyweight: `(Character value: 65) ==
(Character value: 65)` and `$A == (Character value: 65)` (2); `value
asInteger asCharacter` round-trip (2); `< =` and `hash` (3); classify
`isDigit isLetter isVowel` (3); `asUppercase asLowercase` (2).

**07 StringTests (22)** — literals with embedded quotes (1); `at:` answers
Character, `at:put:` with Character and smi (3); `size , copyFrom:to:`
(4); `= < > compare:` incl. prefix ordering `'ab' < 'abc'` (5);
`asSymbol == #foo` and `asString` round-trip (2); `hash` equal for equal
strings (1); `printString` escaping (`'it''s'` prints as `'it''''s'`…
i.e. quoted+doubled) and `displayString` unquoted (3); mutation of a
literal string then re-run shows the aliasing (documented behavior) (1);
`includesChar: indexOf:ifAbsent:` (2).

**08 SymbolTests (8)** — interning identity across files (1); `=` is
identity (`#foo = 'foo' copy` false, `'foo' = #foo` true — pinned
asymmetry) (3); `asString asSymbol` round-trip (2); immutability:
`at:put:` errors — SKIPPED (would kill the run; documented, waits for
S18 `should:raise:`) (0); printString plain and quoted forms (2).

**09 ArrayTests (16)** — literal arrays: nesting, bare symbols, negatives
(4); `at: at:put: size at:ifAbsent:` bounds behavior (4); `do: collect:
detect:ifNone: inject:into:` (4); `copyFrom:to: , = first last` (3);
`printOn:` `#(1 2 3)` form (1).

**10 ByteArrayTests (8)** — `#[1 2 255]` literal (1); `at: at:put:` range
(2); `copyFrom:to:` via p63 (1); `hash =` (2); collect:→Array (1);
printString (1).

**11 OrderedCollectionTests (16)** — add:/addLast:/addFirst: ordering
(3); growth past 8 and past front (2, checks a 100-element build);
`removeFirst removeLast` incl. empty-error SKIPPED (2); `at: size do:
detect:ifNone: includes: collect: asArray` (7); `first last` (1);
printString (1).

**12 DictionaryTests (14)** — `at:put: at:` basic (2); overwrite keeps
tally (2); `at:ifAbsent: at:ifAbsentPut:` (3); growth past load factor
with 50 smi keys then full readback (2); Symbol/String/Character keys
(3); `keysAndValuesDo:` visits tally pairs (1); `includesKey: size` (1).

**13 IntervalTests + ControlFlowTests (15)** — `(1 to: 5) size asArray
do:` (3); `to:by:` positive/negative step sizes (3); `to:do:` inlined vs
real send equality: sum 1..100 both ways = 5050 (2); `to:do:` result is
the receiver (1); loop-var capture bail-out: blocks collected in a loop
see distinct values (real-send path) (1); `whileTrue:`/`whileFalse:`
counting loops (2); `timesRepeat:` (1); nested-block NLR through
`to:do:` (re-covers S4 semantics from source) (2).

**14 StreamAndPrintTests (12)** — WriteStream on String: `nextPut:
nextPutAll: contents` (3); growth (1); `<<` with String/Character/
Integer (3); WriteStream on Array (2); `printString` of a compound
(`#(1 $a 'hi' #sym)` exact text) (2); Transcript `show:`/`cr` smoke
(output checked by the golden transcript) (1).

**15 BlockTests (10)** — closure capture counter shared between two
blocks (2); captured param (1); 3-deep context chain (2); block re-entry
after home return, non-NLR (1); `value:value:` arity behaviors (2);
`ensure:` runs on normal exit and on NLR (2). (Re-expresses S4 goldens
as assertions — the in-language suite becomes the regression net,
SPEC §12.3.)

Total ≈ 215 assertions. `tests/99_run_all.mst` lists all 15 classes;
P9 in the detail doc: per-class counts printed by the runner must match
this inventory (drift = update both).

## Stress / negative tests

- **Load-order torture**: `tests/it_world.rs::order_is_load_bearing` —
  programmatically load world.list with files 12 and 19 swapped and
  assert the loader fails with an "undeclared variable" CompileError
  (proves the ordering law is real and the loader fail-fast works).
- **Reopen misuse**: a fixture world file `Object subclass: Integer [...]`
  (wrong superclass) must fail with "cannot change shape / superclass
  mismatch".
- **error: path**: `tests/it_world.rs::error_kills_with_trace` — run a
  doIt `nil foo` (DNU → error:) and assert nonzero exit + selector name +
  ≥1 stack-trace line on stderr.
- **Deliberately failing assertion**: run a one-off test class whose
  assertion fails; assert exit 1 and the failure line names the test —
  proves the suite can actually fail (guards against a
  vacuously-green runner).
- **Large arithmetic fuzz (Rust)**: `tests/it_large_fuzz.rs` — 10 000
  random i128 pairs; compute `+ - * < =` in Rust i128 (in Large range);
  drive the VM via a generated `.mst` doIt per batch comparing
  printString output. Seeded, deterministic.
- **Dictionary probe wrap**: keys engineered (smi keys ≡ capacity−1 mod
  capacity) to force wraparound in `scanFor:`; readback asserted (lives
  in 12 DictionaryTests but called out as a required case).
- GC stress is **not** run here (no GC until S7) — but
  `MACVM_GC_STRESS=1` over this exact suite is S7's flagship gate;
  nothing in the suite may depend on allocation addresses or hash
  ordering (Dictionary iteration order is never asserted).

## Non-goals

- No coverage of: exceptions/`should:raise:` (S18), `perform:`/
  reflection (S16+), Large÷Large division (post-v1), removeKey: (S22-ish),
  Unicode strings (documented limitation), Fraction/ScaledDecimal
  (absent).
- No performance gating — PERF.md numbers are recorded only
  (SPRINTS standing rule 3); tier-1 speedups are measured against this
  baseline in S10/S14/S15.
- No snapshot/image tests (S16).
- Richards/DeltaBlue ports are S15, not here.
