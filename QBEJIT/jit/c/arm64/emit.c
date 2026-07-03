#include "all.h"
#include <stdlib.h>

/* Check if MADD fusion is enabled via environment variable
 * Returns 1 if enabled, 0 if disabled (default: enabled)
 */
int
is_madd_fusion_enabled(void)
{
	static int checked = 0;
	static int enabled = 1;  /* Default: enabled */
	
	if (!checked) {
		const char *env = getenv("ENABLE_MADD_FUSION");
		if (env) {
			enabled = (strcmp(env, "1") == 0 || strcmp(env, "true") == 0);
		}
		checked = 1;
	}
	
	return enabled;
}

typedef struct E E;

struct E {
	FILE *f;
	Fn *fn;
	uint64_t frame;
	uint padding;
};

#define CMP(X) \
	X(Cieq,       "eq") \
	X(Cine,       "ne") \
	X(Cisge,      "ge") \
	X(Cisgt,      "gt") \
	X(Cisle,      "le") \
	X(Cislt,      "lt") \
	X(Ciuge,      "cs") \
	X(Ciugt,      "hi") \
	X(Ciule,      "ls") \
	X(Ciult,      "cc") \
	X(NCmpI+Cfeq, "eq") \
	X(NCmpI+Cfge, "ge") \
	X(NCmpI+Cfgt, "gt") \
	X(NCmpI+Cfle, "ls") \
	X(NCmpI+Cflt, "mi") \
	X(NCmpI+Cfne, "ne") \
	X(NCmpI+Cfo,  "vc") \
	X(NCmpI+Cfuo, "vs")

enum {
	Ki = -1, /* matches Kw and Kl */
	Ka = -2, /* matches all classes */
};

static struct {
	short op;
	short cls;
	char *fmt;
} omap[] = {
	{ Oadd,    Ki, "add %=, %0, %1" },
	{ Oadd,    Ka, "fadd %=, %0, %1" },
	{ Osub,    Ki, "sub %=, %0, %1" },
	{ Osub,    Ka, "fsub %=, %0, %1" },
	{ Oneg,    Ki, "neg %=, %0" },
	{ Oneg,    Ka, "fneg %=, %0" },
	{ Oand,    Ki, "and %=, %0, %1" },
	{ Oor,     Ki, "orr %=, %0, %1" },
	{ Oxor,    Ki, "eor %=, %0, %1" },
	{ Osar,    Ki, "asr %=, %0, %1" },
	{ Oshr,    Ki, "lsr %=, %0, %1" },
	{ Oshl,    Ki, "lsl %=, %0, %1" },
	{ Omul,    Ki, "mul %=, %0, %1" },
	{ Omul,    Ka, "fmul %=, %0, %1" },
	{ Odiv,    Ki, "sdiv %=, %0, %1" },
	{ Odiv,    Ka, "fdiv %=, %0, %1" },
	{ Oudiv,   Ki, "udiv %=, %0, %1" },
	{ Orem,    Ki, "sdiv %?, %0, %1\n\tmsub\t%=, %?, %1, %0" },
	{ Ourem,   Ki, "udiv %?, %0, %1\n\tmsub\t%=, %?, %1, %0" },
	{ Ocopy,   Ki, "mov %=, %0" },
	{ Ocopy,   Ka, "fmov %=, %0" },
	{ Oswap,   Ki, "mov %?, %0\n\tmov\t%0, %1\n\tmov\t%1, %?" },
	{ Oswap,   Ka, "fmov %?, %0\n\tfmov\t%0, %1\n\tfmov\t%1, %?" },
	{ Ostoreb, Kw, "strb %W0, %M1" },
	{ Ostoreh, Kw, "strh %W0, %M1" },
	{ Ostorew, Kw, "str %W0, %M1" },
	{ Ostorel, Kw, "str %L0, %M1" },
	{ Ostores, Kw, "str %S0, %M1" },
	{ Ostored, Kw, "str %D0, %M1" },
	{ Oloadsb, Ki, "ldrsb %=, %M0" },
	{ Oloadub, Ki, "ldrb %W=, %M0" },
	{ Oloadsh, Ki, "ldrsh %=, %M0" },
	{ Oloaduh, Ki, "ldrh %W=, %M0" },
	{ Oloadsw, Kw, "ldr %=, %M0" },
	{ Oloadsw, Kl, "ldrsw %=, %M0" },
	{ Oloaduw, Ki, "ldr %W=, %M0" },
	{ Oload,   Ka, "ldr %=, %M0" },
	{ Oextsb,  Ki, "sxtb %=, %W0" },
	{ Oextub,  Ki, "uxtb %W=, %W0" },
	{ Oextsh,  Ki, "sxth %=, %W0" },
	{ Oextuh,  Ki, "uxth %W=, %W0" },
	{ Oextsw,  Ki, "sxtw %L=, %W0" },
	{ Oextuw,  Ki, "mov %W=, %W0" },
	{ Oexts,   Kd, "fcvt %=, %S0" },
	{ Otruncd, Ks, "fcvt %=, %D0" },
	{ Ocast,   Kw, "fmov %=, %S0" },
	{ Ocast,   Kl, "fmov %=, %D0" },
	{ Ocast,   Ks, "fmov %=, %W0" },
	{ Ocast,   Kd, "fmov %=, %L0" },
	{ Ostosi,  Ka, "fcvtzs %=, %S0" },
	{ Ostoui,  Ka, "fcvtzu %=, %S0" },
	{ Odtosi,  Ka, "fcvtzs %=, %D0" },
	{ Odtoui,  Ka, "fcvtzu %=, %D0" },
	{ Oswtof,  Ka, "scvtf %=, %W0" },
	{ Ouwtof,  Ka, "ucvtf %=, %W0" },
	{ Osltof,  Ka, "scvtf %=, %L0" },
	{ Oultof,  Ka, "ucvtf %=, %L0" },
	{ Ocall,   Kw, "blr %L0" },

	{ Oacmp,   Ki, "cmp %0, %1" },
	{ Oacmn,   Ki, "cmn %0, %1" },
	{ Oafcmp,  Ka, "fcmpe %0, %1" },

#define X(c, str) \
	{ Oflag+c, Ki, "cset %=, " str },
	CMP(X)
#undef X
	{ NOp, 0, 0 }
};

enum {
	V31 = 0x1fffffff,  /* local name for V31 */
};

static char *
rname(int r, int k)
{
	static char buf[4];

	if (r == SP) {
		assert(k == Kl);
		sprintf(buf, "sp");
	}
	else if (R0 <= r && r <= LR)
		switch (k) {
		default: die("invalid class");
		case Kw: sprintf(buf, "w%d", r-R0); break;
		case Kx:
		case Kl: sprintf(buf, "x%d", r-R0); break;
		}
	else if (V0 <= r && r <= V30)
		switch (k) {
		default: die("invalid class");
		case Ks: sprintf(buf, "s%d", r-V0); break;
		case Kx:
		case Kd: sprintf(buf, "d%d", r-V0); break;
		}
	else if (r == V31)
		switch (k) {
		default: die("invalid class");
		case Ks: sprintf(buf, "s31"); break;
		case Kd: sprintf(buf, "d31"); break;
		}
	else
		die("invalid register");
	return buf;
}

static uint64_t
slot(Ref r, E *e)
{
	int s;

	s = rsval(r);
	if (s == -1)
		return 16 + e->frame;
	if (s < 0) {
		if (e->fn->vararg && !T.apple)
			return 16 + e->frame + 192 - (s+2);
		else
			return 16 + e->frame - (s+2);
	} else
		return 16 + e->padding + 4 * s;
}

static void
emitf(char *s, Ins *i, E *e)
{
	Ref r;
	int k, c;
	Con *pc;
	uint64_t n;
	uint sp;

	fputc('\t', e->f);

	sp = 0;
	for (;;) {
		k = i->cls;
		while ((c = *s++) != '%')
			if (c == ' ' && !sp) {
				fputc('\t', e->f);
				sp = 1;
			} else if (!c) {
				fputc('\n', e->f);
				return;
			} else
				fputc(c, e->f);
	Switch:
		switch ((c = *s++)) {
		default:
			die("invalid escape");
		case 'W':
			k = Kw;
			goto Switch;
		case 'L':
			k = Kl;
			goto Switch;
		case 'S':
			k = Ks;
			goto Switch;
		case 'D':
			k = Kd;
			goto Switch;
		case '?':
			if (KBASE(k) == 0)
				fputs(rname(IP1, k), e->f);
			else
				fputs(rname(V31, k), e->f);
			break;
		case '=':
		case '0':
			r = c == '=' ? i->to : i->arg[0];
			assert(isreg(r) || req(r, TMP(V31)));
			fputs(rname(r.val, k), e->f);
			break;
		case '1':
			r = i->arg[1];
			switch (rtype(r)) {
			default:
				die("invalid second argument");
			case RTmp:
				assert(isreg(r));
				fputs(rname(r.val, k), e->f);
				break;
			case RCon:
				pc = &e->fn->con[r.val];
				n = pc->bits.i;
				assert(pc->type == CBits);
				if (n >> 24) {
					assert(arm64_logimm(n, k));
					fprintf(e->f, "#%"PRIu64, n);
				} else if (n & 0xfff000) {
					assert(!(n & ~0xfff000ull));
					fprintf(e->f, "#%"PRIu64", lsl #12",
						n>>12);
				} else {
					assert(!(n & ~0xfffull));
					fprintf(e->f, "#%"PRIu64, n);
				}
				break;
			}
			break;
		case 'M':
			c = *s++;
			assert(c == '0' || c == '1' || c == '=');
			r = c == '=' ? i->to : i->arg[c - '0'];
			switch (rtype(r)) {
			default:
				die("todo (arm emit): unhandled ref");
			case RTmp:
				assert(isreg(r));
				fprintf(e->f, "[%s]", rname(r.val, Kl));
				break;
			case RSlot:
				fprintf(e->f, "[x29, %"PRIu64"]", slot(r, e));
				break;
			}
			break;
		}
	}
}

