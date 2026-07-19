# MACVM FFI Design — Cocoa + POSIX/BSD via `cocoa_data`

> **Just want to call something native?** See
> [`FFI_utility.md`](FFI_utility.md) — the practical "how do I call a new Cocoa
> method / add a POSIX binding, and when does `cocoa_data` matter" guide. This
> document is the design + machinery behind it.

Design for **S20** (`SPRINTS.md`'s existing, unordered Phase-E placeholder,
"Guest-language Cocoa bridge") — broadened, per this doc, to cover the curated
POSIX/BSD surface `WORLD.md` §9 already wanted alongside it, since both are the
same problem (a Smalltalk-callable native function) wearing two different
calling conventions. This is a **side/parallel track**: nothing here touches
`src/compiler`, `src/interpreter`, or any in-flight S11–S14 work. What ships
with this doc is a design plus one standalone, non-disruptive deliverable — an
automated generator tool (§6) — that runs entirely offline, the same way
`image_store`'s `import_world` binary runs offline against `world/*.mst`
without the GUI needing to change shape around it.

**Supersedes a dangling reference.** `WORLD.md` §9 says "the Alien route
(APPS.md §FFI)" — that section was never written; `APPS.md` has no `§FFI`.
This doc is the real one; §8 below updates that citation and `SPRINTS.md`'s
S20 entry to point here.

## 0. Scope and non-goals

- **Designed and partially built now**: the whole architecture (§§1–7), and a
  working, tested generator crate (§6) that queries `cocoa_data/cocoa.sqlite`
  and emits real Smalltalk-syntax bindings for a small curated manifest.
- **Built now (S20)**: the Tier-1 POSIX/libc calling path is built and
  callable — the VM-side calling primitive and its shape-keyed trampoline (§5)
  that actually execute a native call. dlsym symbol resolution,
  `ffi_stubs.rs`'s shape-keyed trampolines, the FFI primitive dispatched via
  `runtime/ffi`, the `<primitive: FFI ...>` pragma frontend (§6.3), and the
  `Alien` type are all in; the generator's output now runs against a primitive
  that exists.
- **Not built now**: Tier-2 Cocoa/ObjC dispatch — the
  `doesNotUnderstand:`-based `objc_msgSend` path (§3) — remains the scheduled
  remainder of the S20 implementation work.
- **Not attempted**: passing a Smalltalk closure as a native callback/block
  (§7) — genuinely harder than everything else here, explicitly deferred.

## 1. Ground truth: `cocoa_data`

`~/claudeprojects/cocoa_data` (a separate repo, shared across "every
data-driven compiler in the portfolio" per its own README) is the single
source of truth for every fact this design needs. MACVM **queries it, never
re-derives from the SDK** — the same relationship `image_store` has to
`world/*.mst`, and the same one the `cocoa-data` subagent (already used
earlier in this project to verify `performSelectorOnMainThread:`'s real
selector encoding) already has to it.

What's already there, built and working (row counts as of this writing, via
`ccq stats`):

