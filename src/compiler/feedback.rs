//! S14 step 1: the type-feedback reader (SPEC §8.4). Reads one send site's
//! observed receiver types out of the interpreter IC side table (S3/S5,
//! SPEC §4.3) into a [`SiteFeedback`] the inliner (later steps) consumes to
//! decide whether to speculate/inline. This step reads the INTERPRETER side
//! only; the richer compiled-PIC source (per-entry counts) arrives once PICs
//! carry counter words (a later step), so the `prev` parameter is accepted but
//! not yet consulted.
//!
//! **Raw oops, not `Handle`s.** The sprint doc's `SiteFeedback` uses
//! `Handle<KlassOop>`; the as-built compiler runs `compile_method` in a NO-GC
//! window (`driver`'s own invariant: "no HandleScope needed, no GC can strike
//! mid-compile"), so a klass/method read here stays valid for the whole
//! compile — plain `KlassOop`/`MethodOop` are correct and simpler. If a later
//! step ever compiles across a collection, this becomes `Handle`s then.
//!
//! **Read-only** (layer table): takes `&VmState`, never patches or allocates.
//! An nmethod-id target is resolved with a local read-only method walk (the
//! `lookup` walk minus its `&mut` cache insert), not `runtime::lookup::lookup`.

use crate::codecache::nmethod::NmethodId;
use crate::interpreter::ic::InterpreterIc;
use crate::oops::layout::{IC_GUARD_MEGA, IC_GUARD_POLY, IC_POLY_MAX_PAIRS};
use crate::oops::method_dict::MethodDictOop;
use crate::oops::smi::SmallInt;
use crate::oops::wrappers::{ArrayOop, KlassOop, MethodOop, SymbolOop};
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;

/// One send site's observed receivers (SPEC §8.4). The inliner maps this onto a
/// codegen decision: `Untaken` → uncommon trap (Self's lazy cold path);
/// `Mono` → speculate on the single klass; `Poly` → inline the dominant case
/// with a slow-path fallback; `Mega` → a plain dynamic send.
#[derive(Clone, Debug)]
pub enum SiteFeedback {
    /// IC still empty — the site never executed while interpreted.
    Untaken,
    Mono {
        klass: KlassOop,
        method: MethodOop,
    },
    /// Cases ordered by count when counts exist (compiled PIC, a later step),
    /// else by first-seen order — the interpreter POLY array is count-free
    /// (stride pinned by SPEC §4.3), so every `count` here is `None` for now.
    Poly {
        cases: Vec<FeedbackCase>,
    },
    Mega,
}

/// One (receiver klass → resolved method) observation, with an optional
/// execution count (`None` for the count-free interpreter POLY array).
#[derive(Clone, Debug)]
pub struct FeedbackCase {
    pub klass: KlassOop,
    pub method: MethodOop,
    pub count: Option<u32>,
}

/// Read the feedback for send site `ic_index` of `method` (SPEC §8.4). Source:
/// the interpreter IC side table. `prev` (the nmethod being replaced, whose
/// compiled PIC carries richer counts) is accepted for the eventual
/// source-priority rule but not yet consulted — PIC counter words are a later
/// S14 step.
pub fn read_send_site(
    vm: &VmState,
    method: MethodOop,
    ic_index: u16,
    prev: Option<NmethodId>,
) -> SiteFeedback {
    let _ = prev; // compiled-PIC source: later step
    let ic = InterpreterIc::at(method, ic_index);
    let guard = ic.guard();

    // Mega / Poly are smi-tagged guards (SPEC §4.3); Mono is a klassOop guard;
    // Empty is `nil`.
    if let Some(smi) = SmallInt::try_from(guard) {
        return match smi.value() {
            v if v == IC_GUARD_MEGA => SiteFeedback::Mega,
            v if v == IC_GUARD_POLY => read_poly(vm, ic),
            other => panic!("read_send_site: unrecognized IC guard smi {other}"),
        };
    }
    match KlassOop::try_from(guard) {
        Some(klass) => SiteFeedback::Mono {
            klass,
            method: resolve_target(vm, ic.target()),
        },
        None => SiteFeedback::Untaken, // guard == nil
    }
}

/// Walk the `[k1, m1, k2, m2, …]` pairs array (empty slots hold `nil` in the
/// key position — `KlassOop::try_from` rejects them, `ic::poly_arity`'s own
/// convention). First-seen order, counts `None`.
fn read_poly(vm: &VmState, ic: InterpreterIc) -> SiteFeedback {
    let pairs = ArrayOop::try_from(ic.target()).expect("poly IC target must be an Array");
    let mut cases = Vec::new();
    for i in 0..IC_POLY_MAX_PAIRS {
        let Some(klass) = KlassOop::try_from(pairs.at(2 * i)) else {
            break; // first empty slot: the rest are empty too
        };
        cases.push(FeedbackCase {
            klass,
            method: resolve_target(vm, pairs.at(2 * i + 1)),
            count: None,
        });
    }
    SiteFeedback::Poly { cases }
}

