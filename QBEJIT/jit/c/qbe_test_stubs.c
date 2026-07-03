// qbe_test_stubs.c — Stubs for extern symbols required when linking QBE C
// sources into unit-test binaries (zig build test).
//
// QBE's parse.c and util.c call basic_exit() (defined in basic_runtime.c)
// but the runtime is not linked into test modules.  This file provides
// weak stubs so the linker is satisfied.  The stubs are never actually
// called during unit tests — they exist only to resolve symbols.
//
// Marked __attribute__((weak)) so that if a test binary does link the
// real runtime, the real definitions win.

#include <stdlib.h>
#include <stdint.h>
#include <stdio.h>
#include <signal.h>

#define WEAK __attribute__((weak))

// ── basic_runtime stubs ────────────────────────────────────────────────

// QBE's err() and die_() call basic_exit() instead of exit() so the
// JIT harness can longjmp back.  In tests we just exit normally.
WEAK void basic_exit(int code) {
    exit(code);
}

// basic_runtime_init / basic_runtime_cleanup — called at program
// start/end; tests don't need the arena or file handle tracking.
WEAK void basic_runtime_init(void) {}
WEAK void basic_runtime_cleanup(void) {}

// Error reporting — tests should never trigger these, but the linker
// needs the symbols when QBE C code is present.
WEAK void basic_error(int32_t line_number, const char *message) {
    (void)line_number;
    (void)message;
    exit(1);
}

WEAK void basic_error_msg(const char *message) {
    (void)message;
    exit(1);
}

// ── JIT harness stubs ──────────────────────────────────────────────────

// basic_jit_call / basic_jit_exec — the protected-call wrappers are
// not needed during unit tests; provide no-op stubs.
WEAK int basic_jit_call(int (*callback)(void *ctx), void *ctx) {
    return callback(ctx);
}

WEAK int basic_jit_exec(void *fn_ptr, int argc, char **argv) {
    (void)fn_ptr;
    (void)argc;
    (void)argv;
    return 0;
}

// ── QBE JIT cleanup stub ──────────────────────────────────────────────

WEAK void qbe_jit_cleanup(void) {}

// ── SAMM stubs (referenced transitively) ───────────────────────────────

WEAK void samm_init(void) {}
WEAK void samm_shutdown(void) {}
WEAK int  samm_is_enabled(void) { return 0; }