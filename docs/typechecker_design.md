# MACVM Optional Types — design

Status: DESIGN (no code). Owner slice order: T0′ → T4 below; T5 explicitly
optional. Companion sources: the Strongtalk release at
`~/claudeprojects/strongtalk-repo` — this design treats its `StrongtalkSource/
Delta*.dlt` type system as the **executable specification**, not as code to
translate.

## 0. The thesis, inherited

Strongtalk's central architectural claim, verified against its sources
(2026-07-23 survey): static types exist **for programmers**; performance
comes from type feedback. The claim is structural there, and will be
structural here:

- Its checker is ~105 image-side files (codename "Delta", ~5.7k lines of
  type core) with **44 distinct static error classes**; per-method checking
  is incremental (`typecheckedSuccessfully ^ timeTypechecked >= timeSaved`).
- Its VM contains **zero** type-system code (grep-verified across `vm/`).
  The one false friend — `PrimInliner::tryTypeCheck()` (`primInliner.cpp:112`,
  "-Urs 11/95") — consumes *runtime feedback klasses*, not declarations.
- Annotations ride through parsing purely so the checker and browser can see
  them; codegen never looks. Code that fails checking still compiles and
  runs identically.

MACVM keeps every one of those properties. The checker is **advisory,
off the run path, and incapable by construction of changing execution**.

## 1. Decision record: Rust reimplementation, not a Smalltalk port

Considered: (A) port the Delta checker into `world/*.mst`; (B) reimplement
in Rust against our own AST. **Chosen: B.** Grounds, in order:

1. **Project precedent.** The browser find/view tools were Smalltalk and
   were rewritten Rust+SQLite; the Smalltalk `*View` classes are recorded
   dead code. Dev infrastructure survives here on the Rust side.
2. **No AST bridge.** Plan A required Rust to export a typed AST into the
   DB for a Smalltalk walker — a second representation with permanent
   schema-drift risk. Plan B consumes the frontend parser's AST in-process.
   One parser, one AST.
3. **Gating is structural** (§7): a subcommand plus DB rows cannot touch
   the run path, which is a stronger gate than any flag.
4. Tooling: `cargo test` over the subtype lattice; the error catalog
   becomes a Rust test suite; CI-able in the battery.

Cost acknowledged: we lose in-image liveness, and we re-derive a debugged
algorithm. Mitigations: the `.dlt` sources stay open as the spec; each
checking rule is ported **case-by-case with a test** named after the
Strongtalk error class it reproduces; the hairiest machinery (generics,
protocols, the coinductive trail in anger) is staged last (T5) and may
legitimately never be built.

## 2. Non-goals (v1)

- **The inference-clause engine** (`InferenceClause`, `DeltaInferenceSignature`,
  the `def` sugar): SKIP — standing roadmap decision. The `def` marker and
  inference-clause syntax are *parsed and stored* for fidelity, never acted
  on. Polymorphic sends that would need inference type as Dynamic.
- **Generics checking** (`Cltn[E]`, `DeltaGenericApplicationType`): parsed
  and stored from day one (the grammar accepts them — they appear all over
  Strongtalk-derived sources), **checked** only in T5, if ever.
- Protocols as authored entities, brands, mixin types: T5-or-never.
- Typed instance variables: parsed/stored in T0′ if present, checked T3.
- Any influence on codegen, the JIT, or the interpreter: **never** (§7).

## 3. Annotation surface (already in the language — D11)

The frontend parser has accepted-and-discarded Strongtalk-style annotations
since D11 (`src/frontend/parser.rs`, `skip_type_annotation`), in exactly
these positions:

| position | example |
|---|---|
| keyword-message arg | `add: n <Integer> [ … ]` |
| binary-message arg | `< other <Magnitude> [ … ]` |
| return type (before body) | `size ^<Integer> [ … ]` |
| method temps | `\| i <Integer> s <String> \|` |
| block args | `[:e <TypeError> \| … ]` |

**T0′ changes "skip" to "capture"**: the same grammar positions, but the
annotation's token text (and span) is kept on `MethodNode` and exported to
the image DB. Codegen continues to ignore the fields — enforced forever by
the differential gate (§7.2).

Type-expression grammar (v1 parse surface — superset of what v1 checks):

```
TypeExpr := Ident                      -- class or type name: Integer, Self
          | Ident '[' TypeList ']'     -- generic application: Cltn[E]      (stored, unchecked v1)
          | '[' TypeList? (',' '^' TypeExpr)? ']'   -- block type: [Integer, ^Boolean]
          | TypeExpr '|' TypeExpr      -- union as written in Strongtalk sources (stored)
          | TypeExpr 'def'             -- inference-clause sugar (stored, inert)
```

Well-formedness of *uses* (does the name resolve, arity of generic
application) is a T1 check; the parser only builds the tree.

