//! The GUI↔VM threading scaffold from `../../docs/SPEC.md` §16.1 (decision
//! A18): "AppKit owns the main thread; the VM runs on a dedicated worker
//! thread... GUI→VM requests (doits, mirror queries, accepts) are queued
//! over a channel and executed between interpreter turns; VM→GUI output
//! (transcript, rendered fragments) returns via a channel drained on the
//! main thread into `evaluateJavaScript`."
//!
//! There is no real VM to embed yet (`VmHandle`, SPEC §16.2, needs core
//! eval — G2 per `../PLAN.md` §6) — so `worker_loop` below stands in for
//! `VmHandle::eval`, doing exactly what `main.rs`'s inline G1 stub used to
//! do (echo the doit's source text). What's real here is the *threading*:
//! this is a genuine `std::thread::spawn` OS thread, talking to the main
//! thread only through the two channels below, with no shared mutable
//! state and no direct cross-thread AppKit/WebKit calls — the actual shape
//! G2 will need, just with a stub instead of `VmHandle` on the worker end.
//! Only `doit` is routed here: toolbar/navigate are pure UI/file-loading
//! concerns (`NavState`), not VM work, so per SPEC §16.1 they stay on the
//! main thread untouched.
//!
//! Three things this scaffold is already shaped for, even though nothing
//! exercises them yet (no real VM, no smappl-built widgets):
//!
//! **Bulk payloads (RGBA pixel blocks, large text)** — `VmResponse`'s
//! variants own their data outright (`String`, and future `Vec<u8>`/
//! `Arc<[u8]>`), so a large buffer crossing `response_tx.send(..)` is a
//! move, not a copy. The open question is *delivery into the DOM*, not the
//! channel: `evaluateJavaScript` takes a JS source string, so a naive
//! approach (base64-inline a pixel buffer into that string) works but
//! doesn't scale to big/frequent images. The planned mechanism instead: a
//! `WKURLSchemeHandler` registered on the webview's configuration serving
//! `macvm-pixels://<id>` by reading straight from a Rust-side buffer — the
//! page just does `<img src="macvm-pixels://42">` and re-fetches on
//! update, no JS-string-serialization involved. (A temp-file-per-frame,
//! matching the existing `.rendered/` pattern, is the fallback if a custom
//! scheme handler proves awkward to wire through this bridge — strictly
//! worse for high-frequency updates but zero new ObjC surface.)
//!
//! **Slow UI thread vs. a fast producer** — not every response should
//! queue. Discrete, one-off events (a transcript line, a single doit
//! result) belong on the ordered `mpsc` channel below: `drain_responses`
//! already applies a whole backlog in one wake, so a burst just gets
//! batched, never dropped or reordered. But a *continuous* stream (a
//! redrawing pixel buffer, a live status string during a long computation)
//! must not queue — if the UI thread falls behind, stale frames piling up
//! and rendering in sequence is wasted work at best. That case wants
//! "latest value wins," which is exactly what [`LatestSlot`] below
//! provides: publishing a new value drops whatever was there, unread or
//! not. Route continuous updates through a `LatestSlot` keyed by widget id
//! (a `Mutex<HashMap<Id, LatestSlot<T>>>`, once there's a real per-widget
//! id space to key on), and discrete ones through the `mpsc` channel — two
//! disciplines, chosen per response kind, not one queue trying to serve
//! both.
//!
//! **Routing a click to the right closure, and locking** — once smappl
//! (`../smappl.md`) is real, each button/widget captures its own action
//! closure at build time (mirroring Strongtalk's own `Button
//! withImage:action:`). The natural home for that closure table is the
//! worker thread itself, *not* a `Mutex` shared with the UI thread: the UI
//! thread never touches closures directly — a click just carries the
//! widget's opaque id (`data-node-id`, same idea `docs/APPS.md` §7's
//! `ToolRegistry` already uses) through `smtk.js` → `on_script_message` →
//! a new `VmRequest` variant (e.g. `InvokeHandler { id }`) across the
//! *existing* request channel. The VM thread looks the id up in its own,
//! entirely private `HashMap<Id, Handler>` and calls it — no lock needed
//! for that table at all, because only one thread ever touches it. This is
//! "share memory by communicating," not "share memory, protect it with a
//! mutex" — the request channel is already the synchronization.

#![allow(dead_code)] // LatestSlot has no producer yet — no bulk/continuous
                      // response exists until a real VM or smappl widget
                      // needs one; see this module's doc comment.

use crate::browser_render::{self, BrowserSelection, SourceEditTarget};
use crate::objc::{self, Id, Sel};
use macvm_mock_vm::MockWorld;
pub use macvm_mock_vm::Side;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;

