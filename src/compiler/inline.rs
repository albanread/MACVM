//! S14 step 3: the inliner's decision surface (SPEC §8.4, sprint_s14_detail
//! "The inliner" ~line 160 and algorithm A2 ~line 233). Maps one send site's
//! [`SiteFeedback`] onto a codegen decision. This is the FIRST speculative
//! codegen step: it only produces `Trap` (for an `Untaken` site, Self's lazy
//! cold path — an uncommon trap that re-executes the send interpreted and warms
//! its IC for the *next* compilation) and `Call` (a plain compiled send, S11).
//! Method and polymorphic INLINING (the `Inline` / `DominantWithSlowPath`
//! arms) arrive in S14 steps 4/6, which grow [`decide`] without changing its
//! shape.
//!
//! **Raw oops, not `Handle`.** The sprint doc's `InlineDecision` uses
//! `Handle<MethodOop>`; `compile_method` runs in a NO-GC window (see
//! `feedback.rs`'s own documented deviation and `driver::compile_method`'s
//! "allocates nothing on the Smalltalk heap" invariant), so a raw
//! `MethodOop`/`KlassOop` read here stays valid for the whole compile. If a
//! later step ever compiles across a collection, these become `Handle`s then.

use crate::bytecode::opcode::{decode_at, Instr};
use crate::compiler::feedback::SiteFeedback;
use crate::oops::wrappers::{KlassOop, MethodOop};

/// S14 step 4 (SPEC §8.1/§8.4): the inlining budget at one recompilation level.
/// All tunables. `per_call_cost` bounds a single inlinee's [`inline_cost`];
/// `total_bytes` the cumulative inlined bytecode across one compilation;
/// `max_depth` the inline-chain depth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineBudget {
    pub per_call_cost: u32,
    pub total_bytes: u32,
    pub max_depth: u32,
}

/// The budget for recompilation level `level` (1..=4, SPEC §8.1). Higher levels
/// (reached only after the effectiveness/version gates, S14 step 8) inline more
/// aggressively. Clamps `level >= 4` to the top row.
pub fn budget_for_level(level: u8) -> InlineBudget {
    match level {
        0 | 1 => InlineBudget {
            per_call_cost: 30,
            total_bytes: 300,
            max_depth: 4,
        },
        2 => InlineBudget {
            per_call_cost: 50,
            total_bytes: 600,
            max_depth: 6,
        },
        3 => InlineBudget {
            per_call_cost: 80,
            total_bytes: 1200,
            max_depth: 8,
        },
        _ => InlineBudget {
            per_call_cost: 120,
            total_bytes: 2400,
            max_depth: 8,
        },
    }
}

/// Cost of inlining `callee` (SPEC §8.4 cost model). Rules, first match wins:
/// 1. a `primitive != 0` method — its bytecode is only the failure fallback,
///    the real work is the primitive, so it inlines for a flat **4**;
/// 2. an accessor (`push_instvar; ^tos`) or quick return (`push_*; ^tos`, or a
///    bare `^self`) — **2**;
/// 3. otherwise the bytecode length in bytes.
pub fn inline_cost(callee: MethodOop) -> u32 {
    if callee.primitive() != 0 {
        return 4;
    }
    if is_quick_return(callee) {
        return 2;
    }
    callee.bytecode_len() as u32
}

/// S14 step 4b: a method is a **leaf** iff its bytecode contains NO `Instr::Send`
/// at all (super or not). A leaf has no `CallSend`, no smi-inline overflow trap,
/// no allocation → NO safepoint inside its body once inlined → no deopt can
/// occur within it → the inliner needs NO `SenderLink` scope chain for it. This
/// is the exact class of callees step 4b splices inline (`^instvar`, `^self`,
/// `^arg`, `t := expr. ^t` with no sends); anything with even one send (e.g.
/// `^self + 1`, whose `+` is a send) is NOT a leaf and stays a plain `Call`.
///
/// Note this is STRICTLY the presence of a `Send` opcode — a smi-inlinable
/// send (`+`/`<`) is still a `Send` in the callee's own bytecode, so a method
/// containing one is not a leaf here. That is deliberately conservative for
/// step 4b: inlining a body that itself needs the smi-overflow trap (a
/// safepoint) is a later step's problem.
pub fn is_leaf(method: MethodOop) -> bool {
    let len = method.bytecode_len();
    let mut bci = 0;
    while bci < len {
        let (instr, next) = decode_at(method, bci);
        if matches!(instr, Instr::Send { .. }) {
            return false;
        }
        bci = next;
    }
    true
}

