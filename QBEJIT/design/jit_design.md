# FasterBASIC JIT Design

## Goals
The primary goal of the JIT (Just-In-Time) compilation mode in FasterBASIC is to enable immediate execution of FasterBASIC programs, bypassing the traditional edit-compile-link-run cycle.

### Specific Objectives
1.  **Immediate Execution**: The compiler should be able to compile source code into machine code in memory and execute it immediately within the same process.
2.  **No External Dependencies**: The JIT mode must not rely on external system tools such as the system assembler (`as`) or linker (`ld`).
3.  **In-Memory Compilation**: Avoid creating intermediate object files (`.o`) or executable files (`.exe` / `a.out`) on disk during the JIT process.

## Architecture
The JIT architecture bridges the existing QBE-based backend with a new in-memory execution engine.

### Data Flow
1.  **Parsing & Semantics**: The existing Zig frontend parses BASIC source and performs semantic analysis.
2.  **IL Generation**: The compiler generates QBE Intermediate Language (IL) as usual.
3.  **QBE Optimization**: The QBE backend optimizes the IL and performs register allocation.
4.  **Instruction Collection**: Instead of emitting text assembly, the backend uses `jit_collect.h` to emit a flat array of `JitInst` records. This intermediate form captures the complete instruction stream in a structured, analyzable format.
5.  **Analysis & Reporting**: Prior to encoding, the compiler can traverse this `JitInst` array to generate detailed reports. This allows for:
    *   **Code Review**: Human-readable dumping of the generated instructions.
    *   **Validation**: verification of the instruction sequence.
    *   **Optimization Analysis**: collecting metrics like instruction counts and register usage.
6.  **Code Generation**: The Zig runtime reads the validated `JitInst` array and encodes machine instructions directly into executable memory.
7.  **Execution**: The system jumps to the entry point of the generated code.

## Relocation & Encoding Strategy

To support in-memory execution without an external linker, we will implement a **Two-Pass JIT Compilation Strategy** with a **Trampoline Island** for external calls. Be mindful of macOS W^X (Write XOR Execute) protection: all code emission and patching must occur in a `PROT_READ | PROT_WRITE` buffer before finalized to `PROT_READ | PROT_EXEC`.

### **1. The Two-Pass Approach**

1.  **Pass 1 (Emission & Recording)**:
    *   Allocate a writable buffer (`RW-`).
    *   Iterate through the IR/Assembly instructions.
    *   Keep track of the current `PC` (relative to buffer start).
    *   **Known Targets**: If a branch target (backward branch) is known, encode the offset immediately.
    *   **Unknown Targets**: If a target is unknown (forward branch, external symbol, data address), emit a **placeholder instruction (NOP or template)** and record a `RelocationEntry { offset, type, target_symbol_id }` in a fixup list.
    *   Record label positions as they are defined.

3.  **Pass 3 (Cleanup & Metadata)**:
    *   Free temporary buffers (if any).
    *   (Optional) Generate Source Map for debugging.

### **2. Branch Relocations (B / BL / B.cond / CBZ)**

*All ARM64 branch offsets are encoded as standard 2's complement immediate values representing the number of 4-byte instructions (i.e., `offset / 4`).*

| Instruction | Max Range | Encoding Logic |
| :--- | :--- | :--- |
| **B / BL** | ±128 MB | `imm26 = (delta >> 2) & 0x03FFFFFF`<br>`instr |= imm26` |
| **B.cond** | ±1 MB | `imm19 = (delta >> 2) & 0x7FFFF`<br>`instr |= (imm19 << 5)` |
| **CBZ / CBNZ**| ±1 MB | `imm19 = (delta >> 2) & 0x7FFFF`<br>`instr |= (imm19 << 5)` |
| **TBZ / TBNZ**| ±32 KB | `imm14 = (delta >> 2) & 0x3FFF`<br>`instr |= (imm14 << 5)` |

**Out-of-Range Handling**:
If `delta` exceeds the specific range during Pass 2, the JIT must abort or fallback (unlikely for typical basic blocks). For global `CALL` situations exceeding 128MB, we use the **Trampoline Strategy** below.

### **3. Data Relocations (ADRP + ADD)**

To load global data addresses, we use PC-relative addressing. The `ADRP` instruction computes the address of the 4KB page containing the target, relative to the page containing the PC.

**Formula**:
1.  `P_PC = AddressOf(Instruction) & ~0xFFF` (Page of PC)
2.  `P_Target = AddressOf(Data) & ~0xFFF` (Page of Target)
3.  `PageDelta = (P_Target - P_PC) >> 12`
4.  `OffsetIntoPage = AddressOf(Data) & 0xFFF`

**Encoding**:
*   **ADRP (Address Page)**:
    *   `immlo (2 bits) = PageDelta & 3`
    *   `immhi (19 bits) = (PageDelta >> 2) & 0x7FFFF`
    *   `instr |= (immlo << 29) | (immhi << 5)`
*   **ADD (Address Offset)**:
    *   `instr |= (OffsetIntoPage << 10)` (assuming standard `ADD Xd, Xn, #imm` form)
    *   For `LDR/STR`, encode `OffsetIntoPage` into the immediate field scaled by size (if aligned).

### **4. External Symbol Relocations (Trampoline Island)**

Since external C library functions (e.g., `printf`) can be located anywhere in the 64-bit address space (far beyond the ±128MB range of a `BL`), we cannot validly emit `BL _printf` directly.

**Design**:
1.  **Trampoline Area**: Reserve a section at the *end* of the JIT code buffer (guaranteed to be within +128MB of the code).
2.  **Stub Structure**: For every unique external symbol, generate a 16-byte stub in the island:
    ```assembly
    LDR x16, 8      ; Load address from [PC + 8] into temporary register x16 / kp
    BR x16          ; Branch to register
    .quad 0x...     ; The actual 64-bit absolute address of external symbol
    ```
3.  **Code Emission**:
    *   When the JIT encounters `CALL_EXT _printf`, it emits a standard `BL` to the *local trampoline stub* for `printf`.
    *   Pass 2 fixes the `BL` offset to point to the stub.
    *   Pass 2 writes the absolute address of `printf` into the `.quad` slot of the stub.

**Implementation Note — Runtime Integration (Completed)**:

