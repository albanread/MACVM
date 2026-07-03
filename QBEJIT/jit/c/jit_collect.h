/* jit_collect.h — Flat instruction records for JIT machine code generation
 *
 * This header defines the data structures that bridge QBE's internal
 * post-register-allocation Fn* representation into a flat array of
 * instruction records that the Zig JIT engine can read, analyze,
 * transform, print, and encode into machine code.
 *
 * The flow is:
 *   QBE IL → parse → optimize → regalloc → isel
 *     → jit_collect_fn(Fn*) → JitInst[] flat array
 *     → Zig reads JitInst[] → MCInst structured IR
 *     → encode → trampoline link → mmap+execute
 *
 * Design principles:
 *   - Every field is a plain scalar or fixed-size array (no pointers)
 *   - Layout is C ABI compatible, readable from Zig via @cImport
 *   - Each JitInst is self-contained: you can print/analyze it in isolation
 *   - Pseudo-instructions (labels, data directives) are in the same stream
 *   - Symbol references carry the name inline (max 79 chars)
 *   - Register IDs use QBE's physical register numbering from arm64/all.h
 */

#ifndef JIT_COLLECT_H
#define JIT_COLLECT_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ── Maximum sizes ──────────────────────────────────────────────────── */

#define JIT_SYM_MAX    80   /* max symbol name length including NUL */
#define JIT_MAX_INSTS  (1 << 20)  /* 1M instructions per collection */
#define JIT_MAX_DATA   (1 << 20)  /* 1M bytes of data section */

/* ── Instruction kinds ──────────────────────────────────────────────
 *
 * These enumerate every distinct instruction shape the collector can
 * produce. The Zig side maps these to its MCInst tagged union.
 *
 * Naming: JIT_[category]_[specific]
 */
enum JitInstKind {
    /* ── Pseudo-instructions (emit no machine bytes) ──────────── */
    JIT_LABEL = 0,           /* Block label: .id = block_id */
    JIT_FUNC_BEGIN,          /* Function prologue marker */
    JIT_FUNC_END,            /* Function epilogue marker */
    JIT_DBGLOC,              /* Debug location: .imm = line, .imm2 = col */
    JIT_NOP,                 /* No operation (placeholder) */
    JIT_COMMENT,             /* Comment for readability (sym_name = text) */

    /* ── Register-register ALU (3-operand) ────────────────────── */
    JIT_ADD_RRR = 16,        /* rd = rn + rm */
    JIT_SUB_RRR,             /* rd = rn - rm */
    JIT_MUL_RRR,             /* rd = rn * rm */
    JIT_SDIV_RRR,            /* rd = rn / rm (signed) */
    JIT_UDIV_RRR,            /* rd = rn / rm (unsigned) */
    JIT_AND_RRR,             /* rd = rn & rm */
    JIT_ORR_RRR,             /* rd = rn | rm */
    JIT_EOR_RRR,             /* rd = rn ^ rm */
    JIT_LSL_RRR,             /* rd = rn << rm */
    JIT_LSR_RRR,             /* rd = rn >> rm (unsigned) */
    JIT_ASR_RRR,             /* rd = rn >> rm (signed) */
    JIT_NEG_RR,              /* rd = -rn */

    /* ── Register-register ALU with remainder ─────────────────── */
    JIT_MSUB_RRRR = 32,     /* rd = ra - (rn * rm)  (for remainder) */
    JIT_MADD_RRRR,          /* rd = ra + (rn * rm)  (fused multiply-add) */

    /* ── Register-immediate ALU ───────────────────────────────── */
    JIT_ADD_RRI = 48,        /* rd = rn + imm12 */
    JIT_SUB_RRI,             /* rd = rn - imm12 */

    /* ── Move / constant loading ──────────────────────────────── */
    JIT_MOV_RR = 64,         /* rd = rn (register move) */
    JIT_MOVZ,                /* rd = imm16 << shift (zero rest) */
    JIT_MOVK,                /* rd[shift..shift+15] = imm16 (keep rest) */
    JIT_MOVN,                /* rd = ~(imm16 << shift) */
    JIT_MOV_WIDE_IMM,        /* rd = arbitrary immediate (may expand to multiple MOV/MOVK) */

