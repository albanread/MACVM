//! Sprint S10 integration tests (`tests_s10.md`). This file is allowed
//! `unsafe` (it lives in `tests/`, a separate crate from `macvm` itself).

use std::path::PathBuf;
use std::process::{Command, Stdio};

use macvm::bytecode::builder::BytecodeBuilder;
use macvm::codecache::nmethod::{IcSite, IcState, NmState, Nmethod, NmethodId};
use macvm::codecache::pics::PIC_MAX_ENTRIES;
use macvm::codecache::stubs::{self, CallStubFn};
use macvm::codecache::CodeCache;
use macvm::compiler::decode;
use macvm::compiler::driver;
use macvm::compiler::emit;
use macvm::compiler::ir::{
    self, BailoutReason, BlockId, CallSiteInfo, CmpOp, Ir, IrBlock, IrMethod, PoolLit, SmiOp, VReg,
    VRegInfo,
};
use macvm::compiler::jasm_assembler::JasmAssembler;
use macvm::compiler::regalloc;
use macvm::frontend::{classdef, parser};
use macvm::interpreter::compiled_call::{enter_compiled, EnterResult};
use macvm::interpreter::ic::InterpreterIc;
use macvm::memory::alloc;
use macvm::memory::scavenge::scavenge;
use macvm::oops::layout::HEADER_WORDS;
use macvm::oops::smi::SmallInt;
use macvm::oops::wrappers::{KlassOop, MemOop, MethodOop, SymbolOop};
use macvm::oops::{Format, Oop};
use macvm::runtime::lookup::install_method;
use macvm::runtime::{JitMode, VmOptions, VmState};

fn test_vm() -> VmState {
    VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: Default::default(),
        gc_stress: false,
        gc_stress_full_period: None,
        eden_kb: None,
        jit: JitMode::Off,
    })
}

/// tests_s10.md's `run_ir_raw`: hand-construct an `IrMethod` — no
/// bytecode, no interpreter involvement at all — computing
/// `(a + b) < 10 ? 1 : 0` (one `SmiArith`, one `SmiCmpBr`), push it
/// through regalloc + emit + publish + the real `call_stub`, and check
/// the executed result. The first test to write for this pipeline, and
/// the first to consult when it misbehaves (tests_s10.md's own framing).
#[test]
fn run_ir_raw() {
    let mut vm = test_vm();
    let mut cache = CodeCache::new(1 << 20).unwrap();
    let stubs = stubs::install(&mut cache);

    // vregs: 0=self, 1=a, 2=b, 3=sum, 4=const10, 5=result_true, 6=result_false
    let vregs: Vec<VRegInfo> = (0..7).map(|_| VRegInfo { is_oop: true }).collect();

    let block0 = IrBlock {
        id: BlockId(0),
        bci: 0,
        code: vec![
            Ir::Param {
                dst: VReg(0),
                index: 0,
            },
            Ir::Param {
                dst: VReg(1),
                index: 1,
            },
            Ir::Param {
                dst: VReg(2),
                index: 2,
            },
            Ir::SmiArith {
                op: SmiOp::Add,
                dst: VReg(3),
                a: VReg(1),
                b: VReg(2),
                fail: BlockId(3),
            },
            Ir::ConstSmi {
                dst: VReg(4),
                value: 10,
            },
            Ir::SmiCmpBr {
                op: CmpOp::Lt,
                a: VReg(3),
                b: VReg(4),
                if_true: BlockId(1),
                if_false: BlockId(2),
                fail: BlockId(3),
            },
        ],
        entry_stack: Vec::new(),
        deopt_sites: Vec::new(),
    };
    let block1 = IrBlock {
        id: BlockId(1),
        bci: 10,
        code: vec![
            Ir::ConstSmi {
                dst: VReg(5),
                value: 1,
            },
            Ir::Ret { val: VReg(5) },
        ],
        entry_stack: Vec::new(),
        deopt_sites: Vec::new(),
    };
    let block2 = IrBlock {
        id: BlockId(2),
        bci: 20,
        code: vec![
            Ir::ConstSmi {
                dst: VReg(6),
                value: 0,
            },
            Ir::Ret { val: VReg(6) },
        ],
        entry_stack: Vec::new(),
        deopt_sites: Vec::new(),
    };
    let block3 = IrBlock {
        id: BlockId(3),
        bci: 30,
        code: vec![Ir::Bailout {
            reason: BailoutReason::SmiOpFailed,
        }],
        entry_stack: Vec::new(),
        deopt_sites: Vec::new(),
    };

    let method = IrMethod {
        blocks: vec![block0, block1, block2, block3],
        vregs,
        pool: Vec::new(),
        argc: 2,
        ntemps: 0,
        ctx_vregs: Vec::new(),
        safepoints: Vec::new(),
        // Unused: this method has no SmiCmpVal/BoolBr, so emit.rs never
        // dereferences these against the (also empty) pool.
        true_lit: PoolLit(0),
        false_lit: PoolLit(0),
        nil_lit: PoolLit(0),
        mark_slots_lit: PoolLit(0),
        call_sites: Vec::new(),
        site_feedback: Vec::new(),
        inline_deps: Vec::new(),
        method_pool_ix: None,
    };

    let regalloc_result = regalloc::regalloc(&method);

    let mut asm = JasmAssembler::new();
    let (blob, pcs, _verified_entry_off, _ic_sites, _safepoints) = emit::emit(
        &mut asm,
        &method,
        &regalloc_result,
        stubs.stub_poll_addr(),
        stubs.must_be_boolean_addr(),
        stubs.alloc_slow_addr(),
        None,
    );
    assert_eq!(
        pcs.len(),
        4,
        "one BlockPc per block, including the bailout block"
    );

    let h = cache.alloc(blob.code.len()).unwrap();
    let entry = cache.publish(h, &blob);

    let call: CallStubFn = unsafe { std::mem::transmute(stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;

    let argv_low = [
        0u64,
        SmallInt::new(3).oop().raw(),
        SmallInt::new(4).oop().raw(),
    ];
    let r_low = unsafe { call(entry as u64, vm_ptr, argv_low.as_ptr(), 3) };
    assert_eq!(r_low, SmallInt::new(1).oop().raw(), "3+4=7 < 10 -> 1");

    let argv_high = [
        0u64,
        SmallInt::new(8).oop().raw(),
        SmallInt::new(9).oop().raw(),
    ];
    let r_high = unsafe { call(entry as u64, vm_ptr, argv_high.as_ptr(), 3) };
    assert_eq!(r_high, SmallInt::new(0).oop().raw(), "8+9=17 >= 10 -> 0");
}

/// `run_ir_raw` alone never exercises `Mul` (this module's own doc: the
/// riskiest sequence, since overflow detection reads both operands twice
/// across two separate multiply-family instructions) or forces any
/// spilling at all (only 7 vregs, nowhere near the 16-register limit) —
/// exactly the paths the tag-check/Mul/BoolBr aliasing bugs this sprint's
/// own commit history found were hiding in. These three tests exist
/// specifically to close that gap: reasoning through a fix by hand is not
/// the same as running it.
fn build_and_publish(cache: &mut CodeCache, stub_poll_addr: u64, method: &IrMethod) -> *const u8 {
    let regalloc_result = regalloc::regalloc(method);
    let mut asm = JasmAssembler::new();
    let (blob, _pcs, _verified_entry_off, _ic_sites, _safepoints) = emit::emit(
        &mut asm,
        method,
        &regalloc_result,
        stub_poll_addr,
        0,
        0,
        None,
    );
    let h = cache.alloc(blob.code.len()).unwrap();
    cache.publish(h, &blob)
}

fn mul_method() -> IrMethod {
    // vregs: 0=self, 1=a, 2=b, 3=product
    let block0 = IrBlock {
        id: BlockId(0),
        bci: 0,
        code: vec![
            Ir::Param {
                dst: VReg(0),
                index: 0,
            },
            Ir::Param {
                dst: VReg(1),
                index: 1,
            },
            Ir::Param {
                dst: VReg(2),
                index: 2,
            },
            Ir::SmiArith {
                op: SmiOp::Mul,
                dst: VReg(3),
                a: VReg(1),
                b: VReg(2),
                fail: BlockId(1),
            },
            Ir::Ret { val: VReg(3) },
        ],
        entry_stack: Vec::new(),
        deopt_sites: Vec::new(),
    };
    let block1 = IrBlock {
        id: BlockId(1),
        bci: 10,
        code: vec![Ir::Bailout {
            reason: BailoutReason::SmiOpFailed,
        }],
        entry_stack: Vec::new(),
        deopt_sites: Vec::new(),
    };
    IrMethod {
        blocks: vec![block0, block1],
        vregs: (0..4).map(|_| VRegInfo { is_oop: true }).collect(),
        pool: Vec::new(),
        argc: 2,
        ntemps: 0,
        ctx_vregs: Vec::new(),
        safepoints: Vec::new(),
        true_lit: PoolLit(0),
        false_lit: PoolLit(0),
        nil_lit: PoolLit(0),
        mark_slots_lit: PoolLit(0),
        call_sites: Vec::new(),
        site_feedback: Vec::new(),
        inline_deps: Vec::new(),
        method_pool_ix: None,
    }
}

#[test]
fn run_ir_raw_mul() {
    let mut vm = test_vm();
    let mut cache = CodeCache::new(1 << 20).unwrap();
    let stubs = stubs::install(&mut cache);
    let method = mul_method();
    let entry = build_and_publish(&mut cache, stubs.stub_poll_addr(), &method);

    let call: CallStubFn = unsafe { std::mem::transmute(stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;

    let argv = [
        0u64,
        SmallInt::new(6).oop().raw(),
        SmallInt::new(7).oop().raw(),
    ];
    let r = unsafe { call(entry as u64, vm_ptr, argv.as_ptr(), 3) };
    assert_eq!(r, SmallInt::new(42).oop().raw(), "6*7=42, no overflow");
}

/// `BAILOUT` (`0b10`, SPEC §2.1's reserved tag) is what a real overflow
/// must reach — 2e9 * 2e9 * 4 (the tagged-multiply's actual 64-bit product,
/// D5.3's "untag one operand, multiply by the other's still-tagged form"
/// trick) is ~1.6e19, past `i64::MAX` (~9.2e18), so `smulh`'s high bits
/// can't just be `mul`'s own sign extension.
#[test]
fn run_ir_raw_mul_overflow() {
    let mut vm = test_vm();
    let mut cache = CodeCache::new(1 << 20).unwrap();
    let stubs = stubs::install(&mut cache);
    let method = mul_method();
    let entry = build_and_publish(&mut cache, stubs.stub_poll_addr(), &method);

    let call: CallStubFn = unsafe { std::mem::transmute(stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;

    let big = SmallInt::new(2_000_000_000).oop().raw();
    let argv = [0u64, big, big];
    let r = unsafe { call(entry as u64, vm_ptr, argv.as_ptr(), 3) };
    assert_eq!(r, 2, "BAILOUT sentinel (0b10) on real overflow");
}

/// Forces real spilling: 20 constants, all still live when the last is
/// defined (none are consumed until the summation chain starts), well
/// past the 16-register limit — exercising the spilled-operand load/
/// store paths `run_ir_raw`'s 7-vreg method never comes close to needing.
#[test]
fn run_ir_raw_forces_spill() {
    let mut vm = test_vm();
    let mut cache = CodeCache::new(1 << 20).unwrap();
    let stubs = stubs::install(&mut cache);

    let n = 20u32;
    let mut vregs: Vec<VRegInfo> = vec![VRegInfo { is_oop: true }]; // v0 = self
    let mut code = vec![Ir::Param {
        dst: VReg(0),
        index: 0,
    }];

    // v1..=v20: constants 1..=20, all defined up front (all live at once).
    for i in 1..=n {
        vregs.push(VRegInfo { is_oop: true });
        code.push(Ir::ConstSmi {
            dst: VReg(i),
            value: i as i64,
        });
    }
    // Chain-sum them: acc = v1 + v2; acc = acc + v3; ... (dst for the i'th
    // add, i in 2..=n, is vreg n+i-1 -- computed directly rather than via a
    // separate running counter).
    let bailout = BlockId(1);
    let mut acc = VReg(1);
    for i in 2..=n {
        vregs.push(VRegInfo { is_oop: true });
        let dst = VReg(n + i - 1);
        code.push(Ir::SmiArith {
            op: SmiOp::Add,
            dst,
            a: acc,
            b: VReg(i),
            fail: bailout,
        });
        acc = dst;
    }
    code.push(Ir::Ret { val: acc });

    let block0 = IrBlock {
        id: BlockId(0),
        bci: 0,
        code,
        entry_stack: Vec::new(),
        deopt_sites: Vec::new(),
    };
    let block1 = IrBlock {
        id: bailout,
        bci: 1000,
        code: vec![Ir::Bailout {
            reason: BailoutReason::SmiOpFailed,
        }],
        entry_stack: Vec::new(),
        deopt_sites: Vec::new(),
    };
    let method = IrMethod {
        blocks: vec![block0, block1],
        vregs,
        pool: Vec::new(),
        argc: 0,
        ntemps: 0,
        ctx_vregs: Vec::new(),
        safepoints: Vec::new(),
        true_lit: PoolLit(0),
        false_lit: PoolLit(0),
        nil_lit: PoolLit(0),
        mark_slots_lit: PoolLit(0),
        call_sites: Vec::new(),
        site_feedback: Vec::new(),
        inline_deps: Vec::new(),
        method_pool_ix: None,
    };

    let regalloc_result = regalloc::regalloc(&method);
    assert!(
        regalloc_result.frame_slots > 0,
        "20 simultaneously-live vregs must force at least one spill"
    );

    let mut asm = JasmAssembler::new();
    let (blob, _pcs, _verified_entry_off, _ic_sites, _safepoints) = emit::emit(
        &mut asm,
        &method,
        &regalloc_result,
        stubs.stub_poll_addr(),
        stubs.must_be_boolean_addr(),
        stubs.alloc_slow_addr(),
        None,
    );
    let h = cache.alloc(blob.code.len()).unwrap();
    let entry = cache.publish(h, &blob);

    let call: CallStubFn = unsafe { std::mem::transmute(stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let r = unsafe { call(entry as u64, vm_ptr, std::ptr::null(), 0) };
    let expected: i64 = (1..=n as i64).sum();
    assert_eq!(
        r,
        SmallInt::new(expected).oop().raw(),
        "1+2+...+20 = 210, spilled operands included"
    );
}

/// A throwaway method standing in for a real SmallInteger primitive —
/// `driver::eligible` only ever reads its `primitive()` field.
fn primitive_stub(
    vm: &mut VmState,
    sel: SymbolOop,
    prim_id: i64,
) -> macvm::oops::wrappers::MethodOop {
    let mut b = BytecodeBuilder::new();
    b.ret_self();
    let m = b.finish(vm, sel, 1, 0);
    m.set_primitive(prim_id);
    // `run_method`'s own `try_primitive` step (S11 step 7) genuinely tries
    // this primitive before ever falling to `^self` -- a caller that
    // drives it with Fail-inducing args (an overflowing smi add, say)
    // needs `prim_fails` set, or `try_primitive`'s own invariant check
    // panics. Harmless for callers that only ever check eligibility and
    // never actually invoke this stub.
    m.set_flags(1, 0, false, false, true, false, 0);
    m
}

/// S10 step 7's `driver::compile_method`, run through its *real* front
/// door: real bytecode (`self + arg`, built via `BytecodeBuilder`, not a
/// hand-assembled `IrMethod`), a real mono-smi IC, `driver::eligible`
/// itself deciding this method qualifies, then the full decode -> convert
/// -> regalloc -> emit -> publish -> install pipeline, then a real
/// `call_stub` invocation of the result. `run_ir_raw` and its siblings
/// above prove the back half of the pipeline (regalloc/emit/publish/call)
/// in isolation; this is the one test in the suite that also exercises the
/// front half (`eligible`, `decode`, `convert`, real `InterpreterIc`
/// classification) and checks they all agree with each other on argument
/// layout by actually running the result.
#[test]
fn compiled_plus_arg_executes_correctly() {
    let mut vm = test_vm();
    let plus_sel = vm.universe.intern(b"+");

    let mut b = BytecodeBuilder::new();
    b.push_self();
    b.push_temp(0);
    b.send(&mut vm, plus_sel, 1);
    b.ret_tos();
    let m_sel = vm.universe.intern(b"plusArg:");
    let method = b.finish(&mut vm, m_sel, 1, 0);

    let plus_target = primitive_stub(&mut vm, plus_sel, 1);
    let smi_klass = vm.universe.smi_klass;
    let epoch = vm.ic_epoch;
    InterpreterIc::at(method, 0).set_mono(&mut vm, smi_klass, plus_target, epoch);
    // S11 step 7: the overflow check's own fail edge is a real generic
    // send now, which needs a REAL method in smi_klass's own dictionary
    // (`set_mono` above only seeds this call site's own inline cache, a
    // separate thing from what `lookup` actually walks at runtime).
    install_method(&mut vm, smi_klass, plus_sel, plus_target);
    assert!(
        driver::eligible(&vm, method),
        "self + arg, mono smi IC, must be eligible"
    );

    let id =
        driver::compile_method(&mut vm, smi_klass, method).expect("eligible method must compile");
    let nm = vm
        .code_table
        .get(id)
        .expect("installed nmethod must be gettable");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;

    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    // argv[0] = receiver (self), argv[1] = the one real Smalltalk arg —
    // Ir::Param{index: 0} / Param{index: 1}, per ir::convert's entry block.
    let argv = [SmallInt::new(5).oop().raw(), SmallInt::new(37).oop().raw()];
    let result = unsafe { call(entry, vm_ptr, argv.as_ptr(), 2) };
    assert_eq!(result, SmallInt::new(42).oop().raw(), "5 + 37 = 42");

    // S13 step 7b: the OVERFLOW case now emits a real deopt `brk #0xDE00`,
    // which requires a live SIGTRAP handler (armed only under a JIT `VmState`,
    // not this `JitMode::Off` one). The organic trap → deopt → interpret path
    // for an overflowing smi add is exercised by
    // `compiled_smi_overflow_deopts_to_interpreter` below (which uses a
    // JIT-armed VM). This test keeps only the fast (non-overflowing) path.
}

/// S13 step 7b FLAGSHIP (the gate): the FIRST time a `brk` fires from real
/// compiled code under a real SIGTRAP. A compiled `^self + arg` whose smi-add
/// overflows traps (`brk #0xDE00`), the handler redirects to the uncommon
/// trampoline, `rt_uncommon_trap` deoptimizes the frame, and the re-executing
/// send completes IN THE INTERPRETER — its result must equal what the pure
/// interpreter produces for the SAME send (a true differential), and
/// `deopt_count` must bump (proving the brk actually fired, not a silent
/// fast-path wrap). The whole handler/trampoline/materialize/interpret chain
/// is on trial here.
///
/// The fallback `+` returns the ARGUMENT (`^arg`), and the two overflowing
/// operands are DISTINCT (`MAX` and `MAX-7`) — so a deopt that read the wrong
/// frame slot (receiver vs arg, or a stale/unspilled input) would return `a`
/// instead of `b` and fail, not merely "not crash". This is what pins the
/// reexecute operand-stack correctness (`[a, b]` recorded at the send bci).
#[test]
fn compiled_smi_overflow_deopts_to_interpreter() {
    // JIT-armed VmState: `deopt_trap::install` arms the process-global SIGTRAP
    // handler and registers this VM's code-cache range, so a `brk` from the
    // method we compile below is recognised and redirected (a `JitMode::Off`
    // VM installs no handler — the brk would just kill the process).
    let mut vm = VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: Default::default(),
        gc_stress: false,
        gc_stress_full_period: None,
        eden_kb: None,
        jit: JitMode::Threshold(1),
    });
    let plus_sel = vm.universe.intern(b"+");
    let smi_klass = vm.universe.smi_klass;

    // The compiled method under test: `plusArg: arg [ ^self + arg ]`.
    let mut b = BytecodeBuilder::new();
    b.push_self();
    b.push_temp(0);
    b.send(&mut vm, plus_sel, 1);
    b.ret_tos();
    let m_sel = vm.universe.intern(b"plusArg:");
    let method = b.finish(&mut vm, m_sel, 1, 0);

    // The `+` FALLBACK the deopt re-executes into: primitive 1 (`prim_add`,
    // which Fails on overflow), `prim_fails=true`, body `^arg` (returns the
    // SECOND operand). Real bignum promotion isn't installed in a bare
    // `test_vm()` (it lives in `world/06_smallinteger.mst`); `^arg` is a
    // deterministic, operand-discriminating stand-in — exactly the shape
    // `interpreter::send::tests::install_smi_plus` uses for the same reason.
    let plus_fallback = {
        let mut pb = BytecodeBuilder::new();
        pb.push_temp(0); // ^arg
        pb.ret_tos();
        let m = pb.finish(&mut vm, plus_sel, 1, 0);
        m.set_primitive(1);
        m.set_flags(1, 0, false, false, true, false, 0); // prim_fails = true
        m
    };
    // Seed THIS site's inline cache (for eligibility/inlining) AND install the
    // fallback in smi_klass's real dictionary (what `lookup` walks when the
    // deopt re-executes the send interpreted — a separate thing from the IC).
    let epoch = vm.ic_epoch;
    InterpreterIc::at(method, 0).set_mono(&mut vm, smi_klass, plus_fallback, epoch);
    install_method(&mut vm, smi_klass, plus_sel, plus_fallback);

    assert!(
        driver::eligible(&vm, method),
        "self + arg, mono smi IC, must be eligible"
    );
    let id = driver::compile_method(&mut vm, smi_klass, method).expect("must compile");
    // The compiled method genuinely gained a deopt site (its smi fail edge), so
    // `ir::convert` interned its own MethodOop — the nmethod carries deopt
    // metadata the materializer needs.
    assert!(
        !vm.code_table
            .get(id)
            .expect("installed")
            .deopt_pcdescs
            .is_empty(),
        "a smi-overflow method must carry at least one deopt PcDesc (the trap site)"
    );

    // Two DISTINCT operands, each a valid smi, whose sum overflows.
    let big = SmallInt::MAX;
    let a = SmallInt::new(big);
    let b_arg = SmallInt::new(big - 7);

    // Interpreter reference: run the SAME method purely interpreted. The `+`
    // primitive Fails (overflow), the fallback `^arg` runs, yielding `b_arg`.
    let interp_result = macvm::interpreter::run_method(&mut vm, method, a.oop(), &[b_arg.oop()]);
    assert_eq!(
        interp_result.raw(),
        b_arg.oop().raw(),
        "pure-interpreter reference: overflowing '+' falls back to '^arg' = the second operand"
    );

    let nm = vm.code_table.get(id).expect("installed nmethod");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;

    // Fast path first (no overflow, no trap): 5 + 37 = 42, straight-line.
    let fast_argv = [SmallInt::new(5).oop().raw(), SmallInt::new(37).oop().raw()];
    let fast = unsafe { call(entry, vm_ptr, fast_argv.as_ptr(), 2) };
    assert_eq!(
        fast,
        SmallInt::new(42).oop().raw(),
        "fast path: 5 + 37 = 42"
    );
    let deopts_after_fast = unsafe { (*vm_ptr).stats.deopt_count };

    // THE organic trap: overflowing operands. brk -> SIGTRAP -> handler ->
    // uncommon trampoline -> rt_uncommon_trap -> deoptimize_frame ->
    // interpret_active -> result back through call_stub.
    let deopts_before = deopts_after_fast;
    let ovf_argv = [a.oop().raw(), b_arg.oop().raw()];
    let deopt_result = unsafe { call(entry, vm_ptr, ovf_argv.as_ptr(), 2) };
    let deopts_after = unsafe { (*vm_ptr).stats.deopt_count };

    assert_eq!(
        deopt_result,
        interp_result.raw(),
        "the deopt path must produce the IDENTICAL result to the pure interpreter for the \
         same overflowing send (differential equivalence)"
    );
    assert_eq!(
        deopt_result,
        b_arg.oop().raw(),
        "and specifically the SECOND operand (the arg), proving the reexecute stack `[a, b]` \
         resolved to the right frame slots"
    );
    assert_eq!(
        deopts_after,
        deopts_before + 1,
        "exactly one deopt must have been counted (the brk actually fired)"
    );
    // The fast path must NOT have deopted.
    assert_eq!(
        deopts_after_fast, 0,
        "the non-overflowing fast path never traps"
    );
}

/// S13 step 7b-ii (the SECOND organic trap client): a compiled `br_true`/
/// `br_false` on a NON-boolean operand deopts (`brk #0xDE00`, reexecute at the
/// branch bci), the interpreter re-executes the branch, sees the non-boolean,
/// and runs its own `mustBeBoolean` protocol — result identical to a pure
/// interpreter run. Same signal chain as the smi-overflow test, driven by the
/// `BoolBr.not_bool` edge instead of a smi fail edge.
#[test]
fn compiled_not_boolean_deopts_to_interpreter() {
    let mut vm = VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: Default::default(),
        gc_stress: false,
        gc_stress_full_period: None,
        eden_kb: None,
        jit: JitMode::Threshold(1),
    });
    let smi_klass = vm.universe.smi_klass;

    // `mustBeBoolean` handler on SmallInteger: `^true` (SPEC §5.4 Alg 11 — the
    // branch re-executes with the handler's result). A smi is the non-boolean
    // we branch on below, so its klass is where the interpreter looks.
    let mb_sel = vm.universe.sel_must_be_boolean;
    let handler = {
        let mut hb = BytecodeBuilder::new();
        hb.push_true();
        hb.ret_tos();
        hb.finish(&mut vm, mb_sel, 0, 0)
    };
    install_method(&mut vm, smi_klass, mb_sel, handler);

    // `chooseOn: x [ ^x ifTrue: [1] ifFalse: [0] ]` — a NON-fused boolean
    // branch on the arg (distinct branch values so the result discriminates).
    let mut b = BytecodeBuilder::new();
    let tb = b.new_label();
    let end = b.new_label();
    b.push_temp(0); // x
    b.br_true_fwd(tb);
    b.push_smi_i8(0); // false branch -> 0
    b.jump_fwd(end);
    b.bind(tb);
    b.push_smi_i8(1); // true branch -> 1
    b.bind(end);
    b.ret_tos();
    let m_sel = vm.universe.intern(b"chooseOn:");
    let method = b.finish(&mut vm, m_sel, 1, 0);

    assert!(
        driver::eligible(&vm, method),
        "a plain boolean branch is eligible"
    );
    let id = driver::compile_method(&mut vm, smi_klass, method).expect("must compile");
    assert!(
        !vm.code_table
            .get(id)
            .expect("installed")
            .deopt_pcdescs
            .is_empty(),
        "the not_bool edge is a deopt trap site -> at least one deopt PcDesc"
    );

    let recv = SmallInt::new(0).oop(); // receiver klass = smi_klass (customization key)
    let nonbool = SmallInt::new(5).oop(); // a smi: NOT true/false -> not_bool -> deopt

    // Interpreter reference for the non-boolean arg: mustBeBoolean(5) -> true
    // -> the true branch -> 1.
    let interp_result = macvm::interpreter::run_method(&mut vm, method, recv, &[nonbool]);
    assert_eq!(
        interp_result.raw(),
        SmallInt::new(1).oop().raw(),
        "pure interpreter: non-boolean branch -> mustBeBoolean -> true -> the 1 branch"
    );

    let nm = vm.code_table.get(id).expect("installed nmethod");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;

    // Fast paths: real booleans never trap. true -> 1, false -> 0.
    let t_res = unsafe {
        call(
            entry,
            vm_ptr,
            [recv.raw(), vm.universe.true_obj.raw()].as_ptr(),
            2,
        )
    };
    assert_eq!(t_res, SmallInt::new(1).oop().raw(), "fast path: true -> 1");
    let f_res = unsafe {
        call(
            entry,
            vm_ptr,
            [recv.raw(), vm.universe.false_obj.raw()].as_ptr(),
            2,
        )
    };
    assert_eq!(f_res, SmallInt::new(0).oop().raw(), "fast path: false -> 0");
    let deopts_before = unsafe { (*vm_ptr).stats.deopt_count };
    assert_eq!(deopts_before, 0, "boolean branches never trap");

    // THE organic trap: a non-boolean operand. brk -> SIGTRAP -> deopt ->
    // interpret_active runs mustBeBoolean -> true -> the 1 branch.
    let deopt_result = unsafe { call(entry, vm_ptr, [recv.raw(), nonbool.raw()].as_ptr(), 2) };
    let deopts_after = unsafe { (*vm_ptr).stats.deopt_count };
    assert_eq!(
        deopt_result,
        interp_result.raw(),
        "the not_bool deopt must produce the IDENTICAL result to the pure interpreter"
    );
    assert_eq!(
        deopt_result,
        SmallInt::new(1).oop().raw(),
        "mustBeBoolean returned true, so the 1 branch runs"
    );
    assert_eq!(
        deopts_after,
        deopts_before + 1,
        "exactly one deopt (the not_bool brk fired)"
    );
}

/// S14 step 3 (the THIRD organic trap client, and the first SPECULATIVE one):
/// a generic (non-smi) send whose IC is still `Untaken` (never executed while
/// interpreted) now COMPILES — the send lowers to an uncommon trap
/// (`SiteFeedback::Untaken` -> `inline::decide` -> `Ir::UncommonTrap`,
/// reexecute=true at the send's own bci) instead of the pre-S14 `NoRetryLater`
/// that blocked compilation. Running the compiled method fires the trap on the
/// FIRST call: brk -> SIGTRAP -> handler -> uncommon trampoline ->
/// rt_uncommon_trap -> deoptimize_frame -> interpret_active re-executes the
/// WHOLE send in the interpreter (which also warms the IC for a later
/// recompile), and the result must equal a pure `run_method` reference for the
/// same send. `deopt_count` must bump (the brk actually fired).
///
/// The re-executed method `foo:` returns its ARGUMENT (`^arg`), and the two
/// operands (receiver vs. arg) are DISTINCT, so a deopt that read the wrong
/// frame slot for the reexecute stack `[receiver, arg]` would return the
/// receiver instead of the arg and fail — this pins the trap's recorded
/// operand-stack correctness (receiver + args, captured before the send pops).
#[test]
fn compiled_untaken_send_traps_and_reexecutes() {
    let mut vm = VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: Default::default(),
        gc_stress: false,
        gc_stress_full_period: None,
        eden_kb: None,
        jit: JitMode::Threshold(1),
    });
    let smi_klass = vm.universe.smi_klass;
    let foo_sel = vm.universe.intern(b"foo:");

    // The compiled method under test: `callFoo: arg [ ^self foo: arg ]`. Its
    // one inner send (`self foo: arg`) is a generic, non-smi send.
    let mut b = BytecodeBuilder::new();
    b.push_self();
    b.push_temp(0);
    b.send(&mut vm, foo_sel, 1);
    b.ret_tos();
    let m_sel = vm.universe.intern(b"callFoo:");
    let method = b.finish(&mut vm, m_sel, 1, 0);

    // `foo:` on SmallInteger: a plain (non-primitive) method `^arg`, returning
    // the SECOND operand — a deterministic, operand-discriminating target the
    // deopt re-executes into (its klass is where `lookup` walks when the send
    // re-executes interpreted).
    let foo_target = {
        let mut fb = BytecodeBuilder::new();
        fb.push_temp(0); // ^arg
        fb.ret_tos();
        fb.finish(&mut vm, foo_sel, 1, 0)
    };
    install_method(&mut vm, smi_klass, foo_sel, foo_target);

    // The site's IC is LEFT EMPTY (never dispatched) — Untaken. Previously this
    // returned `NoRetryLater` and `compile_method` declined; now it compiles as
    // a trap.
    assert!(
        driver::eligible(&vm, method),
        "an Untaken generic send is now eligible (compiles as a trap)"
    );
    let id = driver::compile_method(&mut vm, smi_klass, method).expect("must compile now");
    assert!(
        !vm.code_table
            .get(id)
            .expect("installed")
            .deopt_pcdescs
            .is_empty(),
        "a trapped send carries at least one deopt PcDesc (the trap site)"
    );

    // Two DISTINCT operands: receiver 7, arg 99. The re-executed `^arg` returns
    // 99 — distinct from the receiver, so a wrong-slot deopt would return 7.
    let recv = SmallInt::new(7).oop();
    let arg = SmallInt::new(99).oop();

    // Interpreter reference: run the SAME method purely interpreted. The send
    // dispatches to `foo:` = `^arg`, yielding the arg (99).
    let interp_result = macvm::interpreter::run_method(&mut vm, method, recv, &[arg]);
    assert_eq!(
        interp_result.raw(),
        arg.raw(),
        "pure-interpreter reference: `^self foo: arg` dispatches to `^arg` = the arg"
    );

    let nm = vm.code_table.get(id).expect("installed nmethod");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;

    let deopts_before = unsafe { (*vm_ptr).stats.deopt_count };
    assert_eq!(deopts_before, 0, "nothing has trapped yet");

    // THE organic trap: the Untaken send. brk -> SIGTRAP -> handler ->
    // uncommon trampoline -> rt_uncommon_trap -> deoptimize_frame ->
    // interpret_active re-executes the send -> `^arg` -> 99, back through
    // call_stub.
    let deopt_result = unsafe { call(entry, vm_ptr, [recv.raw(), arg.raw()].as_ptr(), 2) };
    let deopts_after = unsafe { (*vm_ptr).stats.deopt_count };

    assert_eq!(
        deopt_result,
        interp_result.raw(),
        "the trap-and-reexecute path must produce the IDENTICAL result to the pure interpreter \
         for the same send (differential equivalence)"
    );
    assert_eq!(
        deopt_result,
        arg.raw(),
        "and specifically the ARG (99), proving the reexecute stack `[receiver, arg]` resolved \
         to the right frame slots"
    );
    assert_eq!(
        deopts_after,
        deopts_before + 1,
        "exactly one deopt must have been counted (the Untaken-site brk actually fired)"
    );
}

/// Builds `jit: JitMode::Threshold(1)` VM with the two inlinable smi prims
/// (`+`, `<`) the loop tests use.
fn loop_test_vm() -> VmState {
    let mut vm = VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: Default::default(),
        gc_stress: false,
        gc_stress_full_period: None,
        eden_kb: None,
        jit: JitMode::Threshold(1),
    });
    install_smi_prim(&mut vm, b"+", 1, 1);
    install_smi_prim(&mut vm, b"<", 1, 10);
    vm
}

