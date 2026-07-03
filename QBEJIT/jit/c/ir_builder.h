/* ir_builder.h — construct QBE functions directly, no QBE IL text.
 *
 * New file, not upstream QBE and not FasterBASIC's original JIT glue
 * (qbe_bridge.c/jit_collect.c) — written for MACVM to answer the question
 * design/direct_qbe_ir_jit.md raised and deliberately deferred: can the
 * text round-trip (frontend IR -> formatted .ssa text -> QBE's
 * tokenizer/parser -> Fn/Blk/Ins) be skipped?
 *
 * Answer: mostly yes, and it needed far less new code than that design
 * doc estimated (it assumed ~500-800 lines of new C reimplementing Ref
 * encoding, Tmp/Con array growth, and string interning from scratch).
 * Those are already solved, exported, non-static functions in util.c:
 * newtmp() assigns Ref/Tmp-array slots exactly as parse.c's own tmpref()
 * does; newcon()/getcon() do the same for constants; intern() does the
 * string table. This file's job is much narrower: replicate parsefn()'s
 * setup protocol (jit/c/parse.c, function parsefn) so a Fn* built this way
 * is indistinguishable, to every later pass, from one parse() produced --
 * same physical-register Tmp pre-population loop, same UNDEF/CON_Z
 * constant slots, same block-linking convention -- and provide a
 * qbe_ir_compile_jit() that runs the identical optimizer-pipeline-then-
 * collect sequence jit_collect.c's jit_func_cb (static, so duplicated
 * here rather than exposed) already runs on parser-built functions.
 *
 * What's deliberately out of scope for this first cut (see each build
 * function's comment for specifics): aggregate (struct-by-value)
 * parameters/arguments/returns (Oparc/Oargc, the Kc class), vararg calls,
 * NEON ops, explicit Phi node construction (mutable locals should go
 * through alloc4/8 + load/store instead -- the SAME optimizer pipeline
 * this crate already runs includes promote(), which lifts those to SSA
 * automatically; see qbe_ir_alloc()'s comment), and the ~90 "internal"
 * opcodes QBE's own parser also refuses to accept directly from text
 * (par-, arg-, and call-family opcodes have their own dedicated
 * constructors below instead; the flag/xsel/acmp/xdiv family is
 * backend-pass-internal, never frontend-constructed even in the text
 * path).
 *
 * Thread safety / calling convention: identical to qbe_bridge.c's JIT
 * entry point -- not thread-safe (QBE's global Target T, debug[], and this
 * file's own equivalents of parse.c's curb/blink/nblk statics), and any
 * call here that can reach err()/die() (most of them: newtmp/newcon on
 * capacity limits, typecheck() on malformed input, the optimizer pipeline
 * on anything typecheck() didn't already catch) MUST be wrapped in
 * rust_qbe_protected_call() exactly like qbe_compile_il_jit() already
 * must be -- see csrc/rust_bridge.c's module doc for why.
 */

#ifndef IR_BUILDER_H
#define IR_BUILDER_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

struct Fn;
struct Blk;
struct JitCollector;

/* ── Opcode table ─────────────────────────────────────────────────────
 *
 * QBE_OP_* are indices into a fixed table (ir_builder.c's op_table[]) of
 * QBE's own opcode enum values (Oadd, Osub, ...), NOT the raw enum O
 * values themselves -- ops.h's ordering is an implementation detail this
 * crate shouldn't hardcode against. The table covers every "publicly
 * nameable" opcode (QBE's own NPubOp boundary: everything up to and
 * including Onop -- the same set a .ssa text file's instruction lines can
 * name) that isn't NEON, blit, va*, or one of the aggregate-type (Kc)
 * variants of par/arg. That's a curated ~84-opcode subset: integer/float
 * arithmetic, all four class (w/l/s/d) comparisons, memory access,
 * type conversions, and stack allocation -- everything a straightforward
 * optimizing compiler needs for scalar/pointer code. NEON, aggregates,
 * and vararg calls are real gaps if MACVM needs them later; not
 * architectural blockers, just not built yet (see this file's own
 * top comment).
 *
 * Keep this list and ir_builder.c's op_table[] in exact 1:1 order --
 * QBE_OP_ADD must be index 0 and op_table[0] must be Oadd, etc. The Rust
 * side (jit/rust/src/ir.rs) mirrors this list as an enum in the same
 * order for the same reason jit_collect.h's instruction-kind constants
 * are mirrored rather than shared via a generated binding: one canonical
 * C-side source of truth, hand-kept in sync on the Rust side, matching
 * this crate's existing convention (see ffi.rs's own module doc).
 */
