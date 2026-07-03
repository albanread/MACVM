#!/bin/bash
# Build QBE with integrated FasterBASIC frontend (Code Generator V2)

set -e

# Check for --clean flag
if [ "$1" = "--clean" ]; then
    echo "=== Cleaning build artifacts ==="
    cd "$(dirname "$0")"
    rm -rf obj/*
    rm -rf qbe_source/*.o
    rm -rf qbe_source/amd64/*.o
    rm -rf qbe_source/arm64/*.o
    rm -rf qbe_source/rv64/*.o
    rm -f fbc_qbe qbe_basic
    echo "  ✓ Clean complete"
    exit 0
fi

echo "=== Building QBE with FasterBASIC Integration (CodeGen V2) ==="

cd "$(dirname "$0")"

PROJECT_ROOT="$(pwd)"
QBE_DIR="$PROJECT_ROOT/qbe_source"
FASTERBASIC_SRC="$PROJECT_ROOT/../fsh/FasterBASICT/src"
NUM_JOBS=8

# Check if FasterBASIC sources exist
if [ ! -d "$FASTERBASIC_SRC" ]; then
    echo "Error: FasterBASIC sources not found at $FASTERBASIC_SRC"
    exit 1
fi

# Step 1: Compile FasterBASIC sources to object files
echo "Compiling FasterBASIC compiler sources..."

mkdir -p obj

# Compile each FasterBASIC C++ source in parallel
# NOTE: Using modular CFG v2 structure (February 2026 refactor)
# NOTE: Using NEW codegen_v2 (CFG-v2 compatible)
compile_source() {
    local src="$1"
    local output="obj/$(basename "$src" .cpp).o"
    clang++ -std=c++17 -O2 -I"$FASTERBASIC_SRC" -I"$FASTERBASIC_SRC/../runtime" -c "$src" -o "$output"
}

export -f compile_source
export FASTERBASIC_SRC

# Compile sources in parallel
printf '%s\n' \
    "$FASTERBASIC_SRC/fasterbasic_lexer.cpp" \
    "$FASTERBASIC_SRC/fasterbasic_parser.cpp" \
    "$FASTERBASIC_SRC/fasterbasic_semantic.cpp" \
    "$FASTERBASIC_SRC/cfg/cfg_builder_core.cpp" \
    "$FASTERBASIC_SRC/cfg/cfg_builder_blocks.cpp" \
    "$FASTERBASIC_SRC/cfg/cfg_comprehensive_dump.cpp" \
    "$FASTERBASIC_SRC/cfg/cfg_builder_jumptargets.cpp" \
    "$FASTERBASIC_SRC/cfg/cfg_builder_statements.cpp" \
    "$FASTERBASIC_SRC/cfg/cfg_builder_jumps.cpp" \
    "$FASTERBASIC_SRC/cfg/cfg_builder_conditional.cpp" \
    "$FASTERBASIC_SRC/cfg/cfg_builder_loops.cpp" \
    "$FASTERBASIC_SRC/cfg/cfg_builder_exception.cpp" \
    "$FASTERBASIC_SRC/cfg/cfg_builder_functions.cpp" \
    "$FASTERBASIC_SRC/cfg/cfg_builder_edges.cpp" \
    "$FASTERBASIC_SRC/fasterbasic_data_preprocessor.cpp" \
    "$FASTERBASIC_SRC/fasterbasic_ast_dump.cpp" \
    "$FASTERBASIC_SRC/modular_commands.cpp" \
    "$FASTERBASIC_SRC/command_registry_core.cpp" \
    "$FASTERBASIC_SRC/../runtime/ConstantsManager.cpp" \
    "$FASTERBASIC_SRC/codegen_v2/qbe_builder.cpp" \
    "$FASTERBASIC_SRC/codegen_v2/type_manager.cpp" \
    "$FASTERBASIC_SRC/codegen_v2/symbol_mapper.cpp" \
    "$FASTERBASIC_SRC/codegen_v2/runtime_library.cpp" \
    "$FASTERBASIC_SRC/codegen_v2/ast_emitter.cpp" \
    "$FASTERBASIC_SRC/codegen_v2/cfg_emitter.cpp" \
    "$FASTERBASIC_SRC/codegen_v2/qbe_codegen_v2.cpp" \
| xargs -n 1 -P "$NUM_JOBS" -I {} bash -c 'compile_source "$@"' _ {}

if [ $? -ne 0 ]; then
    echo "  ✗ FasterBASIC compilation failed"
    exit 1
fi

echo "  ✓ FasterBASIC compiled (with codegen_v2)"

# Step 2: Compile runtime library
echo "Compiling BASIC runtime library..."
RUNTIME_SRC="$PROJECT_ROOT/../fsh/runtime_stubs.c"
if [ -f "$RUNTIME_SRC" ]; then
    cc -std=c99 -O2 -c "$RUNTIME_SRC" -o "$PROJECT_ROOT/obj/runtime_stubs.o"
    if [ $? -ne 0 ]; then
        echo "  ✗ Runtime library compilation failed"
        exit 1
    fi
    echo "  ✓ Runtime library compiled"
else
    echo "  ⚠ Warning: runtime_stubs.c not found at $RUNTIME_SRC"
fi

# Step 3: Compile wrapper and frontend
echo "Compiling FasterBASIC wrapper and frontend..."

clang++ -std=c++17 -O2 -I"$FASTERBASIC_SRC" -I"$FASTERBASIC_SRC/../runtime" -I"$QBE_DIR" \
    -c "$PROJECT_ROOT/fasterbasic_wrapper.cpp" \
    -o "$PROJECT_ROOT/obj/fasterbasic_wrapper.o"

if [ $? -ne 0 ]; then
    echo "  ✗ Wrapper compilation failed"
    exit 1
fi

clang++ -std=c++17 -O2 -I"$FASTERBASIC_SRC" -I"$FASTERBASIC_SRC/../runtime" -I"$QBE_DIR" \
    -c "$PROJECT_ROOT/basic_frontend.cpp" \
    -o "$PROJECT_ROOT/obj/basic_frontend.o"

if [ $? -ne 0 ]; then
    echo "  ✗ Frontend compilation failed"
    exit 1
fi

echo "  ✓ Wrapper and frontend compiled"

# Step 4: Copy runtime files to local directory
echo "Copying runtime files..."
RUNTIME_SRC_DIR="$PROJECT_ROOT/../fsh/FasterBASICT/runtime_c"
RUNTIME_DEST="$PROJECT_ROOT/runtime"

mkdir -p "$RUNTIME_DEST"

if [ -d "$RUNTIME_SRC_DIR" ]; then
    cp "$RUNTIME_SRC_DIR"/*.c "$RUNTIME_DEST/" 2>/dev/null || true
    cp "$RUNTIME_SRC_DIR"/*.h "$RUNTIME_DEST/" 2>/dev/null || true
    echo "  ✓ Runtime files copied to runtime/"
else
    echo "  ⚠ Warning: Runtime source not found at $RUNTIME_SRC_DIR"
fi

# Step 5: Build QBE object files
echo "Building QBE object files..."
cd "$QBE_DIR"

# Configure QBE if needed
if [ ! -f "config.h" ]; then
    ARCH=$(uname -m)
    OS=$(uname -s)

    if [ "$OS" = "Darwin" ]; then
        if [ "$ARCH" = "arm64" ]; then
            DEFAULT_TARGET="T_arm64_apple"
        else
            DEFAULT_TARGET="T_amd64_apple"
        fi
    else
        if [ "$ARCH" = "aarch64" ] || [ "$ARCH" = "arm64" ]; then
            DEFAULT_TARGET="T_arm64"
        elif [ "$ARCH" = "riscv64" ]; then
            DEFAULT_TARGET="T_rv64"
        else
            DEFAULT_TARGET="T_amd64_sysv"
        fi
    fi

    cat > config.h << EOF
#define VERSION "qbe+fasterbasic"
#define Deftgt $DEFAULT_TARGET
EOF
    echo "  ✓ Generated config.h (target: $DEFAULT_TARGET)"
fi

# Compile QBE C sources in parallel
echo "  Compiling QBE core..."
printf '%s\n' \
    main.c parse.c ssa.c live.c copy.c fold.c simpl.c ifopt.c gcm.c gvn.c \
    mem.c alias.c load.c util.c rega.c emit.c cfg.c abi.c spill.c \
| xargs -n 1 -P "$NUM_JOBS" -I {} cc -std=c99 -O2 -c {}

if [ $? -ne 0 ]; then
    echo "  ✗ QBE core compilation failed"
    exit 1
fi

# Compile architecture-specific sources in parallel
echo "  Compiling architecture backends..."
(cd amd64 && ls *.c | xargs -n 1 -P "$NUM_JOBS" -I {} cc -std=c99 -O2 -c {})
if [ $? -ne 0 ]; then
    echo "  ✗ AMD64 backend compilation failed"
    exit 1
fi

(cd arm64 && ls *.c | xargs -n 1 -P "$NUM_JOBS" -I {} cc -std=c99 -O2 -c {})
if [ $? -ne 0 ]; then
    echo "  ✗ ARM64 backend compilation failed"
    exit 1
fi

(cd rv64 && ls *.c | xargs -n 1 -P "$NUM_JOBS" -I {} cc -std=c99 -O2 -c {})
if [ $? -ne 0 ]; then
    echo "  ✗ RV64 backend compilation failed"
    exit 1
fi

echo "  ✓ QBE objects built"

# Step 6: Link everything together into the final compiler executable
echo "Linking fbc_qbe compiler..."

clang++ -O2 -o "$PROJECT_ROOT/fbc_qbe" \
    main.o parse.o ssa.o live.o copy.o fold.o simpl.o ifopt.o gcm.o gvn.o \
    mem.o alias.o load.o util.o rega.o emit.o cfg.o abi.o spill.o \
    amd64/*.o \
    arm64/*.o \
    rv64/*.o \
    "$PROJECT_ROOT/obj"/*.o

if [ $? -ne 0 ]; then
    echo "  ✗ Linking failed"
    exit 1
fi

# Create symlink for backward compatibility
cd "$PROJECT_ROOT"
ln -sf fbc_qbe qbe_basic

echo ""
echo "=== Build Complete ==="
echo "Executable: $PROJECT_ROOT/fbc_qbe"
echo "Symlink:    $PROJECT_ROOT/qbe_basic -> fbc_qbe (for backward compatibility)"
echo ""
echo "Build script:"
echo "  ./build_qbe_basic.sh                   # Build compiler"
echo "  ./build_qbe_basic.sh --clean           # Clean build artifacts"
echo ""
echo "Usage:"
echo "  ./fbc_qbe input.bas                    # Compile to executable (default: 'input')"
echo "  ./fbc_qbe input.bas -o program         # Compile to named executable"
echo "  ./fbc_qbe input.bas -i                 # Generate QBE IL only (to stdout)"
echo "  ./fbc_qbe input.bas -i -o output.qbe   # Generate QBE IL to file"
echo "  ./fbc_qbe input.bas -c -o output.s     # Generate assembly only"
echo "  ./fbc_qbe input.bas -G                 # Trace CFG construction and exit"
echo ""
echo "  (or use ./qbe_basic for backward compatibility)"
echo ""
echo "Note: Runtime library will be built automatically on first compilation"
echo "      and cached in runtime/.obj/ for faster subsequent builds."
echo ""
echo "You can now test with:"
echo "  ./fbc_qbe test_hello.bas"
echo "  ./test_hello"
echo ""
