# GPU kernels in Smalltalk — the investigation and the AIR seam

Status: **investigation only — nothing built.** (2026-09-06)

The question. Guests already write MSL *fragment shaders* as strings —
`GamePane>>shader:` compiles Metal source at runtime, `shaderParam:value:`
feeds it uniforms, and the copper/plasma/attractor demos live on that path.
Could a guest instead write a *compute kernel in Smalltalk itself* — a block
or method, compiled by our own toolchain, running on the Apple GPU?

The answer this investigation reaches: **yes, and MACVM is unusually well
positioned**, because an adaptive-optimizing Smalltalk JIT already contains
most of a GPU kernel compiler. The Self/Strongtalk thesis is that sends
become arithmetic once you know the receiver types; a GPU kernel is the
extreme case where you must know all of them. What is genuinely new is
three semantic policy decisions and one emitter — not a compiler.

The other half is borrowed: the AIR lowering built in the MojoCocoa fork
(`../modular`) turns out to be reusable by any compiler that can print
LLVM IR as text. That seam is described first because everything else
leans on it.

---

## 1. The borrowed half: the AIR seam in `../modular`

Verified against the fork's source (2026-09-06):

**AIR is not an LLVM codegen target there.** The `TargetMachine` exists only
to run the optimizer; emission bypasses it. From the moment an `llvm::Module`
exists, the pipeline is pure module surgery, and none of it touches MLIR or
anything Mojo-specific:

1. `legalizeModule()` —
   `../modular/KGEN/lib/Compiler/ObjectCompiler/Target/Air/AirBackend.cpp`
   (~L1899): remaps address spaces, chases captured pointers into the device
   address space (`deviceizeCapturedPointers` — AIR has **no generic address
   space and no `addrspacecast`**), rewrites `llvm.air.*` thread-ID calls
   into the trailing `<3 x i32>` kernel parameters (`legalizeKernel`), and
   stamps all `air.kernel` / per-argument metadata and module flags.
2. `PointerRewriter` + `LLVMIRDowngradePass` — restores typed pointers
   (Apple never adopted opaque pointers) and strips IR constructs the old
   reader predates.
3. `WriteBitcode17ToFile` — serializes in LLVM-17 bitcode format, the
   version Metal's loader accepts.
4. `xcrun -sdk macosx metallib` packages the `.air` into a `.metallib`.

The proof the tail works from *textual* IR is already in that tree:
`../modular/KGEN/tools/kgen-llvm-opt/kgen-llvm-opt.cpp` parses any `.ll`/`.bc`,
resolves the Air target traits by triple (which force bitcode v17), and can
run the `air-legality` firewall and the downgrade pass over it — that is how
the corpus and the golden `.air.ll` oracle files are reviewed. What is
missing is only a thin CLI that also invokes `legalizeModule` and the
metallib packaging. Call it **`air-lower`: `.ll` in, `.metallib` out.**

**The consequence for MACVM: we never link LLVM.** Emitting a kernel is
printing a text file. Everything downstream of the text is the fork's
problem — already solved, and already gated by an oracle corpus.

### 1.1 The kernel-side contract an emitter must honor

- Triple `air64_v28-apple-macosx26.0.0` (spelling and versioning:
  `../oracles/findings/air-triple-and-version.md`).
- Address spaces: device=1, constant=2, threadgroup=3, private=0. No
  generic AS; every pointer's space is assigned at compile time.
- Kernels carry the `"air-kernel"` string attribute; buffer slot = argument
  index; by-value scalars become AS2 constant buffers bound with
  `setBytes:`.
- Thread indices: emit calls to the `llvm.air.thread_position_in_grid.*`
  family; the backend rewrites them to trailing `<3 x i32>` parameters.
- **No `double`, no `fp128`, no >64-bit integers.** This one is a hard
  language-policy constraint, not a nuisance — see §3.3.
- The LLVM-17 bitcode ban list (`freeze`, `poison`, unary `fneg`, new
  attribute codes, …) lives in
  `../oracles/findings/bitcode-version-skew.md`; the downgrade pass handles
  most of it, but an emitter that never produces those constructs is safer.