enum {
	QBE_OP_ADD, QBE_OP_SUB, QBE_OP_NEG, QBE_OP_DIV, QBE_OP_REM,
	QBE_OP_UDIV, QBE_OP_UREM, QBE_OP_MUL, QBE_OP_AND, QBE_OP_OR,
	QBE_OP_XOR, QBE_OP_SAR, QBE_OP_SHR, QBE_OP_SHL,

	QBE_OP_CEQW, QBE_OP_CNEW, QBE_OP_CSGEW, QBE_OP_CSGTW, QBE_OP_CSLEW,
	QBE_OP_CSLTW, QBE_OP_CUGEW, QBE_OP_CUGTW, QBE_OP_CULEW, QBE_OP_CULTW,

	QBE_OP_CEQL, QBE_OP_CNEL, QBE_OP_CSGEL, QBE_OP_CSGTL, QBE_OP_CSLEL,
	QBE_OP_CSLTL, QBE_OP_CUGEL, QBE_OP_CUGTL, QBE_OP_CULEL, QBE_OP_CULTL,

	QBE_OP_CEQS, QBE_OP_CGES, QBE_OP_CGTS, QBE_OP_CLES, QBE_OP_CLTS,
	QBE_OP_CNES, QBE_OP_COS, QBE_OP_CUOS,

	QBE_OP_CEQD, QBE_OP_CGED, QBE_OP_CGTD, QBE_OP_CLED, QBE_OP_CLTD,
	QBE_OP_CNED, QBE_OP_COD, QBE_OP_CUOD,

	QBE_OP_STOREB, QBE_OP_STOREH, QBE_OP_STOREW, QBE_OP_STOREL,
	QBE_OP_STORES, QBE_OP_STORED,
	QBE_OP_LOADSB, QBE_OP_LOADUB, QBE_OP_LOADSH, QBE_OP_LOADUH,
	QBE_OP_LOADSW, QBE_OP_LOADUW, QBE_OP_LOAD,

	QBE_OP_EXTSB, QBE_OP_EXTUB, QBE_OP_EXTSH, QBE_OP_EXTUH,
	QBE_OP_EXTSW, QBE_OP_EXTUW, QBE_OP_EXTS, QBE_OP_TRUNCD,
	QBE_OP_STOSI, QBE_OP_STOUI, QBE_OP_DTOSI, QBE_OP_DTOUI,
	QBE_OP_SWTOF, QBE_OP_UWTOF, QBE_OP_SLTOF, QBE_OP_ULTOF,
	QBE_OP_CAST, QBE_OP_COPY,

	QBE_OP_COUNT
};

/* ── Value classes (QBE's Kw/Kl/Ks/Kd, plus Kx for "void") ─────────── */
enum { QBE_K_W = 0, QBE_K_L = 1, QBE_K_S = 2, QBE_K_D = 3, QBE_K_X = -1 };

/* A QBE Ref, by value -- opaque to Rust, round-tripped through every
 * builder call that produces or consumes an SSA value. Mirrors `struct
 * Ref { uint type:3; uint val:29; }` bit-for-bit (see all.h) so it can be
 * passed across the FFI boundary as a plain `#[repr(C)]` struct instead
 * of an opaque pointer/handle -- Refs are small, copyable, and QBE itself
 * passes them by value everywhere. */
typedef struct { uint32_t bits; } QbeRef;

/* ── Function lifecycle ─────────────────────────────────────────────── */

