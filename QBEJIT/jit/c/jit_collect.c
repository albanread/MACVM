/* jit_collect.c — Populate JitInst[] from QBE's post-regalloc Fn*
 *
 * This is the C-side counterpart of jit_encode.zig. It walks QBE's
 * internal representation after register allocation and instruction
 * selection, producing a flat array of JitInst records that the Zig
 * ARM64 encoder can consume.
 *
 * The flow mirrors arm64/emit.c's arm64_emitfn() but instead of
 * fprintf()ing assembly text, we append structured JitInst records.
 *
 * Fusion passes (MADD, shift, LDP/STP, indexed addressing) are
 * replicated here so the JitInst stream matches what the assembly
 * emitter would have produced.
 */

#include "arm64/all.h"
#include "config.h"
#include "jit_collect.h"
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <inttypes.h>

/* ── Opcode histogram (accumulated across batch runs) ───────────── */

static uint64_t jit_histogram[JIT_INST_KIND_COUNT];
static uint64_t jit_histogram_total;

/* ── Forward declarations ───────────────────────────────────────── */

/* from arm64/emit.c — fusion kill-switch checks */
extern int is_madd_fusion_enabled(void);
extern int is_shift_fusion_enabled(void);
extern int is_ldp_stp_fusion_enabled(void);
extern int is_indexed_addr_enabled(void);
extern int is_neon_copy_enabled(void);
extern int is_neon_arith_enabled(void);

/* from arm64/emit.c — helpers we reuse */
extern int prev_result_used_later(Ins *i, Blk *b, Ref prev_to);
extern int mem_pair_class(Ins *i);
extern int pair_class_size(int pc);
extern int pair_class_k(int pc);

/* ── Internal collector state (per-function) ────────────────────── */

typedef struct JC {
    JitCollector *jc;
    Fn           *fn;
    uint64_t      frame;
    uint          padding;
} JC;

/* ── Helpers ────────────────────────────────────────────────────── */

/* Grow the instruction buffer if needed. Returns NULL on OOM. */
static JitInst *
jit_grow(JitCollector *jc)
{
    if (jc->ninst >= jc->inst_cap) {
        uint32_t newcap = jc->inst_cap ? jc->inst_cap * 2 : 1024;
        JitInst *p = realloc(jc->insts, newcap * sizeof(JitInst));
        if (!p) {
            jc->error = -1;
            snprintf(jc->error_msg, sizeof jc->error_msg,
                     "jit_collect: realloc failed (%u insts)", newcap);
            return NULL;
        }
        jc->insts = p;
        jc->inst_cap = newcap;
    }
    JitInst *slot = &jc->insts[jc->ninst++];
    memset(slot, 0, sizeof *slot);
    slot->rd = JIT_REG_NONE;
    slot->rn = JIT_REG_NONE;
    slot->rm = JIT_REG_NONE;
    slot->ra = JIT_REG_NONE;
    slot->target_id = -1;
    return slot;
}

/* Map a QBE register id to JitInst register convention.
 *
 * QBE register IDs after regalloc:
 *   R0..R28, FP(=R29), LR(=R30), SP (GP regs)
 *   V0..V30 (NEON regs — mapped via JIT_VREG_BASE)
 *   IP0, IP1 (scratch, = R16, R17 in QBE enum)
 *
 * The jit_collect.h sentinel values must match arm64/all.h layout.
 */
static int32_t
mapreg(int r)
{
    if (r >= R0 && r <= R15)
        return (int32_t)(r - R0);
    if (r == IP0) return JIT_REG_IP0;
    if (r == IP1) return JIT_REG_IP1;
    if (r >= R18 && r <= R28)
        return (int32_t)(r - R0);   /* R18=18, ..., R28=28 */
    if (r == FP)  return JIT_REG_FP;
    if (r == LR)  return JIT_REG_LR;
    if (r == SP)  return JIT_REG_SP;
    if (r >= V0 && r <= V30)
        return JIT_VREG_BASE - (int32_t)(r - V0);
    return JIT_REG_NONE;
}

/* QBE Kw/Kl/Ks/Kd → JitCls */
static uint8_t
mapcls(int k)
{
    switch (k) {
    case Kw: return JIT_CLS_W;
    case Kl: return JIT_CLS_L;
    case Ks: return JIT_CLS_S;
    case Kd: return JIT_CLS_D;
    default: return JIT_CLS_W;
    }
}

/* QBE CmpI/CmpF condition → JitCond (ARM64 condition code) */
static uint8_t
mapcond(int c)
{
    /* The CMP(X) table in arm64/emit.c maps QBE comparison codes
     * to ARM64 condition strings. We replicate the mapping here
     * as numeric condition codes. */
    switch (c) {
    case Cieq:           return JIT_COND_EQ;
    case Cine:           return JIT_COND_NE;
    case Cisge:          return JIT_COND_GE;
    case Cisgt:          return JIT_COND_GT;
    case Cisle:          return JIT_COND_LE;
    case Cislt:          return JIT_COND_LT;
    case Ciuge:          return JIT_COND_CS;
    case Ciugt:          return JIT_COND_HI;
    case Ciule:          return JIT_COND_LS;
    case Ciult:          return JIT_COND_CC;
    case NCmpI+Cfeq:     return JIT_COND_EQ;
    case NCmpI+Cfge:     return JIT_COND_GE;
    case NCmpI+Cfgt:     return JIT_COND_GT;
    case NCmpI+Cfle:     return JIT_COND_LS;
    case NCmpI+Cflt:     return JIT_COND_MI;
    case NCmpI+Cfne:     return JIT_COND_NE;
    case NCmpI+Cfo:      return JIT_COND_VC;
    case NCmpI+Cfuo:     return JIT_COND_VS;
    default:             return JIT_COND_AL;
    }
}

/* Compute frame slot offset (mirrors arm64/emit.c slot()) */
static uint64_t
jc_slot(Ref r, JC *e)
{
    int s = rsval(r);
    if (s == -1)
        return 16 + e->frame;
    if (s < 0) {
        if (e->fn->vararg && !T.apple)
            return 16 + e->frame + 192 - (s+2);
        else
            return 16 + e->frame - (s+2);
    }
    return 16 + e->padding + 4 * (uint64_t)s;
}

/* Compute frame layout (mirrors arm64/emit.c framelayout()) */
static void
jc_framelayout(JC *e)
{
    int *r;
    uint o;
    uint64_t f;

    for (o=0, r=arm64_rclob; *r>=0; r++)
        o += 1 & (e->fn->reg >> *r);
    f = e->fn->slot;
    f = (f + 3) & ~(uint64_t)3;
    o += o & 1;
    e->padding = 4*(unsigned)(f - e->fn->slot);
    e->frame = 4*f + 8*o;
}

/* ── Ref → slot fixup ───────────────────────────────────────────── */

/* When a Ref is an RSlot whose offset exceeds the load/store
 * immediate range, we must emit an ADD to compute the address
 * into a scratch register, then replace the ref with that reg.
 *
 * Returns 1 if the fixup failed (no scratch register available),
 * 0 otherwise.
 */
static int
jc_fixarg(Ref *pr, int sz, int scratch_reg, JC *e)
{
    uint64_t s;
    Ref r = *pr;

    if (rtype(r) != RSlot)
        return 0;

    s = jc_slot(r, e);
    if (s <= (uint64_t)sz * 4095u)
        return 0;

    if (scratch_reg < 0)
        return 1;

    /* Emit: ADD scratch, FP, #slot_offset */
    JitInst *ji = jit_grow(e->jc);
    if (!ji) return 1;

    if (s <= 4095) {
        ji->kind = JIT_ADD_RRI;
        ji->cls = JIT_CLS_L;
        ji->rd = mapreg(scratch_reg);
        ji->rn = JIT_REG_FP;
        ji->imm = (int64_t)s;
    } else if (s <= 65535) {
        /* MOV scratch, #s; ADD scratch, FP, scratch */
        ji->kind = JIT_MOV_WIDE_IMM;
        ji->cls = JIT_CLS_L;
        ji->rd = mapreg(scratch_reg);
        ji->imm = (int64_t)s;

        JitInst *ji2 = jit_grow(e->jc);
        if (!ji2) return 1;
        ji2->kind = JIT_ADD_RRR;
        ji2->cls = JIT_CLS_L;
        ji2->rd = mapreg(scratch_reg);
        ji2->rn = JIT_REG_FP;
        ji2->rm = mapreg(scratch_reg);
    } else {
        ji->kind = JIT_MOV_WIDE_IMM;
        ji->cls = JIT_CLS_L;
        ji->rd = mapreg(scratch_reg);
        ji->imm = (int64_t)s;

        JitInst *ji2 = jit_grow(e->jc);
        if (!ji2) return 1;
        ji2->kind = JIT_ADD_RRR;
        ji2->cls = JIT_CLS_L;
        ji2->rd = mapreg(scratch_reg);
        ji2->rn = JIT_REG_FP;
        ji2->rm = mapreg(scratch_reg);
    }

    *pr = TMP(scratch_reg);
    return 0;
}

/* ── Emit a load-constant sequence ──────────────────────────────── */

static void
jc_loadcon(Con *c, int r, int k, JC *e)
{
    int64_t n;
    int w, sh;
    int32_t jr = mapreg(r);
    uint8_t jcls = mapcls(k);

    w = KWIDE(k);
    n = c->bits.i;

    if (c->type == CAddr) {
        /* Address of symbol — emit LOAD_ADDR pseudo */
        JitInst *ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_LOAD_ADDR;
        ji->cls = JIT_CLS_L;
        ji->rd = jr;

        char *l = str(c->sym.id);
        if (l) {
            size_t len = strlen(l);
            if (len >= JIT_SYM_MAX) len = JIT_SYM_MAX - 1;
            memcpy(ji->sym_name, l, len);
            ji->sym_name[len] = 0;
        }
        ji->sym_type = (c->sym.type == SThr) ? JIT_SYM_THREAD_LOCAL
                                              : JIT_SYM_GLOBAL;
        ji->imm = c->bits.i; /* offset, if any */
        return;
    }

    /* CBits: numeric constant */
    if (!w)
        n = (int32_t)n;

    /* Try single MOV (via movn/logical immediate) for simple values */
    if ((n | 0xffff) == -1 || arm64_logimm(n, k)) {
        JitInst *ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_MOV_WIDE_IMM;
        ji->cls = jcls;
        ji->rd = jr;
        ji->imm = n;
        return;
    }

    /* Multi-instruction MOVZ + MOVK sequence */
    {
        JitInst *ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_MOVZ;
        ji->cls = jcls;
        ji->rd = jr;
        ji->imm = n & 0xffff;
        ji->imm2 = 0;
    }
    {
        int64_t shifted = n;
        for (sh = 16; shifted >>= 16; sh += 16) {
            if ((!w && sh == 32) || sh == 64)
                break;
            if ((shifted & 0xffff) == 0)
                continue;
            JitInst *ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_MOVK;
            ji->cls = jcls;
            ji->rd = jr;
            ji->imm = shifted & 0xffff;
            ji->imm2 = sh;
        }
    }
}