/// S13 step 10b (the THIRD deopt path) through the PRODUCTION dispatch path:
/// a CALL-FREE compiled loop deopts via its loop poll when its own nmethod is
/// `NotEntrant`, entered via `enter_compiled` — which pushes a live
/// `TierLink::IntoCompiled` — NOT the raw `call_stub`. That tier-link + the
/// missing stub anchor is the exact state `rt_poll` runs under; an earlier
/// draft walked the native stack from `rt_poll` (`maybe_disarm_poll`) and
/// aborted the VM here (`walk_frames`: IntoCompiled innermost, no anchor set).
/// `rt_poll` no longer walks, so the deopt completes and the correct result
/// flows back out through the whole `enter_compiled` teardown.
#[test]
fn compiled_loop_poll_deopts_via_enter_compiled() {
    let mut vm = loop_test_vm();
    let smi_klass = vm.universe.smi_klass;
    let lt_sel = vm.universe.intern(b"<");
    let plus_sel = vm.universe.intern(b"+");

    // `countTo: n [ |i| i:=0. [i<n] whileTrue:[i:=i+1]. ^i ]`.  t0=n, t1=i.
    let mut b = BytecodeBuilder::new();
    b.push_smi_i8(0);
    b.store_temp_pop(1);
    let loop_hdr = b.new_label();
    b.bind(loop_hdr);
    b.push_temp(1);
    b.push_temp(0);
    b.send(&mut vm, lt_sel, 1);
    let end = b.new_label();
    b.br_false_fwd(end);
    b.push_temp(1);
    b.push_smi_i8(1);
    b.send(&mut vm, plus_sel, 1);
    b.store_temp_pop(1);
    b.jump_back(loop_hdr);
    b.bind(end);
    b.push_temp(1);
    b.ret_tos();
    let m_sel = vm.universe.intern(b"countTo:");
    let method = b.finish(&mut vm, m_sel, 1, 1);

    // Warm the inner smi ICs to mono-smi via one interpreted run.
    let recv = SmallInt::new(0).oop();
    let warm = macvm::interpreter::run_method(&mut vm, method, recv, &[SmallInt::new(3).oop()]);
    assert_eq!(warm.raw(), SmallInt::new(3).oop().raw());

    let id = driver::compile_method(&mut vm, smi_klass, method).expect("must compile");
    {
        let nm = vm.code_table.get(id).expect("installed");
        assert!(nm.ic_sites.is_empty(), "the loop must compile call-free");
        assert!(
            !nm.deopt_pcdescs.is_empty(),
            "the loop poll must carry a LoopPoll deopt scope"
        );
    }

    // Arm §2d (set_not_entrant §2a + both flags) WITHOUT make_not_entrant's
    // entry patch, so the compiled loop actually runs and reaches its own poll.
    vm.code_table.set_not_entrant(id);
    vm.pending_deopt_flag = true;
    vm.reg_block.poll_flag = 1;
    let deopts_before = vm.stats.deopt_count;

    // Enter through the production path (pushes TierLink::IntoCompiled).
    let n = 20i64;
    vm.stack.push(SmallInt::new(0).oop()); // receiver
    vm.stack.push(SmallInt::new(n).oop()); // arg n
    assert_eq!(enter_compiled(&mut vm, id, 1), EnterResult::Completed);
    let result = vm.stack.pop();
    assert_eq!(
        result.raw(),
        SmallInt::new(n).oop().raw(),
        "loop-poll deopt through enter_compiled must produce the correct result"
    );
    assert_eq!(
        vm.stats.deopt_count,
        deopts_before + 1,
        "exactly one loop-poll deopt fired"
    );
    // S13 step 11: the deopt is attributed to the Poll reason.
    assert_eq!(
        vm.stats.deopt_by_reason[macvm::runtime::vm_state::DeoptReason::Poll as usize],
        1,
        "the loop-poll deopt is counted under DeoptReason::Poll"
    );
    // The flags stay ARMED: disarming needs a native walk that is illegal from
    // rt_poll (IntoCompiled innermost + no anchor); it is deferred to step 10c's
    // zombie sweep, which runs at a GC-safe walk point.
    assert!(
        vm.pending_deopt_flag,
        "pending_deopt_flag stays armed until the 10c zombie sweep disarms it"
    );
}

/// S13 step 10b — the M4 merge-height regression. A `LoopPoll` resume bci is a
/// loop HEADER, a genuine CFG merge. If the loop header is fed by a conditional
/// (`x := (n<5) ifTrue:[10] ifFalse:[20]`), the debug-only M4 cross-check's
/// straight-line `interpreter_model_height` double-counts BOTH arms and
/// disagrees with the real (CFG-derived) height — which, before the fix,
/// aborted the whole VM on a `debug_assert_eq!` across the `extern "C"` `rt_poll`
/// boundary. The materialization itself is correct; the check just can't model a
/// merge, so it is skipped for LoopPoll. This test deopts exactly that shape and
/// must produce the right answer in a DEBUG build (where M4 runs).
#[test]
fn loop_poll_deopt_at_merge_header_resume() {
    let mut vm = loop_test_vm();
    let smi_klass = vm.universe.smi_klass;
    let lt_sel = vm.universe.intern(b"<");
    let plus_sel = vm.universe.intern(b"+");

    // `probe: n [ |x i| x := (n<5) ifTrue:[10] ifFalse:[20]. i:=0.
    //             [i<x] whileTrue:[i:=i+1]. ^i ]`.  t0=n, t1=x, t2=i.
    let mut b = BytecodeBuilder::new();
    b.push_temp(0); // n
    b.push_smi_i8(5);
    b.send(&mut vm, lt_sel, 1); // n < 5
    let else_l = b.new_label();
    b.br_false_fwd(else_l);
    b.push_smi_i8(10);
    let merge_l = b.new_label();
    b.jump_fwd(merge_l);
    b.bind(else_l);
    b.push_smi_i8(20);
    b.bind(merge_l); // <- merge feeding the loop header below
    b.store_temp_pop(1); // x := ...
    b.push_smi_i8(0);
    b.store_temp_pop(2); // i := 0
    let loop_hdr = b.new_label();
    b.bind(loop_hdr);
    b.push_temp(2); // i
    b.push_temp(1); // x
    b.send(&mut vm, lt_sel, 1);
    let end = b.new_label();
    b.br_false_fwd(end);
    b.push_temp(2);
    b.push_smi_i8(1);
    b.send(&mut vm, plus_sel, 1);
    b.store_temp_pop(2);
    b.jump_back(loop_hdr);
    b.bind(end);
    b.push_temp(2); // ^i
    b.ret_tos();
    let m_sel = vm.universe.intern(b"probe:");
    let method = b.finish(&mut vm, m_sel, 1, 2);

    let recv = SmallInt::new(0).oop();
    // n=3 -> (3<5) true -> x=10 -> loop counts i to 10 -> result 10.
    let warm = macvm::interpreter::run_method(&mut vm, method, recv, &[SmallInt::new(3).oop()]);
    assert_eq!(
        warm.raw(),
        SmallInt::new(10).oop().raw(),
        "interp ref: probe: 3 = 10"
    );

    let id = driver::compile_method(&mut vm, smi_klass, method).expect("must compile");
    assert!(
        !vm.code_table.get(id).unwrap().deopt_pcdescs.is_empty(),
        "must carry a LoopPoll deopt scope at the merge header"
    );

    vm.code_table.set_not_entrant(id);
    vm.pending_deopt_flag = true;
    vm.reg_block.poll_flag = 1;
    let deopts_before = vm.stats.deopt_count;

    vm.stack.push(SmallInt::new(0).oop()); // receiver
    vm.stack.push(SmallInt::new(3).oop()); // arg n=3
    assert_eq!(enter_compiled(&mut vm, id, 1), EnterResult::Completed);
    let result = vm.stack.pop();
    assert_eq!(
        result.raw(),
        SmallInt::new(10).oop().raw(),
        "deopt resuming at a merge-point loop header must still produce 10 \
         (M4's straight-line model can't be trusted here, so it is skipped)"
    );
    assert_eq!(
        vm.stats.deopt_count,
        deopts_before + 1,
        "one loop-poll deopt fired"
    );
}

/// S13 step 8 (§2a+§2b): `make_not_entrant` on a real compiled method flips it
/// to `NotEntrant`, unhooks it from the `(klass, selector)` lookup (a new send
/// misses + re-resolves), PATCHES both `entry` and `verified_entry` to
/// `b not_entrant_stub` (so a compiled caller's still-live `bl` re-dispatches),
/// and RETAINS the record + address map (an in-flight/trapping frame still
/// resolves). Structural check on the real patched code — the functional C→C
/// re-dispatch is the same `stub_resolve` machine `not_entrant_stub` copies
/// (already exercised) + adversarially verified. JitMode::Off: no code is
/// executed here, so no SIGTRAP handler is needed.
#[test]
fn make_not_entrant_patches_entries_and_unhooks() {
    let mut vm = test_vm();
    let plus_sel = vm.universe.intern(b"+");
    let smi_klass = vm.universe.smi_klass;

    // `plusArg: arg [ ^self + arg ]`, mono-smi IC → eligible + compiled.
    let mut b = BytecodeBuilder::new();
    b.push_self();
    b.push_temp(0);
    b.send(&mut vm, plus_sel, 1);
    b.ret_tos();
    let m_sel = vm.universe.intern(b"plusArg:");
    let method = b.finish(&mut vm, m_sel, 1, 0);
    let plus_target = primitive_stub(&mut vm, plus_sel, 1);
    let epoch = vm.ic_epoch;
    InterpreterIc::at(method, 0).set_mono(&mut vm, smi_klass, plus_target, epoch);
    install_method(&mut vm, smi_klass, plus_sel, plus_target);
    let id = driver::compile_method(&mut vm, smi_klass, method).expect("must compile");

    assert_eq!(
        vm.code_table.lookup(smi_klass, m_sel),
        Some(id),
        "installed → lookup finds it"
    );
    let (base, entry_off, verified_off) = {
        let nm = vm.code_table.get(id).unwrap();
        (
            nm.code.base as usize,
            nm.entry_off as usize,
            nm.verified_entry_off as usize,
        )
    };

    macvm::codecache::flush::make_not_entrant(&mut vm, id);

    assert!(
        matches!(
            vm.code_table.get(id).unwrap().state,
            macvm::codecache::nmethod::NmState::NotEntrant
        ),
        "state → NotEntrant"
    );
    assert_eq!(
        vm.code_table.lookup(smi_klass, m_sel),
        None,
        "unhooked from by_key → a fresh send misses + re-resolves"
    );
    assert_eq!(
        vm.code_table.find_by_pc(base as u64 + entry_off as u64),
        Some(id),
        "record + address map retained → in-flight frames still resolve"
    );

    // Both entries decode to `b not_entrant_stub`.
    let not_entrant = vm.stubs.not_entrant_addr();
    for off in [entry_off, verified_off] {
        let site = base + off;
        let word = unsafe { *(site as *const u32) };
        let disp = not_entrant as i64 - site as i64;
        let expected = 0x1400_0000u32 | (((disp >> 2) as u32) & 0x03FF_FFFF);
        assert_eq!(
            word, expected,
            "entry @ +{off:#x} must be patched to `b not_entrant_stub`"
        );
    }
}

/// S13 step 10c (the zombie sweep): a full GC reclaims a `NotEntrant` nmethod
/// that no live frame references, and disarms the §2d loop poll. Compile
/// `plusArg:`, `make_not_entrant` it (which flips it NotEntrant AND arms the
/// poll), then `full_gc`: with no in-flight frame and no pending redirect, the
/// record + code block are freed and both poll flags clear.
#[test]
fn full_gc_zombies_unreferenced_not_entrant_and_disarms() {
    let mut vm = test_vm();
    let (id, _m_sel) = compile_plus_arg(&mut vm);
    let base = vm.code_table.get(id).unwrap().code.base as u64;

    macvm::codecache::flush::make_not_entrant(&mut vm, id);
    assert!(vm.pending_deopt_flag, "make_not_entrant arms the §2d poll");

    macvm::memory::fullgc::full_gc(&mut vm).expect("full_gc must succeed");

    assert!(
        vm.code_table.get(id).is_none(),
        "the unreferenced NotEntrant nmethod is zombied + removed by the full GC"
    );
    assert_eq!(
        vm.code_table.find_by_pc(base),
        None,
        "its code range returned to the free list"
    );
    assert!(
        !vm.pending_deopt_flag,
        "poll disarmed once no NotEntrant nmethod remains"
    );
    assert_eq!(vm.reg_block.poll_flag, 0, "poll_flag disarmed");
}

/// S13 step 11 (`MACVM_DEOPT_STRESS` behavior 2): every `stress_period` compiled
/// entries, `enter_compiled` force-invalidates the next Alive nmethod
/// round-robin (never the one being entered) via the real D1 path. Two compiled
/// methods A/B, period 2, two entries into A → the second tick makes B
/// NotEntrant while A (the entered method) stays Alive; A keeps returning the
/// correct result throughout (stress is output-equivalent).
#[test]
fn deopt_stress_periodic_invalidation_round_robins() {
    let mut vm = test_vm();
    let smi_klass = vm.universe.smi_klass;
    let (a_id, _) = compile_plus_arg(&mut vm); // A = plusArg: arg [^self + arg]

    // B = retArg: x [^x], a trivial call-free eligible method.
    let ret_sel = vm.universe.intern(b"retArg:");
    let mut b = BytecodeBuilder::new();
    b.push_temp(0);
    b.ret_tos();
    let bm = b.finish(&mut vm, ret_sel, 1, 0);
    let b_id = driver::compile_method(&mut vm, smi_klass, bm).expect("B compiles");

    // Arm stress with a tiny period.
    vm.deopt_stress = true;
    vm.stress_period = 2;
    vm.stress_countdown = 2;

    // Two entries into A: tick 1 just decrements, tick 2 invalidates the
    // round-robin victim (B — A is filtered out as the method being entered).
    for _ in 0..2 {
        vm.stack.push(SmallInt::new(3).oop()); // receiver
        vm.stack.push(SmallInt::new(4).oop()); // arg
        assert_eq!(enter_compiled(&mut vm, a_id, 1), EnterResult::Completed);
        assert_eq!(
            vm.stack.pop().raw(),
            SmallInt::new(7).oop().raw(),
            "A stays correct under stress: 3 + 4 = 7"
        );
    }

    assert!(
        matches!(vm.code_table.get(b_id).unwrap().state, NmState::NotEntrant),
        "stress invalidated B (round-robin, != the entered A) after `period` entries"
    );
    assert!(
        matches!(vm.code_table.get(a_id).unwrap().state, NmState::Alive),
        "the method being entered is never chosen as the stress victim"
    );
}