- The real acceptance gate is **PSO creation** (`../modular/tools/pso-check.sh`),
  not form checks — a form-valid metallib can still take down the Metal
  compiler service.
- If barriers ever appear (§6, K3): `convergent` attributes must be present
  at *declaration* time, before any optimizer runs — the fork has the scar
  tissue (cloned barriers, 22 of 32 lanes reading an unwritten tile).

---

## 2. The half MACVM already owns

A mapping from "what a GPU kernel story needs" to "the mechanism that
already ships":

| Needs | Already here |
|---|---|
| Turning sends into arithmetic | Type feedback + customization + budgeted inlining + block splicing (`src/compiler/{feedback,inline}.rs`, `docs/closure_compilation_design.md`). Kernel compilation is the same middle end under a closed-world rule. |
| A kernel ABI for closures | Mojo kernels *are* closures with capture packs, so the AIR backend's conventions were built for exactly the shape a Smalltalk block presents: captures → AS2 constant buffer, captured pointers chased to AS1. Block arguments = thread indices. |
| Stable buffers vs a moving heap | No pinning exists and none is needed: an **`Alien` (indirect) over `MTLStorageModeShared` contents** is literally the DIRECT_SCREEN pattern (`docs/DIRECT_SCREEN.md`, `publish_screen_memory`). Kernel data never lives in the Smalltalk heap; unified memory means zero copies. A `GpuFloatArray` is world-side sugar over (wrapped `MTLBuffer` id, Alien, length, element type). |
| Host-side dispatch | The objc bridge + FFI: `MTLCreateSystemDefaultDevice` via FFI, then plain sends (`newLibraryWithData:error:`, `newComputePipelineStateWithFunction:error:`, `setBuffer:offset:atIndex:`, `dispatchThreadgroups:threadsPerThreadgroup:`). Pure world code — the sockets/DNS "zero new Rust" pattern. FFI methods being interpreter-only is irrelevant at dispatch frequency. |
| GPU vector types | `Float32x4` / `Int32x4` value classes *are* MSL `float4`/`int4`, LLVM `<4 x float>`/`<4 x i32>`. |
| Live kernels | S13 dependency invalidation + `live_compile`: cache each metallib keyed by the (klass, selector) versions it inlined; a redefinition walks `deps[]`, marks the kernel stale, and the next dispatch recompiles. **Edit a kernel method in the browser and the running simulation picks it up** — `GamePane>>shader:` already proves the live loop for fragment shaders, by hand. |
| A correctness oracle | Every kernel is a block; running it in a `to:do:` loop on the CPU is the semantic ground truth *and* the differential test — the same interpreter-vs-JIT oracle discipline, and the same method as the fork's `air-oracle.sh`. |

Culture precedents worth naming: Squeak's Slang established that "a
restricted Smalltalk subset that compiles statically" is an accepted move in
this community; TornadoVM proves a managed runtime can JIT annotated
methods to GPU code with specialization and fallback. Neither does it
*live*, against an image, with dependency-tracked invalidation. That
combination would be new.

---

## 3. The three genuinely hard problems

### 3.1 No deoptimization on the GPU

This is the deep one. MACVM's entire performance model is
speculate → guard → uncommon-trap into the interpreter. A GPU lane has no
interpreter to trap into. So kernel compilation must be **total**: every
send statically resolved and inlined (or a primitive), every representation
known, or compilation is *refused*.

The saving grace is that refusal has a perfect fallback: run the block on
the CPU. The GPU path is an optimization over unchanged semantics, exactly
like the JIT is over the interpreter. Concretely:

- Guards move from run time to **dispatch time**: shape/type checks on the
  actual arguments, host-side, once per dispatch — not per element.
- Specialization is keyed the way customization already is: one compiled
  metallib per (kernel method versions, argument shapes).
- Anything unprovable at kernel-compile time is a compile-time rejection
  with a reason, and the dispatch runs the block in a loop instead
  (optionally with a Transcript notice).

### 3.2 Exceptions become deferred and collective