The original design proposed passing a `RuntimeContext*` struct pointer to JIT'd code.  The actual implementation takes a simpler and faster approach: the entire BASIC runtime is **linked directly into the `fbc` compiler binary**, so all runtime function addresses are known at link time and populated into the trampoline island's `.quad` slots without any indirection.

Symbol resolution order for populating trampoline addresses:
1. **Jump table** (fast path): `jit_stubs.zig` builds a table of 200+ `{name, address}` pairs at process startup, where each address is obtained via `@intFromPtr(&extern_fn)` against the linked-in runtime.
2. **`dlsym(RTLD_DEFAULT, name)`** fallback: For any symbol not in the jump table. On macOS, retries with underscore-prefixed name (`_symbol`).
3. **Unresolved**: Logged as a diagnostic; trampoline target left as zero (will fault cleanly if called).

Key files:
- `jit_stubs.zig` — 200+ extern declarations, `entry_names[]` → address mapping, `buildJitRuntimeContext()`
- `hashmap_runtime.c` — Native C hashmap (the QBE IL version `hashmap.qbe` is only used in AOT; JIT needs in-process symbols)
- `runtime_shims.c` — Non-inline wrappers for `static inline` header functions and legacy-name aliases
- `build.zig` — Links 20 Zig runtime `.a` libs + 4 C runtime files into `fbc`; sets `exe.rdynamic = true`
The QBE backend (`arm64_emitfn`) provides a structured Control Flow Graph (CFG) of `Blk` objects which maps directly to the JIT intermediate form.

### Label Extraction
We do not need to parse text output. We traverse the QBE `Blk` list:
- **Block Start**: `emit(JIT_LABEL, .target_id = b->id)`. Every basic block is potentially a jump target.

## Error Handling & Debugging

### Error Propagation
JIT compilation is a fallible process. All major steps must return a `JitResult` or error code.
*   **AllocError**: Failed to `mmap` required memory.
*   **LimitError**: Jump target out of range (unlikely with trampoline, but possible).
*   **OpcodeError**: Unsupported instruction or invalid operand.

### Source Mapping
To support debugging runtime crashes, the JIT will generate a sidecar "Source Map" table during emission.
*   **Structure**: `struct { uint32_t pc_offset; uint32_t source_line; }`
*   **Usage**: On a signal (segfault/illegal instruction), a custom signal handler can check if `PC` is within the JIT buffer range. If so, it performs a binary search on the map to find the nearest BASIC source line and print a friendly error message.

## Lifecycle Management
To prevent memory leaks, especially in REPL or server scenarios:
1.  **Context Handle**: The JIT returns a transparent `JitContext*` handle upon success.
2.  **Teardown**: A `jit_free(JitContext*)` function must be implemented to `munmap` the allocated code and data pages and free the trampoline stubs.

## Platform Specifics
While the initial focus is macOS Apple Silicon, the abstractions should adhere to OS-specific protections:
*   **macOS (ARM64)**: Use `MAP_JIT` and `pthread_jit_write_protect_np`.
*   **Linux**: Use `mmap(PROT_READ|PROT_WRITE)` -> Write -> `mprotect(PROT_READ|PROT_EXEC)`. Ensure `__clear_cache` or equivalent instruction cache invalidation is called.
*   **Windows**: Use `VirtualAlloc` with `PAGE_EXECUTE_READWRITE` (simpler, but less secure) or dual-mapping if strict.

### Branch Generation
- **Unconditional (`Jjmp`)**: If the target block `b->s1` is NOT the immediate next block in memory (`b->link`), emit `JIT_B` to `b->s1->id`. Otherwise, do nothing (fallthrough).

## Error Handling & Debugging

### Error Propagation
JIT compilation is a fallible process. All major steps must return a `JitResult` or error code.
*   **AllocError**: Failed to `mmap` required memory.
*   **LimitError**: Jump target out of range (unlikely with trampoline, but possible).
*   **OpcodeError**: Unsupported instruction or invalid operand.

### Source Mapping
To support debugging runtime crashes, the JIT will generate a sidecar "Source Map" table during emission.
*   **Structure**: `struct { uint32_t pc_offset; uint32_t source_line; }`
*   **Usage**: On a signal (segfault/illegal instruction), a custom signal handler can check if `PC` is within the JIT buffer range. If so, it performs a binary search on the map to find the nearest BASIC source line and print a friendly error message.

## Lifecycle Management
To prevent memory leaks, especially in REPL or server scenarios:
1.  **Context Handle**: The JIT returns a transparent `JitContext*` handle upon success.
2.  **Teardown**: A `jit_free(JitContext*)` function must be implemented to `munmap` the allocated code and data pages and free the trampoline stubs.

## Platform Specifics
While the initial focus is macOS Apple Silicon, the abstractions should adhere to OS-specific protections:
*   **macOS (ARM64)**: Use `MAP_JIT` and `pthread_jit_write_protect_np`.
*   **Linux**: Use `mmap(PROT_READ|PROT_WRITE)` -> Write -> `mprotect(PROT_READ|PROT_EXEC)`. Ensure `__clear_cache` or equivalent instruction cache invalidation is called.
*   **Windows**: Use `VirtualAlloc` with `PAGE_EXECUTE_READWRITE` (simpler, but less secure) or dual-mapping if strict.
- **Conditional (`Jjf`...)**: 
    - Determine standard condition code from `jmp.type`.
    - Check fallthrough optimization:
        - If `b->link == b->s2` (False path is next): Emit `JIT_B_COND` to `b->s1` (standard).
        - If `b->link == b->s1` (True path is next): Invert condition, emit `JIT_B_COND` to `b->s2`.
        - If neither is next: Emit `JIT_B_COND` to `b->s2`, then fall through to unconditional `JIT_B` to `b->s1`.

## Data Layout & Memory Management

### 1. Segment Separation Strategy

To comply with macOS strict W^X (Write XOR Execute) policies on Apple Silicon while maintaining performance, we will adopt a **Split-Region Strategy**:

*   **Text Segment (RX/RO)**: Contains executable machine code *and* read-only literals (strings, constant pools).
    *   **Mechanism**: Allocated via `mmap` with `MAP_JIT`.
    *   **Access**: Code can read its own literals via PC-relative addressing (`ADRP`/`LDR`).
    *   **W^X Handling**: We strictly use `pthread_jit_write_protect_np` to toggle this region between Write (for emitting code/literals) and Execute/Read (for running).