/// Compiles `plusArg: arg [ ^self + arg ]` for `smi_klass` and returns its
/// nmethod id — the keystone-test fixture. Identical setup to
/// `make_not_entrant_patches_entries_and_unhooks`, factored out so the two
/// step-10a redefinition tests share exactly the make_not_entrant test's own
/// proven-compilable method.
fn compile_plus_arg(
    vm: &mut VmState,
) -> (
    macvm::codecache::nmethod::NmethodId,
    macvm::oops::wrappers::SymbolOop,
) {
    let plus_sel = vm.universe.intern(b"+");
    let smi_klass = vm.universe.smi_klass;
    let mut b = BytecodeBuilder::new();
    b.push_self();
    b.push_temp(0);
    b.send(vm, plus_sel, 1);
    b.ret_tos();
    let m_sel = vm.universe.intern(b"plusArg:");
    let method = b.finish(vm, m_sel, 1, 0);
    let plus_target = primitive_stub(vm, plus_sel, 1);
    let epoch = vm.ic_epoch;
    InterpreterIc::at(method, 0).set_mono(vm, smi_klass, plus_target, epoch);
    install_method(vm, smi_klass, plus_sel, plus_target);
    let id = driver::compile_method(vm, smi_klass, method).expect("must compile");
    (id, m_sel)
}

/// A trivial `^self` method under `sel` — a valid `MethodOop` to hand
/// `install_method` when a redefinition test just needs the dictionary
/// binding to change (the body is never executed).
fn trivial_method(
    vm: &mut VmState,
    sel: macvm::oops::wrappers::SymbolOop,
) -> macvm::oops::wrappers::MethodOop {
    let mut b = BytecodeBuilder::new();
    b.push_self();
    b.ret_tos();
    b.finish(vm, sel, 0, 0)
}

/// S13 step 10a (D1 + D2, the KEYSTONE): redefining a compiled method's OWN
/// `(klass, selector)` — the ordinary `install_method` path, no direct
/// `make_not_entrant` call — drives the dependency hook end-to-end: the live
/// nmethod flips to `NotEntrant` and is unhooked from the lookup map, so a
/// fresh send re-resolves to the new method while the old code survives for
/// any in-flight frame. This is what makes steps 8 and 9's mechanism *fire*.
#[test]
fn redefining_compiled_method_makes_it_not_entrant() {
    let mut vm = test_vm();
    let smi_klass = vm.universe.smi_klass;
    let (id, m_sel) = compile_plus_arg(&mut vm);
    assert_eq!(
        vm.code_table.lookup(smi_klass, m_sel),
        Some(id),
        "compiled → lookup finds it before redefinition"
    );

    // Redefine `plusArg:` on SmallInt itself — the pure install path.
    let new_body = trivial_method(&mut vm, m_sel);
    install_method(&mut vm, smi_klass, m_sel, new_body);

    assert!(
        matches!(
            vm.code_table.get(id).unwrap().state,
            macvm::codecache::nmethod::NmState::NotEntrant
        ),
        "redefinition must invalidate the old compiled method"
    );
    assert_eq!(
        vm.code_table.lookup(smi_klass, m_sel),
        None,
        "unhooked → a fresh send misses and re-resolves to the new method"
    );
}

/// D2's subclass rule, end-to-end: a compiled `SmallInt>>#plusArg:` is
/// invalidated by installing `plusArg:` on an ANCESTOR (`Integer`), because
/// `lookup(SmallInt, #plusArg:)` walks through `Integer` and could now find
/// the new binding. Installing an UNRELATED selector, or the same selector on
/// a class off SmallInt's chain, must leave it alone.
#[test]
fn redefining_superclass_method_invalidates_subclass_nmethod() {
    let mut vm = test_vm();
    let (id, m_sel) = compile_plus_arg(&mut vm);
    let integer_klass = vm.universe.integer_klass;
    let double_klass = vm.universe.double_klass;

    // A different selector on the ancestor, and the same selector on an
    // off-chain class, are both no-ops.
    let other_sel = vm.universe.intern(b"unrelated");
    let other_body = trivial_method(&mut vm, other_sel);
    install_method(&mut vm, integer_klass, other_sel, other_body);
    let off_chain_body = trivial_method(&mut vm, m_sel);
    install_method(&mut vm, double_klass, m_sel, off_chain_body);
    assert!(
        matches!(
            vm.code_table.get(id).unwrap().state,
            macvm::codecache::nmethod::NmState::Alive
        ),
        "unrelated selector / off-chain class must NOT invalidate"
    );

    // Same selector on a true ancestor → invalidates the subclass nmethod.
    let new_body = trivial_method(&mut vm, m_sel);
    install_method(&mut vm, integer_klass, m_sel, new_body);
    assert!(
        matches!(
            vm.code_table.get(id).unwrap().state,
            macvm::codecache::nmethod::NmState::NotEntrant
        ),
        "redefining #plusArg: on Integer must invalidate compiled SmallInt>>#plusArg:"
    );
}

/// The current AArch64 native stack pointer — `sp` never appears as an
/// ordinary register operand (AArch64 requires `mov`/add-immediate forms
/// for it), so reading it needs one inline-asm instruction; this whole
/// file already carries the crate's "allowed unsafe" exemption for exactly
/// this kind of raw-machine-state check.
fn native_sp() -> u64 {
    let sp: u64;
    unsafe {
        std::arch::asm!("mov {}, sp", out(reg) sp);
    }
    sp
}

/// tests_s10.md's `compiled_frame_teardown_exact`: the native stack
/// pointer must be EXACTLY where it was before `enter_compiled`, both
/// after a normal smi-fast-path return and after the smi-inline op's own
/// FAIL edge fires (an overflowing add). S11 step 7 replaced the old
/// bailout-sentinel-and-restart mechanism with a real generic send that
/// stays inside the SAME compiled call (D1) — so the second case is no
/// longer `EnterResult::Bailout` either, just an ordinary `Completed` whose
/// result came from the fallback method instead of the fused fast path.
/// Still worth checking directly: an imbalance here would silently corrupt
/// the REST of this process's native call stack, not just this one call,
/// regardless of which path produced the result.
#[test]
fn compiled_frame_teardown_exact() {
    let mut vm = test_vm();
    let plus_sel = vm.universe.intern(b"+");

    let mut b = BytecodeBuilder::new();
    b.push_self();
    b.push_temp(0);
    b.send(&mut vm, plus_sel, 1);
    b.ret_tos();
    let m_sel = vm.universe.intern(b"plusArg:");
    let method = b.finish(&mut vm, m_sel, 1, 0);

    let plus_target_body = {
        let mut pb = BytecodeBuilder::new();
        pb.ret_self();
        let m = pb.finish(&mut vm, plus_sel, 1, 0);
        m.set_primitive(1);
        // `run_method`'s own `try_primitive` step (S11 step 7) now tries
        // this primitive for real before ever falling to `^self` -- an
        // overflowing `+` genuinely Fails it, so `prim_fails` must be set
        // (previously masked by `run_method`'s own missing primitive step,
        // which silently skipped straight to the bytecode body).
        m.set_flags(1, 0, false, false, true, false, 0);
        m
    };
    let smi_klass = vm.universe.smi_klass;
    let epoch = vm.ic_epoch;
    InterpreterIc::at(method, 0).set_mono(&mut vm, smi_klass, plus_target_body, epoch);
    // S11 step 7: an overflowing smi-add no longer bails out -- it sends
    // '+' generically (D1: "the LargeInteger/Double fallback via the
    // interpreter callee"), which needs a REAL method in smi_klass's own
    // dictionary to find (the `set_mono` above only seeds this SITE's
    // inline-cache for eligibility/inlining purposes, a separate thing
    // from the klass's real method dictionary `lookup` actually walks).
    // Reusing `plus_target_body` (`^self`) here too keeps this test's
    // fallback trivially predictable.
    install_method(&mut vm, smi_klass, plus_sel, plus_target_body);

    let id = driver::compile_method(&mut vm, smi_klass, method).expect("must compile");

    // Normal (non-bailout) call.
    vm.stack.push(SmallInt::new(5).oop());
    vm.stack.push(SmallInt::new(37).oop());
    let sp_before = native_sp();
    let result = macvm::interpreter::compiled_call::enter_compiled(&mut vm, id, 1);
    let sp_after = native_sp();
    assert_eq!(
        sp_before, sp_after,
        "native sp must be exactly restored after a normal compiled return"
    );
    assert_eq!(
        result,
        macvm::interpreter::compiled_call::EnterResult::Completed
    );
    assert_eq!(vm.stack.pop(), SmallInt::new(42).oop());

    // S13 step 7b: the OVERFLOW fail edge is now a deopt `brk`, needing a live
    // SIGTRAP handler (a JIT-armed VmState). Native-sp restoration ACROSS a
    // deopt (the trampoline discards the whole compiled frame and returns to
    // the native caller) is checked in
    // `compiled_smi_overflow_deopts_to_interpreter` below; the fast-path
    // teardown this test targets is fully proven by the non-overflow half
    // above.
}

// ── Listing goldens (tests_s10.md gate item 2, integration item 1) ────────

fn golden_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn check_golden_lst(name: &str, actual: &str) {
    let path = golden_dir().join(format!("{name}.lst.expected"));
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading golden {}: {e}", path.display()));
    assert_eq!(
        actual, expected,
        "golden {name} mismatch (run with UPDATE_GOLDEN=1 to inspect/regenerate)"
    );
}

fn load_source(vm: &mut VmState, src: &str) {
    let items = parser::parse_file(src).expect("parse");
    for item in items {
        classdef::execute_top_item(vm, item).expect("execute");
    }
}

fn klass_named(vm: &mut VmState, name: &str) -> KlassOop {
    let sym = vm.universe.intern(name.as_bytes());
    let assoc = macvm::runtime::globals::global_lookup(vm, sym)
        .unwrap_or_else(|| panic!("global '{name}' not found"));
    KlassOop::try_from(MemOop::try_from(assoc).unwrap().body_oop(1))
        .unwrap_or_else(|| panic!("'{name}' is not a class"))
}

fn method_named(vm: &mut VmState, klass: KlassOop, selector: &str) -> MethodOop {
    let sel = vm.universe.intern(selector.as_bytes());
    macvm::runtime::lookup::lookup(vm, klass, sel)
        .unwrap_or_else(|| panic!("'{selector}' not installed on the given class"))
}

/// A minimal but functionally real `SmallInteger` primitive method (a
/// bare-bones fallback body, never actually reached since these goldens'
/// own arguments never overflow) — a bare `VmState` has no real
/// `SmallInteger` methods at all (`world/06_smallinteger.mst` isn't
/// loaded), matching every other real-arithmetic test in this session.
fn install_smi_prim(vm: &mut VmState, name: &[u8], argc: usize, prim: i64) {
    let smi_klass = vm.universe.smi_klass;
    let sel = vm.universe.intern(name);
    let mut b = BytecodeBuilder::new();
    b.push_self();
    b.ret_self();
    let m = b.finish(vm, sel, argc, 0);
    m.set_primitive(prim);
    let sel = vm.universe.intern(name); // re-intern: finish may have moved things
    macvm::runtime::lookup::install_method(vm, smi_klass, sel, m);
}

/// Compiles `method` via the real pipeline (`driver::eligible` — the same
/// gate `driver::compile_method` itself uses — then decode/convert/
/// regalloc/emit directly, since `compile_method`'s own `Nmethod` doesn't
/// carry its `CodeBlob`'s listing around; production nmethods have no use
/// for keeping their own disassembly alive). Panics if `method` turns out
/// ineligible — every golden here is chosen to be eligible once warm.
fn compile_and_get_listing(vm: &VmState, method: MethodOop) -> String {
    assert!(
        driver::eligible(vm, method),
        "golden method must be eligible (was it called enough times to warm its own inner ICs?)"
    );
    let cfg = macvm::compiler::decode::decode(method);
    let ir = macvm::compiler::ir::convert(vm, method, &cfg);
    let ra = regalloc::regalloc(&ir);
    let mut asm = JasmAssembler::new();
    // None: this helper predates S11's guard and backs the already-committed
    // S10 listing goldens (s10_sumTo/absDiff/bitsOf) -- keeping their output
    // unchanged is the point, not something to revisit as a side effect of
    // step 2's own scope.
    let (blob, _pcs, _verified_entry_off, _ic_sites, _safepoints) = emit::emit(
        &mut asm,
        &ir,
        &ra,
        0xDEAD_BEEF_0000_0000,
        0xDEAD_BEEF_0000_0001,
        0xDEAD_BEEF_0000_0002,
        None,
    );
    blob.listing.join("\n") + "\n"
}

const GOLDEN_SOURCE: &str = "\
Object subclass: Tier1Golden [\n\
\x20   sumTo: n [\n\
\x20       | s |\n\
\x20       s := 0.\n\
\x20       1 to: n do: [:i | s := s + i].\n\
\x20       ^s\n\
\x20   ]\n\
\x20   absDiff: a with: b [\n\
\x20       ^(a > b)\n\
\x20           ifTrue: [ a - b ]\n\
\x20           ifFalse: [ b - a ]\n\
\x20   ]\n\
\x20   bitsOf: x [\n\
\x20       ^((x bitAnd: 16r0F) bitOr: (x bitXor: 16rFF)) bitAnd: 16rFF\n\
\x20   ]\n\
]\n";

/// Common setup for all three listing goldens: a bare `VmState` (no
/// `.mst` world load needed — these methods only ever touch
/// `SmallInteger`), `Tier1Golden`'s three methods loaded from real
/// source via the real frontend (not `BytecodeBuilder`), and every smi
/// primitive they transitively need installed directly (D1-eligible
/// inlining needs each inner send's own IC to be mono-smi-warm, which
/// only happens by actually running the method body at least once
/// interpreted first).
fn golden_vm() -> (VmState, KlassOop) {
    let mut vm = test_vm();
    install_smi_prim(&mut vm, b"+", 1, 1);
    install_smi_prim(&mut vm, b"-", 1, 2);
    install_smi_prim(&mut vm, b">", 1, 12);
    install_smi_prim(&mut vm, b"<=", 1, 11);
    install_smi_prim(&mut vm, b"bitAnd:", 1, 6);
    install_smi_prim(&mut vm, b"bitOr:", 1, 7);
    install_smi_prim(&mut vm, b"bitXor:", 1, 8);
    load_source(&mut vm, GOLDEN_SOURCE);
    let klass = klass_named(&mut vm, "Tier1Golden");
    (vm, klass)
}

/// `run_method` (a real send-warmup call) can allocate — so `method`,
/// a heap oop, must be re-derived fresh afterward rather than reused from
/// before the call (which a `MACVM_GC_STRESS=1` run of this same suite
/// would have moved). `klass` itself is re-derived via `klass_named`'s own
/// global-dictionary lookup (a root, so always current) rather than reused
/// too, for the same reason — this codebase's own established convention
/// (see e.g. `it_frontend_golden.rs`'s `install_prim` doc comment) for
/// anything that must survive an allocating call in between.
#[test]
fn golden_sum_to() {
    let (mut vm, klass) = golden_vm();
    let recv = macvm::memory::alloc::alloc_slots(&mut vm, klass).oop();
    let warmup_method = method_named(&mut vm, klass, "sumTo:");
    // Warm sumTo:'s own inner sends (+ and the inlined to:do:'s <=) by
    // actually running it interpreted once first.
    macvm::interpreter::run_method(&mut vm, warmup_method, recv, &[SmallInt::new(3).oop()]);
    let klass = klass_named(&mut vm, "Tier1Golden");
    let method = method_named(&mut vm, klass, "sumTo:");
    let listing = compile_and_get_listing(&vm, method);
    check_golden_lst("s10_sumTo", &listing);
}

#[test]
fn golden_abs_diff() {
    let (mut vm, klass) = golden_vm();
    // ifTrue: and ifFalse: each have their OWN `-` send site (distinct
    // bytecode positions, distinct ICs) -- a single call only ever takes
    // one branch, leaving the other's IC empty. Both need warming before
    // `eligible` sees every site as mono-smi. `recv` is re-allocated fresh
    // for each call (not reused across it) since it's a heap oop an
    // allocating call could move.
    let recv1 = macvm::memory::alloc::alloc_slots(&mut vm, klass).oop();
    let warmup_method = method_named(&mut vm, klass, "absDiff:with:");
    macvm::interpreter::run_method(
        &mut vm,
        warmup_method,
        recv1,
        &[SmallInt::new(10).oop(), SmallInt::new(3).oop()], // a > b: ifTrue:
    );
    let klass2 = klass_named(&mut vm, "Tier1Golden");
    let recv2 = macvm::memory::alloc::alloc_slots(&mut vm, klass2).oop();
    let warmup_method2 = method_named(&mut vm, klass2, "absDiff:with:");
    macvm::interpreter::run_method(
        &mut vm,
        warmup_method2,
        recv2,
        &[SmallInt::new(3).oop(), SmallInt::new(10).oop()], // a < b: ifFalse:
    );
    let klass = klass_named(&mut vm, "Tier1Golden");
    let method = method_named(&mut vm, klass, "absDiff:with:");
    let listing = compile_and_get_listing(&vm, method);
    check_golden_lst("s10_absDiff", &listing);
}

#[test]
fn golden_bits_of() {
    let (mut vm, klass) = golden_vm();
    let recv = macvm::memory::alloc::alloc_slots(&mut vm, klass).oop();
    let warmup_method = method_named(&mut vm, klass, "bitsOf:");
    macvm::interpreter::run_method(&mut vm, warmup_method, recv, &[SmallInt::new(0xA5).oop()]);
    let klass = klass_named(&mut vm, "Tier1Golden");
    let method = method_named(&mut vm, klass, "bitsOf:");
    let listing = compile_and_get_listing(&vm, method);
    check_golden_lst("s10_bitsOf", &listing);
}

// ── compiled_result_equals_interpreted (tests_s10.md gate item 2, ─────────
// integration item 2): "the micro-differential harness".

/// A `VmState` with every smi binary op the differential methods below
/// need, real `SMI_INLINE` primitive ids (`compiler::driver::SMI_INLINE`)
/// so each inner send can actually classify as mono-smi-inlinable once
/// warm.
fn diff_vm() -> VmState {
    let mut vm = test_vm();
    install_smi_prim(&mut vm, b"+", 1, 1);
    install_smi_prim(&mut vm, b"-", 1, 2);
    install_smi_prim(&mut vm, b"*", 1, 3);
    install_smi_prim(&mut vm, b"<", 1, 10);
    install_smi_prim(&mut vm, b">=", 1, 13);
    install_smi_prim(&mut vm, b"=", 1, 14);
    vm
}

/// `^a with: b` for a single binary smi selector already installed by
/// [`diff_vm`] -- argc=2, no temps, `self` untouched (every differential
/// method here ignores its own receiver, matching `bitsOf:`/`sumTo:`'s own
/// convention above).
fn build_binop_method(vm: &mut VmState, method_name: &[u8], sel_name: &[u8]) -> MethodOop {
    let sel = vm.universe.intern(sel_name);
    let mut b = BytecodeBuilder::new();
    b.push_temp(0);
    b.push_temp(1);
    b.send(vm, sel, 1);
    b.ret_tos();
    let m_sel = vm.universe.intern(method_name);
    b.finish(vm, m_sel, 2, 0)
}

/// `compareChain: a with: b with: c` ==
/// `(a < b) ifTrue: [1] ifFalse: [(b < c) ifTrue: [2] ifFalse: [3]]` --
/// argc=3, two distinct `<` send sites (real Smalltalk source's own
/// ifTrue:ifFalse: inlining produces exactly this branch shape --
/// `decode.rs`'s `leaders_if_else` test's own convention -- so raw
/// `br_false_fwd`/`jump_fwd` here matches what the real frontend would
/// have emitted, not a synthetic shortcut).
fn build_compare_chain_method(vm: &mut VmState) -> MethodOop {
    let lt_sel = vm.universe.intern(b"<");
    let mut b = BytecodeBuilder::new();
    b.push_temp(0);
    b.push_temp(1);
    b.send(vm, lt_sel, 1);
    let else1 = b.new_label();
    let end = b.new_label();
    b.br_false_fwd(else1);
    b.push_smi_i8(1);
    b.jump_fwd(end);
    b.bind(else1);
    b.push_temp(1);
    b.push_temp(2);
    b.send(vm, lt_sel, 1);
    let else2 = b.new_label();
    b.br_false_fwd(else2);
    b.push_smi_i8(2);
    b.jump_fwd(end);
    b.bind(else2);
    b.push_smi_i8(3);
    b.bind(end);
    b.ret_tos();
    let m_sel = vm.universe.intern(b"compareChain:with:with:");
    b.finish(vm, m_sel, 3, 0)
}

/// `rank: a with: b` ==
/// `(a >= b) ifTrue: [(a = b) ifTrue: [0] ifFalse: [1]] ifFalse: [-1]` --
/// argc=2, `>=`/`=` (a second, distinct pair of `SMI_INLINE` comparison
/// ops from `compareChain:with:with:`'s `<`); `=`'s send site sits
/// strictly inside `>=`'s true branch.
fn build_rank_method(vm: &mut VmState) -> MethodOop {
    let ge_sel = vm.universe.intern(b">=");
    let eq_sel = vm.universe.intern(b"=");
    let mut b = BytecodeBuilder::new();
    b.push_temp(0);
    b.push_temp(1);
    b.send(vm, ge_sel, 1);
    let else1 = b.new_label();
    let end = b.new_label();
    b.br_false_fwd(else1);
    b.push_temp(0);
    b.push_temp(1);
    b.send(vm, eq_sel, 1);
    let else2 = b.new_label();
    b.br_false_fwd(else2);
    b.push_smi_i8(0);
    b.jump_fwd(end);
    b.bind(else2);
    b.push_smi_i8(1);
    b.jump_fwd(end);
    b.bind(else1);
    b.push_smi_i8(-1);
    b.bind(end);
    b.ret_tos();
    let m_sel = vm.universe.intern(b"rank:with:");
    b.finish(vm, m_sel, 2, 0)
}

/// `tempShuffle: a with: b with: c` ==
/// `| t | t := a + b. a := b. b := t + c. ^(a * 100) + b` -- argc=3 plus
/// one true local (temp index 3), reassigning argument slots 0/1 as if
/// they were locals too. The "temp shuffle" case: several vregs whose
/// values get reordered/renamed in straight-line code, stressing the same
/// "one persistent vreg per source temp" interval tracking this sprint's
/// loop-carried-interval regalloc bugfix had to widen (`regalloc.rs`'s
/// `compute_intervals`).
fn build_temp_shuffle_method(vm: &mut VmState) -> MethodOop {
    let plus_sel = vm.universe.intern(b"+");
    let mul_sel = vm.universe.intern(b"*");
    let mut b = BytecodeBuilder::new();
    b.push_temp(0); // a
    b.push_temp(1); // b
    b.send(vm, plus_sel, 1); // a+b
    b.store_temp_pop(3); // t := a+b
    b.push_temp(1); // b
    b.store_temp_pop(0); // a := b
    b.push_temp(3); // t
    b.push_temp(2); // c
    b.send(vm, plus_sel, 1); // t+c
    b.store_temp_pop(1); // b := t+c
    b.push_temp(0); // a (= old b)
    b.push_smi_i8(100);
    b.send(vm, mul_sel, 1); // a*100
    b.push_temp(1); // b (= old (a+b)+c)
    b.send(vm, plus_sel, 1); // (a*100)+b
    b.ret_tos();
    let m_sel = vm.universe.intern(b"tempShuffle:with:with:");
    b.finish(vm, m_sel, 3, 1)
}

/// Warms `method` interpreted with every arg tuple in `warmups` (enough of
/// them, with the right truth values, to reach and warm EVERY send site --
/// a method with two mutually exclusive branches needs one warmup call
/// down each side, same requirement `golden_abs_diff` documents above),
/// compiles it once, then for every `(args, expected)` in `cases` checks
/// three independent computations of the same call agree: a fresh
/// interpreted `run_method`, a direct invocation of the compiled entry,
/// and `expected` itself -- a hand-derived answer, not just whatever the
/// two paths happened to agree on (a bug shared by both would otherwise
/// slip through a pure interpreted-vs-compiled diff).
///
/// `method` is the one value here that must survive multiple allocating
/// `run_method` calls, so it's handle-protected for this whole call
/// (`memory::handles`'s documented purpose) -- there's no klass/selector
/// install to re-derive it from the way the golden tests above do, since
/// these methods are built directly via `BytecodeBuilder`, never installed
/// on any class. `dummy_recv` needs no such protection: a `SmallInt` is an
/// immediate value, never a heap oop, so it can never move -- and every
/// method built above ignores its own receiver anyway.
fn assert_tier1_diff(
    vm: &mut VmState,
    method: MethodOop,
    warmups: &[&[i64]],
    cases: &[(&[i64], i64)],
) {
    let scope = macvm::memory::handles::HandleScope::enter(vm);
    let method_h = scope.handle(vm, method);
    let dummy_recv = SmallInt::new(0).oop();

    for w in warmups {
        let method = method_h.get(vm);
        let arg_oops: Vec<Oop> = w.iter().map(|&v| SmallInt::new(v).oop()).collect();
        macvm::interpreter::run_method(vm, method, dummy_recv, &arg_oops);
    }

    let method = method_h.get(vm);
    let smi_klass = vm.universe.smi_klass;
    let id = driver::compile_method(vm, smi_klass, method)
        .unwrap_or_else(|| panic!("differential method must be eligible+compilable once warm"));
    let nm = vm.code_table.get(id).expect("installed");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;
    let stubs = vm.stubs;

    for &(args, expected) in cases {
        let expected_oop = SmallInt::new(expected).oop();

        let method = method_h.get(vm);
        let arg_oops: Vec<Oop> = args.iter().map(|&v| SmallInt::new(v).oop()).collect();
        let interp_result = macvm::interpreter::run_method(vm, method, dummy_recv, &arg_oops);
        assert_eq!(
            interp_result,
            expected_oop,
            "interpreted{args:?}: expected {expected}, got raw {:#x}",
            interp_result.raw()
        );

        let mut argv: Vec<u64> = vec![dummy_recv.raw()];
        argv.extend(args.iter().map(|&v| SmallInt::new(v).oop().raw()));
        let compiled_result = stubs.invoke(entry, vm, &argv);
        assert_eq!(
            compiled_result,
            expected_oop.raw(),
            "compiled{args:?}: expected {expected} ({:#x}), got raw {compiled_result:#x}",
            expected_oop.raw()
        );
    }
}