/// A method whose whole body is a single `push_*; ^tos` (accessor / quick
/// return) or a bare `^self` — the cost-2 shapes above.
fn is_quick_return(method: MethodOop) -> bool {
    let len = method.bytecode_len();
    if len == 0 {
        return false;
    }
    let (first, next) = decode_at(method, 0);
    match first {
        Instr::ReturnSelf => next == len,
        Instr::PushSelf
        | Instr::PushNil
        | Instr::PushTrue
        | Instr::PushFalse
        | Instr::PushSmi(_)
        | Instr::PushLiteral(_)
        | Instr::PushTemp(_)
        | Instr::PushInstvar(_)
        | Instr::PushGlobal(_) => {
            if next >= len {
                return false;
            }
            let (second, next2) = decode_at(method, next);
            matches!(second, Instr::ReturnTos) && next2 == len
        }
        _ => false,
    }
}

/// What to do at one send site (SPEC §8.4). Step 3 only ever produces `Trap`
/// and `Call`; the `Inline`/`DominantWithSlowPath` arms are declared now (the
/// full lattice) so steps 4-6 fill them without a type-signature churn.
#[derive(Clone, Debug)]
pub enum InlineDecision {
    /// Splice the callee's body into this compilation, guarded per `guard`
    /// (S14 step 4/6). Unused in step 3.
    Inline { callee: MethodOop, guard: GuardKind },
    /// Inline the dominant poly case behind a klass guard, with a real
    /// compiled send on the slow (`b.ne`) path (S14 step 6). Unused in step 3.
    DominantWithSlowPath {
        case_klass: KlassOop,
        case_method: MethodOop,
        guard: GuardKind,
    },
    /// A real compiled send (S11 compiled IC): `Mono`/`Poly`/`Mega` sites all
    /// map here in step 3 (inlining them is later steps).
    Call,
    /// An uncommon trap (`brk #0xDE00`, reexecute=true) for an `Untaken` site —
    /// the site never executed while interpreted, so there is no target to
    /// speculate on. The trap re-executes the send in the interpreter, which
    /// populates the IC for the next compilation (Self's lazy cold path).
    Trap,
}

/// How an inlined/speculated site proves its receiver's klass before running
/// the speculated body (SPEC §8.4, A6). Declared in full now; step 3 never
/// emits a guard (a `Trap` has no guarded fast path, a `Call` dispatches
/// dynamically), so only `None` is constructed here — `KlassTest`/`SmiTest`
/// come online with method/poly inlining (steps 4/6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardKind {
    /// Receiver klass statically known (self after the entry check, a literal,
    /// an inlined allocation's result, or a value that already passed a guard)
    /// — no runtime test needed.
    None,
    /// `cmp klass, pool-literal; b.ne cold` (heap-oop receivers).
    KlassTest,
    /// `tst rcvr, #3; b.ne cold` (smi receivers, SPEC §2.1's 2-bit tag).
    SmiTest,
}

/// Map a send site's observed feedback onto a codegen decision (A2). Step 3:
/// `Untaken` → `Trap` (compile the cold site as an uncommon trap instead of
/// blocking the whole method's compilation, which is what the pre-S14
/// `NoRetryLater` eligibility did); everything else → `Call` (a plain compiled
/// send — `Mono`/`Poly`/`Mega` INLINING is steps 4/6). Kept intentionally tiny;
/// later steps grow the `Mono`/`Poly` arms into real inlining decisions.
pub fn decide(feedback: &SiteFeedback) -> InlineDecision {
    match feedback {
        SiteFeedback::Untaken => InlineDecision::Trap,
        SiteFeedback::Mono { .. } | SiteFeedback::Poly { .. } | SiteFeedback::Mega => {
            InlineDecision::Call
        }
    }
}