*   **Data Segment (RW)**: Contains global variables, static buffers, and mutable state.
    *   **Mechanism**: Allocated via standard `mmap` (Anonymous, Private).
    *   **Access**: Addressed via `ADRP`/`ADD` or `LDR` relative to the code, assuming proximity.
    *   **Permissions**: Always `RW` (Read-Write), never Executable.

### 2. Allocation Strategy (The &plusmn;4GB Constraint)

ARM64 `ADRP` instructions have a limit of &plusmn;4GB from the program counter. To ensure our Data Segment is always reachable from our Text Segment without using slow 64-bit absolute pointers:

**Proposal: The "Buddy Allocation" Technique**

We will not rely on the OS to place two separate `mmap` calls near each other. Instead, we will reserve a contiguous virtual address space pattern:

1.  **Reserve a large block** (e.g., 256MB) using `mmap` with `PROT_NONE` to reserve address space without committing physical memory.
    *   *Hint*: `VM_FLAGS_ANYWHERE` is default, but reserving a chunk guarantees relative offsets.
2.  **Commit Code Region**:
    *   `mmap` the first half (128MB) with `MAP_JIT | MAP_FIXED` over the reserved space.
3.  **Commit Data Region**:
    *   `mmap` the second half (128MB) with `MAP_ANON | MAP_FIXED` (no `MAP_JIT`) over the reserved space.

**Diagram:**
```text
[      Virtual Address Space Reservation (256MB)      ]
|--------------------|--------------------------------|
|  Code (RX) + Lit   |          Data (RW)             |
|  (MAP_JIT)         |          (Standard)            |
|--------------------|--------------------------------|
^                    ^
BasePtr              BasePtr + 128MB
```

*   **Offset Calculation**: The distance from any instruction in Code to any variable in Data is constant and known at JIT-link time (approx +128MB).
*   **Safety**: This guarantees `ADRP` can always reach the data.

## Debugging & Diagnostics

To facilitate expert-level debugging of JIT-compiled code, we will implement a robust runtime diagnostic system capable of intercepting execution and inspecting machine state.

### 1. Breakpoint Mechanism
We will support dynamic insertion of breakpoints into the generated code stream.
*   **Instruction**: Use the ARM64 `BRK` (Breakpoint) instruction.
    *   Specific immediate: `BRK #0xF000` (or similar unique ID) to distinguish JIT breakpoints from system/debugger ones.
*   **API**: `jit_add_breakpoint(label_id, instruction_offset)`.
    *   **Implementation**: Calculates the memory address of the target instruction.
    *   **Patching**: Uses `pthread_jit_write_protect_np(0)` to enable writing, overwrites the instruction with `BRK`, and flushes the instruction cache.
    *   **Restoration**: Stores the original instruction in a side table to allow "step-over" or removal.

### 2. Signal Handling (The Trap)
When the CPU hits a `BRK` instruction, it raises a `SIGTRAP` signal. We will install a custom signal handler:
*   **Structure**: `void jit_signal_handler(int sig, siginfo_t *info, void *context)`.
*   **Context Capture**: The `void *context` argument is a castable `ucontext_t*`, which contains the full CPU state (registers `x0-x30`, `sp`, `pc`, `pstate`) at the moment of the trap.

### 3. State Dump & Inspection
The signal handler will provide a safe, detailed dump of the VM state:
*   **Register Dump**: Hex dump of all general-purpose and vector registers.
*   **Stack Trace**: A heuristic walk of the stack frame (using `fp` / `x29`) to show the call chain.
*   **Source Mapping**: Using the **Source Map** table (PC -> BASIC Line), identifying exactly which line of BASIC code triggered the break.
*   **Safety**: All output inside the signal handler must use `async-signal-safe` functions (e.g., `write` instead of `printf`).


### 3. JIT Module Layout & Handling `JIT_DATA`

QBE acts as a streaming compiler. It may emit data directives (`.data`, `.long`) interleaved with code. Our JIT implementation must buffer these distinct streams and place them correctly.

**The `JitModule` Structure:**

```c
typedef struct {
    uint8_t* code_base;    // Start of RX region
    size_t   code_size;    // Current offset in code
    uint8_t* data_base;    // Start of RW region
    size_t   data_size;    // Current offset in data
    
    // Fixup tracking
    // ...
} JitModule;
```

**Handling `JIT_DATA` Ops:**

When the QBE integration encounters a data emission (e.g., `emitdat` converted to `JIT_DATA_*`):

1.  **Split Stream**: The JIT compiler redirects the byte emission to `JitModule.data_base + data_size`.
2.  **Symbol Recording**: If the data has a label (e.g., a global variable name), record the symbol's address as `data_base + data_size`.
3.  **Alignment**: Apply padding to `data_size` as required by the data type (4 or 8 bytes).

**Handling Literals (RO Strings/Constants):**

1.  **Inline Placement**: Small constants or strings referenced by code can be emitted directly into the `code_base` stream (Text Segment), usually after the function body or in a "constant island" between functions.
2.  **Advantages**:
    *   Hot cache locality.
    *   Zero relocation overhead for intra-function access.
    *   Covered by `MAP_JIT` write protection automatically.

**Summary of Access Patterns:**

*   **Load String Literal**: `ADRP x0, [PC-relative-offset-to-literal-in-RX]` -> `ADD x0, x0, :lo12:offset`
*   **Load Global Var**:   `ADRP x0, [PC-relative-offset-to-DataRW]` -> `LDR x1, [x0, :lo12:offset]` (This offset will be large, e.g., 128MB + var_offset, but fits in ADPR page logic).

## ARM64 Instruction Encoder (`arm64_encoder.zig`)

The ARM64 instruction encoder is a **standalone, dependency-free Zig module** that encodes ARM64 instructions into 32-bit machine words (`u32`). It is the core of Phase 4 — the component that translates `JitInst` records into executable bytes. The encoder was originally derived from ChakraCore's C++ ARM64 encoder and has been extended to cover the full instruction repertoire that QBE's ARM64 backend emits.

### Design Principles