/// tests_s10.md's `compiled_result_equals_interpreted`: ~10 arithmetic
/// methods (add/sub/mul right at the +/-2^61 smi boundary, comparison
/// chains, temp shuffles), each run interpreted and compiled, checked
/// against each other and an independently hand-computed value.
/// Deliberately stays within `[SMI_MIN, SMI_MAX]` throughout -- genuine
/// overflow-then-bailout-then-reinterpret coverage belongs to
/// `bailout_falls_back_correctly` instead: a real overflow's interpreted
/// answer is a heap `LargeInteger`, which "assert identical result oops
/// (smi equality)" (tests_s10.md's own words for this test) doesn't
/// describe.
#[test]
fn compiled_result_equals_interpreted() {
    let mut vm = diff_vm();

    let add_max = build_binop_method(&mut vm, b"addNearMax:with:", b"+");
    assert_tier1_diff(
        &mut vm,
        add_max,
        &[&[1, 1]],
        &[(&[SmallInt::MAX - 1, 1], SmallInt::MAX)],
    );

    let add_min = build_binop_method(&mut vm, b"addNearMin:with:", b"+");
    assert_tier1_diff(
        &mut vm,
        add_min,
        &[&[1, 1]],
        &[(&[SmallInt::MIN + 1, -1], SmallInt::MIN)],
    );

    let sub_max = build_binop_method(&mut vm, b"subNearMax:with:", b"-");
    assert_tier1_diff(
        &mut vm,
        sub_max,
        &[&[1, 1]],
        &[(&[SmallInt::MAX - 1, -1], SmallInt::MAX)],
    );

    let sub_min = build_binop_method(&mut vm, b"subNearMin:with:", b"-");
    assert_tier1_diff(
        &mut vm,
        sub_min,
        &[&[1, 1]],
        &[(&[SmallInt::MIN + 1, 1], SmallInt::MIN)],
    );

    let sub_zero_max = build_binop_method(&mut vm, b"subZeroMinusMax:with:", b"-");
    assert_tier1_diff(
        &mut vm,
        sub_zero_max,
        &[&[1, 1]],
        &[(&[0, SmallInt::MAX], -SmallInt::MAX)],
    );

    let mul_pos = build_binop_method(&mut vm, b"mulLargePos:with:", b"*");
    assert_tier1_diff(
        &mut vm,
        mul_pos,
        &[&[2, 3]],
        &[(&[1_500_000_000, 1_500_000_000], 2_250_000_000_000_000_000)],
    );

    let mul_neg = build_binop_method(&mut vm, b"mulLargeNeg:with:", b"*");
    assert_tier1_diff(
        &mut vm,
        mul_neg,
        &[&[2, 3]],
        &[(&[-1_500_000_000, 1_500_000_000], -2_250_000_000_000_000_000)],
    );

    // `compareChain:with:with:`'s two `<` sites: one warmup with the first
    // true (site 1 only), one with it false (reaches and warms site 2).
    let compare_chain = build_compare_chain_method(&mut vm);
    assert_tier1_diff(
        &mut vm,
        compare_chain,
        &[&[1, 2, 3], &[5, 2, 3]],
        &[
            (&[10, 20, 5], 1), // a < b
            (&[10, 5, 9], 2),  // a >= b, b < c
            (&[10, 5, 1], 3),  // a >= b, b >= c
        ],
    );

    // `rank:with:`'s `=` site sits strictly inside `>=`'s true branch -- a
    // single warmup with a>=b true already reaches and warms both.
    let rank = build_rank_method(&mut vm);
    assert_tier1_diff(
        &mut vm,
        rank,
        &[&[5, 5]],
        &[(&[5, 5], 0), (&[5, 2], 1), (&[2, 5], -1)],
    );

    // t := a+b; a := b; b := t+c; ^(a*100)+b.
    // a=7,b=13,c=5:    t=20,  a=13, b=25,  result=13*100+25=1325.
    // a=100,b=1,c=1:   t=101, a=1,  b=102, result=1*100+102=202.
    let shuffle = build_temp_shuffle_method(&mut vm);
    assert_tier1_diff(
        &mut vm,
        shuffle,
        &[&[1, 2, 3]],
        &[(&[7, 13, 5], 1325), (&[100, 1, 1], 202)],
    );
}

// ── mixed_trace_golden (tests_s10.md gate item 4, integration item 6) ─────

fn check_golden_trace(name: &str, actual: &str) {
    let path = golden_dir().join(format!("{name}.trace.expected"));
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading golden {}: {e}", path.display()));
    assert_eq!(
        actual, expected,
        "golden {name} mismatch (run with UPDATE_GOLDEN=1 to inspect/regenerate)"
    );
}

const MIXED_TRACE_SOURCE: &str = "\
Object subclass: Tier1Trace [\n\
\x20   callB: n [\n\
\x20       ^self sumHelper: n\n\
\x20   ]\n\
\x20   sumHelper: n [\n\
\x20       | s |\n\
\x20       s := 0.\n\
\x20       1 to: n do: [:i | s := s + i].\n\
\x20       ^s\n\
\x20   ]\n\
]\n";

/// `callB:`'s own body is a single opaque send to a non-primitive,
/// non-`SMI_INLINE` method (`sumHelper:`) — structurally `NoPermanent`
/// under `driver::eligibility_detail` (D1's own opcode allowlist), so
/// `callB:` itself can never become eligible no matter how many times
/// it's called. That's exactly the shape this test needs: an interpreted
/// caller `a` (`callB:`) that stays interpreted forever, sending to a
/// compiled callee `b` (`sumHelper:`, `sumTo:`'s own loop shape, so it
/// gets a real `Poll` at its back-edge).
#[test]
fn mixed_trace_golden() {
    let mut vm = VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: Default::default(),
        gc_stress: false,
        gc_stress_full_period: None,
        eden_kb: None,
        jit: JitMode::Threshold(2),
    });
    install_smi_prim(&mut vm, b"+", 1, 1);
    install_smi_prim(&mut vm, b"<=", 1, 11);
    load_source(&mut vm, MIXED_TRACE_SOURCE);

    // Calls 1-2 (small, safe n): call 1 warms sumHelper:'s own inner `+`/
    // `<=` ICs interpreted; call 2 crosses sumHelper:'s invocation
    // threshold (driven through callB:'s own real send site, exactly like
    // send.rs's compile_trigger_fires_and_rewrites_ic_to_compiled), so by
    // the time it returns callB:'s inner send site targets a real
    // compiled nmethod.
    for _ in 0..2 {
        let klass = klass_named(&mut vm, "Tier1Trace");
        let method = method_named(&mut vm, klass, "callB:");
        let recv = macvm::memory::alloc::alloc_slots(&mut vm, klass).oop();
        macvm::interpreter::run_method(&mut vm, method, recv, &[SmallInt::new(1).oop()]);
    }

    let buf = macvm::runtime::vm_state::OutputBuffer::new();
    vm.out = Box::new(buf.clone());

    // poll_flag gates whether the compiled loop's back-edge bothers
    // calling stub_poll at all (nothing else in S10 ever sets it,
    // `codecache::stubs::rt_poll`'s own doc); trace_on_poll gates what
    // rt_poll does once actually reached. n=3 gives two back-edge
    // crossings (i: 1->2, 2->3) -- trace_on_poll is one-shot, so only the
    // first one prints.
    vm.reg_block.poll_flag = 1;
    vm.trace_on_poll = true;

    let klass = klass_named(&mut vm, "Tier1Trace");
    let method = method_named(&mut vm, klass, "callB:");
    let recv = macvm::memory::alloc::alloc_slots(&mut vm, klass).oop();
    let result = macvm::interpreter::run_method(&mut vm, method, recv, &[SmallInt::new(3).oop()]);
    assert_eq!(
        result,
        SmallInt::new(6).oop(),
        "1+2+3 = 6, unaffected by the poll firing"
    );

    assert!(
        !vm.trace_on_poll,
        "the poll must actually have fired and consumed the one-shot flag \
         (otherwise this test isn't exercising rt_poll at all)"
    );

    check_golden_trace("s10_mixed_trace", &buf.as_string());
}

// ── Stress/negative tests (tests_s10.md's own section) ─────────────────────

/// tests_s10.md: "`threshold=1` + tiny code cache (test hook: 64 KiB) --
/// cache exhausts mid-suite; compilation stops gracefully (log line, no
/// panic), suite still passes interpreted." `compile_method` has no
/// cache-size knob on `VmOptions`, and adding one would mean touching
/// every existing `VmOptions{...}` literal across the whole suite for a
/// test-only need -- the simpler hook is that `vm.code_cache` is already
/// a public field with a public `alloc`, so pre-consuming most of a
/// freshly-constructed (still normally-sized, stubs still valid) cache
/// directly reproduces the same "nearly exhausted" starting condition
/// without touching production code at all.
#[test]
fn threshold1_tiny_code_cache_exhausts_gracefully() {
    let mut vm = diff_vm();
    vm.options.jit = JitMode::Threshold(1);

    // S11 step 2 added the klass-guard prologue to every compiled method
    // (bigger than S10's own bare verified_entry-only shape), so this
    // budget needs headroom for at least a few full method-sizes, not just
    // one -- tuned empirically against the actual current size rather than
    // hand-estimated, and re-tune again whenever emit.rs's own prologue
    // shape changes enough to matter (a golden-test-like maintenance cost,
    // not a correctness one).
    let leave_free = 2048usize;
    let prefill = macvm::codecache::DEFAULT_CODE_CACHE_CAPACITY.saturating_sub(leave_free);
    vm.code_cache
        .alloc(prefill)
        .expect("prefilling most of a freshly-constructed cache must itself succeed");

    let methods: Vec<MethodOop> = (0..12)
        .map(|i| build_binop_method(&mut vm, format!("exCache{i}:with:").as_bytes(), b"+"))
        .collect();

    let dummy_recv = SmallInt::new(0).oop();
    let mut successes = 0usize;
    let mut failures = 0usize;
    for &m in &methods {
        // Warms the inner `+` send's own IC via one ordinary interpreted
        // run -- driver::compile_method is called directly below (not
        // through activate_method's counter trigger), so nothing here
        // needs to "cross a threshold", only make the method eligible.
        macvm::interpreter::run_method(
            &mut vm,
            m,
            dummy_recv,
            &[SmallInt::new(1).oop(), SmallInt::new(1).oop()],
        );
        let smi_klass = vm.universe.smi_klass;
        match driver::compile_method(&mut vm, smi_klass, m) {
            Some(_) => successes += 1,
            None => failures += 1,
        }
    }

    assert!(
        successes > 0,
        "a nearly-full (not literally empty) cache must still grant a few compiles \
         before exhausting"
    );
    assert!(
        failures > 0,
        "the prefill must actually have driven the cache to exhaustion partway through \
         -- otherwise this test isn't exercising the exhaustion path at all"
    );
    assert_eq!(
        vm.options.jit,
        JitMode::Off,
        "compile_method's own cache-exhaustion handling must disable the JIT \
         for the rest of the run"
    );

    // "suite still passes interpreted": every method -- including ones
    // that successfully compiled before exhaustion -- still gives the
    // right answer on a fresh call afterward.
    for &m in &methods {
        let r = macvm::interpreter::run_method(
            &mut vm,
            m,
            dummy_recv,
            &[SmallInt::new(3).oop(), SmallInt::new(4).oop()],
        );
        assert_eq!(r, SmallInt::new(7).oop());
    }
}

