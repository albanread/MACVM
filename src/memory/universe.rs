//! `Universe`: the heap, well-known oops, and `genesis()` — the metaclass
//! knot (SPEC §3.1–§3.2 step 1). After genesis,
//! `nil.klass().name() == #UndefinedObject` holds in a running process.
//!
//! Genesis is built almost entirely from **local variables**, not by
//! mutating a partially-constructed `Universe`: the circularities (every
//! object needs a klass, but the first klasses don't exist yet; fields want
//! `nil`, but `nil` needs `UndefinedObject`; klass names are Symbols, but
//! Symbols need `symbol_klass` and the table) are resolved by allocating
//! with placeholder fields and patching, exactly as
//! `sprint_s01_detail.md` §Design prescribes — but the `Universe` struct
//! literal itself is only ever constructed once, at the very end (step 12),
//! so there is never a "partially valid" `Universe` for other code to
//! observe. `PLACEHOLDER = SmallInt::new(0).oop()` throughout: a valid oop
//! that can never be mistaken for a heap reference (tag 00, not 01), so any
//! accidental read through it fails fast in a typed wrapper's `try_from`
//! rather than performing a wild read.

use crate::oops::klass::Format;
use crate::oops::layout::{HEADER_WORDS, KLASS_SIZE_WORDS};
use crate::oops::mark::Mark;
use crate::oops::smi::SmallInt;
use crate::oops::wrappers::{KlassOop, MemOop, SymbolOop};
use crate::oops::Oop;
use crate::runtime::vm_state::VmOptions;

use super::alloc;
use super::reservation::Reservation;
use super::space::Eden;
use super::symbols::SymbolTable;

pub struct Universe {
    #[allow(dead_code)] // kept alive for its Drop impl; no other field reads it in S1
    reservation: Reservation,
    pub eden: Eden,

    pub nil_obj: Oop,
    pub true_obj: Oop,
    pub false_obj: Oop,

    pub metaclass_klass: KlassOop,
    pub class_klass: KlassOop,
    pub object_klass: KlassOop,
    pub undefined_object_klass: KlassOop,
    pub boolean_klass: KlassOop,
    pub true_klass: KlassOop,
    pub false_klass: KlassOop,
    pub smi_klass: KlassOop,
    pub character_klass: KlassOop,
    pub double_klass: KlassOop,
    pub string_klass: KlassOop,
    pub symbol_klass: KlassOop,
    pub array_klass: KlassOop,
    pub bytearray_klass: KlassOop,
    pub association_klass: KlassOop,
    pub methoddict_klass: KlassOop,
    pub method_klass: KlassOop,
    pub closure_klass: KlassOop,
    pub context_klass: KlassOop,
    pub process_klass: KlassOop,

    /// The global namespace. `nil` until S5's loader populates it.
    pub smalltalk: Oop,

    pub symbols: SymbolTable,
    hash_counter: u32,
}

/// Default eden size *(tunable)* — SPEC §7.1.
const EDEN_SIZE: usize = 4 << 20;
/// Initial symbol table capacity *(tunable)* — SPEC §3.1.
const SYMBOL_TABLE_CAPACITY: usize = 1024;