## 4. Semantics (v1)

**Types.** `Dynamic` (the unannotated default), named class types resolved
against the live world (`Integer`, `String`, …), `Self`, block types with
arg/return components, `Boolean`/`UndefinedObject` for literals, and — as
inert stored forms — generic applications and unions.

**The gradual rule.** `Dynamic` is compatible in both directions: it is a
subtype and supertype of everything, and any message sent to a `Dynamic`
receiver checks trivially with a `Dynamic` result. An unannotated world
therefore produces **zero errors by construction**; value scales with
annotation coverage (§6, T4). This is Strongtalk's own retrofit story.

**Class interfaces.** Built per-class from the world sources the checker
already parses: superclass chain, ivars, and each method's captured
signature (defaulting every unannotated slot to Dynamic). Class-side
(metaclass) interfaces are built the same way from `class >>` methods.

**Subtyping (v1): nominal + trivially-terminating.** `S <: T` iff S = T,
S = Dynamic or T = Dynamic, T = Object, or T is on S's superclass chain;
block types check component-wise (contravariant args, covariant return —
ported from `DeltaBlockApplicationType`). With no structural protocols and
no generics, no recursion needs the coinductive trail; the API still takes
an assumption-trail parameter (`subtype_of(s, t, trail)`) so T5's
structural/generic rules land without resignaturing — the seam is
Strongtalk's `subtypeOf:assuming: DeltaGlobalTrail` verbatim.

**Checking rules** (each named for the Strongtalk source it ports):

- *Method rule* (`DeltaMethod>>typecheck`): synthesize the body's type;
  every `^expr`'s type must be a subtype of the declared return; a method
  with no explicit return answers Self, checked against the declared range.
- *Send rule* (`DeltaSend>>type`/`funType`): synthesize the receiver's
  type; if not Dynamic, look the selector up in its interface —
  **absence is the static-DNU error** (`DeltaSelectorUndefinedError`);
  check each argument `subtype_of` its formal; the send's type is the
  signature's range. Cascades check each message against the same receiver
  type (`DeltaCascadedSend`).
- *Assignment/temp/ivar rule* (`DeltaAssignment`): RHS `subtype_of` the
  declared LHS type.
- *Literals*: SmallInteger/LargeInteger → Integer, Float syntax → Double,
  `'…'` → String, `#…` → Symbol, `$c` → Character, true/false → Boolean,
  nil → UndefinedObject (nil is compatible where Strongtalk's sources
  assume it — v1 follows Strongtalk: no non-nullability).
- *Control forms*: `ifTrue:ifFalse:`, `whileTrue:`, `and:`/`or:`, `to:do:`
  are checked **as ordinary sends** against Boolean/Number interfaces with
  block-type arguments — the checker walks the pre-lowering AST and does
  not know the bytecode compiler special-cases them.

