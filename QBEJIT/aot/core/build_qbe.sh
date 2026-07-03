#!/bin/bash
#
# build_qbe.sh
# Build the QBE backend compiler
#

set -e

echo "=== Building QBE Backend Compiler ==="

# Get script directory
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
cd "$SCRIPT_DIR"

# Detect architecture and set default target
ARCH=$(uname -m)
OS=$(uname -s)

if [ "$OS" = "Darwin" ]; then
    if [ "$ARCH" = "arm64" ]; then
        DEFAULT_TARGET="T_arm64_apple"
        ARCH_FILES="arm64/*.c"
    else
        DEFAULT_TARGET="T_amd64_apple"
        ARCH_FILES="amd64/*.c"
    fi
elif [ "$OS" = "Linux" ]; then
    if [ "$ARCH" = "aarch64" ] || [ "$ARCH" = "arm64" ]; then
        DEFAULT_TARGET="T_arm64"
        ARCH_FILES="arm64/*.c"
    elif [ "$ARCH" = "riscv64" ]; then
        DEFAULT_TARGET="T_rv64"
        ARCH_FILES="rv64/*.c"
    else
        DEFAULT_TARGET="T_amd64_sysv"
        ARCH_FILES="amd64/*.c"
    fi
else
    DEFAULT_TARGET="T_amd64_sysv"
    ARCH_FILES="amd64/*.c"
fi

echo "Detected: $OS $ARCH"
echo "Default target: $DEFAULT_TARGET"
echo ""

# Create config.h
echo "Creating config.h..."
cat > config.h << EOF
#define VERSION "dev"
#define Deftgt $DEFAULT_TARGET
EOF

# Compile QBE
echo "Compiling QBE..."
cc -std=c99 -O2 -Wall \
    *.c \
    arm64/*.c \
    amd64/*.c \
    rv64/*.c \
    -o qbe

echo ""
echo "=== Build Complete ==="
echo "QBE executable: $SCRIPT_DIR/qbe"
echo ""