impl Universe {
    pub fn genesis(options: &VmOptions) -> Universe {
        // --- step 1: heap up ------------------------------------------------
        let reservation = Reservation::reserve(options.heap_mib << 20);
        reservation.commit(0, EDEN_SIZE);
        let mut eden = Eden::new(reservation.base(), EDEN_SIZE);

        let placeholder = SmallInt::new(0).oop();

        // --- step 2: nil shell (Slots, 0 named fields) ----------------------
        let nil_shell =
            alloc::alloc_words_raw(&mut eden, placeholder, HEADER_WORDS, placeholder, true);
        // From here on, tagged allocations nil-fill their bodies with the
        // REAL nil oop (still klass-less until step 6's patch).
        let nil = nil_shell.oop();

        // --- step 3: metaclass knot -----------------------------------------
        let metaclass_klass = alloc::alloc_klass_raw(&mut eden, nil, placeholder);
        metaclass_klass.set_format(Format::Klass, false);
        metaclass_klass.set_non_indexable_size(KLASS_SIZE_WORDS);
        metaclass_klass.set_superclass(placeholder); // patched at step 5c

        let metaclass_meta = alloc::alloc_klass_raw(&mut eden, nil, metaclass_klass.oop());
        metaclass_meta.set_format(Format::Klass, false);
        metaclass_meta.set_non_indexable_size(KLASS_SIZE_WORDS);
        metaclass_meta.set_superclass(placeholder); // patched at step 5d

        // Patch: close the knot. Metaclass class class == Metaclass.
        metaclass_klass.set_klass(metaclass_meta);

        // --- step 4: Object and Class ----------------------------------------
        let object_meta = alloc::alloc_klass_raw(&mut eden, nil, metaclass_klass.oop());
        object_meta.set_format(Format::Klass, false);
        object_meta.set_non_indexable_size(KLASS_SIZE_WORDS);
        object_meta.set_superclass(placeholder); // patched at step 4d

        let object_klass = alloc::alloc_klass_raw(&mut eden, nil, object_meta.oop());
        object_klass.set_format(Format::Slots, false);
        object_klass.set_non_indexable_size(HEADER_WORDS);
        object_klass.set_superclass(nil);

        let class_meta = alloc::alloc_klass_raw(&mut eden, nil, metaclass_klass.oop());
        class_meta.set_format(Format::Klass, false);
        class_meta.set_non_indexable_size(KLASS_SIZE_WORDS);

        // Δ (pinned): v1 has no Behavior/ClassDescription layer —
        // `Class superclass == Object`.
        let class_klass = alloc::alloc_klass_raw(&mut eden, nil, class_meta.oop());
        class_klass.set_format(Format::Klass, false);
        class_klass.set_non_indexable_size(KLASS_SIZE_WORDS);
        class_klass.set_superclass(object_klass.oop());

        // "Class class" inherits from "Object class".
        class_meta.set_superclass(object_meta.oop());

        // Patch: Object class superclass == Class (the Smalltalk-80 rule).
        object_meta.set_superclass(class_klass.oop());

        // --- step 5: close Metaclass's superclasses ---------------------------
        // Δ (pinned): Smalltalk-80 has `Metaclass superclass ==
        // ClassDescription`; v1 pins `Class`.
        metaclass_klass.set_superclass(class_klass.oop());
        metaclass_meta.set_superclass(class_meta.oop());

        // --- step 6: UndefinedObject + nil patch -------------------------------
        let undefined_object_klass = new_klass_core(
            &mut eden,
            nil,
            metaclass_klass,
            object_klass.oop(),
            Format::Slots,
            false,
            HEADER_WORDS,
        );
        nil_shell.set_klass(undefined_object_klass);

        // --- step 7: String, Symbol klasses -------------------------------------
        let string_klass = new_klass_core(
            &mut eden,
            nil,
            metaclass_klass,
            object_klass.oop(),
            Format::IndexableBytes,
            true,
            HEADER_WORDS,
        );
        let symbol_klass = new_klass_core(
            &mut eden,
            nil,
            metaclass_klass,
            string_klass.oop(),
            Format::IndexableBytes,
            true,
            HEADER_WORDS,
        );

        // --- step 8: symbol table + name patch pass -------------------------------
        let mut symbols = SymbolTable::with_capacity(SYMBOL_TABLE_CAPACITY);

        let name_of = |eden: &mut Eden, symbols: &mut SymbolTable, s: &str| {
            intern_core(eden, nil, symbols, symbol_klass, s.as_bytes())
        };

        let set_name_and_meta_name =
            |eden: &mut Eden, symbols: &mut SymbolTable, k: KlassOop, plain: &str| {
                let plain_sym = name_of(eden, symbols, plain);
                k.set_name(plain_sym.oop());
                let meta_name = format!("{plain} class");
                let meta_sym = name_of(eden, symbols, &meta_name);
                k.klass().set_name(meta_sym.oop());
            };

        set_name_and_meta_name(&mut eden, &mut symbols, object_klass, "Object");
        set_name_and_meta_name(&mut eden, &mut symbols, class_klass, "Class");
        set_name_and_meta_name(&mut eden, &mut symbols, metaclass_klass, "Metaclass");
        set_name_and_meta_name(
            &mut eden,
            &mut symbols,
            undefined_object_klass,
            "UndefinedObject",
        );
        set_name_and_meta_name(&mut eden, &mut symbols, string_klass, "String");
        set_name_and_meta_name(&mut eden, &mut symbols, symbol_klass, "Symbol");

        // --- step 9: remaining klasses -------------------------------------------
        macro_rules! remaining_klass {
            ($name:literal, $superclass:expr, $format:expr, $untagged:expr, $nis:expr) => {{
                let k = new_klass_core(
                    &mut eden,
                    nil,
                    metaclass_klass,
                    $superclass,
                    $format,
                    $untagged,
                    $nis,
                );
                set_name_and_meta_name(&mut eden, &mut symbols, k, $name);
                k
            }};
        }

        let boolean_klass = remaining_klass!(
            "Boolean",
            object_klass.oop(),
            Format::Slots,
            false,
            HEADER_WORDS
        );
        let true_klass = remaining_klass!(
            "True",
            boolean_klass.oop(),
            Format::Slots,
            false,
            HEADER_WORDS
        );
        let false_klass = remaining_klass!(
            "False",
            boolean_klass.oop(),
            Format::Slots,
            false,
            HEADER_WORDS
        );
        let smi_klass = remaining_klass!(
            "SmallInteger",
            object_klass.oop(),
            Format::Slots,
            false,
            HEADER_WORDS
        );
        let character_klass = remaining_klass!(
            "Character",
            object_klass.oop(),
            Format::Slots,
            false,
            HEADER_WORDS + 1
        );
        let double_klass = remaining_klass!(
            "Double",
            object_klass.oop(),
            Format::Double,
            true,
            HEADER_WORDS + 1
        );
        let array_klass = remaining_klass!(
            "Array",
            object_klass.oop(),
            Format::IndexableOops,
            false,
            HEADER_WORDS
        );
        let bytearray_klass = remaining_klass!(
            "ByteArray",
            object_klass.oop(),
            Format::IndexableBytes,
            true,
            HEADER_WORDS
        );
        let association_klass = remaining_klass!(
            "Association",
            object_klass.oop(),
            Format::Slots,
            false,
            HEADER_WORDS + 2
        );
        let methoddict_klass = remaining_klass!(
            "MethodDictionary",
            object_klass.oop(),
            Format::IndexableOops,
            false,
            HEADER_WORDS
        );
        let method_klass = remaining_klass!(
            "CompiledMethod",
            object_klass.oop(),
            Format::Method,
            true,
            HEADER_WORDS + 7
        );
        let closure_klass = remaining_klass!(
            "BlockClosure",
            object_klass.oop(),
            Format::Closure,
            false,
            HEADER_WORDS + 2
        );
        let context_klass = remaining_klass!(
            "Context",
            object_klass.oop(),
            Format::Context,
            false,
            HEADER_WORDS + 1
        );
        let process_klass = remaining_klass!(
            "Process",
            object_klass.oop(),
            Format::Process,
            true,
            HEADER_WORDS
        );

        // Association's named instance variables: #key #value.
        {
            let key_sym = name_of(&mut eden, &mut symbols, "key");
            let value_sym = name_of(&mut eden, &mut symbols, "value");
            let ivn = alloc::alloc_words_raw(
                &mut eden,
                nil,
                HEADER_WORDS + 1 + 2,
                array_klass.oop(),
                true,
            );
            ivn.set_raw_body_word(0, SmallInt::new(2).oop().raw());
            ivn.set_raw_body_word(1, key_sym.oop().raw());
            ivn.set_raw_body_word(2, value_sym.oop().raw());
            association_klass.set_inst_var_names(ivn.oop());
        }
        // Character's named instance variable: #value.
        {
            let value_sym = name_of(&mut eden, &mut symbols, "value");
            let ivn = alloc::alloc_words_raw(
                &mut eden,
                nil,
                HEADER_WORDS + 1 + 1,
                array_klass.oop(),
                true,
            );
            ivn.set_raw_body_word(0, SmallInt::new(1).oop().raw());
            ivn.set_raw_body_word(1, value_sym.oop().raw());
            character_klass.set_inst_var_names(ivn.oop());
        }

        // --- step 10: true/false --------------------------------------------------
        let true_obj = alloc::alloc_words_raw(&mut eden, nil, HEADER_WORDS, true_klass.oop(), true);
        true_obj.set_mark(Mark::pristine().with_tagged_contents(true));
        let false_obj =
            alloc::alloc_words_raw(&mut eden, nil, HEADER_WORDS, false_klass.oop(), true);
        false_obj.set_mark(Mark::pristine().with_tagged_contents(true));

        // --- step 11: smalltalk + hash counter --------------------------------------
        let smalltalk = nil;
        let hash_counter = 1u32;

        let universe = Universe {
            reservation,
            eden,
            nil_obj: nil,
            true_obj: true_obj.oop(),
            false_obj: false_obj.oop(),
            metaclass_klass,
            class_klass,
            object_klass,
            undefined_object_klass,
            boolean_klass,
            true_klass,
            false_klass,
            smi_klass,
            character_klass,
            double_klass,
            string_klass,
            symbol_klass,
            array_klass,
            bytearray_klass,
            association_klass,
            methoddict_klass,
            method_klass,
            closure_klass,
            context_klass,
            process_klass,
            smalltalk,
            symbols,
            hash_counter,
        };

        // --- step 12: verify -------------------------------------------------------
        super::verify::verify_heap(&universe).expect("genesis produced an invalid heap");

        universe
    }

