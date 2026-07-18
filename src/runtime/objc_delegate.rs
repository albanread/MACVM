//! The Cocoa bridge C6 — **reverse dispatch**: AppKit calls a Smalltalk
//! *delegate/data-source* method **synchronously and gets a return value**
//! (`cocoa_gui_design.md` §4, sprint CG3). This is the one genuinely new bridge
//! capability past C4's fire-and-forget `MacvmAction` (`objc_bridge.rs`): a data
//! source must answer `numberOfRowsInTableView:` with a real integer NOW, to
//! paint — C4 cannot.
//!
//! The mechanism (design §4.2, chosen over `forwardInvocation:` by both design
//! reviews): a **small family of per-role ObjC delegate classes**
//! (`MacvmWindowDelegate`, `MacvmTextDelegate`, `MacvmTableSource`,
//! `MacvmOutlineSource`), each `class_addMethod`-registered with ONLY the
//! selectors that role answers — so `respondsToSelector:` is natively correct
//! with no allow-list. Each IMP is a typed `extern "C" fn` that reads its
//! ABI-delivered arguments, unmarshals each into a Smalltalk oop
//! (`objc_bridge::wrap` — id args cross as freshly retained `ObjcRef`s), and runs
//! `delegate perform: selector withArguments: args` — the reflective RPC
//! primitive (prim 64) reused verbatim — then marshals the result back.
//!
//! **The keystone (design §3/R1):** the UI worker VM sits quiescent parked in
//! `[NSApp run]` on the main thread, so an AppKit callback is NOT mid-doit — it
//! is a **top-level VM entry**. Each IMP reads the thread-local `*mut VmHandle`
//! ([`crate::embed::ui_vm`]) and dispatches through
//! [`crate::embed::VmHandle::dispatch_callback`], the SAME per-entry `sigsetjmp`
//! recovery door `eval` uses. That is what makes Layer-1 recovery (design §5)
//! free: a handler that `error:`s, or a native fault in our marshalling / a bad
//! `Alien` in the handler, unwinds back to the trampoline, which returns the
//! shape's **defined default** (`0` rows / `NO` / `nil` — all zero) and the run
//! loop pumps on.
//!
//! **No oop is ever stored ObjC-side (contract §2 clause 2).** The process-wide
//! [`DELEGATES`] registry maps an ObjC delegate instance pointer → a
//! [`DelegateEntry`] carrying only a VM-generation tag + a **ticket** (a plain
//! integer). The Smalltalk delegate *object* lives in a GC-rooted class-var
//! Dictionary on the world-side `MacvmDelegate` class (ticket → receiver — the
//! C4 `Actions` pattern), reached by name at dispatch time; the ObjC selector
//! IS the Smalltalk selector, so no explicit selector map is needed. A **stale**
//! delegate — registered under a UI worker that has since been restarted (CG7),
//! its generation no longer current — fails **closed** (returns the default),
//! never dispatching into a dead VM (design §4.3).

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_void};
use std::sync::OnceLock;

use crate::oops::wrappers::{ArrayOop, ByteArrayOop, KlassOop, MemOop};
use crate::oops::Oop;
use crate::runtime::vm_state::VmState;

// ── the instance→ticket registry (design §4.3) ──────────────────────────────

/// What a registered delegate instance names: the UI-VM generation it was minted
/// under (the stale-fail-closed guard) + its ticket into the world-side
/// receiver Dictionary. Deliberately NO oop — the §2 contract forbids storing a
/// (movable) oop ObjC-side; the ticket survives every GC trivially.
#[derive(Clone, Copy)]
struct DelegateEntry {
    /// The [`crate::embed::current_ui_vm_generation`] live when this delegate was
    /// minted. A callback refuses to dispatch if it no longer matches (design
    /// §4.3): a delegate from a not-yet-closed window after a UI-worker restart
    /// must fail closed, never dispatch into the dead VM.
    gen: u64,
    /// The world-side `MacvmDelegate` ticket → its Smalltalk receiver.
    ticket: i64,
}

/// Instance pointer → entry. Same `OnceLock<RwLock<HashMap>>` shape as C4's
/// `ACTIONS` (`objc_bridge.rs`). Entries live for the process (tickets never
/// reused); a stale entry fails closed at dispatch rather than being reclaimed.
static DELEGATES: OnceLock<std::sync::RwLock<HashMap<usize, DelegateEntry>>> = OnceLock::new();