/// GUI → VM. `Doit` is the only one with a real (stub) handler; the
/// `BrowserSelect*` variants drive the class browser view
/// (`browser_render.rs`) against `macvm-mock-vm`'s invented class world —
/// see that crate's doc comment for why this is scaffolding, not the real
/// mirror layer. Real mirror queries/accepts join `Doit` once G2 gives them
/// something real to ask the VM thread for.
pub enum VmRequest {
    Doit { code: String },
    /// Reset the worker's `BrowserSelection` to match the fresh default
    /// state the initial page render always shows (see
    /// `main.rs::open_class_browser`) — sent once whenever the browser
    /// view (re)opens, so the worker's retained selection can't drift out
    /// of sync with what's actually on screen.
    BrowserOpen,
    BrowserSelectPackage { name: String },
    BrowserSelectClass { name: String },
    BrowserSelectSide { side: Side },
    BrowserSelectCategory { name: String },
    BrowserSelectMethod { selector: String },
    /// Cmd+S in the source pane's editor (`../smtk.js`) — "accept" the
    /// edited text against whatever the worker's `selection.edit_target`
    /// currently points at (a method, the class comment, the class
    /// definition, or — for the two `New*` targets — parse an identity
    /// (selector, or class name+header) out of the text itself and
    /// create/reopen/redefine accordingly). Stand-in for `mirror
    /// addMethod:category:ifFail:` (`../../docs/APPS.md` §5.2) — real
    /// `.mst`-grammar parsing (`image_store::mst`) now, for the parts of
    /// that grammar that exist, but still no compiler.
    ///
    /// The six `saved_*` fields are a snapshot of the `BrowserSelection`
    /// the source pane was rendered against (`browser_render.rs`'s
    /// `data-save-*` attributes, round-tripped through `smtk.js`), not just
    /// the edited text — without them, a selection-change request queued
    /// between the render and this arriving would apply the edit against
    /// whatever's *now* selected instead of what was actually on screen
    /// when the user hit Cmd+S. `handle`'s arm rejects (no apply, just an
    /// explanatory transcript line) if they no longer match.
    BrowserSaveSource {
        text: String,
        saved_package: String,
        saved_class: String,
        saved_side: String,
        saved_category: String,
        saved_method: String,
        saved_target: String,
    },
    /// "New Method" — switches the source pane into an editable blank
    /// template for `selection.class`/`side`/`category` (all three must
    /// already be selected; a no-op otherwise). Cmd+S on the result is what
    /// actually creates it — see `BrowserSaveSource`.
    BrowserNewMethod,
    /// "New Class" — switches the source pane into an editable blank
    /// class-definition template, prefilling the superclass with whatever
    /// class (if any) is currently selected. Requires `selection.package`
    /// (there's no "file" to derive a package from here, unlike a real
    /// `.mst` file — see `image_store::mst`'s doc comment) — a no-op if
    /// none is selected.
    BrowserNewClass,
    /// Switches the source pane to the selected class's "definition" tab
    /// (superclass + instance variables, as real `.mst` header syntax) —
    /// alongside the existing default "comment" tab. A no-op if no class
    /// is selected.
    BrowserEditDefinition,
    /// Switches back to the "comment" tab — symmetric with
    /// `BrowserEditDefinition`, for the tab pair above the source pane.
    BrowserEditComment,
    /// Remove the selected class. Unconditional — `docs/IMAGE.md`'s
    /// soft-delete means this is undo-able the same way any other edit is
    /// (no dedicated "unremove"), and the *decision* to allow removing a
    /// class that still has subclasses (rather than block, or
    /// auto-reparent them) was made deliberately: the browser is expected
    /// to warn about affected subclasses in the confirmation UI
    /// (`browser_render.rs`) *before* sending this, not have the request
    /// itself carry that logic.
    BrowserRemoveClass,
    /// Remove the selected method. See `BrowserRemoveClass`'s doc comment.
    BrowserRemoveMethod,
    /// The Workspace tool's (`workspace_render.rs`, `docs/APPS.md` §5.5)
    /// "Print it" — evaluate `code` (whatever's selected, or the whole
    /// buffer if nothing is) and answer its printString to insert inline,
    /// rather than just echoing to the transcript the way plain `Doit`
    /// does ("Do it" in the workspace *is* plain `Doit` — no new request
    /// needed for that half). Real `eval` (SPEC §16.2) already answers a
    /// printString directly (`Result<String, GuestError>`), so this is
    /// shaped to become a near-direct caller of it, not a stand-in that'll
    /// need reshaping later.
    WorkspacePrintIt { code: String },
    /// "Smalltalk allocates a widget of a specific size" (`docs/CANVAS.md`
    /// §5.1) — stands in for a real `Canvas extent: w @ h` send (no VM-side
    /// primitive exists yet, same "prove the wire mechanics, don't
    /// simulate evaluation" posture as `WorkspacePrintIt`). Always
    /// (re)creates the single `id: 0` canvas — multiple simultaneous
    /// canvases are deferred (`docs/CANVAS.md` §7).
    CanvasCreate { width: u32, height: u32 },
    /// The Canvas view's (`canvas_render.rs`, `../../docs/CANVAS.md`)
    /// "Run Demo" button — same stand-in posture as `CanvasCreate`.
    CanvasRunDemo,
    /// Clears the canvas — a single `clearRect` batch, not a separate
    /// code path (`docs/CANVAS.md` §5.3: "full redraw" is a command-batch
    /// convention, not a different channel).
    CanvasClear,
}

/// VM → GUI. `Transcript` is what the G1 stub host already produces.
/// `BrowserPanes` carries all five rendered panes at once on every browser
/// selection — simple and cheap at this scale (see `handle`'s doc comment)
/// rather than tracking which panes actually changed. Real rendered
/// fragments (D-G5, once outliners are real) would likely want the finer-
/// grained per-node form `docs/APPS.md` §5.1 describes; this is deliberately
/// the simplest thing that lets the browser *view* and the *messaging
/// protocol* be built and tested now.
pub enum VmResponse {
    Transcript(String),
    BrowserPanes {
        packages_html: String,
        classes_html: String,
        categories_html: String,
        methods_html: String,
        source_html: String,
    },
    /// Distinct from `BrowserPanes` on purpose, not just a "was this the
    /// first one" flag on the same variant: the *handling* genuinely
    /// differs (`main.rs` builds and loads a brand-new page for this one,
    /// vs. patching an already-loaded page's existing panes for
    /// `BrowserPanes`) — see `browser_render::assemble_panes`'s doc comment
    /// for the race this distinction fixes.
    BrowserOpened {
        packages_html: String,
        classes_html: String,
        categories_html: String,
        methods_html: String,
        source_html: String,
    },
    /// Answers `WorkspacePrintIt` — `main.rs` inserts `text` into the
    /// workspace's textarea right after the selection that was evaluated,
    /// rather than routing it to the transcript (`smtk.js`'s
    /// `macvmInsertPrintResult`).
    WorkspacePrintResult { text: String },
    /// Answers `CanvasCreate` — `main.rs` (re)sets the `<canvas>` element's
    /// `width`/`height` attributes (which, per the HTML5 canvas spec,
    /// clears its content as a side effect — matches "allocate a widget of
    /// a specific size" reading as a fresh surface, not a resize-in-place).
    CanvasCreated { id: u32, width: u32, height: u32 },
    /// Answers `CanvasRunDemo` — `commands_json` is a JSON array of
    /// `[opName, ...args]` (`docs/CANVAS.md` §5.2), executed by
    /// `smtk.js`'s `macvmCanvasDraw` against a real `CanvasRenderingContext2D`.
    CanvasDraw { id: u32, commands_json: String },
    /// Answers `CanvasClear`.
    CanvasCleared { id: u32 },
}