*   **Pure Functions**: Every `emit*` function takes register/immediate operands and returns a `u32`. No internal buffer, no side effects.
*   **Caller-Managed Buffer**: The JIT runtime writes the returned `u32` values into its executable memory region. This cleanly separates encoding from memory management and W^X concerns.
*   **Compile-Time Validation**: Many encoding constraints (immediate ranges, alignment, `NeonSize` validity masks) are checked at comptime via Zig's type system and `@compileError`.
*   **Null Sentinel for Unencodable**: Functions that may fail (e.g., out-of-range offsets) return `?u32` — the caller can detect `null` and fall back or report an error.

### Instruction Coverage

The encoder provides **300+ public `emit*` functions** organised into the following categories:

| Category | Examples | Encoder Functions |
|:---|:---|:---|
| **System / Barrier** | `NOP`, `BRK`, `DMB`, `MRS`, `MSR`, `HINT`, `BTI` | `emitNop`, `emitBrk`, `emitDmb`, `emitMrs`, `emitMsr`, `emitHint`, `emitBtiC/J/JC` |
| **Branch** | `B`, `BL`, `B.cond`, `CBZ/CBNZ`, `TBZ/TBNZ`, `BR`, `BLR`, `RET` | `emitB`, `emitBl`, `emitBCond`, `emitCbz/64`, `emitTbz`, `emitBr`, `emitBlr`, `emitRet` |
| **Integer Arithmetic** | `ADD/SUB` (reg & imm), `ADDS/SUBS`, `ADC/SBC`, `MUL`, `SDIV/UDIV`, `NEG/NEGS` | `emitAddRegister/64`, `emitSubImmediate/64`, `emitMul/64`, `emitSdiv/64`, `emitNeg/64` |
| **Fused Multiply** | `MADD`, `MSUB`, `SMULL`, `UMULL`, `SMADDL`, `UMADDL` | `emitMadd/64`, `emitMsub/64`, `emitSmull`, `emitUmull`, `emitSmaddl`, `emitUmaddl` |
| **Logical** | `AND/ORR/EOR` (reg & imm), `BIC`, `ORN`, `EON`, `ANDS/TST`, `MOV`, `MVN` | `emitAndRegister/64`, `emitOrrImmediate/64`, `emitMovRegister/64`, `emitTestRegister/64` |
| **Shift & Bitfield** | `ASR/LSL/LSR/ROR` (reg & imm), `BFM/SBFM/UBFM`, `BFI/BFXIL`, `SXTB/SXTH/SXTW`, `UXTB/UXTH`, `EXTR` | `emitAsrRegister/64`, `emitLslImmediate/64`, `emitBfi/64`, `emitSxtw64` |
| **Conditional** | `CSEL/CSINC/CSINV/CSNEG`, `CINC`, `CSET/CSETM`, `CNEG`, `CINV`, `CCMP/CCMN` | `emitCsel/64`, `emitCset/64`, `emitCcmpRegister/64` |
| **Move Immediate** | `MOVZ/MOVK/MOVN`, multi-instruction loaders | `emitMovz/64`, `emitMovk/64`, `emitLoadImmediate32`, `emitLoadImmediate64` |
| **PC-Relative** | `ADR`, `ADRP` | `emitAdr`, `emitAdrp` |
| **Load/Store (offset)** | `LDR/STR` byte/half/word/dword, signed extends, pre/post-index | `emitLdrOffset/64`, `emitStrOffset/64`, `emitLdrOffsetPostIndex/64`, `emitStrOffsetPreIndex` |
| **Load/Store (register)** | `LDR/STR` with register index, optional extend/scale | `emitLdrRegister/64`, `emitStrRegister/64`, `emitLdrsbRegister`, `emitPrfmRegister` |
| **Load/Store Pair** | `LDP/STP`, pre/post-index variants | `emitLdpOffset/64`, `emitStpOffsetPreIndex/64`, `emitLdpOffsetPostIndex/64` |
| **Atomics** | `LDAR/STLR`, `LDXR/STXR`, `LDAXR/STLXR`, `LDAXP/STLXP` | `emitLdar/64`, `emitStlr/64`, `emitLdaxr/64`, `emitStlxr/64`, `emitLdaxp/64` |
| **Bit Manipulation** | `CLZ`, `RBIT`, `REV/REV16/REV32` | `emitClz/64`, `emitRbit/64`, `emitRev/64`, `emitRev16` |
| **Scalar FP** | `FCVT` (S↔D, S↔H, D↔H), `FMADD/FMSUB`, `FNMADD/FNMSUB`, `FCMP/FCMPE`, `FCSEL` | `emitFcvtStoD`, `emitFmadd/64`, `emitFmsub/64`, `emitNeonFcmp`, `emitNeonFcsel` |
| **NEON Integer** | `ADD/SUB/MUL`, `AND/ORR/EOR`, `CMEQ/CMGE/CMGT`, `ADDP/ADDV`, `SMAX/UMIN`, `SHL/SSHR/USHR`, `DUP/INS/UMOV/SMOV`, `TBL/EXT`, `MOVI`, `ZIP/UZP/TRN` | `emitNeonAdd`, `emitNeonShl`, `emitNeonDupElement`, `emitNeonMovi`, `emitNeonExt` |
| **NEON Float** | `FADD/FSUB/FMUL/FDIV`, `FABS/FNEG/FSQRT`, `FCVTZS/FCVTZU`, `SCVTF/UCVTF`, `FMLA/FMLS`, `FMAX/FMIN` | `emitNeonFadd`, `emitNeonFabs`, `emitNeonFcvtzs`, `emitNeonScvtfVec` |
| **NEON Load/Store** | `LDR/STR` scalar (S/D/Q), `LDP/STP`, `LD1/ST1` | `emitNeonLdrOffset`, `emitNeonStpOffset`, `emitNeonLd1`, `emitNeonSt1` |
| **NEON ↔ GP** | `FMOV` (to/from general), `FCVTZS/ZU` (to general), `SCVTF/UCVTF` (from general) | `emitNeonFmovToGeneral/64`, `emitNeonFcvtzsGen/64`, `emitNeonScvtfGen/64` |
| **AES** | `AESD/AESE`, `AESIMC/AESMC` | `emitNeonAesD`, `emitNeonAesE`, `emitNeonAesImc`, `emitNeonAesMc` |
| **TLS** | `MRS/MSR TPIDR_EL0` | `emitMrsTpidrEl0`, `emitMsrTpidrEl0` |

