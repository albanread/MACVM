/* ir_builder.c — implementation. See ir_builder.h for the design
 * rationale and full API documentation; comments here are about
 * mechanics, not the "why".
 *
 * New file (not upstream QBE, not FasterBASIC's qbe_bridge.c/
 * jit_collect.c). Reuses QBE's own exported-but-previously-unused-outside-
 * parse.c primitives (newtmp, newcon, intern, newblk, typecheck) rather
 * than reimplementing Ref/Tmp/Con bookkeeping — see ir_builder.h's top
 * comment for why that made this much smaller than
 * design/direct_qbe_ir_jit.md's original estimate.
 */

#include "all.h"
#include "config.h"
#include "ir_builder.h"
#include "jit_collect.h"
#include "qbe_bridge.h"
#include <string.h>

/* ── QbeRef <-> Ref: bit-for-bit reinterpret, not a translation ────────
 * Ref is `{uint type:3; uint val:29;}` — a compiler-packed 32-bit
 * bitfield. QbeRef is `{uint32_t bits;}`. Both are exactly 4 bytes on
 * every ABI this crate targets (verified below, not assumed); memcpy
 * between them preserves whatever bit pattern the compiler chose for
 * Ref's layout, so encoding/decoding is symmetric regardless of that
 * layout's specifics. QBE_REF_NONE = {0} works without needing to know
 * the layout at all: R = (Ref){RTmp, 0} and RTmp == 0, so R's bit pattern
 * is all-zero under any bitfield packing. */
MAKESURE(ref_is_4_bytes, sizeof(Ref) == sizeof(uint32_t));

const QbeRef QBE_REF_NONE = {0};

static QbeRef
qberef_from_ref(Ref r)
{
	QbeRef q;
	memcpy(&q, &r, sizeof q);
	return q;
}

static Ref
ref_from_qbe(QbeRef q)
{
	Ref r;
	memcpy(&r, &q, sizeof r);
	return r;
}

/* ── Opcode table — see ir_builder.h's enum for the ordering contract.
 * Indexed by symbolic QBE opcode name (Oadd, ...), not by hand-counted
 * ops.h position, so a future upstream ops.h reordering can't silently
 * desync this table. */