/* ── Emit a memory reference ────────────────────────────────────── */

/* Convert a Ref to a base register + offset for load/store.
 * If the ref is a register, offset is 0.
 * If the ref is a slot, offset is computed from the frame layout.
 */
static void
jc_memref(Ref r, JC *e, int32_t *out_base, int64_t *out_offset)
{
    if (rtype(r) == RTmp) {
        *out_base = mapreg(r.val);
        *out_offset = 0;
    } else if (rtype(r) == RSlot) {
        *out_base = JIT_REG_FP;
        *out_offset = (int64_t)jc_slot(r, e);
    } else {
        *out_base = JIT_REG_NONE;
        *out_offset = 0;
    }
}

/* ── Neon arrangement helpers ───────────────────────────────────── */

static int
jc_neon_is_float(int ac)
{
    return (ac == Ks || ac == Kd
         || ac == 2  /* JIT_NEON_4SF */
         || ac == 3  /* JIT_NEON_2DF */);
}

static uint8_t
jc_neon_arr(int ac)
{
    switch (ac) {
    case Kw: return JIT_NEON_4S;
    case Kl: return JIT_NEON_2D;
    case Ks: return JIT_NEON_4SF;
    case Kd: return JIT_NEON_2DF;
    case 4:  return JIT_NEON_8H;
    case 5:  return JIT_NEON_16B;
    default: return JIT_NEON_4S;
    }
}

static int
jc_neon_arr_from_arg(Ins *i, JC *e)
{
    if (rtype(i->arg[0]) == RCon) {
        Con *c = &e->fn->con[i->arg[0].val];
        if (c->type == CBits) {
            int v = (int)c->bits.i;
            if (v >= 0 && v <= 5)
                return v;
        }
    }
    return i->cls;
}

/* ── Collect a single QBE instruction ───────────────────────────── */