fn lookup_entry(this: *mut c_void) -> Option<DelegateEntry> {
    DELEGATES.get()?.read().ok()?.get(&(this as usize)).copied()
}

/// Test/introspection: how many delegate instances are registered.
pub fn registered_count() -> usize {
    DELEGATES
        .get()
        .and_then(|r| r.read().ok().map(|m| m.len()))
        .unwrap_or(0)
}

// ── marshalling vocabulary (reuses objc_bridge's classify_* shapes) ─────────

/// One ABI-delivered callback argument, tagged with how to marshal it into a
/// Smalltalk oop. The vocabulary is exactly `objc_bridge::classify_arg`'s: an
/// object (`@`/`#`) crosses as an `ObjcRef` (or `nil`), an integer (`q`) as a
/// `SmallInteger`. Grown row-by-row as later roles need more shapes.
#[derive(Clone, Copy)]
enum ArgVal {
    /// `@` / `#` — an object id: wrapped into an `ObjcRef` (nil for NULL).
    Id(*mut c_void),
    /// `q` — an `NSInteger`: a `SmallInteger`.
    Int(i64),
}

/// A callback's return shape (`objc_bridge::classify_ret`'s vocabulary): how to
/// marshal the Smalltalk result back to the ABI return register. The
/// fail-closed default is `0` for EVERY shape (`NO` / `0` rows / `nil` id), which
/// is why [`dispatch`] can hand a single `0` default to the recovery door.
#[derive(Clone, Copy)]
enum RetShape {
    /// `v` — nothing read back.
    Void,
    /// `B` — a Boolean (`true` → 1, everything else → 0).
    Bool,
    /// `q` — an `NSInteger` from a `SmallInteger` (0 if the handler answered a
    /// non-integer).
    Int,
    /// `@` — an object id: a `String` becomes a fresh (+0, borrowed) `NSString`;
    /// an `ObjcRef` yields its raw id; `nil`/anything else → NULL.
    Id,
}

// ── the generic dispatcher: one top-level entry per callback ─────────────────

/// The shared body of every IMP: resolve the delegate, guard staleness, read the
/// thread-local UI worker `VmHandle`, and run the handler as a **top-level entry**
/// through the recovery door. Returns the marshaled native result, or the
/// fail-closed default `0` on ANY failure (unknown instance, stale VM
/// generation, no published VM, a handler that raised, or a recovered native
/// fault — the last two handled inside [`crate::embed::VmHandle::dispatch_callback`]).
fn dispatch(this: *mut c_void, selector: &str, args: &[ArgVal], ret: RetShape) -> u64 {
    let Some(entry) = lookup_entry(this) else {
        return 0; // never registered — a callback on an unknown instance
    };
    // Stale delegate: minted against a UI worker since restarted (design §4.3).
    if entry.gen != crate::embed::current_ui_vm_generation() {
        return 0;
    }
    let vmp = crate::embed::ui_vm();
    if vmp.is_null() {
        return 0; // no UI worker published on this thread — fail closed
    }
    // Re-entrancy guard (CG3 review): fail CLOSED if a delegate callback is
    // already running on this thread, BEFORE the `&mut *vmp` re-borrow below. A
    // nested AppKit callback (a modal/tracking run loop pumped from inside a
    // handler — CG5+) would otherwise alias a live `&mut VmState` and clobber the
    // shared recovery slot / idle baseline. No such nesting path exists in CG3;
    // this keeps the door sound in advance. `dispatch_callback` re-checks the
    // same flag as a second line of defense and owns its lifecycle.
    if crate::embed::callback_active() {
        return 0;
    }
    // SAFETY: the UI worker's `VmHandle` outlives the run loop (design §3 step 4;
    // dropped only at process exit, or re-published across a CG7 restart), and it
    // was published on THIS (main) thread. The VM is quiescent between callbacks
    // (the re-entrancy guard above enforces this), so this is a fresh top-level
    // entry, never a re-entrant borrow of a live `&mut VmState`.
    let handle: &mut crate::embed::VmHandle = unsafe { &mut *vmp };
    handle.dispatch_callback(0, |vm| {
        perform_delegate(vm, entry.ticket, selector, args, ret)
    })
}