    /// Create a new klass with a fresh metaclass, wired into the given
    /// superclass's metaclass chain, and intern+patch its name. The helper
    /// genesis itself uses (`new_klass_core`) taking explicit pieces rather
    /// than `&mut Universe` — genesis has no live `Universe` to borrow yet.
    /// This method is the crate-visible, post-genesis entry point S5's
    /// `subclass:` (and this sprint's `new_klass_regular_path` test) drive.
    #[allow(dead_code)] // exercised by tests now; S5's subclass: calls it for real
    pub(crate) fn new_klass(
        &mut self,
        superclass: KlassOop,
        name: &str,
        format: Format,
        untagged: bool,
        nis_words: usize,
    ) -> KlassOop {
        let metaclass_klass = self.metaclass_klass;
        let nil = self.nil_obj;
        let k = new_klass_core(
            &mut self.eden,
            nil,
            metaclass_klass,
            superclass.oop(),
            format,
            untagged,
            nis_words,
        );
        let plain_sym = self.intern(name.as_bytes());
        k.set_name(plain_sym.oop());
        let meta_name = format!("{name} class");
        let meta_sym = self.intern(meta_name.as_bytes());
        k.klass().set_name(meta_sym.oop());
        k
    }

    /// Interning entry point (SPEC §3.1). Probes by content; on miss
    /// allocates a Symbol and inserts. Rehashes ×2 at 75% load.
    pub fn intern(&mut self, bytes: &[u8]) -> SymbolOop {
        let symbol_klass = self.symbol_klass;
        let nil = self.nil_obj;
        intern_core(&mut self.eden, nil, &mut self.symbols, symbol_klass, bytes)
    }

