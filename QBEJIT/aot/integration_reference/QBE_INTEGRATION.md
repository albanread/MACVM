# QBE Integration Notes

## Overview

This document describes how QBE (Quick Backend) is integrated into the FasterBASIC compiler to generate native executables.

## QBE Location

QBE source code is vendored in the `qbe/` directory. It was cloned from:
```
git://c9x.me/qbe.git
```

## Building QBE

QBE is built as part of the project:

```bash
cd qbe
make
```

This produces the `qbe/qbe` executable which compiles QBE IL (.ssa files) to assembly.

## QBE IL Format

QBE uses a simple SSA-based intermediate language. Here's a minimal example:

```ssa
# Define string constant
data $hello = { b "Hello from QBE!", b 10, b 0 }

# Define main function
export function w $main() {
@start
    # Call puts to print the string
    %r =w call $puts(l $hello)
    ret 0
}
```

### Key Concepts

1. **Sigils**:
   - `$` - Global names (functions, data)
   - `%` - Temporary values (SSA registers)
   - `@` - Block labels
   - `:` - User-defined types

2. **Types**:
   - `w` - Word (32-bit integer)
   - `l` - Long (64-bit integer)
   - `s` - Single precision float
   - `d` - Double precision float
   - `b` - Byte

3. **Instructions**:
   - Arithmetic: `add`, `sub`, `mul`, `div`, `mod`
   - Comparisons: `ceqw`, `cnew`, `csltw`, `cslew`, `csgtw`, `csgew`
   - Memory: `load{b,h,w,l}`, `store{b,h,w,l}`, `alloc4`, `alloc8`
   - Control: `jmp`, `jnz`, `ret`
   - Functions: `call`

## Compilation Pipeline

```
BASIC Source (.bas)
    ↓
Lexer (fasterbasic_lexer.cpp)
    ↓
Parser (fasterbasic_parser.cpp)
    ↓
AST (fasterbasic_ast.h)
    ↓
Semantic Analysis (fasterbasic_semantic.cpp)
    ↓
CFG (fasterbasic_cfg.cpp)
    ↓
QBE Code Generator (fasterbasic_qbe_codegen.cpp) ← NEW
    ↓
QBE IL (.ssa)
    ↓
QBE Compiler (qbe/qbe)
    ↓
Assembly (.s)
    ↓
Assembler (as/cc)
    ↓
Object File (.o)
    ↓
Linker (ld/cc) + Runtime Library
    ↓
Native Executable
```

## Testing QBE

A simple test is provided in `test_qbe_hello.ssa`:

```bash
# Compile QBE IL to assembly
./qbe/qbe test_qbe_hello.ssa > test_qbe_hello.s

# Assemble and link
cc test_qbe_hello.s -o test_qbe_hello

# Run
./test_qbe_hello
```

Expected output:
```
Hello from QBE!
```

## Target Architectures

QBE supports multiple architectures:

- **amd64_sysv** - x86-64 Linux/BSD (System V ABI)
- **amd64_apple** - x86-64 macOS
- **arm64** - ARM64/AArch64 Linux/BSD
- **arm64_apple** - ARM64 macOS (Apple Silicon)
- **rv64** - RISC-V 64-bit

Select target with `-t` flag:
```bash
./qbe/qbe -t arm64_apple input.ssa > output.s
```

On macOS, QBE automatically detects Apple Silicon and uses `arm64_apple` by default.

## Runtime Library Requirements

Since QBE generates native code, we need a C runtime library for high-level operations:

### Required Functions

1. **String Operations**:
   - `str_new()` - Create new string
   - `str_concat()` - Concatenate strings
   - `str_substr()` - Extract substring
   - `str_compare()` - Compare strings
   - `str_free()` - Free string memory

2. **Array Operations**:
   - `array_new()` - Allocate array
   - `array_get()` - Get element
   - `array_set()` - Set element
   - `array_bounds()` - Bounds checking
   - `array_free()` - Free array memory

3. **I/O Operations**:
   - `basic_print()` - Print to console
   - `basic_input()` - Read from console
   - `basic_print_newline()` - Print newline
   - `file_open()`, `file_close()`, `file_read()`, `file_write()`

4. **Type Conversions**:
   - `int_to_str()` - Integer to string
   - `float_to_str()` - Float to string
   - `str_to_int()` - String to integer
   - `str_to_float()` - String to float

5. **Memory Management**:
   - Reference counting for strings
   - Arena allocator for temporaries
   - Array memory management

## Memory Management Strategy

QBE has no garbage collector, so we implement:

1. **Reference Counting** for strings:
   - Each string has a reference count
   - `str_retain()` increments count
   - `str_release()` decrements and frees when 0

2. **Manual Management** for arrays:
   - Explicit `DIM` allocates
   - Explicit `ERASE` deallocates
   - End of scope cleanup

3. **Arena Allocation** for temporaries:
   - Temporary values in expression evaluation
   - Cleared after statement completion

## Example: BASIC to QBE IL

### BASIC Code
```basic
10 PRINT "Hello"
20 FOR I = 1 TO 10
30   PRINT I
40 NEXT I
50 END
```

### Generated QBE IL (Simplified)
```ssa
data $str_hello = { b "Hello", b 0 }

export function w $main() {
@start
    # PRINT "Hello"
    %s1 =l call $str_new(l $str_hello)
    call $basic_print(l %s1)
    call $basic_print_newline()
    
    # Allocate loop counter I
    %i_ptr =l alloc4 4
    storew 1, %i_ptr
    
@loop_check
    %i_val =w loadw %i_ptr
    %cmp =w cslew %i_val, 10
    jnz %cmp, @loop_body, @loop_end
    
@loop_body
    # PRINT I
    %i_val2 =w loadw %i_ptr
    call $basic_print_int(w %i_val2)
    call $basic_print_newline()
    
    # I = I + 1
    %i_val3 =w loadw %i_ptr
    %i_next =w add %i_val3, 1
    storew %i_next, %i_ptr
    jmp @loop_check
    
@loop_end
    ret 0
}
```

## Next Steps

1. Implement `fasterbasic_qbe_codegen.cpp`
2. Implement runtime library `runtime/basic_runtime.c`
3. Update `fbc.cpp` to support `--target=qbe`
4. Create build system integration
5. Add tests for various BASIC constructs

## References

- QBE Documentation: https://c9x.me/compile/doc/il.html
- QBE ABI Documentation: https://c9x.me/compile/doc/abi.html
- QBE Source: git://c9x.me/qbe.git