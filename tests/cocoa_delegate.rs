//! Cocoa bridge C6 gate (`docs/cocoa_gui_design.md` §4, §5 Layer 1; sprint CG3):
//! **reverse dispatch** — AppKit calls a Smalltalk delegate/data-source
//! synchronously and gets a return value, dispatched as a *top-level* VM entry,
//! with Layer-1 crash recovery.
//!
//! `harness = false` so `main()` IS the process's real main thread: the UI worker
//! boots in place on main (the CG2 arrangement), publishes its `*mut VmHandle`
//! (the door the C6 trampolines read), mints per-role delegates, and we invoke
//! their selectors **objc_msgSend-style** (through the bridge's own send path,
//! which is exactly what AppKit does) — proving:
//!  1. a `MacvmTableSource` bound to a Smalltalk object answers
//!     `numberOfRowsInTableView:` with a **real integer**, plus the other three
//!     return shapes the gate needs — an object (`@`), a BOOL, and a void;
//!  2. a delegate whose handler **raises** returns the shape's default (0) AND
//!     the next delegate call still dispatches;
//!  3. a **forced SIGSEGV** inside a handler recovers via the callback door's
//!     `sigsetjmp` and the next call succeeds;
//!  4. a **stale** delegate (minted against a since-superseded UI-VM generation)
//!     fails closed.
//!
//! No display and no AppKit UI class needed — the delegate classes are plain
//! `NSObject` subclasses, dispatched on the (real) main thread.

use macvm::embed::{self, VmHandle};
use macvm::runtime::objc_bridge::{
    self, RetKind, SEND_FPR_SLOTS, SEND_GPR_SLOTS, SEND_STACK_SLOTS,
};
use macvm::runtime::objc_delegate;
use macvm::runtime::vm_state::VmOptions;
use macvm::runtime::JitMode;
use std::os::raw::c_void;
use std::path::Path;

/// Boot a UI-worker-style VM in place on THIS thread and layer the conditional
/// Cocoa world (`MacvmDelegate` lives in `world/65_cocoadelegate.mst`, loaded
/// only via `cocoaui.list`). JIT off keeps the fault path deterministic — the
/// foreign-fault handler is armed by `boot` regardless of JIT mode.
fn boot_ui_worker() -> VmHandle {
    let mut vm = VmHandle::boot(
        VmOptions {
            heap_mib: 64,
            jit: JitMode::Off,
            ..Default::default()
        },
        Path::new("world"),
    )
    .expect("boot the UI worker against world/");
    vm.load_list(Path::new("world/cocoaui.list"))
        .expect("the conditional Cocoa GUI world layer must load");
    vm
}

/// Register `receiver_src` (a Smalltalk expression) under a fresh ticket and mint
/// an ObjC delegate of `role` bound to it — the two halves of
/// `MacvmDelegate on:role:`, split so we learn the raw instance pointer to
/// objc_msgSend. Returns the raw ObjC delegate instance pointer.
fn mint(vm: &mut VmHandle, role: &str, receiver_src: &str) -> *mut c_void {
    let ticket: i64 = vm
        .eval(&format!("MacvmDelegate register: ({receiver_src})"))
        .expect("register the delegate receiver")
        .trim()
        .parse()
        .expect("register: answers an integer ticket");
    objc_delegate::new_delegate(role, embed::current_ui_vm_generation(), ticket)
        .unwrap_or_else(|e| panic!("mint {role} delegate: {e}"))
}

/// objc_msgSend the selector for a GPR-returning shape (NSInteger / BOOL): the
/// bridge send reads `x0`. `a`/`b` fill x2/x3 (the first real args after
/// self/_cmd).
fn send_gpr(inst: *mut c_void, selector: &str, a: *mut c_void, b: *mut c_void) -> u64 {
    objc_bridge::try_send(inst, selector, a, b).expect("delegate send must not throw") as u64
}