    /// SPEC §2.2's lazy identity hash. `0` always means "unassigned"; the
    /// counter never hands out `0` on wraparound. Smis hash from their
    /// value directly — they have no mark word.
    pub fn identity_hash(&mut self, o: Oop) -> u32 {
        if let Some(smi) = SmallInt::try_from(o) {
            let v = smi.value();
            return (v as u32) ^ ((v >> 32) as u32);
        }
        let m = MemOop::try_from(o).expect("identity_hash: not a smi or mem oop");
        let mark = m.mark();
        let existing = mark.hash();
        if existing != 0 {
            return existing;
        }
        let h = self.next_hash();
        m.set_mark(mark.with_hash(h));
        h
    }

    fn next_hash(&mut self) -> u32 {
        self.hash_counter = self.hash_counter.wrapping_add(1);
        if self.hash_counter == 0 {
            self.hash_counter = 1;
        }
        self.hash_counter
    }

    #[cfg(test)]
    pub(crate) fn set_hash_counter_for_test(&mut self, v: u32) {
        self.hash_counter = v;
    }
}

/// The genesis-and-`new_klass`-shared core: allocate a fresh metaclass
/// (klass-shaped, `klass = metaclass_klass`), then the class itself
/// (`klass = ` the new meta), and wire `meta.superclass` to the given
/// superclass's own metaclass. `superclass` must already be a real
/// (non-placeholder) klass oop — true for every call site from genesis
/// step 6 onward.
fn new_klass_core(
    eden: &mut Eden,
    nil_fill: Oop,
    metaclass_klass: KlassOop,
    superclass: Oop,
    format: Format,
    untagged: bool,
    nis_words: usize,
) -> KlassOop {
    let meta = alloc::alloc_klass_raw(eden, nil_fill, metaclass_klass.oop());
    meta.set_format(Format::Klass, false);
    meta.set_non_indexable_size(KLASS_SIZE_WORDS);

    let klass = alloc::alloc_klass_raw(eden, nil_fill, meta.oop());
    klass.set_format(format, untagged);
    klass.set_non_indexable_size(nis_words);
    klass.set_superclass(superclass);

    let super_klass = KlassOop::try_from(superclass).expect("new_klass: superclass is not a klass");
    meta.set_superclass(super_klass.klass().oop());

    klass
}