static const int op_table[QBE_OP_COUNT] = {
	[QBE_OP_ADD] = Oadd,   [QBE_OP_SUB] = Osub,   [QBE_OP_NEG] = Oneg,
	[QBE_OP_DIV] = Odiv,   [QBE_OP_REM] = Orem,   [QBE_OP_UDIV] = Oudiv,
	[QBE_OP_UREM] = Ourem, [QBE_OP_MUL] = Omul,   [QBE_OP_AND] = Oand,
	[QBE_OP_OR] = Oor,     [QBE_OP_XOR] = Oxor,   [QBE_OP_SAR] = Osar,
	[QBE_OP_SHR] = Oshr,   [QBE_OP_SHL] = Oshl,

	[QBE_OP_CEQW] = Oceqw,   [QBE_OP_CNEW] = Ocnew,   [QBE_OP_CSGEW] = Ocsgew,
	[QBE_OP_CSGTW] = Ocsgtw, [QBE_OP_CSLEW] = Ocslew, [QBE_OP_CSLTW] = Ocsltw,
	[QBE_OP_CUGEW] = Ocugew, [QBE_OP_CUGTW] = Ocugtw, [QBE_OP_CULEW] = Oculew,
	[QBE_OP_CULTW] = Ocultw,

	[QBE_OP_CEQL] = Oceql,   [QBE_OP_CNEL] = Ocnel,   [QBE_OP_CSGEL] = Ocsgel,
	[QBE_OP_CSGTL] = Ocsgtl, [QBE_OP_CSLEL] = Ocslel, [QBE_OP_CSLTL] = Ocsltl,
	[QBE_OP_CUGEL] = Ocugel, [QBE_OP_CUGTL] = Ocugtl, [QBE_OP_CULEL] = Oculel,
	[QBE_OP_CULTL] = Ocultl,

	[QBE_OP_CEQS] = Oceqs, [QBE_OP_CGES] = Ocges, [QBE_OP_CGTS] = Ocgts,
	[QBE_OP_CLES] = Ocles, [QBE_OP_CLTS] = Oclts, [QBE_OP_CNES] = Ocnes,
	[QBE_OP_COS] = Ocos,   [QBE_OP_CUOS] = Ocuos,

	[QBE_OP_CEQD] = Oceqd, [QBE_OP_CGED] = Ocged, [QBE_OP_CGTD] = Ocgtd,
	[QBE_OP_CLED] = Ocled, [QBE_OP_CLTD] = Ocltd, [QBE_OP_CNED] = Ocned,
	[QBE_OP_COD] = Ocod,   [QBE_OP_CUOD] = Ocuod,

	[QBE_OP_STOREB] = Ostoreb, [QBE_OP_STOREH] = Ostoreh,
	[QBE_OP_STOREW] = Ostorew, [QBE_OP_STOREL] = Ostorel,
	[QBE_OP_STORES] = Ostores, [QBE_OP_STORED] = Ostored,
	[QBE_OP_LOADSB] = Oloadsb, [QBE_OP_LOADUB] = Oloadub,
	[QBE_OP_LOADSH] = Oloadsh, [QBE_OP_LOADUH] = Oloaduh,
	[QBE_OP_LOADSW] = Oloadsw, [QBE_OP_LOADUW] = Oloaduw,
	[QBE_OP_LOAD] = Oload,

	[QBE_OP_EXTSB] = Oextsb, [QBE_OP_EXTUB] = Oextub,
	[QBE_OP_EXTSH] = Oextsh, [QBE_OP_EXTUH] = Oextuh,
	[QBE_OP_EXTSW] = Oextsw, [QBE_OP_EXTUW] = Oextuw,
	[QBE_OP_EXTS] = Oexts,   [QBE_OP_TRUNCD] = Otruncd,
	[QBE_OP_STOSI] = Ostosi, [QBE_OP_STOUI] = Ostoui,
	[QBE_OP_DTOSI] = Odtosi, [QBE_OP_DTOUI] = Odtoui,
	[QBE_OP_SWTOF] = Oswtof, [QBE_OP_UWTOF] = Ouwtof,
	[QBE_OP_SLTOF] = Osltof, [QBE_OP_ULTOF] = Oultof,
	[QBE_OP_CAST] = Ocast,   [QBE_OP_COPY] = Ocopy,
};

/* ── Builder state — this file's own equivalents of parse.c's static
 * curb/blink/nblk. Deliberately NOT shared with parse.c's identically-
 * named statics (which stay private to that file, untouched) — separate,
 * unrelated storage, safe because both this file and parse.c only ever
 * build one function at a time and nothing here calls into parse.c or
 * vice versa mid-build. insb/curi, unlike curb/blink/nblk, genuinely ARE
 * the shared globals declared in all.h (extern Ins insb[NIns], *curi;) —
 * reused directly rather than duplicated, since they're already exported
 * for exactly this kind of reuse and there is only ever one of them. */
static Blk *ir_curb;
static Blk **ir_blink;
static uint ir_nblk;

static void
closeblk_(Blk *b)
{
	idup(b, insb, curi - insb);
	curi = insb;
}

static void
emitfwd_(int op, int cls, Ref to, Ref arg0, Ref arg1)
{
	if (curi - insb >= NIns)
		err("ir_builder: too many instructions in block");
	curi->op = op;
	curi->cls = cls;
	curi->to = to;
	curi->arg[0] = arg0;
	curi->arg[1] = arg1;
	curi++;
}

/* ── Function lifecycle ──────────────────────────────────────────────── */