/// Inside the recovery door (a live `&mut VmState`): marshal the args, build the
/// `perform:withArguments:` call, run the world-side dispatch, marshal the
/// result. Any `error:`/DNU or native fault here unwinds to `dispatch_callback`,
/// which answers the default — so returning `0` on a resolution miss is the same
/// fail-closed outcome.
fn perform_delegate(
    vm: &mut VmState,
    ticket: i64,
    selector: &str,
    args: &[ArgVal],
    ret: RetShape,
) -> u64 {
    use crate::memory::handles::HandleScope;
    use crate::oops::smi::SmallInt;
    use crate::oops::wrappers::SymbolOop;

    let scope = HandleScope::enter(vm);
    // Marshal each ABI arg to a Smalltalk oop, rooting as we go: wrapping an id
    // allocates an `ObjcRef`, which could move an earlier arg (the perform-prim
    // handle discipline).
    let mut arg_hs = Vec::with_capacity(args.len());
    for a in args {
        let o = match *a {
            ArgVal::Id(p) => crate::runtime::objc_bridge::wrap(vm, p),
            ArgVal::Int(n) => SmallInt::new(n).oop(),
        };
        arg_hs.push(scope.handle(vm, o));
    }
    // The ObjC selector name IS the Smalltalk selector the delegate implements.
    let sel_oop = vm.universe.intern(selector.as_bytes()).oop();
    let sel_h = scope.handle(vm, sel_oop);
    // The world-side dispatch entry point selector, rooted across the allocations.
    let disp_oop = vm
        .universe
        .intern(b"dispatchTicket:selector:arguments:")
        .oop();
    let disp_h = scope.handle(vm, disp_oop);
    // The arguments Array (its own allocation — the marshaled args ride handles).
    let arr = crate::memory::alloc::alloc_indexable_oops(vm, vm.universe.array_klass, args.len());
    let arr_h = scope.handle(vm, arr.oop());
    let arr = ArrayOop::try_from(arr_h.get(vm)).expect("fresh args array");
    for (i, h) in arg_hs.iter().enumerate() {
        arr.at_put(i, h.get(vm));
    }
    // The world-side, GC-rooted ticket→receiver registry. Absent (cocoaui.list
    // not loaded, or the class undeclared) → fail closed.
    let Some(cls) = delegate_registry_class(vm) else {
        return 0;
    };
    let cls_h = scope.handle(vm, cls);
    let k = crate::runtime::primitives::klass_of(vm, cls_h.get(vm));
    let disp = SymbolOop::try_from(disp_h.get(vm)).expect("interned selector is a Symbol");
    let Some(m) = crate::runtime::lookup::lookup(vm, k, disp) else {
        return 0; // MacvmDelegate class does not implement the dispatch method
    };
    // Nothing allocates between here and the reentrant run's own push.
    let argv = [SmallInt::new(ticket).oop(), sel_h.get(vm), arr_h.get(vm)];
    let result = crate::interpreter::run_method_reentrant(vm, m, cls_h.get(vm), &argv);
    marshal_ret(vm, result, ret)
}

/// The world-side `MacvmDelegate` class oop (its class-var Dictionary holds the
/// ticket→receiver map), resolved by name from the globals. `None` if the class
/// is not declared (the conditional `cocoaui.list` layer was not loaded).
fn delegate_registry_class(vm: &mut VmState) -> Option<Oop> {
    let name = vm.universe.intern(b"MacvmDelegate");
    let assoc = crate::runtime::globals::global_lookup(vm, name)?;
    let value = MemOop::try_from(assoc)?.body_oop(1);
    KlassOop::try_from(value).map(|_| value)
}

/// Marshal a handler's result oop back to the ABI return register per the return
/// shape. The fail-closed default is `0` throughout.
fn marshal_ret(vm: &mut VmState, result: Oop, ret: RetShape) -> u64 {
    match ret {
        RetShape::Void => 0,
        RetShape::Bool => u64::from(result.raw() == vm.universe.true_obj.raw()),
        RetShape::Int => crate::oops::smi::SmallInt::try_from(result)
            .map(|s| s.value() as u64)
            .unwrap_or(0),
        RetShape::Id => marshal_id_ret(vm, result),
    }
}