/* Begin building a new function. `ret_cls` is QBE_K_* or QBE_K_X for a
 * void-returning function. `is_export` matches QBE IL's `export` linkage
 * keyword. Mirrors parsefn()'s setup exactly (see ir_builder.c) --
 * including the Tmp0-physical-register pre-population loop, which is not
 * optional: skipping it silently corrupts every RTmp index QBE's own
 * register allocator computes downstream.
 *
 * Not reentrant / not composable with a second in-flight
 * qbe_ir_func_begin() -- exactly like parse.c's parsefn(), which the
 * SAME shared insb/curi scratch buffer (see all.h) and this file's own
 * curb/blink/nblk equivalents assume single-function-at-a-time. The
 * caller (jit/rust/src/compile.rs's COMPILE_LOCK) already serializes
 * this for the text path; ir.rs's builder must be serialized the same
 * way -- see that file. */
struct Fn *qbe_ir_func_begin(const char *name, int ret_cls, int is_export);

/* Finish the current function: closes the last open block, allocates
 * fn->rpo (filled later by the optimizer pipeline's fillcfg(), not
 * here), and runs typecheck() -- the same validation parse() itself runs
 * on every text-parsed function (see all.h's declaration for why this is
 * callable at all: it's a linkage-only patch, not new logic). Returns
 * NULL immediately (without running typecheck) if no block was ever
 * opened or the last block has no terminator -- mirrors parsefn()'s own
 * "empty function" / "last block misses jump" checks.
 *
 * typecheck() calls err() (-> basic_exit() -> the setjmp guard) on a
 * malformed function -- e.g. a temporary used with two different
 * classes, a jnz argument of the wrong class, an unresolved block. Must
 * be called inside rust_qbe_protected_call(); see this file's top
 * comment. */
struct Fn *qbe_ir_func_end(struct Fn *fn);

/* Marks the function as non-leaf (matches parse.c's own `curf->leaf = 0`
 * on parsing a call). qbe_ir_call() does this automatically -- exposed
 * separately only in case a caller wants to force it without an actual
 * Ocall (e.g. a future tail-call construct). */
void qbe_ir_set_nonleaf(struct Fn *fn);

/* ── Blocks ──────────────────────────────────────────────────────────
 *
 * No name-based lookup (unlike parse.c's findblk(), which exists only to
 * resolve forward text references like `jmp @loop` before @loop's label
 * line has been parsed). The caller holds real Blk* handles and passes
 * them directly to qbe_ir_jmp()/qbe_ir_jnz() -- straightforward once the
 * caller already has its own CFG (which any codegen driving this API
 * necessarily does), and it means this file needs no string hash table
 * at all.
 */

/* Allocate a new block -- mirrors parse.c's `findblk()` (minus the
 * name-based hash lookup, per this section's own comment) -- but does
 * NOT make it current; call qbe_ir_blk_set_current() when ready to start
 * appending to it. This split matters: allocating a block must be safe
 * purely to get a forward-reference handle (e.g. a loop body created
 * before the preceding block has finished accumulating its own
 * instructions) without disturbing whatever block is currently being
 * filled in -- exactly like a `jmp @loop` in text IL can name a block
 * whose label the parser hasn't reached yet. The FIRST block created for
 * a function becomes fn->start automatically (linked in at allocation
 * time, regardless of when -- or whether -- it's ever made current).
 * `name` is cosmetic (block dump/debug output only, like a text IL
 * label) and may be NULL. */
struct Blk *qbe_ir_blk_new(struct Fn *fn, const char *name);

/* Switch the "current" block for subsequent instruction calls. Closes
 * whatever block was previously current first (flushes its accumulated
 * instructions from the shared scratch buffer into permanent storage --
 * mirrors parse.c's closeblk()).
 *
 * A block can be made current, filled in, and left (via a terminator or
 * a later qbe_ir_blk_set_current() call) exactly ONCE -- reopening an
 * already-closed, non-empty block to append more instructions is a
 * caller error this function detects and rejects via err(), not
 * something it silently corrupts: closeblk_()'s underlying idup() call
 * *overwrites* a block's instruction array on every close (it doesn't
 * append across separate close cycles), so silently allowing a reopen
 * would silently drop whatever the first close already committed. This
 * is the same one-shot constraint text IL itself has -- QBE's own parser
 * rejects "multiple definitions of block @%s" the same way. Making an
 * *empty* just-allocated block current for the first time is always
 * fine, including when other blocks were allocated (but not yet made
 * current) in between. */
