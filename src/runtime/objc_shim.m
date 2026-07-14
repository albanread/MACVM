// objc_shim.m — the Cocoa bridge's exception boundary (C0,
// docs/cocoa_bridge_design.md §5). One shim covers every C0 shape: the
// target/selector plus up to two GPR arguments (id / NSInteger / pointer all
// travel in x2/x3 under AAPCS64, and passing MORE argument registers than
// the callee reads is harmless), returning the GPR result (id or integer).
//
// The @try CALLS the send — a genuine call, never a tail call: the @catch
// personality scope lives in THIS frame, so the frame must still be on the
// stack when an exception unwinds. A caught exception copies its
// description into the caller's buffer and returns 1; Rust never sees an
// ObjC unwind.
#include <objc/message.h>
#include <objc/runtime.h>
#include <string.h>

long macvm_try_msgsend2(void *target, void *sel, void *a, void *b,
                        void **out, char *excbuf, unsigned long cap) {
    @try {
        void *r = ((void *(*)(void *, void *, void *, void *))objc_msgSend)(
            target, sel, a, b);
        *out = r;
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