static void
jc_ins(Ins *i, JC *e)
{
    JitInst *ji;
    Con *c;
    int k = i->cls;
    uint8_t jcls = mapcls(k);

    switch (i->op) {
    default:
        /* Check for table-driven instructions first */

        /* Handle loads with fixarg */
        if (isload(i->op)) {
            Ref addr = i->arg[0];
            jc_fixarg(&addr, loadsz(i), IP1, e);
            int32_t base;
            int64_t offset;
            jc_memref(addr, e, &base, &offset);

            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = base;
            ji->imm = offset;

            switch (i->op) {
            case Oloadsb:
                ji->kind = (k == Kl) ? JIT_LDRSB_RI : JIT_LDRSB_RI;
                ji->kind = JIT_LDRSB_RI;
                break;
            case Oloadub:
                ji->kind = JIT_LDRB_RI;
                ji->cls = JIT_CLS_W; /* always W dest for byte loads */
                break;
            case Oloadsh:
                ji->kind = JIT_LDRSH_RI;
                break;
            case Oloaduh:
                ji->kind = JIT_LDRH_RI;
                ji->cls = JIT_CLS_W;
                break;
            case Oloadsw:
                if (k == Kl)
                    ji->kind = JIT_LDRSW_RI;
                else
                    ji->kind = JIT_LDR_RI;
                break;
            case Oloaduw:
                ji->kind = JIT_LDR_RI;
                ji->cls = JIT_CLS_W;
                break;
            case Oload:
                ji->kind = JIT_LDR_RI;
                break;
            default:
                ji->kind = JIT_LDR_RI;
                break;
            }
            return;
        }

        /* Handle stores with fixarg */
        if (isstore(i->op)) {
            Ref val = i->arg[0];
            Ref addr = i->arg[1];
            int t = T.apple ? -1 : R18;
            jc_fixarg(&addr, storesz(i), t < 0 ? IP1 : t, e);

            int32_t base;
            int64_t offset;
            jc_memref(addr, e, &base, &offset);

            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->cls = JIT_CLS_W; /* stores use Kw cls in QBE */
            ji->rn = base;
            ji->imm = offset;

            switch (i->op) {
            case Ostoreb:
                ji->kind = JIT_STRB_RI;
                ji->rd = mapreg(val.val);
                break;
            case Ostoreh:
                ji->kind = JIT_STRH_RI;
                ji->rd = mapreg(val.val);
                break;
            case Ostorew:
                ji->kind = JIT_STR_RI;
                ji->rd = mapreg(val.val);
                break;
            case Ostorel:
                ji->kind = JIT_STR_RI;
                ji->cls = JIT_CLS_L;
                ji->rd = mapreg(val.val);
                break;
            case Ostores:
                ji->kind = JIT_STR_RI;
                ji->cls = JIT_CLS_S;
                ji->rd = mapreg(val.val);
                break;
            case Ostored:
                ji->kind = JIT_STR_RI;
                ji->cls = JIT_CLS_D;
                ji->rd = mapreg(val.val);
                break;
            default:
                ji->kind = JIT_STR_RI;
                ji->rd = mapreg(val.val);
                break;
            }
            return;
        }

        /* Fall through to table-driven ops */

        /* ── Integer ALU (3-register) ── */
        if (i->op == Oadd && KBASE(k) == 0) {
            /* Check if arg[1] is an immediate */
            if (rtype(i->arg[1]) == RCon) {
                c = &e->fn->con[i->arg[1].val];
                if (c->type == CBits) {
                    uint64_t n = (uint64_t)c->bits.i;
                    ji = jit_grow(e->jc);
                    if (!ji) return;
                    if (n <= 0xfff || (n & 0xfff000) == n) {
                        ji->kind = JIT_ADD_RRI;
                        ji->cls = jcls;
                        ji->rd = mapreg(i->to.val);
                        ji->rn = mapreg(i->arg[0].val);
                        ji->imm = (int64_t)n;
                    } else {
                        /* Use logical immediate or fall back */
                        ji->kind = JIT_ADD_RRI;
                        ji->cls = jcls;
                        ji->rd = mapreg(i->to.val);
                        ji->rn = mapreg(i->arg[0].val);
                        ji->imm = c->bits.i;
                    }
                    return;
                }
            }
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_ADD_RRR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            return;
        }
        if (i->op == Oadd && KBASE(k) == 1) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_FADD_RRR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            return;
        }
        if (i->op == Osub && KBASE(k) == 0) {
            if (rtype(i->arg[1]) == RCon) {
                c = &e->fn->con[i->arg[1].val];
                if (c->type == CBits) {
                    ji = jit_grow(e->jc);
                    if (!ji) return;
                    ji->kind = JIT_SUB_RRI;
                    ji->cls = jcls;
                    ji->rd = mapreg(i->to.val);
                    ji->rn = mapreg(i->arg[0].val);
                    ji->imm = c->bits.i;
                    return;
                }
            }
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_SUB_RRR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            return;
        }
        if (i->op == Osub && KBASE(k) == 1) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_FSUB_RRR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            return;
        }
        if (i->op == Omul && KBASE(k) == 0) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_MUL_RRR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            return;
        }
        if (i->op == Omul && KBASE(k) == 1) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_FMUL_RRR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            return;
        }
        if (i->op == Odiv && KBASE(k) == 0) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_SDIV_RRR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            return;
        }
        if (i->op == Odiv && KBASE(k) == 1) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_FDIV_RRR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            return;
        }
        if (i->op == Oudiv) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_UDIV_RRR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            return;
        }
        if (i->op == Orem) {
            /* sdiv ip1, rn, rm; msub rd, ip1, rm, rn */
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_COMMENT;
            snprintf(ji->sym_name, JIT_SYM_MAX, "MOD: SDIV+MSUB sequence");

            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_SDIV_RRR;
            ji->cls = jcls;
            ji->rd = JIT_REG_IP1;
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);

            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_MSUB_RRRR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = JIT_REG_IP1;
            ji->rm = mapreg(i->arg[1].val);
            ji->ra = mapreg(i->arg[0].val);
            return;
        }
        if (i->op == Ourem) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_COMMENT;
            snprintf(ji->sym_name, JIT_SYM_MAX, "UMOD: UDIV+MSUB sequence");

            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_UDIV_RRR;
            ji->cls = jcls;
            ji->rd = JIT_REG_IP1;
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);

            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_MSUB_RRRR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = JIT_REG_IP1;
            ji->rm = mapreg(i->arg[1].val);
            ji->ra = mapreg(i->arg[0].val);
            return;
        }
        if (i->op == Oneg && KBASE(k) == 0) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_NEG_RR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            return;
        }
        if (i->op == Oneg && KBASE(k) == 1) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_FNEG_RR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            return;
        }
        if (i->op == Oand) {
            if (rtype(i->arg[1]) == RCon) {
                c = &e->fn->con[i->arg[1].val];
                if (c->type == CBits && arm64_logimm(c->bits.i, k)) {
                    /* AND with logical immediate — handled by encoder
                     * via MOV_WIDE_IMM fallback in the Zig side.
                     * For now, load the immediate and use register form. */
                }
            }
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_AND_RRR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            return;
        }
        if (i->op == Oor) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_ORR_RRR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            return;
        }
        if (i->op == Oxor) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_EOR_RRR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            return;
        }
        if (i->op == Osar) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_ASR_RRR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            return;
        }
        if (i->op == Oshr) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_LSR_RRR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            return;
        }
        if (i->op == Oshl) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_LSL_RRR;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            return;
        }

        /* ── Extensions ── */
        if (i->op == Oextsb) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_SXTB;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            return;
        }
        if (i->op == Oextub) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_UXTB;
            ji->cls = JIT_CLS_W;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            return;
        }
        if (i->op == Oextsh) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_SXTH;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            return;
        }
        if (i->op == Oextuh) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_UXTH;
            ji->cls = JIT_CLS_W;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            return;
        }
        if (i->op == Oextsw) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_SXTW;
            ji->cls = JIT_CLS_L;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            return;
        }
        if (i->op == Oextuw) {
            /* MOV Wd, Wn (clears upper 32 bits) */
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_UXTW;
            ji->cls = JIT_CLS_W;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            return;
        }

        /* ── Float conversions ── */
        if (i->op == Oexts) {
            /* fcvt Dd, Sn  (single→double) */
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_FCVT_SD;
            ji->cls = JIT_CLS_D;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            return;
        }
        if (i->op == Otruncd) {
            /* fcvt Sd, Dn  (double→single) */
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_FCVT_DS;
            ji->cls = JIT_CLS_S;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            return;
        }
        if (i->op == Ocast) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            if (k == Kw || k == Kl) {
                /* fmov Wd/Xd, Sn/Dn */
                ji->kind = JIT_FMOV_GF;
            } else {
                /* fmov Sd/Dd, Wn/Xn */
                ji->kind = JIT_FMOV_FG;
            }
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            return;
        }
        if (i->op == Ostosi || i->op == Odtosi) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_FCVTZS;
            /* cls must reflect the SOURCE fp type (S or D), not the
               destination int type, so the encoder picks the correct
               fcvtzs Wd,Dn vs fcvtzs Wd,Sn variant.
               is_float carries dest-is-64-bit (Kl) so the encoder can
               emit fcvtzs Xd,Dn when the result is a 64-bit integer. */
            ji->cls = (i->op == Odtosi) ? JIT_CLS_D : JIT_CLS_S;
            ji->is_float = (i->cls == Kl) ? 1 : 0;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            return;
        }
        if (i->op == Ostoui || i->op == Odtoui) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_FCVTZU;
            ji->cls = (i->op == Odtoui) ? JIT_CLS_D : JIT_CLS_S;
            ji->is_float = (i->cls == Kl) ? 1 : 0;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            return;
        }
        if (i->op == Oswtof || i->op == Osltof) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_SCVTF;
            /* cls = dest FP type (S or D) for scalarSizeFromCls.
               is_float carries source-is-64-bit so the encoder can
               emit scvtf Dd,Xn when the source is a 64-bit integer. */
            ji->cls = jcls;
            ji->is_float = (i->op == Osltof) ? 1 : 0;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            return;
        }
        if (i->op == Ouwtof || i->op == Oultof) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_UCVTF;
            ji->cls = jcls;
            ji->is_float = (i->op == Oultof) ? 1 : 0;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            return;
        }

        /* ── Compare ── */
        if (i->op == Oacmp) {
            if (rtype(i->arg[1]) == RCon) {
                c = &e->fn->con[i->arg[1].val];
                if (c->type == CBits) {
                    ji = jit_grow(e->jc);
                    if (!ji) return;
                    ji->kind = JIT_CMP_RI;
                    ji->cls = jcls;
                    ji->rn = mapreg(i->arg[0].val);
                    ji->imm = c->bits.i;
                    return;
                }
            }
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_CMP_RR;
            ji->cls = jcls;
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            return;
        }
        if (i->op == Oacmn) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_CMN_RR;
            ji->cls = jcls;
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            return;
        }
        if (i->op == Oafcmp) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_FCMP_RR;
            ji->cls = jcls;
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            return;
        }

        /* ── Flag / conditional set ── */
        if (i->op >= Oflag && i->op <= Oflag1) {
            int cc = i->op - Oflag;
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_CSET;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->cond = mapcond(cc);
            return;
        }

        /* ── Conditional select ── */
        if (isxsel(i->op)) {
            int cc = i->op - Oxsel;
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_CSEL;
            ji->cls = jcls;
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            ji->rm = mapreg(i->arg[1].val);
            ji->cond = mapcond(cc);
            return;
        }

        /* Unhandled instruction — emit a comment diagnostic */
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_COMMENT;
        snprintf(ji->sym_name, JIT_SYM_MAX, "unhandled op %d", i->op);
        return;

    case Onop:
        /* QBE nops are silent */
        return;

    case Ocopy:
        if (req(i->to, i->arg[0]))
            return; /* self-copy, skip */

        if (rtype(i->to) == RSlot) {
            /* Copy to stack slot → store */
            Ref val_ref = i->arg[0];

            if (!isreg(val_ref)) {
                /* Value not in register — load into IP1 first */
                if (rtype(val_ref) == RCon) {
                    jc_loadcon(&e->fn->con[val_ref.val], IP1, k, e);
                    val_ref = TMP(IP1);
                } else if (rtype(val_ref) == RSlot) {
                    /* Load from source slot into IP1, then store */
                    int32_t src_base;
                    int64_t src_off;
                    jc_memref(val_ref, e, &src_base, &src_off);
                    ji = jit_grow(e->jc);
                    if (!ji) return;
                    ji->kind = JIT_LDR_RI;
                    ji->cls = jcls;
                    ji->rd = JIT_REG_IP0;
                    ji->rn = src_base;
                    ji->imm = src_off;
                    val_ref = TMP(IP0);
                }
            }

            /* Now emit the store to the slot */
            {
                int32_t dst_base;
                int64_t dst_off;
                jc_memref(i->to, e, &dst_base, &dst_off);
                ji = jit_grow(e->jc);
                if (!ji) return;
                switch (k) {
                case Kw: ji->kind = JIT_STR_RI; ji->cls = JIT_CLS_W; break;
                case Kl: ji->kind = JIT_STR_RI; ji->cls = JIT_CLS_L; break;
                case Ks: ji->kind = JIT_STR_RI; ji->cls = JIT_CLS_S; break;
                case Kd: ji->kind = JIT_STR_RI; ji->cls = JIT_CLS_D; break;
                default: ji->kind = JIT_STR_RI; ji->cls = JIT_CLS_W; break;
                }
                ji->rd = mapreg(rtype(val_ref) == RTmp ? val_ref.val : IP1);
                ji->rn = dst_base;
                ji->imm = dst_off;
            }
            return;
        }

        /* Copy to register */
        switch (rtype(i->arg[0])) {
        case RCon:
            c = &e->fn->con[i->arg[0].val];
            jc_loadcon(c, i->to.val, k, e);
            return;
        case RSlot:
            /* Load from slot */
            {
                int32_t base;
                int64_t offset;
                jc_memref(i->arg[0], e, &base, &offset);
                ji = jit_grow(e->jc);
                if (!ji) return;
                ji->kind = JIT_LDR_RI;
                ji->cls = jcls;
                ji->rd = mapreg(i->to.val);
                ji->rn = base;
                ji->imm = offset;
            }
            return;
        default:
            /* Register-register copy */
            ji = jit_grow(e->jc);
            if (!ji) return;
            if (KBASE(k) == 0) {
                ji->kind = JIT_MOV_RR;
                ji->cls = jcls;
            } else {
                ji->kind = JIT_FMOV_RR;
                ji->cls = jcls;
            }
            ji->rd = mapreg(i->to.val);
            ji->rn = mapreg(i->arg[0].val);
            return;
        }

    case Oswap:
        /* Swap via temp: mov ip1, r0; mov r0, r1; mov r1, ip1 */
        if (KBASE(k) == 0) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_MOV_RR; ji->cls = jcls;
            ji->rd = JIT_REG_IP1;
            ji->rn = mapreg(i->arg[0].val);

            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_MOV_RR; ji->cls = jcls;
            ji->rd = mapreg(i->arg[0].val);
            ji->rn = mapreg(i->arg[1].val);

            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_MOV_RR; ji->cls = jcls;
            ji->rd = mapreg(i->arg[1].val);
            ji->rn = JIT_REG_IP1;
        } else {
            /* FP swap uses V31 as temp */
            int32_t v31 = JIT_VREG_BASE - 31;
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_FMOV_RR; ji->cls = jcls;
            ji->rd = v31;
            ji->rn = mapreg(i->arg[0].val);

            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_FMOV_RR; ji->cls = jcls;
            ji->rd = mapreg(i->arg[0].val);
            ji->rn = mapreg(i->arg[1].val);

            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_FMOV_RR; ji->cls = jcls;
            ji->rd = mapreg(i->arg[1].val);
            ji->rn = v31;
        }
        return;

    case Oaddr:
        /* Compute address of stack slot */
        {
            uint64_t s = jc_slot(i->arg[0], e);
            ji = jit_grow(e->jc);
            if (!ji) return;
            if (s <= 4095) {
                ji->kind = JIT_ADD_RRI;
                ji->cls = JIT_CLS_L;
                ji->rd = mapreg(i->to.val);
                ji->rn = JIT_REG_FP;
                ji->imm = (int64_t)s;
            } else {
                /* Large offset: load imm, then add */
                ji->kind = JIT_MOV_WIDE_IMM;
                ji->cls = JIT_CLS_L;
                ji->rd = mapreg(i->to.val);
                ji->imm = (int64_t)s;

                ji = jit_grow(e->jc);
                if (!ji) return;
                ji->kind = JIT_ADD_RRR;
                ji->cls = JIT_CLS_L;
                ji->rd = mapreg(i->to.val);
                ji->rn = JIT_REG_FP;
                ji->rm = mapreg(i->to.val);
            }
        }
        return;

    case Ocall:
        if (rtype(i->arg[0]) == RCon) {
            c = &e->fn->con[i->arg[0].val];
            if (c->type == CAddr && c->sym.type == SGlo && !c->bits.i) {
                char *l = str(c->sym.id);
                ji = jit_grow(e->jc);
                if (!ji) return;
                ji->kind = JIT_CALL_EXT;
                if (l) {
                    char *p = (l[0] == '"') ? "" : T.assym;
                    size_t plen = strlen(p);
                    size_t llen = strlen(l);
                    if (plen + llen >= JIT_SYM_MAX)
                        llen = JIT_SYM_MAX - plen - 1;
                    memcpy(ji->sym_name, p, plen);
                    memcpy(ji->sym_name + plen, l, llen);
                    ji->sym_name[plen + llen] = 0;
                }
                ji->sym_type = JIT_SYM_FUNC;
                return;
            }
        }
        /* Indirect call: BLR */
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_BLR;
        ji->cls = JIT_CLS_L;
        ji->rn = mapreg(i->arg[0].val);
        return;

    case Osalloc:
        /* SUB SP, SP, arg[0] */
        ji = jit_grow(e->jc);
        if (!ji) return;
        if (rtype(i->arg[0]) == RCon) {
            c = &e->fn->con[i->arg[0].val];
            ji->kind = JIT_SUB_SP;
            ji->imm = c->bits.i;
        } else {
            /* Dynamic alloc: SUB SP, SP, Xn */
            ji->kind = JIT_SUB_RRR;
            ji->cls = JIT_CLS_L;
            ji->rd = JIT_REG_SP;
            ji->rn = JIT_REG_SP;
            ji->rm = mapreg(i->arg[0].val);
        }
        /* If result is used: MOV rd, SP */
        if (!req(i->to, R)) {
            ji = jit_grow(e->jc);
            if (!ji) return;
            ji->kind = JIT_MOV_SP;
            ji->cls = JIT_CLS_L;
            ji->rd = mapreg(i->to.val);
            ji->rn = JIT_REG_SP;
        }
        return;

    case Odbgloc:
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_DBGLOC;
        ji->imm = i->arg[0].val;
        ji->imm2 = i->arg[1].val;
        return;

    /* ── NEON vector operations ── */

    case Oneonldr:
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_NEON_LDR_Q;
        ji->rn = mapreg(i->arg[0].val);
        return;

    case Oneonstr:
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_NEON_STR_Q;
        ji->rn = mapreg(i->arg[0].val);
        return;

    case Oneonldr2:
        /* Load into q29 — emit as LDR with comment for second vector */
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_NEON_LDR_Q;
        ji->rn = mapreg(i->arg[0].val);
        /* The Zig side uses fixed V28; we note this is V29 in imm2 */
        ji->imm2 = 29;
        return;

    case Oneonstr2:
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_NEON_STR_Q;
        ji->rn = mapreg(i->arg[0].val);
        ji->imm2 = 29;
        return;

    case Oneonldr3:
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_NEON_LDR_Q;
        ji->rn = mapreg(i->arg[0].val);
        ji->imm2 = 30;
        return;

    case Oneonadd: {
        int ac = jc_neon_arr_from_arg(i, e);
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_NEON_ADD;
        ji->imm = jc_neon_arr(ac);
        ji->is_float = jc_neon_is_float(ac) ? 1 : 0;
        return;
    }

    case Oneonsub: {
        int ac = jc_neon_arr_from_arg(i, e);
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_NEON_SUB;
        ji->imm = jc_neon_arr(ac);
        ji->is_float = jc_neon_is_float(ac) ? 1 : 0;
        return;
    }

    case Oneonmul: {
        int ac = jc_neon_arr_from_arg(i, e);
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_NEON_MUL;
        ji->imm = jc_neon_arr(ac);
        ji->is_float = jc_neon_is_float(ac) ? 1 : 0;
        return;
    }

    case Oneondiv: {
        int ac = jc_neon_arr_from_arg(i, e);
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_NEON_DIV;
        ji->imm = jc_neon_arr(ac);
        ji->is_float = 1; /* NEON div only for float */
        return;
    }

    case Oneonneg: {
        int ac = jc_neon_arr_from_arg(i, e);
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_NEON_NEG;
        ji->imm = jc_neon_arr(ac);
        ji->is_float = jc_neon_is_float(ac) ? 1 : 0;
        return;
    }

    case Oneonabs: {
        int ac = jc_neon_arr_from_arg(i, e);
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_NEON_ABS;
        ji->imm = jc_neon_arr(ac);
        ji->is_float = jc_neon_is_float(ac) ? 1 : 0;
        return;
    }

    case Oneonfma: {
        int ac = jc_neon_arr_from_arg(i, e);
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_NEON_FMA;
        ji->imm = jc_neon_arr(ac);
        ji->is_float = jc_neon_is_float(ac) ? 1 : 0;
        return;
    }

    case Oneonmin: {
        int ac = jc_neon_arr_from_arg(i, e);
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_NEON_MIN;
        ji->imm = jc_neon_arr(ac);
        ji->is_float = jc_neon_is_float(ac) ? 1 : 0;
        return;
    }

    case Oneonmax: {
        int ac = jc_neon_arr_from_arg(i, e);
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_NEON_MAX;
        ji->imm = jc_neon_arr(ac);
        ji->is_float = jc_neon_is_float(ac) ? 1 : 0;
        return;
    }

    case Oneondup: {
        int ac = jc_neon_arr_from_arg(i, e);
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_NEON_DUP;
        ji->imm = jc_neon_arr(ac);
        ji->is_float = jc_neon_is_float(ac) ? 1 : 0;
        ji->rm = mapreg(i->arg[1].val);
        return;
    }

    case Oneonaddv: {
        int ac = jc_neon_arr_from_arg(i, e);
        ji = jit_grow(e->jc);
        if (!ji) return;
        ji->kind = JIT_NEON_ADDV;
        ji->imm = jc_neon_arr(ac);
        ji->is_float = jc_neon_is_float(ac) ? 1 : 0;
        ji->rd = mapreg(i->to.val);
        return;
    }
    }
}