/// `Id`/`Sel` are raw pointers, not `Send` by default. Safe to send into the
/// worker thread here specifically because the only use they're put to is
/// as arguments to `performSelector:withObject:waitUntilDone:`
/// (`objc.rs`), which is itself documented as safe to call from any
/// thread — unlike arbitrary AppKit/WebKit message sends, which are
/// main-thread-only.
struct CrossThreadObjcRef(Id, Sel);
unsafe impl Send for CrossThreadObjcRef {}

/// Handle held on the main thread: submits requests, owns the worker
/// thread's `JoinHandle` (dropped, i.e. detached — this is a long-lived
/// background thread for the process's lifetime, same as the app itself).
pub struct VmHost {
    requests: Sender<VmRequest>,
}

impl VmHost {
    pub fn submit(&self, request: VmRequest) {
        // The only failure mode is the worker thread having panicked and
        // dropped its receiver; nothing productive to do from here but
        // drop the request rather than panic the UI thread over it.
        let _ = self.requests.send(request);
    }
}

/// Response queue drained on the main thread. A `Mutex<Receiver<..>>`
/// rather than a lock-free structure: contention is a non-issue (one
/// producer thread, one consumer callback, low frequency), and `Receiver`
/// needs `&mut self` for `try_recv`.
static RESPONSES: Mutex<Option<Receiver<VmResponse>>> = Mutex::new(None);

/// Spawn the VM worker thread and wire it to wake `main_thread_target` (via
/// `drain_selector`) on the main thread whenever a response is ready.
/// `drain_selector`'s method should call [`drain_responses`] and apply each
/// one (see `main.rs::build_vm_bridge`/`vm_bridge_drain_responses`).
pub fn spawn(main_thread_target: Id, drain_selector: Sel) -> VmHost {
    let (request_tx, request_rx) = mpsc::channel::<VmRequest>();
    let (response_tx, response_rx) = mpsc::channel::<VmResponse>();
    *RESPONSES.lock().unwrap() = Some(response_rx);

    let wake = CrossThreadObjcRef(main_thread_target, drain_selector);
    std::thread::spawn(move || {
        let wake = wake; // moved in, lives for the thread's duration
        worker_loop(request_rx, response_tx, &wake);
    });

    VmHost { requests: request_tx }
}

fn worker_loop(requests: Receiver<VmRequest>, responses: Sender<VmResponse>, wake: &CrossThreadObjcRef) {
    // Owned entirely by this thread — no `Mutex`, no `static`. The UI
    // thread never touches `world`/`selection` directly, only ever asks
    // for a change via a `VmRequest`; see this module's doc comment on
    // "routing a click to the right closure, and locking" for why that's
    // the right shape, not just a convenient one. `world` is `mut` now
    // that `BrowserSaveSource` can edit it (still worker-thread-only).
    let image = open_image_from_env();
    let mut world = match &image {
        Some(img) => mock_world_from_image(img),
        None => MockWorld::seed(),
    };
    let mut selection = BrowserSelection::default();
    for request in requests {
        let batch = handle(request, &mut world, &mut selection, image.as_ref());
        let mut disconnected = false;
        for response in batch {
            if responses.send(response).is_err() {
                disconnected = true; // main thread's receiver is gone
                break;
            }
        }
        if disconnected {
            break;
        }
        objc::perform_selector_on_main_thread(wake.0, wake.1, objc::NIL, false);
    }
}

/// `docs/IMAGE.md` §8 — an `image_store::Image` file if `MACVM_IMAGE_PATH`
/// names one that actually opens, else `None` (falling back to the
/// invented `MockWorld` seed, which is the whole point of keeping the mock
/// around: zero-setup UI testing with no image file required).
fn open_image_from_env() -> Option<image_store::Image> {
    let path = std::env::var_os("MACVM_IMAGE_PATH")?;
    match image_store::Image::open(std::path::Path::new(&path)) {
        Ok(img) => Some(img),
        Err(e) => {
            eprintln!("vm_host: MACVM_IMAGE_PATH={path:?} failed to open ({e}) — falling back to the mock world");
            None
        }
    }
}

fn side_to_image(side: Side) -> image_store::Side {
    match side {
        Side::Instance => image_store::Side::Instance,
        Side::Class => image_store::Side::Class,
    }
}

fn side_from_image(side: image_store::Side) -> Side {
    match side {
        image_store::Side::Instance => Side::Instance,
        image_store::Side::Class => Side::Class,
    }
}

/// What a successful mirror-side save/remove reports about the *image*
/// side of the same operation — `image_error: None` means it actually
/// persisted (or there's no image configured at all, pure-mock mode);
/// `Some(reason)` means it didn't, so the transcript message can say so
/// honestly instead of unconditionally claiming "(persisted to image)"
/// the way this used to regardless of outcome.
struct SaveOk {
    what: String,
    image_error: Option<String>,
}

/// Describes what happened writing through to the image for the common
/// `Result<bool, _>` shape (`set_method_source`/`set_class_comment`/
/// `set_class_definition`/`remove_class`/`remove_method` all return this,
/// as `rusqlite::Result<bool>` — generic here rather than naming that type
/// directly since this crate doesn't depend on `rusqlite` itself, only on
/// `image_store`). `Ok(false)` — the mirror found the target but the image
/// didn't — is a mirror/image drift case, not a "successfully did
/// nothing": it's reported the same as a hard error, not silently treated
/// as success.
fn describe_image_write<E: std::fmt::Display>(result: Result<bool, E>) -> Option<String> {
    match result {
        Ok(true) => None,
        Ok(false) => Some("not found in the image (mirror/image out of sync?)".to_string()),
        Err(e) => Some(e.to_string()),
    }
}

