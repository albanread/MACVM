# MACVM World Design — the Smalltalk Library

The design of MACVM's class library ("the world"), grounded in a file-level
survey of the real Strongtalk source (`../strongtalk-repo/StrongtalkSource/`,
1,035 `.dlt` + 85 `.str` files ≈ 121.6 KLOC). Companion doc:
[`APPS.md`](APPS.md) (mirrors, tools, live pages). Phasing: `SPRINTS.md`
Phase W. The S6 core world (`sprints/sprint_s06_detail.md`) is the seed this
design grows from; nothing here changes S6.

Citations are bare filenames in `StrongtalkSource/`.

---

## 1. Adoption strategy — three layers

Strongtalk's 1,120 source files decompose cleanly (by `group:` pragma):
base 196, ast/type-system 319, reflection 78, ui 84 + HTML 35, outliners 49,
benchmarks 84, tests 77, SUnit 10, Aliens ~24, ObjC/OSX 23, protocols (.str)
85, misc ~110.

| Layer | ~Count | Policy |
|---|---|---|
| **A. Port nearly as-is** | 45–55 classes | Strip type annotations, flatten `\|>` mixins into superclasses, keep method bodies. Kernel, numbers, collections, Character, exceptions (later), test oracles. |
| **B. Reimplement fresh** | 25–35 classes' worth | Where MACVM's design differs or Strongtalk over-engineered: streams, Block classes (10 arity classes → 1 `BlockClosure`), Behavior/Class/Metaclass, SUnit-lite, Date/Time/Random/Point. Consult originals for protocol names and bodies worth stealing. |
| **C. Skip** | ~880+ files | The entire `ast` type system (319), `.str` protocols (85), mixin machinery, Win32/platform (~35), UI/Visual stack (replaced by WKWebView — APPS.md), VMMirror specifics, process family (deferred), image-model files (`Smalltalk.dlt` bootstrap, `Bootstrap.dlt`, `VM.dlt`). |

The runnable core MACVM actually wants is **~80–150 files' worth of
function** — almost exactly `base` minus platform/process/mixin plumbing —
and it aligns with the planned S6 world.

## 2. Adopt verbatim — Strongtalk's real gifts

1. **Numeric double-dispatch (`xFromY:`)** — no generality numbers, no
   `retry:coercing:`. Pattern (SmallInteger.dlt:220-231): primitive attempt →
   on `#FirstArgumentHasWrongType` failure → reversed send
   (`a addFromSmallInteger: self`); every Number subclass implements the
   `addFrom*/subtractFrom*/multiplyFrom*/…/lessFrom*` family (Number.dlt has
   Float-coercing defaults). This matches MACVM's smi-primitive-fails design
   exactly and is IC/inliner-friendly. **Adopt the whole scheme.**
2. **Deque-based OrderedCollection** — `AddableSequenceableCollection.dlt`:
   one contents-array + front gap + back gap engine shared by
   OrderedCollection and SortedCollection. Adopt the shape (un-abstracted
   into OrderedCollection is fine for v1; split when SortedCollection lands).
3. **One open-addressed hash engine** — `HashedCollection.dlt` (tombstone =
   the table itself) backing Dictionary/IdentityDictionary/Set/IdentitySet/
   KeyedSet. Adopt: single engine, concrete subclasses, no generics.
4. **Character flyweight + classification tables** — `Character.dlt` +
   `AsciiCharacters.dlt` (lookup-table isLetter etc.). Matches the S6 plan;
   port the tables.
5. **`species`** (Collection.dlt) — Blue-Book species for copying/select
   results. Keep.
6. **Test oracles** — `LargeIntegerTest.dlt`, `StringTest.dlt`,
   `DictionaryTest.dlt`, `ArraySortTest.dlt`, `SymbolTest.dlt`, the
   `Exception*Test` family, and class-side `test` methods. Port assertions
   into `world/tests/` as each area lands.

## 3. Collections