static void
loadaddr(Con *c, char *rn, E *e)
{
	char *p, *l, *s;

	switch (c->sym.type) {
	default:
		die("unreachable");
	case SGlo:
		if (T.apple)
			s = "\tadrp\tR, S@pageO\n"
			    "\tadd\tR, R, S@pageoffO\n";
		else
			s = "\tadrp\tR, SO\n"
			    "\tadd\tR, R, #:lo12:SO\n";
		break;
	case SThr:
		if (T.apple)
			s = "\tadrp\tR, S@tlvppage\n"
			    "\tldr\tR, [R, S@tlvppageoff]\n";
		else
			s = "\tmrs\tR, tpidr_el0\n"
			    "\tadd\tR, R, #:tprel_hi12:SO, lsl #12\n"
			    "\tadd\tR, R, #:tprel_lo12_nc:SO\n";
		break;
	}

	l = str(c->sym.id);
	p = l[0] == '"' ? "" : T.assym;
	for (; *s; s++)
		switch (*s) {
		default:
			fputc(*s, e->f);
			break;
		case 'R':
			fputs(rn, e->f);
			break;
		case 'S':
			fputs(p, e->f);
			fputs(l, e->f);
			break;
		case 'O':
			if (c->bits.i)
				/* todo, handle large offsets */
				fprintf(e->f, "+%"PRIi64, c->bits.i);
			break;
		}
}

static void
loadcon(Con *c, int r, int k, E *e)
{
	char *rn;
	int64_t n;
	int w, sh;

	w = KWIDE(k);
	rn = rname(r, k);
	n = c->bits.i;
	if (c->type == CAddr) {
		rn = rname(r, Kl);
		loadaddr(c, rn, e);
		return;
	}
	assert(c->type == CBits);
	if (!w)
		n = (int32_t)n;
	if ((n | 0xffff) == -1 || arm64_logimm(n, k)) {
		fprintf(e->f, "\tmov\t%s, #%"PRIi64"\n", rn, n);
	} else {
		fprintf(e->f, "\tmov\t%s, #%d\n",
			rn, (int)(n & 0xffff));
		for (sh=16; n>>=16; sh+=16) {
			if ((!w && sh == 32) || sh == 64)
				break;
			fprintf(e->f, "\tmovk\t%s, #0x%x, lsl #%d\n",
				rn, (uint)(n & 0xffff), sh);
		}
	}
}

static void emitins(Ins *, E *);

static int
fixarg(Ref *pr, int sz, int t, E *e)
{
	Ins *i;
	Ref r;
	uint64_t s;

	r = *pr;
	if (rtype(r) == RSlot) {
		s = slot(r, e);
		if (s > sz * 4095u) {
			if (t < 0)
				return 1;
			i = &(Ins){Oaddr, Kl, TMP(t), {r}};
			emitins(i, e);
			*pr = TMP(t);
		}
	}
	return 0;
}

/* Check whether the register written by 'prev' is referenced (read) by any
 * instruction after 'i' in the same basic block, by the block's branch,
 * or by any successor block (live-out).
 *
 * When we fuse prev+i (e.g. MUL+ADD → MADD), prev is never emitted.
 * If any later instruction still reads prev->to, it would get a stale
 * value.  This function returns 1 when fusion would be UNSAFE.
 *
 * Special case: if prev->to == i->to, the fused instruction writes to
 * the same register the consumed instruction would have written, so any
 * later reader of that register sees the fused result (which equals the
 * consumer's result).  The MUL result is overwritten by the ADD anyway,
 * so nothing after i could have been reading the MUL value through that
 * register.  In that case we return 0 (safe) without scanning.
 */
int
prev_result_used_later(Ins *i, Blk *b, Ref prev_to)
{
	Ins *j, *end;

	/* If the consumer overwrites the same register, the fused
	 * instruction will too — no stale value is possible. */
	if (req(i->to, prev_to))
		return 0;

	end = &b->ins[b->nins];

	/* Scan every instruction after 'i' in this block. */
	for (j = i + 1; j != end; j++) {
		if (req(j->arg[0], prev_to))
			return 1;
		if (req(j->arg[1], prev_to))
			return 1;
		/* If j overwrites prev_to, later instructions see j's
		 * result, not the consumed prev.  We can stop scanning
		 * because no subsequent read of prev_to refers to prev.
		 * (This makes the check precise rather than
		 * over-conservative.) */
		if (req(j->to, prev_to))
			return 0;
	}

	/* Check the block's branch / jump argument. */
	if (req(b->jmp.arg, prev_to))
		return 1;

	/* Check if the register is live-out from this block.
	 * After post-regalloc filllive(), b->out contains physical
	 * register IDs.  If prev_to is live-out, a successor block
	 * needs the MUL result, so fusion is unsafe. */
	if (rtype(prev_to) == RTmp && bshas(b->out, prev_to.val))
		return 1;

	return 0;
}

/* Try to fuse multiply-add patterns into MADD/FMADD instructions.
 * Pattern: ADD dest, src1, mul_result
 *          where mul_result = MUL arg1, arg2 (single use)
 * Emits:   MADD dest, arg1, arg2, src1
 *          (dest = src1 + (arg1 * arg2))
 * Returns: 1 if fused, 0 if not fusible
 */
static int
try_madd_fusion(Ins *i, Ins *prev, E *e, Blk *b)
{
	Ref addend;
	int mul_in_arg0, mul_in_arg1;
	char *mnemonic;

	/* Check if pattern matches: ADD following MUL */
	if (!prev || i->op != Oadd || prev->op != Omul)
		return 0;

	/* Type must match */
	if (i->cls != prev->cls)
		return 0;

	/* Check if ADD uses MUL result */
	mul_in_arg0 = req(i->arg[0], prev->to);
	mul_in_arg1 = req(i->arg[1], prev->to);

	if (!mul_in_arg0 && !mul_in_arg1)
		return 0; /* MUL result not used by ADD */

	/* All operands must be registers (not spilled, not constants) */
	if (!isreg(prev->arg[0]) || !isreg(prev->arg[1]))
		return 0;
	if (!isreg(i->arg[0]) || !isreg(i->arg[1]))
		return 0;

	/* Determine the addend (the non-multiply operand) */
	addend = mul_in_arg0 ? i->arg[1] : i->arg[0];

	/* Do not fuse if addend is the same register as MUL output.
	 * The MADD semantics read all sources before writing dest,
	 * but if addend == prev->to the "addend" value is actually
	 * the MUL result itself which was never computed. */
	if (req(addend, prev->to)) {
		if (getenv("DEBUG_MADD"))
			fprintf(stderr, "MADD: Addend is same register as MUL result - unsafe to fuse\n");
		return 0;
	}

	/* CRITICAL: After register allocation, CSE / GVN may have
	 * arranged for multiple instructions to read the MUL result
	 * register.  Fusion skips emission of the MUL, so any later
	 * reader would see a stale value.  Scan forward to verify
	 * the MUL result is truly dead after this ADD. */
	if (prev_result_used_later(i, b, prev->to)) {
		if (getenv("DEBUG_MADD"))
			fprintf(stderr, "MADD: MUL result register used later - unsafe to fuse\n");
		return 0;
	}

	/* Select instruction mnemonic based on type */
	if (KBASE(i->cls) == 0) {
		/* Integer MADD */
		mnemonic = "madd";
	} else {
		/* Floating-point FMADD */
		mnemonic = "fmadd";
	}

	/* Emit fused instruction: MADD dest, mul_op1, mul_op2, addend
	 * Semantics: dest = addend + (mul_op1 * mul_op2)
	 * Can't call rname() multiple times in one fprintf
	 * because it uses a static buffer. Call separately and emit. */
	fprintf(e->f, "\t%s\t", mnemonic);
	fprintf(e->f, "%s, ", rname(i->to.val, i->cls));        /* destination */
	fprintf(e->f, "%s, ", rname(prev->arg[0].val, i->cls)); /* multiply operand 1 */
	fprintf(e->f, "%s, ", rname(prev->arg[1].val, i->cls)); /* multiply operand 2 */
	fprintf(e->f, "%s\n", rname(addend.val, i->cls));       /* addend */

	return 1; /* Successfully fused */
}