/// A mono/poly target is either a plain `MethodOop` (interpreter-resolved) or a
/// smi `NmethodId` (the site tiered up — `ic::set_mono_compiled`). Resolve both
/// to the underlying `MethodOop`; the nmethod path re-looks-up its own
/// `(key_klass, key_selector)`.
fn resolve_target(vm: &VmState, target: Oop) -> MethodOop {
    if let Some(m) = MethodOop::try_from(target) {
        return m;
    }
    let smi =
        SmallInt::try_from(target).expect("mono IC target must be a MethodOop or an nmethod id");
    let id = NmethodId(smi.value() as u32);
    let nm = vm
        .code_table
        .get(id)
        .expect("a mono-compiled IC target must name a live nmethod");
    resolve_method_ro(vm, nm.key_klass, nm.key_selector)
        .expect("a live nmethod's own (key_klass, key_selector) must still resolve to a method")
}

/// Read-only method lookup — the `runtime::lookup::lookup` walk minus its
/// `&mut` lookup-cache insert (this reader is `&VmState` by contract). Probes
/// each klass's `MethodDictOop` up the superclass chain to `nil`.
fn resolve_method_ro(vm: &VmState, klass: KlassOop, selector: SymbolOop) -> Option<MethodOop> {
    let nil = vm.universe.nil_obj;
    let mut k = klass;
    loop {
        if let Some(dict) = MethodDictOop::try_from(k.methods()) {
            if let Some(m) = dict.probe(vm, selector) {
                return Some(m);
            }
        }
        let sc = k.superclass();
        if sc.raw() == nil.raw() {
            return None;
        }
        k = KlassOop::try_from(sc).expect("resolve_method_ro: superclass field is not a klass");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::builder::BytecodeBuilder;
    use crate::oops::layout::IC_POLY_ARRAY_LEN;
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

    /// A host method with exactly one send site (IC index 0) whose feedback the
    /// tests set then read back.
    fn host_with_send(vm: &mut VmState) -> MethodOop {
        let sel = vm.universe.intern(b"foo:");
        let mut b = BytecodeBuilder::new();
        b.push_self();
        b.push_self();
        b.send(vm, sel, 1);
        b.ret_tos();
        let m_sel = vm.universe.intern(b"host:");
        b.finish(vm, m_sel, 1, 0)
    }

    /// A trivial `MethodOop` to use as an IC target (its body is never run).
    fn a_method(vm: &mut VmState, name: &[u8]) -> MethodOop {
        let sel = vm.universe.intern(name);
        let mut b = BytecodeBuilder::new();
        b.ret_self();
        b.finish(vm, sel, 0, 0)
    }

    #[test]
    fn reads_untaken_from_empty_ic() {
        let mut vm = test_vm();
        let host = host_with_send(&mut vm);
        assert!(matches!(
            read_send_site(&vm, host, 0, None),
            SiteFeedback::Untaken
        ));
    }

    #[test]
    fn reads_mono() {
        let mut vm = test_vm();
        let host = host_with_send(&mut vm);
        let klass = vm.universe.smi_klass;
        let target = a_method(&mut vm, b"target");
        let epoch = vm.ic_epoch;
        InterpreterIc::at(host, 0).set_mono(&mut vm, klass, target, epoch);
        match read_send_site(&vm, host, 0, None) {
            SiteFeedback::Mono {
                klass: k,
                method: m,
            } => {
                assert_eq!(k.oop().raw(), klass.oop().raw());
                assert_eq!(m.oop().raw(), target.oop().raw());
            }
            other => panic!("expected Mono, got {other:?}"),
        }
    }

    #[test]
    fn reads_mega() {
        let mut vm = test_vm();
        let host = host_with_send(&mut vm);
        let nil = vm.universe.nil_obj;
        InterpreterIc::at(host, 0).set_mega(&mut vm, nil);
        assert!(matches!(
            read_send_site(&vm, host, 0, None),
            SiteFeedback::Mega
        ));
    }

    /// The interpreter POLY array is count-free and first-seen ordered.
    #[test]
    fn reads_poly_first_seen_order_count_free() {
        let mut vm = test_vm();
        let host = host_with_send(&mut vm);
        let k1 = vm.universe.smi_klass;
        let k2 = vm.universe.boolean_klass;
        let m1 = a_method(&mut vm, b"m1");
        let m2 = a_method(&mut vm, b"m2");
        let array_klass = vm.universe.array_klass;
        // Fill a fresh pairs array [k1, m1, k2, m2, nil, …] AFTER the last
        // allocation (m2), so nothing moves it before the raw fills below.
        let pairs =
            crate::memory::alloc::alloc_indexable_oops(&mut vm, array_klass, IC_POLY_ARRAY_LEN);
        pairs.at_put(0, k1.oop());
        pairs.at_put(1, m1.oop());
        pairs.at_put(2, k2.oop());
        pairs.at_put(3, m2.oop());
        let epoch = vm.ic_epoch;
        InterpreterIc::at(host, 0).set_poly(&mut vm, pairs, epoch);

        match read_send_site(&vm, host, 0, None) {
            SiteFeedback::Poly { cases } => {
                assert_eq!(cases.len(), 2, "two occupied pairs, rest empty");
                assert_eq!(
                    cases[0].klass.oop().raw(),
                    k1.oop().raw(),
                    "first-seen order"
                );
                assert_eq!(cases[0].method.oop().raw(), m1.oop().raw());
                assert!(
                    cases[0].count.is_none(),
                    "interpreter POLY carries no counts"
                );
                assert_eq!(cases[1].klass.oop().raw(), k2.oop().raw());
                assert_eq!(cases[1].method.oop().raw(), m2.oop().raw());
            }
            other => panic!("expected Poly, got {other:?}"),
        }
    }
}