`ZeroDivide`, bounds failures, and overflow cannot raise mid-kernel — there
is no frame to unwind on a lane. The design that fits:

- **Structural bounds safety** for the map/do: pattern: dispatch is sized
  to the buffer, and the emitter inserts the standard `gid < n` guard —
  bounds checks on the *iteration variable* are thereby free.
- Everything else (an explicit `at:` with a computed index, `//` by zero)
  compiles to a **predicated write of a defined lane result** (0 / NaN)
  plus an atomic OR into a one-word status buffer; the host checks the
  word after completion and raises then.

House precedent says this is acceptable: MF66 redesigned division from
"crash" to "guarded THROW", and `FloatArray>>sum` already documents a
defined deviation (pairwise order) with `sequentialSum` for bit-parity.
"Defined deviation, documented per op" is existing style, not a new
compromise.

### 3.3 Numeric policy vs the f64 ban

AIR rejects `double` outright — and Smalltalk `Float` is boxed f64; even
`FloatArray` is f64 lanes. Policy required:

- **Kernels compute in f32.** A documented deviation, like §3.2's. This
  implies a `Float32Array` / `GpuFloatArray` container that does not exist
  today (f32 currently exists only as `Float32x4` lanes and as
  shader-param coercions).
- **SmallInteger in kernels is i32 (or i64) with wraparound.** The
  overflow-to-LargeInteger promotion is a primitive-fail-to-bytecode
  mechanism, which has no meaning on a lane.
- `Float64x2` and anything f64 is rejected in kernel bodies at compile
  time.

---

## 4. The one new component: the emitter

Two candidates were weighed:

**Principal path — SSA-lite IR → textual `.ll`.** `src/compiler/ir.rs` is a
machine-shaped CFG; it maps ~1:1 onto LLVM IR text. Emission is string
printing — no LLVM linkage, no new crate dependencies. `air-lower` does the
rest, and every hard-won finding in `../oracles/findings/` applies
verbatim. Cost: macVM depends on (or bundles) the `air-lower` binary and
shells out at kernel-compile time. We already sign and notarize; a bundled
helper tool is normal.

**Escape hatch (exists today) — MSL source strings.** Fully self-contained
(`newLibraryWithSource:`, the `GamePane` path). But *generating* MSL from
the optimized CFG needs control-flow restructuring (a relooper), and
generating it from the AST instead forfeits the middle end. So: keep MSL
strings as the manual path and as the **bring-up oracle** — the same kernel
hand-written in MSL vs compiled through AIR, outputs diffed — but make
`.ll → air-lower` the compiled path. It is also the only option that
actually reuses the AIR investment.

Rejected: linking LLVM into macvm (an enormous dependency for a printer),
and AST→MSL as the primary compiled path (forfeits feedback and inlining).

---

## 5. Surface sketch (illustrative, not a commitment)

One spelling **is** decided (2026-09-07): **code to be lowered to the GPU is
marked in a method with the `<gpu>` pragma** — the same method-pragma family
as `<primitive:>`, parsed by machinery that already exists. A method carrying
`<gpu>` compiles under the kernel dialect below. Everything else in this
section stays illustrative.

```smalltalk
| saxpy |
saxpy := GPU kernel: [:i | y at: i put: a * (x at: i) + (y at: i)].
saxpy over: x size.
```

- `x`, `y` — `GpuFloatArray`s → AS1 device buffers, slots by argument
  order.
- `a` — captured Float → f32 in the AS2 capture pack. Captures are
  **frozen by value at dispatch**; host-side mutation during a running
  dispatch is unobservable, and mutating a captured temp *inside* a kernel
  body is a compile-time rejection. Both are defined rules, not accidents.
- `:i` — `thread_position_in_grid`.
- First `over:` specializes against the shapes at hand, compiles, caches;
  refusal falls back to `1 to: n do:` on the CPU.

Kernel-dialect restrictions (compile-time rejections): no allocation, no
non-local return / `ensure:`, no f64, no LargeInteger promotion, sends must
resolve and inline against known representations, no Cocoa/FFI/prims with
host effects.