/// S14 step 4b: the inlining decision proper (A2 item 2). A `Mono { klass,
/// method }` site whose callee is a **leaf** ([`is_leaf`]) and cheap enough
/// (`inline_cost(method) <= budget.per_call_cost`) becomes
/// `Inline { callee, guard }`, where the guard is:
///   - `SmiTest` when the observed `klass` is the smi klass (`smi_klass_bits`
///     is `vm.universe.smi_klass.oop().raw()`) — a tag test, not a klass load;
///   - `KlassTest` otherwise — reject-smi-then-compare-klass-word.
///
/// Everything else falls back to a plain `Call` (never a `Trap` here — an
/// `Untaken` site has no observed target to speculate on, so it stays the
/// step-3 caller's concern via [`decide`]; a non-leaf callee, an over-budget
/// leaf, a `Poly`, or a `Mega` all dispatch dynamically). Poly-dominant
/// inlining, non-leaf mono inlining, and static-klass guard elision are all
/// later S14 steps.
///
/// `smi_klass_bits`, not a `KlassOop`: this runs inside `compiler::inline`,
/// which (like `feedback.rs`) works in raw oops for the whole no-GC compile
/// window — the caller (`convert`) passes `vm.universe.smi_klass.oop().raw()`.
pub fn decide_with_budget(
    feedback: &SiteFeedback,
    budget: &InlineBudget,
    smi_klass_bits: u64,
) -> InlineDecision {
    match feedback {
        SiteFeedback::Mono { klass, method } => {
            // A PRIMITIVE method's real behaviour is the primitive itself; its
            // bytecode is only the failure fallback (`instVarAt:`, `+`, etc.).
            // Splicing that fallback inline would run the WRONG code — so a
            // primitive is never leaf-inlined here (primitive-INTRINSIC inlining
            // is a later step). `is_leaf` alone would accept it (a fallback with
            // no sends is send-free), hence this explicit guard.
            if method.primitive() == 0
                && is_leaf(*method)
                && inline_cost(*method) <= budget.per_call_cost
            {
                let guard = if klass.oop().raw() == smi_klass_bits {
                    GuardKind::SmiTest
                } else {
                    GuardKind::KlassTest
                };
                InlineDecision::Inline {
                    callee: *method,
                    guard,
                }
            } else {
                InlineDecision::Call
            }
        }
        // An Untaken site owns no observed target (the step-3 trap is `decide`'s
        // job, reached separately in `convert`); Poly/Mega dispatch dynamically.
        SiteFeedback::Untaken | SiteFeedback::Poly { .. } | SiteFeedback::Mega => {
            InlineDecision::Call
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::feedback::{FeedbackCase, SiteFeedback};
    use crate::oops::wrappers::{KlassOop, MethodOop};
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

    /// A trivial `MethodOop`/`KlassOop` pair to populate the non-`Untaken`
    /// feedback arms (their contents are irrelevant to `decide` in step 3).
    fn a_klass_and_method(vm: &mut VmState) -> (KlassOop, MethodOop) {
        let klass = vm.universe.smi_klass;
        let sel = vm.universe.intern(b"m");
        let mut b = crate::bytecode::builder::BytecodeBuilder::new();
        b.ret_self();
        let method = b.finish(vm, sel, 0, 0);
        (klass, method)
    }

    #[test]
    fn untaken_traps() {
        assert!(matches!(
            decide(&SiteFeedback::Untaken),
            InlineDecision::Trap
        ));
    }

    #[test]
    fn mono_calls() {
        let mut vm = test_vm();
        let (klass, method) = a_klass_and_method(&mut vm);
        assert!(matches!(
            decide(&SiteFeedback::Mono { klass, method }),
            InlineDecision::Call
        ));
    }

    #[test]
    fn poly_calls() {
        let mut vm = test_vm();
        let (klass, method) = a_klass_and_method(&mut vm);
        let fb = SiteFeedback::Poly {
            cases: vec![FeedbackCase {
                klass,
                method,
                count: None,
            }],
        };
        assert!(matches!(decide(&fb), InlineDecision::Call));
    }

    #[test]
    fn mega_calls() {
        assert!(matches!(decide(&SiteFeedback::Mega), InlineDecision::Call));
    }

    use crate::bytecode::builder::BytecodeBuilder;

    #[test]
    fn budget_grows_with_level() {
        assert_eq!(budget_for_level(1).per_call_cost, 30);
        assert_eq!(budget_for_level(0).per_call_cost, 30, "level 0 clamps to 1");
        assert!(budget_for_level(2).per_call_cost > budget_for_level(1).per_call_cost);
        assert!(budget_for_level(3).total_bytes > budget_for_level(2).total_bytes);
        assert_eq!(
            budget_for_level(4),
            budget_for_level(9),
            "level >= 4 clamps"
        );
    }

    #[test]
    fn cost_primitive_is_flat_four() {
        let mut vm = test_vm();
        let sel = vm.universe.intern(b"prim");
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.push_temp(0);
        b.ret_tos(); // a non-trivial fallback body...
        let m = b.finish(&mut vm, sel, 1, 0);
        m.set_primitive(1); // ...but a primitive, so cost is 4 regardless
        assert_eq!(inline_cost(m), 4);
    }

    #[test]
    fn cost_accessor_and_quick_return_are_two() {
        let mut vm = test_vm();
        // `^self` (bare return-self).
        let s1 = vm.universe.intern(b"retSelf");
        let mut b1 = BytecodeBuilder::new();
        b1.ret_self();
        assert_eq!(inline_cost(b1.finish(&mut vm, s1, 0, 0)), 2);

        // `^temp0` (push then return-tos — the accessor/quick-return shape).
        let s2 = vm.universe.intern(b"getArg");
        let mut b2 = BytecodeBuilder::new();
        b2.push_temp(0);
        b2.ret_tos();
        assert_eq!(inline_cost(b2.finish(&mut vm, s2, 1, 0)), 2);
    }

    /// S14 step 4b: `is_leaf` is true exactly when the bytecode has no `Send`.
    #[test]
    fn is_leaf_detects_sends() {
        let mut vm = test_vm();
        // `^instvar0` — accessor, no send → leaf.
        let s1 = vm.universe.intern(b"getIv");
        let mut b1 = BytecodeBuilder::new();
        b1.push_instvar(0);
        b1.ret_tos();
        assert!(is_leaf(b1.finish(&mut vm, s1, 0, 0)));

        // `^self` → leaf.
        let s2 = vm.universe.intern(b"getSelf");
        let mut b2 = BytecodeBuilder::new();
        b2.ret_self();
        assert!(is_leaf(b2.finish(&mut vm, s2, 0, 0)));

        // `t := arg. ^t` (a store + push + return, still no send) → leaf.
        let s3 = vm.universe.intern(b"stash:");
        let mut b3 = BytecodeBuilder::new();
        b3.push_temp(0);
        b3.store_temp_pop(1);
        b3.push_temp(1);
        b3.ret_tos();
        assert!(is_leaf(b3.finish(&mut vm, s3, 1, 1)));

        // `^self foo` (one send) → NOT a leaf.
        let foo = vm.universe.intern(b"foo");
        let s4 = vm.universe.intern(b"callsFoo");
        let mut b4 = BytecodeBuilder::new();
        b4.push_self();
        b4.send(&mut vm, foo, 0);
        b4.ret_tos();
        assert!(!is_leaf(b4.finish(&mut vm, s4, 0, 0)));
    }

    /// S14 step 4b: a mono site to a cheap LEAF inlines; the guard is `SmiTest`
    /// for a smi klass and `KlassTest` for any other; a non-leaf, an
    /// over-budget leaf, Poly/Mega/Untaken all fall back to `Call`.
    #[test]
    fn decide_with_budget_inlines_cheap_leaves() {
        let mut vm = test_vm();
        let budget = budget_for_level(1);
        let smi_bits = vm.universe.smi_klass.oop().raw();

        // A cheap leaf accessor `^instvar0` on a non-smi klass → Inline + KlassTest.
        let acc_sel = vm.universe.intern(b"iv");
        let mut ab = BytecodeBuilder::new();
        ab.push_instvar(0);
        ab.ret_tos();
        let accessor = ab.finish(&mut vm, acc_sel, 0, 0);
        let obj_klass = vm.universe.object_klass;
        match decide_with_budget(
            &SiteFeedback::Mono {
                klass: obj_klass,
                method: accessor,
            },
            &budget,
            smi_bits,
        ) {
            InlineDecision::Inline { callee, guard } => {
                assert_eq!(callee.oop().raw(), accessor.oop().raw());
                assert_eq!(guard, GuardKind::KlassTest, "non-smi klass → KlassTest");
            }
            other => panic!("expected Inline, got {other:?}"),
        }

        // Same accessor but a SMI receiver klass → Inline + SmiTest.
        match decide_with_budget(
            &SiteFeedback::Mono {
                klass: vm.universe.smi_klass,
                method: accessor,
            },
            &budget,
            smi_bits,
        ) {
            InlineDecision::Inline { guard, .. } => {
                assert_eq!(guard, GuardKind::SmiTest, "smi klass → SmiTest");
            }
            other => panic!("expected Inline, got {other:?}"),
        }

        // A NON-leaf mono target (`^self foo`) → Call, never Inline.
        let foo = vm.universe.intern(b"foo");
        let nl_sel = vm.universe.intern(b"nonLeaf");
        let mut nb = BytecodeBuilder::new();
        nb.push_self();
        nb.send(&mut vm, foo, 0);
        nb.ret_tos();
        let non_leaf = nb.finish(&mut vm, nl_sel, 0, 0);
        assert!(matches!(
            decide_with_budget(
                &SiteFeedback::Mono {
                    klass: obj_klass,
                    method: non_leaf,
                },
                &budget,
                smi_bits,
            ),
            InlineDecision::Call
        ));

        // An over-budget leaf (cost > per_call_cost) → Call. Build a leaf whose
        // cost is its bytecode length, larger than a tiny budget.
        let big_sel = vm.universe.intern(b"bigLeaf");
        let mut bb = BytecodeBuilder::new();
        bb.push_self();
        bb.push_temp(0);
        bb.push_temp(0);
        bb.pop();
        bb.pop();
        bb.ret_self();
        let big_leaf = bb.finish(&mut vm, big_sel, 1, 0);
        let tiny = InlineBudget {
            per_call_cost: 1,
            total_bytes: 300,
            max_depth: 4,
        };
        assert!(matches!(
            decide_with_budget(
                &SiteFeedback::Mono {
                    klass: obj_klass,
                    method: big_leaf,
                },
                &tiny,
                smi_bits,
            ),
            InlineDecision::Call
        ));

        // Untaken / Poly / Mega → Call (never Inline here).
        assert!(matches!(
            decide_with_budget(&SiteFeedback::Untaken, &budget, smi_bits),
            InlineDecision::Call
        ));
        assert!(matches!(
            decide_with_budget(&SiteFeedback::Mega, &budget, smi_bits),
            InlineDecision::Call
        ));
    }

    #[test]
    fn cost_general_is_bytecode_len() {
        let mut vm = test_vm();
        let sel = vm.universe.intern(b"general");
        // `push_self; push_temp; push_temp; pop; pop; ^self` — more than a quick
        // return, no primitive → cost is the raw bytecode length.
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.push_temp(0);
        b.push_temp(0);
        b.pop();
        b.pop();
        b.ret_self();
        let m = b.finish(&mut vm, sel, 1, 0);
        assert_eq!(inline_cost(m), m.bytecode_len() as u32);
        assert!(
            inline_cost(m) > 2,
            "a general body costs more than a quick return"
        );
    }
}
