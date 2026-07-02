//! The heap verifier: walks a committed region object-by-object (headers
//! make the heap parsable — size comes from the klass's format plus, for
//! indexable formats, the object's own size slot) and checks the
//! invariants genesis and every later allocator must preserve. Used by
//! genesis itself (SPEC §3.2 step 12) and by tests (CONVENTIONS §4).
//!
//! Format-dispatched from the first line: a `Double`'s body is raw bits,
//! not oops, and must never be read as if it were (SPEC §2.3 — the walker
//! is the first heap-scanning code in the tree, so getting this right here
//! sets the pattern S7's GC follows).

use crate::oops::layout::{HEADER_WORDS, MEM_TAG};
use crate::oops::wrappers::{KlassOop, MemOop};
use crate::oops::Oop;

use super::universe::Universe;

#[derive(Debug, PartialEq, Eq)]
pub struct VerifyError(pub String);

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Walks every object in `universe.eden` from `start` to `top`, checking:
/// every mark word has MARK_TAG + sentinel; every klass word is mem-tagged
/// and points to a validly klass-shaped object (i.e. `KlassOop::try_from`
/// succeeds on it — no klass field anywhere holds a smi, a surviving
/// genesis placeholder); the walk lands exactly on `eden.top` when done (no
/// object claims a size that overruns or undershoots the next header).
pub fn verify_heap(universe: &Universe) -> Result<usize, VerifyError> {
    let mut addr = universe.eden.start;
    let mut count = 0usize;

    while addr < universe.eden.top {
        let candidate = Oop::from_raw(addr as u64 + MEM_TAG);
        let obj = MemOop::try_from(candidate)
            .ok_or_else(|| VerifyError(format!("object at {addr:#x} is not mem-tagged")))?;

        let mark = obj.mark();
        if !mark.is_pristine() && mark.hash() == 0 && mark.age() == 0 {
            // still fine; is_pristine is a convenience, not a requirement.
        }
        // The mark's own tag/sentinel were already validated by
        // `Mark::from_word`'s debug_assert inside `obj.mark()` in debug
        // builds; in release, re-check explicitly so `verify_heap` catches
        // corruption in both profiles.
        let word = obj.mark().word();
        if word & 0b11 != 0b11 || word & 0b100 == 0 {
            return Err(VerifyError(format!(
                "object at {addr:#x} has a malformed mark word {word:#x}"
            )));
        }

        let klass_oop = obj.klass_oop();
        if !klass_oop.is_mem() {
            return Err(VerifyError(format!(
                "object at {addr:#x} has a non-mem klass field {:#x} (unpatched placeholder?)",
                klass_oop.raw()
            )));
        }
        // `KlassOop::try_from` is itself the correct invariant check here:
        // it succeeds iff `klass_oop`'s OWN klass (i.e. this object's
        // metaclass, one level further up) has format Klass — meaning
        // `klass_oop` is validly klass-shaped. Do NOT additionally assert
        // `klass_oop`'s own `.format()` is `Klass`: that field describes
        // the shape of THIS object's instances (e.g. `Format::Slots` for
        // `undefined_object_klass`, correctly), not of `klass_oop` itself —
        // conflating the two was a real S1 bug caught by this test.
        KlassOop::try_from(klass_oop).ok_or_else(|| {
            VerifyError(format!(
                "object at {addr:#x}'s klass {:#x} is not itself klass-shaped",
                klass_oop.raw()
            ))
        })?;

        let words = obj.instance_size_words();
        if words < HEADER_WORDS {
            return Err(VerifyError(format!(
                "object at {addr:#x} claims {words} words, smaller than a header"
            )));
        }

        addr += words * crate::oops::layout::WORD_SIZE;
        count += 1;
    }

    if addr != universe.eden.top {
        return Err(VerifyError(format!(
            "heap walk ended at {addr:#x}, expected exactly eden.top {:#x}",
            universe.eden.top
        )));
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::vm_state::VmOptions;

    #[test]
    fn verify_heap_post_genesis() {
        let u = Universe::genesis(&VmOptions {
            heap_mib: 64,
            trace: Default::default(),
        });
        let count = verify_heap(&u).expect("post-genesis heap must verify");
        assert!(count > 0);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn verify_heap_detects_corruption() {
        let mut u = Universe::genesis(&VmOptions {
            heap_mib: 64,
            trace: Default::default(),
        });
        // Corrupt object_klass's own klass field ("Object class"'s klass,
        // normally metaclass_klass) to a smi placeholder via the raw
        // (non-panicking) write path — never through `set_klass`, which
        // demands an actual `KlassOop`.
        let object_meta = u.object_klass.klass();
        object_meta
            .as_mem()
            .set_klass_raw(crate::oops::smi::SmallInt::new(0).oop());
        let result = verify_heap(&u);
        assert!(result.is_err());
        let _ = &mut u; // keep `u` mutably-bindable for symmetry with other tests
    }
}