/// tests_s10.md's `compile_disabled` churn, driven through the real CLI
/// (`world/bench/churn.mst`, `just bench-s10`'s sibling target for this):
/// `MACVM_TRACE=jit`'s `[jit] ineligible, compile_disabled: ...` line is
/// written via `eprintln!`, not `vm.out` -- a real process-stderr capture
/// is the only way to scrape it (matching `it_world.rs`'s own
/// `error_kills_with_trace`, the established precedent in this suite for
/// "genuinely needs a subprocess" cases). `ChurnIneligible>>
/// tooManyArgs:bar:baz:qux:quux:corge:` has 6 explicit args -- D1's own
/// `argc>5` rule -- so `eligibility_detail` returns `NoPermanent` on its
/// very first attempt and `compile_disabled()` latches permanently;
/// `activate_method`'s own `!m.compile_disabled()` gate must then prevent
/// every one of the other 99,999 calls from ever reaching
/// `eligibility_detail` again.
#[test]
fn compile_disabled_churn() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_macvm"));
    let world_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("world");
    let script = world_dir.join("bench").join("churn.mst");

    let out = Command::new(bin)
        .args([
            "run",
            script.to_str().unwrap(),
            "--world",
            world_dir.to_str().unwrap(),
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("MACVM_JIT", "threshold=1")
        .env("MACVM_TRACE", "jit")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn macvm");

    assert!(
        out.status.success(),
        "churn.mst must run to completion, got status {:?}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("result: 21"),
        "1+2+3+4+5+6 = 21, unaffected by the compile attempts, got stdout:\n{stdout}"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    let disabled_lines = stderr
        .lines()
        .filter(|l| l.contains("tooManyArgs:bar:baz:qux:quux:corge:"))
        .count();
    assert_eq!(
        disabled_lines, 1,
        "eligibility_detail must run exactly once across all 100,000 calls -- \
         activate_method's own compile_disabled() gate must prevent every \
         later call from re-attempting it, got {disabled_lines} occurrences \
         in stderr:\n{stderr}"
    );
}

/// tests_s10.md's "Debug-build frame asserts": process-stack sp unchanged
/// by a compiled call except the argc+1->1 replacement. The assertion
/// itself already lives in `enter_compiled` (`compiled_call.rs`'s own
/// `debug_assert_eq!`, exercised by every debug-build test in this whole
/// suite that ever takes the `Completed` path) -- this test exists to
/// give that property an explicit, named, findable regression home rather
/// than leaving it purely incidental, and to check it across more than
/// one argc (0, 1, 3), not just the argc=1 shape every other test in this
/// file happens to use.
#[test]
fn compiled_entry_stack_discipline_across_argc() {
    for argc in [0u8, 1, 3] {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.ret_self();
        let sel = vm.universe.intern(format!("argc{argc}Method").as_bytes());
        let method = b.finish(&mut vm, sel, argc as usize, 0);
        assert!(
            driver::eligible(&vm, method),
            "argc={argc}: ^self must be eligible (no sends, no closures, argc<=5)"
        );

        let smi_klass = vm.universe.smi_klass;
        let id = driver::compile_method(&mut vm, smi_klass, method)
            .unwrap_or_else(|| panic!("argc={argc}: must compile"));

        let recv = SmallInt::new(argc as i64).oop();
        vm.stack.push(recv);
        for a in 0..argc {
            vm.stack.push(SmallInt::new(a as i64).oop());
        }

        let sp_before = vm.stack.sp;
        let result = macvm::interpreter::compiled_call::enter_compiled(&mut vm, id, argc);
        assert_eq!(
            result,
            macvm::interpreter::compiled_call::EnterResult::Completed,
            "argc={argc}"
        );
        assert_eq!(
            vm.stack.sp,
            sp_before - argc as usize,
            "argc={argc}: net stack effect must be exactly argc+1 -> 1 \
             (popped receiver+args, pushed one result)"
        );
        assert_eq!(
            vm.stack.pop(),
            recv,
            "argc={argc}: ^self must return the receiver"
        );
    }
}

/// S11 step 3's own explicit test target (`sprint_s11_detail.md`'s
/// implementation order, item 3): "C->C mono calls work (test: two
/// compiled methods)". The callee (`S11Target>>foo:with:`) is compiled
/// through the REAL front door (`BytecodeBuilder` + `driver::
/// compile_method`, same as `compiled_plus_arg_executes_correctly`) and
/// genuinely `install_method`-ed first, so `rt_resolve_send`'s own
/// `runtime::lookup::lookup` call finds it exactly the way an interpreted
/// send would. The caller is hand-built `Ir` (S10's `convert()` never
/// constructs `Ir::CallSend` -- that's S11 step 7's job), published and
/// installed directly, its one `IcSite` starting `Unresolved` (its `bl`
/// pointed at `stub_resolve` -- S11 step 2's own patch loop, replicated
/// here by hand since there's no `driver::compile_method` front door for
/// hand-built `Ir` yet).
///
/// `foo:with:` returns its SECOND real argument unchanged -- deliberately
/// not the receiver and not the first argument, so a correct result can
/// only come from x2 (the third RootSpill slot) having survived
/// `stub_resolve`'s own spill/reload AND landed in the right register at
/// the callee's own entry; any bug in either would either crash or return
/// a wrong/unrelated value, never accidentally the right one.
#[test]
fn mono_resolve_patches_call_site_and_dispatches() {
    let mut vm = test_vm();

    let target_klass = vm.universe.new_klass(
        vm.universe.object_klass,
        "S11Target",
        Format::Slots,
        false,
        HEADER_WORDS,
    );
    let foo_sel = vm.universe.intern(b"foo:with:");
    let mut fb = BytecodeBuilder::new();
    fb.push_temp(1); // second real arg
    fb.ret_tos();
    let foo_method = fb.finish(&mut vm, foo_sel, 2, 0);
    install_method(&mut vm, target_klass, foo_sel, foo_method);
    assert!(
        driver::eligible(&vm, foo_method),
        "push_temp+ret_tos has no sends, trivially eligible"
    );
    let callee_id = driver::compile_method(&mut vm, target_klass, foo_method)
        .expect("eligible method must compile");
    let callee_nm = vm.code_table.get(callee_id).unwrap();
    let callee_entry = unsafe { callee_nm.code.base.add(callee_nm.entry_off as usize) } as u64;

    // Caller: one param (the target receiver), one send of `foo:with:`
    // against two fresh smi constants -- self=x0, arg0=x1, arg1=x2.
    let vregs: Vec<VRegInfo> = (0..4).map(|_| VRegInfo { is_oop: true }).collect();
    let block0 = IrBlock {
        id: BlockId(0),
        bci: 0,
        code: vec![
            Ir::Param {
                dst: VReg(0),
                index: 0,
            },
            Ir::ConstSmi {
                dst: VReg(1),
                value: 111,
            },
            Ir::ConstSmi {
                dst: VReg(2),
                value: 222,
            },
            Ir::CallSend {
                dst: VReg(3),
                site: 0,
                args: vec![VReg(0), VReg(1), VReg(2)],
            },
            Ir::Ret { val: VReg(3) },
        ],
        entry_stack: Vec::new(),
        deopt_sites: Vec::new(),
    };
    let caller_method = IrMethod {
        blocks: vec![block0],
        vregs,
        pool: Vec::new(),
        argc: 1,
        ntemps: 0,
        ctx_vregs: Vec::new(),
        safepoints: Vec::new(),
        true_lit: PoolLit(0),
        false_lit: PoolLit(0),
        nil_lit: PoolLit(0),
        mark_slots_lit: PoolLit(0),
        call_sites: vec![CallSiteInfo {
            selector: foo_sel,
            argc: 3,
            static_klass: None,
        }],
        site_feedback: Vec::new(),
        inline_deps: Vec::new(),
        method_pool_ix: None,
    };
    let ra = regalloc::regalloc(&caller_method);
    let mut asm = JasmAssembler::new();
    let (blob, _pcs, _verified_entry_off, emitted_ic_sites, _safepoints) = emit::emit(
        &mut asm,
        &caller_method,
        &ra,
        vm.stubs.stub_poll_addr(),
        vm.stubs.must_be_boolean_addr(),
        vm.stubs.alloc_slow_addr(),
        None,
    );
    assert_eq!(emitted_ic_sites.len(), 1, "exactly one Ir::CallSend");

    let h = vm.code_cache.alloc(blob.code.len()).unwrap();
    vm.code_cache.publish(h, &blob);
    let resolve_addr = vm.stubs.resolve_addr();
    for site in &emitted_ic_sites {
        vm.code_cache.patch_branch26_at(h, site.off, resolve_addr);
    }
    let caller_probe_sel = vm.universe.intern(b"s11CallerProbe");
    let ic_sites: Vec<IcSite> = emitted_ic_sites
        .iter()
        .map(|s| IcSite {
            off: s.off,
            selector: s.selector,
            argc: s.argc,
            state: IcState::Unresolved,
            super_klass: None,
        })
        .collect();
    let caller_nm = Nmethod {
        id: NmethodId(0),
        key_klass: target_klass,
        key_selector: caller_probe_sel,
        code: h,
        entry_off: 0,
        verified_entry_off: 0,
        state: NmState::Alive,
        level: 1,
        version: 0,
        literal_off: blob.literal_off,
        relocs: blob.relocs.clone(),
        frame_slots: ra.frame_slots,
        slot_is_oop: ra.slot_is_oop.clone(),
        pcdescs: Vec::new(),
        oopmaps: Vec::new(),
        ic_sites,
        poll_bci: None,
        deopt_scopes: Vec::new(),
        deopt_pcdescs: Vec::new(),
        inline_deps: Vec::new(),
    };
    let caller_id = vm.code_table.install(caller_nm);
    let caller_entry = h.base as u64; // entry_off == verified_entry_off == 0 (no guard, `None`)

    let receiver = alloc::alloc_slots(&mut vm, target_klass).oop();
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let argv = [receiver.raw()];

    let result = unsafe { call(caller_entry, vm_ptr, argv.as_ptr(), 1) };
    assert_eq!(
        result,
        SmallInt::new(222).oop().raw(),
        "first dispatch (through stub_resolve) must reach foo:with: and return its 2nd arg"
    );

    let nm_after = vm.code_table.get(caller_id).unwrap();
    match nm_after.ic_sites[0].state {
        IcState::Mono { klass, target } => {
            assert_eq!(klass, target_klass, "must record the receiver's own klass");
            assert_eq!(target, callee_entry, "must record foo:with:'s own entry");
        }
        other => panic!("expected Mono after the first resolve, got {other:?}"),
    }

    // Second call through the NOW-PATCHED site (same klass): must reach
    // foo:with: directly, never touching stub_resolve/rt_resolve_send's
    // Unresolved arm again -- exercises the "Mono, same klass" repatch
    // arm instead.
    let result2 = unsafe { call(caller_entry, vm_ptr, argv.as_ptr(), 1) };
    assert_eq!(result2, SmallInt::new(222).oop().raw());
    let nm_after2 = vm.code_table.get(caller_id).unwrap();
    match nm_after2.ic_sites[0].state {
        IcState::Mono { klass, target } => {
            assert_eq!(klass, target_klass);
            assert_eq!(target, callee_entry);
        }
        other => panic!("expected still-Mono after the second (same-klass) resolve, got {other:?}"),
    }
}

/// Builds a target klass with an UNCOMPILED `foo:with:` method (returns
/// its 2nd real arg, same shape as `mono_resolve_patches_call_site_and_
/// dispatches`'s own callee) plus a hand-built caller sending to it,
/// published and installed with one `Unresolved` `IcSite` -- the setup
/// both S11 step 4 (c2i) tests below share. `foo_method` is deliberately
/// NEVER passed to `driver::compile_method`, so `code_table.lookup` must
/// miss and `rt_resolve_send` must fall back to a c2i adapter (D6.1).
/// Returns `(caller_entry, target_klass, caller_id)`.
fn build_c2i_scenario(vm: &mut VmState) -> (u64, KlassOop, NmethodId) {
    let target_klass = vm.universe.new_klass(
        vm.universe.object_klass,
        "S11C2ITarget",
        Format::Slots,
        false,
        HEADER_WORDS,
    );
    let foo_sel = vm.universe.intern(b"foo:with:");
    let mut fb = BytecodeBuilder::new();
    fb.push_temp(1); // second real arg
    fb.ret_tos();
    let foo_method = fb.finish(vm, foo_sel, 2, 0);
    install_method(vm, target_klass, foo_sel, foo_method);

    let vregs: Vec<VRegInfo> = (0..4).map(|_| VRegInfo { is_oop: true }).collect();
    let block0 = IrBlock {
        id: BlockId(0),
        bci: 0,
        code: vec![
            Ir::Param {
                dst: VReg(0),
                index: 0,
            },
            Ir::ConstSmi {
                dst: VReg(1),
                value: 111,
            },
            Ir::ConstSmi {
                dst: VReg(2),
                value: 222,
            },
            Ir::CallSend {
                dst: VReg(3),
                site: 0,
                args: vec![VReg(0), VReg(1), VReg(2)],
            },
            Ir::Ret { val: VReg(3) },
        ],
        entry_stack: Vec::new(),
        deopt_sites: Vec::new(),
    };
    let caller_method = IrMethod {
        blocks: vec![block0],
        vregs,
        pool: Vec::new(),
        argc: 1,
        ntemps: 0,
        ctx_vregs: Vec::new(),
        safepoints: Vec::new(),
        true_lit: PoolLit(0),
        false_lit: PoolLit(0),
        nil_lit: PoolLit(0),
        mark_slots_lit: PoolLit(0),
        call_sites: vec![CallSiteInfo {
            selector: foo_sel,
            argc: 3,
            static_klass: None,
        }],
        site_feedback: Vec::new(),
        inline_deps: Vec::new(),
        method_pool_ix: None,
    };
    let ra = regalloc::regalloc(&caller_method);
    let mut asm = JasmAssembler::new();
    let (blob, _pcs, _verified_entry_off, emitted_ic_sites, _safepoints) = emit::emit(
        &mut asm,
        &caller_method,
        &ra,
        vm.stubs.stub_poll_addr(),
        vm.stubs.must_be_boolean_addr(),
        vm.stubs.alloc_slow_addr(),
        None,
    );
    assert_eq!(emitted_ic_sites.len(), 1, "exactly one Ir::CallSend");

    let h = vm.code_cache.alloc(blob.code.len()).unwrap();
    vm.code_cache.publish(h, &blob);
    let resolve_addr = vm.stubs.resolve_addr();
    for site in &emitted_ic_sites {
        vm.code_cache.patch_branch26_at(h, site.off, resolve_addr);
    }
    let caller_probe_sel = vm.universe.intern(b"s11C2ICallerProbe");
    let ic_sites: Vec<IcSite> = emitted_ic_sites
        .iter()
        .map(|s| IcSite {
            off: s.off,
            selector: s.selector,
            argc: s.argc,
            state: IcState::Unresolved,
            super_klass: None,
        })
        .collect();
    let caller_nm = Nmethod {
        id: NmethodId(0),
        key_klass: target_klass,
        key_selector: caller_probe_sel,
        code: h,
        entry_off: 0,
        verified_entry_off: 0,
        state: NmState::Alive,
        level: 1,
        version: 0,
        literal_off: blob.literal_off,
        relocs: blob.relocs.clone(),
        frame_slots: ra.frame_slots,
        slot_is_oop: ra.slot_is_oop.clone(),
        pcdescs: Vec::new(),
        oopmaps: Vec::new(),
        ic_sites,
        poll_bci: None,
        deopt_scopes: Vec::new(),
        deopt_pcdescs: Vec::new(),
        inline_deps: Vec::new(),
    };
    let caller_id = vm.code_table.install(caller_nm);
    let caller_entry = h.base as u64; // entry_off == verified_entry_off == 0 (no guard, `None`)
    (caller_entry, target_klass, caller_id)
}

/// S11 step 4's own explicit test target (`sprint_s11_detail.md`'s
/// implementation order, item 4): "C->I works". `foo:with:` is
/// deliberately left uncompiled (see `build_c2i_scenario`'s own doc) --
/// `rt_resolve_send` must fall back to a c2i adapter, and
/// `rt_interpret_call` must genuinely re-enter the bytecode interpreter
/// and hand back the right result.
#[test]
fn c2i_adapter_dispatches_to_interpreted_method() {
    let mut vm = test_vm();
    let (caller_entry, target_klass, caller_id) = build_c2i_scenario(&mut vm);

    let receiver = alloc::alloc_slots(&mut vm, target_klass).oop();
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let argv = [receiver.raw()];

    let result = unsafe { call(caller_entry, vm_ptr, argv.as_ptr(), 1) };
    assert_eq!(
        result,
        SmallInt::new(222).oop().raw(),
        "compiled caller -> c2i adapter -> interpreted foo:with: must return its 2nd arg"
    );
    assert!(
        vm.tier_links.is_empty(),
        "TierLink::IntoInterpreter must be popped again once rt_interpret_call returns"
    );

    let nm_after = vm.code_table.get(caller_id).unwrap();
    match nm_after.ic_sites[0].state {
        IcState::Mono { klass, target } => {
            assert_eq!(klass, target_klass, "must record the receiver's own klass");
            // `target` really is a directly-callable adapter entry --
            // invoking it a SECOND time, bypassing the caller entirely,
            // with FRESH args must independently reach foo:with: too.
            let argv2 = [
                receiver.raw(),
                SmallInt::new(1).oop().raw(),
                SmallInt::new(999).oop().raw(),
            ];
            let direct = unsafe { call(target, vm_ptr, argv2.as_ptr(), 3) };
            assert_eq!(
                direct,
                SmallInt::new(999).oop().raw(),
                "the recorded target address must itself be a valid, independently callable \
                 c2i adapter entry"
            );
        }
        other => panic!("expected Mono after resolving to a c2i adapter, got {other:?}"),
    }
}

/// The reentrancy hazard `interpreter::run_method_reentrant`'s own doc
/// warns about: `run_method` unconditionally deactivates `vm.stack` when
/// ITS OWN entry frame returns, which would silently corrupt an OUTER,
/// currently-paused interpreter activation's `fp`/`has_frame` bookkeeping
/// if this C->I call happens to be nested inside one (a real I->C->I
/// round trip). Fabricates that outer state directly (an arbitrary `fp`,
/// `has_frame=true`) rather than building a full 3-tier round trip by
/// hand -- the c2i path never dereferences `vm.stack`'s own slot contents
/// at that `fp`, only copies the `fp`/`has_frame` VALUES themselves
/// (`ProcessStack::save_activation`/`restore_activation`), so a
/// fabricated, never-pushed-to `fp` exercises exactly the same code path
/// a real nested activation would.
#[test]
fn c2i_call_preserves_outer_interpreter_activation() {
    let mut vm = test_vm();
    let (caller_entry, target_klass, _caller_id) = build_c2i_scenario(&mut vm);
    let receiver = alloc::alloc_slots(&mut vm, target_klass).oop();

    vm.stack.activate_frame(12345);
    let sp_before = vm.stack.sp;

    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let argv = [receiver.raw()];
    let result = unsafe { call(caller_entry, vm_ptr, argv.as_ptr(), 1) };
    assert_eq!(result, SmallInt::new(222).oop().raw());

    assert_eq!(
        vm.stack.fp, 12345,
        "an outer (fabricated) interpreter activation's own fp must survive a nested C->I \
         round trip"
    );
    assert!(
        vm.stack.has_frame(),
        "the outer activation must still be considered active after the nested call returns"
    );
    assert_eq!(
        vm.stack.sp, sp_before,
        "sp must net to exactly zero effect too"
    );
}

/// S11 step 5's own explicit test target (implementation order, item 5):
/// "full lattice". Builds `PIC_MAX_ENTRIES + 1` distinct klasses, each
/// with its OWN real compiled `foo` returning a distinct constant, then
/// sends the SAME hand-built call site to a fresh instance of each in
/// turn -- driving Unresolved -> Mono -> Pic{2} -> Pic{3} -> Pic{4} ->
/// Mega across consecutive calls, checking BOTH the dispatched result
/// AND the recorded `IcState` after every single one. Finishes by
/// re-dispatching to the FIRST klass through the now-`Mega` site, proving
/// `rt_mega_lookup` genuinely re-resolves per call (not just "whichever
/// klass triggered the promotion") and that `Mega` never regresses.
#[test]
fn full_ic_lattice_mono_to_pic_to_mega() {
    let mut vm = test_vm();
    let foo_sel = vm.universe.intern(b"foo");
    let n = PIC_MAX_ENTRIES + 1;

    // One real compiled `foo` per klass, each returning `100*(i+1)`.
    let mut klasses = Vec::with_capacity(n);
    for i in 0..n {
        let klass = vm.universe.new_klass(
            vm.universe.object_klass,
            &format!("S11Lattice{i}"),
            Format::Slots,
            false,
            HEADER_WORDS,
        );
        let mut fb = BytecodeBuilder::new();
        fb.push_literal(&mut vm, SmallInt::new(((i + 1) * 100) as i64).oop());
        fb.ret_tos();
        let m = fb.finish(&mut vm, foo_sel, 0, 0);
        install_method(&mut vm, klass, foo_sel, m);
        assert!(driver::eligible(&vm, m), "push_smi+ret has no sends");
        driver::compile_method(&mut vm, klass, m).expect("eligible method must compile");
        klasses.push(klass);
    }

    // Caller: one param (the target receiver), one send of `foo`.
    let vregs: Vec<VRegInfo> = (0..2).map(|_| VRegInfo { is_oop: true }).collect();
    let block0 = IrBlock {
        id: BlockId(0),
        bci: 0,
        code: vec![
            Ir::Param {
                dst: VReg(0),
                index: 0,
            },
            Ir::CallSend {
                dst: VReg(1),
                site: 0,
                args: vec![VReg(0)],
            },
            Ir::Ret { val: VReg(1) },
        ],
        entry_stack: Vec::new(),
        deopt_sites: Vec::new(),
    };
    let caller_method = IrMethod {
        blocks: vec![block0],
        vregs,
        pool: Vec::new(),
        argc: 1,
        ntemps: 0,
        ctx_vregs: Vec::new(),
        safepoints: Vec::new(),
        true_lit: PoolLit(0),
        false_lit: PoolLit(0),
        nil_lit: PoolLit(0),
        mark_slots_lit: PoolLit(0),
        call_sites: vec![CallSiteInfo {
            selector: foo_sel,
            argc: 1,
            static_klass: None,
        }],
        site_feedback: Vec::new(),
        inline_deps: Vec::new(),
        method_pool_ix: None,
    };
    let ra = regalloc::regalloc(&caller_method);
    let mut asm = JasmAssembler::new();
    let (blob, _pcs, _verified_entry_off, emitted_ic_sites, _safepoints) = emit::emit(
        &mut asm,
        &caller_method,
        &ra,
        vm.stubs.stub_poll_addr(),
        vm.stubs.must_be_boolean_addr(),
        vm.stubs.alloc_slow_addr(),
        None,
    );
    assert_eq!(emitted_ic_sites.len(), 1);

    let h = vm.code_cache.alloc(blob.code.len()).unwrap();
    vm.code_cache.publish(h, &blob);
    let resolve_addr = vm.stubs.resolve_addr();
    for site in &emitted_ic_sites {
        vm.code_cache.patch_branch26_at(h, site.off, resolve_addr);
    }
    let caller_probe_sel = vm.universe.intern(b"s11LatticeCallerProbe");
    let ic_sites: Vec<IcSite> = emitted_ic_sites
        .iter()
        .map(|s| IcSite {
            off: s.off,
            selector: s.selector,
            argc: s.argc,
            state: IcState::Unresolved,
            super_klass: None,
        })
        .collect();
    let caller_nm = Nmethod {
        id: NmethodId(0),
        key_klass: klasses[0],
        key_selector: caller_probe_sel,
        code: h,
        entry_off: 0,
        verified_entry_off: 0,
        state: NmState::Alive,
        level: 1,
        version: 0,
        literal_off: blob.literal_off,
        relocs: blob.relocs.clone(),
        frame_slots: ra.frame_slots,
        slot_is_oop: ra.slot_is_oop.clone(),
        pcdescs: Vec::new(),
        oopmaps: Vec::new(),
        ic_sites,
        poll_bci: None,
        deopt_scopes: Vec::new(),
        deopt_pcdescs: Vec::new(),
        inline_deps: Vec::new(),
    };
    let caller_id = vm.code_table.install(caller_nm);
    let caller_entry = h.base as u64;

    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;

    let dispatch = |vm_ptr: *mut VmState, klass: KlassOop| -> u64 {
        let recv = alloc::alloc_slots(unsafe { &mut *vm_ptr }, klass).oop();
        let argv = [recv.raw()];
        unsafe { call(caller_entry, vm_ptr, argv.as_ptr(), 1) }
    };

    // Unresolved -> Mono.
    let r0 = dispatch(vm_ptr, klasses[0]);
    assert_eq!(r0, SmallInt::new(100).oop().raw());
    match vm.code_table.get(caller_id).unwrap().ic_sites[0].state {
        IcState::Mono { klass, .. } => assert_eq!(klass, klasses[0]),
        other => panic!("expected Mono after the 1st dispatch, got {other:?}"),
    }

    // Mono -> Pic{2}, then Pic{2} -> Pic{3} -> ... -> Pic{PIC_MAX_ENTRIES}.
    for (i, &klass) in klasses.iter().enumerate().take(PIC_MAX_ENTRIES).skip(1) {
        let r = dispatch(vm_ptr, klass);
        assert_eq!(r, SmallInt::new(((i + 1) * 100) as i64).oop().raw());
        match vm.code_table.get(caller_id).unwrap().ic_sites[0].state {
            IcState::Pic { stub } => {
                assert_eq!(
                    vm.pic_table.pairs_of(stub).len(),
                    i + 1,
                    "after {} distinct klasses, the PIC must carry exactly that many pairs",
                    i + 1
                );
            }
            other => panic!("expected Pic after dispatch #{}, got {other:?}", i + 1),
        }
    }

    // Precisely verify `resolve_target_entry`'s own `use_verified` choice
    // (D4.3: "PIC targets that are nmethod entries use verified_entry") --
    // a target using `entry_off` instead would STILL dispatch correctly
    // (the target's own guard would just re-verify and match), so nothing
    // above this point would have caught a bug swapping the two. Checked
    // by directly comparing each recorded pair's own target address
    // against `code.base + verified_entry_off` for its klass's compiled
    // method.
    match vm.code_table.get(caller_id).unwrap().ic_sites[0].state {
        IcState::Pic { stub } => {
            for (k, t) in vm.pic_table.pairs_of(stub) {
                let nm_id = vm
                    .code_table
                    .lookup(*k, foo_sel)
                    .expect("every klass in the PIC must have a real compiled nmethod");
                let nm = vm.code_table.get(nm_id).unwrap();
                let expected = nm.code.base as u64 + nm.verified_entry_off as u64;
                assert_eq!(
                    *t, expected,
                    "PIC pair for {k:?} must use verified_entry, not entry, as its target"
                );
                assert_ne!(
                    nm.verified_entry_off, nm.entry_off,
                    "this check is only meaningful if verified_entry_off and entry_off actually \
                     differ for this method -- they don't, so it can't distinguish anything"
                );
            }
        }
        other => panic!("expected still-Pic just before the Mega promotion, got {other:?}"),
    }

    // Pic{PIC_MAX_ENTRIES} -> Mega on the (PIC_MAX_ENTRIES+1)-th distinct
    // klass.
    let last = PIC_MAX_ENTRIES;
    let r_last = dispatch(vm_ptr, klasses[last]);
    assert_eq!(r_last, SmallInt::new(((last + 1) * 100) as i64).oop().raw());
    match vm.code_table.get(caller_id).unwrap().ic_sites[0].state {
        IcState::Mega { .. } => {}
        other => panic!(
            "expected Mega after the {}th distinct klass, got {other:?}",
            last + 1
        ),
    }

    // Re-dispatching to the FIRST klass through the now-Mega site must
    // still work (rt_mega_lookup re-resolves fresh every time) and must
    // NOT regress the state back out of Mega.
    let r_again = dispatch(vm_ptr, klasses[0]);
    assert_eq!(r_again, SmallInt::new(100).oop().raw());
    match vm.code_table.get(caller_id).unwrap().ic_sites[0].state {
        IcState::Mega { .. } => {}
        other => panic!("Mega must never regress, got {other:?}"),
    }
    // And a middle one, for good measure.
    let r_mid = dispatch(vm_ptr, klasses[2]);
    assert_eq!(r_mid, SmallInt::new(300).oop().raw());
}

/// S11 step 6's own DNU target: a compiled call site sending a selector
/// NOTHING implements must reach a real `#doesNotUnderstand:` -- not
/// crash, not silently return garbage, not terminate the process. Installs
/// a stub `#doesNotUnderstand:` on `Object` returning a known sentinel
/// (mirrors `interpreter::ic`'s own `install_stub_dnu` test helper --
/// the established, safe way to make DNU observable instead of letting
/// `runtime::error::dnu_fallback`'s real default, `std::process::exit`,
/// actually fire). Also confirms a DNU miss leaves the site's own
/// `IcState` untouched (`Unresolved`) -- `rt_dnu` is reached without
/// `rt_resolve_send` ever patching or recording anything, since a LATER
/// call through the same site, with a different receiver klass, might
/// still resolve successfully.
#[test]
fn dnu_from_compiled_code_reaches_does_not_understand() {
    let mut vm = test_vm();
    let target_klass = vm.universe.new_klass(
        vm.universe.object_klass,
        "S11DnuTarget",
        Format::Slots,
        false,
        HEADER_WORDS,
    );
    let unknown_sel = vm.universe.intern(b"totallyUndefinedSelector");

    let object_klass = vm.universe.object_klass;
    let dnu_sel = vm.universe.sel_does_not_understand;
    let mut db = BytecodeBuilder::new();
    db.push_smi_i8(-1);
    db.ret_tos();
    let dnu_handler = db.finish(&mut vm, dnu_sel, 1, 0);
    install_method(&mut vm, object_klass, dnu_sel, dnu_handler);

    // Caller: one param (the target receiver), one send of a selector
    // nothing anywhere implements -- must genuinely miss and reach rt_dnu.
    let vregs: Vec<VRegInfo> = (0..2).map(|_| VRegInfo { is_oop: true }).collect();
    let block0 = IrBlock {
        id: BlockId(0),
        bci: 0,
        code: vec![
            Ir::Param {
                dst: VReg(0),
                index: 0,
            },
            Ir::CallSend {
                dst: VReg(1),
                site: 0,
                args: vec![VReg(0)],
            },
            Ir::Ret { val: VReg(1) },
        ],
        entry_stack: Vec::new(),
        deopt_sites: Vec::new(),
    };
    let caller_method = IrMethod {
        blocks: vec![block0],
        vregs,
        pool: Vec::new(),
        argc: 1,
        ntemps: 0,
        ctx_vregs: Vec::new(),
        safepoints: Vec::new(),
        true_lit: PoolLit(0),
        false_lit: PoolLit(0),
        nil_lit: PoolLit(0),
        mark_slots_lit: PoolLit(0),
        call_sites: vec![CallSiteInfo {
            selector: unknown_sel,
            argc: 1,
            static_klass: None,
        }],
        site_feedback: Vec::new(),
        inline_deps: Vec::new(),
        method_pool_ix: None,
    };
    let ra = regalloc::regalloc(&caller_method);
    let mut asm = JasmAssembler::new();
    let (blob, _pcs, _verified_entry_off, emitted_ic_sites, _safepoints) = emit::emit(
        &mut asm,
        &caller_method,
        &ra,
        vm.stubs.stub_poll_addr(),
        vm.stubs.must_be_boolean_addr(),
        vm.stubs.alloc_slow_addr(),
        None,
    );
    assert_eq!(emitted_ic_sites.len(), 1);

    let h = vm.code_cache.alloc(blob.code.len()).unwrap();
    vm.code_cache.publish(h, &blob);
    let resolve_addr = vm.stubs.resolve_addr();
    for site in &emitted_ic_sites {
        vm.code_cache.patch_branch26_at(h, site.off, resolve_addr);
    }
    let caller_probe_sel = vm.universe.intern(b"s11DnuCallerProbe");
    let ic_sites: Vec<IcSite> = emitted_ic_sites
        .iter()
        .map(|s| IcSite {
            off: s.off,
            selector: s.selector,
            argc: s.argc,
            state: IcState::Unresolved,
            super_klass: None,
        })
        .collect();
    let caller_nm = Nmethod {
        id: NmethodId(0),
        key_klass: target_klass,
        key_selector: caller_probe_sel,
        code: h,
        entry_off: 0,
        verified_entry_off: 0,
        state: NmState::Alive,
        level: 1,
        version: 0,
        literal_off: blob.literal_off,
        relocs: blob.relocs.clone(),
        frame_slots: ra.frame_slots,
        slot_is_oop: ra.slot_is_oop.clone(),
        pcdescs: Vec::new(),
        oopmaps: Vec::new(),
        ic_sites,
        poll_bci: None,
        deopt_scopes: Vec::new(),
        deopt_pcdescs: Vec::new(),
        inline_deps: Vec::new(),
    };
    let caller_id = vm.code_table.install(caller_nm);
    let caller_entry = h.base as u64;

    let receiver = alloc::alloc_slots(&mut vm, target_klass).oop();
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let argv = [receiver.raw()];

    let result = unsafe { call(caller_entry, vm_ptr, argv.as_ptr(), 1) };
    assert_eq!(
        result,
        SmallInt::new(-1).oop().raw(),
        "must reach the installed #doesNotUnderstand: stub"
    );
    match vm.code_table.get(caller_id).unwrap().ic_sites[0].state {
        IcState::Unresolved => {}
        other => panic!("a DNU miss must not patch or resolve the site, got {other:?}"),
    }

    // Repeatable: a second dispatch through the SAME still-Unresolved site
    // must reach DNU again, not crash or behave differently.
    let result2 = unsafe { call(caller_entry, vm_ptr, argv.as_ptr(), 1) };
    assert_eq!(result2, SmallInt::new(-1).oop().raw());
    match vm.code_table.get(caller_id).unwrap().ic_sites[0].state {
        IcState::Unresolved => {}
        other => panic!("a second DNU miss must also leave the site Unresolved, got {other:?}"),
    }
}

/// S11 step 6's own explicit `send_super` target -- the FIRST test all
/// S11 (steps 2-6) to compile a `send_super` through the REAL front door
/// (real bytecode, `driver::compile_method`'s own eligibility+convert+
/// emit pipeline, not a hand-built `Ir::CallSend` the way every other
/// S11 test in this file has had to use, since S10's `convert()` never
/// constructed ANY other kind of send until this step's own D4.6).
///
/// Three-klass hierarchy, `foo` overridden at every level (`Root`=100,
/// `Mid`=200, `Leaf`=300) plus `Leaf>>callSuperFoo` doing `^super foo`.
/// An ORDINARY send of `foo` to a `Leaf` instance would resolve to
/// `Leaf`'s own override (300, via normal inheritance) -- the only way
/// `callSuperFoo` returning 200 (`Mid`'s own, skipping `Leaf`'s own
/// override) can be explained is genuine super dispatch, starting the
/// lookup from `Leaf`'s own superclass (`Mid`) rather than `Leaf` itself.
///
/// `Mid>>foo` is compiled FIRST (so the super site's own compile-time
/// resolution finds a real nmethod, not a c2i adapter -- `resolve_target
/// _entry`'s OTHER branch already has its own coverage from steps 3-5).
/// Checks the site is `Mono` immediately after compiling `callSuperFoo`,
/// BEFORE it's ever even called once -- the whole point of D4.6 is
/// resolving at compile time, not on first runtime miss like every
/// other site.
#[test]
fn send_super_resolves_at_compile_time_and_dispatches() {
    let mut vm = test_vm();
    let foo_sel = vm.universe.intern(b"foo");

    let root_klass = vm.universe.new_klass(
        vm.universe.object_klass,
        "S11SuperRoot",
        Format::Slots,
        false,
        HEADER_WORDS,
    );
    let mid_klass = vm.universe.new_klass(
        root_klass,
        "S11SuperMid",
        Format::Slots,
        false,
        HEADER_WORDS,
    );
    let leaf_klass = vm.universe.new_klass(
        mid_klass,
        "S11SuperLeaf",
        Format::Slots,
        false,
        HEADER_WORDS,
    );

    let make_foo = |vm: &mut VmState, n: i64| -> MethodOop {
        let mut b = BytecodeBuilder::new();
        b.push_literal(vm, SmallInt::new(n).oop());
        b.ret_tos();
        b.finish(vm, foo_sel, 0, 0)
    };
    let root_foo = make_foo(&mut vm, 100);
    install_method(&mut vm, root_klass, foo_sel, root_foo);
    let mid_foo = make_foo(&mut vm, 200);
    install_method(&mut vm, mid_klass, foo_sel, mid_foo);
    let leaf_foo = make_foo(&mut vm, 300);
    install_method(&mut vm, leaf_klass, foo_sel, leaf_foo);

    // Compile Mid>>foo FIRST, so callSuperFoo's own compile-time
    // resolution finds a real nmethod entry, not a c2i adapter.
    assert!(driver::eligible(&vm, mid_foo));
    driver::compile_method(&mut vm, mid_klass, mid_foo).expect("Mid>>foo must compile");

    let call_super_foo_sel = vm.universe.intern(b"callSuperFoo");
    let mut b = BytecodeBuilder::new();
    b.push_self();
    b.send_super(&mut vm, foo_sel, 0);
    b.ret_tos();
    let call_super_foo = b.finish(&mut vm, call_super_foo_sel, 0, 0);
    install_method(&mut vm, leaf_klass, call_super_foo_sel, call_super_foo);
    assert!(
        driver::eligible(&vm, call_super_foo),
        "a super send must be unconditionally eligible (D4.6)"
    );

    let id = driver::compile_method(&mut vm, leaf_klass, call_super_foo)
        .expect("callSuperFoo must compile");
    let nm = vm
        .code_table
        .get(id)
        .expect("installed nmethod must be gettable");
    assert_eq!(
        nm.ic_sites.len(),
        1,
        "exactly one send (the super send) in this method"
    );
    match nm.ic_sites[0].state {
        IcState::Mono { klass, .. } => {
            assert_eq!(
                klass, mid_klass,
                "the super site's own compile-time-resolved klass must be Leaf's superclass, \
                 Mid -- not Leaf itself and not Root"
            );
        }
        other => panic!(
            "a send_super site must be Mono IMMEDIATELY after compiling, before ever being \
             called -- got {other:?}"
        ),
    }
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;

    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let receiver = alloc::alloc_slots(unsafe { &mut *vm_ptr }, leaf_klass).oop();
    let argv = [receiver.raw()];
    let result = unsafe { call(entry, vm_ptr, argv.as_ptr(), 1) };
    assert_eq!(
        result,
        SmallInt::new(200).oop().raw(),
        "super foo from a method on Leaf must reach Mid's own foo (200) -- not Leaf's own \
         override (300, what an ORDINARY send would reach) and not Root's (100)"
    );
}

/// tests_s11.md's `card_dirtied_by_compiled_store`: a compiled
/// `store_instvar_pop` storing a YOUNG value into an OLD receiver dirties
/// exactly the receiver's own card -- the full pipeline (decode -> convert
/// -> regalloc -> emit -> publish -> run), not just a listing check
/// (`emit::tests::barrier_emitted_conditions` already covers that half).
#[test]
fn card_dirtied_by_compiled_store() {
    let mut vm = test_vm();
    let recv_klass = vm.universe.new_klass(
        vm.universe.object_klass,
        "S11StoreBarrierRecv",
        Format::Slots,
        false,
        HEADER_WORDS + 1,
    );
    let set_field_sel = vm.universe.intern(b"setField:");
    let mut b = BytecodeBuilder::new();
    b.push_self();
    b.push_temp(0);
    b.store_instvar_pop(0);
    b.ret_self();
    let set_field = b.finish(&mut vm, set_field_sel, 1, 0);
    install_method(&mut vm, recv_klass, set_field_sel, set_field);

    assert!(
        driver::eligible(&vm, set_field),
        "an instvar-store method must be eligible from S11 step 7 on"
    );
    let id =
        driver::compile_method(&mut vm, recv_klass, set_field).expect("setField: must compile");
    let nm = vm
        .code_table
        .get(id)
        .expect("installed nmethod must be gettable");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;

    // Promote `recv` into old gen (tests/it_gc_full.rs's own established
    // "threshold=0, one scavenge" idiom).
    let recv = alloc::alloc_slots(&mut vm, recv_klass).oop();
    vm.stack.push(recv);
    vm.universe.tenuring_threshold = 0;
    scavenge(&mut vm).expect("scavenge must promote recv into old gen");
    let recv = vm.stack.get(vm.stack.sp - 1); // post-scavenge address
    assert!(
        vm.universe.layout.is_old(recv.mem_addr()),
        "recv must actually be in old gen for this test to mean anything"
    );

    // A fresh YOUNG value -- reset the tenuring threshold back up first,
    // so an allocation-triggered scavenge (MACVM_GC_STRESS=1) doesn't
    // immediately re-promote this one too.
    vm.universe.tenuring_threshold = 127;
    let array_klass = vm.universe.array_klass;
    let young_val = alloc::alloc_indexable_oops(&mut vm, array_klass, 0).oop();
    assert!(
        vm.universe.layout.is_new(young_val.mem_addr()),
        "young_val must actually be young for this test to mean anything"
    );

    let slot_addr = recv.mem_addr() + macvm::oops::layout::BODY_OFFSET;
    let card = vm.universe.cards.card_index(slot_addr);
    // `scavenge`'s own promotion just dirtied recv's WHOLE card
    // unconditionally (`record_multistores`, SPEC §7.3 step 2 -- a
    // promoted object's body may reference young survivors regardless of
    // its actual field contents) -- clean it explicitly first, exactly
    // matching what a later real card-scan would have done, so this test
    // isolates the COMPILED STORE's own dirtying from that unrelated,
    // already-covered promotion behavior (`tests_s07.md`'s own card
    // tests).
    vm.universe.cards.set_clean(card);
    assert!(!vm.universe.cards.is_dirty(card), "card must start clean");

    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let argv = [recv.raw(), young_val.raw()];
    unsafe { call(entry, vm_ptr, argv.as_ptr(), 2) };

    assert!(
        vm.universe.cards.is_dirty(card),
        "a compiled store_instvar_pop of a young value into an old receiver \
         must dirty the receiver's own card"
    );
}

/// tests_s11.md integration item 4, REWRITTEN by S12 step 7 (the D8
/// bridge is gone): `X basicNew` where `X` is a compile-time Slots class
/// constant compiles to an inline `Ir::Alloc`. Exercises BOTH edges:
/// (a) the eden fast path — compiled code bumping the ONE live
/// `universe.eden.top` word through `reg_block.eden_top_addr`, visible to
/// Rust with no adopt step; (b) the forced-overflow slow path
/// (`rt_alloc_slow` → the ordinary alloc cascade), which now runs a REAL
/// scavenge under the live compiled frame (`gc_under_compiled` bumps —
/// S12 P10's inversion of this test's original `== 0` assert) and then
/// allocates in the freshly-emptied EDEN, not old gen (the bridge's
/// old-direct diversion is deleted). Both edges must produce a
/// correctly-classed, nil-bodied instance.
#[test]
fn allocation_fast_and_slow() {
    use macvm::interpreter::compiled_call::{enter_compiled, EnterResult};
    use macvm::runtime::lookup::klass_of;

    // A DELIBERATELY TINY eden (32 KiB): the slow-path leg below fills eden
    // honestly with real walkable objects (the old `eden.top = eden.end`
    // lie can't survive a scavenge now — S12 step 7), and under debug's
    // always-on scavenge-entry verify a default multi-MiB eden turns that
    // honest fill into minutes of full-heap verify walks. 32 KiB overflows
    // in a handful of allocations while still exercising the exact same
    // slow edge.
    let mut vm = VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: Default::default(),
        gc_stress: false,
        gc_stress_full_period: None,
        eden_kb: Some(32),
        jit: JitMode::Off,
    });
    // A 2-instance-var Slots class bound to a global (`AllocTarget`).
    for item in parser::parse_file("Object subclass: AllocTarget [ | a b | ]").expect("parse") {
        classdef::execute_top_item(&mut vm, item).expect("execute");
    }
    // Tenure the class/metaclass/global NOW (S12 step 7: the slow-path leg
    // below runs a REAL scavenge that would otherwise relocate a young
    // AllocTarget klass, leaving every Rust-local KlassOop derived from it
    // stale). Old gen is never touched by a scavenge, so deriving
    // target_klass AFTER this keeps it valid through the slow-path
    // collection -- the same "look up the live post-GC value, not a stale
    // pre-GC local" idiom it_gc_jit's own alloc tests use.
    vm.universe.tenuring_threshold = 0;
    scavenge(&mut vm).expect("tenuring scavenge");
    vm.universe.tenuring_threshold = 127;
    let target_sym = vm.universe.intern(b"AllocTarget");
    let target_assoc =
        macvm::runtime::globals::global_lookup(&vm, target_sym).expect("AllocTarget global");
    let target_klass =
        KlassOop::try_from(MemOop::try_from(target_assoc).unwrap().body_oop(1)).unwrap();

    // A bare `test_vm()` has no world loaded, so `basicNew` (a world method)
    // isn't installed. Install the real basicNew primitive (id 23) on
    // AllocTarget's own metaclass so `AllocTarget basicNew` resolves.
    let basic_new_sel = vm.universe.intern(b"basicNew");
    let target_meta = klass_of(&vm, target_klass.oop());
    let basic_new_method = {
        let mut nb = BytecodeBuilder::new();
        nb.ret_self(); // fallback body -- never reached (prim always succeeds here)
        let m = nb.finish(&mut vm, basic_new_sel, 0, 0);
        m.set_primitive(23);
        m.set_flags(0, 0, false, false, true, false, 0);
        m
    };
    install_method(&mut vm, target_meta, basic_new_sel, basic_new_method);

    // `spawn [ ^AllocTarget basicNew ]` -- push_global AllocTarget; send
    // basicNew; ret_tos. Self is ignored, so it can compile for smi_klass.
    let mut b = BytecodeBuilder::new();
    b.push_global(&mut vm, target_assoc);
    b.send(&mut vm, basic_new_sel, 0);
    b.ret_tos();
    let spawn_sel = vm.universe.intern(b"spawn");
    let method = b.finish(&mut vm, spawn_sel, 0, 0);

    // Warm interpreted: mono the basicNew site (guard = AllocTarget's
    // metaclass, target = Object>>basicNew, prim 23). Receiver is a smi
    // (ignored by the body; matches the compile target's own smi_klass).
    let smi_klass = vm.universe.smi_klass;
    let recv = SmallInt::new(1).oop();
    let warm = macvm::interpreter::run_method(&mut vm, method, recv, &[]);
    assert_eq!(
        klass_of(&vm, warm).oop().raw(),
        target_klass.oop().raw(),
        "warmup must produce a real AllocTarget"
    );

    // The detection must fire: an inline Ir::Alloc, not a generic CallSend.
    let cfg = decode::decode(method);
    let ir_method = ir::convert(&vm, method, &cfg);
    assert!(
        ir_method
            .blocks
            .iter()
            .any(|bl| bl.code.iter().any(|i| matches!(i, Ir::Alloc { .. }))),
        "`AllocTarget basicNew` must compile to an inline Ir::Alloc"
    );

    assert!(
        driver::eligible(&vm, method),
        "a mono basicNew site is eligible"
    );
    let id = driver::compile_method(&mut vm, smi_klass, method).expect("must compile");

    // (a) Fast path: compiled code bumps the ONE live eden.top word
    // (through reg_block.eden_top_addr) — Rust sees it immediately, no
    // adopt step.
    let eden_top_before = vm.universe.eden.top;
    vm.stack.push(recv);
    assert_eq!(enter_compiled(&mut vm, id, 0), EnterResult::Completed);
    let obj = vm.stack.pop();
    assert_eq!(
        klass_of(&vm, obj).oop().raw(),
        target_klass.oop().raw(),
        "fast-path result must be a fresh AllocTarget"
    );
    assert!(
        vm.universe.eden.top > eden_top_before,
        "the fast path's bump-through-the-pointer must be immediately visible in eden.top"
    );
    assert!(
        obj.mem_addr() >= vm.universe.eden.start && obj.mem_addr() < vm.universe.eden.end,
        "the fast-path object must live in eden"
    );

    // (b) Slow path: fill eden HONESTLY (real, walkable objects — the old
    // `eden.top = eden.end` lie would leave an uninitialized gap the
    // slow path's own scavenge-entry verify walk now trips over) so the
    // inline bump overflows -> rt_alloc_slow -> the ordinary alloc
    // cascade, which runs a REAL scavenge UNDER the live compiled frame
    // and then allocates in the freshly-emptied eden.
    // Tail-fill with real AllocTarget instances until less than one more
    // fits — a 32 KiB eden makes this ~a thousand tiny allocations at
    // most, sub-millisecond, no GC (gc_stress off) until the compiled
    // slow edge itself forces one.
    let need = target_klass.non_indexable_size() * macvm::oops::layout::WORD_SIZE;
    while vm.universe.eden.end - vm.universe.eden.top >= need {
        alloc::alloc_slots(&mut vm, target_klass);
    }
    let gc_under_before = vm.universe.gc_stats.gc_under_compiled;
    let scav_before = vm.universe.gc_stats.scavenge_count;
    vm.stack.push(recv);
    assert_eq!(enter_compiled(&mut vm, id, 0), EnterResult::Completed);
    let obj2 = vm.stack.pop();
    assert_eq!(
        klass_of(&vm, obj2).oop().raw(),
        target_klass.oop().raw(),
        "slow-path result must still be a valid AllocTarget"
    );
    assert!(
        vm.universe.gc_stats.scavenge_count > scav_before,
        "the forced-overflow slow path must have scavenged (no old-direct diversion exists)"
    );
    assert!(
        vm.universe.gc_stats.gc_under_compiled > gc_under_before,
        "S12 P10 (inverts this test's S11-era `== 0` assert): the scavenge must have run \
         UNDER the live compiled frame -- the hard case genuinely executes now"
    );
    assert!(
        obj2.mem_addr() >= vm.universe.eden.start && obj2.mem_addr() < vm.universe.eden.end,
        "the slow-path object must land in the freshly-scavenged EDEN, not old gen"
    );
}