void qbe_ir_blk_set_current(struct Fn *fn, struct Blk *b);

/* ── Constants ───────────────────────────────────────────────────────── */

/* An integer/bit-pattern constant (QBE's CBits) -- the class it's used
 * at is determined by the instruction consuming it, not fixed here
 * (matches how a bare integer literal works in QBE IL text: `%x =w add
 * 42, %y` and `%x =l add 42, %y` both just use `getcon(42, fn)`). */
QbeRef qbe_ir_con_int(struct Fn *fn, int64_t value);

/* A double-precision float constant. `flt=2` (matches Con.flt's meaning:
 * 1=print as single, 2=print as double -- irrelevant for the JIT path,
 * which never prints, but kept consistent with what parse() itself sets
 * so a constant built this way is bit-identical to one parse() would
 * have produced). */
QbeRef qbe_ir_con_double(struct Fn *fn, double value);

/* A single-precision float constant. */
QbeRef qbe_ir_con_single(struct Fn *fn, float value);

/* The address of a global symbol (QBE's CAddr), optionally offset by
 * `addend` bytes -- e.g. "the 3rd field of this struct type". Resolved
 * later by crate::linker against either the module's own symbol table
 * (another function/data object built in the same compilation) or a
 * caller-supplied RuntimeContext jump table, exactly like a `$symbol`
 * reference in QBE IL text already is -- this is the C-side source of
 * the same JIT_LOAD_ADDR/JIT_CALL_EXT symbol-name mechanism
 * jit_collect.c already implements, not a new relocation path.
 * `sym_name` is copied (via intern()); the caller does not need to keep
 * it alive after this call returns. */
QbeRef qbe_ir_con_addr(struct Fn *fn, const char *sym_name, int64_t addend);

/* ── Instructions ────────────────────────────────────────────────────── */

/* Generic 2-argument (or 1-argument, with arg1 as QBE_REF_NONE)
 * instruction: allocates a fresh destination temporary of class `cls`
 * and appends `Ins{op, cls, dest, {arg0, arg1}}` to the current block.
 * `op` is one of the QBE_OP_* constants above -- covers every
 * arithmetic/compare/convert/load op (stores use qbe_ir_store() instead,
 * since a store has no result; see below).
 *
 * Returns the fresh destination Ref. For 1-argument ops (neg, ext*,
 * trunc*, *tosi, *tof, cast, copy, load*), pass QBE_REF_NONE as arg1. */
QbeRef qbe_ir_ins(struct Fn *fn, int op, int cls, QbeRef arg0, QbeRef arg1);

/* A store: no result. `cls` selects storeb/storeh/storew/storel/
 * stores/stored via the same QBE_OP_STORE* constants as qbe_ir_ins()'s
 * `op` parameter -- pass QBE_OP_STOREW etc. directly as `op`, matching
 * QBE IL text's own `storew VALUE, ADDR` argument order (value first,
 * then address -- easy to get backwards, since most other ops read left-
 * to-right as "compute this address, load/store this value" in prose but
 * QBE's own arg order is value-then-address for stores specifically). */
void qbe_ir_store(struct Fn *fn, int op, QbeRef value, QbeRef addr);

/* Stack allocation (QBE's alloc4/8/16 -- the alignment suffix picks
 * which one; MACVM's codegen should use alloc8 for anything
 * pointer/double-width, alloc4 otherwise, alloc16 for over-aligned
 * data). Returns a fresh Kl (pointer-width) temporary holding the
 * address. `size` is a byte count, evaluated as a QBE_K_L value (matches
 * text IL's own `%p =l alloc8 %n` -- `n` is always long-typed).
 *
 * This -- not explicit Phi construction, which this file doesn't expose
 * -- is how MACVM's codegen should represent a mutable local variable:
 * alloc8 it once at function entry, store/load it like memory everywhere
 * else. qbe_ir_compile_jit() runs the exact same promote() pass (part of
 * the standard pipeline every function -- built this way or parsed from
 * text -- goes through) that lifts stack slots meeting SSA-promotion
 * criteria into real SSA temporaries automatically. Simpler to emit
 * correctly than hand-built Phi nodes, and just as fast after promote()
 * runs. */