Fn *
qbe_ir_func_begin(const char *name, int ret_cls, int is_export)
{
	Fn *fn;
	int i;

	(void)ret_cls; /* not stored on Fn — see ir_builder.h's comment on
	                 * qbe_ir_func_begin: each qbe_ir_ret() call carries
	                 * its own class, exactly like each `ret` line in a
	                 * text .qbe file's Jret* jump type does. */

	ir_curb = 0;
	ir_nblk = 0;
	curi = insb;

	fn = alloc(sizeof *fn);
	fn->ntmp = 0;
	fn->ncon = 2;
	fn->tmp = vnew(fn->ntmp, sizeof fn->tmp[0], PFn);
	fn->con = vnew(fn->ncon, sizeof fn->con[0], PFn);

	/* Mirrors parsefn() exactly: pre-populate Tmp0 (NBit == 64) physical-
	 * register temp slots before any real SSA temp exists, alternating
	 * class by whether the index falls in this target's FPR range. Every
	 * later pass (register allocation especially) assumes RTmp indices
	 * below Tmp0 mean a physical register, not an SSA value — skipping
	 * this desyncs every real Ref this function goes on to create. */
	for (i = 0; i < Tmp0; ++i)
		if (T.fpr0 <= i && i < T.fpr0 + T.nfpr)
			newtmp(0, Kd, fn);
		else
			newtmp(0, Kl, fn);

	fn->con[0].type = CBits;
	fn->con[0].bits.i = 0xdeaddead; /* UNDEF, matches parsefn() */
	fn->con[1].type = CBits;        /* CON_Z: bits.i left at 0 by alloc()'s zeroing */

	memset(&fn->lnk, 0, sizeof fn->lnk);
	fn->lnk.export = is_export != 0;
	fn->leaf = 1;
	fn->retty = -1; /* no aggregate return — see ir_builder.h's scope note */
	fn->start = 0;
	ir_blink = &fn->start;

	if (name) {
		strncpy(fn->name, name, NString - 1);
		fn->name[NString - 1] = 0;
	}

	return fn;
}

void
qbe_ir_set_nonleaf(Fn *fn)
{
	fn->leaf = 0;
}

Fn *
qbe_ir_func_end(Fn *fn)
{
	Blk *b;

	if (ir_curb) {
		closeblk_(ir_curb);
		ir_curb = 0;
	}
	if (!fn->start)
		err("ir_builder: empty function \"%s\"", fn->name);
	for (b = fn->start; b->link; b = b->link)
		;
	if (b->jmp.type == Jxxx)
		err("ir_builder: last block of \"%s\" misses a terminator", fn->name);

	fn->mem = vnew(0, sizeof fn->mem[0], PFn);
	fn->nmem = 0;
	fn->nblk = ir_nblk;
	fn->rpo = vnew(ir_nblk, sizeof fn->rpo[0], PFn);

	/* Same validation parse() itself runs on every text-parsed function
	 * (see all.h's declaration of typecheck() for why this is callable
	 * here at all — a linkage-only patch, not new logic). Calls err() on
	 * a malformed function; caller must be inside
	 * rust_qbe_protected_call(). */
	typecheck(fn);

	return fn;
}

/* ── Blocks ──────────────────────────────────────────────────────────── */

Blk *
qbe_ir_blk_new(Fn *fn, const char *name)
{
	Blk *b;

	(void)fn;
	/* Deliberately does NOT touch ir_curb/closeblk_ — allocating a block
	 * must be safe to do purely for a forward-reference handle (e.g. a
	 * loop body block created before the preceding block has finished
	 * accumulating its own instructions), exactly like a jmp/jnz target
	 * in text IL can name a block whose label hasn't been reached by the
	 * parser yet. Only qbe_ir_blk_set_current() closes/switches — see
	 * its own comment. */
	b = newblk();
	b->id = ir_nblk++;
	if (name)
		strncpy(b->name, name, NString - 1);
	else
		strf(b->name, "b%u", b->id);
	*ir_blink = b;
	ir_blink = &b->link;
	return b;
}

void
qbe_ir_blk_set_current(Fn *fn, Blk *b)
{
	(void)fn;
	if (ir_curb == b)
		return; /* already current -- no-op, not a reopen */
	if (b->nins != 0 || b->jmp.type != Jxxx)
		err("ir_builder: block @%s was already closed; blocks can only "
		    "be filled in once (same constraint text IL has: QBE's own "
		    "parser rejects redefining a block)", b->name);
	if (ir_curb)
		closeblk_(ir_curb);
	ir_curb = b;
}