/* ── Try MADD/MSUB fusion ───────────────────────────────────────── */

static int
jc_try_madd(Ins *i, Ins *prev, JC *e, Blk *b)
{
    Ref addend;
    int mul_in_arg0, mul_in_arg1;

    if (!prev || i->op != Oadd || prev->op != Omul)
        return 0;
    if (i->cls != prev->cls)
        return 0;
    mul_in_arg0 = req(i->arg[0], prev->to);
    mul_in_arg1 = req(i->arg[1], prev->to);
    if (!mul_in_arg0 && !mul_in_arg1)
        return 0;
    if (!isreg(prev->arg[0]) || !isreg(prev->arg[1]))
        return 0;
    if (!isreg(i->arg[0]) || !isreg(i->arg[1]))
        return 0;
    addend = mul_in_arg0 ? i->arg[1] : i->arg[0];
    if (req(addend, prev->to))
        return 0;
    if (prev_result_used_later(i, b, prev->to))
        return 0;

    JitInst *ji = jit_grow(e->jc);
    if (!ji) return 0;
    ji->kind = JIT_COMMENT;
    snprintf(ji->sym_name, JIT_SYM_MAX, "fused: MUL+ADD -> MADD");

    ji = jit_grow(e->jc);
    if (!ji) return 0;

    if (KBASE(i->cls) == 0)
        ji->kind = JIT_MADD_RRRR;
    else {
        /* For FP we can't directly emit FMADD through the JitInst IR
         * without extending it. Emit as separate MUL + ADD. */
        /* TODO: add JIT_FMADD_RRRR */
        return 0;
    }

    ji->cls = mapcls(i->cls);
    ji->rd = mapreg(i->to.val);
    ji->rn = mapreg(prev->arg[0].val);
    ji->rm = mapreg(prev->arg[1].val);
    ji->ra = mapreg(addend.val);
    return 1;
}

static int
jc_try_msub(Ins *i, Ins *prev, JC *e, Blk *b)
{
    Ref minuend;
    int mul_in_arg1;

    if (!prev || i->op != Osub || prev->op != Omul)
        return 0;
    if (i->cls != prev->cls)
        return 0;
    /* SUB dest, minuend, mul_result → MSUB dest, mul_op1, mul_op2, minuend */
    mul_in_arg1 = req(i->arg[1], prev->to);
    if (!mul_in_arg1)
        return 0;
    if (!isreg(prev->arg[0]) || !isreg(prev->arg[1]))
        return 0;
    if (!isreg(i->arg[0]) || !isreg(i->arg[1]))
        return 0;
    minuend = i->arg[0];
    if (req(minuend, prev->to))
        return 0;
    if (prev_result_used_later(i, b, prev->to))
        return 0;
    if (KBASE(i->cls) != 0)
        return 0; /* only integer MSUB for now */

    JitInst *ji = jit_grow(e->jc);
    if (!ji) return 0;
    ji->kind = JIT_COMMENT;
    snprintf(ji->sym_name, JIT_SYM_MAX, "fused: MUL-SUB -> MSUB");

    ji = jit_grow(e->jc);
    if (!ji) return 0;
    ji->kind = JIT_MSUB_RRRR;
    ji->cls = mapcls(i->cls);
    ji->rd = mapreg(i->to.val);
    ji->rn = mapreg(prev->arg[0].val);
    ji->rm = mapreg(prev->arg[1].val);
    ji->ra = mapreg(minuend.val);
    return 1;
}

/* ── Try shift fusion ───────────────────────────────────────────── */

static int
jc_try_shift_fusion(Ins *i, Ins *prev, JC *e, Blk *b)
{
    uint16_t kind;
    int shift_amt;
    uint8_t shift_type;
    int shift_in_arg0, shift_in_arg1;

    if (!prev)
        return 0;
    if (prev->op != Oshl && prev->op != Oshr && prev->op != Osar)
        return 0;
    /* The shift amount must be a constant */
    if (rtype(prev->arg[1]) != RCon)
        return 0;
    {
        Con *sc = &e->fn->con[prev->arg[1].val];
        if (sc->type != CBits)
            return 0;
        shift_amt = (int)sc->bits.i;
    }
    if (!isreg(prev->arg[0]))
        return 0;
    if (i->cls != prev->cls)
        return 0;
    if (KBASE(i->cls) != 0)
        return 0;

    /* Determine which argument of the consumer uses the shift result */
    shift_in_arg0 = req(i->arg[0], prev->to);
    shift_in_arg1 = req(i->arg[1], prev->to);
    if (!shift_in_arg0 && !shift_in_arg1)
        return 0;
    if (!isreg(i->arg[0]) || !isreg(i->arg[1]))
        return 0;
    if (prev_result_used_later(i, b, prev->to))
        return 0;

    /* Only ADD, SUB, AND, ORR, EOR can fuse with shifted operand */
    switch (i->op) {
    case Oadd: kind = JIT_ADD_SHIFT; break;
    case Osub:
        if (!shift_in_arg1) return 0; /* shift must be in arg1 for SUB */
        kind = JIT_SUB_SHIFT;
        break;
    case Oand: kind = JIT_AND_SHIFT; break;
    case Oor:  kind = JIT_ORR_SHIFT; break;
    case Oxor: kind = JIT_EOR_SHIFT; break;
    default:   return 0;
    }

    switch (prev->op) {
    case Oshl: shift_type = JIT_SHIFT_LSL; break;
    case Oshr: shift_type = JIT_SHIFT_LSR; break;
    case Osar: shift_type = JIT_SHIFT_ASR; break;
    default:   return 0;
    }

    /* For commutative ops with shift in arg0, swap so shifted reg is rm */
    int32_t rn_val, rm_val;
    if (shift_in_arg1 || i->op == Osub) {
        rn_val = mapreg(i->arg[0].val);
        rm_val = mapreg(prev->arg[0].val);
    } else {
        /* shift in arg0, commutative op: swap */
        rn_val = mapreg(i->arg[1].val);
        rm_val = mapreg(prev->arg[0].val);
    }

    JitInst *ji = jit_grow(e->jc);
    if (!ji) return 0;
    ji->kind = JIT_COMMENT;
    {
        const char *op_name = "alu";
        switch (i->op) {
        case Oadd: op_name = "ADD"; break;
        case Osub: op_name = "SUB"; break;
        case Oand: op_name = "AND"; break;
        case Oor:  op_name = "ORR"; break;
        case Oxor: op_name = "EOR"; break;
        default: break;
        }
        const char *sh_name = "?";
        switch (prev->op) {
        case Oshl: sh_name = "LSL"; break;
        case Oshr: sh_name = "LSR"; break;
        case Osar: sh_name = "ASR"; break;
        default: break;
        }
        snprintf(ji->sym_name, JIT_SYM_MAX,
                 "fused: %s(shifted) -> %s %s #%d",
                 op_name, op_name, sh_name, shift_amt);
    }

    ji = jit_grow(e->jc);
    if (!ji) return 0;
    ji->kind = kind;
    ji->cls = mapcls(i->cls);
    ji->rd = mapreg(i->to.val);
    ji->rn = rn_val;
    ji->rm = rm_val;
    ji->shift_type = shift_type;
    ji->imm2 = shift_amt;
    return 1;
}

