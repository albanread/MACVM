# Accelerate for array/vector processing — investigation + design

**Status: BUILT (A0–A4, 2026-07-21).** A0 = zero-tail indirect Aliens + the
loud >8-arg/arity-mismatch FFI errors (856f9ff). A1 = the `Accel` bindings
+ `NativeFloatArray` world layer + AccelTests (1205e4b). A2 = the U1
per-descriptor dlsym cache (6fd9f17). A3 = the U2 stack-spill trampoline
tier + `cblas_dgemm`/`vDSP_mmulD` + `NativeMatrix` (887ba6e). A4 capstone
measured: a 256×256 matrix product runs 24.5 s as a naive Smalltalk triple
loop and <1 ms through `NativeMatrix mm:into:` (dgemm) — four orders of
magnitude, numerically identical results. The FFT worked example is the
one de-scoped A4 item: `vDSP_create_fftsetupD`/`vDSP_fft_zipD` are
bindable today (≤5 g args, split-complex staged by pointer) and wait only
on someone wanting them.

Question asked: can Smalltalk naturally use the matrix/vector operations
in Apple's Accelerate framework, and how should MACVM enable them for
array/vector acceleration?

**Answer: yes — a useful majority of Accelerate (vDSP elementwise +
reductions, all of vForce, double-precision FFT) is callable TODAY with
zero new Rust**, through the existing S20 FFI, and was proven live by a
probe in this session. Two small, optional Rust unlocks (a dlsym cache and
a stack-spill tier for the trampoline) later extend the reach to BLAS-3
(`cblas_dgemm`) and LAPACK, and move the profitable size range down by two
orders of magnitude.

## 1. What the investigation established (all run for real)

The probe (`accel_probe.mst` / `accel_scale.mst`, session scratchpad) did
the following against the live VM:

1. **Loading the framework needs no Rust.** `dlopen` is itself a 2-arg C
   function in libSystem, so it is FFI-bindable like any Posix call:

   ```smalltalk
   Accel class >> dlopenPath: p mode: m [
       <primitive: FFI function: #dlopen ret: #g args: #(g g)> ]
   "dlopenPath: <path cString> mode: 10   (RTLD_NOW|RTLD_GLOBAL)"
   ```

   `RTLD_GLOBAL` makes Accelerate's symbols visible to the FFI's own
   `RTLD_DEFAULT` dlsym, so every subsequent `<primitive: FFI function:
   #vDSP_vaddD …>` just resolves. Verified: handle non-zero, all
   subsequent binds resolved.

2. **Real kernels, numerically verified**: `vDSP_vaddD` (elementwise add,
   7 int/pointer args), `vDSP_dotprD` (dot product, 6 — result returned
   through a pointer), and vForce's `vvexp` (3 — the COUNT arrives by
   pointer, an Accelerate idiom that conveniently keeps arities tiny).
   Exact expected values came back in all three.

3. **The hard boundary is the trampoline's 8-GPR limit** (`ffi_stubs.rs`:
   a fixed 8 GPR + 8 FPR register load, no stack spill). `vDSP_mmulD`
   (matrix multiply, 9 integer-class args) is one arg over; probed and
   refused. `cblas_dgemm` (12 g + 2 f) and LAPACK drivers are further
   over. **Hazard found while probing**: a >8-arg FFI pragma today
   returns `Fallthrough` into the empty pragma body — a SILENT no-op that
   answers the receiver. Under the 2026-07 FFI error convention
   (`error::guest_fatal`, f91a8b8) this should be a loud guest fatal
   naming the limit; one-line fix in `dispatch_ffi_primitive`.

