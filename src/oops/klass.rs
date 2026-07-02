//! `Format` and the klass-object accessors (SPEC §2.3–§2.4). A klass is a
//! fixed-shape object (`KLASS_SIZE_WORDS` words, never indexable), so its
//! accessors read/write via statically-bounded raw body words — never
//! through the general, dynamically-sized [`super::heap`] accessors. This is
//! what breaks the circularity between "compute an object's size" (which
//! needs the object's klass's format/non_indexable_size) and "read the
//! klass's own fields" (which must not itself depend on a computed size).

use super::layout::{
    FORMAT_KIND_MASK, FORMAT_UNTAGGED_BIT, KLASS_CLASS_VARS_INDEX, KLASS_FORMAT_INDEX,
    KLASS_INST_VAR_NAMES_INDEX, KLASS_METHODS_INDEX, KLASS_MIXIN_INDEX, KLASS_NAME_INDEX,
    KLASS_NON_INDEXABLE_SIZE_INDEX, KLASS_SUPERCLASS_INDEX,
};
use super::smi::SmallInt;
use super::wrappers::{KlassOop, MemOop};
use super::Oop;

/// The VM-level object format (SPEC §2.3). Discriminants are pinned — do not
/// renumber; they are stored on the heap.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Format {
    Slots = 0,
    IndexableOops = 1,
    IndexableBytes = 2,
    Method = 3,
    Klass = 4,
    Double = 5,
    Closure = 6,
    Context = 7,
    Process = 8,
}

impl Format {
    fn from_smi_value_checked(v: i64) -> Option<Format> {
        match v & FORMAT_KIND_MASK {
            0 => Some(Format::Slots),
            1 => Some(Format::IndexableOops),
            2 => Some(Format::IndexableBytes),
            3 => Some(Format::Method),
            4 => Some(Format::Klass),
            5 => Some(Format::Double),
            6 => Some(Format::Closure),
            7 => Some(Format::Context),
            8 => Some(Format::Process),
            _ => None,
        }
    }

    fn from_smi_value(v: i64) -> Format {
        Self::from_smi_value_checked(v)
            .unwrap_or_else(|| panic!("invalid format discriminant in smi value {v:#x}"))
    }

    fn to_smi_value(self, untagged: bool) -> i64 {
        let base = self as i64;
        if untagged {
            base | FORMAT_UNTAGGED_BIT
        } else {
            base
        }
    }
}

/// Reads the `format` field directly off a raw (unvalidated) klass-field
/// oop — used by the typed wrappers' `try_from` to check "is this oop's
/// klass format X" WITHOUT recursively re-validating that the klass field
/// is itself a fully-formed `KlassOop`. That recursive form does not
/// terminate: at the root of the klass hierarchy, `Metaclass` and
/// `"Metaclass class"` are each other's klass (SPEC §3.2's genesis knot),
/// so "is X's klass validly klass-shaped" would ask "is X's klass's klass
/// validly klass-shaped" forever. This shallow read instead trusts the VM
/// invariant that any object's klass field, if mem-tagged, is laid out with
/// the klass body shape — established once at genesis and preserved by
/// construction thereafter, exactly as the reference VMs assume. Never
/// panics: returns `None` for anything that doesn't parse as a klass.
pub(crate) fn format_of_klass_field(k_field: Oop) -> Option<Format> {
    let m = MemOop::try_from(k_field)?;
    let raw = m.raw_body_word(KLASS_FORMAT_INDEX);
    let smi = SmallInt::try_from(Oop::from_raw_unchecked(raw))?;
    Format::from_smi_value_checked(smi.value())
}

impl KlassOop {
    /// This klass's own klass (its metaclass, or `metaclass_klass` if `self`
    /// is itself a metaclass). Convenience forward to `MemOop::klass`, used
    /// throughout genesis and by `try_from`'s format checks.
    pub fn klass(self) -> KlassOop {
        self.as_mem().klass()
    }

    pub fn set_klass(self, k: KlassOop) {
        self.as_mem().set_klass(k);
    }

    fn format_smi_value(self) -> i64 {
        let raw = self.as_mem().raw_body_word(KLASS_FORMAT_INDEX);
        SmallInt::try_from(Oop::from_raw(raw))
            .expect("klass format field is not a smi")
            .value()
    }

    pub fn format(self) -> Format {
        Format::from_smi_value(self.format_smi_value())
    }

    pub fn has_untagged_contents(self) -> bool {
        self.format_smi_value() & FORMAT_UNTAGGED_BIT != 0
    }

    pub fn set_format(self, f: Format, untagged: bool) {
        let v = f.to_smi_value(untagged);
        self.as_mem()
            .set_raw_body_word(KLASS_FORMAT_INDEX, SmallInt::new(v).oop().raw());
    }

    /// Total instance size in words, including the header, of the fixed
    /// (non-indexable) prefix. SPEC §2.3's rule stated for `Slots`, pinned
    /// here for every format (`sprint_s01_detail.md` §Design).
    pub fn non_indexable_size(self) -> usize {
        let raw = self.as_mem().raw_body_word(KLASS_NON_INDEXABLE_SIZE_INDEX);
        SmallInt::try_from(Oop::from_raw(raw))
            .expect("klass non_indexable_size field is not a smi")
            .value() as usize
    }

    pub fn set_non_indexable_size(self, words: usize) {
        self.as_mem().set_raw_body_word(
            KLASS_NON_INDEXABLE_SIZE_INDEX,
            SmallInt::new(words as i64).oop().raw(),
        );
    }

    pub fn superclass(self) -> Oop {
        Oop::from_raw(self.as_mem().raw_body_word(KLASS_SUPERCLASS_INDEX))
    }

    pub fn set_superclass(self, o: Oop) {
        self.as_mem()
            .set_raw_body_word(KLASS_SUPERCLASS_INDEX, o.raw());
    }

    pub fn methods(self) -> Oop {
        Oop::from_raw(self.as_mem().raw_body_word(KLASS_METHODS_INDEX))
    }

    pub fn set_methods(self, o: Oop) {
        self.as_mem()
            .set_raw_body_word(KLASS_METHODS_INDEX, o.raw());
    }

    pub fn name(self) -> Oop {
        Oop::from_raw(self.as_mem().raw_body_word(KLASS_NAME_INDEX))
    }

    pub fn set_name(self, o: Oop) {
        self.as_mem().set_raw_body_word(KLASS_NAME_INDEX, o.raw());
    }

    pub fn inst_var_names(self) -> Oop {
        Oop::from_raw(self.as_mem().raw_body_word(KLASS_INST_VAR_NAMES_INDEX))
    }

    pub fn set_inst_var_names(self, o: Oop) {
        self.as_mem()
            .set_raw_body_word(KLASS_INST_VAR_NAMES_INDEX, o.raw());
    }

    pub fn class_vars(self) -> Oop {
        Oop::from_raw(self.as_mem().raw_body_word(KLASS_CLASS_VARS_INDEX))
    }

    pub fn set_class_vars(self, o: Oop) {
        self.as_mem()
            .set_raw_body_word(KLASS_CLASS_VARS_INDEX, o.raw());
    }

    pub fn mixin(self) -> Oop {
        Oop::from_raw(self.as_mem().raw_body_word(KLASS_MIXIN_INDEX))
    }

    pub fn set_mixin(self, o: Oop) {
        self.as_mem().set_raw_body_word(KLASS_MIXIN_INDEX, o.raw());
    }
}
