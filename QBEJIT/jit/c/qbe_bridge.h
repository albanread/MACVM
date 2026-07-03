/* qbe_bridge.h — C API for embedding QBE in the FasterBASIC Zig compiler
 *
 * This replaces QBE's main() with a library interface so the Zig compiler
 * can compile QBE IL to assembly in-process, without shelling out.
 *
 * Usage from Zig:
 *   const result = qbe_compile_il(il_ptr, il_len, asm_path, target);
 *   if (result != 0) { ... handle error ... }
 *
 * The bridge owns all QBE global state (Target T, debug flags, etc.)
 * that would normally live in main.c.
 */

#ifndef QBE_BRIDGE_H
#define QBE_BRIDGE_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ── Target names (match QBE's Target.name strings) ─────────────────────
 *
 *   "amd64_sysv"   — x86-64 System V ABI (Linux, FreeBSD, etc.)
 *   "amd64_apple"  — x86-64 macOS
 *   "arm64"        — AArch64 Linux
 *   "arm64_apple"  — AArch64 macOS (Apple Silicon)
 *   "rv64"         — RISC-V 64-bit
 *   NULL           — use compile-time default for the host platform
 */

/* Return codes */
#define QBE_OK              0
#define QBE_ERR_OUTPUT     -1   /* Cannot open/write output file */
#define QBE_ERR_INPUT      -2   /* Cannot create input stream from IL text */
#define QBE_ERR_TARGET     -3   /* Unknown target name */
#define QBE_ERR_PARSE      -4   /* QBE IL parse error (fatal) */

/* ── Primary API ────────────────────────────────────────────────────────
 *
 * qbe_compile_il()
 *
 *   Compiles a QBE IL text buffer to an assembly file.
 *
 *   il_text       — pointer to the QBE IL source text (need not be NUL-terminated)
 *   il_len        — length of il_text in bytes
 *   asm_path      — output file path for the generated assembly
 *   target_name   — one of the target name strings above, or NULL for host default
 *
 *   Returns QBE_OK (0) on success, negative error code on failure.
 *
 *   Thread safety: NOT thread-safe. QBE uses extensive global state.
 *   Call from a single thread only.
 */
int qbe_compile_il(const char *il_text, size_t il_len,
                   const char *asm_path, const char *target_name);

/* ── Convenience: compile IL to an already-open FILE* ───────────────────
 *
 * qbe_compile_il_to_file()
 *
 *   Same as qbe_compile_il() but writes assembly to an open FILE* instead
 *   of a path. The caller is responsible for opening/closing the FILE*.
 *   Useful for writing to stdout or to a tmpfile().
 */
int qbe_compile_il_to_file(const char *il_text, size_t il_len,
                           void *output_file, const char *target_name);

/* ── Query: get the default target name for this build ──────────────────
 *
 * Returns a static string like "arm64_apple" — never NULL.
 */
const char *qbe_default_target(void);

/* ── Query: list available targets ──────────────────────────────────────
 *
 * Returns a NULL-terminated array of target name strings.
 * The array and strings are static and must not be freed.
 */
const char **qbe_available_targets(void);

/* ── Version string ─────────────────────────────────────────────────────
 *
 * Returns the QBE version string (includes "qbe+fasterbasic-zig").
 */
const char *qbe_version(void);

/* ── JIT compilation API ────────────────────────────────────────────────
 *
 * qbe_compile_il_jit()
 *
 *   Compiles QBE IL text through the full optimization pipeline but
 *   instead of emitting assembly text, populates a JitCollector with
 *   structured JitInst records that can be consumed by the Zig ARM64
 *   encoder (jit_encode.zig) to produce machine code in memory.
 *
 *   il_text       — pointer to the QBE IL source text
 *   il_len        — length of il_text in bytes
 *   jc            — pointer to an initialized JitCollector
 *   target_name   — target name string, or NULL for host default
 *
 *   Returns QBE_OK (0) on success, negative error code on failure.
 *   The JitCollector must have been initialized with jit_collector_init()
 *   before calling this function.
 *
 *   Thread safety: NOT thread-safe (same as qbe_compile_il).
 */
struct JitCollector;

int qbe_compile_il_jit(const char *il_text, size_t il_len,
                       struct JitCollector *jc, const char *target_name);

/* ── QBE JIT cleanup after aborted compilation ─────────────────────────
 *
 * When basic_exit() fires during QBE compilation (from err(), die_(), or
 * an assert), the longjmp skips all cleanup in qbe_compile_il_jit().
 * Call this from the recovery path to:
 *
 *   1. Close the fmemopen FILE handle that parse() was reading from.
 *   2. Call freeall() to release QBE's pool allocator memory.
 *   3. Reset the bridge_jc pointer so the next compilation starts clean.
 *
 * Safe to call even when no compilation was in progress.
 */
void qbe_jit_cleanup(void);

#ifdef __cplusplus
}
#endif

#endif /* QBE_BRIDGE_H */