4. **Performance shape** (fanless-Air numbers, interleaved rounds,
   consistent across repeats; treat as magnitudes not absolutes):

   | lanes | vDSP via FFI | in-VM NEON `+@` | note |
   |---|---|---|---|
   | 512 × 2000 reps | ~29 ms | ~2 ms | FFI per-call tax dominates |
   | 8 192 × 500 | ~8 ms | ~4–5 ms | crossover region |
   | 131 072 × 60 | ~2–3 ms | ~9 ms | vDSP ~3× |
   | 1 048 576 × 10 | ~3 ms | ~18–23 ms | vDSP 6–8× (and `+@` allocates its 8 MB result per call; vDSP writes in place) |
   | `vvexp` 4 096 × 200 | ~2–3 ms | ~117 ms (scalar `exp` loop) | **40–60×** — no in-VM competitor exists |

   The per-call tax is ≈14 µs, and it is almost entirely the FFI's
   **dlsym-on-every-call** — already documented in `runtime/ffi.rs` as a
   deliberate scope cut with a per-descriptor cache as the planned fix.

5. **The GC law that shapes the API**: `FloatArray` data lives in the
   movable heap; Accelerate must be handed **GC-stable memory** (the
   established rule — `NativeBuffer`'s mmap page, PosixFile, the async-IO
   buffers). Big regions work the same way: the probe mmap'd 3 × 8 MB
   anonymous regions from pure Smalltalk (`Posix mmapAddr:length:…`) and
   ran vDSP over a million lanes.

## 2. Design

Three Smalltalk layers (zero new Rust), then two optional Rust unlocks.
The in-VM NEON `FloatArray` kernels are NOT replaced — they own the
small-N/interactive tier; Accelerate owns bulk and transcendentals. Dual
placement, complementary tiers.

### A. `NativeFloatArray` — GC-stable lanes (world/61a)

N f64 lanes in an anonymous mmap region: `address` (SmallInteger, for
`#g` args) + an internal `Alien` for `doubleAt:`-style lane access, plus
`size`. The FloatArray protocol surface (`at:`, `at:put:`, `from:`,
`asFloatArray`, `do:`) so the two interconvert by copying.

Lifecycle is EXPLICIT, like every other native resource in this world
(streams `close`, `Cocoa poolDo:`): `free` munmaps (`munmap` is a 2-arg
FFI bind), and the scoped form is the recommended idiom:

```smalltalk
NativeFloatArray lanes: 1000000 do: [ :v | ... ]   "frees on the way out"
```

No finalizers exist in MACVM by design, so an unfreed array is simply
leaked until exit — same honest price the IoWorker kq fd already pays,
but here with a `free` the user can and should call.

### B. `Accel` — the raw bindings (world/61a)

Class-side facade, lazy one-shot `ensureLoaded` (class-var latch around
the dlopen). The surface that fits ≤8 g args — which is most of what
matters:

- **vDSP elementwise** (7 g): `vaddD vsubD vmulD vdivD`, `vsmulD`
  (scalar by pointer), `vfillD`, `vclrD`, `vabsD`, `vnegD`.
- **vDSP reductions** (4–6 g): `sveD` (sum), `dotprD`, `maxvD`, `minvD`,
  `meanvD`, `rmsqvD`, `svesqD`.
- **vForce transcendentals** (3–4 g, count by pointer): `vvexp vvlog
  vvsin vvcos vvtan vvsqrt vvpow vvatan2` — the biggest win, nothing
  in-VM competes.
- **FFT** (double, split-complex): `vDSP_create_fftsetupD` (2 g → opaque
  setup pointer, held in a Smalltalk-side handle), `vDSP_fft_zipD` (5 g —
  the split-complex struct passes BY POINTER, staged in a NativeBuffer).
  Reachable today; worth a worked example in the capstone.

Every binding follows the Posix file's conventions: fixed arity, errors
as values, a comment naming the C signature.

### C. High-level API (world/61a)