/// A bare `test_vm()` has no world loaded, so the block/arith primitives
/// the NLR scenarios need (`value`/`ensure:` on BlockClosure, `+` on
/// SmallInteger) aren't installed — install a real primitive-backed method
/// by pinned id, mirroring `primitive_stub` but with the right argc per
/// selector.
fn install_prim(vm: &mut VmState, klass: KlassOop, name: &[u8], argc: usize, prim_id: i64) {
    let sel = vm.universe.intern(name);
    let mut b = BytecodeBuilder::new();
    b.ret_self();
    let m = b.finish(vm, sel, argc, 0);
    m.set_primitive(prim_id);
    m.set_flags(argc, 0, false, false, true, false, 0);
    install_method(vm, klass, sel, m);
}

/// tests_s11.md integration item 3, `nlr_through_compiled_frame` (S11 D6.3,
/// as CORRECTED by this step — see the sprint doc's D6.3 SPEC-QUESTION):
/// interpreted `outer` (the block's home, permanently ineligible via its
/// `push_closure`) calls compiled `mid:` (a single super send —
/// unconditionally eligible, D4.6, and under step 7's conservative gate the
/// ONE production shape that gives a compiled frame an interpreted, c2i-
/// reached callee), which reaches interpreted `NlrBase>>inner:` via a c2i
/// adapter; `inner:` runs the block, which NLRs to `outer`. The escape must
/// cross BOTH the c2i boundary (interpreter-side escape, `vm.nlr_state`
/// parked) and the compiled frame (the send-site `sub/cbz` check routing
/// the sentinel through `mid:`'s own epilogue), then resume in
/// `enter_compiled` and deliver at home. Asserts: the NLR value (42, NOT
/// 1042 — the post-NLR tail of `outer` must never run), `compiled_depth`
/// back to 0, `nlr_state` fully consumed, and that `mid:` really was
/// compiled (so the test can't silently pass all-interpreted).
#[test]
fn nlr_through_compiled_frame() {
    let mut vm = test_vm();
    vm.options.jit = JitMode::Threshold(1);
    let closure_klass = vm.universe.closure_klass;
    install_prim(&mut vm, closure_klass, b"value", 0, 50);
    load_source(
        &mut vm,
        "Object subclass: NlrBase [\n\
        \x20   inner: aBlock [ ^aBlock value ]\n\
         ]\n\
         NlrBase subclass: NlrProbe [\n\
        \x20   mid: aBlock [ ^super inner: aBlock ]\n\
        \x20   outer [ | r | r := self mid: [ ^42 ]. ^r + 1000 ]\n\
         ]\n",
    );
    let probe_klass = klass_named(&mut vm, "NlrProbe");
    let outer = method_named(&mut vm, probe_klass, "outer");
    let recv = alloc::alloc_slots(&mut vm, probe_klass).oop();

    // Twice: the first call compiles mid: (threshold=1, super-send sites
    // need no IC warmup) and already takes the full mixed-tier NLR path;
    // the second re-enters through the now-warm mono-compiled IC.
    for pass in 0..2 {
        let result = macvm::interpreter::run_method(&mut vm, outer, recv, &[]);
        assert_eq!(
            result,
            SmallInt::new(42).oop(),
            "pass {pass}: the NLR must deliver 42 at home (1042 would mean \
             outer's post-NLR tail ran)"
        );
        assert_eq!(
            vm.compiled_depth, 0,
            "pass {pass}: every compiled frame the NLR crossed must have been \
             unwound through enter_compiled's own depth bookkeeping"
        );
        assert!(
            vm.nlr_state.is_none(),
            "pass {pass}: the in-flight NLR state must be fully consumed"
        );
    }

    let mid_sel = vm.universe.intern(b"mid:");
    assert!(
        vm.code_table.lookup(probe_klass, mid_sel).is_some(),
        "mid: must actually have compiled -- otherwise this test silently \
         degraded to the pure-interpreter NLR path"
    );
}

/// The `ensure:`-straddling variant (adversarial-review HOLE D territory):
/// an `ensure:` armed on the HOME side of the compiled frame must run
/// exactly once when the NLR unwinds across it — on the resume side, after
/// the sentinel bounce, via the ordinary marked-frame walk.
#[test]
fn nlr_through_compiled_frame_runs_home_side_ensure() {
    let mut vm = test_vm();
    vm.options.jit = JitMode::Threshold(1);
    let closure_klass = vm.universe.closure_klass;
    install_prim(&mut vm, closure_klass, b"value", 0, 50);
    install_prim(&mut vm, closure_klass, b"ensure:", 1, 60);
    let smi_klass = vm.universe.smi_klass;
    install_prim(&mut vm, smi_klass, b"+", 1, 1);
    load_source(
        &mut vm,
        "Object subclass: NlrEnsBase [\n\
        \x20   inner: aBlock [ ^aBlock value ]\n\
         ]\n\
         NlrEnsBase subclass: NlrEnsProbe [\n\
        \x20   | tally |\n\
        \x20   setUp [ tally := 0 ]\n\
        \x20   tally [ ^tally ]\n\
        \x20   outerEnsured [\n\
        \x20       ^[ self mid: [ ^7 ] ] ensure: [ tally := tally + 1 ]\n\
        \x20   ]\n\
        \x20   mid: aBlock [ ^super inner: aBlock ]\n\
         ]\n",
    );
    let probe_klass = klass_named(&mut vm, "NlrEnsProbe");
    let set_up = method_named(&mut vm, probe_klass, "setUp");
    let outer_ensured = method_named(&mut vm, probe_klass, "outerEnsured");
    let tally = method_named(&mut vm, probe_klass, "tally");
    let recv = alloc::alloc_slots(&mut vm, probe_klass).oop();

    macvm::interpreter::run_method(&mut vm, set_up, recv, &[]);
    let result = macvm::interpreter::run_method(&mut vm, outer_ensured, recv, &[]);
    assert_eq!(
        result,
        SmallInt::new(7).oop(),
        "the NLR value must arrive at home through the ensure: interception"
    );
    let t = macvm::interpreter::run_method(&mut vm, tally, recv, &[]);
    assert_eq!(
        t,
        SmallInt::new(1).oop(),
        "the home-side ensure: handler must run exactly once during the \
         cross-tier unwind"
    );
    assert_eq!(vm.compiled_depth, 0);
    assert!(vm.nlr_state.is_none());
}

// ── S13 step 3b: deopt scope-desc vreg→ValueLoc resolution golden ─────────

/// Golden for `compiler::scopes::resolve_frame_loc` against a REAL
/// compiled method's regalloc output (not hand-faked intervals): the
/// load-bearing mapping the whole deopt-metadata recorder is built on.
/// `foo: a [ self bar. ^ self baz: a ]` — `self` (VReg 0) and the arg `a`
/// (VReg 1) are BOTH live across the first `self bar` send (each is used
/// again in `baz: a`), so S12's spill-all forces them to canonical frame
/// slots there; `resolve_frame_loc` must return exactly those
/// `FrameSlot`s, and a never-defined vreg must resolve to `Nil` (the dead/
/// absent case). Both sends are warmed to a NON-smi mono IC so they stay
/// generic `CallSend` safepoints (S14 step 3: an Untaken/empty IC would now
/// lower to an uncommon trap instead, a different scope shape).
#[test]
fn deopt_resolve_frame_loc_from_real_regalloc() {
    use macvm::compiler::scopes::{resolve_frame_loc, ValueLoc};

    let mut vm = test_vm();
    let bar_sel = vm.universe.intern(b"bar");
    let baz_sel = vm.universe.intern(b"baz:");
    let foo_sel = vm.universe.intern(b"foo:");

    let mut b = BytecodeBuilder::new();
    b.push_self();
    b.send(&mut vm, bar_sel, 0); // self bar
    b.pop(); // discard its result
    b.push_self();
    b.push_temp(0); // the arg `a`
    b.send(&mut vm, baz_sel, 1); // self baz: a
    b.ret_tos();
    let method = b.finish(&mut vm, foo_sel, 1, 0);

    // S14 step 3: warm both send sites to Mono on a NON-smi klass so they stay
    // real generic `CallSend`s (this golden targets the two call-return
    // safepoints). An Untaken IC would now lower each send to an uncommon trap.
    let obj_klass = vm.universe.object_klass;
    let bar_target = {
        let mut tb = BytecodeBuilder::new();
        tb.ret_self();
        tb.finish(&mut vm, bar_sel, 0, 0)
    };
    let baz_target = {
        let mut tb = BytecodeBuilder::new();
        tb.ret_self();
        tb.finish(&mut vm, baz_sel, 1, 0)
    };
    let epoch = vm.ic_epoch;
    InterpreterIc::at(method, 0).set_mono(&mut vm, obj_klass, bar_target, epoch);
    InterpreterIc::at(method, 1).set_mono(&mut vm, obj_klass, baz_target, epoch);

    let cfg = decode::decode(method);
    let ir = ir::convert(&vm, method, &cfg);
    let ra = regalloc::regalloc(&ir);

    assert!(
        ra.safepoint_positions.len() >= 2,
        "two generic sends must produce two safepoints: {:?}",
        ra.safepoint_positions
    );
    let p0 = ra.safepoint_positions[0]; // the `self bar` send

    // Derive the EXPECTED FrameSlot for VReg(0) directly from regalloc's
    // own assignment covering p0, then confirm resolve_frame_loc agrees --
    // proving it reads the real slot, not a coincidence.
    let self_iv = ra
        .intervals
        .iter()
        .find(|iv| iv.vreg == VReg(0) && iv.start <= p0 && iv.end > p0)
        .expect("self must be live across the first send");
    let expected_self = match self_iv.assignment {
        Some(macvm::compiler::regalloc::Assignment::Spill(slot)) => {
            ValueLoc::FrameSlot(-8 * (slot.0 as i32 + 1))
        }
        other => panic!("S12 spill-all: self must be SPILLED across a safepoint, got {other:?}"),
    };
    assert_eq!(resolve_frame_loc(VReg(0), p0, &ra.intervals), expected_self);

    // The arg `a` (VReg 1) is likewise live-across → a FrameSlot.
    assert!(
        matches!(
            resolve_frame_loc(VReg(1), p0, &ra.intervals),
            ValueLoc::FrameSlot(_)
        ),
        "the arg `a`, used again after the first send, must resolve to a frame slot"
    );

    // A vreg that doesn't exist (or is dead at p0) → Nil, the materialize-
    // nil case for a value never read after the resume bci.
    assert_eq!(
        resolve_frame_loc(VReg(9999), p0, &ra.intervals),
        ValueLoc::Nil
    );
}

// ─── S14 step 4b: leaf-method inlining ─────────────────────────────────────

