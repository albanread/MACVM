# QBE Backend Extensions and Modifications

**Status:** Implementation Complete  
**Date:** February 2026  
**Component:** `qbe_basic_integrated/qbe_source`

## Overview

FasterBASIC uses a modified version of the [QBE Compiler Backend](https://c9x.me/compile/) (Small C Compiler Version). While we maintain compatibility with the core QBE IL (Intermediate Language), we have significantly extended the ARM64 backend to support high-performance BASIC features, particularly for SIMD array operations and modern processor optimizations.

This document details the specific changes made to the QBE codebase available in `qbe_basic_integrated/qbe_source/`.

## 1. NEON SIMD Extensions (ARM64)

We have extended the QBE intermediate language with custom opcodes to support the ARM64 NEON instruction set. These allow the frontend to emit vectorized loops directly.

### New Opcodes

The following opcodes were added to `ops.h` and the parser:

| Opcode | Description | ARM64 Instruction |
|---|---|---|
| `neonldr` | Load 128-bit vector | `ldr qN, [addr]` |
| `neonstr` | Store 128-bit vector | `str qN, [addr]` |
| `neonldr2`| Load 2 interleaved vectors | `ld2 {vN.2d, vM.2d}, [addr]` |
| `neonstr2`| Store 2 interleaved vectors | `st2 {vN.2d, vM.2d}, [addr]` |
| `neonldr3`| Load 3 vectors (for FMA) | `ldr` (optimized sequence) |
| `neonadd` | Vector float addition | `fadd vN.xs, vM.xs, vK.xs` |
| `neonsub` | Vector float subtraction| `fsub` |
| `neonmul` | Vector float multiply | `fmul` |
| `neonaddv`| Vector reduction (add) | `faddp` / `fadd` |

### Arrangement Support

Modifications in `arm64/emit.c` support various NEON data arrangements. The backend maps internal arrangement codes to assembly suffixes:

- `.4s` (4x SINGLE)
- `.2d` (2x DOUBLE)
- `.8h` (8x SHORT) - **Added for Phase 3**
- `.16b` (16x BYTE) - **Added for Phase 3**

### Register Reservation

To facilitate NEON operations without complex register allocation changes, we reserved the top three NEON registers in `arm64/targ.c`:

- `V28`, `V29`, `V30`: Reserved as scratch registers for NEON macros. `arm64/all.h` number of floating point registers (`NFPS`) was adjusted to exclude these.

## 2. ARM64 Instruction Fusion & Optimizations

We implemented several "peephole" optimizations and instruction fusion passes in `arm64/emit.c` to generate production-quality machine code.

### Multiply-Add (MADD/MSUB) Fusion
Merges separate multiply and add instructions into a single hardware instruction.
- **Pattern:** `T = A * B; R = T + C` → `madd R, A, B, C`
- **Safety:** Includes `prev_result_used_later()` check to ensure `T` isn't needed elsewhere before fusing.
- **Impact:** Reduces instruction count and latency in math-heavy code.

### Compare-Branch Fusion (CBZ/CBNZ)
Optimizes comparisons against zero.
- **Pattern:** `cmp R, 0; beq Label` → `cbz R, Label`
- **Pattern:** `cmp R, 0; bne Label` → `cbnz R, Label`
- **Gain:** Eliminates the `cmp` instruction entirely.

### Load/Store Pair (LDP/STP)
Fuses sequential memory accesses to adjacent addresses.
- **Prologue/Epilogue:** Pairs distinct `str`/`ldr` of callee-save registers into `stp`/`ldp`.
- **Stack Access:** Merges spills and reloads.
- **Impact:** ~50% reduction in memory instruction count for function entry/exit.

### Indexed Addressing
Fuses address calculation into the load/store instruction.
- **Pattern:** `add R_addr, R_base, R_index; ldr R_val, [R_addr]`
- **Result:** `ldr R_val, [R_base, R_index]`
- **Gain:** Eliminates the `add` instruction and reduces register pressure.

## 3. Core Modifications

### Removal of `select` Instruction
The vanilla QBE `select` instruction (conditional move) was removed and replaced with explicit branch-based patterns in our fork. This ensures consistent behavior for complex types and simplifies the status of conditional execution.

### Precision Fixes
- **Integer to Double:** Fixed precision loss in `INT` -> `DOUBLE` conversion. Previously routed through `float` (23-bit mantissa), causing data loss for integers > 16,777,216. Now uses direct `scvtf` to double precision.

## 4. Runtime & Plugin Integration

### Main Driver (`main.c`) changes
- **Plugin Linking:** Added logic to link against dynamic plugin libraries (`.so`/`.dylib`/`.dll`) located in `plugins/enabled/`.
- **Runtime Compilation:** Extended build process to compile and link auxiliary C runtime files defined by plugins.

### Lexer Generation
- **`tools/lexh_neon.c`**: A new tool was added to generate the perfect hash functions for the extended keyword set (NEON opcodes), ensuring the lexer remains fast despite the added instructions.

## Summary of Files Modified

- `qbe_source/ops.h`: Opcode definitions.
- `qbe_source/parse.c`: Keyword registration.
- `qbe_source/arm64/emit.c`: Code emission, NEON macros, Peephole optimizations.
- `qbe_source/arm64/isel.c`: Instruction selection for new ops.
- `qbe_source/arm64/targ.c`: Register reservation.
- `qbe_source/tools/lexh_neon.c`: Lexer hash generation.