/* ── Try LDP/STP fusion ─────────────────────────────────────────── */

static int
jc_try_ldp_stp(Ins *i, Ins *prev_mem, JC *e, Blk *b)
{
    int pc1, pc2, sz, k;
    int32_t base1, base2;
    int64_t off1, off2;

    pc1 = mem_pair_class(prev_mem);
    pc2 = mem_pair_class(i);
    if (!pc1 || !pc2 || pc1 != pc2)
        return 0;

    sz = pair_class_size(pc1);
    k = pair_class_k(pc1);

    /* Both must reference the same base register and adjacent offsets */
    jc_memref(isload(prev_mem->op) ? prev_mem->arg[0] : prev_mem->arg[1],
              e, &base1, &off1);
    jc_memref(isload(i->op) ? i->arg[0] : i->arg[1],
              e, &base2, &off2);

    if (base1 != base2)
        return 0;

    /* Check adjacency */
    int64_t lo_off, hi_off;
    Ins *lo_ins, *hi_ins;
    if (off1 < off2) {
        lo_off = off1; hi_off = off2;
        lo_ins = prev_mem; hi_ins = i;
    } else {
        lo_off = off2; hi_off = off1;
        lo_ins = i; hi_ins = prev_mem;
    }
    if (hi_off - lo_off != sz)
        return 0;

    /* Offset must be in range for LDP/STP (signed 7-bit scaled) */
    int64_t scaled = lo_off / sz;
    if (scaled < -64 || scaled > 63)
        return 0;
    if (lo_off % sz != 0)
        return 0;

    /* Determine operation type */
    int is_load = isload(prev_mem->op);

    JitInst *ji = jit_grow(e->jc);
    if (!ji) return 0;
    ji->kind = JIT_COMMENT;
    snprintf(ji->sym_name, JIT_SYM_MAX,
             "fused: %s+%s -> %s",
             is_load ? "LDR" : "STR",
             is_load ? "LDR" : "STR",
             is_load ? "LDP" : "STP");

    ji = jit_grow(e->jc);
    if (!ji) return 0;

    ji->kind = is_load ? JIT_LDP : JIT_STP;
    ji->cls = mapcls(k);
    ji->rn = base1;
    ji->imm = lo_off;

    if (is_load) {
        ji->rd = mapreg(lo_ins->to.val);
        ji->rm = mapreg(hi_ins->to.val);
    } else {
        ji->rd = mapreg(lo_ins->arg[0].val);
        ji->rm = mapreg(hi_ins->arg[0].val);
    }

    return 1;
}

/* ── CBZ/CBNZ fusion at block end ───────────────────────────────── */

static int
jc_try_cbz(Ins *prev, Blk *b, JC *e, int *out_cbz, int *out_reg, int *out_cls)
{
    if (!prev || prev->op != Oacmp)
        return 0;
    if (!isreg(prev->arg[0]))
        return 0;
    if (rtype(prev->arg[1]) != RCon)
        return 0;

    Con *c = &e->fn->con[prev->arg[1].val];
    if (c->type != CBits || c->bits.i != 0)
        return 0;

    if (b->jmp.type < Jjf || b->jmp.type > Jjf1)
        return 0;

    int jc = b->jmp.type - Jjf;
    int adj;
    if (b->link == b->s2)
        adj = jc;
    else
        adj = cmpneg(jc);

    if (adj == Cieq) {
        *out_cbz = 1; /* CBZ */
        *out_reg = prev->arg[0].val;
        *out_cls = prev->cls;
        return 1;
    }
    if (adj == Cine) {
        *out_cbz = 2; /* CBNZ */
        *out_reg = prev->arg[0].val;
        *out_cls = prev->cls;
        return 1;
    }
    return 0;
}

/* ═══════════════════════════════════════════════════════════════════
 * Public API
 * ═══════════════════════════════════════════════════════════════════*/

int
jit_collector_init(JitCollector *jc)
{
    memset(jc, 0, sizeof *jc);
    jc->inst_cap = 4096;
    jc->insts = calloc(jc->inst_cap, sizeof(JitInst));
    if (!jc->insts) {
        jc->error = -1;
        snprintf(jc->error_msg, sizeof jc->error_msg,
                 "jit_collector_init: calloc failed");
        return -1;
    }
    return 0;
}

void
jit_collector_free(JitCollector *jc)
{
    free(jc->insts);
    jc->insts = NULL;
    jc->ninst = 0;
    jc->inst_cap = 0;
}

void
jit_collector_reset(JitCollector *jc)
{
    jc->ninst = 0;
    jc->nfunc = 0;
    jc->ndata = 0;
    jc->error = 0;
    jc->error_msg[0] = 0;
}

JitInst *
jit_emit(JitCollector *jc)
{
    return jit_grow(jc);
}

/* ── Collect a complete function ────────────────────────────────── */