### QBE Coverage: `JitInst` → Encoder Mapping

Every `JitInstKind` defined in `jit_collect.h` maps to one or more encoder functions. The table below shows the complete mapping:

| `JitInstKind` | Encoder Function(s) | Notes |
|:---|:---|:---|
| `JIT_NOP` | `emitNop()` | |
| `JIT_HINT` | `emitHint(imm)` | BTI via `emitBtiC/J/JC` |
| `JIT_BRK` | `emitBrk(imm)` | |
| `JIT_ADD_RRR` | `emitAddRegister/64(rd, rn, rm)` | cls selects 32/64 |
| `JIT_SUB_RRR` | `emitSubRegister/64(rd, rn, rm)` | |
| `JIT_MUL_RRR` | `emitMul/64(rd, rn, rm)` | |
| `JIT_SDIV_RRR` | `emitSdiv/64(rd, rn, rm)` | |
| `JIT_UDIV_RRR` | `emitUdiv/64(rd, rn, rm)` | |
| `JIT_AND_RRR` | `emitAndRegister/64(rd, rn, rm)` | |
| `JIT_ORR_RRR` | `emitOrrRegister/64(rd, rn, rm)` | |
| `JIT_EOR_RRR` | `emitEorRegister/64(rd, rn, rm)` | |
| `JIT_LSL_RRR` | `emitLslRegister/64(rd, rn, rm)` | |
| `JIT_LSR_RRR` | `emitLsrRegister/64(rd, rn, rm)` | |
| `JIT_ASR_RRR` | `emitAsrRegister/64(rd, rn, rm)` | |
| `JIT_NEG_RR` | `emitNeg/64(rd, rm)` | |
| `JIT_MADD_RRRR` | `emitMadd/64(rd, rn, rm, ra)` | |
| `JIT_MSUB_RRRR` | `emitMsub/64(rd, rn, rm, ra)` | |
| `JIT_ADD_RRI` | `emitAddImmediate/64(rd, rn, imm)` | |
| `JIT_SUB_RRI` | `emitSubImmediate/64(rd, rn, imm)` | |
| `JIT_ADD_SHIFT` | `emitAddRegister/64(rd, rn, shifted_rm)` | `RegisterParam.shifted(rm, shift, amt)` |
| `JIT_SUB_SHIFT` | `emitSubRegister/64(rd, rn, shifted_rm)` | |
| `JIT_AND_SHIFT` | `emitAndRegister/64(rd, rn, shifted_rm)` | |
| `JIT_ORR_SHIFT` | `emitOrrRegister/64(rd, rn, shifted_rm)` | |
| `JIT_EOR_SHIFT` | `emitEorRegister/64(rd, rn, shifted_rm)` | |
| `JIT_MOV_RR` | `emitMovRegister/64(rd, rm)` | |
| `JIT_MOVZ` | `emitMovz/64(rd, imm, hw)` | |
| `JIT_MOVK` | `emitMovk/64(rd, imm, hw)` | |
| `JIT_MOVN` | `emitMovn/64(rd, imm, hw)` | |
| `JIT_MOV_WIDE_IMM` | `emitLoadImmediate32/64(rd, value)` | Multi-instruction sequence |
| `JIT_FADD_RRR` | `emitNeonFadd(rd, rn, rm, .Size1S/.Size1D)` | Scalar FP via NEON |
| `JIT_FSUB_RRR` | `emitNeonFsub(rd, rn, rm, size)` | |
| `JIT_FMUL_RRR` | `emitNeonFmul(rd, rn, rm, size)` | |
| `JIT_FDIV_RRR` | `emitNeonFdiv(rd, rn, rm, size)` | |
| `JIT_FNEG_RR` | `emitNeonFneg(rd, rn, .Size1S/.Size1D)` | |
| `JIT_FMOV_RR` | `emitNeonFmov(rd, rn, size)` | |
| `JIT_FCVT_SD` | `emitFcvtStoD(rd, rn)` | Single→Double |
| `JIT_FCVT_DS` | `emitFcvtDtoS(rd, rn)` | Double→Single |
| `JIT_FCVTZS` | `emitNeonFcvtzsGen/64(rd, rn, size)` | FP→GP signed |
| `JIT_FCVTZU` | `emitNeonFcvtzuGen/64(rd, rn, size)` | FP→GP unsigned |
| `JIT_SCVTF` | `emitNeonScvtfGen/64(rd, rn, size)` | GP→FP signed |
| `JIT_UCVTF` | `emitNeonUcvtfGen/64(rd, rn, size)` | GP→FP unsigned |
| `JIT_FMOV_GF` | `emitNeonFmovToGeneral/64(rd, rn)` | FP reg → GP reg |
| `JIT_FMOV_FG` | `emitNeonFmovFromGeneral/64(rd, rn)` | GP reg → FP reg |
| `JIT_SXTB` | `emitSxtb/64(rd, rn)` | |
| `JIT_UXTB` | `emitUxtb/64(rd, rn)` | |
| `JIT_SXTH` | `emitSxth/64(rd, rn)` | |
| `JIT_UXTH` | `emitUxth/64(rd, rn)` | |
| `JIT_SXTW` | `emitSxtw64(rd, rn)` | |
| `JIT_CMP_RR` | `emitCmpRegister/64(rn, rm)` | `SUBS XZR, Xn, Xm` |
| `JIT_CMP_RI` | `emitCmpImmediate/64(rn, imm)` | |
| `JIT_CMN_RR` | `emitCmnRegister/64(rn, rm)` | `ADDS XZR, Xn, Xm` |
| `JIT_FCMP_RR` | `emitNeonFcmp(rn, rm, size)` | |
| `JIT_TST_RR` | `emitTestRegister/64(rn, rm)` | `ANDS XZR, Xn, Xm` |
| `JIT_CSET` | `emitCset/64(rd, cond)` | |
| `JIT_CSEL` | `emitCsel/64(rd, rn, rm, cond)` | |
| `JIT_LDR_RI` | `emitLdrOffset/64(rt, rn, imm)` | cls selects size |
| `JIT_LDRB_RI` | `emitLdrbOffset(rt, rn, imm)` | |
| `JIT_LDRH_RI` | `emitLdrhOffset(rt, rn, imm)` | |
| `JIT_LDRSB_RI` | `emitLdrsbOffset/64(rt, rn, imm)` | |
| `JIT_LDRSH_RI` | `emitLdrshOffset/64(rt, rn, imm)` | |
| `JIT_LDRSW_RI` | `emitLdrswOffset64(rt, rn, imm)` | |
| `JIT_STR_RI` | `emitStrOffset/64(rt, rn, imm)` | |
| `JIT_STRB_RI` | `emitStrbOffset(rt, rn, imm)` | |
| `JIT_STRH_RI` | `emitStrhOffset(rt, rn, imm)` | |
| `JIT_LDR_RR` | `emitLdrRegister/64(rt, rn, rm)` | Register-indexed |
| `JIT_STR_RR` | `emitStrRegister/64(rt, rn, rm)` | |
| `JIT_LDRB_RR` | `emitLdrbRegister(rt, rn, rm)` | |
| `JIT_LDRH_RR` | `emitLdrhRegister(rt, rn, rm)` | |
| `JIT_LDRSB_RR` | `emitLdrsbRegister/64(rt, rn, rm)` | |
| `JIT_LDRSH_RR` | `emitLdrshRegister/64(rt, rn, rm)` | |
| `JIT_LDRSW_RR` | `emitLdrswRegister64(rt, rn, rm)` | |
| `JIT_STRB_RR` | `emitStrbRegister(rt, rn, rm)` | |
| `JIT_STRH_RR` | `emitStrhRegister(rt, rn, rm)` | |
| `JIT_LDP` | `emitLdpOffset/64(rt1, rt2, rn, imm)` | |
| `JIT_STP` | `emitStpOffset/64(rt1, rt2, rn, imm)` | |
| `JIT_LDP_POST` | `emitLdpOffsetPostIndex/64(rt1, rt2, rn, imm)` | |
| `JIT_STP_PRE` | `emitStpOffsetPreIndex/64(rt1, rt2, rn, imm)` | |
| `JIT_B` | `emitB(offset)` | ±128 MB range |
| `JIT_BL` | `emitBl(offset)` | |
| `JIT_B_COND` | `emitBCond(cond, offset)` | ±1 MB range |
| `JIT_CBZ` | `emitCbz/64(rt, offset)` | ±1 MB range |
| `JIT_CBNZ` | `emitCbnz/64(rt, offset)` | |
| `JIT_BR` | `emitBr(rn)` | Register indirect |
| `JIT_BLR` | `emitBlr(rn)` | |
| `JIT_RET` | `emitRet(rn)` | |
| `JIT_CALL_EXT` | `emitBl(trampoline_offset)` | Via Trampoline Island |
| `JIT_ADRP` | `emitAdrp(rd, imm)` | Page-relative |
| `JIT_ADR` | `emitAdr(rd, imm)` | PC-relative |
| `JIT_NEON_LDR_Q` | `emitNeonLdrOffset(rt, rn, imm, .Size1Q)` | 128-bit load |
| `JIT_NEON_STR_Q` | `emitNeonStrOffset(rt, rn, imm, .Size1Q)` | 128-bit store |
| `JIT_NEON_ADD` | `emitNeonAdd(rd, rn, rm, arr)` | Vector add |
| `JIT_NEON_SUB` | `emitNeonSub(rd, rn, rm, arr)` | |
| `JIT_NEON_MUL` | `emitNeonMul(rd, rn, rm, arr)` | |
| `JIT_NEON_NEG` | `emitNeonNeg(rd, rn, arr)` | |
| `JIT_NEON_ABS` | `emitNeonAbs(rd, rn, arr)` | |
| `JIT_NEON_FMA` | `emitNeonFmla(rd, rn, rm, arr)` | Fused multiply-add |
| `JIT_NEON_MIN` | `emitNeonSmin` / `emitNeonFmin` | Int or FP per `is_float` |
| `JIT_NEON_MAX` | `emitNeonSmax` / `emitNeonFmax` | Int or FP per `is_float` |
| `JIT_NEON_DUP` | `emitNeonDup(rd, rn, arr)` | GP→vector broadcast |
| `JIT_NEON_ADDV` | `emitNeonAddv(rd, rn, arr)` | Horizontal reduce |
| `JIT_NEON_DIV` | `emitNeonFdiv(rd, rn, rm, arr)` | FP vector divide |