/* Try to fuse multiply-subtract patterns into MSUB/FMSUB instructions.
 * Pattern: SUB dest, src1, mul_result
 *          where mul_result = MUL arg1, arg2 (single use)
 * Emits:   MSUB dest, arg1, arg2, src1
 *          (dest = src1 - (arg1 * arg2))
 * Returns: 1 if fused, 0 if not fusible
 */
static int
try_msub_fusion(Ins *i, Ins *prev, E *e, Blk *b)
{
	char *mnemonic;

	/* Check if pattern matches: SUB following MUL */
	if (!prev || i->op != Osub || prev->op != Omul)
		return 0;

	/* Type must match */
	if (i->cls != prev->cls) {
		if (getenv("DEBUG_MADD"))
			fprintf(stderr, "MSUB: Type mismatch\n");
		return 0;
	}

	/* SUB must use MUL result as second operand (subtrahend) */
	if (!req(i->arg[1], prev->to)) {
		if (getenv("DEBUG_MADD"))
			fprintf(stderr, "MSUB: MUL result not in SUB arg[1]\n");
		return 0;
	}

	/* All operands must be registers */
	if (!isreg(prev->arg[0]) || !isreg(prev->arg[1])) {
		if (getenv("DEBUG_MADD"))
			fprintf(stderr, "MSUB: MUL operands not registers\n");
		return 0;
	}
	if (!isreg(i->arg[0]) || !isreg(i->arg[1])) {
		if (getenv("DEBUG_MADD"))
			fprintf(stderr, "MSUB: SUB operands not registers\n");
		return 0;
	}

	/* Verify MUL result register is not read again after this SUB. */
	if (prev_result_used_later(i, b, prev->to)) {
		if (getenv("DEBUG_MADD"))
			fprintf(stderr, "MSUB: MUL result register used later - unsafe to fuse\n");
		return 0;
	}

	/* Select instruction mnemonic based on type */
	if (KBASE(i->cls) == 0) {
		/* Integer MSUB */
		mnemonic = "msub";
	} else {
		/* Floating-point FMSUB */
		mnemonic = "fmsub";
	}

	/* Emit fused instruction: MSUB dest, mul_op1, mul_op2, minuend
	 * Semantics: dest = minuend - (mul_op1 * mul_op2)
	 * Can't call rname() multiple times in one fprintf (static buffer)
	 */
	fprintf(e->f, "\t%s\t", mnemonic);
	fprintf(e->f, "%s, ", rname(i->to.val, i->cls));        /* destination */
	fprintf(e->f, "%s, ", rname(prev->arg[0].val, i->cls)); /* multiply operand 1 */
	fprintf(e->f, "%s, ", rname(prev->arg[1].val, i->cls)); /* multiply operand 2 */
	fprintf(e->f, "%s\n", rname(i->arg[0].val, i->cls));    /* minuend */

	return 1; /* Successfully fused */
}

/* Check if shifted operand fusion is enabled via environment variable
 * Returns 1 if enabled, 0 if disabled (default: enabled)
 */
int
is_shift_fusion_enabled(void)
{
	static int checked = 0;
	static int enabled = 1;  /* Default: enabled */
	
	if (!checked) {
		const char *env = getenv("ENABLE_SHIFT_FUSION");
		if (env) {
			enabled = (strcmp(env, "1") == 0 || strcmp(env, "true") == 0);
		}
		checked = 1;
	}
	
	return enabled;
}

/* Check if LDP/STP pairing is enabled via environment variable
 * Returns 1 if enabled, 0 if disabled (default: enabled)
 */
int
is_ldp_stp_fusion_enabled(void)
{
	static int checked = 0;
	static int enabled = 1;  /* Default: enabled */

	if (!checked) {
		const char *env = getenv("ENABLE_LDP_STP_FUSION");
		if (env) {
			enabled = (strcmp(env, "1") == 0 || strcmp(env, "true") == 0);
		}
		checked = 1;
	}

	return enabled;
}

/* Check if indexed addressing fusion is enabled via environment variable
 * Returns 1 if enabled, 0 if disabled (default: enabled)
 */
int
is_indexed_addr_enabled(void)
{
	static int checked = 0;
	static int enabled = 1;  /* Default: enabled */

	if (!checked) {
		const char *env = getenv("ENABLE_INDEXED_ADDR");
		if (env) {
			enabled = (strcmp(env, "1") == 0 || strcmp(env, "true") == 0);
		}
		checked = 1;
	}

	return enabled;
}

/* Check if NEON copy optimization is enabled via environment variable
 * Returns 1 if enabled, 0 if disabled (default: enabled)
 */
int
is_neon_copy_enabled(void)
{
	static int checked = 0;
	static int enabled = 1;  /* Default: enabled */

	if (!checked) {
		const char *env = getenv("ENABLE_NEON_COPY");
		if (env) {
			enabled = (strcmp(env, "1") == 0 || strcmp(env, "true") == 0);
		}
		checked = 1;
	}

	return enabled;
}

/* Check if NEON arithmetic is enabled via environment variable
 * Returns 1 if enabled, 0 if disabled (default: enabled)
 */
int
is_neon_arith_enabled(void)
{
	static int checked = 0;
	static int enabled = 1;  /* Default: enabled */

	if (!checked) {
		const char *env = getenv("ENABLE_NEON_ARITH");
		if (env) {
			enabled = (strcmp(env, "1") == 0 || strcmp(env, "true") == 0);
		}
		checked = 1;
	}

	return enabled;
}

/* Map NEON instruction class to arrangement suffix string.
 * Kw → ".4s"  (4×32-bit integer)
 * Kl → ".2d"  (2×64-bit integer)
 * Ks → ".4s"  (4×32-bit float — same encoding)
 * Kd → ".2d"  (2×64-bit float)
 *  4 → ".8h"  (8×16-bit integer, SHORT)
 *  5 → ".16b" (16×8-bit integer, BYTE)
 */
static char *
neon_arrangement(int cls)
{
	switch (cls) {
	case Kw: return "4s";
	case Ks: return "4s";
	case Kl: return "2d";
	case Kd: return "2d";
	case 4:  return "8h";
	case 5:  return "16b";
	default: return "4s";
	}
}

/* Determine if NEON arrangement uses float instructions.
 * Ks and Kd use fadd/fsub/fmul, Kw and Kl use add/sub/mul.
 * Codes 4 (.8h) and 5 (.16b) are integer-only.
 */
static int
neon_is_float(int cls)
{
	return cls == Ks || cls == Kd;
}

/* Decode arrangement encoding from arg[0] for NEON arithmetic ops.
 * Resultless ops always get cls=Kw from the parser, so the frontend
 * encodes the actual arrangement as an integer constant in arg[0]:
 *   0 = Kw (.4s integer)
 *   1 = Kl (.2d integer)
 *   2 = Ks (.4s float)
 *   3 = Kd (.2d float)
 *   4 =     .8h integer  (8×16-bit, SHORT)
 *   5 =     .16b integer (16×8-bit, BYTE)
 * The parser creates RCon refs via getcon() for integer literals,
 * so we must check both RInt (inline) and RCon (constant table).
 * Falls back to the instruction's cls field if neither matches.
 */
static int
neon_arr_from_arg(Ins *i, E *e)
{
	int v;

	if (rtype(i->arg[0]) == RInt) {
		v = rsval(i->arg[0]);
		if (v >= 0 && v <= 5)
			return v;
	}
	if (rtype(i->arg[0]) == RCon) {
		Con *c = &e->fn->con[i->arg[0].val];
		if (c->type == CBits) {
			v = (int)c->bits.i;
			if (v >= 0 && v <= 5)
				return v;
		}
	}
	return i->cls;
}

/* Get the scalar GPR suffix for neondup based on arrangement.
 * .4s / .4s-float → "w" (32-bit GPR)
 * .2d / .2d-float → "x" (64-bit GPR)
 * .8h / .16b      → "w" (32-bit GPR, value is zero-extended into lanes)
 */
static char *
neon_dup_gpr_prefix(int arr)
{
	switch (arr) {
	case Kl: case Kd: return "x";
	default: return "w";  /* Kw, Ks, 4 (.8h), 5 (.16b) all use w register */
	}
}

/* Try to fuse shift + arithmetic patterns into single instructions with shifted operands.
 * ARM64 supports shifted operands in many instructions: ADD, SUB, AND, OR, EOR, etc.
 * Pattern: SHIFT dest, src, #imm
 *          followed by: OP result, arg1, dest
 * Emits:   OP result, arg1, src, SHIFT #imm
 * Returns: 1 if fused, 0 if not fusible
 */