Target hierarchy (Strongtalk's, minus generics/mixins/type-system artifacts):

```
Collection                                    (abstract on #do:)
├─ SequenceableCollection
│  ├─ Interval                                (over Number, not just ints)
│  ├─ OrderedCollection                       (deque engine, §2.2)
│  │   └─ SortedCollection                    (sortBlock; later)
│  ├─ Array, ByteArray                        (indexable formats)
│  └─ String                                  (see §6)
├─ HashedCollection                           (engine, §2.3)
│  ├─ Dictionary → IdentityDictionary
│  └─ Set → IdentitySet
├─ Bag                                        (Dictionary of counts; later)
└─ (LinkedList, Queue, SharedQueue — with processes, deferred)
```

Skip: `AbsoluteArray` (type-system/compiler artifact), `KeyedSet`/`AssocSet`
(until the reflective tools want them), `WeakSet`/`WeakArray` (needs weak
refs — stretch S22), the lazy `Virtual*` collections. Protocol groups to
cover per class: accessing, iterating (`do:`, `keysAndValuesDo:`,
`inject:into:`, `detect:ifNone:`, `collect:`, `select:`, `reject:`,
`with:do:`), testing, copying (`copyFrom:to:`, `copyReplaceAll:with:`),
converting (`asArray/asOrderedCollection/asSet/asSortedCollection:`),
adding/removing, streaming (`readStream`/`writeStream`).

## 4. Streams

**Finding:** Strongtalk has *no* Blue-Book ReadStream/WriteStream classes —
those names are protocols (`.str` types). Its class lattice is 9+ classes
with streams inheriting from Collection and a mixin layer (`CharacterInput`).

**MACVM keeps its simple S6 design** (concrete `WriteStream` on a growable
String; later `ReadStream`) and adopts exactly one idea: the
**`actualNext` + peek-buffer factoring** (BasicInputStream.dlt:
`havePeeked`/`peekVal`; subclasses implement only `actualNext`/`actualAtEnd`)
when ReadStream arrives. Do **not** adopt streams-as-Collections or the
read-write conflation. External/file streams: deferred with files (§9).

## 5. Numbers

```
Magnitude → Number → Float(=Double in MACVM)          Float.dlt
                   → RationalNumber → Fraction         Fraction.dlt
                                    → Integer → SmallInteger, LargeInteger
```

- **Fraction is cheap and worth having** (auto-reduced, denominator > 0) —
  add in the first library wave after S6; it exercises the double-dispatch
  tower nicely.
- **LargeInteger**: S6 does digit algorithms in Smalltalk (ByteArray digits).
  Strongtalk instead uses ~14 VM primitives
  (`primitiveIndexedByteLargeIntegerAdd/…/ToStringBase/FromSmallInteger`,
  LargeInteger.dlt) — that list is the **ready-made upgrade path** if bignum
  performance ever matters. One signed LargeInteger (Strongtalk) vs. S6's
  Large± pair: keep S6's pair for v1 (matches the planned fallback code);
  revisit at converter time.
- No ScaledDecimal (Strongtalk has none either). `ABCFLOAT` is a Win32 GDI
  struct — ignore.

## 6. Strings & characters — the one deliberate divergence

Strongtalk strings are **double-byte (UCS-2)** (`UncompressedReadString` =
IndexedDoubleByteInstanceVariables; Symbols byte-compressed;
`ReadString < Magnitude` with case-sensitive `<`). MACVM strings are **UTF-8
bytes** (SPEC §1.3). Consequences:

- String method bodies that index (`at:` = i-th Character, 16-bit storage)
  must be **adapted, not copied**, at porting time. Rule for the converter
  (§10): flag any ported method that sends `at:`/`at:put:`/`size` to a
  String for manual review.
- Adopt: ReadString's protocol surface (comparison operators, `@<`-style
  case-insensitive ops if wanted later), Character flyweight/classification.
- MACVM v1 stays byte-indexed (documented limitation, SPEC §1.3); a proper
  Character/codepoint story is a library-level project after the tools work.

## 7. Exceptions — committed stretch design (was: vague)

**Survey finding: Strongtalk's ANSI exception layer is 100% in-image** —
blocks + NLR + `ensure:` + one per-process `handlerChain` slot. Mechanism:
`on:do:` = `LinkedExceptionHandler … return: [:v | ^v]` (the NLR block *is*
the return path, BlockWithoutArguments.dlt:206-212); handler chain hangs on
the process (Process.dlt:117-144); `signal` walks it (Exception.dlt:218-227);
resumption via a self-reinstalling NLR trampoline (`installContextAndDo:`,
Exception.dlt:177-185).

