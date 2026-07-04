//! Sprint S10 integration tests (`tests_s10.md`). This file is allowed
//! `unsafe` (it lives in `tests/`, a separate crate from `macvm` itself).

use std::path::PathBuf;
use std::process::{Command, Stdio};

use macvm::bytecode::builder::BytecodeBuilder;
use macvm::codecache::nmethod::{IcSite, IcState, NmState, Nmethod, NmethodId};
use macvm::codecache::pics::PIC_MAX_ENTRIES;
use macvm::codecache::stubs::{self, CallStubFn};
use macvm::codecache::CodeCache;
use macvm::compiler::driver;
use macvm::compiler::emit;
use macvm::compiler::ir::{
    BailoutReason, BlockId, CallSiteInfo, CmpOp, Ir, IrBlock, IrMethod, PoolLit, SmiOp, VReg,
    VRegInfo,
};
use macvm::compiler::jasm_assembler::JasmAssembler;
use macvm::compiler::regalloc;
use macvm::frontend::{classdef, parser};
use macvm::interpreter::ic::InterpreterIc;
use macvm::memory::alloc;
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
        call_sites: Vec::new(),
    };

    let regalloc_result = regalloc::regalloc(&method);

    let mut asm = JasmAssembler::new();
    let (blob, pcs, _verified_entry_off, _ic_sites) = emit::emit(
        &mut asm,
        &method,
        &regalloc_result,
        stubs.stub_poll_addr(),
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
    let (blob, _pcs, _verified_entry_off, _ic_sites) =
        emit::emit(&mut asm, method, &regalloc_result, stub_poll_addr, None);
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
        call_sites: Vec::new(),
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
        call_sites: Vec::new(),
    };

    let regalloc_result = regalloc::regalloc(&method);
    assert!(
        regalloc_result.frame_slots > 0,
        "20 simultaneously-live vregs must force at least one spill"
    );

    let mut asm = JasmAssembler::new();
    let (blob, _pcs, _verified_entry_off, _ic_sites) = emit::emit(
        &mut asm,
        &method,
        &regalloc_result,
        stubs.stub_poll_addr(),
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
/// after a normal (non-bailout) return and after a bailout — the call
/// stub's own prologue/epilogue and the compiled method's own frame
/// (`sub sp,sp,#frame_bytes` / `mov sp,x29`) must net to zero either way,
/// since both paths share the same epilogue (emit.rs's own `Ret`/
/// `Bailout` handling both just `b` to it). An imbalance here would
/// silently corrupt the REST of this process's native call stack — not
/// just this one call — so this is checked directly rather than inferred
/// from "the test suite didn't crash".
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
        m
    };
    let smi_klass = vm.universe.smi_klass;
    let epoch = vm.ic_epoch;
    InterpreterIc::at(method, 0).set_mono(&mut vm, smi_klass, plus_target_body, epoch);

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

    // Bailout call (overflowing operands).
    let big = SmallInt::new(SmallInt::MAX);
    vm.stack.push(big.oop());
    vm.stack.push(big.oop());
    let sp_before2 = native_sp();
    let result2 = macvm::interpreter::compiled_call::enter_compiled(&mut vm, id, 1);
    let sp_after2 = native_sp();
    assert_eq!(
        sp_before2, sp_after2,
        "native sp must be exactly restored after a bailout too -- same shared epilogue"
    );
    assert_eq!(
        result2,
        macvm::interpreter::compiled_call::EnterResult::Bailout
    );
    // Bailout leaves vm.stack untouched (still [receiver, arg]).
    assert_eq!(vm.stack.pop(), big.oop());
    assert_eq!(vm.stack.pop(), big.oop());
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
    let (blob, _pcs, _verified_entry_off, _ic_sites) =
        emit::emit(&mut asm, &ir, &ra, 0xDEAD_BEEF_0000_0000, None);
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
    };
    let caller_method = IrMethod {
        blocks: vec![block0],
        vregs,
        pool: Vec::new(),
        argc: 1,
        ntemps: 0,
        safepoints: Vec::new(),
        true_lit: PoolLit(0),
        false_lit: PoolLit(0),
        call_sites: vec![CallSiteInfo {
            selector: foo_sel,
            argc: 3,
        }],
    };
    let ra = regalloc::regalloc(&caller_method);
    let mut asm = JasmAssembler::new();
    let (blob, _pcs, _verified_entry_off, emitted_ic_sites) = emit::emit(
        &mut asm,
        &caller_method,
        &ra,
        vm.stubs.stub_poll_addr(),
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
        pcdescs: Vec::new(),
        oopmaps: Vec::new(),
        ic_sites,
        poll_bci: None,
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
    };
    let caller_method = IrMethod {
        blocks: vec![block0],
        vregs,
        pool: Vec::new(),
        argc: 1,
        ntemps: 0,
        safepoints: Vec::new(),
        true_lit: PoolLit(0),
        false_lit: PoolLit(0),
        call_sites: vec![CallSiteInfo {
            selector: foo_sel,
            argc: 3,
        }],
    };
    let ra = regalloc::regalloc(&caller_method);
    let mut asm = JasmAssembler::new();
    let (blob, _pcs, _verified_entry_off, emitted_ic_sites) = emit::emit(
        &mut asm,
        &caller_method,
        &ra,
        vm.stubs.stub_poll_addr(),
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
        pcdescs: Vec::new(),
        oopmaps: Vec::new(),
        ic_sites,
        poll_bci: None,
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
    };
    let caller_method = IrMethod {
        blocks: vec![block0],
        vregs,
        pool: Vec::new(),
        argc: 1,
        ntemps: 0,
        safepoints: Vec::new(),
        true_lit: PoolLit(0),
        false_lit: PoolLit(0),
        call_sites: vec![CallSiteInfo {
            selector: foo_sel,
            argc: 1,
        }],
    };
    let ra = regalloc::regalloc(&caller_method);
    let mut asm = JasmAssembler::new();
    let (blob, _pcs, _verified_entry_off, emitted_ic_sites) = emit::emit(
        &mut asm,
        &caller_method,
        &ra,
        vm.stubs.stub_poll_addr(),
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
        pcdescs: Vec::new(),
        oopmaps: Vec::new(),
        ic_sites,
        poll_bci: None,
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
