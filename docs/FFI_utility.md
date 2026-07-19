# Calling native code from MACVM — a practical guide

*How to call Cocoa/AppKit and POSIX/C from Smalltalk, when you need a binding
and when you don't, and where the `cocoa_data` database actually fits. For the
machinery and design rationale see [`FFI.md`](FFI.md); this is the
"how do I actually do it" companion.*

## The one thing to know

There are two ways native code is reached, and they behave very differently:

| | **Dynamic ObjC bridge** | **FFI pragma** |
|---|---|---|
| For | Cocoa / AppKit / Foundation **methods** | raw **C / POSIX** functions |
| You write | just a message send | a `<primitive: FFI …>` pragma |
| ABI comes from | the **live macOS runtime** (`@encode`), read at the call | tokens **you write** in the pragma |
| Needs a binding? | **No** — works out of the box | Yes — one pragma line |
| Needs `cocoa_data`? | **Never** | Only if you auto-generate instead of hand-writing |

**Calling a new Cocoa method needs nothing.** You only reach for a pragma —
and only *maybe* touch `cocoa_data` — when you bind a raw C function.

## Path A — Cocoa / ObjC: just send the message

The bridge resolves classes and selectors **by name** against the live
Objective-C runtime and asks the runtime for the method's type signature
(`@encode`, via `method_getTypeEncoding`) at call time, then marshals the
arguments and calls `objc_msgSend`. Resolved shapes are cached per
`(class, selector)`. Nothing is pre-declared; nothing is bundled.

```smalltalk
"Get a class object by name, then send it any selector."
| sound |
sound := (Cocoa classNamed: 'NSSound') onMain soundNamed: 'Ping'.
sound onMain play.

"Numbers marshal to the callee's registers per its own @encode:"
(Cocoa classNamed: 'NSColor') onMain
    colorWithRed: 0.2 green: 0.5 blue: 0.9 alpha: 1.0.   "four doubles -> d0..d3"

"Structs-by-value are passed as an Array of numbers:"
aView onMain setFrame: (Array with: 0.0 with: 0.0 with: 200.0 with: 120.0).  "NSRect"
```

`onMain` runs the call on the AppKit main thread (required for UI); a
non-UI ObjC call can use the plain bridge send. This is exactly how the entire
Cocoa GUI is written — hundreds of distinct selectors (`setTitle:`,
`initWithFrame:`, `addSubview:`, …), **not one of them a pre-declared binding.**
A method you create in the class browser can use these freely, and it runs in
the shipped `.app` with no extra setup — because the type information is read
live from macOS, which is the same place `cocoa_data` scraped it from.

**What the bridge can marshal** (argument → return): objects / `Class`
(`ObjcRef`, `nil`, or a `String` → temporary `NSString`); `Bool`; every integer
width; `Double` / `CGFloat`; `float`; a selector name; and the common
value-structs — `CGPoint`/`CGSize` (2-number `Array`), `CGRect` (4-number
`Array`), `NSRange` (2-integer `Array`); plus `char*` returns (copied out).
That covers the overwhelming majority of AppKit and Foundation.

**What it can't** (see "Real limits" below): a method whose `@encode` has a
shape the marshaller doesn't model — an odd large struct-by-value that isn't
one of the known ones, a block / function-pointer argument, or a variadic
method. These are *bridge* limits, not data limits — `cocoa_data` wouldn't help.

## Path B — raw C / POSIX: one pragma line

A plain C function (`getpid`, `mmap`, `kqueue`, …) has no `objc_msgSend` and no
`@encode` to read, so you declare its ABI yourself in a method pragma. The
symbol is resolved by `dlsym` in the process-global namespace (`RTLD_DEFAULT`),
so anything already linked (libc, and whatever frameworks are loaded) is
reachable — no `dlopen` path needed.

```smalltalk
Object subclass: Posix [
    Posix class >> getpid [ <primitive: FFI function: #getpid ret: #g args: #()> ]
    Posix class >> mmapLen: len [
        <primitive: FFI function: #mmap ret: #g args: #(g g g g g g)>
    ]
]
```

**Shape tokens** (the AAPCS64 register-classification vocabulary, `FFI.md` §1):

| token | meaning | register(s) |
|---|---|---|
| `g` | integer / pointer / id | one GPR (return: `x0`) |
| `f` | float / double | one v-reg (return: `d0`) |
| `h2` `h3` `h4` | homogeneous float aggregate (HFA) — `NSPoint`/`NSSize` = `h2`, `NSRect` = `h4` | *k* consecutive v-regs |
| `i1` `i2` | small integer struct ≤ 16 bytes — `NSRange` = `i2` | `ceil(size/8)` GPRs |
| `b` | large struct (> 16 B) passed by value | caller copy, pointer in a GPR |
| `s` | large struct **return** | `sret`, hidden pointer in `x8` |
| `v` | void | return only |

Most POSIX functions are all-`g` (pointers and ints), so the pragma is trivial
to write by hand. Verified examples from the shipped world:

```
mmap             ret=g  args=(g g g g g g)      world/30_date_time.mst
clock_gettime    ret=g  args=(g g)              world/30_date_time.mst
kqueue           ret=g  args=()                 world/61_posix_io.mst
```