QbeRef qbe_ir_alloc(struct Fn *fn, int align /* 4, 8, or 16 */, QbeRef size);

/* ── Function parameters ────────────────────────────────────────────── */

/* Declare the next parameter, in left-to-right order (must be called in
 * the function's entry block, before any other instruction -- matches
 * QBE IL text's own requirement that `Opar` instructions come first).
 * Returns a fresh temporary of class `cls` holding the parameter value.
 * Aggregate (struct-by-value) parameters are out of scope for this first
 * cut -- see this file's top comment. */
QbeRef qbe_ir_par(struct Fn *fn, int cls);

/* ── Calls ───────────────────────────────────────────────────────────── */

/* Declare the next call argument, in left-to-right order, immediately
 * before the qbe_ir_call() it belongs to -- mirrors QBE IL text's own
 * requirement (parserefl()'s Oarg instructions are emitted directly
 * before the Ocall). Do not interleave qbe_ir_arg() calls for two
 * different pending calls. */
void qbe_ir_arg(struct Fn *fn, int cls, QbeRef value);

/* Emit the call itself, after all its qbe_ir_arg() calls. `func` is
 * typically a qbe_ir_con_addr() (direct call to a named symbol) but can
 * be any Ref (indirect call through a computed address -- e.g. a vtable
 * slot). `ret_cls` is QBE_K_* or QBE_K_X for a void call. Marks the
 * function non-leaf. Returns the fresh destination Ref (ignore it for a
 * void call -- it will be QBE_REF_NONE). Aggregate return / vararg calls
 * are out of scope for this first cut. */
QbeRef qbe_ir_call(struct Fn *fn, QbeRef func, int ret_cls);

/* ── Terminators ─────────────────────────────────────────────────────── */

void qbe_ir_jmp(struct Fn *fn, struct Blk *target);
void qbe_ir_jnz(struct Fn *fn, QbeRef cond, struct Blk *if_true, struct Blk *if_false);
/* `cls` is QBE_K_X for a value-less return (`ret` / void function). */
void qbe_ir_ret(struct Fn *fn, int cls, QbeRef value);
void qbe_ir_hlt(struct Fn *fn);

/* ── Compile: same optimizer pipeline + JitInst collection as the text
 * path, just starting from an already-built Fn* instead of parse()'s
 * output. Duplicates jit_collect.c's jit_func_cb (static, so not
 * directly callable) -- see ir_builder.c. `jc` must already be
 * jit_collector_init()'d (same contract as qbe_compile_il_jit()); this
 * does NOT reset it, so multiple functions can be built and compiled
 * into the same collector before Rust reads the result out, matching how
 * a multi-function .qbe text file collects all its functions into one
 * JitCollector today. `target_name` is the same string
 * qbe_compile_il_jit() accepts (NULL for the build default). Frees `fn`
 * (via QBE's arena, same as the text path's freeall()) on return --
 * `fn` must not be used again after this call, success or failure.
 *
 * Must be called inside rust_qbe_protected_call(); see this file's top
 * comment. Returns QBE_OK (0) or a QBE_ERR_* code (qbe_bridge.h). */
int qbe_ir_compile_jit(struct Fn *fn, struct JitCollector *jc, const char *target_name);

/* Sentinel: QBE's own R (an invalid/absent Ref) -- pass this for a
 * missing second argument to qbe_ir_ins(), or check a return value
 * against it (e.g. qbe_ir_call()'s result for a void call). */
extern const QbeRef QBE_REF_NONE;

#ifdef __cplusplus
}
#endif

#endif /* IR_BUILDER_H */