fn main() {
    let mut vm = boot_ui_worker();

    // Publish the UI worker `VmHandle` on THIS (main) thread — the door the C6
    // trampolines read — and prove the generation bumped off zero (so a delegate
    // can never be minted at generation 0 and pass the stale check by accident).
    embed::publish_ui_vm(&mut vm as *mut VmHandle);
    assert!(
        !embed::ui_vm().is_null(),
        "the UI worker VM must be published"
    );
    let gen0 = embed::current_ui_vm_generation();
    assert!(gen0 >= 1, "publishing a UI worker must bump the generation");

    // A data source that answers from a local snapshot (design §4.1): a row
    // count (NSInteger) and a per-row value (an object). One MacvmTableSource
    // instance answers BOTH selectors (each is registered on the class).
    vm.exec(
        "Object subclass: CG3Model [ | rows | \
           CG3Model class >> rows: n [ ^self new setRows: n ] \
           setRows: n [ rows := n ] \
           numberOfRowsInTableView: aTv [ ^rows ] \
           tableView: aTv objectValueForTableColumn: aCol row: aRow [ ^'row-', aRow printString ] ]",
    )
    .expect("define the table model");
    let table = mint(&mut vm, "table", "CG3Model rows: 5");

    // (1a) NSInteger return — the headline: a real integer, dispatched as a
    // top-level VM entry, comes back through objc_msgSend.
    assert_eq!(
        send_gpr(
            table,
            "numberOfRowsInTableView:",
            std::ptr::null_mut(),
            std::ptr::null_mut()
        ),
        5,
        "the data source must answer numberOfRowsInTableView: with a real integer"
    );

    // (1b) Object (`@`) return — a Smalltalk String marshals to a fresh NSString.
    // Needs three args (table, column, row=2), so the full register model.
    let mut gpr = [0u64; SEND_GPR_SLOTS];
    gpr[2] = 2; // row index — an NSInteger in x4
    let out = objc_bridge::try_send_full(
        table,
        "tableView:objectValueForTableColumn:row:",
        &gpr,
        &[0.0; SEND_FPR_SLOTS],
        &[0u64; SEND_STACK_SLOTS],
        RetKind::Gpr,
    )
    .expect("objectValue send must not throw");
    let ns = out.gpr[0] as *mut c_void;
    assert!(
        !ns.is_null(),
        "an object-returning data source must answer a non-nil id"
    );
    let bytes = objc_bridge::nsstring_utf8_bytes(ns).expect("the answer must be a real NSString");
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        "row-2",
        "the object value must be the String the Smalltalk handler built"
    );

    // (1c) BOOL + (1d) void — a window delegate. windowShouldClose: → YES;
    // windowWillClose: (void) runs for effect, proven by a counter.
    vm.exec(
        "Object subclass: CG3Win [ <classVars: Closes> \
           CG3Win class >> resetCloses [ Closes := 0 ] \
           CG3Win class >> closes [ ^Closes ] \
           CG3Win class >> noteClose [ Closes := (Closes ifNil: [ 0 ]) + 1 ] \
           windowShouldClose: aWin [ ^true ] \
           windowWillClose: aNote [ CG3Win noteClose ] ]",
    )
    .expect("define the window delegate");
    vm.exec("CG3Win resetCloses.").expect("reset");
    let win = mint(&mut vm, "window", "CG3Win new");
    assert_eq!(
        send_gpr(
            win,
            "windowShouldClose:",
            std::ptr::null_mut(),
            std::ptr::null_mut()
        ) & 0xFF,
        1,
        "windowShouldClose: must answer BOOL YES"
    );
    let _ = send_gpr(
        win,
        "windowWillClose:",
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    ); // void
    assert_eq!(
        vm.eval("CG3Win closes").expect("closes").trim(),
        "1",
        "the void windowWillClose: delegate must have run its side effect"
    );

    // (2) A raising handler returns the shape default (0), and — critically —
    // the VM recovers cleanly so the NEXT delegate call still dispatches
    // (design §5 Layer 1).
    vm.exec(
        "Object subclass: CG3Boom [ \
           numberOfRowsInTableView: aTv [ ^self error: 'boom: a delegate handler raised' ] ]",
    )
    .expect("define the raising model");
    let boom = mint(&mut vm, "table", "CG3Boom new");
    assert_eq!(
        send_gpr(
            boom,
            "numberOfRowsInTableView:",
            std::ptr::null_mut(),
            std::ptr::null_mut()
        ),
        0,
        "a raising delegate handler must answer the shape default (0 rows)"
    );
    assert_eq!(
        send_gpr(
            table,
            "numberOfRowsInTableView:",
            std::ptr::null_mut(),
            std::ptr::null_mut()
        ),
        5,
        "after a raising handler, the next delegate call must still dispatch"
    );

    // (3) A forced native fault (SIGSEGV) inside a handler — a bad Alien deref of
    // an unmapped low address (the S20/S21 mechanism) — recovers via the callback
    // door's sigsetjmp, answers the default, and the next call succeeds.
    vm.exec(
        "Object subclass: CG3Segv [ \
           numberOfRowsInTableView: aTv [ ^(Alien forAddress: 8 size: 8) byteAt: 1 ] ]",
    )
    .expect("define the faulting model");
    let segv = mint(&mut vm, "table", "CG3Segv new");
    assert_eq!(
        send_gpr(
            segv,
            "numberOfRowsInTableView:",
            std::ptr::null_mut(),
            std::ptr::null_mut()
        ),
        0,
        "a forced SIGSEGV in a delegate handler must recover to the shape default"
    );
    assert_eq!(
        send_gpr(
            table,
            "numberOfRowsInTableView:",
            std::ptr::null_mut(),
            std::ptr::null_mut()
        ),
        5,
        "after a recovered SIGSEGV, the next delegate call must still dispatch"
    );

    // (CG4) The action-target role: a menu item / button (a Workspace ⌘P/⌘D)
    // dispatches SYNCHRONOUSLY on the main thread through a `MacvmActionTarget`
    // delegate answering `macvmPrintIt:`/`macvmDoIt:` — the correct mechanism for
    // a UI-worker-LOCAL control (a C4 `Cocoa action:` would post to the primary,
    // unable to read the local NSTextView). This proves the dispatch seam; the
    // shipDoit → uiRequest → primary-eval → reply round-trip is proven headless in
    // embed.rs. (`0` return is the void shape's default from the callback door.)
    vm.exec(
        "Object subclass: CG4Act [ <classVars: N Which> \
           CG4Act class >> reset [ N := 0. Which := nil ] \
           CG4Act class >> n [ ^N ] \
           CG4Act class >> which [ ^Which ] \
           CG4Act class >> bump: w [ N := (N ifNil: [ 0 ]) + 1. Which := w ] \
           macvmDoIt: sender [ CG4Act bump: #doIt ] \
           macvmPrintIt: sender [ CG4Act bump: #printIt ] ]",
    )
    .expect("define the action-target receiver");
    vm.exec("CG4Act reset.").expect("reset");
    let act = mint(&mut vm, "action", "CG4Act new");
    let _ = send_gpr(act, "macvmPrintIt:", std::ptr::null_mut(), std::ptr::null_mut());
    assert_eq!(
        vm.eval("CG4Act n").expect("n").trim(),
        "1",
        "a Print It menu action must dispatch synchronously to macvmPrintIt:"
    );
    assert_eq!(
        vm.eval("CG4Act which").expect("which").trim(),
        "#printIt",
        "the right action selector must have run"
    );
    let _ = send_gpr(act, "macvmDoIt:", std::ptr::null_mut(), std::ptr::null_mut());
    assert_eq!(
        vm.eval("CG4Act n").expect("n").trim(),
        "2",
        "a Do It menu action must also dispatch (both selectors on one target)"
    );
    assert_eq!(
        vm.eval("CG4Act which").expect("which").trim(),
        "#doIt"
    );

    // (CG7) The outline data source's item-producing half: `child:ofItem:`
    // answers a handle object (an NSString keyed by tree path) that AppKit
    // hands BACK as `item` in every later call — the full round-trip is the
    // property under test, plus the pointer STABILITY NSOutlineView's item
    // identity model depends on (same node → same handle id, so the world side
    // must cache handles per path, keyed by Symbol — String keys would hash by
    // identity and mint fresh NSStrings every call).
    vm.exec(
        "Object subclass: CG7Tree [ <classVars: Handles Sels> \
           CG7Tree class >> reset [ Handles := Dictionary new. Sels := 0 ] \
           CG7Tree class >> sels [ ^Sels ] \
           CG7Tree class >> noteSel [ Sels := (Sels ifNil: [ 0 ]) + 1 ] \
           CG7Tree class >> handleFor: path [ \
               | k h | k := path asSymbol. \
               h := Handles at: k ifAbsent: [ nil ]. \
               h isNil ifTrue: [ h := Cocoa nsString: path. Handles at: k put: h ]. \
               ^h ] \
           pathOf: item [ ^item isNil ifTrue: [ '' ] ifFalse: [ item sendString: 'description' ] ] \
           outlineView: ov numberOfChildrenOfItem: item [ \
               | p | p := self pathOf: item. \
               p = '' ifTrue: [ ^2 ]. \
               p = '1' ifTrue: [ ^3 ]. \
               ^0 ] \
           outlineView: ov isItemExpandable: item [ \
               ^(self outlineView: ov numberOfChildrenOfItem: item) > 0 ] \
           outlineView: ov child: i ofItem: item [ \
               | p | p := self pathOf: item. \
               ^CG7Tree handleFor: (p = '' ifTrue: [ i printString ] \
                                          ifFalse: [ p , '.' , i printString ]) ] \
           outlineView: ov objectValueForTableColumn: col byItem: item [ \
               ^'label-' , (self pathOf: item) ] \
           outlineViewSelectionDidChange: aNote [ CG7Tree noteSel ] ]",
    )
    .expect("define the outline tree model");
    vm.exec("CG7Tree reset.").expect("reset the tree model");
    let tree = mint(&mut vm, "outline", "CG7Tree new");

    // child 1 of the ROOT (item = nil crosses as NULL): a real NSString handle.
    let mut gpr = [0u64; SEND_GPR_SLOTS];
    gpr[1] = 1; // the child INDEX (an NSInteger between two ids)
    let out = objc_bridge::try_send_full(
        tree,
        "outlineView:child:ofItem:",
        &gpr,
        &[0.0; SEND_FPR_SLOTS],
        &[0u64; SEND_STACK_SLOTS],
        RetKind::Gpr,
    )
    .expect("child:ofItem: send must not throw");
    let h1 = out.gpr[0] as *mut c_void;
    assert!(!h1.is_null(), "child:ofItem: must answer a handle id");
    assert_eq!(
        String::from_utf8_lossy(&objc_bridge::nsstring_utf8_bytes(h1).expect("a real NSString")),
        "1",
        "the root's child 1 handle is its tree path"
    );

    // Hand the handle BACK as the item: the node resolves by content.
    assert_eq!(
        send_gpr(
            tree,
            "outlineView:numberOfChildrenOfItem:",
            std::ptr::null_mut(),
            h1
        ),
        3,
        "the handed-back item must resolve to ITS node (3 children)"
    );
    assert_eq!(
        send_gpr(
            tree,
            "outlineView:isItemExpandable:",
            std::ptr::null_mut(),
            h1
        ) & 0xFF,
        1,
        "a node with children is expandable"
    );

    // Item pointer STABILITY: asking for the same child again must answer the
    // SAME id (NSOutlineView tracks expansion/selection by item identity).
    let out2 = objc_bridge::try_send_full(
        tree,
        "outlineView:child:ofItem:",
        &gpr,
        &[0.0; SEND_FPR_SLOTS],
        &[0u64; SEND_STACK_SLOTS],
        RetKind::Gpr,
    )
    .expect("second child:ofItem: send");
    assert_eq!(
        out2.gpr[0] as *mut c_void, h1,
        "the same node must answer the SAME handle id every time"
    );

    // A NESTED child: child 2 of node "1" → path "1.2".
    let mut gpr_n = [0u64; SEND_GPR_SLOTS];
    gpr_n[1] = 2;
    gpr_n[2] = h1 as u64;
    let out3 = objc_bridge::try_send_full(
        tree,
        "outlineView:child:ofItem:",
        &gpr_n,
        &[0.0; SEND_FPR_SLOTS],
        &[0u64; SEND_STACK_SLOTS],
        RetKind::Gpr,
    )
    .expect("nested child:ofItem: send");
    assert_eq!(
        String::from_utf8_lossy(
            &objc_bridge::nsstring_utf8_bytes(out3.gpr[0] as *mut c_void)
                .expect("a real NSString")
        ),
        "1.2",
        "a nested child's handle extends its parent's path"
    );

    // The display value for an item: a Smalltalk String marshaled to NSString.
    let mut gpr_v = [0u64; SEND_GPR_SLOTS];
    gpr_v[2] = h1 as u64; // (outline, column, item)
    let out4 = objc_bridge::try_send_full(
        tree,
        "outlineView:objectValueForTableColumn:byItem:",
        &gpr_v,
        &[0.0; SEND_FPR_SLOTS],
        &[0u64; SEND_STACK_SLOTS],
        RetKind::Gpr,
    )
    .expect("objectValueForTableColumn:byItem: send");
    assert_eq!(
        String::from_utf8_lossy(
            &objc_bridge::nsstring_utf8_bytes(out4.gpr[0] as *mut c_void)
                .expect("a real NSString")
        ),
        "label-1",
        "the display value comes from the node the item resolves to"
    );

    // ItemId fail-closed (CG7 review): `child:ofItem:` produces an object
    // AppKit STORES across run-loop turns, so a handler answering a bare
    // String is refused (NULL) — the Id shape's autoreleased-NSString
    // fallback would hand AppKit a pointer that dangles after the next pool
    // drain. Only a world-retained ObjcRef may cross as an item.
    vm.exec(
        "Object subclass: CG7Bad [ \
           outlineView: ov child: i ofItem: item [ ^'not-a-handle' ] ]",
    )
    .expect("define the misbehaving item producer");
    let bad = mint(&mut vm, "outline", "CG7Bad new");
    let out_bad = objc_bridge::try_send_full(
        bad,
        "outlineView:child:ofItem:",
        &gpr,
        &[0.0; SEND_FPR_SLOTS],
        &[0u64; SEND_STACK_SLOTS],
        RetKind::Gpr,
    )
    .expect("bad child:ofItem: send must not throw");
    assert_eq!(
        out_bad.gpr[0], 0,
        "a bare-String item answer must fail CLOSED (NULL), never an autoreleased NSString"
    );

    // The selection notification (void, delegate-side) lands at the SAME
    // receiver as the data-source rows.
    let _ = send_gpr(
        tree,
        "outlineViewSelectionDidChange:",
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
    assert_eq!(
        vm.eval("CG7Tree sels").expect("sels").trim(),
        "1",
        "outlineViewSelectionDidChange: must dispatch to the model"
    );

    // (4) Stale-fail-closed (design §4.3): re-publishing the UI worker bumps the
    // generation, so every delegate minted at the old generation — a not-yet-
    // closed window's data source after a restart — now fails closed rather than
    // dispatching into what could be a dead VM. Done LAST: it invalidates all the
    // delegates above.
    embed::publish_ui_vm(&mut vm as *mut VmHandle);
    assert!(
        embed::current_ui_vm_generation() > gen0,
        "re-publishing must bump the generation"
    );
    assert_eq!(
        send_gpr(
            table,
            "numberOfRowsInTableView:",
            std::ptr::null_mut(),
            std::ptr::null_mut()
        ),
        0,
        "a delegate from a superseded UI-VM generation must fail closed (0), not dispatch"
    );

    // Clear the door before the VM drops (the trampolines must never read a
    // dangling pointer). Publishing null does not bump the generation.
    embed::publish_ui_vm(std::ptr::null_mut());
    println!("cocoa_delegate: ok (integer + object + BOOL + void returns; raise + SIGSEGV recovery; stale fail-closed)");
}