fn symbol_bytes_eq(sym: SymbolOop, bytes: &[u8]) -> bool {
    if sym.len() != bytes.len() {
        return false;
    }
    (0..bytes.len()).all(|i| sym.as_mem().tail_byte_at(i) == bytes[i])
}

fn intern_core(
    eden: &mut Eden,
    nil_fill: Oop,
    symbols: &mut SymbolTable,
    symbol_klass: KlassOop,
    bytes: &[u8],
) -> SymbolOop {
    let hash = SymbolTable::content_hash(bytes);
    if let Some(found) = probe(symbols, symbol_klass, hash, bytes) {
        return found;
    }

    let nis = symbol_klass.non_indexable_size();
    let sym =
        alloc::alloc_indexable_bytes_raw(eden, nil_fill, symbol_klass.oop(), nis, bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        sym.byte_at_put(i, b);
    }
    // SAFETY: freshly allocated with klass = symbol_klass.
    let sym = unsafe { SymbolOop::from_oop_unchecked(sym.oop()) };

    insert(symbols, hash, sym.oop());

    if symbols.count.saturating_mul(4) >= symbols.buckets.len().saturating_mul(3) {
        rehash(symbols, symbol_klass);
    }

    sym
}

/// `None` = not found, but ALSO tells the caller nothing about which slot
/// is free (a fresh linear probe is cheap and avoids returning a second,
/// easy-to-misuse output).
fn probe(
    symbols: &SymbolTable,
    symbol_klass: KlassOop,
    hash: u64,
    bytes: &[u8],
) -> Option<SymbolOop> {
    let mask = symbols.buckets.len() - 1;
    let mut idx = (hash as usize) & mask;
    loop {
        match symbols.buckets[idx] {
            Some(existing) => {
                // SAFETY: every occupied slot holds a valid Symbol oop.
                let sym = unsafe { SymbolOop::from_oop_unchecked(existing) };
                debug_assert_eq!(sym.as_mem().klass().oop(), symbol_klass.oop());
                if symbol_bytes_eq(sym, bytes) {
                    return Some(sym);
                }
            }
            None => return None,
        }
        idx = (idx + 1) & mask;
    }
}

fn insert(symbols: &mut SymbolTable, hash: u64, sym: Oop) {
    let mask = symbols.buckets.len() - 1;
    let mut idx = (hash as usize) & mask;
    while symbols.buckets[idx].is_some() {
        idx = (idx + 1) & mask;
    }
    symbols.buckets[idx] = Some(sym);
    symbols.count += 1;
}

