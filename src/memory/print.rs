//! `print_oop` — the debug object printer (SPEC §2.5's disassembler/trace
//! dependency). Lives here, not in `oops::print`, because printing needs to
//! compare against the well-known oops (`nil`/`true`/`false`,
//! `symbol_klass`, `string_klass`), which only `Universe` knows — and
//! `oops` must not import `memory` (`sprint_s01_detail.md` §Layer
//! boundaries grants this as the pinned escape hatch). Never allocates;
//! never panics on malformed input in release (prints `<bad oop …>`
//! instead).

use crate::oops::smi::SmallInt;
use crate::oops::wrappers::{ArrayOop, ByteArrayOop, DoubleOop, KlassOop, MemOop, SymbolOop};
use crate::oops::Oop;

use super::universe::Universe;

const MAX_DEPTH: usize = 3;
const MAX_ARRAY_LEN: usize = 10;

pub fn print_oop(u: &Universe, o: Oop) -> String {
    print_depth(u, o, 0)
}

/// A klass's or object's `name` field, as PLAIN TEXT (no `#` prefix). Never
/// recurse through `print_depth` for a name: that always renders Symbols
/// with the `#` prefix, which is correct for `#foo` as a *value* but wrong
/// for "the name of a class"/"the name of a class prefixing `a `".
fn plain_name(name: Oop) -> String {
    match SymbolOop::try_from(name) {
        Some(sym) => sym.as_string(),
        None => "<unnamed>".to_string(),
    }
}

fn print_depth(u: &Universe, o: Oop, depth: usize) -> String {
    if let Some(smi) = SmallInt::try_from(o) {
        return smi.value().to_string();
    }
    if o.raw() == u.nil_obj.raw() {
        return "nil".to_string();
    }
    if o.raw() == u.true_obj.raw() {
        return "true".to_string();
    }
    if o.raw() == u.false_obj.raw() {
        return "false".to_string();
    }

    let Some(m) = MemOop::try_from(o) else {
        return format!("<bad oop {:#x}>", o.raw());
    };

    if let Some(sym) = SymbolOop::try_from_exact(o, u.symbol_klass) {
        return format!("#{}", sym.as_string());
    }
    if let Some(s) = ByteArrayOop::try_from(o) {
        if m.klass().oop() == u.string_klass.oop() {
            let mut bytes = Vec::new();
            s.copy_bytes_out(&mut bytes);
            return format!("'{}'", String::from_utf8_lossy(&bytes));
        }
    }
    if let Some(d) = DoubleOop::try_from(o) {
        return print_f64(d.value());
    }
    if let Some(k) = KlassOop::try_from(o) {
        return plain_name(k.name());
    }
    if let Some(a) = ArrayOop::try_from(o) {
        if depth >= MAX_DEPTH {
            return "#(...)".to_string();
        }
        let len = a.len();
        let shown = len.min(MAX_ARRAY_LEN);
        let mut parts = Vec::with_capacity(shown);
        for i in 0..shown {
            parts.push(print_depth(u, a.at(i), depth + 1));
        }
        let tail = if len > MAX_ARRAY_LEN { " …" } else { "" };
        return format!("#({}{})", parts.join(" "), tail);
    }

    format!("a {}", plain_name(m.klass().name()))
}

fn print_f64(v: f64) -> String {
    if v.is_nan() {
        return "nan".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    // Rust's default f64 Display is shortest-round-trip.
    let s = format!("{v}");
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{s}.0")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::alloc;
    use crate::runtime::vm_state::VmOptions;

    fn boot() -> Universe {
        Universe::genesis(&VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            eden_kb: None,
        })
    }

    #[test]
    fn print_oop_basics() {
        let mut u = boot();
        assert_eq!(print_oop(&u, SmallInt::new(-42).oop()), "-42");
        assert_eq!(print_oop(&u, u.nil_obj), "nil");
        assert_eq!(print_oop(&u, u.true_obj), "true");
        assert_eq!(print_oop(&u, u.false_obj), "false");
        let sym = u.intern(b"foo");
        assert_eq!(print_oop(&u, sym.oop()), "#foo");
        assert_eq!(print_oop(&u, u.array_klass.oop()), "Array");

        let assoc = alloc::alloc_slots_raw(&mut u.eden, u.nil_obj, u.association_klass);
        assert_eq!(print_oop(&u, assoc.oop()), "a Association");
    }

    #[test]
    fn print_oop_depth_cap() {
        let mut u = boot();
        let arr = alloc::alloc_words_raw(
            &mut u.eden,
            u.nil_obj,
            crate::oops::layout::HEADER_WORDS + 1 + 1,
            u.array_klass.oop(),
            true,
        );
        arr.set_raw_body_word(0, SmallInt::new(1).oop().raw());
        arr.set_raw_body_word(1, arr.oop().raw()); // self-reference
        let s = print_oop(&u, arr.oop());
        assert!(s.starts_with("#("));
    }

    fn alloc_double_raw(u: &mut Universe, v: f64) -> DoubleOop {
        let klass = u.double_klass;
        let words = klass.non_indexable_size();
        let obj = alloc::alloc_words_raw(&mut u.eden, u.nil_obj, words, klass.oop(), false);
        obj.set_raw_body_word(0, v.to_bits());
        // SAFETY: freshly allocated with format Double.
        unsafe { DoubleOop::from_oop_unchecked(obj.oop()) }
    }

    #[test]
    fn print_oop_double() {
        let mut u = boot();
        let half = alloc_double_raw(&mut u, 0.5);
        assert_eq!(print_oop(&u, half.oop()), "0.5");
        let big = alloc_double_raw(&mut u, 1e10);
        assert_eq!(print_oop(&u, big.oop()), "10000000000.0");
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn print_oop_bad_input() {
        let u = boot();
        let bad = Oop::from_raw_unchecked(0x2);
        let s = print_oop(&u, bad);
        assert!(s.contains("bad oop"), "got: {s}");
    }
}