/// A `@`-returning data-source value: an `ObjcRef` yields its (borrowed) raw id;
/// a `String` becomes a fresh +0 autoreleased `NSString` (borrowed by AppKit
/// under main's run-loop pool — valid for the return, retained by AppKit only if
/// it keeps it, so no bridge retain); `nil`/anything else → NULL.
fn marshal_id_ret(vm: &mut VmState, result: Oop) -> u64 {
    if result.raw() == vm.universe.nil_obj.raw() {
        return 0;
    }
    if let Some(id) = crate::runtime::objc_bridge::read_id(vm, result) {
        return id as u64;
    }
    if let Some(bytes) = string_bytes(vm, result) {
        if let Ok(ns) = crate::runtime::objc_bridge::nsstring_from(&bytes) {
            return ns as u64;
        }
    }
    0
}

/// A `String` oop's UTF-8 bytes, or `None` if `o` isn't a `String`.
fn string_bytes(vm: &VmState, o: Oop) -> Option<Vec<u8>> {
    let m = MemOop::try_from(o)?;
    if m.klass().oop().raw() != vm.universe.string_klass.oop().raw() {
        return None;
    }
    let b = ByteArrayOop::try_from(o)?;
    let mut buf = Vec::new();
    b.copy_bytes_out(&mut buf);
    Some(buf)
}

// ── the typed IMPs (design §4.2) ─────────────────────────────────────────────
//
// Each is one `extern "C" fn` whose signature IS the selector's `@encode` shape
// (self, _cmd, then the selector args), delegating to `dispatch` with its own
// selector, marshalled args, and return shape. Registered on exactly one role
// class, so `respondsToSelector:` needs no allow-list. Cover the four return
// classes the acceptance gate and later sprints need: BOOL, NSInteger, void, id.

// MacvmWindowDelegate — `NSWindowDelegate`.
extern "C" fn imp_window_should_close(
    this: *mut c_void,
    _cmd: *mut c_void,
    sender: *mut c_void,
) -> u8 {
    dispatch(
        this,
        "windowShouldClose:",
        &[ArgVal::Id(sender)],
        RetShape::Bool,
    ) as u8
}
extern "C" fn imp_window_will_close(this: *mut c_void, _cmd: *mut c_void, note: *mut c_void) {
    dispatch(
        this,
        "windowWillClose:",
        &[ArgVal::Id(note)],
        RetShape::Void,
    );
}

// MacvmTextDelegate — `NSTextDelegate`/`NSTextViewDelegate`.
extern "C" fn imp_text_did_change(this: *mut c_void, _cmd: *mut c_void, note: *mut c_void) {
    dispatch(this, "textDidChange:", &[ArgVal::Id(note)], RetShape::Void);
}

// MacvmActionTarget — a menu item / button target/action (Cocoa GUI CG4): a
// void `-(void)action:(id)sender` the UI worker answers SYNCHRONOUSLY on the
// main thread. This is the correct mechanism for a UI-worker-LOCAL control (a
// Workspace ⌘P/⌘D) — unlike C4 `Cocoa action:`, which posts a fire-and-forget
// envelope to the primary and so could not read the local NSTextView and ship
// its text. Two named selectors so ⌘P and ⌘D can be distinct menu items on one
// target, plus ONE generic `macvmAction:` (CG5) so an arbitrary NUMBER of
// controls (a toolbar's N buttons) each mint their OWN `MacvmActionTarget`
// (its own ticket → its own Smalltalk receiver, `MacvmDelegate
// actionTargetOn:`), all sharing this one IMP — dispatch already routes by
// ticket, so no new IMP is needed per button; the receiver just implements
// `macvmAction: sender`.
extern "C" fn imp_menu_do_it(this: *mut c_void, _cmd: *mut c_void, sender: *mut c_void) {
    dispatch(this, "macvmDoIt:", &[ArgVal::Id(sender)], RetShape::Void);
}
extern "C" fn imp_menu_print_it(this: *mut c_void, _cmd: *mut c_void, sender: *mut c_void) {
    dispatch(this, "macvmPrintIt:", &[ArgVal::Id(sender)], RetShape::Void);
}
extern "C" fn imp_menu_action(this: *mut c_void, _cmd: *mut c_void, sender: *mut c_void) {
    dispatch(this, "macvmAction:", &[ArgVal::Id(sender)], RetShape::Void);
}

// MacvmTableSource — `NSTableViewDataSource`.
extern "C" fn imp_number_of_rows(this: *mut c_void, _cmd: *mut c_void, table: *mut c_void) -> i64 {
    dispatch(
        this,
        "numberOfRowsInTableView:",
        &[ArgVal::Id(table)],
        RetShape::Int,
    ) as i64
}
extern "C" fn imp_object_value(
    this: *mut c_void,
    _cmd: *mut c_void,
    table: *mut c_void,
    column: *mut c_void,
    row: i64,
) -> *mut c_void {
    dispatch(
        this,
        "tableView:objectValueForTableColumn:row:",
        &[ArgVal::Id(table), ArgVal::Id(column), ArgVal::Int(row)],
        RetShape::Id,
    ) as *mut c_void
}