**MACVM adoption plan (Phase W5):**
- Prereqs: S4 (`ensure:`/`ifCurtailed:`, NLR, `cannotReturn:`) — nothing else.
  **Zero new VM features.**
- Port ~10 classes: Exception, Error, Warning, Notification, ZeroDivide,
  Halt, MessageNotUnderstood, ExceptionSet, LinkedExceptionHandler,
  BlockExceptionHandler (+ descriptors), with their ready-made tests
  (ExceptionTest.dlt, ZeroDivideTest.dlt, …).
- Single-process v1: `handlerChain` lives in **one global**
  (`Smalltalk handlerChain`) instead of `Processor activeProcess` — the only
  concurrency dependence. Moves to Process when W-processes land.
- Wire-in: DNU raises MessageNotUnderstood; `error:` raises Error; SUnit
  converges (§8).
- ⚠️ **Caveat for the compiled tier**: the NLR trampoline is a deopt/inlining
  stress test — signal-through-optimized-frames exercises S13 hard. Add the
  exception tests to the S13+ stress gates when W5 lands.

## 8. SUnit — lite now, 3.1 later, same surface

Strongtalk ships real Camp Smalltalk **SUnit 3.1** (TestCase.dlt 368 ln,
TestResult, TestSuite, TestResource + TextTestRunner headless runner; the
whole framework ≈1,100 lines). Its failure path is exception-based
(`signalFailure:` → signal, caught in `TestResult>>runCase:`; teardown via
`sunitEnsure:`), and `runAll`/`debugAsFailure` fork processes.

**Binding rule for S6's SUnit-lite: keep the selector surface identical** —
`assert:` (boolean or block), `deny:`, `assert:description:`, `setUp`,
`tearDown`, `test*` discovery, `TestResult` with passed/failures/errors —
recording failures instead of unwinding. Defer `should:raise:`, resumable
failures, TestResource, forked runners. Then at W5 (+exceptions) the real
SUnit 3.1 drops in over the same test corpus, using the `sunit*` portability
shims (SUnitNameResolver etc.) exactly as upstream intended.

## 9. Deferred surfaces (design known, scheduled later)

- **Processes** (SPEC §11): mechanism = 5 VM primitives (create/yield/
  terminate/status/transfer), policy in-image (Semaphore is a plain object
  over suspend/resume — Semaphore.dlt:36-48). Only 55/1,035 files touch
  processes; the batch path (fileIn, tests, benchmarks) is process-free.
- **Files**: Strongtalk does file I/O **via FFI DLL calls, not dedicated
  primitives** (`{{<libc ExternalProxy read/write/…>}}`,
  UnixFileDescriptor.dlt:75-149). MACVM's minimal tool surface for now:
  Transcript write, `millisecondClockValue` (both in SPEC §10). When
  in-image files matter: ~10 primitives or the Alien route (APPS.md §FFI).
- **Benchmarks** (feeds S15): Richards = 13 classes/1.0 KLOC
  (RichardsBenchmarks.dlt + RB* support), DeltaBlue = 12 classes/1.5 KLOC
  (Planner.dlt etc.), Stanford micro-suite ≈29 benchmarks with a
  deterministic seeded LCG (`seed*1309+13849 bitAnd: 65535`, seed 74755 —
  copy for reproducibility), Slopstone/Smopstone. Driver protocol:
  `run`/`name`/`factor` + a harness (`BenchmarkRunner.dlt`) whose `evaluate:`
  hook wraps runs in instrumentation — mirror this shape in `world/bench/`.
  All pure Smalltalk, no FFI/process/exception dependencies.

## 10. The `.dlt → .mst` converter (new tool, `tools/dlt2mst`)

A small Rust (or throwaway) converter makes layer-A porting mechanical.
Input format (fully documented by survey):

- **Bang-chunk format**: chunks separated by `!`; embedded `!` doubles;
  quotes double inside strings. Per class: a `Delta define: #Name as: (…)`
  header, `(Delta mirrorFor: #X) revision:/group:/comment:` metadata chunks,
  then repeated `! (Delta mirrorFor: #X) methodsFor: 'cat' !` +
  bang-terminated method chunks, category closed by `! !`.