/// One-shot mirror of every class/method `image`'s latest versions hold
/// into a fresh `MockWorld` — `browser_render.rs` renders from this exactly
/// as it would the invented seed; `handle`'s `BrowserSaveSource` arm keeps
/// both in sync afterward (mirror updated for immediate display, `image`
/// updated so the edit survives a restart).
fn mock_world_from_image(image: &image_store::Image) -> MockWorld {
    let mut world = MockWorld::empty();
    let classes = image.all_classes().unwrap_or_default();
    for c in &classes {
        world.add_class(&c.name, c.superclass.as_deref(), &c.category, &c.comment, &c.instance_vars, &c.class_vars);
    }
    for c in &classes {
        for m in image.all_methods_of(&c.name).unwrap_or_default() {
            world.add_method(&c.name, side_from_image(m.side), &m.selector, &m.category, &m.source);
        }
    }
    world
}

/// Stand-in for `VmHandle::eval` (SPEC §16.2) — the exact G1 stub behavior
/// `main.rs` used to run inline on the main thread, now running on the
/// worker thread instead. Swap the `Doit` arm's body for a real
/// `vm.eval(&code)` call once G2 lands; the threading around it doesn't
/// need to change. The `BrowserSelect*` arms update `selection` then
/// re-render every pane — simplest-thing-that-works at mock scale (see
/// `VmResponse::BrowserPanes`'s doc comment). Returns a `Vec` rather than a
/// single `VmResponse` because `BrowserSaveSource` needs to deliver two
/// things — updated panes *and* a transcript confirmation — not because
/// any request produces a high-frequency stream (that's `LatestSlot`'s job,
/// see this module's doc comment).
fn handle(request: VmRequest, world: &mut MockWorld, selection: &mut BrowserSelection, image: Option<&image_store::Image>) -> Vec<VmResponse> {
    match request {
        VmRequest::Doit { code } => vec![VmResponse::Transcript(format!("> {code}"))],
        VmRequest::BrowserOpen => {
            // No longer resets `selection` to default — that made sense
            // when the initial page render was a hardcoded mock (so the
            // worker's state needed to match what was already on screen),
            // but every render now comes from live worker state regardless
            // (`BrowserOpened` vs. `BrowserPanes` is only about *how* the
            // page gets shown, not what data it shows — see
            // `browser_render::assemble_panes`'s doc comment). Preserving
            // `selection` here means refresh, a theme switch, and a
            // font-scale change (all of which route through this same
            // request via `reload_current_page`) keep your package/class/
            // method position instead of silently discarding it. The
            // worker's own initial `BrowserSelection::default()`
            // (`worker_loop`) still covers first-ever open.
            let VmResponse::BrowserPanes { packages_html, classes_html, categories_html, methods_html, source_html } =
                render_browser_panes(world, selection)
            else {
                unreachable!("render_browser_panes always returns BrowserPanes")
            };
            vec![VmResponse::BrowserOpened { packages_html, classes_html, categories_html, methods_html, source_html }]
        }
        VmRequest::BrowserSelectPackage { name } => {
            selection.package = Some(name);
            selection.class = None;
            selection.category = None;
            selection.method = None;
            selection.edit_target = SourceEditTarget::ClassComment;
            vec![render_browser_panes(world, selection)]
        }
        VmRequest::BrowserSelectClass { name } => {
            selection.class = Some(name);
            selection.category = None;
            selection.method = None;
            selection.edit_target = SourceEditTarget::ClassComment;
            vec![render_browser_panes(world, selection)]
        }
        VmRequest::BrowserSelectSide { side } => {
            selection.side = side;
            selection.category = None;
            selection.method = None;
            selection.edit_target = SourceEditTarget::ClassComment;
            vec![render_browser_panes(world, selection)]
        }
        VmRequest::BrowserSelectCategory { name } => {
            // An empty name is the "(all)" pseudo-category
            // (`browser_render.rs`'s `render_categories_pane`) clearing
            // back to "show every method on this side," not a category
            // literally named "".
            selection.category = if name.is_empty() { None } else { Some(name) };
            selection.method = None;
            selection.edit_target = SourceEditTarget::ClassComment;
            vec![render_browser_panes(world, selection)]
        }
        VmRequest::BrowserSelectMethod { selector } => {
            selection.method = Some(selector);
            selection.edit_target = SourceEditTarget::Method;
            vec![render_browser_panes(world, selection)]
        }
        VmRequest::BrowserNewMethod => {
            // No longer requires a category to already be selected — a
            // fresh, still-methodless class has none to pick from at all
            // (`render_methods_pane`'s doc comment); `BrowserSaveSource`'s
            // `NewMethod` arm defaults the new method to "as yet
            // unclassified" when `selection.category` is `None`.
            if selection.class.is_some() {
                selection.method = None;
                selection.edit_target = SourceEditTarget::NewMethod;
            }
            vec![render_browser_panes(world, selection)]
        }
        VmRequest::BrowserNewClass => {
            // Same "default to the first package" fallback
            // `render_classes_pane` already applies for display — so the
            // "+ New Class" button, shown whenever any package exists, has
            // a real package to work with even before the user's ever
            // explicitly clicked one.
            if selection.package.is_none() {
                selection.package = world.packages().first().cloned();
            }
            selection.class = None;
            selection.category = None;
            selection.method = None;
            selection.edit_target = SourceEditTarget::NewClass;
            vec![render_browser_panes(world, selection)]
        }
        VmRequest::BrowserEditDefinition => {
            if selection.class.is_some() {
                selection.method = None;
                selection.edit_target = SourceEditTarget::ClassDefinition;
            }
            vec![render_browser_panes(world, selection)]
        }
        VmRequest::BrowserEditComment => {
            if selection.class.is_some() {
                selection.method = None;
                selection.edit_target = SourceEditTarget::ClassComment;
            }
            vec![render_browser_panes(world, selection)]
        }
        VmRequest::BrowserRemoveClass => {
            let message = match selection.class.clone() {
                Some(class_name) => {
                    let removed = world.remove_class(&class_name);
                    let image_error = image.and_then(|img| describe_image_write(img.remove_class(&class_name)));
                    if removed {
                        selection.class = None;
                        selection.category = None;
                        selection.method = None;
                        selection.edit_target = SourceEditTarget::ClassComment;
                        match image_error {
                            None => format!("Removed {class_name}."),
                            Some(reason) => format!("Removed {class_name}, but failed to persist the removal to the image: {reason}."),
                        }
                    } else {
                        "Nothing selected to remove.".to_string()
                    }
                }
                None => "Nothing selected to remove.".to_string(),
            };
            vec![render_browser_panes(world, selection), VmResponse::Transcript(message)]
        }
        VmRequest::BrowserRemoveMethod => {
            let message = match (selection.class.clone(), selection.method.clone()) {
                (Some(class_name), Some(selector)) => {
                    let removed = world.remove_method(&class_name, selection.side, &selector);
                    let image_error = image.and_then(|img| describe_image_write(img.remove_method(&class_name, side_to_image(selection.side), &selector)));
                    if removed {
                        selection.method = None;
                        selection.edit_target = SourceEditTarget::ClassComment;
                        match image_error {
                            None => format!("Removed {class_name}>>{selector}."),
                            Some(reason) => format!("Removed {class_name}>>{selector}, but failed to persist the removal to the image: {reason}."),
                        }
                    } else {
                        "Nothing selected to remove.".to_string()
                    }
                }
                _ => "Nothing selected to remove.".to_string(),
            };
            vec![render_browser_panes(world, selection), VmResponse::Transcript(message)]
        }
        VmRequest::BrowserSaveSource { text, saved_package, saved_class, saved_side, saved_category, saved_method, saved_target } => {
            // The source pane's `data-save-*` snapshot (`browser_render.rs`)
            // must still match what's actually selected right now — a
            // selection-change request queued between the render this came
            // from and this save arriving would otherwise apply the edited
            // text to whatever's selected *now* instead of what was on
            // screen when the user hit Cmd+S. Reject rather than guess:
            // the panes still get re-rendered (showing current reality),
            // but the stale edit is simply dropped, same as any other
            // unsaved-edit-discarded-by-navigation case.
            let snapshot = BrowserSelection::from_wire(&saved_package, &saved_class, &saved_side, &saved_category, &saved_method, &saved_target);
            if snapshot != *selection {
                return vec![
                    render_browser_panes(world, selection),
                    VmResponse::Transcript("Selection changed since this edit was opened — not saved.".to_string()),
                ];
            }

            // Write through to both: `world` (the in-memory mirror
            // `browser_render.rs` reads) for immediate display, and the
            // real `image` (if `MACVM_IMAGE_PATH` gave us one) so the edit
            // actually survives a restart — `docs/IMAGE.md` §8. The mirror
            // save is authoritative for *display* either way (it's what
            // gets re-rendered right below), so a mirror/image mismatch
            // can't leave the browser showing something that isn't there —
            // but it can mean the edit doesn't survive a restart, which is
            // now reported honestly (`SaveOk`/`describe_image_write`)
            // rather than unconditionally claiming "(persisted to image)".
            let outcome: Result<SaveOk, String> = match &selection.edit_target {
                SourceEditTarget::ClassComment => match &selection.class {
                    Some(class_name) if world.set_class_comment(class_name, text.clone()) => {
                        let image_error = image.and_then(|img| describe_image_write(img.set_class_comment(class_name, &text)));
                        Ok(SaveOk { what: format!("Saved {class_name} comment"), image_error })
                    }
                    _ => Err("Nothing selected to save.".to_string()),
                },
                SourceEditTarget::Method => match (&selection.class, &selection.method) {
                    (Some(class_name), Some(selector)) if world.set_method_source(class_name, selection.side, selector, text.clone()) => {
                        let image_error =
                            image.and_then(|img| describe_image_write(img.set_method_source(class_name, side_to_image(selection.side), selector, &text)));
                        Ok(SaveOk { what: format!("Saved {class_name}>>{selector}"), image_error })
                    }
                    _ => Err("Nothing selected to save.".to_string()),
                },
                SourceEditTarget::ClassDefinition => match &selection.class {
                    Some(class_name) => match image_store::mst::parse_mst_source(&text).into_iter().next() {
                        Some(pc) if pc.name == *class_name => {
                            world.set_class_definition(class_name, pc.superclass.clone(), pc.instance_vars.clone());
                            let image_error = image
                                .and_then(|img| describe_image_write(img.set_class_definition(class_name, pc.superclass.as_deref(), &pc.instance_vars)));
                            Ok(SaveOk { what: format!("Saved {class_name} definition"), image_error })
                        }
                        Some(pc) => Err(format!("Definition names \"{}\", not \"{class_name}\" — renaming a class isn't supported here.", pc.name)),
                        None => Err("Could not parse the class definition — check the syntax.".to_string()),
                    },
                    None => Err("Nothing selected to save.".to_string()),
                },
                SourceEditTarget::NewMethod => match selection.class.clone() {
                    Some(class_name) => {
                        // No category selected means "(all)" was showing
                        // (`render_categories_pane`) — default the new
                        // method to "as yet unclassified" (the schema's own
                        // default for an uncategorized method) rather than
                        // requiring one to be picked first. Deliberately
                        // *not* overwritten into `selection.category` on
                        // success: leaving the filter exactly as it was
                        // means an "(all)" view stays showing everything
                        // (including the new method), and an explicit
                        // category stays narrowed to what it already was.
                        let category = selection.category.clone().unwrap_or_else(|| "as yet unclassified".to_string());
                        match image_store::mst::parse_method_selector(&text) {
                            Some(selector) => {
                                let created = world.create_or_reopen_method(&class_name, selection.side, &selector, &category, &text);
                                let image_error = image.and_then(|img| {
                                    match img.create_or_reopen_method(&class_name, side_to_image(selection.side), &selector, &category, &text) {
                                        Ok(Some(_)) => None,
                                        Ok(None) => Some("class not found in the image (mirror/image out of sync?)".to_string()),
                                        Err(e) => Some(e.to_string()),
                                    }
                                });
                                if created {
                                    selection.method = Some(selector.clone());
                                    selection.edit_target = SourceEditTarget::Method;
                                    Ok(SaveOk { what: format!("Saved {class_name}>>{selector}"), image_error })
                                } else {
                                    Err("Nothing selected to save.".to_string())
                                }
                            }
                            None => Err("Could not parse a message pattern from the method source.".to_string()),
                        }
                    }
                    None => Err("Select a class first.".to_string()),
                },
                SourceEditTarget::NewClass => match selection.package.clone() {
                    Some(package) => match image_store::mst::parse_mst_source(&text).into_iter().next() {
                        Some(pc) if world.class_named(&pc.name).is_some() => Err(format!("A class named {} already exists.", pc.name)),
                        Some(pc) => {
                            let new_name = pc.name.clone();
                            world.create_or_reopen_class(&pc.name, pc.superclass.as_deref(), &package, "", &pc.instance_vars);
                            let mut image_error = image.and_then(|img| {
                                match img.create_or_reopen_class(&pc.name, pc.superclass.as_deref(), &package, "", &pc.instance_vars) {
                                    Ok(image_store::ClassCreateOutcome::AlreadyLive) => {
                                        Some(format!("image already has a live class named {new_name} (mirror/image out of sync?)"))
                                    }
                                    Ok(_) => None,
                                    Err(e) => Some(e.to_string()),
                                }
                            });
                            let mut failed_methods = 0usize;
                            for m in &pc.methods {
                                let side = if m.is_class_side { Side::Class } else { Side::Instance };
                                world.create_or_reopen_method(&new_name, side, &m.selector, "as yet unclassified", &m.source);
                                if let Some(img) = image {
                                    if img.create_or_reopen_method(&new_name, side_to_image(side), &m.selector, "as yet unclassified", &m.source).is_err() {
                                        failed_methods += 1;
                                    }
                                }
                            }
                            if failed_methods > 0 {
                                let note = format!("{failed_methods} method(s) failed to persist to the image");
                                image_error = Some(match image_error {
                                    Some(existing) => format!("{existing}; {note}"),
                                    None => note,
                                });
                            }
                            selection.package = Some(package);
                            selection.class = Some(new_name.clone());
                            selection.category = None;
                            selection.method = None;
                            selection.edit_target = SourceEditTarget::ClassComment;
                            Ok(SaveOk { what: format!("Created {new_name}"), image_error })
                        }
                        None => Err("Could not parse the class definition — check the syntax.".to_string()),
                    },
                    None => Err("Select a package first.".to_string()),
                },
            };
            let message = match outcome {
                Ok(SaveOk { what, image_error: None }) => match image {
                    Some(_) => format!("{what} (persisted to image)."),
                    None => format!("{what}."),
                },
                Ok(SaveOk { what, image_error: Some(reason) }) => format!("{what}, but failed to persist to the image: {reason}."),
                Err(e) => e,
            };
            vec![render_browser_panes(world, selection), VmResponse::Transcript(message)]
        }
        VmRequest::WorkspacePrintIt { code } => {
            // Stub — no real VM yet (`docs/APPS.md` §5.5). Deliberately
            // *not* a plausible-looking fake result: the point of this
            // round trip right now is proving the wire-and-insertion
            // mechanics work, not simulating evaluation, so `main.rs`'s
            // `open_workspace` starting comment says as much too.
            let text = format!(" \"(no VM yet: {code})\"");
            vec![VmResponse::WorkspacePrintResult { text }]
        }
        VmRequest::CanvasCreate { width, height } => {
            vec![VmResponse::CanvasCreated { id: CANVAS_ID, width, height }]
        }
        VmRequest::CanvasRunDemo => {
            vec![VmResponse::CanvasDraw { id: CANVAS_ID, commands_json: canvas_demo_commands().to_json() }]
        }
        VmRequest::CanvasClear => {
            let mut cmds = CanvasCommands::new();
            cmds.call("clearRect", &[0.0.into(), 0.0.into(), 10_000.0.into(), 10_000.0.into()]);
            vec![VmResponse::CanvasCleared { id: CANVAS_ID }, VmResponse::CanvasDraw { id: CANVAS_ID, commands_json: cmds.to_json() }]
        }
    }
}