### Verification Infrastructure

The encoder includes two layers of automated verification:

1.  **Zig Unit Tests** — 309 `test` blocks compiled and run with `zig test arm64_encoder.zig`. Each test calls an `emit*` function and asserts the returned `u32` matches a known-good constant.

2.  **Clang/otool Round-Trip Driver** — 828 `VerifyCase` entries in a comptime table. The `main()` function assembles each case's assembly text with the system `clang`, extracts the opcode via `otool -tv`, and compares it to the encoder's `u32`. This ensures exact parity with the system toolchain. Run with:
    ```bash
    zig build-exe arm64_encoder.zig && ./arm64_encoder
    ```

### Logical Immediate Encoder

The encoder includes a full implementation of the ARM64 logical immediate encoding algorithm (`encodeLogicalImmediate64`), which converts arbitrary bitmask patterns into the `N:immr:imms` triple. This is critical because QBE frequently emits `AND/ORR/EOR` with immediate operands (e.g., mask operations), and ARM64's bitmask encoding is notoriously complex. The encoder correctly handles all valid patterns and returns `null` for unencodable values (0 and all-ones).

### Branch Linker

The `ArmBranchLinker` struct provides deferred branch resolution:
*   Records instruction offsets and target offsets during Pass 1.
*   `resolve()` computes the relative displacement and patches the branch immediate.
*   `linkRaw()` performs the final bit manipulation for `Imm26`, `Imm19`, and `Imm14` branch classes.

This directly implements the **Two-Pass Relocation Strategy** described earlier in this document.

---

## Automation: Instruction Encoding Verifier