void
jit_collect_fn(JitCollector *jc, Fn *fn)
{
    static int id0 = 0;
    JC env;
    JitInst *ji;
    Ins *i;
    Blk *b, *t;
    int *r;
    int s, c, lbl;
    uint64_t o;

    env.jc = jc;
    env.fn = fn;
    jc_framelayout(&env);

    /* ── FUNC_BEGIN ── */
    ji = jit_grow(jc);
    if (!ji) return;
    ji->kind = JIT_FUNC_BEGIN;
    {
        size_t nlen = strlen(fn->name);
        if (nlen >= JIT_SYM_MAX) nlen = JIT_SYM_MAX - 1;
        memcpy(ji->sym_name, fn->name, nlen);
        ji->sym_name[nlen] = 0;
    }
    ji->imm = (int64_t)(env.frame + 16);

    /* Emit prologue frame-size comment for Capstone listing */
    ji = jit_grow(jc);
    if (!ji) return;
    ji->kind = JIT_COMMENT;
    snprintf(ji->sym_name, JIT_SYM_MAX, "prologue: frame=%d bytes",
             (int)(env.frame + 16));

    /* ── Prologue: HINT #34 (BTI C) ── */
    ji = jit_grow(jc);
    if (!ji) return;
    ji->kind = JIT_HINT;
    ji->imm = 34;

    /* ── Prologue: STP x29, x30, [sp, -frame]! ── */
    if (env.frame + 16 <= 512) {
        ji = jit_grow(jc);
        if (!ji) return;
        ji->kind = JIT_STP_PRE;
        ji->cls = JIT_CLS_L;
        ji->rd = JIT_REG_FP;
        ji->rm = JIT_REG_LR;
        ji->rn = JIT_REG_SP;
        ji->imm = -(int64_t)(env.frame + 16);
    } else {
        /* Large frame: SUB SP first, then STP */
        ji = jit_grow(jc);
        if (!ji) return;
        ji->kind = JIT_SUB_SP;
        ji->imm = (int64_t)env.frame;

        ji = jit_grow(jc);
        if (!ji) return;
        ji->kind = JIT_STP_PRE;
        ji->cls = JIT_CLS_L;
        ji->rd = JIT_REG_FP;
        ji->rm = JIT_REG_LR;
        ji->rn = JIT_REG_SP;
        ji->imm = -16;
    }

    /* ── MOV x29, sp ── */
    ji = jit_grow(jc);
    if (!ji) return;
    ji->kind = JIT_MOV_SP;
    ji->cls = JIT_CLS_L;
    ji->rd = JIT_REG_FP;
    ji->rn = JIT_REG_SP;

    /* ── Save callee-saved registers ── */
    s = (int)((env.frame - env.padding) / 4);
    for (r = arm64_rclob; *r >= 0; r++) {
        if (fn->reg & BIT(*r)) {
            s -= 2;
            uint64_t off = 16 + env.padding + 4 * (uint64_t)s;
            ji = jit_grow(jc);
            if (!ji) return;
            ji->kind = JIT_STR_RI;
            ji->cls = (*r >= V0) ? JIT_CLS_D : JIT_CLS_L;
            ji->rd = mapreg(*r);
            ji->rn = JIT_REG_FP;
            ji->imm = (int64_t)off;
        }
    }

    /* ── Basic blocks ── */
    for (lbl = 0, b = fn->start; b; b = b->link) {
        Ins *prev = NULL;

        /* Emit a comment with the QBE block name so Capstone
         * disassembly can display it alongside the machine code. */
        if (b->name[0]) {
            ji = jit_grow(jc);
            if (!ji) return;
            ji->kind = JIT_COMMENT;
            snprintf(ji->sym_name, JIT_SYM_MAX, "block @%s", b->name);
        }

        if (lbl || b->npred > 1) {
            ji = jit_grow(jc);
            if (!ji) return;
            ji->kind = JIT_LABEL;
            ji->target_id = (int32_t)(id0 + b->id);
        }

        for (i = b->ins; i != &b->ins[b->nins]; i++) {
            /* Try fusion with buffered previous instruction */
            if (prev) {
                if (is_madd_fusion_enabled() && prev->op == Omul) {
                    if (jc_try_madd(i, prev, &env, b)) {
                        prev = NULL;
                        continue;
                    }
                    if (jc_try_msub(i, prev, &env, b)) {
                        prev = NULL;
                        continue;
                    }
                }
                if (is_shift_fusion_enabled() &&
                    (prev->op == Oshl || prev->op == Oshr || prev->op == Osar)) {
                    if (jc_try_shift_fusion(i, prev, &env, b)) {
                        prev = NULL;
                        continue;
                    }
                }
                /* Emit the unfused pending instruction */
                jc_ins(prev, &env);
                prev = NULL;
            }

            /* Buffer fusible instructions */
            if ((is_madd_fusion_enabled() && i->op == Omul) ||
                (is_shift_fusion_enabled() &&
                 (i->op == Oshl || i->op == Oshr || i->op == Osar) &&
                 rtype(i->arg[1]) == RCon) ||
                i->op == Oacmp) {
                prev = i;
                continue;
            }

            jc_ins(i, &env);
        }

        /* Handle pending instruction at end of block */
        int use_cbz = 0, cbz_reg = -1, cbz_cls = Kw;

        if (prev) {
            if (jc_try_cbz(prev, b, &env, &use_cbz, &cbz_reg, &cbz_cls)) {
                /* CBZ/CBNZ fusion — don't emit the CMP */
                ji = jit_grow(jc);
                if (!ji) return;
                ji->kind = JIT_COMMENT;
                snprintf(ji->sym_name, JIT_SYM_MAX,
                         "fused: CMP+B.cond -> %s",
                         (use_cbz == 1) ? "CBZ" : "CBNZ");
            } else {
                jc_ins(prev, &env);
            }
            prev = NULL;
        }

        lbl = 1;

        /* ── Block terminator ── */
        switch (b->jmp.type) {
        case Jhlt:
            ji = jit_grow(jc);
            if (!ji) return;
            ji->kind = JIT_BRK;
            ji->imm = 1000;
            break;

        case Jret0:
            /* ── Epilogue: restore callee-saved regs ── */
            ji = jit_grow(jc);
            if (!ji) return;
            ji->kind = JIT_COMMENT;
            snprintf(ji->sym_name, JIT_SYM_MAX, "epilogue: restore frame");

            {
                int rs = (int)((env.frame - env.padding) / 4);
                for (r = arm64_rclob; *r >= 0; r++) {
                    if (fn->reg & BIT(*r)) {
                        rs -= 2;
                        uint64_t off = 16 + env.padding + 4 * (uint64_t)rs;
                        ji = jit_grow(jc);
                        if (!ji) return;
                        ji->kind = JIT_LDR_RI;
                        ji->cls = (*r >= V0) ? JIT_CLS_D : JIT_CLS_L;
                        ji->rd = mapreg(*r);
                        ji->rn = JIT_REG_FP;
                        ji->imm = (int64_t)off;
                    }
                }
            }

            /* Restore SP if dynamic alloc was used */
            if (fn->dynalloc) {
                ji = jit_grow(jc);
                if (!ji) return;
                ji->kind = JIT_MOV_SP;
                ji->cls = JIT_CLS_L;
                ji->rd = JIT_REG_SP;
                ji->rn = JIT_REG_FP;
            }

            /* LDP x29, x30, [sp], #frame+16 */
            o = env.frame + 16;
            if (fn->vararg && !T.apple)
                o += 192;
            if (o <= 504) {
                ji = jit_grow(jc);
                if (!ji) return;
                ji->kind = JIT_LDP_POST;
                ji->cls = JIT_CLS_L;
                ji->rd = JIT_REG_FP;
                ji->rm = JIT_REG_LR;
                ji->rn = JIT_REG_SP;
                ji->imm = (int64_t)o;
            } else {
                /* LDP x29, x30, [sp], #16; ADD sp, sp, #(o-16) */
                ji = jit_grow(jc);
                if (!ji) return;
                ji->kind = JIT_LDP_POST;
                ji->cls = JIT_CLS_L;
                ji->rd = JIT_REG_FP;
                ji->rm = JIT_REG_LR;
                ji->rn = JIT_REG_SP;
                ji->imm = 16;

                ji = jit_grow(jc);
                if (!ji) return;
                ji->kind = JIT_ADD_SP;
                ji->imm = (int64_t)(o - 16);
            }

            /* RET */
            ji = jit_grow(jc);
            if (!ji) return;
            ji->kind = JIT_RET;
            break;

        case Jjmp:
        Jmp:
            if (b->s1 != b->link) {
                ji = jit_grow(jc);
                if (!ji) return;
                ji->kind = JIT_B;
                ji->target_id = (int32_t)(id0 + b->s1->id);
            } else {
                lbl = 0;
            }
            break;

        default:
            /* Conditional branch */
            c = b->jmp.type - Jjf;
            if (c < 0 || c > NCmp)
                break;
            if (b->link == b->s2) {
                t = b->s1;
                b->s1 = b->s2;
                b->s2 = t;
            } else {
                c = cmpneg(c);
            }

            /* Emit a comment showing branch target block names */
            ji = jit_grow(jc);
            if (!ji) return;
            ji->kind = JIT_COMMENT;
            if (b->s1->name[0] && b->s2->name[0])
                snprintf(ji->sym_name, JIT_SYM_MAX,
                         "branch: true->@%s, false->@%s",
                         b->s1->name, b->s2->name);
            else
                snprintf(ji->sym_name, JIT_SYM_MAX,
                         "branch: true->.L%d, false->.L%d",
                         (int)(id0 + b->s1->id),
                         (int)(id0 + b->s2->id));

            if (use_cbz) {
                ji = jit_grow(jc);
                if (!ji) return;
                ji->kind = (use_cbz == 1) ? JIT_CBZ : JIT_CBNZ;
                ji->cls = mapcls(cbz_cls);
                ji->rd = mapreg(cbz_reg);
                ji->target_id = (int32_t)(id0 + b->s2->id);
            } else {
                ji = jit_grow(jc);
                if (!ji) return;
                ji->kind = JIT_B_COND;
                ji->cond = mapcond(c);
                ji->target_id = (int32_t)(id0 + b->s2->id);
            }
            goto Jmp;
        }
    }

    /* ── FUNC_END ── */
    ji = jit_grow(jc);
    if (!ji) return;
    ji->kind = JIT_FUNC_END;

    id0 += fn->nblk;
    jc->nfunc++;
}

/* ── Collect data definitions ───────────────────────────────────── */

void
jit_collect_data(JitCollector *jc, Dat *d)
{
    JitInst *ji;

    switch (d->type) {
    case DStart:
        ji = jit_grow(jc);
        if (!ji) return;
        ji->kind = JIT_DATA_START;
        if (d->name) {
            size_t len = strlen(d->name);
            if (len >= JIT_SYM_MAX) len = JIT_SYM_MAX - 1;
            memcpy(ji->sym_name, d->name, len);
            ji->sym_name[len] = 0;
        }
        if (d->lnk && d->lnk->thread)
            ji->sym_type = JIT_SYM_THREAD_LOCAL;
        else
            ji->sym_type = JIT_SYM_DATA;
        break;

    case DEnd:
        ji = jit_grow(jc);
        if (!ji) return;
        ji->kind = JIT_DATA_END;
        jc->ndata++;
        break;

    case DB:
        if (d->isstr && d->u.str) {
            /* String data — strip surrounding quotes and process escapes.
             * QBE's lexer stores strings as "..." including the quotes
             * and with backslash escapes unprocessed (like gas .ascii).
             * The JIT path must do what the assembler would: strip the
             * quotes and convert \n \r \t \\ \" \0 \xHH to bytes.
             *
             * We store the decoded length in ji->imm so the encoder
             * can emit the right number of bytes even if the string
             * contains embedded NUL characters. */
            const char *src = d->u.str;
            size_t srclen = strlen(src);
            ji = jit_grow(jc);
            if (!ji) return;
            ji->kind = JIT_DATA_ASCII;

            /* Skip leading quote if present */
            size_t si = 0;
            if (srclen > 0 && src[0] == '"') si = 1;
            /* Determine end (skip trailing quote) */
            size_t send = srclen;
            if (send > si && src[send - 1] == '"') send--;

            size_t di = 0;
            while (si < send && di < JIT_SYM_MAX - 1) {
                if (src[si] == '\\' && si + 1 < send) {
                    char next = src[si + 1];
                    switch (next) {
                    case 'n':  ji->sym_name[di++] = '\n'; si += 2; break;
                    case 'r':  ji->sym_name[di++] = '\r'; si += 2; break;
                    case 't':  ji->sym_name[di++] = '\t'; si += 2; break;
                    case '\\': ji->sym_name[di++] = '\\'; si += 2; break;
                    case '"':  ji->sym_name[di++] = '"';  si += 2; break;
                    case '0':  ji->sym_name[di++] = '\0'; si += 2; break;
                    case 'x': case 'X':
                        /* \xHH — up to two hex digits */
                        if (si + 3 < send) {
                            unsigned val = 0;
                            int ok = 1;
                            for (int k = 0; k < 2 && si + 2 + k < send; k++) {
                                char h = src[si + 2 + k];
                                if (h >= '0' && h <= '9')      val = val * 16 + (h - '0');
                                else if (h >= 'a' && h <= 'f') val = val * 16 + (h - 'a' + 10);
                                else if (h >= 'A' && h <= 'F') val = val * 16 + (h - 'A' + 10);
                                else { ok = k; break; }
                                ok = k + 1;
                            }
                            ji->sym_name[di++] = (char)(val & 0xFF);
                            si += 2 + (ok > 0 ? ok : 1);
                        } else {
                            ji->sym_name[di++] = src[si]; si++;
                        }
                        break;
                    default:
                        /* Unknown escape — copy backslash and char literally */
                        ji->sym_name[di++] = src[si++];
                        if (di < JIT_SYM_MAX - 1)
                            ji->sym_name[di++] = src[si++];
                        break;
                    }
                } else {
                    ji->sym_name[di++] = src[si++];
                }
            }
            ji->sym_name[di] = 0;
            ji->imm = (int64_t)di;  /* actual decoded byte count */
        } else if (d->isref && d->u.ref.name) {
            ji = jit_grow(jc);
            if (!ji) return;
            ji->kind = JIT_DATA_SYMREF;
            {
                size_t nlen = strlen(d->u.ref.name);
                if (nlen >= JIT_SYM_MAX) nlen = JIT_SYM_MAX - 1;
                memcpy(ji->sym_name, d->u.ref.name, nlen);
                ji->sym_name[nlen] = 0;
            }
            ji->imm = d->u.ref.off;
        } else {
            ji = jit_grow(jc);
            if (!ji) return;
            ji->kind = JIT_DATA_BYTE;
            ji->imm = d->u.num;
        }
        break;

    case DH:
        ji = jit_grow(jc);
        if (!ji) return;
        ji->kind = JIT_DATA_HALF;
        ji->imm = d->u.num;
        break;

    case DW:
        if (d->isref && d->u.ref.name) {
            ji = jit_grow(jc);
            if (!ji) return;
            ji->kind = JIT_DATA_SYMREF;
            {
                size_t nlen = strlen(d->u.ref.name);
                if (nlen >= JIT_SYM_MAX) nlen = JIT_SYM_MAX - 1;
                memcpy(ji->sym_name, d->u.ref.name, nlen);
                ji->sym_name[nlen] = 0;
            }
            ji->imm = d->u.ref.off;
        } else {
            ji = jit_grow(jc);
            if (!ji) return;
            ji->kind = JIT_DATA_WORD;
            ji->imm = d->u.num;
        }
        break;

    case DL:
        if (d->isref && d->u.ref.name) {
            ji = jit_grow(jc);
            if (!ji) return;
            ji->kind = JIT_DATA_SYMREF;
            {
                size_t nlen = strlen(d->u.ref.name);
                if (nlen >= JIT_SYM_MAX) nlen = JIT_SYM_MAX - 1;
                memcpy(ji->sym_name, d->u.ref.name, nlen);
                ji->sym_name[nlen] = 0;
            }
            ji->imm = d->u.ref.off;
        } else {
            ji = jit_grow(jc);
            if (!ji) return;
            ji->kind = JIT_DATA_QUAD;
            ji->imm = d->u.num;
        }
        break;

    case DZ:
        ji = jit_grow(jc);
        if (!ji) return;
        ji->kind = JIT_DATA_ZERO;
        ji->imm = d->u.num;
        break;
    }
}

