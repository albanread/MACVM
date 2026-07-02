//! The MACVM bytecode instruction set (SPEC §4.1–§4.2): opcode constants
//! (the wire format — external contract, do not renumber), the decoded
//! `Instr` form, and `decode_at`. The interpreter's hot loop does NOT go
//! through `Instr` — it matches the raw opcode byte and reads operands
//! inline (SPEC §5.2); `Instr`/`decode_at` serve the disassembler, the
//! builder's verifier, and S10's CFG decoder.

use crate::oops::wrappers::MethodOop;

pub const OP_PUSH_SELF: u8 = 0x00;
pub const OP_PUSH_NIL: u8 = 0x01;
pub const OP_PUSH_TRUE: u8 = 0x02;
pub const OP_PUSH_FALSE: u8 = 0x03;
pub const OP_PUSH_SMI_I8: u8 = 0x04; // <i8>
pub const OP_PUSH_LITERAL: u8 = 0x05; // <u8>
pub const OP_PUSH_TEMP: u8 = 0x06; // <u8>  unified arg/temp
pub const OP_STORE_TEMP: u8 = 0x07; // <u8>
pub const OP_STORE_TEMP_POP: u8 = 0x08; // <u8>
pub const OP_PUSH_INSTVAR: u8 = 0x09; // <u8>
pub const OP_STORE_INSTVAR_POP: u8 = 0x0A; // <u8>
pub const OP_PUSH_GLOBAL: u8 = 0x0B; // <u8>  literal = Association
pub const OP_STORE_GLOBAL_POP: u8 = 0x0C; // <u8>
pub const OP_POP: u8 = 0x0D;
pub const OP_DUP: u8 = 0x0E;
pub const OP_PUSH_CTX_TEMP: u8 = 0x0F; // <u8 depth> <u8 idx>   S4 exec
pub const OP_STORE_CTX_TEMP_POP: u8 = 0x10; // <u8 depth> <u8 idx>   S4 exec
pub const OP_PUSH_CLOSURE: u8 = 0x11; // <u8 lit> <u8 ncopied> S4 exec
pub const OP_PUSH_LITERAL_W: u8 = 0x12; // <u16>
pub const OP_SEND: u8 = 0x20; // <u8 ic>               S3 exec
pub const OP_SEND_SUPER: u8 = 0x21; // <u8 ic>               S3
pub const OP_SEND_W: u8 = 0x22; // <u16 ic>              S3
pub const OP_SEND_SUPER_W: u8 = 0x23; // <u16 ic>              S3
pub const OP_JUMP_FWD: u8 = 0x30; // <u16>
pub const OP_JUMP_BACK: u8 = 0x31; // <u16>
pub const OP_BR_TRUE_FWD: u8 = 0x32; // <u16>
pub const OP_BR_FALSE_FWD: u8 = 0x33; // <u16>
pub const OP_RETURN_TOS: u8 = 0x40;
pub const OP_RETURN_SELF: u8 = 0x41;
pub const OP_BLOCK_RETURN_TOS: u8 = 0x42; //                       S4
pub const OP_NLR_TOS: u8 = 0x43; //                       S4

/// The decoded form of one instruction. Not used by the interpreter's hot
/// loop (SPEC §5.2) — this is for the disassembler, the builder's
/// verifier, and (S10) the CFG decoder.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Instr {
    PushSelf,
    PushNil,
    PushTrue,
    PushFalse,
    PushSmi(i8),
    PushLiteral(u16), // widened; also carries the wide (0x12) form
    PushTemp(u8),
    StoreTemp(u8),
    StoreTempPop(u8),
    PushInstvar(u8),
    StoreInstvarPop(u8),
    PushGlobal(u8),
    StoreGlobalPop(u8),
    Pop,
    Dup,
    PushCtxTemp { depth: u8, idx: u8 },
    StoreCtxTempPop { depth: u8, idx: u8 },
    PushClosure { lit: u8, ncopied: u8 },
    Send { ic: u16, super_: bool },
    JumpFwd(u16),
    JumpBack(u16),
    BrTrueFwd(u16),
    BrFalseFwd(u16),
    ReturnTos,
    ReturnSelf,
    BlockReturnTos,
    NlrTos,
}

