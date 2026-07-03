/* rust_bridge.c — the only C symbols this crate adds on top of the
 * vendored `jit/c/` QBE-fork sources.
 *
 * QBE's parser (parse.c, util.c) calls `basic_exit(int)` on a fatal
 * parse error (malformed IL) instead of `exit(3)`. In FasterBASIC's own
 * runtime that function longjmps back to a setjmp() scope the caller
 * establishes before invoking the QBE pipeline. There is no other
 * definition of `basic_exit` anywhere in `jit/c/` — without one, the
 * linker fails; with a naive one that calls `exit()`, a malformed QBE
 * IL string handed to this crate would tear down the entire host
 * process (MACVM itself). So: implement the real protected-call
 * contract, in C, using setjmp/longjmp confined entirely to the C call
 * graph below `rust_qbe_protected_call` — this never unwinds through
 * the Rust frame that calls it, which is the standard, well-defined way
 * to bridge a C library's non-local exit into a language that has no
 * concept of longjmp (the same pattern Lua's `lua_pcall` uses).
 *
 * QBE's own internal `assert()` failures (e.g. an invariant violated by
 * a QBE optimizer bug) are NOT caught here — those raise SIGABRT
 * directly and are not routed through basic_exit(). That mirrors
 * upstream QBE's own behavior: an assert failure is "this should never
 * happen," not a recoverable input error.
 */

#include <setjmp.h>
#include <stdlib.h>
#include <stdio.h>

static _Thread_local jmp_buf *g_jit_jmp = NULL;

/* Sentinel returned by rust_qbe_protected_call() when the wrapped call
 * aborted via basic_exit() instead of returning normally. Chosen well
 * outside QBE_OK/QBE_ERR_* (-1..-4) and JitCollector's own error
 * space so callers can distinguish "longjmp fired" unambiguously. */
#define RUST_QBE_LONGJMP_SENTINEL (-12345)

void
basic_exit(int code)
{
	(void)code;
	if (g_jit_jmp) {
		longjmp(*g_jit_jmp, 1);
	}
	/* No protection scope active — every entry point in this crate
	 * that can reach basic_exit() must go through
	 * rust_qbe_protected_call() first. Reaching here is a bug in
	 * this crate, not a QBE input error; fail loudly rather than
	 * silently exiting the host process. */
	fprintf(stderr,
	    "rust_bridge: basic_exit(%d) called with no protection scope "
	    "active (missing rust_qbe_protected_call wrapper) — aborting\n",
	    code);
	abort();
}

/* Runs `compile_fn(ctx)` with a setjmp/longjmp guard around it. If QBE
 * calls basic_exit() during the call (fatal parse error), returns
 * RUST_QBE_LONGJMP_SENTINEL instead of unwinding further. Otherwise
 * returns compile_fn's own return value. Safe to nest is NOT supported
 * (QBE's global state isn't reentrant either — see qbe_bridge.h's
 * "NOT thread-safe" note); call from one thread at a time. */
int
rust_qbe_protected_call(int (*compile_fn)(void *ctx), void *ctx)
{
	jmp_buf buf;
	jmp_buf *prev = g_jit_jmp;
	int rc;

	g_jit_jmp = &buf;
	if (setjmp(buf) == 0) {
		rc = compile_fn(ctx);
	} else {
		rc = RUST_QBE_LONGJMP_SENTINEL;
	}
	g_jit_jmp = prev;
	return rc;
}
