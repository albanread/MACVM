//! The disassembler — pinned golden format (goldens break if it drifts):
//!
//! ```text
//! method #<selector> argc=<n> ntemps=<n> prim=<n> flags=<has_ctx?,is_block?>
//!   <bci>: <mnemonic> <operands>        ; jumps annotated "-> <target-bci>"
//! literals:
//!   <i>: <print_oop>
//! ics: <count>
//! ```
//!
//! Mnemonics are the SPEC §4.1/§4.2 names verbatim. Implemented over
//! `decode_at` — the disassembler is the decode test.

use std::fmt::Write as _;

use crate::memory::Universe;
use crate::oops::wrappers::MethodOop;

use super::opcode::*;
use super::Instr;

fn mnemonic(op: u8) -> &'static str {
    match op {
        OP_PUSH_SELF => "push_self",
        OP_PUSH_NIL => "push_nil",
        OP_PUSH_TRUE => "push_true",
        OP_PUSH_FALSE => "push_false",
        OP_PUSH_SMI_I8 => "push_smi",
        OP_PUSH_LITERAL => "push_literal",
        OP_PUSH_TEMP => "push_temp",
        OP_STORE_TEMP => "store_temp",
        OP_STORE_TEMP_POP => "store_temp_pop",
        OP_PUSH_INSTVAR => "push_instvar",
        OP_STORE_INSTVAR_POP => "store_instvar_pop",
        OP_PUSH_GLOBAL => "push_global",
        OP_STORE_GLOBAL_POP => "store_global_pop",
        OP_POP => "pop",
        OP_DUP => "dup",
        OP_PUSH_CTX_TEMP => "push_ctx_temp",
        OP_STORE_CTX_TEMP_POP => "store_ctx_temp_pop",
        OP_PUSH_CLOSURE => "push_closure",
        OP_PUSH_LITERAL_W => "push_literal_w",
        OP_SEND => "send",
        OP_SEND_SUPER => "send_super",
        OP_SEND_W => "send_w",
        OP_SEND_SUPER_W => "send_super_w",
        OP_JUMP_FWD => "jump_fwd",
        OP_JUMP_BACK => "jump_back",
        OP_BR_TRUE_FWD => "br_true_fwd",
        OP_BR_FALSE_FWD => "br_false_fwd",
        OP_RETURN_TOS => "return_tos",
        OP_RETURN_SELF => "return_self",
        OP_BLOCK_RETURN_TOS => "block_return_tos",
        OP_NLR_TOS => "nlr_tos",
        other => panic!("disasm: undefined opcode {other:#04x}"),
    }
}