/* ── Debug printing ─────────────────────────────────────────────── */

static const char *
kind_names[] = {
    [JIT_LABEL]       = "LABEL",
    [JIT_FUNC_BEGIN]  = "FUNC_BEGIN",
    [JIT_FUNC_END]    = "FUNC_END",
    [JIT_DBGLOC]      = "DBGLOC",
    [JIT_NOP]         = "NOP",
    [JIT_COMMENT]     = "COMMENT",
    [JIT_ADD_RRR]     = "ADD_RRR",
    [JIT_SUB_RRR]     = "SUB_RRR",
    [JIT_MUL_RRR]     = "MUL_RRR",
    [JIT_SDIV_RRR]    = "SDIV_RRR",
    [JIT_UDIV_RRR]    = "UDIV_RRR",
    [JIT_AND_RRR]     = "AND_RRR",
    [JIT_ORR_RRR]     = "ORR_RRR",
    [JIT_EOR_RRR]     = "EOR_RRR",
    [JIT_LSL_RRR]     = "LSL_RRR",
    [JIT_LSR_RRR]     = "LSR_RRR",
    [JIT_ASR_RRR]     = "ASR_RRR",
    [JIT_NEG_RR]      = "NEG_RR",
    [JIT_MSUB_RRRR]   = "MSUB_RRRR",
    [JIT_MADD_RRRR]   = "MADD_RRRR",
    [JIT_ADD_RRI]     = "ADD_RRI",
    [JIT_SUB_RRI]     = "SUB_RRI",
    [JIT_MOV_RR]      = "MOV_RR",
    [JIT_MOVZ]        = "MOVZ",
    [JIT_MOVK]        = "MOVK",
    [JIT_MOVN]        = "MOVN",
    [JIT_MOV_WIDE_IMM]= "MOV_WIDE_IMM",
    [JIT_FADD_RRR]    = "FADD_RRR",
    [JIT_FSUB_RRR]    = "FSUB_RRR",
    [JIT_FMUL_RRR]    = "FMUL_RRR",
    [JIT_FDIV_RRR]    = "FDIV_RRR",
    [JIT_FNEG_RR]     = "FNEG_RR",
    [JIT_FMOV_RR]     = "FMOV_RR",
    [JIT_FCVT_SD]     = "FCVT_SD",
    [JIT_FCVT_DS]     = "FCVT_DS",
    [JIT_FCVTZS]      = "FCVTZS",
    [JIT_FCVTZU]      = "FCVTZU",
    [JIT_SCVTF]       = "SCVTF",
    [JIT_UCVTF]       = "UCVTF",
    [JIT_FMOV_GF]     = "FMOV_GF",
    [JIT_FMOV_FG]     = "FMOV_FG",
    [JIT_SXTB]        = "SXTB",
    [JIT_UXTB]        = "UXTB",
    [JIT_SXTH]        = "SXTH",
    [JIT_UXTH]        = "UXTH",
    [JIT_SXTW]        = "SXTW",
    [JIT_UXTW]        = "UXTW",
    [JIT_CMP_RR]      = "CMP_RR",
    [JIT_CMP_RI]      = "CMP_RI",
    [JIT_CMN_RR]      = "CMN_RR",
    [JIT_FCMP_RR]     = "FCMP_RR",
    [JIT_TST_RR]      = "TST_RR",
    [JIT_CSET]        = "CSET",
    [JIT_CSEL]        = "CSEL",
    [JIT_LDR_RI]      = "LDR_RI",
    [JIT_LDRB_RI]     = "LDRB_RI",
    [JIT_LDRH_RI]     = "LDRH_RI",
    [JIT_LDRSB_RI]    = "LDRSB_RI",
    [JIT_LDRSH_RI]    = "LDRSH_RI",
    [JIT_LDRSW_RI]    = "LDRSW_RI",
    [JIT_STR_RI]      = "STR_RI",
    [JIT_STRB_RI]     = "STRB_RI",
    [JIT_STRH_RI]     = "STRH_RI",
    [JIT_LDR_RR]      = "LDR_RR",
    [JIT_STR_RR]      = "STR_RR",
    [JIT_LDRB_RR]     = "LDRB_RR",
    [JIT_LDRH_RR]     = "LDRH_RR",
    [JIT_LDRSB_RR]    = "LDRSB_RR",
    [JIT_LDRSH_RR]    = "LDRSH_RR",
    [JIT_LDRSW_RR]    = "LDRSW_RR",
    [JIT_STRB_RR]     = "STRB_RR",
    [JIT_STRH_RR]     = "STRH_RR",
    [JIT_LDP]         = "LDP",
    [JIT_STP]         = "STP",
    [JIT_LDP_POST]    = "LDP_POST",
    [JIT_STP_PRE]     = "STP_PRE",
    [JIT_B]           = "B",
    [JIT_BL]          = "BL",
    [JIT_B_COND]      = "B_COND",
    [JIT_CBZ]         = "CBZ",
    [JIT_CBNZ]        = "CBNZ",
    [JIT_BR]          = "BR",
    [JIT_BLR]         = "BLR",
    [JIT_RET]         = "RET",
    [JIT_CALL_EXT]    = "CALL_EXT",
    [JIT_ADRP]        = "ADRP",
    [JIT_ADR]         = "ADR",
    [JIT_LOAD_ADDR]   = "LOAD_ADDR",
    [JIT_SUB_SP]      = "SUB_SP",
    [JIT_ADD_SP]      = "ADD_SP",
    [JIT_MOV_SP]      = "MOV_SP",
    [JIT_HINT]        = "HINT",
    [JIT_BRK]         = "BRK",
    [JIT_NEON_LDR_Q]  = "NEON_LDR_Q",
    [JIT_NEON_STR_Q]  = "NEON_STR_Q",
    [JIT_NEON_ADD]    = "NEON_ADD",
    [JIT_NEON_SUB]    = "NEON_SUB",
    [JIT_NEON_MUL]    = "NEON_MUL",
    [JIT_NEON_DIV]    = "NEON_DIV",
    [JIT_NEON_NEG]    = "NEON_NEG",
    [JIT_NEON_ABS]    = "NEON_ABS",
    [JIT_NEON_FMA]    = "NEON_FMA",
    [JIT_NEON_MIN]    = "NEON_MIN",
    [JIT_NEON_MAX]    = "NEON_MAX",
    [JIT_NEON_DUP]    = "NEON_DUP",
    [JIT_NEON_ADDV]   = "NEON_ADDV",
    [JIT_ADD_SHIFT]   = "ADD_SHIFT",
    [JIT_SUB_SHIFT]   = "SUB_SHIFT",
    [JIT_AND_SHIFT]   = "AND_SHIFT",
    [JIT_ORR_SHIFT]   = "ORR_SHIFT",
    [JIT_EOR_SHIFT]   = "EOR_SHIFT",
    [JIT_DATA_START]  = "DATA_START",
    [JIT_DATA_END]    = "DATA_END",
    [JIT_DATA_BYTE]   = "DATA_BYTE",
    [JIT_DATA_HALF]   = "DATA_HALF",
    [JIT_DATA_WORD]   = "DATA_WORD",
    [JIT_DATA_QUAD]   = "DATA_QUAD",
    [JIT_DATA_ZERO]   = "DATA_ZERO",
    [JIT_DATA_SYMREF] = "DATA_SYMREF",
    [JIT_DATA_ASCII]  = "DATA_ASCII",
    [JIT_DATA_ALIGN]  = "DATA_ALIGN",
};

#define KIND_NAMES_MAX (sizeof(kind_names)/sizeof(kind_names[0]))

const char *
jit_inst_kind_name(uint16_t kind)
{
    if (kind < KIND_NAMES_MAX && kind_names[kind])
        return kind_names[kind];
    return "???";
}