/// v1's only canvas (`docs/CANVAS.md` §7: multiple canvases deferred).
const CANVAS_ID: u32 = 0;

/// A small ergonomic builder for a canvas command batch
/// (`VmResponse::CanvasDraw`'s `commands_json`, `docs/CANVAS.md` §5.2) —
/// stands in for how the real Smalltalk-side `Canvas` would accumulate
/// calls before flushing (§3), just in Rust for the demo/test path here.
/// Hand-writes JSON directly rather than pulling in a serde dependency
/// for a handful of numbers and strings.
#[derive(Default)]
pub struct CanvasCommands {
    commands: Vec<String>,
}

/// One command argument — canvas ops take a mix of numbers (coordinates,
/// widths) and strings (colors, fonts, text), so a single `call` needs a
/// heterogeneous arg list rather than separate numeric/string methods.
pub enum CanvasArg {
    Num(f64),
    Str(String),
}

impl From<f64> for CanvasArg {
    fn from(v: f64) -> Self {
        CanvasArg::Num(v)
    }
}

impl From<&str> for CanvasArg {
    fn from(v: &str) -> Self {
        CanvasArg::Str(v.to_string())
    }
}

impl CanvasCommands {
    pub fn new() -> Self {
        Self::default()
    }

    /// `name` is either a `CanvasRenderingContext2D` method (called as
    /// `ctx[name](...args)`) or a property (assigned as `ctx[name] =
    /// args[0]`) — `smtk.js`'s `macvmCanvasDraw` decides which via its own
    /// two allowlists; this builder doesn't need to know which is which.
    pub fn call(&mut self, name: &str, args: &[CanvasArg]) -> &mut Self {
        let mut parts = vec![json_string(name)];
        for a in args {
            parts.push(match a {
                CanvasArg::Num(n) => n.to_string(),
                CanvasArg::Str(s) => json_string(s),
            });
        }
        self.commands.push(format!("[{}]", parts.join(",")));
        self
    }