// MacvmOutlineSource — `NSOutlineViewDataSource`.
extern "C" fn imp_num_children(
    this: *mut c_void,
    _cmd: *mut c_void,
    outline: *mut c_void,
    item: *mut c_void,
) -> i64 {
    dispatch(
        this,
        "outlineView:numberOfChildrenOfItem:",
        &[ArgVal::Id(outline), ArgVal::Id(item)],
        RetShape::Int,
    ) as i64
}
extern "C" fn imp_is_expandable(
    this: *mut c_void,
    _cmd: *mut c_void,
    outline: *mut c_void,
    item: *mut c_void,
) -> u8 {
    dispatch(
        this,
        "outlineView:isItemExpandable:",
        &[ArgVal::Id(outline), ArgVal::Id(item)],
        RetShape::Bool,
    ) as u8
}

// ── per-role class registration (the MacvmAction pattern, generalized) ───────

/// libobjc's class-pair entry points — plain `dlsym`, exactly as
/// `objc_bridge::macvm_action_class` resolves them (its `resolve` is private to
/// that module; this mirrors it rather than widening its surface).
fn dlsym(name: &str) -> Option<u64> {
    crate::vendor::wfasm::native_macos::dlsym_resolve(None, name)
}

type AllocPair = unsafe extern "C" fn(*mut c_void, *const c_char, usize) -> *mut c_void;
type RegisterPair = unsafe extern "C" fn(*mut c_void);
// `class_addMethod(Class, SEL, IMP, types)`. The IMP is passed as an opaque code
// pointer (`*const c_void`) — each typed IMP transmutes to it at the call site,
// a valid pointer-sized→pointer-sized transmute (`objc_bridge`'s own idiom).
type AddMethod = unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_void, *const c_char) -> u8;

/// Register one per-role delegate class as an `NSObject` subclass carrying
/// exactly `methods` (each `(selector, IMP, @encode types)`), once. `None` on a
/// missing runtime symbol or a name collision (a prior registration owns it).
fn register_class(name: &str, methods: &[(&str, *const c_void, &str)]) -> Option<*mut c_void> {
    let alloc_pair = dlsym("objc_allocateClassPair")?;
    let register_pair = dlsym("objc_registerClassPair")?;
    let add_method = dlsym("class_addMethod")?;
    // NSObject is never AppKit-guarded, so `class_named` resolves it on any
    // thread even under the CG2 Cocoa-mode guard.
    let superclass = crate::runtime::objc_bridge::class_named("NSObject")?;
    let cname = CString::new(name).ok()?;
    let cls =
        unsafe { std::mem::transmute::<u64, AllocPair>(alloc_pair)(superclass, cname.as_ptr(), 0) };
    if cls.is_null() {
        return None; // name collision — a previous registration owns it
    }
    for (selector, imp, types) in methods {
        let sel = crate::runtime::objc_bridge::register_selector(selector)?;
        let ctypes = CString::new(*types).ok()?;
        unsafe {
            std::mem::transmute::<u64, AddMethod>(add_method)(cls, sel, *imp, ctypes.as_ptr());
        }
    }
    unsafe { std::mem::transmute::<u64, RegisterPair>(register_pair)(cls) };
    Some(cls)
}

/// Coerce a typed IMP fn to the opaque code pointer `class_addMethod` wants: `$f`
/// is coerced to its exact fn-pointer type `$t`, then cast to `*const c_void` (a
/// plain fn-pointer→raw-pointer cast — no transmute, no `unsafe`).
macro_rules! imp_ptr {
    ($f:expr, $t:ty) => {
        $f as $t as *const c_void
    };
}

type ImpB1 = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> u8;
type ImpV1 = extern "C" fn(*mut c_void, *mut c_void, *mut c_void);
type ImpQ1 = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i64;
type ImpIdTcr =
    extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void, i64) -> *mut c_void;
type ImpQ2 = extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> i64;
type ImpB2 = extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> u8;