`NativeFloatArray` gains the ergonomic ops, in-place by default (that is
vDSP's shape, and the benchmark showed result-allocation is real cost):

```smalltalk
a add: b into: c        "c := a + b (c may be a — in place)"
a scaleBy: 2.5          "vsmulD"
a dot: b                "dotprD -> Double"
a sum  a mean  a max
a expInto: c            "vvexp"
(a fftForwardWith: setup) ...
```

Plus copy bridges: `FloatArray>>asNative`, `NativeFloatArray>>
asFloatArray`. Guidance baked into the class comment: stay
native-resident across a pipeline; a one-shot copy-in/copy-out only pays
for transcendentals (measured 40–60×) or very large N.

### The two optional Rust unlocks (small, ordered by value)

- **U1 — per-descriptor dlsym cache** (`runtime/ffi.rs`; already the
  documented planned pass). Removes the ~14 µs/call tax that currently
  sets the break-even at ~8 K lanes; benefits EVERY FFI user (Posix,
  kqueue, sockets) for free. After it, vDSP should win from a few
  hundred lanes up. **BUILT (A2): descriptor slot 7 caches the resolved
  address as an immediate SmallInt. Re-measured: the 512-lane × 2000-rep
  case fell 29 ms → 1-2 ms — vDSP through the FFI is now at parity or
  faster than the in-VM NEON kernels at EVERY size swept (and 5-8× ahead
  from 8 K lanes up), so the "small N belongs to FloatArray" guidance
  weakens to "FloatArray is fine when the data already lives on the
  heap".**
- **U2 — stack-spill tier for the trampoline** (`ffi_stubs.rs`): AAPCS64
  passes integer args 9+ on the stack; a second trampoline flavor that
  spills `argv_g[8..15]`/`argv_f[8..]` to `[sp]` (16-byte aligned)
  unlocks `vDSP_mmulD`, `cblas_dgemm` (12 g + 2 f), and the LAPACK
  drivers (`dgesv_`, `dpotrf_`, …) — i.e. real matrix work and a future
  `Matrix` class. Bounded by `METHOD_ARGC_MAX` (15), which covers
  `dgemm` exactly. Prerequisite fix regardless: make the current >8-arg
  silent no-op a loud `guest_fatal`.

### Suggested ladder (when/if built)

- **A0**: `Accel` bindings + `ensureLoaded` + the >8-arg guest-fatal fix
  + gates in it_world (numeric checks — everything here runs headless).
- **A1**: `NativeFloatArray` + lifecycle + high-level ops + copy bridges
  + gates; benchmark entry alongside the SIMD ones.
- **A2** (Rust): U1 dlsym cache + re-run the sweep (expect break-even
  ≈ hundreds of lanes).
- **A3** (Rust): U2 stack-spill + `dgemm` + a `Matrix` class over
  NativeFloatArray (row-major, `mm:into:`).
- **A4 capstone**: FFT worked example + a matrix-multiply benchmark vs a
  pure-Smalltalk triple loop (expect orders of magnitude).

## 3. What NOT to do

- **Don't migrate FloatArray onto native memory.** Its heap residency is
  what makes it cheap, GC-integrated, and safe for casual use; the NEON
  prims own small N (they beat vDSP 15× at 512 lanes today). Two tiers,
  both first-class.
- **Don't link Accelerate into the VM binary.** dlopen-at-first-use keeps
  the dependency lazy and the feature harmless anywhere it's unused.
- **Don't auto-copy heap arrays under the covers.** Address-of-heap-data
  handed across a send boundary is exactly the movable-memory bug class
  the NativeBuffer rule exists to prevent; copies stay explicit
  (`asNative`), residence stays visible in the types.

## Cross-references

- `docs/FFI.md` — the fixed-arity/register-marshaling contract this
  design lives inside (§6.2 variadic exclusion; the ret/args tokens).
- `docs/SIMD.md` — the in-VM NEON tier this complements (level 2's
  FloatArray kernels; the q-pool residency design that remains parked).
- `world/61_posix_io.mst` — the GC-stable-memory rule and the mmap/
  binding conventions this follows.
- `runtime/ffi.rs` — the dlsym-per-call scope cut (U1) and the
  guest-fatal error convention (f91a8b8) the >8-arg fix extends.