    /* ── Floating point register-register ─────────────────────── */
    JIT_FADD_RRR = 80,       /* fd = fn + fm */
    JIT_FSUB_RRR,            /* fd = fn - fm */
    JIT_FMUL_RRR,            /* fd = fn * fm */
    JIT_FDIV_RRR,            /* fd = fn / fm */
    JIT_FNEG_RR,             /* fd = -fn */
    JIT_FMOV_RR,             /* fd = fn */

    /* ── Float ↔ Int conversions ──────────────────────────────── */
    JIT_FCVT_SD = 96,        /* single → double */
    JIT_FCVT_DS,             /* double → single */
    JIT_FCVTZS,              /* float → signed int (truncate) */
    JIT_FCVTZU,              /* float → unsigned int (truncate) */
    JIT_SCVTF,               /* signed int → float */
    JIT_UCVTF,               /* unsigned int → float */
    JIT_FMOV_GF,             /* GPR → FPR (bitcast) */
    JIT_FMOV_FG,             /* FPR → GPR (bitcast) */

    /* ── Extensions ───────────────────────────────────────────── */
    JIT_SXTB = 112,          /* signed extend byte */
    JIT_UXTB,                /* unsigned extend byte */
    JIT_SXTH,                /* signed extend halfword */
    JIT_UXTH,                /* unsigned extend halfword */
    JIT_SXTW,                /* signed extend word (32→64) */
    JIT_UXTW,                /* unsigned extend word (aliased as MOV Wd, Wn) */

    /* ── Compare ──────────────────────────────────────────────── */
    JIT_CMP_RR = 128,        /* set flags: rn - rm */
    JIT_CMP_RI,              /* set flags: rn - imm */
    JIT_CMN_RR,              /* set flags: rn + rm */
    JIT_FCMP_RR,             /* set flags: fn cmp fm */
    JIT_TST_RR,              /* set flags: rn & rm */

    /* ── Conditional set ──────────────────────────────────────── */
    JIT_CSET = 144,          /* rd = (cond) ? 1 : 0 */
    JIT_CSEL,                /* rd = (cond) ? rn : rm */

    /* ── Memory: load with register + immediate offset ────────── */
    JIT_LDR_RI = 160,        /* rt = [rn + imm] */
    JIT_LDRB_RI,             /* rt = byte [rn + imm] (zero ext) */
    JIT_LDRH_RI,             /* rt = half [rn + imm] (zero ext) */
    JIT_LDRSB_RI,            /* rt = byte [rn + imm] (sign ext) */
    JIT_LDRSH_RI,            /* rt = half [rn + imm] (sign ext) */
    JIT_LDRSW_RI,            /* rt = word [rn + imm] (sign ext to 64) */

    /* ── Memory: store with register + immediate offset ───────── */
    JIT_STR_RI = 176,        /* [rn + imm] = rt */
    JIT_STRB_RI,             /* byte [rn + imm] = rt */
    JIT_STRH_RI,             /* half [rn + imm] = rt */

    /* ── Memory: load/store with register + register offset ───── */
    JIT_LDR_RR = 192,        /* rt = [rn + rm] */
    JIT_STR_RR,              /* [rn + rm] = rt */
    JIT_LDRB_RR,
    JIT_LDRH_RR,
    JIT_LDRSB_RR,
    JIT_LDRSH_RR,
    JIT_LDRSW_RR,
    JIT_STRB_RR,
    JIT_STRH_RR,

    /* ── Memory: load/store pair ──────────────────────────────── */
    JIT_LDP = 208,           /* rt, rt2 = [rn + imm7*scale] */
    JIT_STP,                 /* [rn + imm7*scale] = rt, rt2 */
    JIT_LDP_POST,            /* rt, rt2 = [rn], rn += imm7*scale */
    JIT_STP_PRE,             /* rn -= imm, [rn] = rt, rt2 */

    /* ── Branch unconditional ─────────────────────────────────── */
    JIT_B = 224,             /* branch to label */
    JIT_BL,                  /* branch with link to label (intra-function) */

    /* ── Branch conditional ───────────────────────────────────── */
    JIT_B_COND,              /* branch if condition to label */

    /* ── Compare and branch ───────────────────────────────────── */
    JIT_CBZ,                 /* branch if rt == 0 */
    JIT_CBNZ,                /* branch if rt != 0 */