/// Builds a klass with one instvar plus a leaf accessor `val [ ^instvar0 ]`
/// installed on it, and a caller `getVal: x [ ^x val ]` — customized for
/// `SmallInteger` (so the entry guard proves `self`, NOT `x`) — warmed mono to
/// that accessor on the ARGUMENT `x`. Sending to `x` (not `self`) is what makes
/// the inline guard's cold path genuinely reachable: the entry guard proves
/// nothing about `x`'s klass, so a wrong-klass `x` really does miss the inline
/// guard rather than the method-level entry guard. Returns
/// `(recv_klass, val_sel, caller_method)`.
fn inline_accessor_scenario(vm: &mut VmState) -> (KlassOop, SymbolOop, MethodOop) {
    let recv_klass = vm.universe.new_klass(
        vm.universe.object_klass,
        "S14InlineAccessor",
        Format::Slots,
        false,
        HEADER_WORDS + 1,
    );
    // `val [ ^instvar0 ]` — a leaf accessor (no send).
    let val_sel = vm.universe.intern(b"val");
    let val = {
        let mut vb = BytecodeBuilder::new();
        vb.push_instvar(0);
        vb.ret_tos();
        vb.finish(vm, val_sel, 0, 0)
    };
    install_method(vm, recv_klass, val_sel, val);

    // `getVal: x [ ^x val ]` — the send's receiver is the ARGUMENT, whose klass
    // the entry guard (which customizes on `self`) never constrains.
    let get_sel = vm.universe.intern(b"getVal:");
    let mut b = BytecodeBuilder::new();
    b.push_temp(0); // x
    b.send(vm, val_sel, 0);
    b.ret_tos();
    let caller = b.finish(vm, get_sel, 1, 0);

    // Warm the `x val` site to Mono on `recv_klass` (its real target).
    let epoch = vm.ic_epoch;
    InterpreterIc::at(caller, 0).set_mono(vm, recv_klass, val, epoch);
    (recv_klass, val_sel, caller)
}

/// S14 step 4b (a): the inlined-accessor DIFFERENTIAL. A caller `getVal: x [ ^x
/// val ]`, warmed mono to a leaf accessor `^instvar0`, compiles with the send
/// SPLICED INLINE (no `IcSite`). Running the compiled nmethod on an argument
/// whose instvar holds a discriminating value returns exactly that value — the
/// same value the pure interpreter (`run_method`) produces for the same send.
#[test]
fn compiled_inlined_accessor_matches_interpreter() {
    let mut vm = test_vm();
    let smi_klass = vm.universe.smi_klass;
    let (recv_klass, _val_sel, caller) = inline_accessor_scenario(&mut vm);

    // Compile the caller customized for SmallInteger (self is a smi; the inlined
    // send's receiver is the ARG `x`). Eligible because its one send is a mono
    // leaf accessor (S14 step 4b eligibility relaxation).
    assert!(
        driver::eligible(&vm, caller),
        "a mono leaf-accessor send must be eligible (it inlines)"
    );
    let id = driver::compile_method(&mut vm, smi_klass, caller).expect("must compile");

    // The send was inlined: NO IcSite for it, and the nmethod records one
    // inline dependency.
    {
        let nm = vm.code_table.get(id).expect("installed");
        assert!(
            nm.ic_sites.is_empty(),
            "the `x val` send was inlined → no compiled IC site"
        );
        assert_eq!(nm.inline_deps.len(), 1, "one inline dependency recorded");
        assert_eq!(nm.inline_deps[0].0.oop().raw(), recv_klass.oop().raw());
    }

    // An argument whose instvar0 holds a discriminating value (54321).
    let discriminating = SmallInt::new(54321).oop();
    let arg = alloc::alloc_slots(&mut vm, recv_klass).oop();
    MemOop::try_from(arg)
        .unwrap()
        .set_body_oop(0, discriminating);
    let self_smi = SmallInt::new(3).oop(); // the customization receiver (unused by the body)

    // Interpreter reference: `^x val` dispatches to `^instvar0` = 54321.
    let interp = macvm::interpreter::run_method(&mut vm, caller, self_smi, &[arg]);
    assert_eq!(
        interp.raw(),
        discriminating.raw(),
        "interpreter reference: `^x val` = the argument's instvar0"
    );

    // Compiled: the inlined accessor loads instvar0 off the (guarded) argument.
    let nm = vm.code_table.get(id).expect("installed");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let result = unsafe { call(entry, vm_ptr, [self_smi.raw(), arg.raw()].as_ptr(), 2) };
    assert_eq!(
        result,
        discriminating.raw(),
        "compiled inlined accessor must load the argument's instvar0 (differential match)"
    );
}

/// S14 step 4b (b): the guard COLD PATH. The SAME compiled nmethod (its `x val`
/// send inlined behind a guard speculating `x` is `recv_klass`) called with an
/// ARGUMENT of a DIFFERENT klass fails the inline guard, deopts (`brk` → SIGTRAP
/// → uncommon trampoline → re-execute the send generically in the interpreter)
/// and returns THAT klass's own `val` result — while `deopt_count` bumps (the
/// guard's brk actually fired). Sending to the ARG (not `self`) is essential:
/// the method's entry guard customizes on `self`, so it constrains `self`'s
/// klass but says nothing about `x`, leaving the inline guard's cold path
/// genuinely reachable (a `self`-receiver send would be caught by the redundant
/// entry guard first — static-klass guard elision is a later step).
#[test]
fn compiled_inlined_accessor_guard_cold_path_deopts() {
    // A JIT-armed VM so the SIGTRAP handler is live (the guard's cold path is a
    // real `brk #0xDE00`).
    let mut vm = VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: Default::default(),
        gc_stress: false,
        gc_stress_full_period: None,
        eden_kb: None,
        jit: JitMode::Threshold(1),
    });
    let smi_klass = vm.universe.smi_klass;
    let (_recv_klass, val_sel, caller) = inline_accessor_scenario(&mut vm);
    // Customize for SmallInteger (self is a smi); the inlined send's receiver is
    // the arg `x`, whose klass the entry guard never constrains.
    let id = driver::compile_method(&mut vm, smi_klass, caller).expect("must compile");

    // A SECOND klass, off `recv_klass`'s branch, with its OWN `val` returning a
    // DISTINCT value.
    let other_klass = vm.universe.new_klass(
        vm.universe.object_klass,
        "S14InlineOther",
        Format::Slots,
        false,
        HEADER_WORDS + 1,
    );
    let other_val = {
        let mut vb = BytecodeBuilder::new();
        vb.push_instvar(0);
        vb.ret_tos();
        vb.finish(&mut vm, val_sel, 0, 0)
    };
    install_method(&mut vm, other_klass, val_sel, other_val);

    // Argument of the OTHER klass, instvar0 = a discriminating value.
    let other_value = SmallInt::new(98765).oop();
    let other_arg = alloc::alloc_slots(&mut vm, other_klass).oop();
    MemOop::try_from(other_arg)
        .unwrap()
        .set_body_oop(0, other_value);
    let self_smi = SmallInt::new(3).oop();

    // Interpreter reference for the OTHER argument.
    let interp = macvm::interpreter::run_method(&mut vm, caller, self_smi, &[other_arg]);
    assert_eq!(interp.raw(), other_value.raw());

    let nm = vm.code_table.get(id).expect("installed");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;

    let deopts_before = unsafe { (*vm_ptr).stats.deopt_count };
    // The wrong-klass ARGUMENT fails the inline guard → cold trap → deopt →
    // re-execute `x val` generically in the interpreter → `other_klass`'s own
    // accessor → 98765.
    let result = unsafe { call(entry, vm_ptr, [self_smi.raw(), other_arg.raw()].as_ptr(), 2) };
    let deopts_after = unsafe { (*vm_ptr).stats.deopt_count };

    assert_eq!(
        result,
        other_value.raw(),
        "the guard cold path must deopt and return the OTHER klass's own val result"
    );
    assert_eq!(
        result,
        interp.raw(),
        "and match the pure-interpreter reference for the same send (differential)"
    );
    assert_eq!(
        deopts_after,
        deopts_before + 1,
        "exactly one deopt (the argument-klass guard's brk fired)"
    );
}

/// S14 step 4b (c): REDEFINITION invalidation. Redefining the INLINED callee
/// (`val` on the receiver klass — or an ancestor) makes the caller nmethod
/// `NotEntrant`, because its guard assumed `lookup(recv_klass, val)` == the
/// spliced accessor, an assumption a redefinition breaks.
#[test]
fn redefining_inlined_callee_invalidates_caller() {
    let mut vm = test_vm();
    let smi_klass = vm.universe.smi_klass;
    let (recv_klass, val_sel, caller) = inline_accessor_scenario(&mut vm);
    // Customized for SmallInteger; the inline dependency it records is
    // `(recv_klass, val)` — the accessor's own key, independent of the
    // customization klass.
    let id = driver::compile_method(&mut vm, smi_klass, caller).expect("must compile");
    assert!(
        matches!(vm.code_table.get(id).unwrap().state, NmState::Alive),
        "freshly compiled → Alive"
    );

    // Redefining an UNRELATED selector, or `val` on an off-chain class, must NOT
    // invalidate the caller.
    let unrelated = vm.universe.intern(b"unrelatedSel");
    let unrelated_body = trivial_method(&mut vm, unrelated);
    install_method(&mut vm, recv_klass, unrelated, unrelated_body);
    let off_chain = vm.universe.new_klass(
        vm.universe.object_klass,
        "S14InlineOffChain",
        Format::Slots,
        false,
        HEADER_WORDS + 1,
    );
    let off_body = trivial_method(&mut vm, val_sel);
    install_method(&mut vm, off_chain, val_sel, off_body);
    assert!(
        matches!(vm.code_table.get(id).unwrap().state, NmState::Alive),
        "unrelated selector / off-chain klass must NOT invalidate the inlining caller"
    );

    // Redefining the inlined callee itself (`val` on `recv_klass`) → NotEntrant.
    let new_val = {
        let mut vb = BytecodeBuilder::new();
        vb.push_instvar(0);
        vb.ret_tos();
        vb.finish(&mut vm, val_sel, 0, 0)
    };
    install_method(&mut vm, recv_klass, val_sel, new_val);
    assert!(
        matches!(vm.code_table.get(id).unwrap().state, NmState::NotEntrant),
        "redefining the inlined `val` must make the caller nmethod NotEntrant"
    );
}

// ─── S14 step 4c: non-leaf method inlining ─────────────────────────────────

/// Builds the non-leaf inline scenario:
///   `bar [ ^instvar0 ]`      — a leaf accessor on `recv_klass`,
///   `run [ ^self bar ]`      — a NON-leaf helper (its `self bar` send is the
///                              in-body safepoint that makes 4c's depth-2 deopt
///                              live), warmed mono to `bar`,
///   `outer: x [ ^x run ]`    — the caller; the `x run` send is warmed mono to
///                              `run` on the ARGUMENT `x` (so the inline guard's
///                              cold path is genuinely reachable — the entry
///                              guard customizes on `self`, not `x`).
/// `warm_bar` controls the helper's OWN `self bar` IC: `true` warms it mono (so
/// the inlined body's inner send becomes a real compiled `CallSend`), `false`
/// leaves it Empty (so the inlined body's inner send becomes a step-3 uncommon
/// TRAP — the in-body deopt trigger). Returns `(recv_klass, run_sel, bar_sel,
/// outer_method)`.
fn nonleaf_inline_scenario(
    vm: &mut VmState,
    warm_bar: bool,
) -> (KlassOop, SymbolOop, SymbolOop, MethodOop) {
    let recv_klass = vm.universe.new_klass(
        vm.universe.object_klass,
        "S14NonLeafRecv",
        Format::Slots,
        false,
        HEADER_WORDS + 1,
    );
    // `bar [ ^instvar0 ]` — a leaf accessor.
    let bar_sel = vm.universe.intern(b"bar");
    let bar = {
        let mut bb = BytecodeBuilder::new();
        bb.push_instvar(0);
        bb.ret_tos();
        bb.finish(vm, bar_sel, 0, 0)
    };
    install_method(vm, recv_klass, bar_sel, bar);

    // `run [ ^self bar ]` — a NON-leaf helper (one inner send).
    let run_sel = vm.universe.intern(b"run");
    let run = {
        let mut rb = BytecodeBuilder::new();
        rb.push_self();
        rb.send(vm, bar_sel, 0);
        rb.ret_tos();
        rb.finish(vm, run_sel, 0, 0)
    };
    install_method(vm, recv_klass, run_sel, run);
    if warm_bar {
        // Warm the helper's OWN `self bar` site to Mono on `recv_klass` so the
        // inlined body's inner send is a real compiled `CallSend`.
        let epoch = vm.ic_epoch;
        InterpreterIc::at(run, 0).set_mono(vm, recv_klass, bar, epoch);
    }

    // `outer: x [ ^x run ]` — the `x run` send is on the ARGUMENT.
    let outer_sel = vm.universe.intern(b"outer:");
    let mut ob = BytecodeBuilder::new();
    ob.push_temp(0); // x
    ob.send(vm, run_sel, 0);
    ob.ret_tos();
    let outer = ob.finish(vm, outer_sel, 1, 0);

    // Warm the `x run` site to Mono on `recv_klass` (its real target).
    let epoch = vm.ic_epoch;
    InterpreterIc::at(outer, 0).set_mono(vm, recv_klass, run, epoch);
    (recv_klass, run_sel, bar_sel, outer)
}

/// S14 step 4c (a): the NON-LEAF inlined DIFFERENTIAL. `outer: x [ ^x run ]`
/// with `run [ ^self bar ]` (a helper with a real inner send) warmed mono →
/// `run` is spliced inline, and its `self bar` send becomes a plain compiled
/// `CallSend` INSIDE the inlined body (recording a `SenderLink` deopt scope).
/// The compiled result equals the pure interpreter for a discriminating value.
#[test]
fn compiled_inlined_nonleaf_matches_interpreter() {
    let mut vm = test_vm();
    let smi_klass = vm.universe.smi_klass;
    let (recv_klass, _run_sel, _bar_sel, outer) = nonleaf_inline_scenario(&mut vm, true);

    // Compile `outer:` customized for SmallInteger (self is a smi; the inlined
    // send's receiver is the ARG `x`). Eligible: its one send is a mono callee
    // whose body is a single-block non-leaf (4c).
    assert!(
        driver::eligible(&vm, outer),
        "a mono non-leaf send must be eligible (it inlines)"
    );
    let id = driver::compile_method(&mut vm, smi_klass, outer).expect("must compile");

    // The `x run` send was inlined: it records NO IcSite of its own; the
    // inlined body's inner `self bar` send DID emit one real compiled IC site
    // (it dispatches). And the nmethod records the (recv_klass, run) inline dep.
    {
        let nm = vm.code_table.get(id).expect("installed");
        assert_eq!(
            nm.ic_sites.len(),
            1,
            "the inlined body's inner `self bar` send is one real compiled IC site \
             (the outer `x run` was inlined away)"
        );
        assert_eq!(
            nm.inline_deps.len(),
            1,
            "one inline dependency recorded (the inlined `run`)"
        );
        assert_eq!(nm.inline_deps[0].0.oop().raw(), recv_klass.oop().raw());
    }

    // An argument whose instvar0 holds a discriminating value (12321).
    let discriminating = SmallInt::new(12321).oop();
    let arg = alloc::alloc_slots(&mut vm, recv_klass).oop();
    MemOop::try_from(arg)
        .unwrap()
        .set_body_oop(0, discriminating);
    let self_smi = SmallInt::new(7).oop(); // the customization receiver (unused by the body)

    // Interpreter reference: `^x run` → `^self bar` (self=x) → `^instvar0`.
    let interp = macvm::interpreter::run_method(&mut vm, outer, self_smi, &[arg]);
    assert_eq!(
        interp.raw(),
        discriminating.raw(),
        "interpreter reference: `^x run` = the argument's instvar0"
    );

    // Compiled: guard `x`, splice `run`, dispatch `self bar` → the same value.
    let nm = vm.code_table.get(id).expect("installed");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let result = unsafe { call(entry, vm_ptr, [self_smi.raw(), arg.raw()].as_ptr(), 2) };
    assert_eq!(
        result,
        discriminating.raw(),
        "compiled inlined non-leaf must match the interpreter (differential)"
    );
}

/// S14 step 4c (b): THE CRUX — a deopt at a safepoint INSIDE the inlined body.
/// `outer: x [ ^x run ]` with `run [ ^self bar ]` where `run`'s OWN `self bar`
/// IC is left Empty (Untaken): when `run` is inlined into the compiled `outer`,
/// that inner send becomes a step-3 uncommon TRAP INSIDE the inlined body.
/// Calling the compiled `outer` hits the trap → `deoptimize_frame` must rebuild
/// BOTH interpreter frames (the inlined `run` frame AND the caller `outer`
/// frame) from the ONE physical compiled frame, following the `SenderLink`
/// chain → `interpret_active` resumes → re-executes `self bar` generically →
/// identical result to the pure interpreter, with `deopt_count` bumped. This is
/// the first time the depth-N materializer runs at depth 2.
#[test]
fn deopt_through_inlined_nonleaf_body_rebuilds_both_frames() {
    // A JIT-armed VM so the SIGTRAP handler is live (the in-body trap and the
    // guard cold path are real `brk`s).
    let mut vm = VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: Default::default(),
        gc_stress: false,
        gc_stress_full_period: None,
        eden_kb: None,
        jit: JitMode::Threshold(1),
    });
    let smi_klass = vm.universe.smi_klass;
    // warm_bar = false → the helper's inner `self bar` stays Untaken → an
    // in-body trap once inlined.
    let (recv_klass, _run_sel, _bar_sel, outer) = nonleaf_inline_scenario(&mut vm, false);

    assert!(
        driver::eligible(&vm, outer),
        "a mono non-leaf send (whose inner send is a cold trap) is still eligible"
    );
    let id = driver::compile_method(&mut vm, smi_klass, outer).expect("must compile");

    // The inlined body's inner send is an uncommon TRAP (Untaken), not a
    // CallSend → the nmethod records NO compiled IC site, but DOES record the
    // (recv_klass, run) inline dep and a nested (depth-2) deopt scope.
    {
        let nm = vm.code_table.get(id).expect("installed");
        assert!(
            nm.ic_sites.is_empty(),
            "the inlined body's cold inner send is a trap, not a compiled IC site"
        );
        assert_eq!(nm.inline_deps.len(), 1, "one inline dependency (`run`)");
    }

    // Argument of recv_klass whose instvar0 holds a discriminating value.
    let discriminating = SmallInt::new(45654).oop();
    let arg = alloc::alloc_slots(&mut vm, recv_klass).oop();
    MemOop::try_from(arg)
        .unwrap()
        .set_body_oop(0, discriminating);
    let self_smi = SmallInt::new(7).oop();

    // Interpreter reference.
    let interp = macvm::interpreter::run_method(&mut vm, outer, self_smi, &[arg]);
    assert_eq!(interp.raw(), discriminating.raw());

    let nm = vm.code_table.get(id).expect("installed");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;

    let deopts_before = unsafe { (*vm_ptr).stats.deopt_count };
    // Entering the compiled `outer` passes the inline guard (x IS recv_klass),
    // splices into the inlined `run` body, and hits the in-body trap on
    // `self bar` → deopt through the SenderLink chain → rebuild the `run` frame
    // AND the `outer` frame → interpret both → `^self bar` = 45654.
    let result = unsafe { call(entry, vm_ptr, [self_smi.raw(), arg.raw()].as_ptr(), 2) };
    let deopts_after = unsafe { (*vm_ptr).stats.deopt_count };

    assert_eq!(
        result,
        discriminating.raw(),
        "the in-body trap must deopt through BOTH frames and return the inlined \
         body's own computed value"
    );
    assert_eq!(
        result,
        interp.raw(),
        "and match the pure-interpreter reference (differential — proves both frames rebuilt)"
    );
    assert_eq!(
        deopts_after,
        deopts_before + 1,
        "exactly one deopt (the in-body trap's brk fired)"
    );
}

/// S14 step 4c (c): REDEFINITION invalidation for a non-leaf inlined callee.
/// Redefining the inlined `run` on the receiver klass makes the caller nmethod
/// `NotEntrant` — its guard assumed `lookup(recv_klass, run)` == the spliced
/// body, an assumption a redefinition breaks (identical mechanism to 4b, on a
/// non-leaf callee).
#[test]
fn redefining_inlined_nonleaf_callee_invalidates_caller() {
    let mut vm = test_vm();
    let smi_klass = vm.universe.smi_klass;
    let (recv_klass, run_sel, _bar_sel, outer) = nonleaf_inline_scenario(&mut vm, true);
    let id = driver::compile_method(&mut vm, smi_klass, outer).expect("must compile");
    assert!(
        matches!(vm.code_table.get(id).unwrap().state, NmState::Alive),
        "freshly compiled → Alive"
    );

    // An unrelated redefinition must NOT invalidate.
    let unrelated = vm.universe.intern(b"unrelatedNL");
    let unrelated_body = trivial_method(&mut vm, unrelated);
    install_method(&mut vm, recv_klass, unrelated, unrelated_body);
    assert!(
        matches!(vm.code_table.get(id).unwrap().state, NmState::Alive),
        "unrelated selector must NOT invalidate the inlining caller"
    );

    // Redefining the inlined callee `run` itself → NotEntrant.
    let new_run = {
        let mut rb = BytecodeBuilder::new();
        rb.push_self();
        rb.ret_tos();
        rb.finish(&mut vm, run_sel, 0, 0)
    };
    install_method(&mut vm, recv_klass, run_sel, new_run);
    assert!(
        matches!(vm.code_table.get(id).unwrap().state, NmState::NotEntrant),
        "redefining the inlined non-leaf `run` must make the caller nmethod NotEntrant"
    );
}

// ── S14 step 7-I: value-send block inlining (non-capturing, safepoint-free) ──

/// Install `value` (argc 0) and `value:` (argc 1) as primitive-50 block
/// activation on `closure_klass`, so the INTERPRETER reference path can
/// activate a literal block. The COMPILED path never dispatches these (it
/// splices the block body inline), so this only feeds the interpreter oracle.
fn install_value_prims(vm: &mut VmState) {
    // Value-family primitive ids are `50 + argc` (runtime/primitives.rs:
    // value=50, value:=51, value:value:=52, …).
    for (name, argc) in [
        (b"value".as_slice(), 0usize),
        (b"value:".as_slice(), 1usize),
    ] {
        let sel = vm.universe.intern(name);
        let mut vb = BytecodeBuilder::new();
        vb.push_self();
        vb.ret_self();
        let m = vb.finish(vm, sel, argc, 0);
        m.set_primitive((50 + argc) as i64);
        let closure_klass = vm.universe.closure_klass;
        let sel = vm.universe.intern(name);
        install_method(vm, closure_klass, sel, m);
    }
}

/// S14 step 7-I (a): a literal block invoked directly by `value` in the same
/// method is SPLICED inline — no closure is allocated, no `value` is dispatched.
/// `run [ ^[42] value ]` compiles (the escape pre-pass proves `[42]` elidable)
/// and the compiled nmethod returns 42, exactly the interpreter's answer.
#[test]
fn compiled_direct_value_block_matches_interpreter() {
    let mut vm = test_vm();
    install_value_prims(&mut vm);
    let smi_klass = vm.universe.smi_klass;
    let value_sel = vm.universe.intern(b"value");
    let run_sel = vm.universe.intern(b"run");

    // `run [ ^[42] value ]`.
    let mut b = BytecodeBuilder::new();
    let lit = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, _vm| {
        blk.push_smi_i8(42);
        blk.block_return_tos();
    });
    b.push_closure(lit, 0);
    b.send(&mut vm, value_sel, 0);
    b.ret_tos();
    let run = b.finish(&mut vm, run_sel, 0, 0);

    // Eligible (the only closure it makes is directly value'd → elidable).
    assert!(
        driver::eligible(&vm, run),
        "a method whose only closure is directly value'd must be eligible (7-I)"
    );

    // Interpreter reference.
    let self_smi = SmallInt::new(1).oop();
    let interp = macvm::interpreter::run_method(&mut vm, run, self_smi, &[]);
    assert_eq!(
        interp.raw(),
        SmallInt::new(42).oop().raw(),
        "interp: [42] value = 42"
    );

    // Compiled (customized for SmallInteger; self is unused by the body). The
    // block body was spliced — no compiled IC site for the `value` send.
    let id = driver::compile_method(&mut vm, smi_klass, run).expect("must compile");
    assert!(
        vm.code_table.get(id).unwrap().ic_sites.is_empty(),
        "the `value` send was spliced → no compiled IC site"
    );
    let nm = vm.code_table.get(id).expect("installed");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let result = unsafe { call(entry, vm_ptr, [self_smi.raw()].as_ptr(), 1) };
    assert_eq!(
        result,
        SmallInt::new(42).oop().raw(),
        "compiled `[42] value` splices to 42 (differential match)"
    );
}