static int
try_shift_fusion(Ins *i, Ins *prev, E *e, Blk *b)
{
	char *shift_mnemonic = NULL;
	int shift_amount;
	Ref shift_src, other_operand;
	int shift_in_arg0, shift_in_arg1;

	/* Only fuse integer operations */
	if (KBASE(i->cls) != 0 || KBASE(prev->cls) != 0)
		return 0;

	/* Check if prev is a shift instruction */
	if (prev->op != Oshl && prev->op != Oshr && prev->op != Osar)
		return 0;

	/* Shift amount must be a constant immediate */
	if (prev->arg[1].type != RCon)
		return 0;

	/* Get the shift constant value */
	Con *shift_con = &e->fn->con[prev->arg[1].val];
	if (shift_con->type != CBits)
		return 0;
	
	shift_amount = (int)shift_con->bits.i;

	/* Shift amount must be reasonable (0-63 for 64-bit, 0-31 for 32-bit) */
	if (shift_amount < 0 || shift_amount > 63)
		return 0;

	/* Current instruction must be ADD, SUB, AND, OR, or XOR */
	if (i->op != Oadd && i->op != Osub && i->op != Oand && i->op != Oor && i->op != Oxor)
		return 0;

	/* Check which operand of current instruction uses the shift result */
	shift_in_arg0 = req(i->arg[0], prev->to);
	shift_in_arg1 = req(i->arg[1], prev->to);

	if (!shift_in_arg0 && !shift_in_arg1)
		return 0;  /* Current instruction doesn't use shift result */

	if (shift_in_arg0 && shift_in_arg1)
		return 0;  /* Both operands use shift result - shouldn't happen but be safe */

	/* All operands must be registers */
	if (!isreg(prev->arg[0]) || !isreg(i->arg[0]) || !isreg(i->arg[1]))
		return 0;

	/* Verify shift result register is not read again after this instruction. */
	if (prev_result_used_later(i, b, prev->to))
		return 0;

	/* Determine the non-shift operand */
	other_operand = shift_in_arg0 ? i->arg[1] : i->arg[0];
	shift_src = prev->arg[0];

	/* SUB doesn't support shifted operand in arg[0], only arg[1] */
	if (i->op == Osub && shift_in_arg0)
		return 0;

	/* Determine shift mnemonic */
	switch (prev->op) {
	case Oshl:
		shift_mnemonic = "lsl";
		break;
	case Oshr:
		shift_mnemonic = "lsr";
		break;
	case Osar:
		shift_mnemonic = "asr";
		break;
	default:
		return 0;
	}

	/* Emit fused instruction with shifted operand.
	 * rname() already selects w/x register names based on i->cls,
	 * so no additional prefix is needed. */
	switch (i->op) {
	case Oadd:
		fprintf(e->f, "\tadd\t");
		fprintf(e->f, "%s, ", rname(i->to.val, i->cls));
		fprintf(e->f, "%s, ", rname(other_operand.val, i->cls));
		fprintf(e->f, "%s, %s #%d\n", rname(shift_src.val, i->cls),
			shift_mnemonic, shift_amount);
		break;
	case Osub:
		/* For SUB, the shifted operand is always arg[1] */
		fprintf(e->f, "\tsub\t");
		fprintf(e->f, "%s, ", rname(i->to.val, i->cls));
		fprintf(e->f, "%s, ", rname(other_operand.val, i->cls));
		fprintf(e->f, "%s, %s #%d\n", rname(shift_src.val, i->cls),
			shift_mnemonic, shift_amount);
		break;
	case Oand:
		fprintf(e->f, "\tand\t");
		fprintf(e->f, "%s, ", rname(i->to.val, i->cls));
		fprintf(e->f, "%s, ", rname(other_operand.val, i->cls));
		fprintf(e->f, "%s, %s #%d\n", rname(shift_src.val, i->cls),
			shift_mnemonic, shift_amount);
		break;
	case Oor:
		fprintf(e->f, "\torr\t");
		fprintf(e->f, "%s, ", rname(i->to.val, i->cls));
		fprintf(e->f, "%s, ", rname(other_operand.val, i->cls));
		fprintf(e->f, "%s, %s #%d\n", rname(shift_src.val, i->cls),
			shift_mnemonic, shift_amount);
		break;
	case Oxor:
		fprintf(e->f, "\teor\t");
		fprintf(e->f, "%s, ", rname(i->to.val, i->cls));
		fprintf(e->f, "%s, ", rname(other_operand.val, i->cls));
		fprintf(e->f, "%s, %s #%d\n", rname(shift_src.val, i->cls),
			shift_mnemonic, shift_amount);
		break;
	default:
		return 0;
	}

	if (getenv("DEBUG_SHIFT_FUSION"))
		fprintf(stderr, "SHIFT: Fused %s + %s into single instruction\n",
			shift_mnemonic, i->op == Oadd ? "ADD" : 
			i->op == Osub ? "SUB" : i->op == Oand ? "AND" :
			i->op == Oor ? "OR" : "XOR");

	return 1; /* Successfully fused */
}

/* Determine the "pair class" of a memory instruction for LDP/STP fusion.
 * Returns 0 if the instruction cannot participate in pairing.
 *   1 = 4-byte integer (W registers)
 *   2 = 8-byte integer (X registers)
 *   3 = 4-byte float   (S registers)
 *   4 = 8-byte float   (D registers)
 * The pair class encodes both the size and the register bank.
 */
int
mem_pair_class(Ins *i)
{
	/* Stores */
	switch (i->op) {
	case Ostorew: return 1;  /* str wN */
	case Ostorel: return 2;  /* str xN */
	case Ostores: return 3;  /* str sN */
	case Ostored: return 4;  /* str dN */
	default: break;
	}

	/* Loads — only pair those that use 'ldr' (not ldrsb, ldrsh, ldrsw) */
	switch (i->op) {
	case Oloaduw: return 1;  /* ldr wN */
	case Oloadsw:
		/* Kw → ldr w (can pair), Kl → ldrsw x (cannot pair) */
		return (i->cls == Kw) ? 1 : 0;
	case Oload:
		switch (i->cls) {
		case Kw: return 1;
		case Kl: return 2;
		case Ks: return 3;
		case Kd: return 4;
		}
		return 0;
	default:
		return 0;
	}
}

/* Size in bytes for a given pair class. */
int
pair_class_size(int pc)
{
	switch (pc) {
	case 1: return 4;
	case 2: return 8;
	case 3: return 4;
	case 4: return 8;
	default: return 0;
	}
}

/* Register class (for rname) for a given pair class. */
int
pair_class_k(int pc)
{
	switch (pc) {
	case 1: return Kw;
	case 2: return Kl;
	case 3: return Ks;
	case 4: return Kd;
	default: return Kw;
	}
}

/* Try to fuse two consecutive loads into a single LDP instruction,
 * or two consecutive stores into a single STP instruction.
 *
 * ARM64 LDP/STP use a signed 7-bit scaled immediate offset from the
 * base register.  Ranges (from first element):
 *   32-bit (W/S): [-256, 252]  in steps of 4
 *   64-bit (X/D): [-512, 504]  in steps of 8
 *
 * Only slot-based addressing (relative to x29) is paired.
 *
 * Returns: 1 if fused (both instructions emitted), 0 if not fusible.
 */
static int
try_ldp_stp_fusion(Ins *i, Ins *prev, E *e, Blk *b)
{
	int pc_prev, pc_cur, sz, k;
	uint64_t off1, off2, lo, hi;
	int is_load_prev, is_load_cur, is_store_prev, is_store_cur;
	Ref addr_prev, addr_cur;
	Ref reg1, reg2;  /* the data registers (load dest / store src) */

	pc_prev = mem_pair_class(prev);
	pc_cur  = mem_pair_class(i);

	/* Both must be pairable and of the same class */
	if (pc_prev == 0 || pc_cur == 0 || pc_prev != pc_cur)
		return 0;

	/* Both must be the same direction (both loads or both stores) */
	is_load_prev  = isload(prev->op);
	is_load_cur   = isload(i->op);
	is_store_prev = isstore(prev->op);
	is_store_cur  = isstore(i->op);

	if (is_load_prev != is_load_cur || is_store_prev != is_store_cur)
		return 0;

	/* Determine the address refs */
	if (is_load_prev) {
		addr_prev = prev->arg[0];
		addr_cur  = i->arg[0];
		reg1 = prev->to;
		reg2 = i->to;
	} else {
		/* stores: arg[0] = value, arg[1] = address */
		addr_prev = prev->arg[1];
		addr_cur  = i->arg[1];
		reg1 = prev->arg[0];
		reg2 = i->arg[0];
	}

	/* Both must be slot references */
	if (rtype(addr_prev) != RSlot || rtype(addr_cur) != RSlot)
		return 0;

	/* Data operands must be registers */
	if (!isreg(reg1) || !isreg(reg2))
		return 0;

	/* For LDP, destination registers must be distinct */
	if (is_load_prev && req(reg1, reg2))
		return 0;

	/* Compute slot offsets */
	off1 = slot(addr_prev, e);
	off2 = slot(addr_cur, e);

	sz = pair_class_size(pc_prev);
	k  = pair_class_k(pc_prev);

	/* Determine which comes first; we need them adjacent.
	 * After this block: lo = lower offset, hi = lo + sz,
	 * and reg1/reg2 are ordered to match. */
	if (off2 == off1 + (uint64_t)sz) {
		lo = off1;
		/* reg1 already first, reg2 second — keep order */
	} else if (off1 == off2 + (uint64_t)sz) {
		lo = off2;
		/* Swap so reg ordering matches offset ordering */
		Ref tmp = reg1; reg1 = reg2; reg2 = tmp;
	} else {
		return 0; /* not adjacent */
	}

	/* Alignment: base offset must be a multiple of access size */
	if (lo % (uint64_t)sz != 0)
		return 0;

	/* Range check: LDP/STP signed 7-bit scaled offset.
	 * The offset is encoded as imm7 * scale relative to base.
	 * Positive range up to 63 * scale. */
	hi = lo;  /* we use the lower offset for the instruction */
	if (sz == 4 && hi > 252)
		return 0;
	if (sz == 8 && hi > 504)
		return 0;

	/* Also check that fixarg wouldn't have transformed these
	 * (fixarg triggers when offset > sz * 4095, far above LDP range,
	 *  so this is always safe — but be defensive). */
	if (lo > (uint64_t)sz * 4095u || lo + sz > (uint64_t)sz * 4095u)
		return 0;

	/* Emit the paired instruction */
	if (is_load_prev) {
		fprintf(e->f, "\tldp\t");
	} else {
		fprintf(e->f, "\tstp\t");
	}
	fprintf(e->f, "%s, ", rname(reg1.val, k));
	fprintf(e->f, "%s, ", rname(reg2.val, k));
	fprintf(e->f, "[x29, #%"PRIu64"]\n", lo);

	if (getenv("DEBUG_LDP_STP"))
		fprintf(stderr, "LDP/STP: Paired %s at offsets %"PRIu64" and %"PRIu64" (size %d)\n",
			is_load_prev ? "loads" : "stores", off1, off2, sz);

	return 1; /* Successfully fused */
}