static const char *
reg_str(int32_t r)
{
    static char buf[16];
    if (r == JIT_REG_NONE)  return "---";
    if (r == JIT_REG_SP)    return "sp";
    if (r == JIT_REG_FP)    return "x29";
    if (r == JIT_REG_LR)    return "x30";
    if (r == JIT_REG_IP0)   return "x16";
    if (r == JIT_REG_IP1)   return "x17";
    if (r >= 0 && r <= 30) {
        snprintf(buf, sizeof buf, "r%d", r);
        return buf;
    }
    if (r <= JIT_VREG_BASE) {
        int idx = JIT_VREG_BASE - r;
        snprintf(buf, sizeof buf, "v%d", idx);
        return buf;
    }
    snprintf(buf, sizeof buf, "?%d", r);
    return buf;
}

void
jit_inst_dump(const JitInst *inst)
{
    const char *kn = jit_inst_kind_name(inst->kind);
    fprintf(stderr, "  %-16s", kn);

    switch (inst->kind) {
    case JIT_LABEL:
        fprintf(stderr, ".L%d", inst->target_id);
        break;
    case JIT_FUNC_BEGIN:
        fprintf(stderr, "%s  frame=%"PRId64, inst->sym_name, inst->imm);
        break;
    case JIT_FUNC_END:
        break;
    case JIT_COMMENT:
        fprintf(stderr, "// %s", inst->sym_name);
        break;
    case JIT_B:
    case JIT_BL:
        fprintf(stderr, ".L%d", inst->target_id);
        break;
    case JIT_B_COND:
        fprintf(stderr, "cond=%d .L%d", inst->cond, inst->target_id);
        break;
    case JIT_CBZ:
    case JIT_CBNZ:
        fprintf(stderr, "%s, .L%d", reg_str(inst->rd), inst->target_id);
        break;
    case JIT_CALL_EXT:
        fprintf(stderr, "%s", inst->sym_name);
        break;
    case JIT_RET:
        break;
    case JIT_HINT:
    case JIT_BRK:
        fprintf(stderr, "#%"PRId64, inst->imm);
        break;
    default:
        if (inst->rd != JIT_REG_NONE)
            fprintf(stderr, "%s", reg_str(inst->rd));
        if (inst->rn != JIT_REG_NONE)
            fprintf(stderr, ", %s", reg_str(inst->rn));
        if (inst->rm != JIT_REG_NONE)
            fprintf(stderr, ", %s", reg_str(inst->rm));
        if (inst->ra != JIT_REG_NONE)
            fprintf(stderr, ", %s", reg_str(inst->ra));
        if (inst->imm)
            fprintf(stderr, "  imm=%"PRId64, inst->imm);
        if (inst->imm2)
            fprintf(stderr, "  imm2=%"PRId64, inst->imm2);
        break;
    }
    fprintf(stderr, "\n");
}

void
jit_collector_dump(const JitCollector *jc)
{
    fprintf(stderr, "\n=== JitCollector: %u instructions, %u functions, %u data ===\n",
            jc->ninst, jc->nfunc, jc->ndata);
    if (jc->error)
        fprintf(stderr, "  ERROR: %s\n", jc->error_msg);

    for (uint32_t i = 0; i < jc->ninst; i++) {
        fprintf(stderr, "[%4u] ", i);
        jit_inst_dump(&jc->insts[i]);
    }
    fprintf(stderr, "=== End JitCollector ===\n\n");
}

/* ── Bridge integration ─────────────────────────────────────────── */

/* Emit stashed FP literal-pool constants as JIT data records.
 * Defined in emit.c alongside the stash list. */
extern void jit_emit_fp_constants(JitCollector *jc);

/* JIT-mode callbacks for QBE's parse() */

static JitCollector *bridge_jc;

static void
jit_data_cb(Dat *d)
{
    jit_collect_data(bridge_jc, d);
    if (d->type == DEnd)
        freeall();
}

static void
jit_func_cb(Fn *fn)
{
    /* Run the full QBE optimization pipeline (same as qbe_bridge.c) */
    uint n;

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

    /* Reconstruct linked-list order from RPO */
    assert(fn->rpo[0] == fn->start);
    for (n = 0;; n++)
        if (n == fn->nblk - 1) {
            fn->rpo[n]->link = 0;
            break;
        } else
            fn->rpo[n]->link = fn->rpo[n+1];

    /* Collect into JitInst[] instead of emitting text */
    jit_collect_fn(bridge_jc, fn);

    freeall();
}

static void
jit_dbgfile_cb(char *fn)
{
    (void)fn; /* We don't need debug file markers in JIT mode */
}

/* ── Global FILE handle for cleanup after longjmp ───────────────────── */
static FILE *g_jit_parse_file = NULL;

/* Select target (duplicated from qbe_bridge.c to avoid link issues) */
extern Target T_amd64_sysv;
extern Target T_amd64_apple;
extern Target T_arm64;
extern Target T_arm64_apple;
extern Target T_rv64;

static int
jit_select_target(const char *name)
{
    Target *tlist[] = {
        &T_amd64_sysv,
        &T_amd64_apple,
        &T_arm64,
        &T_arm64_apple,
        &T_rv64,
        0
    };
    Target **t;

    if (!name) {
        T = Deftgt;
        return 0;
    }
    for (t = tlist; *t; t++) {
        if (strcmp(name, (*t)->name) == 0) {
            T = **t;
            return 0;
        }
    }
    return -1;
}

int
qbe_compile_il_jit(const char *il_text, size_t il_len,
                   JitCollector *jc, const char *target_name)
{
    FILE *inf;

    if (!il_text || il_len == 0)
        return -2; /* QBE_ERR_INPUT */

    if (jit_select_target(target_name) != 0)
        return -3; /* QBE_ERR_TARGET */

    memset(debug, 0, sizeof(debug));

    bridge_jc = jc;
    jit_collector_reset(jc);

    inf = fmemopen((void *)il_text, il_len, "r");
    if (!inf) {
        jc->error = -1;
        snprintf(jc->error_msg, sizeof jc->error_msg,
                 "qbe_compile_il_jit: fmemopen failed");
        return -2;
    }

    /* Track the FILE handle globally so qbe_jit_cleanup() can close it
     * if parse() longjmps out via err()/die_() → basic_exit(). */
    g_jit_parse_file = inf;

    parse(inf, "<jit>", jit_dbgfile_cb, jit_data_cb, jit_func_cb);

    g_jit_parse_file = NULL;
    fclose(inf);

    /* Emit any floating-point constants that were stashed during
     * ARM64 isel (via stashbits()).  In the assembly path these
     * are written by T.emitfin(); in JIT mode we must emit them
     * as JIT_DATA_START/QUAD/END records so they land in the
     * JIT data section with the correct symbol names ("Lfp0", …). */
    jit_emit_fp_constants(jc);

    bridge_jc = NULL;

    /* Accumulate opcode histogram for this compilation */
    if (!jc->error)
        jit_histogram_accumulate(jc);

    return jc->error ? -1 : 0;
}

/* ── Opcode histogram API ──────────────────────────────────────────────
 *
 * The histogram is a global array of counters indexed by JitInstKind.
 * It accumulates across multiple qbe_compile_il_jit() calls (e.g. in
 * --batch-jit mode) and can be dumped as a sorted table at the end.
 */

void
jit_histogram_reset(void)
{
    memset(jit_histogram, 0, sizeof jit_histogram);
    jit_histogram_total = 0;
}

void
jit_histogram_accumulate(const JitCollector *jc)
{
    for (uint32_t i = 0; i < jc->ninst; i++) {
        uint16_t k = jc->insts[i].kind;
        if (k < JIT_INST_KIND_COUNT) {
            jit_histogram[k]++;
            jit_histogram_total++;
        }
    }
}

void
jit_histogram_dump(void)
{
    /* Build a list of (kind, count) pairs for non-zero entries */
    typedef struct { uint16_t kind; uint64_t count; } Entry;
    Entry entries[JIT_INST_KIND_COUNT];
    int n = 0;

    for (int i = 0; i < JIT_INST_KIND_COUNT; i++) {
        if (jit_histogram[i] > 0) {
            entries[n].kind = (uint16_t)i;
            entries[n].count = jit_histogram[i];
            n++;
        }
    }

    if (n == 0) {
        fprintf(stderr, "  (no instructions collected)\n");
        return;
    }

    /* Sort descending by count (simple insertion sort — n is small) */
    for (int i = 1; i < n; i++) {
        Entry tmp = entries[i];
        int j = i - 1;
        while (j >= 0 && entries[j].count < tmp.count) {
            entries[j + 1] = entries[j];
            j--;
        }
        entries[j + 1] = tmp;
    }

    /* Find the maximum count for bar scaling */
    uint64_t max_count = entries[0].count;

    /* Print header */
    fprintf(stderr, "\n");
    fprintf(stderr, "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
    fprintf(stderr, "  JIT Opcode Histogram  (%"PRIu64" total instructions)\n",
            jit_histogram_total);
    fprintf(stderr, "──────────────────────────────────────────────────────\n");

    /* Bar width in characters */
    const int bar_max = 30;

    for (int i = 0; i < n; i++) {
        const char *name = jit_inst_kind_name(entries[i].kind);
        uint64_t cnt = entries[i].count;
        double pct = 100.0 * (double)cnt / (double)jit_histogram_total;

        /* Compute bar length */
        int bar_len = (int)((double)cnt / (double)max_count * bar_max);
        if (bar_len < 1 && cnt > 0) bar_len = 1;

        fprintf(stderr, "  %-16s %7"PRIu64"  %5.1f%%  ", name, cnt, pct);
        for (int b = 0; b < bar_len; b++)
            fputc('#', stderr);
        fputc('\n', stderr);
    }

    fprintf(stderr, "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
}

/* ── QBE JIT cleanup after aborted compilation ─────────────────────────
 *
 * When basic_exit() fires during QBE compilation (from err(), die_(), or
 * an assert), the longjmp skips all cleanup in qbe_compile_il_jit().
 * This function is called from basic_jit_call()'s recovery path to:
 *
 *   1. Close the fmemopen FILE handle that parse() was reading from.
 *   2. Call freeall() to release QBE's pool allocator memory.
 *   3. Reset the bridge_jc pointer so the next compilation starts clean.
 *
 * It is safe to call this even when no compilation was in progress
 * (all operations are guarded).
 */
void
qbe_jit_cleanup(void)
{
    if (g_jit_parse_file) {
        fclose(g_jit_parse_file);
        g_jit_parse_file = NULL;
    }
    freeall();
    bridge_jc = NULL;
}