    /* ── Branch register ──────────────────────────────────────── */
    JIT_BR = 232,            /* branch to address in register */
    JIT_BLR,                 /* branch with link to address in register */
    JIT_RET,                 /* return (branch to x30) */

    /* ── Call external symbol (needs relocation/trampoline) ───── */
    JIT_CALL_EXT = 240,      /* bl to external symbol by name */

    /* ── PC-relative address ──────────────────────────────────── */
    JIT_ADRP = 248,          /* rd = PC-relative page address of symbol */
    JIT_ADR,                 /* rd = PC-relative address of symbol */

    /* ── Address of symbol (load address, multi-instruction) ──── */
    JIT_LOAD_ADDR = 252,     /* rd = address of symbol (ADRP+ADD or literal) */

    /* ── Stack manipulation ───────────────────────────────────── */
    JIT_SUB_SP = 256,        /* sp = sp - imm */
    JIT_ADD_SP,              /* sp = sp + imm */
    JIT_MOV_SP,              /* rd = sp or sp = rn */

    /* ── Special ──────────────────────────────────────────────── */
    JIT_HINT = 264,          /* hint #imm (BTI, NOP, etc.) */
    JIT_BRK,                 /* breakpoint */

    /* ── NEON vector (128-bit) ────────────────────────────────── */
    JIT_NEON_LDR_Q = 272,    /* qt = [rn] (128-bit load) */
    JIT_NEON_STR_Q,          /* [rn] = qt (128-bit store) */
    JIT_NEON_ADD,             /* vd.arr = vn.arr + vm.arr */
    JIT_NEON_SUB,
    JIT_NEON_MUL,
    JIT_NEON_DIV,             /* float only */
    JIT_NEON_NEG,
    JIT_NEON_ABS,
    JIT_NEON_FMA,             /* vd.arr += vn.arr * vm.arr */
    JIT_NEON_MIN,
    JIT_NEON_MAX,
    JIT_NEON_DUP,             /* broadcast scalar to all lanes */
    JIT_NEON_ADDV,            /* horizontal sum → scalar GPR */

    /* ── Fused shifted-operand ALU ────────────────────────────── */
    JIT_ADD_SHIFT = 296,      /* rd = rn + (rm SHIFT #amt) */
    JIT_SUB_SHIFT,
    JIT_AND_SHIFT,
    JIT_ORR_SHIFT,
    JIT_EOR_SHIFT,

    /* ── Data directives (emitted into data section) ──────────── */
    JIT_DATA_START = 320,     /* begin data definition */
    JIT_DATA_END,             /* end data definition */
    JIT_DATA_BYTE,            /* .byte imm */
    JIT_DATA_HALF,            /* .short imm */
    JIT_DATA_WORD,            /* .int imm */
    JIT_DATA_QUAD,            /* .quad imm */
    JIT_DATA_ZERO,            /* .fill imm, 1, 0 */
    JIT_DATA_SYMREF,          /* .quad symbol + offset */
    JIT_DATA_ASCII,           /* .ascii (data in sym_name) */
    JIT_DATA_ALIGN,           /* .balign imm */

    /* ── Sentinel ─────────────────────────────────────────────── */
    JIT_INST_KIND_COUNT
};

/* ── Condition codes (ARM64 encoding values) ────────────────────────
 *
 * These match the 4-bit ARM64 condition field encoding exactly,
 * so they can be used directly in instruction words.
 */
enum JitCond {
    JIT_COND_EQ = 0x0,    /* equal (Z=1) */
    JIT_COND_NE = 0x1,    /* not equal (Z=0) */
    JIT_COND_CS = 0x2,    /* carry set / unsigned >= (C=1) */
    JIT_COND_CC = 0x3,    /* carry clear / unsigned < (C=0) */
    JIT_COND_MI = 0x4,    /* minus / negative (N=1) */
    JIT_COND_PL = 0x5,    /* plus / positive (N=0) */
    JIT_COND_VS = 0x6,    /* overflow (V=1) */
    JIT_COND_VC = 0x7,    /* no overflow (V=0) */
    JIT_COND_HI = 0x8,    /* unsigned > (C=1 && Z=0) */
    JIT_COND_LS = 0x9,    /* unsigned <= (C=0 || Z=1) */
    JIT_COND_GE = 0xA,    /* signed >= (N=V) */
    JIT_COND_LT = 0xB,    /* signed < (N!=V) */
    JIT_COND_GT = 0xC,    /* signed > (Z=0 && N=V) */
    JIT_COND_LE = 0xD,    /* signed <= (Z=1 || N!=V) */
    JIT_COND_AL = 0xE,    /* always */
    JIT_COND_NV = 0xF,    /* never (reserved, treated as always) */
};

