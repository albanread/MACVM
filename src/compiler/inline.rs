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

use crate::compiler::feedback::SiteFeedback;
use crate::oops::wrappers::{KlassOop, MethodOop};

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
}