/// S14 step 7-I (b): a one-arg block invoked with `value:` splices, its arg
/// aliasing the send operand. `applyTo7 [ ^[:x | x] value: 7 ]` → 7, both
/// interpreted and compiled.
#[test]
fn compiled_value_arg_block_matches_interpreter() {
    let mut vm = test_vm();
    install_value_prims(&mut vm);
    let smi_klass = vm.universe.smi_klass;
    let value_arg_sel = vm.universe.intern(b"value:");
    let sel = vm.universe.intern(b"applyTo7");

    // `applyTo7 [ ^[:x | x] value: 7 ]` — identity block, send-free body.
    let mut b = BytecodeBuilder::new();
    let lit = b.build_block(&mut vm, 1, 0, false, 0, false, |blk, _vm| {
        blk.push_temp(0); // x (the block's arg)
        blk.block_return_tos();
    });
    b.push_closure(lit, 0);
    b.push_smi_i8(7);
    b.send(&mut vm, value_arg_sel, 1);
    b.ret_tos();
    let m = b.finish(&mut vm, sel, 0, 0);

    assert!(
        driver::eligible(&vm, m),
        "value:-invoked identity block is eligible"
    );

    let self_smi = SmallInt::new(1).oop();
    let interp = macvm::interpreter::run_method(&mut vm, m, self_smi, &[]);
    assert_eq!(
        interp.raw(),
        SmallInt::new(7).oop().raw(),
        "interp: [:x|x] value: 7 = 7"
    );

    let id = driver::compile_method(&mut vm, smi_klass, m).expect("must compile");
    let nm = vm.code_table.get(id).expect("installed");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let result = unsafe { call(entry, vm_ptr, [self_smi.raw()].as_ptr(), 1) };
    assert_eq!(
        result,
        SmallInt::new(7).oop().raw(),
        "compiled `[:x|x] value: 7` splices its arg through to 7 (differential match)"
    );
}

/// S14 step 7-I: a closure that ESCAPES (stored into an instvar) keeps the whole
/// method interpreted — the "inline-or-gated" soundness boundary. The method is
/// ineligible and never compiles, and the interpreter still runs it correctly.
#[test]
fn compiled_escaping_block_stays_interpreted() {
    let mut vm = test_vm();
    install_value_prims(&mut vm);
    let holder = vm.universe.new_klass(
        vm.universe.object_klass,
        "S14EscapingHolder",
        Format::Slots,
        false,
        HEADER_WORDS + 1,
    );
    let stash_sel = vm.universe.intern(b"stash");

    // `stash [ block := [42]. ^self ]` — the closure is stored into instvar 0
    // (it escapes: a compiled frame cannot be its `home_frame_ref`).
    let mut b = BytecodeBuilder::new();
    let lit = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, _vm| {
        blk.push_smi_i8(42);
        blk.block_return_tos();
    });
    b.push_closure(lit, 0);
    b.store_instvar_pop(0);
    b.ret_self();
    let stash = b.finish(&mut vm, stash_sel, 0, 0);

    // Ineligible: the escape pre-pass finds a non-elidable closure site.
    assert!(
        !driver::eligible(&vm, stash),
        "a method that stores a closure (escaping) must stay interpreted (7-I gate)"
    );
    assert!(
        driver::compile_method(&mut vm, holder, stash).is_none(),
        "an escaping-closure method must not compile"
    );

    // The interpreter still runs it: returns self, and instvar0 holds the closure.
    let obj = alloc::alloc_slots(&mut vm, holder).oop();
    let result = macvm::interpreter::run_method(&mut vm, stash, obj, &[]);
    assert_eq!(result.raw(), obj.raw(), "interp: `stash` returns self");
    let stored = MemOop::try_from(obj).unwrap().body_oop(0);
    assert!(
        macvm::oops::wrappers::ClosureOop::try_from(stored).is_some(),
        "the escaping closure was stored into instvar0 by the interpreter"
    );
}

/// S14 step 7-II: a spliced block with an in-body send that DEOPTS. `applyTo7
/// [ ^[:x | x bar] value: 7 ]` splices `[:x | x bar]` inline; the block's own
/// `x bar` send is cold (Untaken) → a step-3 uncommon TRAP inside the elided
/// block, recording an `is_block` deopt scope chained to the home method. Calling
/// the compiled method hits that trap → `deoptimize_frame` must rebuild the home
/// method's frame AND the BLOCK's own activation frame (an `is_block` scope) from
/// the ONE physical compiled frame → `interpret_active` re-executes `x bar`
/// generically → identical result to the pure interpreter, `deopt_count` bumped.
/// This is the first time the materializer rebuilds a block frame.
#[test]
fn deopt_through_inlined_block_rebuilds_block_frame() {
    let mut vm = VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: Default::default(),
        gc_stress: false,
        gc_stress_full_period: None,
        eden_kb: None,
        jit: JitMode::Threshold(1),
    });
    install_value_prims(&mut vm);
    let smi_klass = vm.universe.smi_klass;

    // `bar [ ^42 ]` on SmallInteger (the block's `x bar`, x == 7, dispatches here).
    let bar_sel = vm.universe.intern(b"bar");
    let bar = {
        let mut bb = BytecodeBuilder::new();
        bb.push_smi_i8(42);
        bb.ret_tos();
        bb.finish(&mut vm, bar_sel, 0, 0)
    };
    install_method(&mut vm, smi_klass, bar_sel, bar);

    // `applyTo7 [ ^[:x | x bar] value: 7 ]`. The block's `x bar` IC stays Empty
    // → Untaken → an in-body trap once spliced.
    let value_arg_sel = vm.universe.intern(b"value:");
    let sel = vm.universe.intern(b"applyTo7");
    let mut b = BytecodeBuilder::new();
    let lit = b.build_block(&mut vm, 1, 0, false, 0, false, |blk, vm| {
        blk.push_temp(0); // x
        blk.send(vm, bar_sel, 0); // x bar  (cold → trap)
        blk.block_return_tos();
    });
    b.push_closure(lit, 0);
    b.push_smi_i8(7);
    b.send(&mut vm, value_arg_sel, 1);
    b.ret_tos();
    let m = b.finish(&mut vm, sel, 0, 0);

    assert!(
        driver::eligible(&vm, m),
        "a send-ful directly-value'd block is eligible (7-II)"
    );
    let id = driver::compile_method(&mut vm, smi_klass, m).expect("must compile");
    // The block's cold inner send is a trap, not a compiled IC site.
    assert!(
        vm.code_table.get(id).unwrap().ic_sites.is_empty(),
        "the block's cold `x bar` is a trap → no compiled IC site"
    );

    let self_smi = SmallInt::new(1).oop();
    // Interpreter reference.
    let interp = macvm::interpreter::run_method(&mut vm, m, self_smi, &[]);
    assert_eq!(
        interp.raw(),
        SmallInt::new(42).oop().raw(),
        "interp: [:x|x bar] value: 7 = 42"
    );

    let nm = vm.code_table.get(id).expect("installed");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let deopts_before = unsafe { (*vm_ptr).stats.deopt_count };
    let result = unsafe { call(entry, vm_ptr, [self_smi.raw()].as_ptr(), 1) };
    let deopts_after = unsafe { (*vm_ptr).stats.deopt_count };

    assert_eq!(
        result,
        SmallInt::new(42).oop().raw(),
        "the in-block trap must deopt through the block frame + home frame and return 42"
    );
    assert_eq!(
        deopts_after,
        deopts_before + 1,
        "exactly one deopt fired (the block's trap)"
    );
}

/// S14 step 7-II: a spliced block whose in-body send is WARM → a real compiled
/// `CallSend` inside the elided block (no trap, no deopt). `[:x | x bar] value:
/// 7` with `x bar` warmed mono → the block body dispatches `bar` and returns 42,
/// matching the interpreter, WITHOUT any deopt.
#[test]
fn compiled_block_with_warm_send_matches_interpreter() {
    let mut vm = test_vm();
    install_value_prims(&mut vm);
    let smi_klass = vm.universe.smi_klass;

    let bar_sel = vm.universe.intern(b"bar");
    let bar = {
        let mut bb = BytecodeBuilder::new();
        bb.push_smi_i8(42);
        bb.ret_tos();
        bb.finish(&mut vm, bar_sel, 0, 0)
    };
    install_method(&mut vm, smi_klass, bar_sel, bar);

    let value_arg_sel = vm.universe.intern(b"value:");
    let sel = vm.universe.intern(b"applyTo7");
    let mut b = BytecodeBuilder::new();
    let lit = b.build_block(&mut vm, 1, 0, false, 0, false, |blk, vm| {
        blk.push_temp(0);
        blk.send(vm, bar_sel, 0);
        blk.block_return_tos();
    });
    b.push_closure(lit, 0);
    b.push_smi_i8(7);
    b.send(&mut vm, value_arg_sel, 1);
    b.ret_tos();
    let m = b.finish(&mut vm, sel, 0, 0);

    // Warm the BLOCK's own `x bar` IC (ic index 0) to Mono on SmallInteger so it
    // splices as a compiled `CallSend`, not a trap.
    let block = MethodOop::try_from(m.literals().at(lit)).unwrap();
    let epoch = vm.ic_epoch;
    InterpreterIc::at(block, 0).set_mono(&mut vm, smi_klass, bar, epoch);

    assert!(driver::eligible(&vm, m));
    let id = driver::compile_method(&mut vm, smi_klass, m).expect("must compile");
    // The block's warm `x bar` splices as a real compiled IC site.
    assert_eq!(
        vm.code_table.get(id).unwrap().ic_sites.len(),
        1,
        "the block's warm `x bar` is a compiled IC site inside the splice"
    );

    let self_smi = SmallInt::new(1).oop();
    let interp = macvm::interpreter::run_method(&mut vm, m, self_smi, &[]);
    assert_eq!(interp.raw(), SmallInt::new(42).oop().raw());

    let nm = vm.code_table.get(id).expect("installed");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let deopts_before = unsafe { (*vm_ptr).stats.deopt_count };
    let result = unsafe { call(entry, vm_ptr, [self_smi.raw()].as_ptr(), 1) };
    let deopts_after = unsafe { (*vm_ptr).stats.deopt_count };
    assert_eq!(
        result,
        SmallInt::new(42).oop().raw(),
        "compiled block CallSend → 42"
    );
    assert_eq!(
        deopts_after, deopts_before,
        "the warm block send does not deopt"
    );
}

// ── S14 step 7-II-b: captured-temp promotion + Context elision ──────────────

/// S14 step 7-II-b: a home method whose captured temp is READ by a send-free
/// elided block. `foo [ |x| x := 7. [x] value. ^x ]` — `x` is a ctx-temp
/// (nctx=1) captured by `[x]`; with the block inlined, `x` promotes to a vreg
/// and M's Context is elided. Both interp and compiled return 7.
#[test]
fn compiled_captured_temp_read_matches_interpreter() {
    let mut vm = test_vm();
    install_value_prims(&mut vm);
    let smi_klass = vm.universe.smi_klass;
    let value_sel = vm.universe.intern(b"value");
    let sel = vm.universe.intern(b"foo");

    let mut b = BytecodeBuilder::new();
    // block `[x]` — captures_ctx, reads M's ctx-temp 0 at depth 0, send-free.
    let lit = b.build_block(&mut vm, 0, 0, false, 0, true, |blk, _vm| {
        blk.push_ctx_temp(0, 0);
        blk.block_return_tos();
    });
    b.push_smi_i8(7);
    b.store_ctx_temp_pop(0, 0); // x := 7
    b.push_closure(lit, 0);
    b.send(&mut vm, value_sel, 0); // [x] value
    b.pop(); // discard the value result
    b.push_ctx_temp(0, 0); // x
    b.ret_tos();
    let m = b.finish(&mut vm, sel, 0, 0);
    m.set_flags(0, 0, true, false, false, false, 1); // has_ctx, nctx=1

    assert!(
        driver::eligible(&vm, m),
        "a has_ctx method with an elidable capturing block is eligible"
    );
    let self_smi = SmallInt::new(1).oop();
    let interp = macvm::interpreter::run_method(&mut vm, m, self_smi, &[]);
    assert_eq!(
        interp.raw(),
        SmallInt::new(7).oop().raw(),
        "interp: captured x read = 7"
    );

    let id = driver::compile_method(&mut vm, smi_klass, m).expect("must compile");
    let nm = vm.code_table.get(id).expect("installed");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let result = unsafe { call(entry, vm_ptr, [self_smi.raw()].as_ptr(), 1) };
    assert_eq!(
        result,
        SmallInt::new(7).oop().raw(),
        "compiled: promoted ctx-temp read = 7"
    );
}

/// S14 step 7-II-b: a captured temp WRITTEN by a send-free elided block.
/// `foo [ |x| x := 0. [:v | x := v. x] value: 9. ^x ]` → 9 (the block writes M's
/// ctx-temp, M reads it back through the promoted vreg).
#[test]
fn compiled_captured_temp_write_matches_interpreter() {
    let mut vm = test_vm();
    install_value_prims(&mut vm);
    let smi_klass = vm.universe.smi_klass;
    let value_arg_sel = vm.universe.intern(b"value:");
    let sel = vm.universe.intern(b"foo");

    let mut b = BytecodeBuilder::new();
    // block `[:v | x := v. x]` — captures_ctx, writes+reads M's ctx-temp 0.
    let lit = b.build_block(&mut vm, 1, 0, false, 0, true, |blk, _vm| {
        blk.push_temp(0); // v (block arg)
        blk.store_ctx_temp_pop(0, 0); // x := v
        blk.push_ctx_temp(0, 0); // x
        blk.block_return_tos();
    });
    b.push_smi_i8(0);
    b.store_ctx_temp_pop(0, 0); // x := 0
    b.push_closure(lit, 0);
    b.push_smi_i8(9);
    b.send(&mut vm, value_arg_sel, 1); // [:v|...] value: 9
    b.pop();
    b.push_ctx_temp(0, 0); // x
    b.ret_tos();
    let m = b.finish(&mut vm, sel, 0, 0);
    m.set_flags(0, 0, true, false, false, false, 1); // has_ctx, nctx=1

    assert!(driver::eligible(&vm, m));
    let self_smi = SmallInt::new(1).oop();
    let interp = macvm::interpreter::run_method(&mut vm, m, self_smi, &[]);
    assert_eq!(
        interp.raw(),
        SmallInt::new(9).oop().raw(),
        "interp: block-written x = 9"
    );

    let id = driver::compile_method(&mut vm, smi_klass, m).expect("must compile");
    let nm = vm.code_table.get(id).expect("installed");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let result = unsafe { call(entry, vm_ptr, [self_smi.raw()].as_ptr(), 1) };
    assert_eq!(
        result,
        SmallInt::new(9).oop().raw(),
        "compiled: block writes promoted ctx-temp = 9"
    );
}

/// S14 step 7-II-b: the `CtxLoc::Elided` materialization. M reads a captured
/// temp, then hits a COLD send that traps → deopt must allocate a fresh Context
/// and fill it from the promoted vreg, so the post-deopt `^x` (a ctx-temp read
/// in the interpreter) sees the right value. == interp, deopt_count +1.
#[test]
fn compiled_captured_temp_deopt_materializes_context() {
    let mut vm = VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: Default::default(),
        gc_stress: false,
        gc_stress_full_period: None,
        eden_kb: None,
        jit: JitMode::Threshold(1),
    });
    install_value_prims(&mut vm);
    let smi_klass = vm.universe.smi_klass;
    let value_sel = vm.universe.intern(b"value");

    // `poke [ ^self ]` on SmallInteger — the cold send's target (IC stays Empty
    // → Untaken → a trap once compiled).
    let poke_sel = vm.universe.intern(b"poke");
    let poke = {
        let mut pb = BytecodeBuilder::new();
        pb.ret_self();
        pb.finish(&mut vm, poke_sel, 0, 0)
    };
    install_method(&mut vm, smi_klass, poke_sel, poke);

    // `foo [ |x| x := 7. [x] value. self poke. ^x ]` (self is a smi).
    let sel = vm.universe.intern(b"foo");
    let mut b = BytecodeBuilder::new();
    let lit = b.build_block(&mut vm, 0, 0, false, 0, true, |blk, _vm| {
        blk.push_ctx_temp(0, 0);
        blk.block_return_tos();
    });
    b.push_smi_i8(7);
    b.store_ctx_temp_pop(0, 0);
    b.push_closure(lit, 0);
    b.send(&mut vm, value_sel, 0);
    b.pop();
    b.push_self();
    b.send(&mut vm, poke_sel, 0); // self poke  (cold → trap → deopt)
    b.pop();
    b.push_ctx_temp(0, 0); // x
    b.ret_tos();
    let m = b.finish(&mut vm, sel, 0, 0);
    m.set_flags(0, 0, true, false, false, false, 1);

    assert!(driver::eligible(&vm, m));
    let self_smi = SmallInt::new(3).oop();
    // Compile BEFORE the interp reference: running the interpreter first would
    // warm `self poke` to Mono (→ inlined leaf, no trap). Compiling with the IC
    // still Empty keeps it Untaken → the cold trap this test's deopt needs.
    let id = driver::compile_method(&mut vm, smi_klass, m).expect("must compile");
    let interp = macvm::interpreter::run_method(&mut vm, m, self_smi, &[]);
    assert_eq!(interp.raw(), SmallInt::new(7).oop().raw());

    let nm = vm.code_table.get(id).expect("installed");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let deopts_before = unsafe { (*vm_ptr).stats.deopt_count };
    let result = unsafe { call(entry, vm_ptr, [self_smi.raw()].as_ptr(), 1) };
    let deopts_after = unsafe { (*vm_ptr).stats.deopt_count };
    assert_eq!(
        result,
        SmallInt::new(7).oop().raw(),
        "deopt must materialize M's elided Context so the post-deopt ctx-temp read = 7"
    );
    assert_eq!(
        deopts_after,
        deopts_before + 1,
        "the cold `self poke` trap fired once"
    );
}

/// S14 step 7-II-b-ii: a SEND-FUL capturing block whose in-block send deopts.
/// `foo [ |sum| sum := 0. [:e | sum := e bar. sum] value: 5. ^sum ]` with the
/// block's `e bar` cold (Untaken) → a trap INSIDE the elided block. The deopt
/// rebuilds the block's activation frame whose Context ALIASES M's (materialized
/// from the promoted vreg); the post-deopt `sum := e bar` writes THAT Context, so
/// M's `^sum` reads it back == interp. Exercises the block-frame context aliasing.
#[test]
fn deopt_through_capturing_block_aliases_home_context() {
    let mut vm = VmState::with_options(VmOptions {
        heap_mib: 64,
        trace: Default::default(),
        gc_stress: false,
        gc_stress_full_period: None,
        eden_kb: None,
        jit: JitMode::Threshold(1),
    });
    install_value_prims(&mut vm);
    let smi_klass = vm.universe.smi_klass;

    // `bar [ ^42 ]` on SmallInteger (the block's `e bar`, e == 5, dispatches here).
    let bar_sel = vm.universe.intern(b"bar");
    let bar = {
        let mut bb = BytecodeBuilder::new();
        bb.push_smi_i8(42);
        bb.ret_tos();
        bb.finish(&mut vm, bar_sel, 0, 0)
    };
    install_method(&mut vm, smi_klass, bar_sel, bar);

    let value_arg_sel = vm.universe.intern(b"value:");
    let sel = vm.universe.intern(b"foo");
    let mut b = BytecodeBuilder::new();
    // block `[:e | sum := e bar. sum]` — captures_ctx, a send (`e bar`) then a
    // ctx-temp write, then reads it back.
    let lit = b.build_block(&mut vm, 1, 0, false, 0, true, |blk, vm| {
        blk.push_temp(0); // e
        blk.send(vm, bar_sel, 0); // e bar (cold → trap)
        blk.store_ctx_temp_pop(0, 0); // sum := (e bar)
        blk.push_ctx_temp(0, 0); // sum
        blk.block_return_tos();
    });
    b.push_smi_i8(0);
    b.store_ctx_temp_pop(0, 0); // sum := 0
    b.push_closure(lit, 0);
    b.push_smi_i8(5);
    b.send(&mut vm, value_arg_sel, 1); // [:e|...] value: 5
    b.pop();
    b.push_ctx_temp(0, 0); // sum
    b.ret_tos();
    let m = b.finish(&mut vm, sel, 0, 0);
    m.set_flags(0, 0, true, false, false, false, 1); // has_ctx, nctx=1

    assert!(
        driver::eligible(&vm, m),
        "a send-ful capturing block is eligible (7-II-b-ii)"
    );
    let self_smi = SmallInt::new(1).oop();
    // Compile before interp (keep `e bar` Untaken → the trap this test forces).
    let id = driver::compile_method(&mut vm, smi_klass, m).expect("must compile");
    let interp = macvm::interpreter::run_method(&mut vm, m, self_smi, &[]);
    assert_eq!(
        interp.raw(),
        SmallInt::new(42).oop().raw(),
        "interp: sum := (5 bar) = 42"
    );

    let nm = vm.code_table.get(id).expect("installed");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let deopts_before = unsafe { (*vm_ptr).stats.deopt_count };
    let result = unsafe { call(entry, vm_ptr, [self_smi.raw()].as_ptr(), 1) };
    let deopts_after = unsafe { (*vm_ptr).stats.deopt_count };
    assert_eq!(
        result,
        SmallInt::new(42).oop().raw(),
        "the in-block trap must alias the block frame's Context to M's, so the \
         post-deopt `sum :=` write is read back by M's `^sum` = 42"
    );
    assert_eq!(
        deopts_after,
        deopts_before + 1,
        "the cold `e bar` trap fired once"
    );
}

// ── S14 step 7-III: non-local return from an inlined (send-free) block ──────

/// S14 step 7-III: `^expr` inside an inlined block returns from the block's HOME
/// method. `foo [ [^42] value. ^0 ]` — the block's `^42` is a non-local return
/// from `foo`, so `foo` returns 42 and the trailing `^0` is never reached. When
/// `[^42]` is spliced into `foo`, the NLR lowers to a plain return-from-foo
/// (`Ir::Ret`). Compiled == interpreter.
#[test]
fn compiled_block_nlr_returns_from_home() {
    let mut vm = test_vm();
    install_value_prims(&mut vm);
    let smi_klass = vm.universe.smi_klass;
    let value_sel = vm.universe.intern(b"value");
    let sel = vm.universe.intern(b"foo");

    let mut b = BytecodeBuilder::new();
    // block `[^42]` — a send-free non-local return.
    let lit = b.build_block(&mut vm, 0, 0, false, 0, false, |blk, _vm| {
        blk.push_smi_i8(42);
        blk.nlr_tos();
    });
    b.push_closure(lit, 0);
    b.send(&mut vm, value_sel, 0); // [^42] value   → NLR returns 42 from foo
    b.pop();
    b.push_smi_i8(0);
    b.ret_tos(); // ^0  — unreachable (the block already returned from foo)
    let m = b.finish(&mut vm, sel, 0, 0);

    assert!(
        driver::eligible(&vm, m),
        "a send-free NLR block is eligible (7-III)"
    );
    let self_smi = SmallInt::new(1).oop();
    let interp = macvm::interpreter::run_method(&mut vm, m, self_smi, &[]);
    assert_eq!(
        interp.raw(),
        SmallInt::new(42).oop().raw(),
        "interp: [^42] value returns 42 from foo"
    );

    let id = driver::compile_method(&mut vm, smi_klass, m).expect("must compile");
    let nm = vm.code_table.get(id).expect("installed");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let result = unsafe { call(entry, vm_ptr, [self_smi.raw()].as_ptr(), 1) };
    assert_eq!(
        result,
        SmallInt::new(42).oop().raw(),
        "compiled: block NLR lowers to a return-from-foo = 42 (differential match)"
    );
}

/// S14 step 7-III: an NLR block that returns its own arg. `foo [ [:x | ^x]
/// value: 7. ^0 ]` → 7 (the `^x` returns the block's arg from foo).
#[test]
fn compiled_block_nlr_with_arg_matches_interpreter() {
    let mut vm = test_vm();
    install_value_prims(&mut vm);
    let smi_klass = vm.universe.smi_klass;
    let value_arg_sel = vm.universe.intern(b"value:");
    let sel = vm.universe.intern(b"foo");

    let mut b = BytecodeBuilder::new();
    let lit = b.build_block(&mut vm, 1, 0, false, 0, false, |blk, _vm| {
        blk.push_temp(0); // x
        blk.nlr_tos(); // ^x
    });
    b.push_closure(lit, 0);
    b.push_smi_i8(7);
    b.send(&mut vm, value_arg_sel, 1);
    b.pop();
    b.push_smi_i8(0);
    b.ret_tos();
    let m = b.finish(&mut vm, sel, 0, 0);

    assert!(driver::eligible(&vm, m));
    let self_smi = SmallInt::new(1).oop();
    let interp = macvm::interpreter::run_method(&mut vm, m, self_smi, &[]);
    assert_eq!(interp.raw(), SmallInt::new(7).oop().raw());

    let id = driver::compile_method(&mut vm, smi_klass, m).expect("must compile");
    let nm = vm.code_table.get(id).expect("installed");
    let entry = unsafe { nm.code.base.add(nm.entry_off as usize) } as u64;
    let call: CallStubFn = unsafe { std::mem::transmute(vm.stubs.call_stub_entry()) };
    let vm_ptr: *mut VmState = &mut vm;
    let result = unsafe { call(entry, vm_ptr, [self_smi.raw()].as_ptr(), 1) };
    assert_eq!(
        result,
        SmallInt::new(7).oop().raw(),
        "compiled block NLR of arg = 7"
    );
}