    pub fn to_json(&self) -> String {
        format!("[{}]", self.commands.join(","))
    }
}

/// Minimal JSON string escaping — only what this fixed command/color/text
/// vocabulary ever actually needs (quotes, backslashes), not a general-
/// purpose JSON encoder.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// "Run Demo"'s fixed content — a filled rectangle, a stroked circle, a
/// line, and a text label, deliberately exercising one op from each
/// vocabulary group (`docs/CANVAS.md` §5.2: paint, path, state/transform,
/// text, properties) so a visual check of this one demo is a real check
/// of the whole interpreter, not just one code path through it.
fn canvas_demo_commands() -> CanvasCommands {
    let mut c = CanvasCommands::new();
    c.call("clearRect", &[0.0.into(), 0.0.into(), 10_000.0.into(), 10_000.0.into()]);
    c.call("fillStyle", &["steelblue".into()]);
    c.call("fillRect", &[20.0.into(), 20.0.into(), 120.0.into(), 80.0.into()]);
    c.call("strokeStyle", &["crimson".into()]);
    c.call("lineWidth", &[3.0.into()]);
    c.call("beginPath", &[]);
    c.call("arc", &[220.0.into(), 60.0.into(), 40.0.into(), 0.0.into(), (std::f64::consts::PI * 2.0).into()]);
    c.call("stroke", &[]);
    c.call("save", &[]);
    c.call("strokeStyle", &["seagreen".into()]);
    c.call("lineWidth", &[2.0.into()]);
    c.call("beginPath", &[]);
    c.call("moveTo", &[20.0.into(), 140.0.into()]);
    c.call("lineTo", &[300.0.into(), 140.0.into()]);
    c.call("stroke", &[]);
    c.call("restore", &[]);
    c.call("fillStyle", &["#222".into()]);
    c.call("font", &["16px sans-serif".into()]);
    c.call("fillText", &["MACVM Canvas".into(), 20.0.into(), 175.0.into()]);
    c
}