/* Try to fold an ADD into a subsequent load/store as indexed addressing.
 *
 * Pattern:  ADD rN, rBase, rIndex   (64-bit, both operands registers)
 *           LDR rD, [rN]           (or STR rS, [rN])
 * Emits:    LDR rD, [rBase, rIndex] (or STR rS, [rBase, rIndex])
 *
 * This is the common array-access pattern where base + scaled_index
 * has been computed in a separate ADD, and the result is used once
 * as a memory address.
 *
 * Safety: The ADD result must not be used by any later instruction.
 *         (We reuse prev_result_used_later() for this check.)
 *
 * Returns: 1 if fused, 0 if not fusible.
 */
static int
try_indexed_addr_fusion(Ins *i, Ins *prev, E *e, Blk *b)
{
	int is_ld, is_st;
	Ref addr_ref;
	char *mnemonic;
	int data_k;  /* register class for the data register */

	/* prev must be an ADD in Kl (64-bit address arithmetic) */
	if (!prev || prev->op != Oadd || prev->cls != Kl)
		return 0;

	/* Both ADD operands must be registers (not constants, not slots) */
	if (rtype(prev->arg[0]) != RTmp || rtype(prev->arg[1]) != RTmp)
		return 0;
	if (!isreg(prev->arg[0]) || !isreg(prev->arg[1]))
		return 0;

	/* Neither ADD operand should be the scratch register IP1 */
	if (prev->arg[0].val == IP1 || prev->arg[1].val == IP1)
		return 0;

	/* ADD must produce a register result */
	if (!isreg(prev->to))
		return 0;

	/* Current instruction must be a load or store */
	is_ld = isload(i->op);
	is_st = isstore(i->op);
	if (!is_ld && !is_st)
		return 0;

	/* Determine the address operand of the memory instruction */
	if (is_ld) {
		addr_ref = i->arg[0];
	} else {
		addr_ref = i->arg[1];
	}

	/* The address must be an RTmp that matches the ADD result */
	if (rtype(addr_ref) != RTmp || !req(addr_ref, prev->to))
		return 0;

	/* Safety: the ADD result must not be used after this load/store */
	if (prev_result_used_later(i, b, prev->to))
		return 0;

	/* Determine the memory instruction mnemonic and data register class.
	 * We need to handle each load/store variant to emit the correct
	 * instruction with register-offset addressing. */
	switch (i->op) {
	case Oloadsb:
		mnemonic = (i->cls == Kl) ? "ldrsb" : "ldrsb";
		data_k = i->cls;
		break;
	case Oloadub:
		mnemonic = "ldrb";
		data_k = Kw;
		break;
	case Oloadsh:
		mnemonic = (i->cls == Kl) ? "ldrsh" : "ldrsh";
		data_k = i->cls;
		break;
	case Oloaduh:
		mnemonic = "ldrh";
		data_k = Kw;
		break;
	case Oloadsw:
		if (i->cls == Kl) {
			mnemonic = "ldrsw";
			data_k = Kl;
		} else {
			mnemonic = "ldr";
			data_k = Kw;
		}
		break;
	case Oloaduw:
		mnemonic = "ldr";
		data_k = Kw;
		break;
	case Oload:
		mnemonic = "ldr";
		data_k = i->cls;
		break;
	case Ostoreb:
		mnemonic = "strb";
		data_k = Kw;
		break;
	case Ostoreh:
		mnemonic = "strh";
		data_k = Kw;
		break;
	case Ostorew:
		mnemonic = "str";
		data_k = Kw;
		break;
	case Ostorel:
		mnemonic = "str";
		data_k = Kl;
		break;
	case Ostores:
		mnemonic = "str";
		data_k = Ks;
		break;
	case Ostored:
		mnemonic = "str";
		data_k = Kd;
		break;
	default:
		return 0;
	}

	/* For loads, the data register is i->to.
	 * For stores, the data register is i->arg[0]. */
	Ref data_reg;
	if (is_ld) {
		data_reg = i->to;
		if (!isreg(data_reg))
			return 0;
	} else {
		data_reg = i->arg[0];
		if (!isreg(data_reg))
			return 0;
	}

	/* Emit: mnemonic dataReg, [base, index]
	 * Use separate fprintf calls because rname uses a static buffer. */
	fprintf(e->f, "\t%s\t", mnemonic);
	fprintf(e->f, "%s, [", rname(data_reg.val, data_k));
	fprintf(e->f, "%s, ", rname(prev->arg[0].val, Kl));
	fprintf(e->f, "%s]\n", rname(prev->arg[1].val, Kl));

	if (getenv("DEBUG_INDEXED_ADDR"))
		fprintf(stderr, "IDXADDR: Fused ADD+%s into indexed addressing [base, index]\n",
			is_ld ? "load" : "store");

	return 1; /* Successfully fused */
}