- **Six declaration forms** to parse: `Class subclassOf:instanceVariables:`
  [`classVariables:`] [`abstract`] [`protocols:`];
  `Generic forAll: '…' body: (…)`; mixin application `A |> B` /
  `X mixin |> Y` in superclass strings (→ **flatten** into the superclass);
  `Mixin superclassType:body:`; `Protocol superProtocol:` (skip);
  `Delta let: #X be: (…)` type alias (skip);
  `Delta declareGlobal: #X type: '…'` (→ world global).
- **Annotation stripping** (token rules, with real examples in the survey):
  `<…>` after parameter names / after `^` / after temp names in `| |`
  (handles nested `<[E,E,^Boolean]>`, unions `<A|B>`, generics `<Dict[K,E]>`);
  `{where … }` clauses after signatures; `guaranteed <T> expr` → `expr`;
  generic instantiation `Array[EX]` → `Array`; `{{…}}` inline-primitive
  syntax → rewrite to MACVM `<primitive: N>` + fallback body (the inline
  `ifFail:` block *is* the fallback code — hoist it).
- **Load order for free**: `base.gr` (240 file-ins) and `world.gr` (1,114)
  are dependency-ordered manifests — the converter emits `world.list`
  entries in `.gr` order.
- Output: one `.mst` per class in MACVM's brace syntax, with a
  `"group: base"` comment tag retained (§11).

Converter effort is bounded (a chunk splitter + 6 forms + token filters);
build it at W1, iterate as porting reveals cases. Methods flagged §6 (String
indexing) and anything sending to `Processor`/`Platform` are emitted with a
`"REVIEW:"` comment rather than silently converted.

## 11. World organization (adopted lessons)

- **One class per file** scales to 1,100+ files — keep it.
- **Keep a per-class group tag** (comment convention in `.mst`) so manifests
  can be *regenerated from the world* rather than hand-maintained —
  Strongtalk's `fileOutWorldToFile:` round-trip (Smalltalk.dlt) is the model.
- **Two-pass loading** (declare-all-then-compile) is what Strongtalk's
  format enables and its fileIn does (classes → then methods, with the
  declarative `define:` header separate from method chunks). MACVM's loader
  should adopt this at W1: pass 1 executes all class-definition headers in
  world.list order; pass 2 compiles method bodies. This eliminates most
  forward-reference ordering pain in `world.list`.
- **No image needed**: Strongtalk *had* to boot a prebuilt `.bst` because
  fileIn required a live compiler+mirrors. MACVM's Rust-side compiler boots
  from bare source — keep that advantage; snapshot (S16) stays optional.
  **Still true** with [`IMAGE.md`](IMAGE.md)'s SQLite container in the
  picture: that's a versioned *source* database (plus an optional,
  always-re-derivable bytecode cache) for the GUI browser's interactive
  edits to land in — not a heap/memory image, and not a replacement for
  `.mst` files as the git-tracked, hand-authored seed library. The two
  coexist: `.mst` stays the checked-in source of truth; `image_store`
  imports it once and is where the browser's own edits actually persist.

## 12.1 The image store — a parallel, no-core-dependency track

Like Phase G (`gui/PLAN.md`), building `image_store` and importing the
existing `world/*.mst` files into it needs nothing from the core sprints
still in flight — the `.mst` text already exists, checked in, regardless of
which core sprint is currently green. Full design: [`IMAGE.md`](IMAGE.md).

## 12. Phasing — Phase W (see SPRINTS.md)

| Wave | Contents | Needs |
|---|---|---|
| W1 | `dlt2mst` converter; library wave 1: full collections protocol, Fraction, Character tables, ReadStream, test-oracle port; two-pass loader | S6 |
| W2 | Mirrors + reflection primitives + HtmlWriter (APPS.md) | S6 (+A16) |
| W3 | Tools wave 1: Workspace/eval, Inspector, find tools | W2, G2 |
| W4 | Tools wave 2: outliner suite + accept path | W3, G3 |
| W5 | ANSI exceptions + SUnit 3.1 convergence | S4 (solid), W1 |
| W6 | Benchmark harvest (Richards/DeltaBlue/Stanford) | W1 (feeds S15) |

W-waves interleave with core sprints; W1 can start the moment S6 is green.