static WINDOW_DELEGATE_CLASS: OnceLock<Option<usize>> = OnceLock::new();
static TEXT_DELEGATE_CLASS: OnceLock<Option<usize>> = OnceLock::new();
static TABLE_SOURCE_CLASS: OnceLock<Option<usize>> = OnceLock::new();
static OUTLINE_SOURCE_CLASS: OnceLock<Option<usize>> = OnceLock::new();
static ACTION_TARGET_CLASS: OnceLock<Option<usize>> = OnceLock::new();

fn window_delegate_class() -> Option<*mut c_void> {
    WINDOW_DELEGATE_CLASS
        .get_or_init(|| {
            register_class(
                "MacvmWindowDelegate",
                &[
                    (
                        "windowShouldClose:",
                        imp_ptr!(imp_window_should_close, ImpB1),
                        "B@:@",
                    ),
                    (
                        "windowWillClose:",
                        imp_ptr!(imp_window_will_close, ImpV1),
                        "v@:@",
                    ),
                ],
            )
            .map(|c| c as usize)
        })
        .map(|p| p as *mut c_void)
}

fn text_delegate_class() -> Option<*mut c_void> {
    TEXT_DELEGATE_CLASS
        .get_or_init(|| {
            register_class(
                "MacvmTextDelegate",
                &[(
                    "textDidChange:",
                    imp_ptr!(imp_text_did_change, ImpV1),
                    "v@:@",
                )],
            )
            .map(|c| c as usize)
        })
        .map(|p| p as *mut c_void)
}

fn table_source_class() -> Option<*mut c_void> {
    TABLE_SOURCE_CLASS
        .get_or_init(|| {
            register_class(
                "MacvmTableSource",
                &[
                    (
                        "numberOfRowsInTableView:",
                        imp_ptr!(imp_number_of_rows, ImpQ1),
                        "q@:@",
                    ),
                    (
                        "tableView:objectValueForTableColumn:row:",
                        imp_ptr!(imp_object_value, ImpIdTcr),
                        "@@:@@q",
                    ),
                ],
            )
            .map(|c| c as usize)
        })
        .map(|p| p as *mut c_void)
}

fn outline_source_class() -> Option<*mut c_void> {
    OUTLINE_SOURCE_CLASS
        .get_or_init(|| {
            register_class(
                "MacvmOutlineSource",
                &[
                    (
                        "outlineView:numberOfChildrenOfItem:",
                        imp_ptr!(imp_num_children, ImpQ2),
                        "q@:@@",
                    ),
                    (
                        "outlineView:isItemExpandable:",
                        imp_ptr!(imp_is_expandable, ImpB2),
                        "B@:@@",
                    ),
                ],
            )
            .map(|c| c as usize)
        })
        .map(|p| p as *mut c_void)
}

fn action_target_class() -> Option<*mut c_void> {
    ACTION_TARGET_CLASS
        .get_or_init(|| {
            register_class(
                "MacvmActionTarget",
                &[
                    ("macvmDoIt:", imp_ptr!(imp_menu_do_it, ImpV1), "v@:@"),
                    ("macvmPrintIt:", imp_ptr!(imp_menu_print_it, ImpV1), "v@:@"),
                    ("macvmAction:", imp_ptr!(imp_menu_action, ImpV1), "v@:@"),
                ],
            )
            .map(|c| c as usize)
        })
        .map(|p| p as *mut c_void)
}

/// The role symbol (`#window`/`#text`/`#table`/`#outline`/`#action`) → its
/// registered delegate class. `None` for an unknown role.
fn role_class(role: &str) -> Option<*mut c_void> {
    match role {
        "window" => window_delegate_class(),
        "text" => text_delegate_class(),
        "table" => table_source_class(),
        "outline" => outline_source_class(),
        "action" => action_target_class(),
        _ => None,
    }
}