static void
emitins(Ins *i, E *e)
{
	char *l, *p, *rn;
	uint64_t s;
	int o, t;
	Ref r;
	Con *c;
	char *arr;

	switch (i->op) {
	default:
		if (isload(i->op))
			fixarg(&i->arg[0], loadsz(i), IP1, e);
		if (isstore(i->op)) {
			t = T.apple ? -1 : R18;
			if (fixarg(&i->arg[1], storesz(i), t, e)) {
				if (req(i->arg[0], TMP(IP1))) {
					fprintf(e->f,
						"\tfmov\t%c31, %c17\n",
						"ds"[i->cls == Kw],
						"xw"[i->cls == Kw]);
					i->arg[0] = TMP(V31);
					i->op = Ostores + (i->cls-Kw);
				}
				fixarg(&i->arg[1], storesz(i), IP1, e);
			}
		}
	Table:
		/* most instructions are just pulled out of
		 * the table omap[], some special cases are
		 * detailed below */
		for (o=0;; o++) {
			/* this linear search should really be a binary
			 * search */
			if (omap[o].op == NOp)
				die("no match for %s(%c)",
					optab[i->op].name, "wlsd"[i->cls]);
			if (omap[o].op == i->op)
			if (omap[o].cls == i->cls || omap[o].cls == Ka
			|| (omap[o].cls == Ki && KBASE(i->cls) == 0))
				break;
		}
		emitf(omap[o].fmt, i, e);
		break;
	case Onop:
		break;
	case Ocopy:
		if (req(i->to, i->arg[0]))
			break;
		if (rtype(i->to) == RSlot) {
			r = i->to;
			if (!isreg(i->arg[0])) {
				i->to = TMP(IP1);
				emitins(i, e);
				i->arg[0] = i->to;
			}
			i->op = Ostorew + i->cls;
			i->cls = Kw;
			i->arg[1] = r;
			emitins(i, e);
			break;
		}
		assert(isreg(i->to));
		switch (rtype(i->arg[0])) {
		case RCon:
			c = &e->fn->con[i->arg[0].val];
			loadcon(c, i->to.val, i->cls, e);
			break;
		case RSlot:
			i->op = Oload;
			emitins(i, e);
			break;
		default:
			assert(i->to.val != IP1);
			goto Table;
		}
		break;
	case Oaddr:
		assert(rtype(i->arg[0]) == RSlot);
		rn = rname(i->to.val, Kl);
		s = slot(i->arg[0], e);
		if (s <= 4095)
			fprintf(e->f, "\tadd\t%s, x29, #%"PRIu64"\n", rn, s);
		else if (s <= 65535)
			fprintf(e->f,
				"\tmov\t%s, #%"PRIu64"\n"
				"\tadd\t%s, x29, %s\n",
				rn, s, rn, rn
			);
		else
			fprintf(e->f,
				"\tmov\t%s, #%"PRIu64"\n"
				"\tmovk\t%s, #%"PRIu64", lsl #16\n"
				"\tadd\t%s, x29, %s\n",
				rn, s & 0xFFFF, rn, s >> 16, rn, rn
			);
		break;
	case Ocall:
		if (rtype(i->arg[0]) != RCon)
			goto Table;
		c = &e->fn->con[i->arg[0].val];
		if (c->type != CAddr
		|| c->sym.type != SGlo
		|| c->bits.i)
			die("invalid call argument");
		l = str(c->sym.id);
		p = l[0] == '"' ? "" : T.assym;
		fprintf(e->f, "\tbl\t%s%s\n", p, l);
		break;
	case Osalloc:
		emitf("sub sp, sp, %0", i, e);
		if (!req(i->to, R))
			emitf("mov %=, sp", i, e);
		break;
	case Odbgloc:
		emitdbgloc(i->arg[0].val, i->arg[1].val, e->f);
		break;

	/* ===== ARM64 NEON Vector Operations ===== */

	case Oneonldr:
		/* Load 128 bits from [arg0] into q28 */
		if (!is_neon_copy_enabled()) {
			/* Fallback: should not reach here if frontend
			 * checks the kill-switch, but be safe */
			die("neonldr emitted but NEON copy disabled");
		}
		assert(isreg(i->arg[0]));
		fprintf(e->f, "\tldr\tq28, [%s]\n",
			rname(i->arg[0].val, Kl));
		break;

	case Oneonstr:
		/* Store 128 bits from q28 to [arg0] */
		if (!is_neon_copy_enabled()) {
			die("neonstr emitted but NEON copy disabled");
		}
		assert(isreg(i->arg[0]));
		fprintf(e->f, "\tstr\tq28, [%s]\n",
			rname(i->arg[0].val, Kl));
		break;

	case Oneonldr2:
		/* Load 128 bits from [arg0] into q29 */
		if (!is_neon_copy_enabled()) {
			die("neonldr2 emitted but NEON copy disabled");
		}
		assert(isreg(i->arg[0]));
		fprintf(e->f, "\tldr\tq29, [%s]\n",
			rname(i->arg[0].val, Kl));
		break;

	case Oneonstr2:
		/* Store 128 bits from q29 to [arg0] */
		if (!is_neon_copy_enabled()) {
			die("neonstr2 emitted but NEON copy disabled");
		}
		fprintf(e->f, "\tstr\tq29, [%s]\n",
			rname(i->arg[0].val, Kl));
		break;

	case Oneonldr3:
		/* Load 128 bits from [arg0] into q30 (for FMA third operand) */
		if (!is_neon_copy_enabled()) {
			die("neonldr3 emitted but NEON copy disabled");
		}
		fprintf(e->f, "\tldr\tq30, [%s]\n",
			rname(i->arg[0].val, Kl));
		break;

	case Oneonadd: {
		/* Vector add: v28.arr = v28.arr + v29.arr
		 * Arrangement encoded in arg[0] as integer constant. */
		int ac;
		if (!is_neon_arith_enabled()) {
			die("neonadd emitted but NEON arith disabled");
		}
		ac = neon_arr_from_arg(i, e);
		arr = neon_arrangement(ac);
		if (neon_is_float(ac))
			fprintf(e->f, "\tfadd\tv28.%s, v28.%s, v29.%s\n",
				arr, arr, arr);
		else
			fprintf(e->f, "\tadd\tv28.%s, v28.%s, v29.%s\n",
				arr, arr, arr);
		break;
	}

	case Oneonsub: {
		/* Vector sub: v28.arr = v28.arr - v29.arr */
		int ac;
		if (!is_neon_arith_enabled()) {
			die("neonsub emitted but NEON arith disabled");
		}
		ac = neon_arr_from_arg(i, e);
		arr = neon_arrangement(ac);
		if (neon_is_float(ac))
			fprintf(e->f, "\tfsub\tv28.%s, v28.%s, v29.%s\n",
				arr, arr, arr);
		else
			fprintf(e->f, "\tsub\tv28.%s, v28.%s, v29.%s\n",
				arr, arr, arr);
		break;
	}

	case Oneonmul: {
		/* Vector mul: v28.arr = v28.arr * v29.arr
		 * Note: integer MUL is not available for .2d arrangement
		 * on AArch64 NEON. Only .4s/.8h/.16b are supported. */
		int ac;
		if (!is_neon_arith_enabled()) {
			die("neonmul emitted but NEON arith disabled");
		}
		ac = neon_arr_from_arg(i, e);
		arr = neon_arrangement(ac);
		if (neon_is_float(ac))
			fprintf(e->f, "\tfmul\tv28.%s, v28.%s, v29.%s\n",
				arr, arr, arr);
		else
			fprintf(e->f, "\tmul\tv28.%s, v28.%s, v29.%s\n",
				arr, arr, arr);
		break;
	}

	case Oneonaddv: {
		/* Horizontal sum: reduce v28 lanes to scalar in dest GPR.
		 * For .4s:  addv s28, v28.4s  then  fmov wDest, s28
		 * For .2d:  addp d28, v28.2d  then  fmov xDest, d28
		 * For .8h:  addv h28, v28.8h  then  smov wDest, v28.h[0]
		 * For .16b: addv b28, v28.16b then  smov wDest, v28.b[0]
		 *
		 * The arrangement is encoded in arg[0] (same as other ops).
		 * cls Kw → .4s, cls Kl → .2d (legacy), or explicit 4/5.
		 */
		int ac;
		if (!is_neon_arith_enabled()) {
			die("neonaddv emitted but NEON arith disabled");
		}
		assert(isreg(i->to));
		ac = neon_arr_from_arg(i, e);
		if (ac == 5) {
			/* .16b: addv reduces 16 byte lanes → b28 */
			fprintf(e->f, "\taddv\tb28, v28.16b\n");
			fprintf(e->f, "\tsmov\t%s, v28.b[0]\n",
				rname(i->to.val, Kw));
		} else if (ac == 4) {
			/* .8h: addv reduces 8 halfword lanes → h28 */
			fprintf(e->f, "\taddv\th28, v28.8h\n");
			fprintf(e->f, "\tsmov\t%s, v28.h[0]\n",
				rname(i->to.val, Kw));
		} else if (ac == Kl || ac == Kd) {
			/* .2d: use addp for pairwise add */
			fprintf(e->f, "\taddp\td28, v28.2d\n");
			fprintf(e->f, "\tfmov\t%s, d28\n",
				rname(i->to.val, Kl));
		} else if (ac == Kd || ac == Ks) {
			/* .4s float: addv s28, v28.4s then fmov */
			fprintf(e->f, "\tfaddp\tv28.4s, v28.4s, v28.4s\n");
			fprintf(e->f, "\tfaddp\ts28, v28.2s\n");
			fprintf(e->f, "\tfmov\t%s, s28\n",
				rname(i->to.val, Kw));
		} else {
			/* .4s integer: addv s28, v28.4s then fmov */
			fprintf(e->f, "\taddv\ts28, v28.4s\n");
			fprintf(e->f, "\tfmov\t%s, s28\n",
				rname(i->to.val, Kw));
		}
		break;
	}

	case Oneondiv: {
		/* Vector division: v28.arr = v28.arr / v29.arr
		 * NEON only supports float division (fdiv), not integer.
		 * The frontend must only emit this for float arrangements. */
		int ac;
		if (!is_neon_arith_enabled()) {
			die("neondiv emitted but NEON arith disabled");
		}
		ac = neon_arr_from_arg(i, e);
		arr = neon_arrangement(ac);
		if (neon_is_float(ac))
			fprintf(e->f, "\tfdiv\tv28.%s, v28.%s, v29.%s\n",
				arr, arr, arr);
		else
			die("neondiv: integer vector division not supported on NEON");
		break;
	}

	case Oneonneg: {
		/* Vector negation: v28.arr = -v28.arr */
		int ac;
		if (!is_neon_arith_enabled()) {
			die("neonneg emitted but NEON arith disabled");
		}
		ac = neon_arr_from_arg(i, e);
		arr = neon_arrangement(ac);
		if (neon_is_float(ac))
			fprintf(e->f, "\tfneg\tv28.%s, v28.%s\n", arr, arr);
		else
			fprintf(e->f, "\tneg\tv28.%s, v28.%s\n", arr, arr);
		break;
	}

	case Oneonabs: {
		/* Vector absolute value: v28.arr = |v28.arr| */
		int ac;
		if (!is_neon_arith_enabled()) {
			die("neonabs emitted but NEON arith disabled");
		}
		ac = neon_arr_from_arg(i, e);
		arr = neon_arrangement(ac);
		if (neon_is_float(ac))
			fprintf(e->f, "\tfabs\tv28.%s, v28.%s\n", arr, arr);
		else
			fprintf(e->f, "\tabs\tv28.%s, v28.%s\n", arr, arr);
		break;
	}

	case Oneonfma: {
		/* Fused multiply-add: v28.arr += v29.arr * v30.arr
		 * Uses FMLA for float, MLA for integer.
		 * The caller must have loaded v29 and v30 before this op. */
		int ac;
		if (!is_neon_arith_enabled()) {
			die("neonfma emitted but NEON arith disabled");
		}
		ac = neon_arr_from_arg(i, e);
		arr = neon_arrangement(ac);
		if (neon_is_float(ac))
			fprintf(e->f, "\tfmla\tv28.%s, v29.%s, v30.%s\n",
				arr, arr, arr);
		else
			fprintf(e->f, "\tmla\tv28.%s, v29.%s, v30.%s\n",
				arr, arr, arr);
		break;
	}

	case Oneonmin: {
		/* Element-wise minimum: v28.arr = min(v28.arr, v29.arr) */
		int ac;
		if (!is_neon_arith_enabled()) {
			die("neonmin emitted but NEON arith disabled");
		}
		ac = neon_arr_from_arg(i, e);
		arr = neon_arrangement(ac);
		if (neon_is_float(ac))
			fprintf(e->f, "\tfmin\tv28.%s, v28.%s, v29.%s\n",
				arr, arr, arr);
		else
			fprintf(e->f, "\tsmin\tv28.%s, v28.%s, v29.%s\n",
				arr, arr, arr);
		break;
	}

	case Oneonmax: {
		/* Element-wise maximum: v28.arr = max(v28.arr, v29.arr) */
		int ac;
		if (!is_neon_arith_enabled()) {
			die("neonmax emitted but NEON arith disabled");
		}
		ac = neon_arr_from_arg(i, e);
		arr = neon_arrangement(ac);
		if (neon_is_float(ac))
			fprintf(e->f, "\tfmax\tv28.%s, v28.%s, v29.%s\n",
				arr, arr, arr);
		else
			fprintf(e->f, "\tsmax\tv28.%s, v28.%s, v29.%s\n",
				arr, arr, arr);
		break;
	}

	case Oneondup: {
		/* Broadcast scalar GPR to all lanes of v28.
		 * arg[0] = arrangement encoding (0=.4s, 1=.2d, 2=.4s-flt, 3=.2d-flt)
		 * arg[1] = GPR register holding the scalar value.
		 * For .4s: dup v28.4s, wN
		 * For .2d: dup v28.2d, xN */
		int ac;
		if (!is_neon_arith_enabled()) {
			die("neondup emitted but NEON arith disabled");
		}
		ac = neon_arr_from_arg(i, e);
		arr = neon_arrangement(ac);
		assert(isreg(i->arg[1]));
		fprintf(e->f, "\tdup\tv28.%s, %s%d\n",
			arr,
			neon_dup_gpr_prefix(ac),
			i->arg[1].val - R0);
		break;
	}
	}
}