/// Reused by the interpreter's hot loop (SPEC §5.2 reads operands inline,
/// not via `decode_at`) — kept as one implementation, not duplicated.
#[inline]
pub(crate) fn read_u16(m: MethodOop, bci: usize) -> u16 {
    let lo = m.bytecode_byte(bci) as u16;
    let hi = m.bytecode_byte(bci + 1) as u16;
    lo | (hi << 8)
}

/// Decode the instruction starting at `bci`. Returns `(instr, next_bci)`.
/// Reads bytes one at a time via `MethodOop::bytecode_byte` — never takes a
/// slice of heap memory. An unknown opcode panics: bytecode is VM-generated
/// and immutable, so an undefined opcode is a VM bug, not guest input.
pub fn decode_at(m: MethodOop, bci: usize) -> (Instr, usize) {
    let op = m.bytecode_byte(bci);
    match op {
        OP_PUSH_SELF => (Instr::PushSelf, bci + 1),
        OP_PUSH_NIL => (Instr::PushNil, bci + 1),
        OP_PUSH_TRUE => (Instr::PushTrue, bci + 1),
        OP_PUSH_FALSE => (Instr::PushFalse, bci + 1),
        OP_PUSH_SMI_I8 => (Instr::PushSmi(m.bytecode_byte(bci + 1) as i8), bci + 2),
        OP_PUSH_LITERAL => (Instr::PushLiteral(m.bytecode_byte(bci + 1) as u16), bci + 2),
        OP_PUSH_TEMP => (Instr::PushTemp(m.bytecode_byte(bci + 1)), bci + 2),
        OP_STORE_TEMP => (Instr::StoreTemp(m.bytecode_byte(bci + 1)), bci + 2),
        OP_STORE_TEMP_POP => (Instr::StoreTempPop(m.bytecode_byte(bci + 1)), bci + 2),
        OP_PUSH_INSTVAR => (Instr::PushInstvar(m.bytecode_byte(bci + 1)), bci + 2),
        OP_STORE_INSTVAR_POP => (Instr::StoreInstvarPop(m.bytecode_byte(bci + 1)), bci + 2),
        OP_PUSH_GLOBAL => (Instr::PushGlobal(m.bytecode_byte(bci + 1)), bci + 2),
        OP_STORE_GLOBAL_POP => (Instr::StoreGlobalPop(m.bytecode_byte(bci + 1)), bci + 2),
        OP_POP => (Instr::Pop, bci + 1),
        OP_DUP => (Instr::Dup, bci + 1),
        OP_PUSH_CTX_TEMP => (
            Instr::PushCtxTemp {
                depth: m.bytecode_byte(bci + 1),
                idx: m.bytecode_byte(bci + 2),
            },
            bci + 3,
        ),
        OP_STORE_CTX_TEMP_POP => (
            Instr::StoreCtxTempPop {
                depth: m.bytecode_byte(bci + 1),
                idx: m.bytecode_byte(bci + 2),
            },
            bci + 3,
        ),
        OP_PUSH_CLOSURE => (
            Instr::PushClosure {
                lit: m.bytecode_byte(bci + 1),
                ncopied: m.bytecode_byte(bci + 2),
            },
            bci + 3,
        ),
        OP_PUSH_LITERAL_W => (Instr::PushLiteral(read_u16(m, bci + 1)), bci + 3),
        OP_SEND => (
            Instr::Send {
                ic: m.bytecode_byte(bci + 1) as u16,
                super_: false,
            },
            bci + 2,
        ),
        OP_SEND_SUPER => (
            Instr::Send {
                ic: m.bytecode_byte(bci + 1) as u16,
                super_: true,
            },
            bci + 2,
        ),
        OP_SEND_W => (
            Instr::Send {
                ic: read_u16(m, bci + 1),
                super_: false,
            },
            bci + 3,
        ),
        OP_SEND_SUPER_W => (
            Instr::Send {
                ic: read_u16(m, bci + 1),
                super_: true,
            },
            bci + 3,
        ),
        OP_JUMP_FWD => (Instr::JumpFwd(read_u16(m, bci + 1)), bci + 3),
        OP_JUMP_BACK => (Instr::JumpBack(read_u16(m, bci + 1)), bci + 3),
        OP_BR_TRUE_FWD => (Instr::BrTrueFwd(read_u16(m, bci + 1)), bci + 3),
        OP_BR_FALSE_FWD => (Instr::BrFalseFwd(read_u16(m, bci + 1)), bci + 3),
        OP_RETURN_TOS => (Instr::ReturnTos, bci + 1),
        OP_RETURN_SELF => (Instr::ReturnSelf, bci + 1),
        OP_BLOCK_RETURN_TOS => (Instr::BlockReturnTos, bci + 1),
        OP_NLR_TOS => (Instr::NlrTos, bci + 1),
        bad => panic!("decode_at: undefined opcode {bad:#04x} at bci {bci}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcode_values_pinned() {
        assert_eq!(OP_PUSH_SELF, 0x00);
        assert_eq!(OP_PUSH_NIL, 0x01);
        assert_eq!(OP_PUSH_TRUE, 0x02);
        assert_eq!(OP_PUSH_FALSE, 0x03);
        assert_eq!(OP_PUSH_SMI_I8, 0x04);
        assert_eq!(OP_PUSH_LITERAL, 0x05);
        assert_eq!(OP_PUSH_TEMP, 0x06);
        assert_eq!(OP_STORE_TEMP, 0x07);
        assert_eq!(OP_STORE_TEMP_POP, 0x08);
        assert_eq!(OP_PUSH_INSTVAR, 0x09);
        assert_eq!(OP_STORE_INSTVAR_POP, 0x0A);
        assert_eq!(OP_PUSH_GLOBAL, 0x0B);
        assert_eq!(OP_STORE_GLOBAL_POP, 0x0C);
        assert_eq!(OP_POP, 0x0D);
        assert_eq!(OP_DUP, 0x0E);
        assert_eq!(OP_PUSH_CTX_TEMP, 0x0F);
        assert_eq!(OP_STORE_CTX_TEMP_POP, 0x10);
        assert_eq!(OP_PUSH_CLOSURE, 0x11);
        assert_eq!(OP_PUSH_LITERAL_W, 0x12);
        assert_eq!(OP_SEND, 0x20);
        assert_eq!(OP_SEND_SUPER, 0x21);
        assert_eq!(OP_SEND_W, 0x22);
        assert_eq!(OP_SEND_SUPER_W, 0x23);
        assert_eq!(OP_JUMP_FWD, 0x30);
        assert_eq!(OP_JUMP_BACK, 0x31);
        assert_eq!(OP_BR_TRUE_FWD, 0x32);
        assert_eq!(OP_BR_FALSE_FWD, 0x33);
        assert_eq!(OP_RETURN_TOS, 0x40);
        assert_eq!(OP_RETURN_SELF, 0x41);
        assert_eq!(OP_BLOCK_RETURN_TOS, 0x42);
        assert_eq!(OP_NLR_TOS, 0x43);
    }

    fn test_vm() -> crate::runtime::VmState {
        crate::runtime::VmState::with_options(crate::runtime::VmOptions {
            heap_mib: 64,
            trace: Default::default(),
        })
    }

    fn packed_method(vm: &mut crate::runtime::VmState, bytes: &[u8]) -> MethodOop {
        let m = crate::memory::alloc::alloc_method(vm, bytes.len());
        for (i, &b) in bytes.iter().enumerate() {
            m.set_bytecode_byte(i, b);
        }
        m
    }

    #[test]
    fn decode_le_operands() {
        let mut vm = test_vm();
        // jump_fwd with distance 0x0102 encodes bytes `30 02 01`.
        let m = packed_method(&mut vm, &[OP_JUMP_FWD, 0x02, 0x01, OP_RETURN_SELF]);
        let (instr, next) = decode_at(m, 0);
        assert_eq!(instr, Instr::JumpFwd(0x0102));
        assert_eq!(next, 3);
    }

    #[test]
    fn decode_i8_sign() {
        let mut vm = test_vm();
        let m = packed_method(
            &mut vm,
            &[
                OP_PUSH_SMI_I8,
                0xFF,
                OP_PUSH_SMI_I8,
                0x80,
                OP_PUSH_SMI_I8,
                0x7F,
                OP_RETURN_SELF,
            ],
        );
        assert_eq!(decode_at(m, 0).0, Instr::PushSmi(-1));
        assert_eq!(decode_at(m, 2).0, Instr::PushSmi(-128));
        assert_eq!(decode_at(m, 4).0, Instr::PushSmi(127));
    }

    #[test]
    fn decode_roundtrip_all() {
        let mut vm = test_vm();
        // One instruction of each S2-decodable shape, back to back; assert
        // Instr fields and next_bci agree with the operand-width table.
        let bytes: &[u8] = &[
            OP_PUSH_SELF,  // 1 byte
            OP_PUSH_NIL,   // 1
            OP_PUSH_TRUE,  // 1
            OP_PUSH_FALSE, // 1
            OP_PUSH_SMI_I8,
            5, // 2
            OP_PUSH_LITERAL,
            3, // 2
            OP_PUSH_TEMP,
            2, // 2
            OP_STORE_TEMP,
            1, // 2
            OP_STORE_TEMP_POP,
            0, // 2
            OP_PUSH_INSTVAR,
            1, // 2
            OP_STORE_INSTVAR_POP,
            1, // 2
            OP_PUSH_GLOBAL,
            0, // 2
            OP_STORE_GLOBAL_POP,
            0,      // 2
            OP_POP, // 1
            OP_DUP, // 1
            OP_PUSH_CTX_TEMP,
            1,
            2, // 3
            OP_STORE_CTX_TEMP_POP,
            1,
            2, // 3
            OP_PUSH_CLOSURE,
            4,
            1, // 3
            OP_PUSH_LITERAL_W,
            0x00,
            0x01, // 3
            OP_SEND,
            0, // 2
            OP_SEND_SUPER,
            0, // 2
            OP_SEND_W,
            0x00,
            0x01, // 3
            OP_SEND_SUPER_W,
            0x00,
            0x01, // 3
            OP_JUMP_FWD,
            0,
            0, // 3
            OP_JUMP_BACK,
            0,
            0, // 3
            OP_BR_TRUE_FWD,
            0,
            0, // 3
            OP_BR_FALSE_FWD,
            0,
            0,                   // 3
            OP_RETURN_TOS,       // 1
            OP_RETURN_SELF,      // 1
            OP_BLOCK_RETURN_TOS, // 1
            OP_NLR_TOS,          // 1
        ];
        let m = packed_method(&mut vm, bytes);

        let expected: &[(Instr, usize)] = &[
            (Instr::PushSelf, 1),
            (Instr::PushNil, 1),
            (Instr::PushTrue, 1),
            (Instr::PushFalse, 1),
            (Instr::PushSmi(5), 2),
            (Instr::PushLiteral(3), 2),
            (Instr::PushTemp(2), 2),
            (Instr::StoreTemp(1), 2),
            (Instr::StoreTempPop(0), 2),
            (Instr::PushInstvar(1), 2),
            (Instr::StoreInstvarPop(1), 2),
            (Instr::PushGlobal(0), 2),
            (Instr::StoreGlobalPop(0), 2),
            (Instr::Pop, 1),
            (Instr::Dup, 1),
            (Instr::PushCtxTemp { depth: 1, idx: 2 }, 3),
            (Instr::StoreCtxTempPop { depth: 1, idx: 2 }, 3),
            (Instr::PushClosure { lit: 4, ncopied: 1 }, 3),
            (Instr::PushLiteral(0x0100), 3),
            (
                Instr::Send {
                    ic: 0,
                    super_: false,
                },
                2,
            ),
            (
                Instr::Send {
                    ic: 0,
                    super_: true,
                },
                2,
            ),
            (
                Instr::Send {
                    ic: 0x0100,
                    super_: false,
                },
                3,
            ),
            (
                Instr::Send {
                    ic: 0x0100,
                    super_: true,
                },
                3,
            ),
            (Instr::JumpFwd(0), 3),
            (Instr::JumpBack(0), 3),
            (Instr::BrTrueFwd(0), 3),
            (Instr::BrFalseFwd(0), 3),
            (Instr::ReturnTos, 1),
            (Instr::ReturnSelf, 1),
            (Instr::BlockReturnTos, 1),
            (Instr::NlrTos, 1),
        ];

        let mut bci = 0usize;
        for (want_instr, width) in expected {
            let (instr, next) = decode_at(m, bci);
            assert_eq!(instr, *want_instr, "at bci {bci}");
            assert_eq!(next - bci, *width, "width at bci {bci}");
            bci = next;
        }
        assert_eq!(bci, bytes.len());
    }
}