| table | rows | what |
|---|---|---|
| `rt_classes` / `rt_methods` | 28,814 / 522,170 | every Obj-C class + every method's raw `@encode` signature, walked live off `libobjc` |
| `structs` / `struct_fields` | 2,403 / — | resolved C struct layouts — real member names (from BridgeSupport), byte offsets, size/align, an `object`/`nested`/`flat` exposure tier |
| `posix_functions` / `posix_function_args` | 299 | a curated POSIX.1/SUSv4 + Darwin BSD systems-programming surface (`ingest_posix.py`'s own scope note: "not all ~30k libc symbols" — file I/O, `mmap`, sockets, pthreads, signals, time, dirent, dlfcn), pulled from clang's AST, not hand-typed |
| `method_abi` | 522,170 | **every** runtime method already classified into AAPCS64 register-allocation tokens |
| `posix_function_abi` | 299 | the same token vocabulary for every curated POSIX function |

The token vocabulary (`derive_method_abi.py`'s own docstring, verbatim):

```
g          integer / pointer / id   -> one GPR    (return: a cell in x0)
f          float / double           -> one v-reg  (return: d0)
h2 h3 h4   homogeneous float aggregate (HFA) -> k consecutive v-regs
           (NSRect = h4, NSPoint / NSSize = h2)
i1 i2      small integer struct <=16 bytes  -> ceil(size/8) GPRs (NSRange = i2)
b          large struct (>16) by value      -> caller copy, pointer in a GPR (arg)
s          large struct return              -> sret, hidden pointer in x8 (return)
v          void                              (return only)
?          unmodelable / parse failure
```

Concrete, verified examples (`ccq abi method ...`, run against the live
database while writing this doc — not asserted from memory):

```
NSView>>frame                                   ret=h4   args=()        (NSRect back in v0-v3)
NSView>>initWithFrame:                          ret=g    args=(h4)      (NSRect IN via v0-v3, self back via x0)
NSColor class>>colorWithRed:green:blue:alpha:   ret=g    args=(f,f,f,f) (four doubles in d0-d3, id back in x0)
mmap                                            ret=g    args=(g,g,g,g,g,g)
```

## 2. Historical precedent: Strongtalk's `Alien`

Strongtalk already solved this problem once (`strongtalk-repo/StrongtalkSource/`
has ~24 Alien-related files — `Alien.dlt`, `ObjectiveCAlien.dlt`,
`ObjectiveCClassAlien.dlt`, `ExternalProxy.dlt`, `UnixFileDescriptor.dlt`,
plus a full test suite). Read directly (not from memory) before writing this
design, because it's the right shape to inherit *and* has one load-bearing
gap that explains why cocoa_data's ABI tables aren't optional.

**`Alien` is a `ByteArray` subclass** (`Alien.dlt:1-2`:
`Class subclassOf: 'ByteArray'`) — a raw memory buffer with a sign-flagged
size: positive means the bytes live inline in the object (`direct`), negative
means `addressField` holds a pointer to `malloc`'d (or borrowed) memory
(`indirect`/`C`) — `dataSize` is `self size abs` (`Alien.dlt:225-227`). Typed
access is a small, *fixed* family of accessors, not one per struct field:
`signedByteAt:` / `unsignedByteAt:` / `signedShortAt:` / `unsignedShortAt:` /
`signedLongAt:` / `unsignedLongAt:` / `floatAt:` / `doubleAt:`, each a thin
wrapper over one VM primitive (`Alien.dlt:216–358`). Calling out is one
arity-specialized primitive family — `primFFICallResult:with:with:...` up to
7 args, `withArguments:` beyond that (`Alien.dlt:363–558`) — invoked *on* an
`Alien` that wraps the target function's address (`thunk:`/`forPointer:`,
`Alien.dlt:80–85,155–157`).

**`ObjectiveCAlien`** (`ObjectiveCAlien.dlt`) builds Cocoa dispatch on top of
that with **one generic `doesNotUnderstand:`** (lines 282–300): any
unrecognized selector gets looked up via `class_getInstanceMethod` (itself an
Alien-wrapped `libobjc` call!), its `@encode` return-type character is parsed
live, and the send is forwarded through `objc_msgSend` using the same
`primFFICallResult:` mechanism — `MethodTypeMap`/`MethodTypeResultMap`
(lines 70–124) are the *only* place per-type marshaling logic lives, keyed by
the encoding character, not by selector. **No per-method code generation is
structurally required for this to work at all** — it's fully dynamic,
resolved at send time. (It also handles selectors that collide with existing
Smalltalk messages — `#class` can't be forwarded, so `#objectiveCclass` is
recognized and the prefix stripped, `asObjectiveCSelector:`, lines 164–191 —
a real naming problem MACVM inherits unchanged.)

**A second, distinct mechanism existed for plain C calls** — not
message-based at all. `UnixFileDescriptor.dlt` calls libc directly through a
*compiler-level* inline-primitive syntax:

```smalltalk
status := {{<libc ExternalProxy __fxstat> version: statBuffer version
                                          name: self handle
                                          buffer: statBuffer }}
```

(`UnixFileDescriptor.dlt:23-28`, matching `WORLD.md` §9's own citation). This
is resolved and compiled essentially like any other `<primitive: N>` — no
`doesNotUnderstand:`, no per-call dispatch — which is exactly the shape a hot
`read`/`write`/`mmap` loop wants and a fully dynamic Cocoa send doesn't need.

**Why this can't be ported byte-for-byte to MACVM's ARM64.**
`ObjectiveCAlien`'s marshaling (`FourByteParameterTypes`/
`EightByteParameterTypes`, lines 97–98, 147–162) computes one **flat byte
offset** per argument into a single marshaled-args buffer — there is no
notion of a separate float register file, because Strongtalk's original
target ABI didn't need one at that call boundary the way AAPCS64 does. On
Apple Silicon, `colorWithRed:green:blue:alpha:`'s four doubles go entirely
into `d0`–`d3`; `frame`'s `NSRect` return comes back in `v0`–`v3` as an HFA,
not via a flat offset into anything. A flat-buffer marshaler simply cannot
express "these bytes go in the FP register file, those in the GPR file, and
this one return shape is HFA, not sret." **This is precisely the fact
`method_abi`/`posix_function_abi`'s `g`/`f`/`h`/`i`/`s`/`v` tokens exist to
supply** — cocoa_data isn't a convenience for this design, it's the missing
piece that makes a *correct* ARM64 port of the Alien idea possible at all.

## 3. The two-tier design

Both tiers below share one representation (§4) and one underlying call
mechanism (§5); they differ only in *when* the ABI shape gets resolved and
*how* the Smalltalk-facing call reads.

**Tier 1 — POSIX/libc, direct C functions.** Mirrors the `{{<library Type fn>
args}}` shape: a compiler-recognized primitive naming a `posix_functions` row,
resolved (dlsym) and shape-classified (`posix_function_abi`) once — at image
build/generation time via the generator (§6), not per call. Fast by
construction: this is architecturally identical to any other
`<primitive: N>`, just backed by a shape-keyed trampoline (§5) instead of
hand-written Rust.

**Tier 2 — Cocoa/ObjC objects.** A `CocoaAlien` class (name TBD at
implementation time — `Alien`/`ObjectiveCAlien` are the obvious, historically
faithful choices, matching this project's existing practice of keeping real
Strongtalk names — `Workspace`, `Transcript`, `doit`, `printIt`) overriding
`doesNotUnderstand:`, exactly like the original: look up `(class, selector)`
in `method_abi` (pre-cached for anything the generator declared, §6; falling
back to a live `class_getInstanceMethod` + `@encode` parse — the same runtime
path `ObjectiveCAlien` always used — for anything sent that *wasn't*
pre-declared), then dispatch through `objc_msgSend` via the shape-keyed
trampoline. This is what makes "any of the 522,170 methods" genuinely
reachable, not just the curated slice the generator names — the generator
(§6) exists for **ergonomics and browsability** (real class/method
declarations, not a bag of dynamic sends), not because dynamic dispatch is
incapable of reaching the rest.

**Tier 2 is exactly where S11's PIC machinery already earns its keep a second
time.** A repeated Cocoa send from a hot loop is structurally the same
problem a polymorphic Smalltalk send is — same site, receiver class known
after the first hit, wants to skip re-resolution on the next 10,000 calls.
The inline-cache infrastructure built for ordinary sends (S11 steps 1–9)
applies unchanged: cache `(rcvr class, selector) -> (resolved objc_msgSend
target, shape tokens)` at the send site, same as a normal PIC caches
`(rcvr class) -> nmethod`. No new caching design needed here — just a new
kind of PIC entry.

## 4. Representation

- **Backing storage**: a new klass using the *existing* `Format::IndexableBytes`
  (`src/oops/klass.rs:25`) — the same format `ByteArrayOop` already uses —
  with the same direct/indirect sign-flagged-size duality real `Alien` uses
  (`Alien.dlt:225-227`, §2). This means the byte-level substrate is **already
  built**: `prim_at`/`prim_at_put`/`prim_byte_at`/`prim_byte_at_put`/
  `prim_byte_size` (`src/runtime/primitives.rs`, existing IDs) work on it
  unmodified. What's actually new is a small, fixed family of *typed*
  accessors (`signedLongAt:put:`, `doubleAt:`, …) — a handful of new
  `PrimDesc` entries mirroring real Alien's own accessor list 1:1, not a
  large undertaking and not one primitive per struct type.
- **Struct field access is generated Smalltalk, not new primitives** — a
  `struct_fields` row is `(name, idx, ty, offset)`; the generator (§6) emits
  one accessor method per field, each just calling the right typed accessor
  at the right byte offset (`frame origin x` → `self doubleAt: 1` if that's
  where `CGPoint`'s `x` lands). Real Strongtalk never needed per-struct
  primitives either — this preserves that.
- **Pointer/object wrapping and GC** — originally flagged open here; now
  CLOSED by [`cocoa_bridge_design.md`](cocoa_bridge_design.md): retain-on-wrap
  with a +1-family (alloc/new/copy) classifier, explicit release-with-poison +
  scoped `poolDo:` in v1 (no finalization dependency; leak-side failure bias),
  and — the punchline — **zero GC changes**: the `id` lives in a GC-opaque
  bytes payload (the Alien mechanism), the retain count is the external root,
  and neither memory manager ever traverses the other's graph.

## 5. The native call mechanism

- **External symbol resolution already exists.** `src/vendor/wfasm/
  native_macos.rs`'s `define_extern`/`lookup`/`has_symbol` already let
  JIT-compiled code reference external native addresses by name — this is
  presumably how VM-internal stubs (`stub_poll_addr`, `alloc_slow_addr`, seen
  in `driver.rs`) are wired today. Resolving `objc_msgSend`, a specific libc
  function, or a specific Obj-C selector's IMP is the same capability,
  pointed at a different symbol table (`dlopen`/`dlsym`, matching the
  existing `gui/src/objc.rs` bridge exactly — same technique, same
  dependency-free posture).
- **A small, fixed library of shape-keyed trampolines, not one per call
  site.** The number of *distinct* `arg_classes` strings across the entire
  522,170-method surface is bounded and small (a token alphabet of 8 symbols,
  argument lists rarely longer than ~6) — nowhere near 522,170. One
  hand-written (or JIT-emitted, reusing the tier-1 JASM assembler from
  S9/S10) trampoline per distinct shape — "load these N GPR args, these M FP
  args, call address in a register, unpack the return per `ret_class`" —
  covers the whole SDK. A call site (Tier 1: resolved once; Tier 2: resolved
  and PIC-cached, §3) just looks up which trampoline its shape maps to and
  supplies the target address plus already-marshaled argument words. This is
  the standard technique real general-purpose FFIs (libffi, Python's
  `ctypes`) use internally — MACVM doesn't need a novel mechanism, just this
  one, sized to reuse infrastructure that already exists.

## 6. The automated FFI generator — built now

A new, standalone workspace crate, **`ffi_gen`**, structurally parallel to
`image_store`: an offline tool that touches neither the interpreter nor the
compiler, consumes an external data source read-only, and emits artifacts a
later, still-to-be-built runtime piece will consume.

### 6.1 What it does

1. Opens `cocoa_data/cocoa.sqlite` read-only (`rusqlite`, matching
   `image_store`'s own dependency choice — `resolve_db_path`-style lookup:
   explicit path, `COCOA_DATA_DB` env var, then a well-known checkout
   location, mirroring `cocoa-core`'s own resolution order exactly so the two
   projects' tooling behaves identically for anyone using both).
2. Reads a **curated manifest** — which classes+selectors (Cocoa) and which
   function names (POSIX) MACVM's world actually wants named, browsable
   bindings for. Curated deliberately, same reasoning `ingest_posix.py`
   itself already gives for not ingesting all ~30k libc symbols: this is the
   part of the surface real Smalltalk code will `subclassResponsibility`/
   browse/edit, not an attempt to cover the SDK exhaustively (§3 already
   covers "exhaustively" via Tier 2's dynamic path).
3. For each manifest entry, queries `rt_classes`/`rt_methods`/`method_abi`
   (Cocoa) or `posix_functions`/`posix_function_args`/`posix_function_abi`
   (POSIX), plus `structs`/`struct_fields` for any struct-shaped
   argument/return type reachable from the manifest.
4. Emits real `.mst`-syntax Smalltalk (§6.3) — the same grammar
   `image_store::mst` already parses, so generated bindings are indistinguishable
   from hand-written ones and show up in the class browser (`docs/IMAGE.md`)
   like anything else.

### 6.2 What's built and tested today

- `ffi_gen` crate (`Cargo.toml` added to the workspace `members` list): a
  library (`ffi_gen/src/lib.rs`) plus a CLI, `generate_ffi`
  (`ffi_gen/src/bin/generate_ffi.rs`) — `cargo run -p ffi_gen --bin
  generate_ffi` (or point `COCOA_DATA_DB` at a `cocoa.sqlite` elsewhere).
- A query module mirroring the handful of `cocoa-core` lookups actually
  needed (`method_abi`, `posix_function_abi`, `structs`/`struct_fields`) — a
  fresh, small implementation rather than a cross-repo path dependency on
  `cocoa_data`'s own Rust crate, since `cocoa_data` is an
  independently-versioned shared resource for a whole portfolio of
  compilers, not something MACVM should couple its build to.
- A small worked manifest (`NSColor`/`NSView`-shaped Cocoa methods exercising
  `f`/`h4`/`g` shapes; the file-I/O POSIX functions `WORLD.md` §9 already
  wanted — `open`/`read`/`write`/`close`/`mmap`; a `CGPoint` struct-accessor
  class) and 10 tests confirming the emitted `.mst` text parses cleanly
  through `image_store::mst::parse_mst_source` and carries the right ABI
  shape tokens, run against the real database, not fixtures.
- **Two real bugs, found only by actually running the generator against the
  real database and reading its output** (not by the unit tests alone,
  which is exactly why this got done rather than left as a paper design):
  1. The `CGPoint` accessor's *setter* was malformed —
     `x: aNumber [ ^self doubleAt:put: 1 put: aNumber ]`, a bogus combined
     `"doubleAt:put:"` token reused as if it were one keyword before the
     index argument, doubling the `put:` part. Real Alien setters are a
     two-keyword send reusing the *getter's own* keyword
     (`self doubleAt: 1 put: aNumber`) — fixed, and the test that only
     checked the getter now checks the setter's exact text too.
  2. The seed manifest declared `open` as `openPath:flags:mode:` (3 args);
     `posix_function_abi` says 2. `open`'s real declaration is variadic
     (`open(path, flags, ...)`) — the `mode` argument only applies with
     `O_CREAT`, and clang's AST (what `posix_function_args` is built from)
     never records a variadic tail. The manifest was wrong, not the
     database; fixed to `openPath:flags:`, and `generate_posix_function`'s
     arity check (which caught this) is itself a test
     (`posix_manifest_arity_mismatch_is_a_generation_error_not_a_panic`).

### 6.3 What the generated output looks like (now runs — the Tier-1 FFI primitive exists)

Real output, `cargo run -p ffi_gen --bin generate_ffi`, verified against the
live database while writing this doc (not hand-typed to look plausible):

```smalltalk
"Generated by ffi_gen from cocoa.sqlite (docs/FFI.md, S20) — do not hand-edit; re-run instead.
 Forward-declared: no <primitive: FFI ...> exists in src/runtime/primitives.rs yet."

Alien subclass: CGPointAlien [
    "16 bytes, 2 field(s), byte-offset-addressed via the shared Alien typed accessors — docs/FFI.md §4."
    x [ ^self doubleAt: 1 ]
    x: aNumber [ ^self doubleAt: 1 put: aNumber ]
    y [ ^self doubleAt: 9 ]
    y: aNumber [ ^self doubleAt: 9 put: aNumber ]
]

Alien subclass: NSColorAlien [
    NSColorAlien class >> colorWithRed: a1 green: a2 blue: a3 alpha: a4 [
        <primitive: FFI selector: #colorWithRed:green:blue:alpha: class: #NSColor classSide: true ret: #g args: #(f f f f)>
    ]
]
Alien subclass: NSViewAlien [
    frame [
        <primitive: FFI selector: #frame class: #NSView classSide: false ret: #h4 args: #()>
    ]
    initWithFrame: a1 [
        <primitive: FFI selector: #initWithFrame: class: #NSView classSide: false ret: #g args: #(h4)>
    ]
]

Alien subclass: FFIPosix [
    FFIPosix class >> mmapAddr: a1 length: a2 prot: a3 flags: a4 fd: a5 offset: a6 [
        <primitive: FFI function: #mmap ret: #g args: #(g g g g g g)>
    ]
]
```

(`FFIPosix`'s other four methods — `open`/`read`/`write`/`close` — omitted
here for length; all five are in the real output.) Parameter names are
deliberately generic (`a1`, `a2`, …) rather than derived from ObjC keyword
text — real keyword parts don't always make distinct, valid Smalltalk
identifiers on their own, and generated code doesn't need to read as
hand-written (the header comment already says so).

The `<primitive: FFI ...>` pragma now resolves against the Tier-1 `FFI`
primitive that exists (dispatched via `src/runtime/ffi`) — the honest
forward-declared gap `world/*.mst` still carries in a few places for
primitives future sprints haven't reached is, for Tier-1 FFI, now closed. The
generator's contract — "produce the right *declaration*, resolvable the moment
the real primitive exists" — is met: the primitive exists.

## 7. Explicitly deferred

- **Callbacks/blocks** — passing a Smalltalk closure into a Cocoa API that
  wants a completion handler or enumeration block. Needs a stable per-closure
  native trampoline address and a story for safely re-entering the
  interpreter from arbitrary native call stacks — real Strongtalk's own
  block-arg handling (`ObjectiveCAlien.dlt`'s `argumentListFromMArgs:` and the
  `ObjectiveCSmalltalkObjectProxy` reference) shows the *receiving* half was
  solved; sending a Smalltalk block out as a native callback is the harder,
  unsolved direction. Not attempted here.
- **Variadic C functions** (`printf`-family) — no calling-convention story;
  cocoa_data itself doesn't attempt this either.
- **The one function `ingest_posix.py` already flags as unresolvable** —
  `signal`'s function-pointer-returning declarator (`derive_posix_abi.py`'s
  `KNOWN_STRUCT_RETURNS` comment, `ingest_posix.py:130-139`) needs a real C
  declarator parser upstream; this design inherits that limitation rather
  than working around it.

## 8. Doc cross-references fixed by this design

- `WORLD.md` §9: "the Alien route (APPS.md §FFI)" → this document.
- `SPRINTS.md` S20: still unordered/Phase-E, now points here instead of
  standing as a one-line placeholder; the VM-side primitive/trampoline
  remains the actual scheduled sprint work, deliberately not pulled forward.
- `SPEC.md` decision log gains **A23** (below), following the same one-line
  format as A17–A22.