**Open design fork — how the world gets closed.** Two defensible answers:

- *Annotations required* (Strongtalk's redemption): kernel methods carry
  type annotations, making compilation deterministic and giving
  `src/types/` its first run-path job. Tension: the deliberate
  types-off-the-run-path invariant — resolvable by framing kernel
  annotations as part of the kernel dialect (like `<primitive:>` pragmas)
  rather than as the type system acting on execution.
- *Shapes at dispatch* (the Self answer): specialize on the actual argument
  types the way customization already does; no annotations, but whether a
  kernel compiles can depend on the call site.

---

## 6. Order of risk, if this is ever built

- **K0 — zero VM changes.** Build `air-lower` in `../modular`; hand-write
  `saxpy.ll` to the §1.1 contract; write the *entire* host side in world
  code (FFI device creation, objc sends for pipeline and dispatch, results
  read back through an Alien). K0 proves the whole chain end to end;
  everything after it is compiler work, not systems risk.
- **K1 — straight-line map kernels.** `GpuFloatArray`, scalar captures,
  the auto `gid < n` guard, the metallib cache, the CPU fallback.
- **K2 — control flow and reductions.** `ifTrue:`/`whileTrue:` in kernel
  bodies, the i32 policy, the deferred-exception status buffer.
- **K3 — live-edit integration and threadgroup memory.** `deps[]`-driven
  metallib invalidation wired to the browser accept path; threadgroup
  memory and barriers last (the convergent-at-declaration lesson applies
  the moment barriers exist).

---

## 7. Open questions

- **Device/queue ownership.** A world-owned `MTLDevice` (pure Smalltalk)
  vs sharing MacGamePane's device in the GUI process — required the moment
  kernels feed pane textures. Unified memory suggests both compose; the
  ownership rule still needs writing.
- **Workers.** A dispatch is naturally asynchronous — model the GPU as a
  worker answering `send:onReply:`? Share-nothing survives, because
  buffers are Aliens outside every heap.
- **`air-lower` distribution.** Bundled signed helper in macVM.app vs a
  dev-machine-only feature initially (the tool is built from the modular
  tree and links a chunk of LLVM).
- **Threadgroup memory surface** in the language (deferred to K3).

---

## Appendix: the MacModula2 comparison

The same investigation covered MacModula2, which turns out to be a genuine
LLVM frontend (inkwell, static LLVM 22.1). There the gaps are frontend
features only: no address-space qualifier on pointer types, no kernel
attribute on the IR `Func`, no GPU builtins, no device-module split — while
the host side is nearly finished (Metal.framework linked into every AOT
build, `ShaderPane` runtime-MSL demos, cocoa.sqlite already carrying the
`MTL*` compute selectors). Both languages meet the same seam: emit a
kernel-only LLVM module honoring §1.1, hand it to `air-lower`. The seam
does not care whether the producer links LLVM (MacModula2) or prints text
(MACVM) — which also means any other port in the workspace could join by
printing a `.ll` for kernels alone.

## References

- `../modular/KGEN/lib/Compiler/ObjectCompiler/Target/Air/AirBackend.cpp` — `legalizeModule`, `emitObject`
- `../modular/KGEN/tools/kgen-llvm-opt/kgen-llvm-opt.cpp` — textual IR entry, versioned bitcode writers
- `../modular/APPLE_GPU_LOWERING_REVIEW.md`, `../modular/AIR_APPLE_SILICON.md`
- `../oracles/findings/` — `bitcode-version-skew.md`, `address-spaces.md`, `typed-pointers.md`, `pipeline-state-gate.md`, `air-triple-and-version.md`
- `../oracles/oracle/apple-m4/` — golden `.air.ll` files from the released compiler
- In-tree assets this leans on: `docs/DIRECT_SCREEN.md`, `docs/gamepane_design.md`, `docs/ALIEN.md`, `docs/SIMD.md`, `docs/closure_compilation_design.md`, `docs/cocoa_bridge_design.md`, `src/compiler/ir.rs`
