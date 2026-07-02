//! S5 frontend golden tests (`tests_s05.md` §Acceptance gate / §Integration
//! golden tests): source→bytecode disassembly goldens for real `.mst`
//! source (as opposed to `tests/it_golden.rs`'s S2 hand-`BytecodeBuilder`-
//! built ones), the full-opcode-coverage meta-test, and S2–S4 golden
//! programs re-expressed in source producing identical transcripts to
//! their hand-assembled originals.

mod common;

use std::path::PathBuf;

use macvm::bytecode::disasm::disassemble;
use macvm::frontend::ast::TopItem;
use macvm::frontend::{classdef, parser};
use macvm::oops::wrappers::{KlassOop, MemOop, MethodOop};
use macvm::runtime::vm_state::{OutputBuffer, VmState};

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn check_golden(name: &str, actual: &str) {
    let path = golden_dir().join(format!("{name}.bc.expected"));
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

/// Recursively disassembles `m`, then every `CompiledBlock` in its own
/// literal frame (depth-first, in literal-index order) — the plain
/// `disassemble` only covers one unit; nested block literals need their
/// own listing too for a real golden to be complete.
fn disassemble_deep(vm: &VmState, m: MethodOop, out: &mut String) {
    out.push_str(&disassemble(&vm.universe, m));
    let literals = m.literals();
    for i in 0..literals.len() {
        if let Some(blk) = MethodOop::try_from(literals.at(i)) {
            if blk.is_block() {
                disassemble_deep(vm, blk, out);
            }
        }
    }
}

/// Acceptance gate item 1: every opcode 00–12, 20–23, 30–33, 40–43 appears
/// at least once somewhere in the golden corpus.
#[test]
fn opcode_coverage_meta() {
    let mut mnemonics: Vec<&str> = vec![
        "push_self",
        "push_nil",
        "push_true",
        "push_false",
        "push_smi",
        "push_literal",
        "push_temp",
        "store_temp",
        "store_temp_pop",
        "push_instvar",
        "store_instvar_pop",
        "push_global",
        "store_global_pop",
        "pop",
        "dup",
        "push_ctx_temp",
        "store_ctx_temp_pop",
        "push_closure",
        "push_literal_w",
        "send",
        "send_super",
        "send_w",
        "send_super_w",
        "jump_fwd",
        "jump_back",
        "br_true_fwd",
        "br_false_fwd",
        "return_tos",
        "return_self",
        "block_return_tos",
        "nlr_tos",
    ];

    let mut corpus = String::new();
    for entry in std::fs::read_dir(golden_dir()).expect("read tests/golden") {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("expected")
            && path.to_str().is_some_and(|p| p.ends_with(".bc.expected"))
        {
            corpus.push_str(&std::fs::read_to_string(&path).unwrap());
            corpus.push('\n');
        }
    }

    mnemonics.retain(|m| {
        // Word-boundary-ish check: every disasm line is "  <bci>: <mnem> ...",
        // so " <mnem> " or " <mnem>\n" both occur for a real hit. Checking
        // both `send` (narrow) and e.g. `send_w`/`send_super` separately
        // works because we search for the exact mnemonic surrounded by
        // spaces/newlines, and mnemonics never collide as substrings of
        // ANOTHER mnemonic in a way that would falsely satisfy the shorter
        // one (e.g. "send" alone never appears as a prefix match for
        // "send_w" in the disassembly text — each line has exactly one
        // mnemonic token).
        !corpus
            .lines()
            .any(|line| line.split_whitespace().nth(1).is_some_and(|tok| tok == *m))
    });
    assert!(
        mnemonics.is_empty(),
        "opcodes never emitted anywhere in tests/golden/*.bc.expected: {mnemonics:?}"
    );
}

#[test]
fn g_full_disasm() {
    let mut vm = common::test_vm();
    let src = std::fs::read_to_string(golden_dir().join("g_full.mst")).unwrap();
    load_source(&mut vm, &src);

    let base = klass_named(&mut vm, "GFullBase");
    let full = klass_named(&mut vm, "GFull");

    let mut text = String::new();
    {
        let m = method_named(&mut vm, base, "greet");
        disassemble_deep(&vm, m, &mut text);
    }
    for sel in [
        "isReady",
        "notReady",
        "nothing",
        "label",
        "setIv:",
        "getIv",
        "bumpTotal",
        "makeAdder:",
        "runBlock",
        "choose:",
        "checkOr:with:",
        "countTo:",
        "greet",
    ] {
        let m = method_named(&mut vm, full, sel);
        disassemble_deep(&vm, m, &mut text);
    }
    check_golden("g_full", &text);
}

#[test]
fn g_full_parses_as_class_def_items() {
    let src = std::fs::read_to_string(golden_dir().join("g_full.mst")).unwrap();
    let items = parser::parse_file(&src).unwrap();
    assert_eq!(items.len(), 2);
    assert!(items.iter().all(|i| matches!(i, TopItem::ClassDef(_))));
}

/// SPEC end-to-end acceptance gate item 3: `macvm run
/// tests/golden/point_demo.mst` prints the expected transcript.
#[test]
fn point_demo_transcript() {
    let mut vm = common::test_vm();
    let src = std::fs::read_to_string(golden_dir().join("point_demo.mst")).unwrap();
    let buf = OutputBuffer::new();
    vm.out = Box::new(buf.clone());
    load_source(&mut vm, &src);
    let expected =
        std::fs::read_to_string(golden_dir().join("point_demo.expected")).expect("read golden");
    assert_eq!(buf.as_string(), expected);
}

/// `klass` must be freshly derived at the call site (not cached across a
/// previous `install_prim` call — under MACVM_GC_STRESS every `finish`
/// inside moves it). It rides in a handle across this function's own
/// allocations, and the selector is re-interned after them.
fn install_prim(vm: &mut VmState, klass: KlassOop, name: &[u8], argc: usize, prim: i64) {
    let scope = macvm::memory::handles::HandleScope::enter(vm);
    let klass_h = scope.handle(vm, klass);
    let sel = vm.universe.intern(name);
    let mut b = macvm::bytecode::BytecodeBuilder::new();
    b.push_self();
    b.ret_self();
    let m = b.finish(vm, sel, argc, 0);
    m.set_primitive(prim);
    let sel = vm.universe.intern(name);
    macvm::runtime::lookup::install_method(vm, klass_h.get(vm), sel, m);
}

/// Acceptance gate item 4: an S4 golden program (the "counter closure" —
/// a shared mutable Context capture outliving its creating frame,
/// `tests/it_blocks.rs::counter_closure`) re-expressed in source produces
/// the same shape of result as the hand-assembled original.
#[test]
fn s4_counter_closure_reexpressed() {
    let mut vm = common::test_vm();
    load_source(
        &mut vm,
        "Object subclass: S4Counter [\n\
         \x20   make [ | n counter | n := 0. counter := [ n := n + 1. n ]. ^counter ]\n\
         ]\n\
         Object subclass: S4CounterUse [\n\
         \x20   run: aCounter [ | a b c | a := aCounter value. b := aCounter value. c := aCounter value. ^(a * 100) + (b * 10) + c ]\n\
         ]\n",
    );
    // Universe klass fields are re-read before EACH call — a local cached
    // across a previous install_prim is stale under MACVM_GC_STRESS.
    let smi_klass = vm.universe.smi_klass;
    install_prim(&mut vm, smi_klass, b"+", 1, 1);
    let smi_klass = vm.universe.smi_klass;
    install_prim(&mut vm, smi_klass, b"*", 1, 3);
    let closure_klass = vm.universe.closure_klass;
    install_prim(&mut vm, closure_klass, b"value", 0, 50);

    // Every oop that crosses an allocating call below rides in a handle
    // (MACVM_GC_STRESS moves everything on every allocation).
    let scope = macvm::memory::handles::HandleScope::enter(&mut vm);
    let counter_klass = klass_named(&mut vm, "S4Counter");
    let make_sel = vm.universe.intern(b"make");
    let make_m = macvm::runtime::lookup::lookup(&mut vm, counter_klass, make_sel).unwrap();
    let make_m_h = scope.handle(&mut vm, make_m);
    let recv = macvm::memory::alloc::alloc_slots(&mut vm, counter_klass).oop();
    let make_m = make_m_h.get(&vm);
    let counter = macvm::interpreter::run_method(&mut vm, make_m, recv, &[]);
    let counter_h = scope.handle(&mut vm, counter);

    let use_klass = klass_named(&mut vm, "S4CounterUse");
    let run_sel = vm.universe.intern(b"run:");
    let run_m = macvm::runtime::lookup::lookup(&mut vm, use_klass, run_sel).unwrap();
    let run_m_h = scope.handle(&mut vm, run_m);
    let use_recv = macvm::memory::alloc::alloc_slots(&mut vm, use_klass).oop();
    let arg = counter_h.get(&vm);
    let run_m = run_m_h.get(&vm);
    let result = macvm::interpreter::run_method(&mut vm, run_m, use_recv, &[arg]);
    assert_eq!(
        result,
        macvm::oops::smi::SmallInt::new(123).oop(),
        "shared mutable Context capture must persist across 3 `value` sends: 1,2,3 -> 123"
    );
}

/// Acceptance gate item 4: S4's gate-3 golden — an NLR through two nested
/// `ensure:`-marked frames, handlers running innermost-first before the value
/// is delivered — re-expressed in source. (The original hand-assembled
/// `tests/it_nlr.rs` was deleted in S7.5 as GC-unsafe; this source form and
/// `world/tests/16_dispatch_tests.mst`'s `testEnsureOrderThroughTwoFrames`
/// are its GC-safe replacements.)
#[test]
fn s4_nlr_ensure_reexpressed() {
    let mut vm = common::test_vm();
    load_source(
        &mut vm,
        "Object subclass: S4Nlr [\n\
         \x20   run [ ^[[^42] ensure: [self trace: 'inner']] ensure: [self trace: 'outer'] ]\n\
         \x20   trace: aString [ <primitive: 91> ^self ]\n\
         ]\n\
         Object subclass: S4NlrDriver [\n\
         \x20   go: anNlr [ | r | r := anNlr run. self trace: 'done'. ^r ]\n\
         \x20   trace: aString [ <primitive: 91> ^self ]\n\
         ]\n",
    );
    let closure_klass = vm.universe.closure_klass;
    install_prim(&mut vm, closure_klass, b"ensure:", 1, 60);

    // Handles for everything crossing an allocation (MACVM_GC_STRESS).
    let scope = macvm::memory::handles::HandleScope::enter(&mut vm);
    let nlr_klass = klass_named(&mut vm, "S4Nlr");
    let nlr_recv = macvm::memory::alloc::alloc_slots(&mut vm, nlr_klass).oop();
    let nlr_recv_h = scope.handle(&mut vm, nlr_recv);
    let driver_klass = klass_named(&mut vm, "S4NlrDriver");
    let driver_recv = macvm::memory::alloc::alloc_slots(&mut vm, driver_klass).oop();
    let driver_recv_h = scope.handle(&mut vm, driver_recv);

    let buf = OutputBuffer::new();
    vm.out = Box::new(buf.clone());
    let driver_klass = klass_named(&mut vm, "S4NlrDriver");
    let go_sel = vm.universe.intern(b"go:");
    let go_m = macvm::runtime::lookup::lookup(&mut vm, driver_klass, go_sel).unwrap();
    let recv = driver_recv_h.get(&vm);
    let arg = nlr_recv_h.get(&vm);
    let result = macvm::interpreter::run_method(&mut vm, go_m, recv, &[arg]);

    assert_eq!(result, macvm::oops::smi::SmallInt::new(42).oop());
    assert_eq!(
        buf.as_string(),
        "innerouterdone",
        "ensure: handlers must run innermost-first, before the NLR value is delivered"
    );
}

/// S6: `Object`'s real superclass is the `nil` oop itself (root of the
/// hierarchy) — reopening it must accept the special `nil subclass:
/// Object [...]` spelling and correctly compare against that nil
/// superclass, not attempt to resolve "nil" as a bound klass global.
#[test]
fn reopen_object_via_nil_superclass() {
    let mut vm = common::test_vm();
    load_source(&mut vm, "nil subclass: Object [ answer [ ^42 ] ]\n");
    // Allocate the receiver first, then re-fetch every oop the run needs
    // AFTER it: under `MACVM_GC_STRESS=1` every allocation scavenges and moves
    // young objects, so any bare `KlassOop`/`MethodOop` captured before
    // `alloc_slots` dangles (the bare-oop-across-an-allocation bug class, SPEC
    // §7.6.1). `klass_named`/`intern`/`lookup` don't allocate (the names were
    // interned by `load_source`), so nothing moves between here and
    // `run_method`, and `recv` (allocated last among heap ops) stays valid.
    let object_klass = klass_named(&mut vm, "Object");
    let recv = macvm::memory::alloc::alloc_slots(&mut vm, object_klass).oop();
    let object_klass = klass_named(&mut vm, "Object"); // re-fetch: the alloc moved it
    let sel = vm.universe.intern(b"answer");
    let m = macvm::runtime::lookup::lookup(&mut vm, object_klass, sel).unwrap();
    let result = macvm::interpreter::run_method(&mut vm, m, recv, &[]);
    assert_eq!(result, macvm::oops::smi::SmallInt::new(42).oop());
}