fn rehash(symbols: &mut SymbolTable, symbol_klass: KlassOop) {
    let new_cap = symbols.buckets.len() * 2;
    let old = std::mem::replace(symbols, SymbolTable::with_capacity(new_cap));
    for slot in old.buckets.into_iter().flatten() {
        // SAFETY: every occupied slot held a valid Symbol oop.
        let sym = unsafe { SymbolOop::from_oop_unchecked(slot) };
        let mut bytes = Vec::new();
        let len = sym.len();
        for i in 0..len {
            bytes.push(sym.as_mem().tail_byte_at(i));
        }
        let hash = SymbolTable::content_hash(&bytes);
        insert(symbols, hash, slot);
    }
    let _ = symbol_klass; // reserved: format re-checks could go here later
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot() -> Universe {
        Universe::genesis(&VmOptions {
            heap_mib: 64,
            trace: Default::default(),
        })
    }

    #[test]
    fn genesis_knot() {
        let u = boot();
        assert_eq!(u.object_klass.klass().klass(), u.metaclass_klass);
        assert_eq!(u.metaclass_klass.klass().klass(), u.metaclass_klass);
        assert_eq!(u.class_klass.klass().klass(), u.metaclass_klass);
    }

    #[test]
    fn genesis_superclasses() {
        let u = boot();
        assert_eq!(u.object_klass.superclass(), u.nil_obj);
        assert_eq!(
            u.object_klass.klass().superclass().raw(),
            u.class_klass.oop().raw()
        );
        assert_eq!(u.class_klass.superclass().raw(), u.object_klass.oop().raw());
        assert_eq!(
            u.metaclass_klass.superclass().raw(),
            u.class_klass.oop().raw()
        );
        assert_eq!(u.true_klass.superclass().raw(), u.boolean_klass.oop().raw());
        assert_eq!(
            u.true_klass.klass().superclass().raw(),
            u.boolean_klass.klass().oop().raw()
        );
    }

    #[test]
    fn genesis_nil_true_false() {
        let mut u = boot();
        let nil = MemOop::try_from(u.nil_obj).unwrap();
        assert_eq!(nil.klass(), u.undefined_object_klass);
        assert_eq!(
            u.undefined_object_klass.name(),
            u.intern(b"UndefinedObject").oop()
        );
        assert_eq!(u.true_klass.name(), u.intern(b"True").oop());
        assert_eq!(u.false_klass.name(), u.intern(b"False").oop());
        assert_ne!(u.true_obj.raw(), u.false_obj.raw());
    }

    #[test]
    fn genesis_no_placeholders() {
        let u = boot();
        let klasses = [
            u.metaclass_klass,
            u.class_klass,
            u.object_klass,
            u.undefined_object_klass,
            u.boolean_klass,
            u.true_klass,
            u.false_klass,
            u.smi_klass,
            u.character_klass,
            u.double_klass,
            u.string_klass,
            u.symbol_klass,
            u.array_klass,
            u.bytearray_klass,
            u.association_klass,
            u.methoddict_klass,
            u.method_klass,
            u.closure_klass,
            u.context_klass,
            u.process_klass,
        ];
        for k in klasses {
            assert!(k.oop().is_mem());
            let sc = k.superclass();
            assert!(sc.raw() == u.nil_obj.raw() || KlassOop::try_from(sc).is_some());
            assert!(SymbolOop::try_from(k.name()).is_some());
            assert_eq!(k.mixin(), u.nil_obj);
        }
    }

    #[test]
    fn genesis_formats_and_sizes() {
        let u = boot();
        assert_eq!(u.method_klass.non_indexable_size(), HEADER_WORDS + 7);
        assert_eq!(u.association_klass.non_indexable_size(), HEADER_WORDS + 2);
        assert_eq!(u.double_klass.non_indexable_size(), HEADER_WORDS + 1);
        assert_eq!(u.double_klass.format(), Format::Double);
        assert_eq!(u.array_klass.format(), Format::IndexableOops);
        assert_eq!(u.bytearray_klass.format(), Format::IndexableBytes);
        assert!(u.bytearray_klass.has_untagged_contents());
        assert!(!u.array_klass.has_untagged_contents());
    }

    #[test]
    fn metaclass_names() {
        let mut u = boot();
        assert_eq!(
            u.object_klass.klass().name(),
            u.intern(b"Object class").oop()
        );
        assert_eq!(u.array_klass.klass().name(), u.intern(b"Array class").oop());
        assert_eq!(
            u.metaclass_klass.klass().name(),
            u.intern(b"Metaclass class").oop()
        );
    }

    #[test]
    fn metaclass_shape() {
        let u = boot();
        let klasses = [
            u.object_klass,
            u.class_klass,
            u.metaclass_klass,
            u.array_klass,
            u.process_klass,
        ];
        for k in klasses {
            let meta = k.klass();
            assert_eq!(meta.format(), Format::Klass);
            assert_eq!(meta.non_indexable_size(), KLASS_SIZE_WORDS);
            assert_eq!(meta.klass(), u.metaclass_klass);
        }
    }

    #[test]
    fn new_klass_regular_path() {
        let mut u = boot();
        let zork = u.new_klass(u.object_klass, "Zork", Format::Slots, false, HEADER_WORDS);
        assert_eq!(zork.klass().klass(), u.metaclass_klass);
        assert_eq!(
            zork.klass().superclass().raw(),
            u.object_klass.klass().oop().raw()
        );
        assert_eq!(zork.name(), u.intern(b"Zork").oop());
    }

    #[test]
    fn symbol_intern_identity() {
        let mut u = boot();
        let a = u.intern(b"foo");
        let b = u.intern(b"foo");
        assert_eq!(a.oop().raw(), b.oop().raw());
        let c = u.intern(b"bar");
        assert_ne!(a.oop().raw(), c.oop().raw());
        let empty1 = u.intern(b"");
        let empty2 = u.intern(b"");
        assert_eq!(empty1.oop().raw(), empty2.oop().raw());
    }

    #[test]
    fn symbol_intern_content() {
        let mut u = boot();
        let s = u.intern(b"at:put:");
        assert_eq!(s.len(), 7);
        assert_eq!(s.as_string(), "at:put:");
        assert_eq!(s.as_mem().klass(), u.symbol_klass);
    }

    #[test]
    fn symbol_rehash() {
        let mut u = boot();
        // Genesis itself already interns every klass/metaclass name (and
        // #key/#value) before this test runs — the table does not start
        // empty. Assert the RELATIVE growth across several rehash cycles,
        // not an absolute count.
        let before = u.symbols.len();
        let names: Vec<String> = (0..2000).map(|i| format!("sym{i}")).collect();
        let first: Vec<Oop> = names.iter().map(|n| u.intern(n.as_bytes()).oop()).collect();
        for (n, expected) in names.iter().zip(first.iter()) {
            assert_eq!(u.intern(n.as_bytes()).oop().raw(), expected.raw());
        }
        assert_eq!(u.symbols.len(), before + 2000);
    }

    fn alloc_plain_object(u: &mut Universe) -> MemOop {
        let object_klass = u.object_klass;
        let nil = u.nil_obj;
        alloc::alloc_slots_raw(&mut u.eden, nil, object_klass)
    }

    #[test]
    fn identity_hash_lazy() {
        let mut u = boot();
        let a = alloc_plain_object(&mut u);
        let b = alloc_plain_object(&mut u);
        assert_eq!(a.mark().hash(), 0);

        let h1 = u.identity_hash(a.oop());
        assert_ne!(h1, 0);
        let h1_again = u.identity_hash(a.oop());
        assert_eq!(h1, h1_again);

        let h2 = u.identity_hash(b.oop());
        assert_ne!(h1, h2);
    }

    #[test]
    fn identity_hash_smi() {
        let mut u = boot();
        let eden_top_before = u.eden.top;
        let five = SmallInt::new(5).oop();
        assert_eq!(u.identity_hash(five), u.identity_hash(five));
        assert_eq!(u.eden.top, eden_top_before, "smi hashing must not allocate");
    }

    #[test]
    fn identity_hash_counter_skips_zero() {
        let mut u = boot();
        u.set_hash_counter_for_test(u32::MAX);
        let a = alloc_plain_object(&mut u);
        let b = alloc_plain_object(&mut u);
        let h1 = u.identity_hash(a.oop());
        let h2 = u.identity_hash(b.oop());
        assert_ne!(h1, 0);
        assert_ne!(h2, 0);
    }

    #[test]
    fn association_ivar_names() {
        let mut u = boot();
        let ivn = crate::oops::wrappers::ArrayOop::try_from(u.association_klass.inst_var_names())
            .expect("Association inst_var_names is an Array");
        assert_eq!(ivn.len(), 2);
        assert_eq!(ivn.at(0), u.intern(b"key").oop());
        assert_eq!(ivn.at(1), u.intern(b"value").oop());
    }

    #[test]
    fn eden_initial_state() {
        let u = boot();
        assert_eq!(u.eden.start % 8, 0);
        assert!(u.eden.top > u.eden.start);
        assert!(u.eden.top - u.eden.start < 256 * 1024);
        assert_eq!(u.eden.end - u.eden.start, EDEN_SIZE);
    }

    #[test]
    fn verify_heap_post_genesis() {
        let u = boot();
        super::super::verify::verify_heap(&u).expect("post-genesis heap must verify");
    }
}
