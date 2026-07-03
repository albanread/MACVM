#include "all.h"
#include <stdlib.h>

/* Check if MADD fusion is enabled via environment variable
 * Returns 1 if enabled, 0 if disabled (default: enabled)
 */
static int
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

/* Try to fuse multiply-add patterns into MADD/FMADD instructions.
 * Pattern: ADD dest, src1, mul_result
 *          where mul_result = MUL arg1, arg2 (single use)
 * Emits:   MADD dest, arg1, arg2, src1
 *          (dest = src1 + (arg1 * arg2))
 * Returns: 1 if fused, 0 if not fusible
 */
static int
try_madd_fusion(Ins *i, Ins *prev, E *e)
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

	/* Note: We cannot check nuse at emitter stage because after register
	 * allocation, prev->to.val is a physical register number, not a temp index.
	 * Instead, we rely on the peephole pattern: if the MUL is immediately
	 * followed by an ADD that uses its result, and no other instruction between
	 * them, then the result is single-use within this basic block.
	 * This is safe because:
	 * 1. We only look at adjacent instructions
	 * 2. If MUL result was needed elsewhere, register allocator would have
	 *    inserted a copy or the instructions wouldn't be adjacent
	 */

	/* Determine the addend (the non-multiply operand) */
	addend = mul_in_arg0 ? i->arg[1] : i->arg[0];

	/* CRITICAL SAFETY CHECK: Do not fuse if addend is the same register as MUL output.
	 * This can happen when register allocator reuses registers across control flow merges.
	 * If addend == prev->to, the addend value may be stale from a PHI node or control merge.
	 * This bug manifests as incorrect array accesses after WHILE loops with nested IFs.
	 */
	if (req(addend, prev->to)) {
		if (getenv("DEBUG_MADD"))
			fprintf(stderr, "MADD: Addend is same register as MUL result - unsafe to fuse\n");
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
try_msub_fusion(Ins *i, Ins *prev, E *e)
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

	/* Note: nuse check not valid after register allocation (see try_madd_fusion) */

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
static int
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

/* Try to fuse shift + arithmetic patterns into single instructions with shifted operands.
 * ARM64 supports shifted operands in many instructions: ADD, SUB, AND, OR, EOR, etc.
 * Pattern: SHIFT dest, src, #imm
 *          followed by: OP result, arg1, dest
 * Emits:   OP result, arg1, src, SHIFT #imm
 * Returns: 1 if fused, 0 if not fusible
 */
static int
try_shift_fusion(Ins *i, Ins *prev, E *e)
{
	char *shift_mnemonic = NULL;
	char *op_prefix = "";
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

	/* Determine operation prefix (for wide vs long) */
	if (KWIDE(i->cls)) {
		op_prefix = "W";
	} else {
		op_prefix = "";
	}

	/* Emit fused instruction with shifted operand */
	switch (i->op) {
	case Oadd:
		fprintf(e->f, "\tadd\t%s%s, %s%s, %s%s, %s #%d\n",
			op_prefix, rname(i->to.val, i->cls),
			op_prefix, rname(other_operand.val, i->cls),
			op_prefix, rname(shift_src.val, i->cls),
			shift_mnemonic, shift_amount);
		break;
	case Osub:
		/* For SUB, the shifted operand is always arg[1] */
		fprintf(e->f, "\tsub\t%s%s, %s%s, %s%s, %s #%d\n",
			op_prefix, rname(i->to.val, i->cls),
			op_prefix, rname(other_operand.val, i->cls),
			op_prefix, rname(shift_src.val, i->cls),
			shift_mnemonic, shift_amount);
		break;
	case Oand:
		fprintf(e->f, "\tand\t%s%s, %s%s, %s%s, %s #%d\n",
			op_prefix, rname(i->to.val, i->cls),
			op_prefix, rname(other_operand.val, i->cls),
			op_prefix, rname(shift_src.val, i->cls),
			shift_mnemonic, shift_amount);
		break;
	case Oor:
		fprintf(e->f, "\torr\t%s%s, %s%s, %s%s, %s #%d\n",
			op_prefix, rname(i->to.val, i->cls),
			op_prefix, rname(other_operand.val, i->cls),
			op_prefix, rname(shift_src.val, i->cls),
			shift_mnemonic, shift_amount);
		break;
	case Oxor:
		fprintf(e->f, "\teor\t%s%s, %s%s, %s%s, %s #%d\n",
			op_prefix, rname(i->to.val, i->cls),
			op_prefix, rname(other_operand.val, i->cls),
			op_prefix, rname(shift_src.val, i->cls),
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

static void
emitins(Ins *i, E *e)
{
	char *l, *p, *rn;
	uint64_t s;
	int o, t;
	Ref r;
	Con *c;

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
	for (r=arm64_rclob; *r>=0; r++)
		if (e->fn->reg & BIT(*r)) {
			s -= 2;
			i = &(Ins){.arg = {TMP(*r), SLOT(s)}};
			i->op = *r >= V0 ? Ostored : Ostorel;
			emitins(i, e);
		}

	for (lbl=0, b=e->fn->start; b; b=b->link) {
		Ins *prev = NULL;
		if (lbl || b->npred > 1)
			fprintf(e->f, "%s%d:\n", T.asloc, id0+b->id);
		for (i=b->ins; i!=&b->ins[b->nins]; i++) {
			/* If we have a pending instruction, try to fuse with current instruction */
			if (prev) {
				/* Try MADD/MSUB fusion if previous was MUL */
				if (is_madd_fusion_enabled() && prev->op == Omul) {
					if (try_madd_fusion(i, prev, e)) {
						/* Fused! Both MUL and ADD emitted as MADD, continue */
						prev = NULL;
						continue;
					}
					if (try_msub_fusion(i, prev, e)) {
						/* Fused! Both MUL and SUB emitted as MSUB, continue */
						prev = NULL;
						continue;
					}
				}
				
				/* Try shift fusion if previous was a shift */
				if (is_shift_fusion_enabled() && 
				    (prev->op == Oshl || prev->op == Oshr || prev->op == Osar)) {
					if (try_shift_fusion(i, prev, e)) {
						/* Fused! Both SHIFT and OP emitted as single instruction */
						prev = NULL;
						continue;
					}
				}
				
				/* Couldn't fuse - emit the pending instruction now */
				emitins(prev, e);
				prev = NULL;
			}
			
			/* Check if this is a fusible instruction - defer emission */
			if ((is_madd_fusion_enabled() && i->op == Omul) ||
			    (is_shift_fusion_enabled() && 
			     (i->op == Oshl || i->op == Oshr || i->op == Osar))) {
				prev = i;
				continue;
			}
			
			/* Not a fusible instruction, emit normally */
			emitins(i, e);
		}
		
		/* If we have a pending instruction at end of block, emit it now */
		if (prev) {
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
			for (r=arm64_rclob; *r>=0; r++)
				if (e->fn->reg & BIT(*r)) {
					s -= 2;
					i = &(Ins){Oload, 0, TMP(*r), {SLOT(s)}};
					i->cls = *r >= V0 ? Kd : Kl;
					emitins(i, e);
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
			fprintf(e->f,
				"\tb%s\t%s%d\n",
				ctoa[c], T.asloc, id0+b->s2->id
			);
			goto Jmp;
		}
	}
	id0 += e->fn->nblk;
	if (!T.apple)
		elf_emitfnfin(fn->name, out);
}