If you get a token wrong the call mis-marshals (wrong register, shifted stack
args), so match the C prototype: pointers/ints → `g`, `double`/`float` → `f`,
a by-value `struct { double … }` → `h`*k*, a by-value small int struct → `i`*k*.

## Where `cocoa_data` fits — and where it doesn't

`cocoa_data` is a **sibling repo** (`~/claudeprojects/cocoa_data/cocoa.sqlite`)
— a SQLite mirror of the ObjC runtime + a curated POSIX surface, every entry
pre-classified into the shape tokens above. It is read by exactly one thing:
the offline generator **`generate_ffi`** (`ffi_gen` crate).

```
cocoa.sqlite  --(generate_ffi reads it once)-->  .mst pragmas  --(committed)-->  world/*.mst
```

`generate_ffi` reads the database and **emits `.mst` Smalltalk** with the FFI
pragmas already filled in — those `.mst` files are committed into `world/` and
loaded like any other code. So the database's data is baked into source at
*authoring* time; after that it's out of the loop.

**It is NOT:**
- a build dependency — `cargo build` never opens it (`ffi_gen` is a standalone
  workspace member, not a `build.rs` and not a dependency of the VM or GUIs);
- a runtime dependency — the VM, the JIT, and both GUIs never read it (Path A
  reads `@encode` live; Path B reads the tokens from the committed pragma);
- bundled in the `.app`/`.dmg` — the generated `.mst` is bundled, not the DB.

**When you actually need it:** only to *auto-derive* a gnarly ABI for a new
raw C/POSIX (or Tier-2 static Cocoa) binding instead of hand-writing the
pragma. On a dev machine with the sibling repo present:

```
cargo run -p ffi_gen --bin generate_ffi
```

It resolves `cocoa.sqlite` from, in order: an explicit path argument →
`$COCOA_DATA_DB` → `./cocoa.sqlite` → the fallback
`/Users/oberon/claudeprojects/cocoa_data/cocoa.sqlite`. It prints/emits the
`.mst` you then commit. A user of a shipped `.app` who wants a new C binding
either hand-writes the one-line pragma (the tokens above) or you regenerate and
ship an updated build — the running app never needs the database.

## Real limits (independent of `cocoa_data`)

- **Variadic C functions** (`printf`, `open` with the optional `mode`) — the
  FFI primitive is fixed-arity, so a variadic tail mis-marshals (arm64
  stack-passes variadic args while the trampoline loads registers). Three ways
  out, cheapest first: reach for a non-variadic libc equivalent, split the
  call, or add a shim/primitive. `world/61_posix_io.mst` takes the first two
  for files — it binds the fixed 2-arg `creat(path, mode)` to create/truncate
  and `open(path, flags)` (never `O_CREAT`, whose `mode` is the variadic tail)
  to open existing files, so `PosixFile` reaches the whole file surface with no
  variadic call at all.
- **Block / function-pointer arguments** to ObjC methods — the dynamic bridge
  doesn't synthesize a C callback from a Smalltalk block for arbitrary
  signatures (the reverse path is the C6 delegate mechanism, not a general
  block-to-`imp` bridge).
- **Unmodelled `@encode` shapes** — a by-value struct that isn't a known HFA /
  small-int-struct, a union argument, a C-array argument. These come back as
  `?` (unmodelable) and need either a bridge extension (Rust) or a wrapper
  method with a friendlier signature.

For any of these, the fix is Rust-side (extend the bridge / add a primitive),
not a data lookup — `cocoa_data` classifies ABIs, it doesn't remove these
marshalling gaps.

## Worked example — whole-file access

Both paths side by side: the two file classes (pure Smalltalk, zero new Rust).

- **`PosixFile`** (`world/61_posix_io.mst`, Path B) — byte-exact fd I/O over
  `open`/`creat`/`read`/`write`/`lseek`/`close` bindings; whole-file
  `slurp:`/`spit:contents:` on a `NativeBuffer`.
- **`CocoaFile`** (`world/49a_cocoafile.mst`, Path A) — UTF-8 text through
  `NSString`'s own file methods, sent with the direct bridge (no binding, so it
  also works headless).

```smalltalk
PosixFile spit: '/tmp/x' contents: 'raw bytes'.    PosixFile slurp: '/tmp/x'.
CocoaFile spit: '/tmp/x' contents: 'héllo ☕'.       CocoaFile slurp: '/tmp/x'.
```

`PosixFile` moves bytes verbatim (binary-safe: a `String` and a `ByteArray`
are both byte-indexed, so a file's bytes *are* a String's UTF-8); `CocoaFile`
decodes/encodes UTF-8 explicitly (a non-UTF-8 file slurps as `nil`). On valid
UTF-8 the two interoperate byte-for-byte.

## See also

- [`FFI.md`](FFI.md) — the FFI design, the two-tier model, the native call
  mechanism, and the `generate_ffi` internals.
- The dynamic bridge implementation: `src/runtime/objc_bridge.rs` (the
  `@encode` → `Shape` parser and the `objc_msgSend` marshaller).
- The FFI primitive + `dlsym` resolution: `src/runtime/ffi.rs`,
  `src/codecache/ffi_stubs.rs`.
- Worked bindings in the world: `world/30_date_time.mst`,
  `world/61_posix_io.mst` (POSIX + `PosixFile`), `world/49_cocoa.mst`,
  `world/49a_cocoafile.mst` (`CocoaFile`).