fn render_browser_panes(world: &MockWorld, selection: &BrowserSelection) -> VmResponse {
    VmResponse::BrowserPanes {
        packages_html: browser_render::render_packages_pane(world, selection),
        classes_html: browser_render::render_classes_pane(world, selection),
        categories_html: browser_render::render_categories_pane(world, selection),
        methods_html: browser_render::render_methods_pane(world, selection),
        source_html: browser_render::render_source_pane(world, selection),
    }
}

/// Drain whatever responses have arrived since the last call. Called from
/// the main-thread selector `perform_selector_on_main_thread` triggers.
pub fn drain_responses() -> Vec<VmResponse> {
    let mut guard = RESPONSES.lock().unwrap();
    let Some(rx) = guard.as_mut() else { return Vec::new() };
    let mut out = Vec::new();
    while let Ok(response) = rx.try_recv() {
        out.push(response);
    }
    out
}

/// A single-value mailbox where publishing overwrites whatever's there,
/// read or not — the "latest wins" counterpart to the ordered `mpsc`
/// channel above, for continuous/high-frequency updates (a redrawing pixel
/// buffer, a live status line) where a slow consumer should skip straight
/// to the newest value instead of working through a backlog of stale ones.
/// See this module's doc comment for which response kind wants which
/// discipline.
pub struct LatestSlot<T> {
    value: Mutex<Option<T>>,
}

impl<T> LatestSlot<T> {
    pub fn new() -> Self {
        Self { value: Mutex::new(None) }
    }

    /// Replace whatever value was there. The old one (if any, if unread)
    /// is simply dropped — that's the point.
    pub fn publish(&self, value: T) {
        *self.value.lock().unwrap() = Some(value);
    }

    /// Take the current value, leaving the slot empty. Returns `None` if
    /// nothing's been published since the last `take`.
    pub fn take(&self) -> Option<T> {
        self.value.lock().unwrap().take()
    }
}

impl<T> Default for LatestSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_select_category_treats_empty_name_as_clearing_to_all() {
        let mut world = MockWorld::seed();
        let mut selection = BrowserSelection { class: Some("Object".to_string()), ..Default::default() };
        handle(VmRequest::BrowserSelectCategory { name: "printing".to_string() }, &mut world, &mut selection, None);
        assert_eq!(selection.category.as_deref(), Some("printing"));