/* ── Size/type class ────────────────────────────────────────────────
 *
 * Indicates the operand width. Matches QBE's Kw/Kl/Ks/Kd but as
 * explicit constants so the Zig side doesn't need QBE headers.
 */
enum JitCls {
    JIT_CLS_W = 0,    /* 32-bit integer (Wd) */
    JIT_CLS_L = 1,    /* 64-bit integer (Xd) */
    JIT_CLS_S = 2,    /* 32-bit float (Sd) */
    JIT_CLS_D = 3,    /* 64-bit float (Dd) */
};

/* ── Shift type for shifted-operand instructions ────────────────── */
enum JitShift {
    JIT_SHIFT_LSL = 0,
    JIT_SHIFT_LSR = 1,
    JIT_SHIFT_ASR = 2,
    JIT_SHIFT_ROR = 3,   /* not used by QBE but defined for completeness */
};

/* ── NEON arrangement specifier ─────────────────────────────────── */
enum JitNeonArr {
    JIT_NEON_4S  = 0,    /* 4 × 32-bit integer */
    JIT_NEON_2D  = 1,    /* 2 × 64-bit integer */
    JIT_NEON_4SF = 2,    /* 4 × 32-bit float */
    JIT_NEON_2DF = 3,    /* 2 × 64-bit float */
    JIT_NEON_8H  = 4,    /* 8 × 16-bit integer */
    JIT_NEON_16B = 5,    /* 16 × 8-bit integer */
};

/* ── Register sentinel ──────────────────────────────────────────── */
#define JIT_REG_NONE   (-1)
#define JIT_REG_SP     (-2)   /* stack pointer (encoded specially) */
#define JIT_REG_FP     (-3)   /* frame pointer (x29) */
#define JIT_REG_LR     (-4)   /* link register (x30) */
#define JIT_REG_IP0    (-5)   /* scratch register (x16) */
#define JIT_REG_IP1    (-6)   /* scratch register (x17) */

/* For NEON instructions, vector register IDs are stored as
 * negative values: JIT_VREG_BASE - qbe_vreg_id.
 * The Zig side decodes: if (reg < JIT_VREG_BASE) → NEON v-reg. */
#define JIT_VREG_BASE  (-100)

/* ── Symbol reference flags ─────────────────────────────────────── */
enum JitSymType {
    JIT_SYM_NONE = 0,
    JIT_SYM_GLOBAL,         /* normal global symbol */
    JIT_SYM_THREAD_LOCAL,   /* thread-local symbol */
    JIT_SYM_DATA,           /* data section label */
    JIT_SYM_FUNC,           /* function label (internal) */
};