static void
framelayout(E *e)
{
	int *r;
	uint o;
	uint64_t f;

	for (o=0, r=arm64_rclob; *r>=0; r++)
		o += 1 & (e->fn->reg >> *r);
	f = e->fn->slot;
	f = (f + 3) & -4;
	o += o & 1;
	e->padding = 4*(f-e->fn->slot);
	e->frame = 4*f + 8*o;
}

/*

  Stack-frame layout:

  +=============+
  | varargs     |
  |  save area  |
  +-------------+
  | callee-save |  ^
  |  registers  |  |
  +-------------+  |
  |    ...      |  |
  | spill slots |  |
  |    ...      |  | e->frame
  +-------------+  |
  |    ...      |  |
  |   locals    |  |
  |    ...      |  |
  +-------------+  |
  | e->padding  |  v
  +-------------+
  |  saved x29  |
  |  saved x30  |
  +=============+ <- x29

*/

void
arm64_emitfn(Fn *fn, FILE *out)
{
	static char *ctoa[] = {
	#define X(c, s) [c] = s,
		CMP(X)
	#undef X
	};
	static int id0;
	int s, n, c, lbl, *r;
	uint64_t o;
	Blk *b, *t;
	Ins *i;
	E *e;

	e = &(E){.f = out, .fn = fn};
	if (T.apple)
		e->fn->lnk.align = 4;
	emitfnlnk(e->fn->name, &e->fn->lnk, e->f);
	fputs("\thint\t#34\n", e->f);
	framelayout(e);

	if (e->fn->vararg && !T.apple) {
		for (n=7; n>=0; n--)
			fprintf(e->f, "\tstr\tq%d, [sp, -16]!\n", n);
		for (n=7; n>=0; n-=2)
			fprintf(e->f, "\tstp\tx%d, x%d, [sp, -16]!\n", n-1, n);
	}

	if (e->frame + 16 <= 512)
		fprintf(e->f,
			"\tstp\tx29, x30, [sp, -%"PRIu64"]!\n",
			e->frame + 16
		);
	else if (e->frame <= 4095)
		fprintf(e->f,
			"\tsub\tsp, sp, #%"PRIu64"\n"
			"\tstp\tx29, x30, [sp, -16]!\n",
			e->frame
		);
	else if (e->frame <= 65535)
		fprintf(e->f,
			"\tmov\tx16, #%"PRIu64"\n"
			"\tsub\tsp, sp, x16\n"
			"\tstp\tx29, x30, [sp, -16]!\n",
			e->frame
		);
	else
		fprintf(e->f,
			"\tmov\tx16, #%"PRIu64"\n"
			"\tmovk\tx16, #%"PRIu64", lsl #16\n"
			"\tsub\tsp, sp, x16\n"
			"\tstp\tx29, x30, [sp, -16]!\n",
			e->frame & 0xFFFF, e->frame >> 16
		);
	fputs("\tmov\tx29, sp\n", e->f);
	s = (e->frame - e->padding) / 4;
	if (is_ldp_stp_fusion_enabled()) {
		/* Collect callee-save registers that need saving,
		 * then emit STP pairs for adjacent slots. */
		int csave_regs[64];
		int csave_slots[64];
		int csave_n = 0;
		int stmp = s;
		for (r=arm64_rclob; *r>=0; r++)
			if (e->fn->reg & BIT(*r)) {
				stmp -= 2;
				csave_regs[csave_n] = *r;
				csave_slots[csave_n] = stmp;
				csave_n++;
			}
		/* Try to pair consecutive saves of the same bank */
		int ci = 0;
		while (ci < csave_n) {
			if (ci + 1 < csave_n) {
				int r1 = csave_regs[ci];
				int r2 = csave_regs[ci+1];
				int s1 = csave_slots[ci];
				int s2 = csave_slots[ci+1];
				int both_gpr = (r1 < V0 && r2 < V0);
				int both_fpr = (r1 >= V0 && r2 >= V0);
				if ((both_gpr || both_fpr) && (s2 == s1 - 2 || s1 == s2 - 2)) {
					/* Adjacent slots, same bank — emit STP */
					int lo_slot, lo_r, hi_r;
					int k = both_gpr ? Kl : Kd;
					int sz = both_gpr ? 8 : 8;
					if (s1 < s2) {
						lo_slot = s1; lo_r = r1; hi_r = r2;
					} else {
						lo_slot = s2; lo_r = r2; hi_r = r1;
					}
					uint64_t off = 16 + e->padding + 4 * (uint64_t)lo_slot;
					if (off <= 504) {
						fprintf(e->f, "\tstp\t");
						fprintf(e->f, "%s, ", rname(lo_r, k));
						fprintf(e->f, "%s, ", rname(hi_r, k));
						fprintf(e->f, "[x29, #%"PRIu64"]\n", off);
						ci += 2;
						continue;
					}
				}
			}
			/* Unpaired — emit single store */
			int sr = csave_regs[ci];
			int ss = csave_slots[ci];
			i = &(Ins){.arg = {TMP(sr), SLOT(ss)}};
			i->op = sr >= V0 ? Ostored : Ostorel;
			emitins(i, e);
			ci++;
		}
		s = stmp;
	} else {
		for (r=arm64_rclob; *r>=0; r++)
			if (e->fn->reg & BIT(*r)) {
				s -= 2;
				i = &(Ins){.arg = {TMP(*r), SLOT(s)}};
				i->op = *r >= V0 ? Ostored : Ostorel;
				emitins(i, e);
			}
	}

	for (lbl=0, b=e->fn->start; b; b=b->link) {
		Ins *prev = NULL;
		Ins *prev_mem = NULL;  /* buffered load/store for LDP/STP pairing */
		if (lbl || b->npred > 1)
			fprintf(e->f, "%s%d:\n", T.asloc, id0+b->id);
		for (i=b->ins; i!=&b->ins[b->nins]; i++) {
			/* If we have a pending instruction, try to fuse with current instruction */
			if (prev) {
				/* Try MADD/MSUB fusion if previous was MUL */
				if (is_madd_fusion_enabled() && prev->op == Omul) {
					if (try_madd_fusion(i, prev, e, b)) {
						/* Fused! Both MUL and ADD emitted as MADD, continue */
						prev = NULL;
						continue;
					}
					if (try_msub_fusion(i, prev, e, b)) {
						/* Fused! Both MUL and SUB emitted as MSUB, continue */
						prev = NULL;
						continue;
					}
				}
				
				/* Try shift fusion if previous was a shift */
				if (is_shift_fusion_enabled() && 
				    (prev->op == Oshl || prev->op == Oshr || prev->op == Osar)) {
					if (try_shift_fusion(i, prev, e, b)) {
						/* Fused! Both SHIFT and OP emitted as single instruction */
						prev = NULL;
						continue;
					}
				}

				/* Try indexed addressing: ADD + load/store → ldr/str [base, index] */
				if (is_indexed_addr_enabled() && prev->op == Oadd && prev->cls == Kl) {
					if (try_indexed_addr_fusion(i, prev, e, b)) {
						/* Fused! ADD folded into memory instruction */
						prev = NULL;
						/* This memory op was fused, so don't buffer it
						 * for LDP/STP (it's already emitted). */
						continue;
					}
				}
				
				/* Couldn't fuse - emit the pending instruction now */
				emitins(prev, e);
				prev = NULL;
			}
			
			/* Check if this is a fusible instruction - defer emission.
			 * Oacmp is deferred so that cmp rN,#0 + beq/bne
			 * can be fused into cbz/cbnz at end of block. */
			if ((is_madd_fusion_enabled() && i->op == Omul) ||
			    (is_shift_fusion_enabled() && 
			     (i->op == Oshl || i->op == Oshr || i->op == Osar)) ||
			    (is_indexed_addr_enabled() && i->op == Oadd
			     && i->cls == Kl
			     && rtype(i->arg[0]) == RTmp && rtype(i->arg[1]) == RTmp
			     && isreg(i->arg[0]) && isreg(i->arg[1])
			     && i->arg[0].val != IP1 && i->arg[1].val != IP1) ||
			    i->op == Oacmp) {
				/* Flush any pending memory op before deferring */
				if (prev_mem) {
					emitins(prev_mem, e);
					prev_mem = NULL;
				}
				prev = i;
				continue;
			}

			/* LDP/STP pairing: buffer loads/stores for adjacent-pair fusion */
			if (is_ldp_stp_fusion_enabled() && mem_pair_class(i) != 0) {
				if (prev_mem) {
					/* Try to pair prev_mem with i */
					if (try_ldp_stp_fusion(i, prev_mem, e, b)) {
						/* Both emitted as a single LDP/STP */
						prev_mem = NULL;
						continue;
					}
					/* Can't pair — emit the buffered one, buffer the new one */
					emitins(prev_mem, e);
				}
				prev_mem = i;
				continue;
			}

			/* Non-memory, non-fusible instruction.
			 * Flush any buffered memory op first. */
			if (prev_mem) {
				emitins(prev_mem, e);
				prev_mem = NULL;
			}

			/* Emit current instruction normally */
			emitins(i, e);
		}

		/* Flush any remaining buffered memory instruction */
		if (prev_mem) {
			emitins(prev_mem, e);
			prev_mem = NULL;
		}
		
		/* If we have a pending instruction at end of block, try
		 * CBZ/CBNZ fusion before falling back to normal emission.
		 *
		 * Pattern:  cmp rN, #0  /  beq|bne label
		 * Becomes:  cbz|cbnz rN, label
		 *
		 * We compute the final adjusted condition (after the
		 * layout-driven swap / negate that the branch emitter
		 * performs) and only fuse when it resolves to eq or ne.
		 */
		int use_cbz = 0;   /* 0 = normal, 1 = cbz, 2 = cbnz */
		int cbz_reg = -1;
		int cbz_cls = Kw;

		if (prev) {
			if (prev->op == Oacmp
			&& isreg(prev->arg[0])
			&& rtype(prev->arg[1]) == RCon
			&& e->fn->con[prev->arg[1].val].type == CBits
			&& e->fn->con[prev->arg[1].val].bits.i == 0
			&& b->jmp.type >= Jjf
			&& b->jmp.type <= Jjf1) {
				int jc = b->jmp.type - Jjf;
				int adj;
				if (b->link == b->s2)
					adj = jc;
				else
					adj = cmpneg(jc);
				if (adj == Cieq) {
					use_cbz = 1;
					cbz_reg = prev->arg[0].val;
					cbz_cls = prev->cls;
				} else if (adj == Cine) {
					use_cbz = 2;
					cbz_reg = prev->arg[0].val;
					cbz_cls = prev->cls;
				}
			}
			if (!use_cbz)
				emitins(prev, e);
			prev = NULL;
		}
		lbl = 1;
		switch (b->jmp.type) {
		case Jhlt:
			fprintf(e->f, "\tbrk\t#1000\n");
			break;
		case Jret0:
			s = (e->frame - e->padding) / 4;
			if (is_ldp_stp_fusion_enabled()) {
				/* Collect callee-save registers for restore,
				 * then emit LDP pairs for adjacent slots. */
				int cr_regs[64];
				int cr_slots[64];
				int cr_n = 0;
				int rtmp = s;
				for (r=arm64_rclob; *r>=0; r++)
					if (e->fn->reg & BIT(*r)) {
						rtmp -= 2;
						cr_regs[cr_n] = *r;
						cr_slots[cr_n] = rtmp;
						cr_n++;
					}
				int ri = 0;
				while (ri < cr_n) {
					if (ri + 1 < cr_n) {
						int r1 = cr_regs[ri];
						int r2 = cr_regs[ri+1];
						int s1 = cr_slots[ri];
						int s2 = cr_slots[ri+1];
						int both_gpr = (r1 < V0 && r2 < V0);
						int both_fpr = (r1 >= V0 && r2 >= V0);
						if ((both_gpr || both_fpr) && (s2 == s1 - 2 || s1 == s2 - 2)) {
							int lo_slot, lo_r, hi_r;
							int k = both_gpr ? Kl : Kd;
							if (s1 < s2) {
								lo_slot = s1; lo_r = r1; hi_r = r2;
							} else {
								lo_slot = s2; lo_r = r2; hi_r = r1;
							}
							uint64_t off = 16 + e->padding + 4 * (uint64_t)lo_slot;
							if (off <= 504) {
								fprintf(e->f, "\tldp\t");
								fprintf(e->f, "%s, ", rname(lo_r, k));
								fprintf(e->f, "%s, ", rname(hi_r, k));
								fprintf(e->f, "[x29, #%"PRIu64"]\n", off);
								ri += 2;
								continue;
							}
						}
					}
					/* Unpaired — emit single load */
					int rr = cr_regs[ri];
					int rs = cr_slots[ri];
					i = &(Ins){Oload, 0, TMP(rr), {SLOT(rs)}};
					i->cls = rr >= V0 ? Kd : Kl;
					emitins(i, e);
					ri++;
				}
				s = rtmp;
			} else {
				for (r=arm64_rclob; *r>=0; r++)
					if (e->fn->reg & BIT(*r)) {
						s -= 2;
						i = &(Ins){Oload, 0, TMP(*r), {SLOT(s)}};
						i->cls = *r >= V0 ? Kd : Kl;
						emitins(i, e);
					}
			}
			if (e->fn->dynalloc)
				fputs("\tmov sp, x29\n", e->f);
			o = e->frame + 16;
			if (e->fn->vararg && !T.apple)
				o += 192;
			if (o <= 504)
				fprintf(e->f,
					"\tldp\tx29, x30, [sp], %"PRIu64"\n",
					o
				);
			else if (o - 16 <= 4095)
				fprintf(e->f,
					"\tldp\tx29, x30, [sp], 16\n"
					"\tadd\tsp, sp, #%"PRIu64"\n",
					o - 16
				);
			else if (o - 16 <= 65535)
				fprintf(e->f,
					"\tldp\tx29, x30, [sp], 16\n"
					"\tmov\tx16, #%"PRIu64"\n"
					"\tadd\tsp, sp, x16\n",
					o - 16
				);
			else
				fprintf(e->f,
					"\tldp\tx29, x30, [sp], 16\n"
					"\tmov\tx16, #%"PRIu64"\n"
					"\tmovk\tx16, #%"PRIu64", lsl #16\n"
					"\tadd\tsp, sp, x16\n",
					(o - 16) & 0xFFFF, (o - 16) >> 16
				);
			fprintf(e->f, "\tret\n");
			break;
		case Jjmp:
		Jmp:
			if (b->s1 != b->link)
				fprintf(e->f,
					"\tb\t%s%d\n",
					T.asloc, id0+b->s1->id
				);
			else
				lbl = 0;
			break;
		default:
			c = b->jmp.type - Jjf;
			if (c < 0 || c > NCmp)
				die("unhandled jump %d", b->jmp.type);
			if (b->link == b->s2) {
				t = b->s1;
				b->s1 = b->s2;
				b->s2 = t;
			} else
				c = cmpneg(c);
			if (use_cbz) {
				fprintf(e->f,
					"\t%s\t%s, %s%d\n",
					use_cbz == 1 ? "cbz" : "cbnz",
					rname(cbz_reg, cbz_cls),
					T.asloc, id0+b->s2->id
				);
			} else {
				fprintf(e->f,
					"\tb%s\t%s%d\n",
					ctoa[c], T.asloc, id0+b->s2->id
				);
			}
			goto Jmp;
		}
	}
	id0 += e->fn->nblk;
	if (!T.apple)
		elf_emitfnfin(fn->name, out);
}