**Error catalog (v1 ≈ 12 of Strongtalk's 44).** SelectorUndefined,
SendArgNotSubtype (per-argument index, as in `DeltaSendArgumentNotSubtypesError`),
ReturnNotSubtype, AssignmentNotSubtype, TempInitNotSubtype,
IvarAssignNotSubtype, UndeclaredTypeName, GenericArityMismatch (use-site),
MalformedTypeExpr, BlockArityMismatch, BlockComponentNotSubtype,
CascadeOnErrorReceiver. Each ships with fixture tests reproducing the
Strongtalk semantics; the remaining ~32 (protocol conflicts, inference,
generic variance…) are T5's checklist.

## 5. Architecture

```
src/frontend/parser.rs      -- T0′: capture (not skip) annotations on MethodNode
src/types/                  -- the checker: type exprs, interfaces, subtype, rules
    (reachable ONLY from the `typecheck` subcommand — §7.1)
src/main.rs                 -- `macvm typecheck --world <dir> [--class C] [--json] [--strict]`
image_store                 -- two additive tables (§5.1); editor/browser surface errors
```

The checker parses the world with the ordinary frontend, builds interfaces,
checks method-by-method, writes the error table, prints a human report.
Exit code is 0 unless `--strict` (CI advisory by default — Strongtalk
fidelity: checking never gates anything).

### 5.1 DB schema (additive)

```sql
CREATE TABLE IF NOT EXISTS method_signatures (
  method_version_id INTEGER PRIMARY KEY REFERENCES method_versions(id),
  ret_type TEXT,            -- raw annotation text, NULL = Dynamic
  arg_types TEXT,           -- JSON array, NULLs for unannotated
  temp_types TEXT           -- JSON array
);
CREATE TABLE IF NOT EXISTS type_errors (
  id INTEGER PRIMARY KEY,
  class_name TEXT NOT NULL, selector TEXT NOT NULL, class_side INTEGER,
  kind TEXT NOT NULL,       -- catalog name, e.g. 'SelectorUndefined'
  message TEXT NOT NULL, arg_index INTEGER, span_start INTEGER, span_len INTEGER,
  method_version_id INTEGER, checked_at TEXT NOT NULL
);
```

Every `typecheck` run **replaces** the whole `type_errors` table (the
blast-don't-patch rule); incrementality, when wanted, is Strongtalk's
timestamp scheme relocated: skip methods whose `method_version_id` already
has a clean row set.

## 6. Staging — runnable checkpoint per slice

- **T0′ — capture.** Parser stores annotation text+spans on `MethodNode`;
  image import fills `method_signatures`. Ships in the SAME commit as the
  differential gate (§7.2) proving byte-identical codegen. Small.
- **T1 — representation + environment.** TypeExpr parser over captured
  text; interface builder; `macvm typecheck` reports UndeclaredTypeName /
  MalformedTypeExpr / GenericArityMismatch over the world (unannotated
  world ⇒ zero errors; fixtures exercise each).
- **T2 — subtyping + local rules.** `subtype_of` with trail seam; literal
  typing; assignment/temp/return rules; the catalog fixture suite begins
  (one test per error class, semantics cross-checked against the `.dlt`).
- **T3 — the send rule.** Static-DNU + argument checking + cascades.
  Checkpoint: `macvm typecheck --world world` runs clean and fast over all
  ~1,200 methods (expected findings ≈ 0 while unannotated — stated
  honestly: gradual typing finds little until signatures exist; literal
  receivers give the first real signal).
- **T4 — annotate the core library.** The payoff stage. Our collection/
  stream/number classes descend from Strongtalk's *already-annotated*
  `.dlt` sources — the annotations are ported from the same files the code
  came from (mechanical, per-class, each a small commit that must pass the
  differential gate AND the checker). This is when static-DNU starts
  catching real mistakes, and when the checker earns a battery slot.
- **T5 — optional, may never happen.** Generics checking, structural
  protocols, the trail in anger, brands, protocol-conflict errors.
  Decision deferred until T4 experience says whether the value is there.

## 7. Gating (the load-bearing section)

1. **Run-path isolation.** `src/types/` is reachable only from the
   `typecheck` subcommand. Nothing in the interpreter, compiler, JIT, GC,
   or world boot may reference it — reviewable by `grep -r "types::" src/`
   minus `main.rs`. The VM remains type-blind, like Strongtalk's.
2. **The differential gate** (battery-resident from T0′ onward): compile an
   ANNOTATED fixture world and its annotation-STRIPPED twin; every emitted
   method must be **byte-identical** (same discipline as this week's JIT
   gates). A stripping tool (`scripts/strip_types.py` or a `--strip-types`
   flag) keeps the twin honest. Any divergence fails the battery — this is
   the machine-checked form of "annotations must never change dynamic
   semantics" (the D11 comment, promoted to law).
3. **Advisory checking.** No mode ever blocks compiling, loading, or
   running a method that fails checking. `--strict` affects the EXIT CODE
   only, for CI consumers that opt in.
4. **Annotation-rot counterweight.** Unchecked annotations lie. Once T4
   annotates a class, that class's clean check joins the battery, so its
   annotations cannot silently rot.

## 8. Risks

- **Spec divergence** — a rewrite can quietly disagree with the debugged
  original. Countered by the per-error-class fixture porting and by
  keeping generics/protocols out of scope until the base has mileage.
- **Scope creep toward T5** — generics are seductive and hairy (variance,
  the trail, `DeltaInfiniteTypeExpansionError` territory). The staging is
  the defense; T5 needs its own design pass before any code.
- **Low early yield** — gradual Dynamic defaults mean T3 finds little on
  an unannotated world. Stated up front; T4 is where value concentrates,
  and it is deliberately mechanical.
- **Schema churn** — two additive tables, versioned like every image_store
  migration.

## 9. References

- `~/claudeprojects/strongtalk-repo/StrongtalkSource/DeltaSend.dlt` — the
  send rule (funType / inferTypeFrom: / per-arg subtypeOf:).
- `…/DeltaMethod.dlt` (`typecheck`, lines ~148-170) — the method rule,
  assumptions, incrementality.
- `…/DeltaBlockApplicationType.dlt`, `…/DeltaMsgSignature.dlt` — block and
  signature subtyping.
- `…/vm/compiler/primInliner.cpp:112` — the feedback-not-declarations
  false friend, kept as the cautionary exhibit.
- Bracha & Griswold, *Strongtalk: Typechecking Smalltalk in a Production
  Environment* (OOPSLA '93); Bracha, *Pluggable Type Systems* (2004) — the
  optional/pluggable framing this design keeps.
- `docs/cog_bench.md` — the measured record backing "performance comes
  from feedback, not declarations."