/* ── The flat instruction record ────────────────────────────────────
 *
 * This is the core data structure. Each record is exactly 128 bytes
 * for alignment friendliness. Every instruction the collector produces
 * is one of these. The Zig side reads them as a contiguous slice.
 *
 * Unused fields are set to 0 / -1 / "" as appropriate.
 *
 * Field usage by instruction kind:
 *
 *   JIT_ADD_RRR:   cls, rd, rn, rm
 *   JIT_ADD_RRI:   cls, rd, rn, imm
 *   JIT_MOV_RR:    cls, rd, rn
 *   JIT_MOVZ:      cls, rd, imm (value), imm2 (shift: 0/16/32/48)
 *   JIT_MOVK:      cls, rd, imm (value), imm2 (shift)
 *   JIT_LDR_RI:    cls, rd (data reg), rn (base), imm (offset)
 *   JIT_STR_RI:    cls, rd (data reg), rn (base), imm (offset)
 *   JIT_LDR_RR:    cls, rd (data reg), rn (base), rm (index)
 *   JIT_LDP:       cls, rd (first), rm (second), rn (base), imm (offset)
 *   JIT_STP:       cls, rd (first), rm (second), rn (base), imm (offset)
 *   JIT_STP_PRE:   cls, rd (first), rm (second), rn (base), imm (offset)
 *   JIT_LDP_POST:  cls, rd (first), rm (second), rn (base), imm (offset)
 *   JIT_B:         target_id (block id, or -1 if sym_name is set)
 *   JIT_BL:        target_id
 *   JIT_B_COND:    cond, target_id
 *   JIT_CBZ:       cls, rd (test reg), target_id
 *   JIT_CBNZ:      cls, rd (test reg), target_id
 *   JIT_BR:        rn
 *   JIT_BLR:       rn
 *   JIT_RET:       (no operands)
 *   JIT_CALL_EXT:  sym_name
 *   JIT_LOAD_ADDR: rd, sym_name, imm (addend)
 *   JIT_CSET:      cls, rd, cond
 *   JIT_CSEL:      cls, rd, rn, rm, cond
 *   JIT_CMP_RR:    cls, rn, rm
 *   JIT_CMP_RI:    cls, rn, imm
 *   JIT_FCMP_RR:   cls, rn, rm
 *   JIT_LABEL:     target_id (block id)
 *   JIT_HINT:      imm (hint number)
 *   JIT_DBGLOC:    imm (line), imm2 (column)
 *   JIT_SUB_SP:    imm
 *   JIT_ADD_SP:    imm
 *   JIT_MOV_SP:    rd (if reading SP) or rn (if writing SP)
 *   JIT_FUNC_BEGIN: sym_name (function name), imm (frame size)
 *   JIT_FUNC_END:  (no operands)
 *   JIT_NEON_*:    rn (address reg), imm (arrangement enum)
 *   JIT_ADD_SHIFT: cls, rd, rn, rm, shift_type, imm2 (shift amount)
 *   JIT_MSUB_RRRR: cls, rd, rn, rm, ra (rd = ra - rn*rm)
 *   JIT_MADD_RRRR: cls, rd, rn, rm, ra (rd = ra + rn*rm)
 *   JIT_DATA_*:    sym_name (data label), imm (value/size)
 */
typedef struct JitInst {
    uint16_t kind;              /* enum JitInstKind */
    uint8_t  cls;               /* enum JitCls */
    uint8_t  cond;              /* enum JitCond (for B_COND, CSET, CSEL) */
    uint8_t  shift_type;        /* enum JitShift (for shifted-operand ops) */
    uint8_t  sym_type;          /* enum JitSymType */
    uint8_t  is_float;          /* 1 if floating-point variant */
    uint8_t  _pad1;

    int32_t  rd;                /* destination register (or first data reg) */
    int32_t  rn;                /* first source / base register */
    int32_t  rm;                /* second source / index register */
    int32_t  ra;                /* accumulator register (MADD/MSUB) */

    int64_t  imm;               /* primary immediate value */
    int64_t  imm2;              /* secondary immediate (shift amount, column, etc.) */

    int32_t  target_id;         /* branch target block id (-1 = none) */
    int32_t  _pad2;

    char     sym_name[JIT_SYM_MAX]; /* symbol name for external refs / data labels */
} JitInst;

/* Verify struct size at compile time */
/* (We aim for 128 bytes but don't mandate it — alignment may vary) */

/* ── Collection buffer ──────────────────────────────────────────────
 *
 * The collector fills this buffer. The Zig side reads it after
 * qbe_compile_il_jit() returns.
 *
 * Functions and data are interleaved in the instruction stream,
 * delimited by FUNC_BEGIN/FUNC_END and DATA_START/DATA_END markers.
 */
typedef struct JitCollector {
    JitInst *insts;             /* instruction array (malloc'd) */
    uint32_t ninst;             /* number of instructions collected */
    uint32_t inst_cap;          /* allocated capacity */

    /* Function tracking */
    uint32_t nfunc;             /* number of functions collected */
    uint32_t ndata;             /* number of data definitions collected */

    /* Error state */
    int      error;             /* non-zero if collection failed */
    char     error_msg[256];    /* human-readable error message */
} JitCollector;