pub fn disassemble(u: &Universe, m: MethodOop) -> String {
    let mut out = String::new();

    let selector = crate::memory::print_oop(u, m.selector());
    let mut flag_names = Vec::new();
    if m.has_ctx() {
        flag_names.push("has_ctx");
    }
    if m.is_block() {
        flag_names.push("is_block");
    }
    writeln!(
        out,
        "method {} argc={} ntemps={} prim={} flags={}",
        selector,
        m.argc(),
        m.ntemps(),
        m.primitive(),
        flag_names.join(",")
    )
    .unwrap();

    let literals = m.literals();
    let mut bci = 0usize;
    while bci < m.bytecode_len() {
        let op = m.bytecode_byte(bci);
        let (instr, next) = decode_at(m, bci);
        let name = mnemonic(op);
        write!(out, "  {bci}: {name}").unwrap();
        match instr {
            Instr::PushSelf
            | Instr::PushNil
            | Instr::PushTrue
            | Instr::PushFalse
            | Instr::Pop
            | Instr::Dup
            | Instr::ReturnTos
            | Instr::ReturnSelf
            | Instr::BlockReturnTos
            | Instr::NlrTos => {}
            Instr::PushSmi(v) => write!(out, " {v}").unwrap(),
            Instr::PushLiteral(idx) => {
                write!(out, " {idx}").unwrap();
                if (idx as usize) < literals.len() {
                    let lit_str = crate::memory::print_oop(u, literals.at(idx as usize));
                    write!(out, "; {lit_str}").unwrap();
                }
            }
            Instr::PushTemp(t) | Instr::StoreTemp(t) | Instr::StoreTempPop(t) => {
                write!(out, " {t}").unwrap()
            }
            Instr::PushInstvar(i) | Instr::StoreInstvarPop(i) => write!(out, " {i}").unwrap(),
            Instr::PushGlobal(idx) | Instr::StoreGlobalPop(idx) => {
                write!(out, " {idx}").unwrap();
                if (idx as usize) < literals.len() {
                    let lit_str = crate::memory::print_oop(u, literals.at(idx as usize));
                    write!(out, "; {lit_str}").unwrap();
                }
            }
            Instr::PushCtxTemp { depth, idx } | Instr::StoreCtxTempPop { depth, idx } => {
                write!(out, " {depth} {idx}").unwrap()
            }
            Instr::PushClosure { lit, ncopied } => write!(out, " {lit} {ncopied}").unwrap(),
            Instr::Send { ic, .. } => {
                write!(out, " {ic}").unwrap();
                // P12 (sprint_s05_detail.md Pitfalls): print both the site
                // index and its selector — an off-by-4 (word vs. site
                // index) reads the wrong site's selector and "works"
                // alarmingly often; goldens must be able to pin this.
                let ics = m.ics();
                let base = ic as usize * crate::oops::layout::IC_STRIDE;
                if base + crate::oops::layout::IC_SEL_OFFSET < ics.len() {
                    let sel_str = crate::memory::print_oop(
                        u,
                        ics.at(base + crate::oops::layout::IC_SEL_OFFSET),
                    );
                    write!(out, "; {sel_str}").unwrap();
                }
            }
            Instr::JumpFwd(d) | Instr::BrTrueFwd(d) | Instr::BrFalseFwd(d) => {
                let target = next + d as usize;
                write!(out, " {d} -> {target}").unwrap();
            }
            Instr::JumpBack(d) => {
                let target = next - d as usize;
                write!(out, " {d} -> {target}").unwrap();
            }
        }
        writeln!(out).unwrap();
        bci = next;
    }

    writeln!(out, "literals:").unwrap();
    for i in 0..literals.len() {
        let s = crate::memory::print_oop(u, literals.at(i));
        writeln!(out, "  {i}: {s}").unwrap();
    }

    writeln!(out, "ics: {}", m.ics().len() / 4).unwrap();

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::BytecodeBuilder;
    use crate::runtime::vm_state::{VmOptions, VmState};

    fn test_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            gc_stress_full_period: None,
            eden_kb: None,
            jit: crate::runtime::JitMode::Off,
        })
    }

    #[test]
    fn disasm_header_line() {
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        b.ret_self();
        let sel = vm.universe.intern(b"foo");
        let m = b.finish(&mut vm, sel, 1, 2);
        let text = disassemble(&vm.universe, m);
        let first_line = text.lines().next().unwrap();
        assert_eq!(first_line, "method #foo argc=1 ntemps=2 prim=0 flags=");
    }

    #[test]
    fn disasm_annotates_jumps() {
        // Reconstructs the pinned k_bool_loop kernel from tests_s02.md and
        // checks the two annotated jump lines.
        let mut vm = test_vm();
        let mut b = BytecodeBuilder::new();
        let loop_head = b.new_label();
        let exit = b.new_label();
        b.push_true(); // 0
        b.store_temp_pop(0); // 1..3
        b.bind(loop_head); // bci 3
        b.push_temp(0); // 3..5
        b.br_false_fwd(exit); // 5..8
        b.push_false(); // 8
        b.store_temp_pop(0); // 9..11
        b.jump_back(loop_head); // 11..14
        b.bind(exit); // bci 14
        b.push_smi_i8(42); // 14..16
        b.ret_tos(); // 16
        let sel = vm.universe.intern(b"loop");
        let m = b.finish(&mut vm, sel, 0, 1);
        let text = disassemble(&vm.universe, m);

        assert!(
            text.contains("11: jump_back 11 -> 3\n"),
            "text was:\n{text}"
        );
        let br_line = text
            .lines()
            .find(|l| l.contains("br_false_fwd"))
            .expect("br_false_fwd line present");
        assert!(br_line.ends_with("-> 14"), "line was: {br_line}");
    }
}
