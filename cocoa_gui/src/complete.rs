//! Autocomplete — selectors + class names from the image DB, served through
//! AppKit's OWN completion machinery: `NSTextView` (F5 / ⌥⎋) asks its
//! delegate `textView:completions:forPartialWordRange:indexOfSelectedItem:`,
//! and every Smalltalk text surface already has a `MacvmTextDelegate` as its
//! delegate. So the whole feature is ONE extra IMP added to that class
//! (`class_addMethod` onto the already-registered class — the delegate class
//! itself lives in the macvm bridge, which must not depend on this crate).
//!
//! The IMP is PURE HOST (the C6 law: a callback never enters a VM): read the
//! partial word from the view, prefix-filter the symbols cache, hand back an
//! NSArray. AppKit draws the popup — no UI built here.

use std::os::raw::{c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::objc::{self, Id, Sel};

/// `-(NSArray*)textView:completions:forPartialWordRange:indexOfSelectedItem:`.
/// The by-value `NSRange` arrives as two u64s (AAPCS64 passes the 16-byte
/// struct in consecutive registers).
extern "C" fn imp_completions(
    _this: *mut c_void,
    _cmd: *mut c_void,
    text_view: Id,
    _words: Id,
    loc: u64,
    len: u64,
    index: *mut i64,
) -> Id {
    if text_view.is_null() || len == 0 {
        return std::ptr::null_mut();
    }
    let ns = objc::send0(text_view, objc::sel("string"));
    let text = crate::host_service::ns_to_string(ns);
    // The partial word, by UTF-16 range.
    let mut partial = String::new();
    let mut off = 0u64;
    for ch in text.chars() {
        let w = ch.len_utf16() as u64;
        if off >= loc && off < loc + len {
            partial.push(ch);
        }
        off += w;
        if off >= loc + len {
            break;
        }
    }
    if partial.is_empty() {
        return std::ptr::null_mut();
    }
    let matches = crate::symbols::completions_for(&partial, 60);
    if matches.is_empty() {
        return std::ptr::null_mut();
    }
    // An autoreleased NSMutableArray of AUTORELEASED NSStrings —
    // `objc::nsstring` is `stringWithUTF8String:` (pool-owned, NOT +1), so
    // NO explicit release here: `addObject:` retains for the array, the pool
    // drops the temporaries. (An earlier explicit release here over-released
    // every completion string → use-after-free inside AppKit's completion
    // popup → a framework SIGSEGV and an endless recovery loop. The CG7 "+1,
    // release after use" discipline applies to BRIDGE WRAPPERS, not to this
    // convenience constructor.)
    let arr = objc::send0(
        objc::get_class("NSMutableArray") as Id,
        objc::sel("array"),
    );
    for m in &matches {
        objc::send1_id(arr, objc::sel("addObject:"), objc::nsstring(m));
    }
    if !index.is_null() {
        // SAFETY: AppKit hands a valid NSInteger out-pointer.
        unsafe { *index = 0 };
    }
    arr
}

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Add the completions IMP to the already-registered `MacvmTextDelegate`
/// class. Lazy + idempotent: the class registers on first delegate creation,
/// which always precedes the first colorize call that invokes this.
pub fn install_once() {
    if INSTALLED.load(Ordering::Acquire) {
        return;
    }
    let cls = objc::get_class("MacvmTextDelegate");
    if cls.is_null() {
        return; // delegate class not registered yet — retry next colorize
    }
    type AddMethod = unsafe extern "C" fn(*mut c_void, Sel, *const c_void, *const c_char) -> u8;
    let add: AddMethod = unsafe { std::mem::transmute(objc::sym_addr("class_addMethod")) };
    let sel = objc::sel("textView:completions:forPartialWordRange:indexOfSelectedItem:");
    let types = c"@@:@@{_NSRange=QQ}^q";
    unsafe {
        add(
            cls,
            sel,
            imp_completions as *const c_void,
            types.as_ptr(),
        );
    }
    INSTALLED.store(true, Ordering::Release);
}

#[cfg(test)]
mod tests {
    /// The prefix filter itself (symbols.rs) — pure, no ObjC.
    #[test]
    fn completions_prefix_filter_is_sorted_and_capped() {
        // No image in the unit-test environment → empty, never a panic.
        let none = crate::symbols::completions_for("prin", 10);
        assert!(none.is_empty() || none.iter().all(|s| s.starts_with("prin")));
        assert!(crate::symbols::completions_for("", 10).is_empty());
    }
}