/* ── Constants ───────────────────────────────────────────────────────── */

QbeRef
qbe_ir_con_int(Fn *fn, int64_t value)
{
	return qberef_from_ref(getcon(value, fn));
}

QbeRef
qbe_ir_con_double(Fn *fn, double value)
{
	Con c;

	memset(&c, 0, sizeof c);
	c.type = CBits;
	c.bits.d = value;
	c.flt = 2;
	return qberef_from_ref(newcon(&c, fn));
}

QbeRef
qbe_ir_con_single(Fn *fn, float value)
{
	Con c;

	memset(&c, 0, sizeof c);
	c.type = CBits;
	c.bits.s = value;
	c.flt = 1;
	return qberef_from_ref(newcon(&c, fn));
}

QbeRef
qbe_ir_con_addr(Fn *fn, const char *sym_name, int64_t addend)
{
	Con c;

	memset(&c, 0, sizeof c);
	c.type = CAddr;
	c.sym.type = SGlo;
	c.sym.id = intern((char *)sym_name);
	c.bits.i = addend;
	return qberef_from_ref(newcon(&c, fn));
}

/* ── Instructions ────────────────────────────────────────────────────── */

QbeRef
qbe_ir_ins(Fn *fn, int op, int cls, QbeRef arg0, QbeRef arg1)
{
	Ref dst;

	if (op < 0 || op >= QBE_OP_COUNT)
		err("ir_builder: invalid op index %d", op);
	dst = newtmp(0, cls, fn);
	emitfwd_(op_table[op], cls, dst, ref_from_qbe(arg0), ref_from_qbe(arg1));
	return qberef_from_ref(dst);
}

void
qbe_ir_store(Fn *fn, int op, QbeRef value, QbeRef addr)
{
	(void)fn;
	if (op < QBE_OP_STOREB || op > QBE_OP_STORED)
		err("ir_builder: qbe_ir_store called with non-store op %d", op);
	/* .cls is a placeholder for stores — the width comes from which
	 * store opcode variant was used (storeb/w/l/s/d), matching parse.c's
	 * own `k = Kw;` filler value for this instruction shape. */
	emitfwd_(op_table[op], Kw, R, ref_from_qbe(value), ref_from_qbe(addr));
}

QbeRef
qbe_ir_alloc(Fn *fn, int align, QbeRef size)
{
	int op;
	Ref dst;

	switch (align) {
	case 4: op = Oalloc4; break;
	case 8: op = Oalloc8; break;
	case 16: op = Oalloc16; break;
	default:
		err("ir_builder: invalid alloc alignment %d (must be 4, 8, or 16)", align);
	}
	dst = newtmp(0, Kl, fn);
	emitfwd_(op, Kl, dst, ref_from_qbe(size), R);
	return qberef_from_ref(dst);
}

/* ── Function parameters ─────────────────────────────────────────────── */

QbeRef
qbe_ir_par(Fn *fn, int cls)
{
	Ref dst = newtmp(0, cls, fn);
	emitfwd_(Opar, cls, dst, R, R);
	return qberef_from_ref(dst);
}

/* ── Calls ───────────────────────────────────────────────────────────── */

void
qbe_ir_arg(Fn *fn, int cls, QbeRef value)
{
	(void)fn;
	emitfwd_(Oarg, cls, R, ref_from_qbe(value), R);
}

QbeRef
qbe_ir_call(Fn *fn, QbeRef func, int ret_cls)
{
	Ref dst, funcref;
	int cls;

	fn->leaf = 0;
	funcref = ref_from_qbe(func);
	if (ret_cls == QBE_K_X) {
		dst = R;
		cls = Kw; /* placeholder, matches parse.c's void-call `k = Kw` */
	} else {
		dst = newtmp(0, ret_cls, fn);
		cls = ret_cls;
	}
	emitfwd_(Ocall, cls, dst, funcref, R);
	return qberef_from_ref(dst);
}

/* ── Terminators ─────────────────────────────────────────────────────── */

