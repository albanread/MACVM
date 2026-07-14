// objc_shim.m — the Cocoa bridge's exception boundary (C0/C1,
// docs/cocoa_bridge_design.md §5). ONE general shim covers every C1 shape:
// the target/selector plus the full AAPCS64 outgoing-argument model — 6 GPR
// argument words (x2..x7; x0/x1 are self/_cmd), 8 FPR doubles (d0..d7), and
// 4 stack words for spilled arguments. Loading MORE argument registers than
// the callee's own prototype reads is harmless (the same reasoning
// codecache/ffi_stubs.rs leans on), and unread outgoing stack words in the
// caller's own frame are equally invisible to a narrower callee.
//
// Stack-word note (Darwin AAPCS64): Apple's arm64 ABI packs NON-variadic
// stack arguments to their NATURAL size, so this fixed unsigned-long shape
// is only correct for callees whose spilled parameters are 8-byte types
// (id / NSInteger / NSUInteger / double / pointers). That covers the entire
// C1 surface; a callee spilling char/int stack args would need per-shape
// packing (deferred with the rest of Tier-2 shapes). Variadic methods
// (stringWithFormat:) are NOT callable through this shim at all — variadic
// arguments always go to the stack under Darwin arm64, register slots are
// never read.
//
// The return side is the one dimension registers can't fake: the CALLER's
// cast return type decides which registers the compiler reads after the
// call. So the shim switches on a return-kind token and casts objc_msgSend
// per class — unsigned long (x0), double (d0), the two HFA structs (d0..d1
// / d0..d3 — a C struct of 2/4 doubles IS the HFA, the compiler reads the
// right registers), the 16-byte integer pair (x0/x1 — NSRange's shape), and
// void. arm64 has no objc_msgSend_stret: ≤16-byte composites and HFAs come
// back in registers from the plain entry point.
//
// The @try CALLS the send — a genuine call, never a tail call: the @catch
// personality scope lives in THIS frame, so the frame must still be on the
// stack when an exception unwinds. A caught exception copies its
// description into the caller's buffer and returns 1; Rust never sees an
// ObjC unwind.
#include <objc/message.h>
#include <objc/runtime.h>
#include <string.h>

// Return-kind tokens (mirrored by objc_bridge.rs's RetKind — keep in sync).
enum {
    MACVM_RET_GPR = 0,     // id / NSInteger / BOOL / pointer  -> out_gpr[0]
    MACVM_RET_FPR = 1,     // double / CGFloat                 -> out_fpr[0]
    MACVM_RET_HFA2 = 2,    // CGPoint / CGSize / NSSize        -> out_fpr[0..2]
    MACVM_RET_HFA4 = 3,    // CGRect / NSRect                  -> out_fpr[0..4]
    MACVM_RET_INTPAIR = 4, // NSRange (16-byte integer struct) -> out_gpr[0..2]
    MACVM_RET_VOID = 5,    // void — nothing read after the call
};

typedef struct { double a, b; } macvm_hfa2;
typedef struct { double a, b, c, d; } macvm_hfa4;
typedef struct { unsigned long a, b; } macvm_ipair;

// The one argument shape every cast below shares: self, _cmd, 6 GPR words,
// 8 FPR doubles, 4 stack words. The C compiler places these exactly where
// AAPCS64 says: x0..x7, d0..d7, [sp..sp+24].
#define MACVM_ARG_TYPES                                                        \
    void *, void *, unsigned long, unsigned long, unsigned long,               \
        unsigned long, unsigned long, unsigned long, double, double, double,   \
        double, double, double, double, double, unsigned long, unsigned long,  \
        unsigned long, unsigned long
#define MACVM_ARG_VALUES                                                       \
    target, sel, gpr[0], gpr[1], gpr[2], gpr[3], gpr[4], gpr[5], fpr[0],        \
        fpr[1], fpr[2], fpr[3], fpr[4], fpr[5], fpr[6], fpr[7], stack[0],       \
        stack[1], stack[2], stack[3]

long macvm_try_msgsend(void *target, void *sel, const unsigned long *gpr,
                       const double *fpr, const unsigned long *stack,
                       long ret_kind, unsigned long *out_gpr,
                       double *out_fpr, char *excbuf, unsigned long cap) {
    @try {
        switch (ret_kind) {
        case MACVM_RET_FPR:
            out_fpr[0] =
                ((double (*)(MACVM_ARG_TYPES))objc_msgSend)(MACVM_ARG_VALUES);
            break;
        case MACVM_RET_HFA2: {
            macvm_hfa2 r = ((macvm_hfa2 (*)(MACVM_ARG_TYPES))objc_msgSend)(
                MACVM_ARG_VALUES);
            out_fpr[0] = r.a;
            out_fpr[1] = r.b;
            break;
        }
        case MACVM_RET_HFA4: {
            macvm_hfa4 r = ((macvm_hfa4 (*)(MACVM_ARG_TYPES))objc_msgSend)(
                MACVM_ARG_VALUES);
            out_fpr[0] = r.a;
            out_fpr[1] = r.b;
            out_fpr[2] = r.c;
            out_fpr[3] = r.d;
            break;
        }
        case MACVM_RET_INTPAIR: {
            macvm_ipair r = ((macvm_ipair (*)(MACVM_ARG_TYPES))objc_msgSend)(
                MACVM_ARG_VALUES);
            out_gpr[0] = r.a;
            out_gpr[1] = r.b;
            break;
        }
        case MACVM_RET_VOID:
            ((void (*)(MACVM_ARG_TYPES))objc_msgSend)(MACVM_ARG_VALUES);
            break;
        case MACVM_RET_GPR:
        default:
            out_gpr[0] = ((unsigned long (*)(MACVM_ARG_TYPES))objc_msgSend)(
                MACVM_ARG_VALUES);
            break;
        }
        return 0;
    } @catch (id e) {
        excbuf[0] = 0;
        @try {
            id desc = ((id (*)(id, SEL))objc_msgSend)(
                e, sel_registerName("description"));
            const char *d = ((const char *(*)(id, SEL))objc_msgSend)(
                desc, sel_registerName("UTF8String"));
            if (d) {
                strncpy(excbuf, d, cap - 1);
                excbuf[cap - 1] = 0;
            }
        } @catch (id e2) {
            // Description itself threw — report generically rather than die.
        }
        if (excbuf[0] == 0) {
            strncpy(excbuf, "Objective-C exception (no description)", cap - 1);
            excbuf[cap - 1] = 0;
        }
        return 1;
    }
}