/// Mint one delegate instance of `role`'s class, bound (Rust-side) to
/// `(gen, ticket)` — the world-side `MacvmDelegate` owns the ticket→receiver
/// map. Answers a +1 id (alloc/init — the caller wraps with `wrap_owned`).
/// Works from ANY VM role: unlike a C4 action it posts nothing, so it needs no
/// inbox sender — the dispatch is synchronous through the callback door, which
/// lifts C4's primary-only refusal for the UI worker (design §4.3, review item 5).
pub fn new_delegate(role: &str, gen: u64, ticket: i64) -> Result<*mut c_void, String> {
    let cls = role_class(role).ok_or_else(|| {
        format!("unknown delegate role '{role}' (want window/text/table/outline/action)")
    })?;
    let inst = crate::runtime::objc_bridge::try_send(
        cls,
        "alloc",
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    )?;
    let inst = crate::runtime::objc_bridge::try_send(
        inst,
        "init",
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    )?;
    if inst.is_null() {
        return Err(format!("MacvmDelegate ({role}) alloc/init answered nil"));
    }
    DELEGATES
        .get_or_init(|| std::sync::RwLock::new(HashMap::new()))
        .write()
        .map_err(|_| "delegate registry poisoned".to_string())?
        .insert(inst as usize, DelegateEntry { gen, ticket });
    Ok(inst)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stale-fail-closed guard is a pure decision on the entry's generation
    /// vs. the process's current one — provable with no ObjC and no VM. A missing
    /// instance and a generation mismatch both fail closed (answer the default,
    /// dispatched as `0` before any VM entry). The live end-to-end dispatch is
    /// the `harness=false` main-thread gate `tests/cocoa_delegate.rs`.
    #[test]
    fn stale_and_unknown_delegates_fail_closed() {
        // An unknown instance never reaches a VM — 0 straight out of `dispatch`.
        let bogus = 0xDEAD_BEEF_usize as *mut c_void;
        assert_eq!(
            dispatch(bogus, "numberOfRowsInTableView:", &[], RetShape::Int),
            0,
            "an unregistered instance must fail closed"
        );

        // Register a synthetic entry under a generation that is NOT the current
        // one, and prove `dispatch` refuses it before reaching `ui_vm`. (The real
        // mint path needs the ObjC runtime; here we only exercise the guard.)
        let inst = 0x1000_usize as *mut c_void;
        let current = crate::embed::current_ui_vm_generation();
        DELEGATES
            .get_or_init(|| std::sync::RwLock::new(HashMap::new()))
            .write()
            .unwrap()
            .insert(
                inst as usize,
                DelegateEntry {
                    gen: current.wrapping_add(7),
                    ticket: 1,
                },
            );
        assert_eq!(
            dispatch(inst, "numberOfRowsInTableView:", &[], RetShape::Int),
            0,
            "a delegate from a stale VM generation must fail closed"
        );
        // Cleanup so the count-based gate (registered_count) isn't perturbed.
        DELEGATES
            .get()
            .unwrap()
            .write()
            .unwrap()
            .remove(&(inst as usize));
    }

    /// The re-entrancy guard (CG3 review): a callback that arrives while another
    /// is already active on this thread must fail CLOSED (`0`) BEFORE it borrows
    /// the UI worker's `VmHandle`. Proven without a real nested run loop: publish
    /// a non-null but deliberately INVALID `VmHandle` pointer (so the unknown /
    /// stale / null gates all pass), force the "a callback is active" flag, and
    /// assert `dispatch` answers the default — if the guard were missing it would
    /// dereference the sentinel and crash, so a clean `0` proves it short-circuits
    /// before `&mut *vmp`.
    #[test]
    fn reentrant_callback_fails_closed_before_vm_borrow() {
        // Publish FIRST (this bumps the generation), THEN read the now-current
        // generation and register under it — so the entry is NOT stale and the
        // only thing that can make `dispatch` return early is the callback guard.
        let sentinel = 0x1_usize as *mut crate::embed::VmHandle; // page 0 — unmapped
        crate::embed::publish_ui_vm(sentinel);
        let gen = crate::embed::current_ui_vm_generation();
        let inst = 0x2000_usize as *mut c_void;
        DELEGATES
            .get_or_init(|| std::sync::RwLock::new(HashMap::new()))
            .write()
            .unwrap()
            .insert(inst as usize, DelegateEntry { gen, ticket: 1 });

        crate::embed::set_callback_active_for_test(true);
        // Must fail closed at the guard, NOT dereference the 0x1 sentinel.
        assert_eq!(
            dispatch(inst, "numberOfRowsInTableView:", &[], RetShape::Int),
            0,
            "a re-entrant callback must fail closed before borrowing the VmHandle"
        );

        // Restore all thread/process state this test perturbed.
        crate::embed::set_callback_active_for_test(false);
        crate::embed::publish_ui_vm(std::ptr::null_mut());
        DELEGATES
            .get()
            .unwrap()
            .write()
            .unwrap()
            .remove(&(inst as usize));
    }
}
