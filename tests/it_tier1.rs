//! Sprint S10 integration tests (`tests_s10.md`). This file is allowed
//! `unsafe` (it lives in `tests/`, a separate crate from `macvm` itself).

use macvm::bytecode::builder::BytecodeBuilder;
use macvm::codecache::stubs::{self, CallStubFn};
use macvm::codecache::CodeCache;
use macvm::compiler::driver;
use macvm::compiler::emit;
use macvm::compiler::ir::{
    BailoutReason, BlockId, CmpOp, Ir, IrBlock, IrMethod, PoolLit, SmiOp, VReg, VRegInfo,
};
use macvm::compiler::jasm_assembler::JasmAssembler;
use macvm::compiler::regalloc;
use macvm::interpreter::ic::InterpreterIc;
use macvm::oops::smi::SmallInt;
use macvm::oops::wrappers::SymbolOop;
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
    };
    let block3 = IrBlock {
        id: BlockId(3),
        bci: 30,
        code: vec![Ir::Bailout {
            reason: BailoutReason::SmiOpFailed,
        }],
        entry_stack: Vec::new(),
    };

    let method = IrMethod {
        blocks: vec![block0, block1, block2, block3],
        vregs,
        pool: Vec::new(),
        argc: 2,
        ntemps: 0,
        safepoints: Vec::new(),
        // Unused: this method has no SmiCmpVal/BoolBr, so emit.rs never
        // dereferences these against the (also empty) pool.
        true_lit: PoolLit(0),
        false_lit: PoolLit(0),
    };

    let regalloc_result = regalloc::regalloc(&method);

    let mut asm = JasmAssembler::new();
    let (blob, pcs) = emit::emit(&mut asm, &method, &regalloc_result, stubs.stub_poll_addr());
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
    let (blob, _pcs) = emit::emit(&mut asm, method, &regalloc_result, stub_poll_addr);
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
    };
    let block1 = IrBlock {
        id: BlockId(1),
        bci: 10,
        code: vec![Ir::Bailout {
            reason: BailoutReason::SmiOpFailed,
        }],
        entry_stack: Vec::new(),
    };
    IrMethod {
        blocks: vec![block0, block1],
        vregs: (0..4).map(|_| VRegInfo { is_oop: true }).collect(),
        pool: Vec::new(),
        argc: 2,
        ntemps: 0,
        safepoints: Vec::new(),
        true_lit: PoolLit(0),
        false_lit: PoolLit(0),
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
    };
    let block1 = IrBlock {
        id: bailout,
        bci: 1000,
        code: vec![Ir::Bailout {
            reason: BailoutReason::SmiOpFailed,
        }],
        entry_stack: Vec::new(),
    };
    let method = IrMethod {
        blocks: vec![block0, block1],
        vregs,
        pool: Vec::new(),
        argc: 0,
        ntemps: 0,
        safepoints: Vec::new(),
        true_lit: PoolLit(0),
        false_lit: PoolLit(0),
    };

    let regalloc_result = regalloc::regalloc(&method);
    assert!(
        regalloc_result.frame_slots > 0,
        "20 simultaneously-live vregs must force at least one spill"
    );

    let mut asm = JasmAssembler::new();
    let (blob, _pcs) = emit::emit(&mut asm, &method, &regalloc_result, stubs.stub_poll_addr());
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

    // Overflowing operands must bail out to the sentinel, not crash or
    // silently wrap — the interpreter fallback (S10 step 8) isn't wired up
    // yet, so this just checks the compiled entry itself does the right
    // thing at its own boundary.
    // Both individually valid smis (SMI_MAX itself), but their sum isn't.
    let big = macvm::oops::smi::SmallInt::MAX;
    let argv_overflow = [
        SmallInt::new(big).oop().raw(),
        SmallInt::new(big).oop().raw(),
    ];
    let overflow_result = unsafe { call(entry, vm_ptr, argv_overflow.as_ptr(), 2) };
    assert_eq!(
        overflow_result, 0b10,
        "overflowing smi add must return the BAILOUT sentinel"
    );
}