        handle(VmRequest::BrowserSelectCategory { name: String::new() }, &mut world, &mut selection, None);
        assert_eq!(selection.category, None);
    }

    #[test]
    fn browser_new_method_only_requires_a_class_not_a_category() {
        let mut world = MockWorld::seed();
        let mut selection = BrowserSelection { class: Some("Object".to_string()), ..Default::default() };
        assert_eq!(selection.category, None); // "(all)" — no category picked
        handle(VmRequest::BrowserNewMethod, &mut world, &mut selection, None);
        assert_eq!(selection.edit_target, SourceEditTarget::NewMethod);
    }

    #[test]
    fn browser_save_source_rejects_a_stale_snapshot() {
        let mut world = MockWorld::seed();
        // Selection has already moved on to "hash" by the time this save
        // arrives, but the snapshot (as if captured when "printString" was
        // still open) still names "printString" — exactly the race a
        // selection-change request queued ahead of a Cmd+S creates.
        let mut selection = BrowserSelection {
            class: Some("Object".to_string()),
            category: Some("comparing".to_string()),
            method: Some("hash".to_string()),
            edit_target: SourceEditTarget::Method,
            ..Default::default()
        };
        let responses = handle(
            VmRequest::BrowserSaveSource {
                text: "printString\n\t^'HIJACKED'".to_string(),
                saved_package: String::new(),
                saved_class: "Object".to_string(),
                saved_side: "instance".to_string(),
                saved_category: "printing".to_string(),
                saved_method: "printString".to_string(),
                saved_target: "method".to_string(),
            },
            &mut world,
            &mut selection,
            None,
        );
        // Neither method was touched — not hash (what's *currently*
        // selected) and not printString (what the save actually said it
        // was for).
        assert_eq!(world.method_source("Object", Side::Instance, "hash").unwrap(), "hash\n\t^self identityHash");
        assert!(world.method_source("Object", Side::Instance, "printString").unwrap().starts_with("printString\n\t^String"));
        let message = responses
            .iter()
            .find_map(|r| match r {
                VmResponse::Transcript(t) => Some(t.clone()),
                _ => None,
            })
            .expect("expected a transcript message");
        assert!(message.contains("Selection changed"), "{message}");
    }

    #[test]
    fn browser_save_source_applies_when_snapshot_matches_current_selection() {
        let mut world = MockWorld::seed();
        let mut selection = BrowserSelection {
            class: Some("Object".to_string()),
            category: Some("comparing".to_string()),
            method: Some("hash".to_string()),
            edit_target: SourceEditTarget::Method,
            ..Default::default()
        };
        handle(
            VmRequest::BrowserSaveSource {
                text: "hash\n\t^42".to_string(),
                saved_package: String::new(),
                saved_class: "Object".to_string(),
                saved_side: "instance".to_string(),
                saved_category: "comparing".to_string(),
                saved_method: "hash".to_string(),
                saved_target: "method".to_string(),
            },
            &mut world,
            &mut selection,
            None,
        );
        assert_eq!(world.method_source("Object", Side::Instance, "hash").unwrap(), "hash\n\t^42");
    }

    #[test]
    fn browser_save_source_reports_image_write_failure_honestly() {
        let mut world = MockWorld::empty();
        world.add_class("Ghost", None, "Test", "old comment", "", "");
        // Deliberately never added to `image` — simulates mirror/image
        // drift (handle()'s own codepaths always keep both in sync, but
        // this exercises the defensive honesty check regardless of how
        // drift could happen).
        let image = image_store::Image::open_in_memory().unwrap();
        let mut selection =
            BrowserSelection { class: Some("Ghost".to_string()), edit_target: SourceEditTarget::ClassComment, ..Default::default() };
        let responses = handle(
            VmRequest::BrowserSaveSource {
                text: "new comment".to_string(),
                saved_package: String::new(),
                saved_class: "Ghost".to_string(),
                saved_side: "instance".to_string(),
                saved_category: String::new(),
                saved_method: String::new(),
                saved_target: "comment".to_string(),
            },
            &mut world,
            &mut selection,
            Some(&image),
        );
        // The mirror save still succeeds — immediate display is unaffected...
        assert_eq!(world.class_named("Ghost").unwrap().comment, "new comment");
        // ...but the transcript must not falsely claim it reached the image.
        let message = responses
            .iter()
            .find_map(|r| match r {
                VmResponse::Transcript(t) => Some(t.clone()),
                _ => None,
            })
            .expect("expected a transcript message");
        assert!(!message.contains("(persisted to image)"), "{message}");
        assert!(message.contains("failed to persist"), "{message}");
    }

    #[test]
    fn canvas_commands_builds_a_json_array_of_op_arrays() {
        let mut cmds = CanvasCommands::new();
        cmds.call("beginPath", &[]);
        cmds.call("lineTo", &[20.0.into(), 140.0.into()]);
        cmds.call("fillStyle", &["steelblue".into()]);
        assert_eq!(cmds.to_json(), r#"[["beginPath"],["lineTo",20,140],["fillStyle","steelblue"]]"#);
    }

    #[test]
    fn json_string_escapes_quotes_and_backslashes() {
        assert_eq!(json_string("plain"), "\"plain\"");
        assert_eq!(json_string(r#"say "hi" \ ok"#), r#""say \"hi\" \\ ok""#);
    }

    #[test]
    fn canvas_create_echoes_back_the_requested_size_under_the_fixed_v1_id() {
        let mut world = MockWorld::seed();
        let mut selection = BrowserSelection::default();
        let responses = handle(VmRequest::CanvasCreate { width: 640, height: 480 }, &mut world, &mut selection, None);
        assert!(matches!(responses.as_slice(), [VmResponse::CanvasCreated { id: 0, width: 640, height: 480 }]));
    }

    #[test]
    fn canvas_run_demo_returns_one_nonempty_draw_batch_for_the_fixed_canvas() {
        let mut world = MockWorld::seed();
        let mut selection = BrowserSelection::default();
        let responses = handle(VmRequest::CanvasRunDemo, &mut world, &mut selection, None);
        let [VmResponse::CanvasDraw { id, commands_json }] = responses.as_slice() else {
            panic!("expected exactly one CanvasDraw response");
        };
        assert_eq!(*id, 0);
        assert!(commands_json.starts_with('[') && commands_json.ends_with(']'), "{commands_json}");
        assert!(commands_json.contains("fillRect"), "{commands_json}");
        assert!(commands_json.contains("MACVM Canvas"), "{commands_json}");
    }

    #[test]
    fn canvas_clear_answers_cleared_then_a_full_canvas_clear_rect_batch() {
        let mut world = MockWorld::seed();
        let mut selection = BrowserSelection::default();
        let responses = handle(VmRequest::CanvasClear, &mut world, &mut selection, None);
        let [VmResponse::CanvasCleared { id: cleared_id }, VmResponse::CanvasDraw { id: draw_id, commands_json }] = responses.as_slice()
        else {
            panic!("expected [CanvasCleared, CanvasDraw]");
        };
        assert_eq!(*cleared_id, 0);
        assert_eq!(*draw_id, 0);
        assert!(commands_json.contains("clearRect"), "{commands_json}");
    }

    #[test]
    fn latest_slot_overwrites_unread_values() {
        let slot = LatestSlot::new();
        slot.publish(1);
        slot.publish(2);
        slot.publish(3);
        // Only the most recent publish survives — 1 and 2 were never read.
        assert_eq!(slot.take(), Some(3));
        assert_eq!(slot.take(), None);
    }

    #[test]
    fn latest_slot_read_then_publish_then_read() {
        let slot = LatestSlot::new();
        slot.publish("frame-1");
        assert_eq!(slot.take(), Some("frame-1"));
        assert_eq!(slot.take(), None);
        slot.publish("frame-2");
        assert_eq!(slot.take(), Some("frame-2"));
    }
}