void
qbe_ir_jmp(Fn *fn, Blk *target)
{
	(void)fn;
	ir_curb->jmp.type = Jjmp;
	ir_curb->s1 = target;
	closeblk_(ir_curb);
	ir_curb = 0;
}

void
qbe_ir_jnz(Fn *fn, QbeRef cond, Blk *if_true, Blk *if_false)
{
	(void)fn;
	ir_curb->jmp.type = Jjnz;
	ir_curb->jmp.arg = ref_from_qbe(cond);
	ir_curb->s1 = if_true;
	ir_curb->s2 = if_false;
	closeblk_(ir_curb);
	ir_curb = 0;
}

void
qbe_ir_ret(Fn *fn, int cls, QbeRef value)
{
	(void)fn;
	if (cls == QBE_K_X) {
		ir_curb->jmp.type = Jret0;
	} else {
		ir_curb->jmp.type = Jretw + cls;
		ir_curb->jmp.arg = ref_from_qbe(value);
	}
	closeblk_(ir_curb);
	ir_curb = 0;
}

void
qbe_ir_hlt(Fn *fn)
{
	(void)fn;
	ir_curb->jmp.type = Jhlt;
	closeblk_(ir_curb);
	ir_curb = 0;
}

/* ── Compile: duplicates jit_collect.c's jit_func_cb (static there) and
 * qbe_bridge.c's select_target (also static) — see ir_builder.h's top
 * comment for why duplicating ~40 lines here beats exposing either. Both
 * of those files already duplicate a near-identical target-selection
 * helper between each other; this is a third copy of an established
 * pattern, not a new one. ────────────────────────────────────────────── */

extern Target T_amd64_sysv, T_amd64_apple, T_arm64, T_arm64_apple, T_rv64;

static int
select_target_(const char *name)
{
	Target *tlist[] = {&T_amd64_sysv, &T_amd64_apple, &T_arm64, &T_arm64_apple, &T_rv64, 0};
	Target **t;

	if (!name) {
		T = Deftgt;
		return 0;
	}
	for (t = tlist; *t; t++)
		if (strcmp(name, (*t)->name) == 0) {
			T = **t;
			return 0;
		}
	return -1;
}

int
qbe_ir_compile_jit(Fn *fn, JitCollector *jc, const char *target_name)
{
	uint n;

	if (select_target_(target_name) != 0)
		return QBE_ERR_TARGET;

	/* Identical to jit_collect.c's jit_func_cb body — see that function
	 * for why each pass runs where it does; this is not a new pipeline,
	 * just the same one with an explicit `fn`/`jc` instead of parse()'s
	 * callback-supplied Fn* and jit_collect.c's static bridge_jc. */
	T.abi0(fn);
	fillcfg(fn);
	filluse(fn);
	promote(fn);
	filluse(fn);
	ssa(fn);
	filluse(fn);
	ssacheck(fn);
	fillalias(fn);
	loadopt(fn);
	filluse(fn);
	fillalias(fn);
	coalesce(fn);
	filluse(fn);
	filldom(fn);
	ssacheck(fn);
	gvn(fn);
	fillcfg(fn);
	simplcfg(fn);
	filluse(fn);
	filldom(fn);
	gcm(fn);
	filluse(fn);
	ssacheck(fn);
	if (T.cansel) {
		ifconvert(fn);
		fillcfg(fn);
		filluse(fn);
		filldom(fn);
		ssacheck(fn);
	}
	T.abi1(fn);
	simpl(fn);
	fillcfg(fn);
	filluse(fn);
	T.isel(fn);
	fillcfg(fn);
	filllive(fn);
	fillloop(fn);
	fillcost(fn);
	spill(fn);
	rega(fn);
	fillcfg(fn);
	simpljmp(fn);
	fillcfg(fn);
	filllive(fn);

	assert(fn->rpo[0] == fn->start);
	for (n = 0;; n++)
		if (n == fn->nblk - 1) {
			fn->rpo[n]->link = 0;
			break;
		} else
			fn->rpo[n]->link = fn->rpo[n + 1];

	jit_collect_fn(jc, fn);
	freeall();

	return QBE_OK;
}