Manual verification of 32-bit ARM64 instruction encoding is complex and error-prone due to varied addressing modes, shift options, and immediate encoding rules. To facilitate development and ensure correctness of the JIT encoder, we propose an automated toolchain based on `clang` / `llvm-mc` to generate "ground truth" encodings.

### Toolchain Design

The verifier uses the system assembler (specifically the LLVM machine code playground `llvm-mc` or the system `clang` assembler) to generate reference bytes.

**Workflow:**
1.  **Input**: An ARM64 assembly instruction string (e.g., `add x0, x1, #42`).
2.  **Assembly**: The tool pipes this string to `llvm-mc` with `-arch=arm64 -show-encoding`, or uses `clang -c` to produce an object file.
3.  **Extraction**: The output is parsed to extract the raw machine code bytes.
4.  **Formatting**: The bytes represent the expected output of our JIT encoder function for the given inputs.

### Proposed Script: `tools/jit_instruction_dump.sh`

A helper script `tools/jit_instruction_dump.sh` has been created to streamline this process. It automatically detects available tools (preferring `llvm-mc` from Homebrew/LLVM if available, falling back to Xcode's `clang`).

**Usage:**
```bash
./tools/jit_instruction_dump.sh "add x0, x1, lsl #2"
```

**Output Example:**
```
	.text
	add	x0, x1, x1, lsl #2      ; encoding: [0x20,0x08,0x21,0x8b]
```
The encoding bytes `[0x20,0x08,0x21,0x8b]` (little-endian: `8b210820`) can be directly copied into unit tests to verify the JIT implementation.

### Integration into Development Workflow

1.  **Unit Test Generation**: When implementing a new instruction (e.g., `Bitwise AND`), run the script with several variants:
    *   `and x0, x1, x2`
    *   `and x0, x1, #0xF`
2.  **Regression Testing**: Periodically, the script can be used to generate a large set of test cases (input assembly vs expected hex) to bulk-verify the JIT encoder logic.
3.  **Debugging**: If the JIT-generated code crashes or behaves unexpectedly, use the script to compare the JIT's output bytes against the system assembler's output for the intended instruction.

### Current Verification Status

The `arm64_encoder.zig` verification driver embeds **828 `VerifyCase` entries** covering:
*   All integer arithmetic/logical ops (register & immediate forms, with and without shifts)
*   All load/store variants (byte/half/word/dword, register-indexed, offset, pre/post-index, pair)
*   All branch types (`B`, `BL`, `B.cond`, `CBZ/CBNZ`, `TBZ/TBNZ`, `BR`, `BLR`, `RET`)
*   Scalar FP ops (`FADD/FSUB/FMUL/FDIV`, `FCMP/FCMPE`, `FCSEL`, `FCVT`, `FMADD/FMSUB`)
*   NEON vector ops (integer arithmetic, FP arithmetic, shifts, permutes, element manipulation)
*   System instructions (`MRS/MSR`, `DMB`, `BRK`, `HINT/BTI`, `TPIDR_EL0`)
*   Atomic load/store operations (`LDAR/STLR`, `LDXR/STXR`, `LDAXR/STLXR`, `LDAXP/STLXP`)

All 828 cases produce identical opcodes to the system `clang` assembler. Additionally, 309 Zig unit tests pass, covering the encoder's API surface and edge cases.

## Integration: Wiring the Encoder into the QBE JIT Pipeline

The encoder slots into the JIT pipeline at **Phase 4 (Encoding & Linking)**. The integration point is a Zig function that consumes the `JitInst` array produced by QBE's `jit_collect_fn()` and emits machine code:

```
JitInst[] ──► jit_encode() ──► u8[] (executable buffer)
```

**Proposed `jit_encode` loop (pseudocode)**:
1.  Iterate over `collector.insts[0..collector.ninst]`.
2.  For each `JitInst`, switch on `inst.kind`:
    *   Map `cls` field (`JIT_CLS_W`/`JIT_CLS_L`/`JIT_CLS_S`/`JIT_CLS_D`) to select 32-bit vs 64-bit encoder variant.
    *   Map `inst.rd/rn/rm/ra` (integer register IDs) to `Register` enum values.
    *   For NEON ops, map `inst.rd/rn/rm` to `NeonRegister` values and `JitNeonArr` to `NeonSize`.
    *   Call the corresponding `emit*` function and write the `u32` into the code buffer.
3.  For `JIT_LABEL`, record `label_id → current_offset` in the symbol table.
4.  For branches (`JIT_B`, `JIT_B_COND`, `JIT_CBZ`, etc.) with forward targets, emit a placeholder and record an `ArmBranchLinker` entry.
5.  After the loop, resolve all deferred branches via `ArmBranchLinker.resolve()` + `linkRaw()`.
6.  For `JIT_CALL_EXT`, emit `BL` to the Trampoline Island stub for the target symbol.

This architecture keeps the encoder stateless while the JIT driver manages the buffer, relocations, and memory protection.

## Implementation Status

All core JIT pipeline stages described in this design document have been implemented and tested. The following modules comprise the complete JIT subsystem:

### Implemented Modules

| Module | File | Status | Tests |
|:---|:---|:---:|:---:|
| ARM64 Encoder | `zig_compiler/src/arm64_encoder.zig` | ✅ Complete | 309 unit + 828 clang-verified |
| JitInst IR | `zig_compiler/qbe/jit_collect.h` + `jit_collect.c` | ✅ Complete | — |
| JIT Encoder | `zig_compiler/src/jit_encode.zig` | ✅ Complete | 27 |
| QBE Integration | `zig_compiler/src/qbe.zig` | ✅ Complete | 12 |
| JIT Memory | `zig_compiler/src/jit_memory.zig` | ✅ Complete | 19 |
| JIT Linker | `zig_compiler/src/jit_linker.zig` | ✅ Complete | 20 |
| JIT Runtime | `zig_compiler/src/jit_runtime.zig` | ✅ Complete | 13 |
| JIT Stubs | `zig_compiler/src/jit_stubs.zig` | ✅ Complete | 7 |
| JIT Capstone | `zig_compiler/src/jit_capstone.zig` | ✅ Complete | 17 |
| Runtime Integration | `jit_stubs.zig` + `hashmap_runtime.c` + `runtime_shims.c` | ✅ Complete | — |
| **Total** | | | **~1512** |

### Pipeline Data Flow (Implemented)

```
BASIC source
  → Zig frontend (parse, semantic)
  → QBE IL text
  → qbe_compile_il_jit()           [C — QBE parse/SSA/regalloc/isel → JitInst[]]
  → jit.jitEncode()                [Zig — JitInst[] → JitModule with code/data/fixups]
  → JitMemoryRegion.allocate()     [Zig — MAP_JIT mmap (macOS) / buddy alloc (Linux)]
  → JitLinker.link()               [Zig — copy code/data, trampolines, ADRP+ADD relocs]
  → JitMemoryRegion.makeExecutable() [Zig — W^X toggle, icache flush]
  → JitSession.execute()           [Zig — cast to fn ptr, call, return exit code]
  → JitSession.deinit()            [Zig — munmap, free all resources]
```

### Runtime Integration (Implemented)

The JIT calls the real FasterBASIC runtime functions in-process — no stubs, no dynamic loading, no context-pointer protocol. This was achieved by:

1. **Linking runtime into `fbc`**: All 20 Zig runtime libraries are compiled as static `.a` files and linked into the compiler binary via `exe.linkLibrary()`. Runtime C sources (`basic_runtime.c`, `worker_runtime.c`, `hashmap_runtime.c`, `runtime_shims.c`) are compiled directly into the `fbc` module. `exe.rdynamic = true` exports all symbols.

2. **Jump table resolution**: `jit_stubs.zig` declares 200+ `extern fn` against the linked runtime and builds a runtime-initialized jump table mapping QBE external-call names (e.g. `"_basic_print_int"`) to real function addresses via `@intFromPtr(&extern_fn)`.

3. **Native hashmap** (`runtime/hashmap_runtime.c`): The hashmap was previously only implemented in QBE IL (`hashmap.qbe`) — compiled alongside user code in the AOT path but invisible to in-process JIT. A native C implementation (314 lines) provides the same open-addressing / FNV-1a interface and data layout, making all 9 hashmap functions callable from JIT code.

4. **Shims for inline/legacy functions** (`runtime/runtime_shims.c`): Thin C wrappers for functions that are `static inline` in headers (`string_length`, `basic_len`) or exported under different names than the codegen emits (`hideCursor` → `basic_cursor_hide`, `list_erase` → `list_remove`).

5. **Symbol resolution order**: Jump table (fast O(n) scan) → `dlsym(RTLD_DEFAULT, name)` with macOS underscore prefix retry → unresolved (logged). 459 runtime symbols exported from the `fbc` binary.

### Key Design Decisions Validated

1. **Two-Pass Branch Resolution** (§2): Implemented in `jit_encode.zig` via `resolveFixups()`. Forward branches emit placeholder + `BranchFixup`, resolved after all labels are known. Verified with IF/ELSE test producing correct Imm26 and Imm19 patched opcodes.

2. **Data Relocations / ADRP+ADD** (§3): Implemented in `jit_linker.zig` via `resolveDataRelocations()`. Computes page delta and lo12 offset using real mmap'd addresses. Formula matches this document exactly.

3. **Trampoline Island** (§4): Implemented in `jit_memory.zig` (`writeTrampoline`) + `jit_linker.zig` (`buildTrampolineIsland`, `patchExternalCalls`). 16-byte stubs with `LDR X16,[PC,#8]; BR X16; .quad addr`. Symbol resolution via `RuntimeContext` jump table → `dlsym()` fallback with macOS underscore prefix retry.

4. **Split-Region Strategy** (§Data Layout): On macOS, a single `MAP_JIT` mmap is used for the entire region (code + trampolines + data) rather than the buddy allocation with MAP_FIXED overlay, because macOS disallows `MAP_FIXED | MAP_JIT` over a `PROT_NONE` reservation. The Linux path uses the buddy allocation as designed. Both guarantee ADRP reachability.

5. **W^X Compliance** (§Platform Specifics): macOS uses `pthread_jit_write_protect_np(0/1)` for thread-local W^X toggling. Linux uses `mprotect` to switch code region between RW and RX. `ProtectionState` enum tracks the current state and all write operations check it.

6. **Instruction Cache Invalidation**: macOS uses `sys_icache_invalidate()`. Linux uses the DC CVAU + IC IVAU + DSB + ISB sequence. Called automatically during `makeExecutable()`.

### Remaining Work

- ~~**CLI `--jit` flag**~~: ✅ Done — `--jit` and `--jit-verbose` route through JIT pipeline.
- ~~**Runtime function wiring**~~: ✅ Done — 200+ real runtime functions wired via jump table, native hashmap, shims for inline/legacy names.
- ~~**FUNCTION/RETURN under JIT**~~: ✅ Fixed — User-defined functions work under JIT including recursive calls. Verified with `Factorial(10)` → `3628800`.
- ~~**GOSUB/RETURN**~~: ✅ Fixed — Subroutine calls resume correctly after the GOSUB call site. Verified with simple, nested, and multi-call tests.
- ~~**SQR/math double-return**~~: ✅ Fixed — Math functions returning doubles display correctly. Fixed via FMOV FP→GP encoding correction (`is64()` not `isDouble()`).
- ~~**`--run` flag**~~: ✅ Done — `fbc --run program.bas arg1 arg2` for JIT execution with argument passthrough.
- ~~**`--metrics` flag**~~: ✅ Done — Phase timings and SAMM memory stats for JIT runs.
- ~~**`--batch-jit` flag**~~: ✅ Done — Batch test harness with recursive subdirectory support and pass/fail reporting.
- ~~**Codegen comment annotations**~~: ✅ Done — `JIT_COMMENT` pseudo-instructions preserved through encoding; Capstone disassembly shows block names, prologue/epilogue, branch targets, fusion explanations.
- ~~**Linked disassembly**~~: ✅ Done — Post-link Capstone disassembly of live code buffer with patched addresses and comment annotations.
- **Signal handler implementation**: Full `sigaction` + `ucontext_t` parsing for crash diagnostics (stubs are in place).
- **Breakpoint API**: Hot-patch `BRK` instructions using the `makeWritable()` → patch → `makeExecutable()` cycle.
- **Windows support**: `VirtualAlloc` based allocation.
- **Benchmarks**: JIT startup and execution time vs compiled binaries.