/* ── Public API ─────────────────────────────────────────────────────
 *
 * These are called from qbe_bridge.c in JIT mode.
 */

/* Initialize a collector. Must be called before first use.
 * Returns 0 on success, -1 on allocation failure. */
int jit_collector_init(JitCollector *jc);

/* Free all resources held by a collector. */
void jit_collector_free(JitCollector *jc);

/* Reset a collector for reuse (keeps allocated memory). */
void jit_collector_reset(JitCollector *jc);

/* Collect instructions from a fully-optimized, register-allocated
 * QBE function. This is called from the bridge's function callback
 * instead of T.emitfn().
 *
 * The function walks the Fn* exactly as arm64_emitfn() would, but
 * appends JitInst records instead of fprintf calls.
 *
 * Must be called before freeall() destroys the Fn. */
struct Fn;
void jit_collect_fn(JitCollector *jc, struct Fn *fn);

/* Collect data definitions. Called from the bridge's data callback
 * instead of emitdat(). */
struct Dat;
void jit_collect_data(JitCollector *jc, struct Dat *d);

/* ── Convenience: append a single instruction ───────────────────── */

/* Append an instruction record, growing the array if needed.
 * Returns a pointer to the new (zeroed) instruction slot,
 * or NULL if allocation fails. */
JitInst *jit_emit(JitCollector *jc);

/* ── Query helpers ──────────────────────────────────────────────── */

/* Returns 1 if the instruction kind produces machine code bytes,
 * 0 if it's a pseudo-instruction (label, comment, debug, etc.). */
static inline int
jit_inst_has_encoding(uint16_t kind)
{
    switch (kind) {
    case JIT_LABEL:
    case JIT_FUNC_BEGIN:
    case JIT_FUNC_END:
    case JIT_DBGLOC:
    case JIT_NOP:
    case JIT_COMMENT:
    case JIT_DATA_START:
    case JIT_DATA_END:
    case JIT_DATA_ALIGN:
        return 0;
    default:
        return (kind < JIT_DATA_START || kind > JIT_DATA_ALIGN);
    }
}

/* Returns 1 if the instruction is a branch of any kind. */
static inline int
jit_inst_is_branch(uint16_t kind)
{
    return (kind >= JIT_B && kind <= JIT_CBNZ)
        || kind == JIT_BR
        || kind == JIT_BLR
        || kind == JIT_CALL_EXT;
}

/* Returns 1 if the instruction references an external symbol. */
static inline int
jit_inst_has_symbol(uint16_t kind)
{
    return kind == JIT_CALL_EXT
        || kind == JIT_LOAD_ADDR
        || kind == JIT_ADRP
        || kind == JIT_ADR
        || kind == JIT_DATA_SYMREF;
}

/* Returns the human-readable name for an instruction kind. */
const char *jit_inst_kind_name(uint16_t kind);

/* ── Debug printing ─────────────────────────────────────────────── */

/* Print a single instruction to stderr in a human-readable format. */
void jit_inst_dump(const JitInst *inst);

/* Print all instructions in a collector to stderr. */
void jit_collector_dump(const JitCollector *jc);

/* ── Bridge integration ─────────────────────────────────────────── */

/* Full JIT compilation entry point (called from qbe_bridge.c).
 *
 * Compiles QBE IL text into a JitCollector, running the full QBE
 * optimization pipeline but collecting structured instructions
 * instead of emitting assembly text.
 *
 * The caller owns the JitCollector and must call jit_collector_free()
 * when done.
 *
 * Returns 0 (QBE_OK) on success, negative error code on failure.
 */
int qbe_compile_il_jit(const char *il_text, size_t il_len,
                       JitCollector *jc, const char *target_name);

/* ── Opcode histogram ───────────────────────────────────────────── */

/* Reset all histogram counters to zero. Call before a batch run. */
void jit_histogram_reset(void);

/* Accumulate instruction counts from a collector into the global
 * histogram.  Called automatically at the end of qbe_compile_il_jit()
 * on success; can also be called manually. */
void jit_histogram_accumulate(const JitCollector *jc);

/* Print the histogram to stderr, sorted by count descending, with
 * a simple bar chart and percentages. */
void jit_histogram_dump(void);

#ifdef __cplusplus
}
#endif

#endif /* JIT_COLLECT_H */