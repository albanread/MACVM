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
use macvm::embed::{TranscriptSink, VmHandle};
use macvm_mock_vm::MockWorld;
pub use macvm_mock_vm::Side;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The two top-level doIts the `.mst` world runs at load (`Transcript` bind +
/// `Character initTable`). S22-B moves these into the image; until then they
/// are replayed explicitly by [`crate::world_boot::load_world_from_image`].
const WORLD_DOITS: &[&str] = &[
    "Transcript := TranscriptStream new.",
    "Character initTable.",
];

/// GUI → VM. `Doit` is the only one with a real (stub) handler; the
/// `BrowserSelect*` variants drive the class browser view
/// (`browser_render.rs`) against `macvm-mock-vm`'s invented class world —
/// see that crate's doc comment for why this is scaffolding, not the real
/// mirror layer. Real mirror queries/accepts join `Doit` once G2 gives them
/// something real to ask the VM thread for.
pub enum VmRequest {
    Doit {
        code: String,
    },
    /// A `<smappl visual="CODE">` on the current page (`../smappl.md`): render
    /// the `visual=` expression to an HTML fragment (`VmHandle::render_fragment`,
    /// D-G5). `id` is the placeholder span's `data-widget-id`; the worker
    /// answers [`VmResponse::SmapplFragment`] with the same `id` so `main.rs`
    /// can swap the right span. A shape that can't be built yet (needs the
    /// Phase-W mirror stack) renders nothing — the placeholder box stays.
    SmapplRender {
        id: String,
        code: String,
    },
    /// A rendered smappl widget was clicked (`smtk.js`): run its action
    /// closure. `action_id` is the opaque id the widget carries in
    /// `data-widget-action`, stored image-side in `SmapplRegistry`; the
    /// worker runs `SmapplRegistry fire: action_id`, whose transcript/effects
    /// flow back over the normal channels (`../PLAN.md` G1).
    SmapplAction {
        action_id: String,
    },
    /// Cmd+S in a ClassOutliner source editor (`smtk.js`): accept an edited
    /// method. The worker **versions** it back into image_store
    /// (`set_method_source` → a new `method_versions` row, never an overwrite)
    /// AND live-compiles it into the running VM — the same proven path the
    /// class browser's `BrowserSaveSource` uses.
    SmapplAccept {
        cls: String,
        side: String,
        sel: String,
        text: String,
        /// The `data-widget-id` of the outliner being edited — left out of the
        /// post-accept live refresh so its editor isn't yanked out mid-edit.
        widget_id: String,
    },
    /// Drill down: a class name in a hierarchy outliner was clicked. Replace
    /// that widget (`widget_id`) with the class's method browser (a
    /// `ClassOutliner`, with editable source), carrying a back link to `root`'s
    /// hierarchy.
    SmapplOpenClass {
        cls: String,
        root: String,
        widget_id: String,
    },
    /// The back link in a drilled-down ClassOutliner: re-open `root`'s
    /// hierarchy in that widget.
    SmapplOpenHierarchy {
        root: String,
        widget_id: String,
    },
    /// A find-tool query (Implementors/Senders, docs/APPS.md §5.4): render the
    /// result list image-side and return it for the find page's results div.
    Find {
        tool: String,
        query: String,
    },
    /// Reset the worker's `BrowserSelection` to match the fresh default
    /// state the initial page render always shows (see
    /// `main.rs::open_class_browser`) — sent once whenever the browser
    /// view (re)opens, so the worker's retained selection can't drift out
    /// of sync with what's actually on screen.
    BrowserOpen,
    BrowserSelectPackage {
        name: String,
    },
    BrowserSelectClass {
        name: String,
    },
    BrowserSelectSide {
        side: Side,
    },
    BrowserSelectCategory {
        name: String,
    },
    BrowserSelectMethod {
        selector: String,
    },
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
    WorkspacePrintIt {
        code: String,
    },
    /// "Smalltalk allocates a widget of a specific size" (`docs/CANVAS.md`
    /// §5.1) — stands in for a real `Canvas extent: w @ h` send (no VM-side
    /// primitive exists yet, same "prove the wire mechanics, don't
    /// simulate evaluation" posture as `WorkspacePrintIt`). Always
    /// (re)creates the single `id: 0` canvas — multiple simultaneous
    /// canvases are deferred (`docs/CANVAS.md` §7).
    CanvasCreate {
        width: u32,
        height: u32,
    },
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
#[derive(Debug)]
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
    WorkspacePrintResult {
        text: String,
    },
    /// Answers `CanvasCreate` — `main.rs` (re)sets the `<canvas>` element's
    /// `width`/`height` attributes (which, per the HTML5 canvas spec,
    /// clears its content as a side effect — matches "allocate a widget of
    /// a specific size" reading as a fresh surface, not a resize-in-place).
    CanvasCreated {
        id: u32,
        width: u32,
        height: u32,
    },
    /// Answers `CanvasRunDemo` — `commands_json` is a JSON array of
    /// `[opName, ...args]` (`docs/CANVAS.md` §5.2), executed by
    /// `smtk.js`'s `macvmCanvasDraw` against a real `CanvasRenderingContext2D`.
    CanvasDraw {
        id: u32,
        commands_json: String,
    },
    /// Answers `CanvasClear`.
    CanvasCleared {
        id: u32,
    },
    /// Answers [`VmRequest::SmapplRender`] — `main.rs` calls `smtk.js`'s
    /// `macvmRenderSmappl(id, html)` to swap the placeholder span with `id`
    /// for the rendered widget `html` (D-G5).
    SmapplFragment {
        id: String,
        html: String,
    },
    /// Answers [`VmRequest::Find`] — `main.rs` fills the find page's results
    /// container with `html`.
    FindResults {
        html: String,
    },
    /// Internal supervisor liveness marker (S21 step 3) — NOT a UI response.
    /// The worker sends one after fully completing each request, i.e. once
    /// it is back to idle and provably still alive. `VmHost::drain_responses`
    /// consumes it (clearing the in-flight timer) and never forwards it, so
    /// the main thread never actually receives it — the arm in
    /// `main.rs::vm_bridge_drain` exists only to satisfy match exhaustiveness.
    /// The whole point: a worker that dies mid-request via
    /// `FatalMode::ExitThread` (`libc::pthread_exit`, `embed.rs`) never
    /// reaches this send, and because `pthread_exit` also leaks the request-
    /// channel receiver (no `Drop`, so `Sender::send` keeps succeeding and
    /// can't report the death), this absent marker is the ONLY signal the
    /// worker is gone — see [`WORKER_RESPONSE_TIMEOUT`].
    WorkerIdle,
}

/// `Id`/`Sel` are raw pointers, not `Send` by default. Safe to send into the
/// worker thread here specifically because the only use they're put to is
/// as arguments to `performSelector:withObject:waitUntilDone:`
/// (`objc.rs`), which is itself documented as safe to call from any
/// thread — unlike arbitrary AppKit/WebKit message sends, which are
/// main-thread-only. `Clone`/`Copy` (both fields are plain pointers) so
/// `ChannelTranscript` (S21) can hold its own copy alongside the worker
/// loop's.
#[derive(Clone, Copy)]
struct CrossThreadObjcRef(Id, Sel);
unsafe impl Send for CrossThreadObjcRef {}

impl CrossThreadObjcRef {
    /// Wake the main thread to drain responses. A NIL target means "no main
    /// thread to wake" (tests drive `drain_responses` directly) — short-
    /// circuited here so a headless test needs no Objective-C runtime at
    /// all, rather than relying on `objc_msgSend`-to-nil being a no-op.
    fn notify(self) {
        if self.0.is_null() {
            return;
        }
        objc::perform_selector_on_main_thread(self.0, self.1, objc::NIL, false);
    }
}

/// How long `VmHost::submit` waits for ANY response before assuming the
/// worker thread is gone and respawning (S21 step 3 — "if the language
/// thread dies, the GUI must not die"). Generous on purpose: a real
/// Smalltalk doit can legitimately run for a while, and a false-positive
/// respawn would abandon a still-live computation. This is the ONLY
/// detection mechanism, not a fallback for a rarer case: a worker that
/// terminates via `FatalMode::ExitThread` (`libc::pthread_exit`) runs no
/// `Drop` glue at all, so the response channel never reports disconnected
/// and `JoinHandle::join()`/`.is_finished()` panic/hang (see `embed.rs`'s
/// module doc) — a bounded "nothing came back" timeout is the only signal
/// available for that death mode. An ordinary Rust panic (not one of
/// `embed.rs`'s `fatal_exit` sites) disconnects the channel immediately
/// instead, which `submit` also handles, faster than this timeout.
const WORKER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// The mutable state a respawn must replace atomically — kept together so
/// "swap in a fresh worker" can never leave the request/response halves
/// pointing at two different worker generations.
struct HostInner {
    requests: Sender<VmRequest>,
    responses: Receiver<VmResponse>,
    /// Held ONLY so it can be `drop()`-ped when superseded — NEVER
    /// `.join()`-ed or `.is_finished()`-polled (see `WORKER_RESPONSE_
    /// TIMEOUT`'s own doc for why).
    #[allow(dead_code)]
    worker: std::thread::JoinHandle<()>,
    wake: CrossThreadObjcRef,
    world_dir: std::path::PathBuf,
    timeout: Duration,
    /// `Some(t)` from the moment a request is sent until the worker signals
    /// it finished that request (a `VmResponse::WorkerIdle` marker, cleared
    /// in `drain_responses`). Crucially NOT cleared by ordinary responses:
    /// a fatal doit emits its error transcript (via `ChannelTranscript`)
    /// and only THEN `pthread_exit`s, so clearing on any output would hide
    /// exactly the death this is meant to catch. If `submit` finds this
    /// still `Some` and older than `timeout`, the worker is presumed dead.
    pending_since: Option<Instant>,
}

/// Handle held on the main thread: submits requests, drains responses, and
/// transparently respawns the worker thread if it ever stops responding.
pub struct VmHost {
    inner: Mutex<HostInner>,
}

impl VmHost {
    pub fn submit(&self, request: VmRequest) {
        let mut inner = self.inner.lock().unwrap();
        if inner
            .pending_since
            .is_some_and(|t| t.elapsed() > inner.timeout)
        {
            respawn(&mut inner, RESPAWN_NOTICE_TIMEOUT);
        }
        let request = match inner.requests.send(request) {
            Ok(()) => {
                inner.pending_since = Some(Instant::now());
                return;
            }
            Err(mpsc::SendError(request)) => request,
        };
        // The worker's receiver is already gone even though we hadn't yet
        // timed out waiting for it — an ordinary Rust panic disconnects
        // the channel immediately (unlike a `pthread_exit` death, which
        // disconnects nothing at all). Respawn now and deliver this
        // request to the fresh worker instead of dropping it.
        respawn(&mut inner, RESPAWN_NOTICE_TIMEOUT);
        let _ = inner.requests.send(request);
        inner.pending_since = Some(Instant::now());
    }

    /// Kill the current language-thread worker and start a fresh one (a new
    /// VM booted from the image, new channels, new OS thread) — the "Restart
    /// VM Thread" menu item (`main.rs`). Safe to invoke at any time: if the
    /// worker is wedged in a runaway loop, it can't be force-killed (a Rust
    /// thread has no safe async kill), but it is abandoned and disconnected,
    /// and the fresh worker takes over immediately. Wakes the main thread so
    /// the "restarted" transcript notice shows right away rather than waiting
    /// for the next response.
    pub fn restart(&self) {
        let wake = {
            let mut inner = self.inner.lock().unwrap();
            respawn(&mut inner, "--- VM thread restarted ---");
            inner.wake
        };
        wake.notify();
    }

    /// Drain whatever responses have arrived since the last call. Called
    /// from the main-thread selector `perform_selector_on_main_thread`
    /// triggers. `WorkerIdle` markers are consumed here (clearing the
    /// in-flight timer) and never returned — see that variant's doc.
    pub fn drain_responses(&self) -> Vec<VmResponse> {
        let mut inner = self.inner.lock().unwrap();
        let mut out = Vec::new();
        while let Ok(response) = inner.responses.try_recv() {
            match response {
                VmResponse::WorkerIdle => inner.pending_since = None,
                other => out.push(other),
            }
        }
        out
    }
}

/// One worker generation's channel endpoints + thread handle, as produced
/// by `spawn_worker` — grouped so a respawn always replaces all three
/// together (never a torn mix of an old receiver with a new sender, etc).
struct WorkerHandles {
    requests: Sender<VmRequest>,
    responses: Receiver<VmResponse>,
    /// A clone of the worker's own response sender, for injecting a
    /// supervisor-level notice (e.g. "restarted") that doesn't come from
    /// the worker itself. `mpsc::Sender` is multi-producer by design —
    /// holding a second clone alongside the worker's own doesn't change
    /// anything about the channel's behavior or lifetime.
    notices: Sender<VmResponse>,
    worker: std::thread::JoinHandle<()>,
}

fn spawn_worker(wake: CrossThreadObjcRef, world_dir: std::path::PathBuf) -> WorkerHandles {
    let (request_tx, request_rx) = mpsc::channel::<VmRequest>();
    let (response_tx, response_rx) = mpsc::channel::<VmResponse>();
    let notices = response_tx.clone();
    let worker = std::thread::spawn(move || {
        worker_loop(request_rx, response_tx, wake, &world_dir);
    });
    WorkerHandles {
        requests: request_tx,
        responses: response_rx,
        notices,
        worker,
    }
}

/// Replaces `inner`'s worker with a brand new one (fresh channels, fresh
/// `VmHandle`, on a fresh OS thread) and drops the stale `JoinHandle` (safe
/// — never joined). `notice` is queued on the FRESH channel so the user sees
/// in the transcript why their workspace just "came back." The old worker,
/// if idle, exits when its request channel is dropped here; if it's
/// mid-computation (a runaway loop) it is simply abandoned — disconnected,
/// its output ignored — and the fresh worker serves everything from now on.
fn respawn(inner: &mut HostInner, notice: &str) {
    let w = spawn_worker(inner.wake, inner.world_dir.clone());
    let _ = w.notices.send(VmResponse::Transcript(notice.to_string()));
    inner.requests = w.requests;
    inner.responses = w.responses;
    inner.worker = w.worker;
    inner.pending_since = None;
}

const RESPAWN_NOTICE_TIMEOUT: &str =
    "--- the language thread stopped responding; a fresh one has been started ---";

/// Spawn the VM worker thread and wire it to wake `main_thread_target` (via
/// `drain_selector`) on the main thread whenever a response is ready.
/// `drain_selector`'s method should call [`VmHost::drain_responses`] and
/// apply each one (see `main.rs::build_vm_bridge`/`vm_bridge_drain`). The
/// world directory defaults to `world` (relative to the process's own
/// launch directory, same convention as the CLI's `--world`, `main.rs::
/// load_world_with_warning`), overridable via `MACVM_WORLD_PATH` — reading
/// (not mutating) an env var here is race-free even under a parallel test
/// runner (see `spawn_with_world_and_timeout`'s own doc for why tests use
/// an explicit path instead of this env var regardless).
pub fn spawn(main_thread_target: Id, drain_selector: Sel) -> VmHost {
    let world_dir = std::env::var_os("MACVM_WORLD_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("world"));
    spawn_with_world_and_timeout(
        main_thread_target,
        drain_selector,
        world_dir,
        WORKER_RESPONSE_TIMEOUT,
    )
}

/// `spawn`'s fully-parameterized form — a testability seam, not a second
/// production entry point. Real callers always go through `spawn` (fixed
/// world-directory convention, the real 30s timeout); tests need an
/// explicit `world_dir` (`cargo test`'s working directory is the crate
/// root, `gui/`, not the workspace root `world/` lives in — env var
/// mutation to fix that up would race under a parallel test runner, per
/// `tests/common/mod.rs`'s own established rule in the main crate) and a
/// much shorter `timeout` (so a respawn test doesn't need to sleep 30 real
/// seconds).
fn spawn_with_world_and_timeout(
    main_thread_target: Id,
    drain_selector: Sel,
    world_dir: std::path::PathBuf,
    timeout: Duration,
) -> VmHost {
    let wake = CrossThreadObjcRef(main_thread_target, drain_selector);
    let w = spawn_worker(wake, world_dir.clone());
    VmHost {
        inner: Mutex::new(HostInner {
            requests: w.requests,
            responses: w.responses,
            worker: w.worker,
            wake,
            world_dir,
            timeout,
            pending_since: None,
        }),
    }
}

fn worker_loop(
    requests: Receiver<VmRequest>,
    responses: Sender<VmResponse>,
    wake: CrossThreadObjcRef,
    world_dir: &Path,
) {
    // Owned entirely by this thread — no `Mutex`, no `static`. The UI
    // thread never touches `world`/`selection` directly, only ever asks
    // for a change via a `VmRequest`; see this module's doc comment on
    // "routing a click to the right closure, and locking" for why that's
    // the right shape, not just a convenient one. `world` is `mut` now
    // that `BrowserSaveSource` can edit it (still worker-thread-only).
    //
    // S22: the versioned image database is the single source of truth. The
    // VM boots FROM the image (genesis + `load_world_from_image`, not the
    // `.mst` file tree), and the browser's mock mirrors the SAME image, so
    // an edit written through to both stays consistent. The image is
    // auto-seeded from `world/*.mst` on first run (zero setup). If the DB
    // can't be established at all, fall back to the old `.mst` boot + mock
    // seed so the GUI still works.
    let image_path = resolve_image_path(world_dir);
    let (image, mut vm) = match open_or_seed_image(world_dir, &image_path) {
        Ok(img) => match boot_vm_from_image(&img, responses.clone(), wake) {
            Some(vm) => (Some(img), vm),
            // The image opened but the world load failed — most likely a
            // STALE image written by an older importer. Fall back to the
            // .mst boot + mock so the GUI still works, rather than
            // respawn-looping forever on a bad image. (A boot failure was
            // already reported to the transcript by `boot_vm_from_image`.)
            None => match boot_real_vm(responses.clone(), wake, world_dir) {
                Some(vm) => (None, vm),
                None => return,
            },
        },
        Err(e) => {
            eprintln!("vm_host: {e} — falling back to .mst boot + mock world");
            match boot_real_vm(responses.clone(), wake, world_dir) {
                Some(vm) => (None, vm),
                None => return,
            }
        }
    };
    let mut world = match &image {
        Some(img) => mock_world_from_image(img),
        None => MockWorld::seed(),
    };
    let mut selection = BrowserSelection::default();

    for request in requests {
        let batch = handle(request, &mut world, &mut selection, image.as_ref(), &mut vm);
        // Reaching HERE (rather than `pthread_exit`ing inside `handle` on a
        // fatal guest condition) is exactly the "still alive" fact the
        // WorkerIdle marker below reports — so it is sent unconditionally
        // AFTER the batch, as the last thing each iteration does before
        // blocking for the next request.
        let mut disconnected = false;
        for response in batch
            .into_iter()
            .chain(std::iter::once(VmResponse::WorkerIdle))
        {
            if responses.send(response).is_err() {
                disconnected = true; // main thread's receiver is gone
                break;
            }
        }
        if disconnected {
            break;
        }
        wake.notify();
    }
}

/// Routes `Transcript show:`/`printOnStdout:` output (SPEC §16.2) onto the
/// worker's own response channel as ordinary `VmResponse::Transcript`
/// messages, waking the main thread after EVERY line rather than only once
/// the whole request finishes. That immediacy matters most for the case
/// this whole sprint (S21) exists to handle: a fatal guest condition writes
/// its error message here BEFORE the worker thread terminates
/// (`FatalMode::ExitThread`/`fatal_exit`, `embed.rs`) — batching the wake
/// until `handle` returns would mean it never fires at all for that
/// request, since `handle` never returns in that case.
struct ChannelTranscript {
    responses: Sender<VmResponse>,
    wake: CrossThreadObjcRef,
}

impl TranscriptSink for ChannelTranscript {
    fn show(&mut self, text: &str) {
        if self
            .responses
            .send(VmResponse::Transcript(text.to_string()))
            .is_ok()
        {
            self.wake.notify();
        }
    }
}

/// VM options for the GUI's own worker. Same as `VmOptions::from_env`
/// (`MACVM_JIT`/`MACVM_HEAP`/… all honored), except the JIT defaults to ON
/// when `MACVM_JIT` is unset — the GUI runs real programs interactively, and
/// the JIT is the whole point. `threshold=10` compiles a method after a brief
/// warmup: the *interactive* workload (tool/outliner rendering) is many
/// methods each called dozens of times, not one 2000-iteration hot loop, so a
/// high threshold (the old 2000) never compiled the UI code and left it
/// interpreter-slow; 10 gets it compiled within the first render or two while
/// still not compiling genuinely one-shot boot code. `MACVM_JIT` still
/// overrides (including `MACVM_JIT=off`); the library default
/// (`from_env` → off) is deliberately left alone so the test suite keeps its
/// pure-interpreter baseline (gate scripts opt in explicitly).
fn gui_vm_options() -> macvm::runtime::VmOptions {
    let mut opts = macvm::runtime::VmOptions::from_env();
    if std::env::var_os("MACVM_JIT").is_none() {
        opts.jit = macvm::runtime::JitMode::Threshold(10);
    }
    opts
}

/// Boots the real embedded VM (SPEC §16.2, `src/embed.rs`) against
/// `world_dir` using [`gui_vm_options`] (JIT on by default). Used as the
/// `.mst` fallback when the image database can't be established.
fn boot_real_vm(
    responses: Sender<VmResponse>,
    wake: CrossThreadObjcRef,
    world_dir: &Path,
) -> Option<VmHandle> {
    let opts = gui_vm_options();
    match VmHandle::boot(opts, world_dir) {
        Ok(mut vm) => {
            vm.set_transcript(Box::new(ChannelTranscript { responses, wake }));
            Some(vm)
        }
        Err(e) => {
            let _ = responses.send(VmResponse::Transcript(format!("VM boot failed: {}", e.msg)));
            wake.notify();
            None
        }
    }
}

/// The image database path (S22): `MACVM_IMAGE_PATH` if set, else the
/// default `<world_dir>/image.sqlite3` — so the DB is the out-of-the-box
/// source of truth, no env var required.
fn resolve_image_path(world_dir: &Path) -> PathBuf {
    std::env::var_os("MACVM_IMAGE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| world_dir.join("image.sqlite3"))
}

/// Opens the image at `image_path` (creating the file if missing), seeding
/// it from `world_dir`'s `.mst` files the first time (when it has no
/// classes) so a fresh checkout Just Works with zero setup. `docs/IMAGE.md`
/// §6/§8.
fn open_or_seed_image(world_dir: &Path, image_path: &Path) -> Result<image_store::Image, String> {
    let image = image_store::Image::open(image_path)
        .map_err(|e| format!("opening image {}: {e}", image_path.display()))?;
    let empty = image
        .all_classes()
        .map_err(|e| format!("reading image {}: {e}", image_path.display()))?
        .is_empty();
    if empty {
        let stats = image_store::import::import_world_dir(&image, world_dir)?;
        eprintln!(
            "vm_host: seeded {} from {} ({} classes, {} methods)",
            image_path.display(),
            world_dir.display(),
            stats.classes,
            stats.methods
        );
    }
    Ok(image)
}

/// Boots the VM FROM the image (S22): genesis, then replay the whole world
/// out of the database ([`crate::world_boot::load_world_from_image`]) rather
/// than the `.mst` file tree. Installs the transcript sink first so any
/// load-time output reaches the GUI. Returns `None` (reporting to the
/// transcript) on a load failure, so the caller can end this worker
/// generation and let the supervisor respawn.
fn boot_vm_from_image(
    image: &image_store::Image,
    responses: Sender<VmResponse>,
    wake: CrossThreadObjcRef,
) -> Option<VmHandle> {
    let opts = gui_vm_options();
    let mut vm = VmHandle::boot_without_world(opts);
    vm.set_transcript(Box::new(ChannelTranscript {
        responses: responses.clone(),
        wake,
    }));
    match crate::world_boot::load_world_from_image(&mut vm, image, WORLD_DOITS) {
        Ok(stats) => {
            eprintln!(
                "vm_host: booted VM from image ({} classes, {} methods, {} doits)",
                stats.classes, stats.methods, stats.doits
            );
            Some(vm)
        }
        Err(e) => {
            let _ = responses.send(VmResponse::Transcript(format!(
                "VM boot from image failed: {e}"
            )));
            wake.notify();
            None
        }
    }
}

/// Wraps one method's edited `.mst` `text` as a reopen of `class_name`, so
/// [`live_compile`] installs/replaces just that method in the running VM.
/// The superclass (from the mock, which was just updated) is needed for the
/// reopen — `install_class_def` checks it matches the live class. Class-side
/// methods carry their own `<Class> class >>` prefix in the stored source,
/// so `text` drops straight in either way.
fn reopen_one_method(world: &MockWorld, class_name: &str, text: &str) -> String {
    let superclass = world
        .class_named(class_name)
        .and_then(|c| c.superclass.clone())
        .unwrap_or_else(|| "nil".to_string());
    format!(
        "{superclass} subclass: {class_name} [\n{}\n]\n",
        text.trim()
    )
}

/// Compiles `mst_source` into the running VM (S22-E) so a browser edit is
/// LIVE — usable immediately — not merely saved to the mock + image. A
/// compile failure is returned as a short string (the edit is still saved;
/// it just isn't live until fixed), never a panic or a thread kill: a syntax
/// error is a `GuestError::Compile`, and `exec` recovers a genuine native
/// fault too (`embed.rs`).
fn live_compile(vm: &mut VmHandle, mst_source: &str) -> Result<(), String> {
    vm.exec(mst_source).map_err(|e| e.to_string())
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
        world.add_class(
            &c.name,
            c.superclass.as_deref(),
            &c.category,
            &c.comment,
            &c.instance_vars,
            &c.class_vars,
        );
    }
    for c in &classes {
        for m in image.all_methods_of(&c.name).unwrap_or_default() {
            world.add_method(
                &c.name,
                side_from_image(m.side),
                &m.selector,
                &m.category,
                &m.source,
            );
        }
    }
    world
}

thread_local! {
    /// The open smappl outliners on the current page — the `ToolRegistry`
    /// (`docs/APPS.md` §7): `(widget_id, visual= code)`. Populated as each
    /// outliner renders; consulted after an accept to live-refresh the others
    /// so an edit in one view shows up in the rest without a page reload. Kept
    /// thread-local on the single-threaded worker so `handle`'s signature (and
    /// its ~15 test call sites) stay put. Bounded: `data-widget-id`s restart
    /// at `s0` per page, so re-rendering a page replaces its own entries.
    static OPEN_OUTLINERS: std::cell::RefCell<Vec<(String, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Render a `visual=` expression and fill any ClassOutliner source blocks from
/// image_store (the VM keeps no method source). `None` on a render failure.
fn render_and_inject(
    vm: &mut VmHandle,
    image: Option<&image_store::Image>,
    code: &str,
) -> Option<String> {
    let html = vm.render_fragment(code).ok()?;
    Some(match image {
        Some(img) => crate::preprocess::inject_method_sources(&html, |c, s, sel| {
            let side = if s == "class" {
                image_store::Side::Class
            } else {
                image_store::Side::Instance
            };
            img.method_source(c, side, sel).ok().flatten()
        }),
        None => html,
    })
}

use crate::preprocess::tag_widget_id;

/// Re-render every open outliner except `skip_id` (the one being edited, whose
/// DOM must not be disrupted mid-edit) and answer fragment updates for them —
/// the live half of the `ToolRegistry` (`docs/APPS.md` §7). Called after an
/// accept so a change shows up across all open outliners.
fn refresh_open_outliners(
    vm: &mut VmHandle,
    image: Option<&image_store::Image>,
    skip_id: &str,
) -> Vec<VmResponse> {
    let entries: Vec<(String, String)> = OPEN_OUTLINERS.with(|o| o.borrow().clone());
    let mut out = Vec::new();
    for (id, code) in entries {
        if id == skip_id {
            continue;
        }
        if let Some(html) = render_and_inject(vm, image, &code) {
            out.push(VmResponse::SmapplFragment {
                html: tag_widget_id(&html, &id),
                id,
            });
        }
    }
    out
}

/// `Doit`/`WorkspacePrintIt` (S21 step 3) call the real `vm.eval` — see
/// `boot_real_vm`. Every other request is still the Browser/Canvas mock
/// (`docs/SPEC.md` §16.3's mirror-primitive bridge is a separate, later
/// piece of work). The `BrowserSelect*` arms update `selection` then
/// re-render every pane — simplest-thing-that-works at mock scale (see
/// `VmResponse::BrowserPanes`'s doc comment). Returns a `Vec` rather than a
/// single `VmResponse` because `BrowserSaveSource` needs to deliver two
/// things — updated panes *and* a transcript confirmation — not because
/// any request produces a high-frequency stream (that's `LatestSlot`'s job,
/// see this module's doc comment).
fn handle(
    request: VmRequest,
    world: &mut MockWorld,
    selection: &mut BrowserSelection,
    image: Option<&image_store::Image>,
    vm: &mut VmHandle,
) -> Vec<VmResponse> {
    match request {
        VmRequest::Doit { code } => {
            // "Do it": evaluate for effect (Transcript show:, etc. arrive
            // separately via ChannelTranscript) and echo what ran — the
            // result value itself is deliberately not shown, matching
            // "Print it"'s (WorkspacePrintIt) own distinct job below.
            match vm.eval(&code) {
                Ok(_) => vec![VmResponse::Transcript(format!("> {code}"))],
                Err(e) => vec![VmResponse::Transcript(format!("> {code}\n{e}"))],
            }
        }
        VmRequest::SmapplRender { id, code } => {
            // Render the widget image-side (D-G5). A shape that can't be
            // built yet fails cleanly — answer nothing so the placeholder box
            // remains (the swallow-to-fallback contract, `../smappl.md` §2),
            // rather than surfacing the error onto the page.
            match render_and_inject(vm, image, &code) {
                Some(html) => {
                    // Register open outliners in the ToolRegistry so an accept
                    // elsewhere can live-refresh them (docs/APPS.md §7);
                    // replace any prior entry for this widget id (page reload).
                    if html.contains("st-outliner") {
                        OPEN_OUTLINERS.with(|o| {
                            let mut v = o.borrow_mut();
                            v.retain(|(wid, _)| wid != &id);
                            v.push((id.clone(), code.clone()));
                        });
                    }
                    vec![VmResponse::SmapplFragment {
                        html: tag_widget_id(&html, &id),
                        id,
                    }]
                }
                None => vec![],
            }
        }
        VmRequest::SmapplAction { action_id } => {
            // Fire the widget's stored action closure. Any Transcript output
            // it produces arrives separately via ChannelTranscript; a failure
            // is echoed to the transcript like a doit rather than dropped.
            let code = format!("SmapplRegistry fire: '{action_id}'.");
            match vm.exec(&code) {
                Ok(()) => vec![],
                Err(e) => vec![VmResponse::Transcript(format!("{e}"))],
            }
        }
        VmRequest::SmapplAccept {
            cls,
            side,
            sel,
            text,
            widget_id,
        } => {
            // Reuse the class browser's proven accept path (BrowserSaveSource):
            // (1) VERSION the edit into image_store — a new method_versions row,
            //     never an overwrite (the user's explicit requirement);
            // (2) mirror it into `world` so the browser view stays consistent;
            // (3) live-compile it into the running VM so it takes effect now.
            let img_side = if side == "class" {
                image_store::Side::Class
            } else {
                image_store::Side::Instance
            };
            let mirror_side = if side == "class" {
                Side::Class
            } else {
                Side::Instance
            };
            world.set_method_source(&cls, mirror_side, &sel, text.clone());
            let versioned = image
                .map(|img| img.set_method_source(&cls, img_side, &sel, &text))
                .transpose();
            // Reopen source needs the real superclass. image_store is the
            // authoritative source (the running VM has it too, but the mock
            // `world` may not — it isn't synced to arbitrary VM classes).
            let superclass = image
                .and_then(|img| img.superclass_of(&cls).ok().flatten())
                .or_else(|| world.class_named(&cls).and_then(|c| c.superclass.clone()))
                .unwrap_or_else(|| "nil".to_string());
            let reopen = format!("{superclass} subclass: {cls} [\n{}\n]\n", text.trim());
            let versioned = match versioned {
                Ok(_) => match live_compile(vm, &reopen) {
                    Ok(()) => format!("Accepted {cls}>>{sel} — versioned to the image and live"),
                    Err(e) => format!("Accepted {cls}>>{sel} to the image, but compile failed: {e}"),
                },
                Err(e) => format!("{cls}>>{sel}: image write FAILED (not versioned): {e}"),
            };
            // Live-refresh every OTHER open outliner so the edit shows up
            // across all views on the page (ToolRegistry, docs/APPS.md §7);
            // the edited outliner (`widget_id`) is skipped so its editor stays.
            let mut responses = vec![VmResponse::Transcript(versioned)];
            responses.extend(refresh_open_outliners(vm, image, &widget_id));
            responses
        }
        VmRequest::SmapplOpenClass {
            cls,
            root,
            widget_id,
        } => {
            let code = format!("ClassOutliner for: (ClassMirror on: {cls})");
            match render_and_inject(vm, image, &code) {
                Some(inner) => {
                    // Wrap with a back link to the hierarchy; the wrapper is
                    // the fragment root the widget-id is stamped on.
                    // `data-hierarchy-root` on the wrapper lets a nested drill
                    // (a subclass link inside this browser, e.g. Boolean→True)
                    // keep the same back-to-hierarchy target.
                    let wrapped = format!(
                        "<div class=\"st-classbrowser\" data-hierarchy-root=\"{root}\">\
                         <div class=\"st-back\">\
                         <span class=\"st-class-link\" data-open-hierarchy=\"{root}\">\
                         &#8249; {root} hierarchy</span></div>{inner}</div>"
                    );
                    OPEN_OUTLINERS.with(|o| {
                        let mut v = o.borrow_mut();
                        v.retain(|(wid, _)| wid != &widget_id);
                        v.push((widget_id.clone(), code));
                    });
                    vec![VmResponse::SmapplFragment {
                        html: tag_widget_id(&wrapped, &widget_id),
                        id: widget_id,
                    }]
                }
                None => vec![VmResponse::Transcript(format!("(cannot open {cls})"))],
            }
        }
        VmRequest::SmapplOpenHierarchy { root, widget_id } => {
            let code = format!("ClassHierarchyOutliner imbeddedVisualForClass: {root}");
            match render_and_inject(vm, image, &code) {
                Some(html) => {
                    OPEN_OUTLINERS.with(|o| {
                        let mut v = o.borrow_mut();
                        v.retain(|(wid, _)| wid != &widget_id);
                        v.push((widget_id.clone(), code));
                    });
                    vec![VmResponse::SmapplFragment {
                        html: tag_widget_id(&html, &widget_id),
                        id: widget_id,
                    }]
                }
                None => vec![VmResponse::Transcript(format!("(cannot open {root} hierarchy)"))],
            }
        }
        VmRequest::Find { tool, query } => {
            // Double single-quotes for the Smalltalk string literal.
            let q = query.replace('\'', "''");
            let code = match tool.as_str() {
                "senders" => format!("SendersView of: '{q}'"),
                _ => format!("ImplementorsView of: '{q}'"),
            };
            let html = render_and_inject(vm, image, &code).unwrap_or_else(|| {
                "<div class=\"st-find-empty\">(this find tool isn't available yet)</div>"
                    .to_string()
            });
            vec![VmResponse::FindResults { html }]
        }
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
            let VmResponse::BrowserPanes {
                packages_html,
                classes_html,
                categories_html,
                methods_html,
                source_html,
            } = render_browser_panes(world, selection)
            else {
                unreachable!("render_browser_panes always returns BrowserPanes")
            };
            vec![VmResponse::BrowserOpened {
                packages_html,
                classes_html,
                categories_html,
                methods_html,
                source_html,
            }]
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
                    let image_error =
                        image.and_then(|img| describe_image_write(img.remove_class(&class_name)));
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
            vec![
                render_browser_panes(world, selection),
                VmResponse::Transcript(message),
            ]
        }
        VmRequest::BrowserRemoveMethod => {
            let message = match (selection.class.clone(), selection.method.clone()) {
                (Some(class_name), Some(selector)) => {
                    let removed = world.remove_method(&class_name, selection.side, &selector);
                    let image_error = image.and_then(|img| {
                        describe_image_write(img.remove_method(
                            &class_name,
                            side_to_image(selection.side),
                            &selector,
                        ))
                    });
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
            vec![
                render_browser_panes(world, selection),
                VmResponse::Transcript(message),
            ]
        }
        VmRequest::BrowserSaveSource {
            text,
            saved_package,
            saved_class,
            saved_side,
            saved_category,
            saved_method,
            saved_target,
        } => {
            // The source pane's `data-save-*` snapshot (`browser_render.rs`)
            // must still match what's actually selected right now — a
            // selection-change request queued between the render this came
            // from and this save arriving would otherwise apply the edited
            // text to whatever's selected *now* instead of what was on
            // screen when the user hit Cmd+S. Reject rather than guess:
            // the panes still get re-rendered (showing current reality),
            // but the stale edit is simply dropped, same as any other
            // unsaved-edit-discarded-by-navigation case.
            let snapshot = BrowserSelection::from_wire(
                &saved_package,
                &saved_class,
                &saved_side,
                &saved_category,
                &saved_method,
                &saved_target,
            );
            if snapshot != *selection {
                return vec![
                    render_browser_panes(world, selection),
                    VmResponse::Transcript(
                        "Selection changed since this edit was opened — not saved.".to_string(),
                    ),
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
            // S22-E: remember the edit KIND before the arms below mutate
            // `selection.edit_target` (NewMethod→Method, NewClass→ClassComment),
            // so the post-match live-compile knows how to make it live.
            let edit_kind = selection.edit_target.clone();
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
            // S22-E: make a SUCCESSFUL edit LIVE in the running VM — not just
            // saved to the mock + image. `text` is the edited source; the
            // (post-edit) `selection` names the class. A ClassComment edit
            // compiles nothing (`None`). A live-compile failure is reported
            // but does NOT undo the save — the source is persisted; the user
            // fixes and re-accepts.
            let vm_live: Option<Result<(), String>> = match (&outcome, &edit_kind) {
                (Ok(_), SourceEditTarget::Method | SourceEditTarget::NewMethod) => selection
                    .class
                    .clone()
                    .map(|c| live_compile(vm, &reopen_one_method(world, &c, &text))),
                (Ok(_), SourceEditTarget::ClassDefinition | SourceEditTarget::NewClass) => {
                    Some(live_compile(vm, &text))
                }
                _ => None,
            };

            let message = match outcome {
                Ok(SaveOk { what, image_error }) => {
                    let base = match (image.is_some(), image_error) {
                        (true, None) => format!("{what} (persisted to image)"),
                        (_, None) => what,
                        (_, Some(reason)) => {
                            format!("{what}, but failed to persist to the image: {reason}")
                        }
                    };
                    match vm_live {
                        Some(Ok(())) => format!("{base} — live in the VM."),
                        Some(Err(e)) => format!("{base}; saved but NOT live in the VM: {e}."),
                        None => format!("{base}."),
                    }
                }
                Err(e) => e,
            };
            vec![
                render_browser_panes(world, selection),
                VmResponse::Transcript(message),
            ]
        }
        VmRequest::WorkspacePrintIt { code } => {
            // "Print it": insert the real printString inline right after
            // the selection (`main.rs`'s `WorkspacePrintResult` handler) —
            // `eval`'s `Ok(String)` already IS that printString (e.g. a
            // Smalltalk String's own printString already carries its own
            // quotes), so it's inserted as-is, not re-quoted.
            let text = match vm.eval(&code) {
                Ok(result) => format!(" {result}"),
                Err(e) => format!(" \"ERROR: {e}\""),
            };
            vec![VmResponse::WorkspacePrintResult { text }]
        }
        VmRequest::CanvasCreate { width, height } => {
            vec![VmResponse::CanvasCreated {
                id: CANVAS_ID,
                width,
                height,
            }]
        }
        VmRequest::CanvasRunDemo => {
            vec![VmResponse::CanvasDraw {
                id: CANVAS_ID,
                commands_json: canvas_demo_commands().to_json(),
            }]
        }
        VmRequest::CanvasClear => {
            let mut cmds = CanvasCommands::new();
            cmds.call(
                "clearRect",
                &[0.0.into(), 0.0.into(), 10_000.0.into(), 10_000.0.into()],
            );
            vec![
                VmResponse::CanvasCleared { id: CANVAS_ID },
                VmResponse::CanvasDraw {
                    id: CANVAS_ID,
                    commands_json: cmds.to_json(),
                },
            ]
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
    c.call(
        "clearRect",
        &[0.0.into(), 0.0.into(), 10_000.0.into(), 10_000.0.into()],
    );
    c.call("fillStyle", &["steelblue".into()]);
    c.call(
        "fillRect",
        &[20.0.into(), 20.0.into(), 120.0.into(), 80.0.into()],
    );
    c.call("strokeStyle", &["crimson".into()]);
    c.call("lineWidth", &[3.0.into()]);
    c.call("beginPath", &[]);
    c.call(
        "arc",
        &[
            220.0.into(),
            60.0.into(),
            40.0.into(),
            0.0.into(),
            (std::f64::consts::PI * 2.0).into(),
        ],
    );
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
    c.call(
        "fillText",
        &["MACVM Canvas".into(), 20.0.into(), 175.0.into()],
    );
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
        Self {
            value: Mutex::new(None),
        }
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

    /// The real `world/` directory, resolved via `CARGO_MANIFEST_DIR`
    /// rather than a bare relative path: `cargo test`'s working directory
    /// is this crate's own root (`gui/`), not the workspace root `world/`
    /// actually lives under.
    fn test_world_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../world"))
    }

    /// A real, booted `VmHandle` (heap kept small for a test — the GUI's
    /// own worker uses `VmOptions::from_env`'s larger default) for tests
    /// that exercise `handle()`'s arms directly.
    fn test_vm_handle(jit: macvm::runtime::JitMode) -> VmHandle {
        VmHandle::boot(
            macvm::runtime::VmOptions {
                heap_mib: 64,
                jit,
                ..Default::default()
            },
            &test_world_dir(),
        )
        .expect("boot against the real world/ directory must succeed")
    }

    #[test]
    fn browser_select_category_treats_empty_name_as_clearing_to_all() {
        let mut world = MockWorld::seed();
        let mut selection = BrowserSelection {
            class: Some("Object".to_string()),
            ..Default::default()
        };
        let mut vm = test_vm_handle(macvm::runtime::JitMode::Off);
        handle(
            VmRequest::BrowserSelectCategory {
                name: "printing".to_string(),
            },
            &mut world,
            &mut selection,
            None,
            &mut vm,
        );
        assert_eq!(selection.category.as_deref(), Some("printing"));

        handle(
            VmRequest::BrowserSelectCategory {
                name: String::new(),
            },
            &mut world,
            &mut selection,
            None,
            &mut vm,
        );
        assert_eq!(selection.category, None);
    }

    #[test]
    fn browser_new_method_only_requires_a_class_not_a_category() {
        let mut world = MockWorld::seed();
        let mut selection = BrowserSelection {
            class: Some("Object".to_string()),
            ..Default::default()
        };
        let mut vm = test_vm_handle(macvm::runtime::JitMode::Off);
        assert_eq!(selection.category, None); // "(all)" — no category picked
        handle(
            VmRequest::BrowserNewMethod,
            &mut world,
            &mut selection,
            None,
            &mut vm,
        );
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
        let mut vm = test_vm_handle(macvm::runtime::JitMode::Off);
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
            &mut vm,
        );
        // Neither method was touched — not hash (what's *currently*
        // selected) and not printString (what the save actually said it
        // was for).
        assert_eq!(
            world
                .method_source("Object", Side::Instance, "hash")
                .unwrap(),
            "hash\n\t^self identityHash"
        );
        assert!(world
            .method_source("Object", Side::Instance, "printString")
            .unwrap()
            .starts_with("printString\n\t^String"));
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
        let mut vm = test_vm_handle(macvm::runtime::JitMode::Off);
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
            &mut vm,
        );
        assert_eq!(
            world
                .method_source("Object", Side::Instance, "hash")
                .unwrap(),
            "hash\n\t^42"
        );
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
        let mut selection = BrowserSelection {
            class: Some("Ghost".to_string()),
            edit_target: SourceEditTarget::ClassComment,
            ..Default::default()
        };
        let mut vm = test_vm_handle(macvm::runtime::JitMode::Off);
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
            &mut vm,
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
        assert_eq!(
            cmds.to_json(),
            r#"[["beginPath"],["lineTo",20,140],["fillStyle","steelblue"]]"#
        );
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
        let mut vm = test_vm_handle(macvm::runtime::JitMode::Off);
        let responses = handle(
            VmRequest::CanvasCreate {
                width: 640,
                height: 480,
            },
            &mut world,
            &mut selection,
            None,
            &mut vm,
        );
        assert!(matches!(
            responses.as_slice(),
            [VmResponse::CanvasCreated {
                id: 0,
                width: 640,
                height: 480
            }]
        ));
    }

    #[test]
    fn canvas_run_demo_returns_one_nonempty_draw_batch_for_the_fixed_canvas() {
        let mut world = MockWorld::seed();
        let mut selection = BrowserSelection::default();
        let mut vm = test_vm_handle(macvm::runtime::JitMode::Off);
        let responses = handle(
            VmRequest::CanvasRunDemo,
            &mut world,
            &mut selection,
            None,
            &mut vm,
        );
        let [VmResponse::CanvasDraw { id, commands_json }] = responses.as_slice() else {
            panic!("expected exactly one CanvasDraw response");
        };
        assert_eq!(*id, 0);
        assert!(
            commands_json.starts_with('[') && commands_json.ends_with(']'),
            "{commands_json}"
        );
        assert!(commands_json.contains("fillRect"), "{commands_json}");
        assert!(commands_json.contains("MACVM Canvas"), "{commands_json}");
    }

    #[test]
    fn canvas_clear_answers_cleared_then_a_full_canvas_clear_rect_batch() {
        let mut world = MockWorld::seed();
        let mut selection = BrowserSelection::default();
        let mut vm = test_vm_handle(macvm::runtime::JitMode::Off);
        let responses = handle(
            VmRequest::CanvasClear,
            &mut world,
            &mut selection,
            None,
            &mut vm,
        );
        let [VmResponse::CanvasCleared { id: cleared_id }, VmResponse::CanvasDraw {
            id: draw_id,
            commands_json,
        }] = responses.as_slice()
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

    // ── S21 step 3: real VmHandle wiring + restart-on-death ───────────────

    fn transcript_containing(rs: &[VmResponse], needle: &str) -> bool {
        rs.iter()
            .any(|r| matches!(r, VmResponse::Transcript(t) if t.contains(needle)))
    }

    /// G2 smappl slice: a `<smappl>` render request evaluates the `visual=`
    /// code and answers a live button fragment; the fragment's advertised
    /// action id fires the widget's closure; an unbuildable shape answers no
    /// fragment (the placeholder box stays). Exercises the worker seam
    /// (`handle`) end to end, the same path `main.rs` drives.
    #[test]
    fn smappl_render_and_action_round_trip_through_handle() {
        let mut world = MockWorld::seed();
        let mut selection = BrowserSelection::default();
        let mut vm = test_vm_handle(macvm::runtime::JitMode::Off);

        let rendered = handle(
            VmRequest::SmapplRender {
                id: "s0".to_string(),
                code: "Button labeled: 'Hi' action: [ :b | Transcript show: 'fired' ]".to_string(),
            },
            &mut world,
            &mut selection,
            None,
            &mut vm,
        );
        let (id, html) = match rendered.as_slice() {
            [VmResponse::SmapplFragment { id, html }] => (id.clone(), html.clone()),
            other => panic!("expected one SmapplFragment, got {other:?}"),
        };
        assert_eq!(id, "s0", "the answer must carry back the placeholder's id");
        assert!(
            html.contains("smappl-button") && html.contains("Hi"),
            "must render a live beveled button carrying its label, got {html:?}"
        );

        // The action id the fragment advertises fires the stored closure.
        let marker = "data-widget-action=\"";
        let start = html.find(marker).expect("fragment has an action id") + marker.len();
        let end = html[start..].find('"').unwrap() + start;
        let action_id = html[start..end].to_string();
        let fired = handle(
            VmRequest::SmapplAction { action_id },
            &mut world,
            &mut selection,
            None,
            &mut vm,
        );
        // A clean fire produces no UI response of its own (its Transcript
        // output goes to the sink, absent in this direct test) — the point is
        // it ran without surfacing an error back.
        assert!(
            fired.is_empty(),
            "a clean action fire returns no error responses, got {fired:?}"
        );

        // A shape that isn't built yet (an unknown tool class — e.g. the
        // CodeView shape, gui/smappl.md §3.5) renders nothing, so the GUI
        // keeps showing the G0 placeholder box.
        let unbuildable = handle(
            VmRequest::SmapplRender {
                id: "s1".to_string(),
                code: "CodeView forString".to_string(),
            },
            &mut world,
            &mut selection,
            None,
            &mut vm,
        );
        assert!(
            unbuildable.is_empty(),
            "an unbuildable shape must yield no fragment, got {unbuildable:?}"
        );
    }

    /// The Implementors find tool lists every class that defines the selector,
    /// each a drill-link into that class's browser.
    #[test]
    fn find_implementors_lists_classes_that_define_the_selector() {
        OPEN_OUTLINERS.with(|o| o.borrow_mut().clear());
        let mut world = MockWorld::seed();
        let mut sel = BrowserSelection::default();
        let mut vm = test_vm_handle(macvm::runtime::JitMode::Off);
        let responses = handle(
            VmRequest::Find {
                tool: "implementors".to_string(),
                query: "printOn:".to_string(),
            },
            &mut world,
            &mut sel,
            None,
            &mut vm,
        );
        let html = match responses.as_slice() {
            [VmResponse::FindResults { html }] => html.clone(),
            other => panic!("expected FindResults, got {other:?}"),
        };
        assert!(
            html.contains("Implementors of") && html.contains("data-open-class=\"Point\""),
            "must list Point among printOn: implementors (a drill-link), got a {}-char result",
            html.len()
        );
    }

    /// Drill-down: clicking a class in a hierarchy outliner opens that class's
    /// method browser (a ClassOutliner with editors) in the same widget, with a
    /// back link to the hierarchy.
    #[test]
    fn open_class_drills_into_a_method_browser_with_a_back_link() {
        OPEN_OUTLINERS.with(|o| o.borrow_mut().clear());
        let world_dir = test_world_dir();
        let tmp = std::env::temp_dir()
            .join(format!("macvm_drill_{}.sqlite3", std::process::id()));
        std::fs::remove_file(&tmp).ok();
        let image = open_or_seed_image(&world_dir, &tmp).expect("seed");
        let mut vm = VmHandle::boot_without_world(macvm::runtime::VmOptions {
            heap_mib: 64,
            jit: macvm::runtime::JitMode::Off,
            ..Default::default()
        });
        crate::world_boot::load_world_from_image(&mut vm, &image, WORLD_DOITS).expect("db load");
        let mut world = MockWorld::seed();
        let mut sel = BrowserSelection::default();

        let opened = handle(
            VmRequest::SmapplOpenClass {
                cls: "Point".to_string(),
                root: "Object".to_string(),
                widget_id: "s0".to_string(),
            },
            &mut world,
            &mut sel,
            Some(&image),
            &mut vm,
        );
        let html = match opened.as_slice() {
            [VmResponse::SmapplFragment { id, html }] if id == "s0" => html.clone(),
            other => panic!("expected a SmapplFragment for s0, got {other:?}"),
        };
        assert!(
            html.contains("st-classoutliner")
                && html.contains("st-smappl-src")
                && html.contains("instance side"),
            "drilling into Point must show its method browser with editors, got a {}-char fragment",
            html.len()
        );
        assert!(
            html.contains("data-open-hierarchy=\"Object\""),
            "the class browser must carry a back link to the Object hierarchy: {html}"
        );

        // Back re-opens the hierarchy in the same widget.
        let back = handle(
            VmRequest::SmapplOpenHierarchy {
                root: "Object".to_string(),
                widget_id: "s0".to_string(),
            },
            &mut world,
            &mut sel,
            Some(&image),
            &mut vm,
        );
        assert!(
            matches!(back.as_slice(), [VmResponse::SmapplFragment { id, html }]
                if id == "s0" && html.contains("st-outliner") && html.contains("data-open-class")),
            "back must re-open the clickable hierarchy, got {back:?}"
        );

        std::fs::remove_file(&tmp).ok();
    }

    /// View sync (ToolRegistry, docs/APPS.md §7): with two outliners open, an
    /// accept in one live-refreshes the OTHER (so the edit shows everywhere)
    /// but skips the one being edited (so its editor isn't yanked mid-edit).
    #[test]
    fn accept_refreshes_other_open_outliners_but_not_the_edited_one() {
        OPEN_OUTLINERS.with(|o| o.borrow_mut().clear());
        let world_dir = test_world_dir();
        let tmp = std::env::temp_dir()
            .join(format!("macvm_sync_{}.sqlite3", std::process::id()));
        std::fs::remove_file(&tmp).ok();
        let image = open_or_seed_image(&world_dir, &tmp).expect("seed");
        let mut vm = VmHandle::boot_without_world(macvm::runtime::VmOptions {
            heap_mib: 64,
            jit: macvm::runtime::JitMode::Off,
            ..Default::default()
        });
        crate::world_boot::load_world_from_image(&mut vm, &image, WORLD_DOITS).expect("db load");
        let mut world = MockWorld::seed();
        let mut sel = BrowserSelection::default();

        // Two open Point outliners: s0 (being edited) and s1 (a bystander).
        for id in ["s0", "s1"] {
            handle(
                VmRequest::SmapplRender {
                    id: id.to_string(),
                    code: "ClassOutliner for: (ClassMirror on: Point)".to_string(),
                },
                &mut world,
                &mut sel,
                Some(&image),
                &mut vm,
            );
        }

        let responses = handle(
            VmRequest::SmapplAccept {
                cls: "Point".to_string(),
                side: "instance".to_string(),
                sel: "y".to_string(),
                text: "y [ ^42 ]".to_string(),
                widget_id: "s0".to_string(), // the edited outliner
            },
            &mut world,
            &mut sel,
            Some(&image),
            &mut vm,
        );

        let refreshed: Vec<&str> = responses
            .iter()
            .filter_map(|r| match r {
                VmResponse::SmapplFragment { id, html } => {
                    assert!(html.contains("^42"), "refresh must carry the new source");
                    Some(id.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            refreshed,
            vec!["s1"],
            "only the bystander outliner refreshes; the edited one (s0) is skipped, got {refreshed:?}"
        );

        std::fs::remove_file(&tmp).ok();
    }

    /// Accepting a ClassOutliner edit VERSIONS it into image_store (a new
    /// `method_versions` row, never an overwrite — the user's explicit
    /// requirement) AND live-compiles it into the running VM.
    #[test]
    fn smappl_accept_versions_the_edit_and_makes_it_live() {
        OPEN_OUTLINERS.with(|o| o.borrow_mut().clear());
        let world_dir = test_world_dir();
        let tmp = std::env::temp_dir()
            .join(format!("macvm_accept_{}.sqlite3", std::process::id()));
        std::fs::remove_file(&tmp).ok();
        let image = open_or_seed_image(&world_dir, &tmp).expect("seed");

        let mut vm = VmHandle::boot_without_world(macvm::runtime::VmOptions {
            heap_mib: 64,
            jit: macvm::runtime::JitMode::Off,
            ..Default::default()
        });
        crate::world_boot::load_world_from_image(&mut vm, &image, WORLD_DOITS).expect("db load");
        let mut world = MockWorld::seed();
        let mut selection = BrowserSelection::default();

        let versions_before = image
            .method_version_count("Point", image_store::Side::Instance, "y")
            .unwrap();
        assert_eq!(
            vm.eval("(Point x: 7 y: 2) y.").unwrap(),
            "2",
            "sanity: Point>>y returns the y ivar"
        );

        // Redefine Point>>y to return the x ivar — proves redefinition is live
        // AND that reopening preserved the ivars.
        let responses = handle(
            VmRequest::SmapplAccept {
                cls: "Point".to_string(),
                side: "instance".to_string(),
                sel: "y".to_string(),
                text: "y [ ^x ]".to_string(),
                widget_id: String::new(),
            },
            &mut world,
            &mut selection,
            Some(&image),
            &mut vm,
        );
        assert!(
            transcript_containing(&responses, "versioned"),
            "accept must report it versioned the edit, got {responses:?}"
        );

        // 1. A NEW version row exists (not an overwrite).
        let versions_after = image
            .method_version_count("Point", image_store::Side::Instance, "y")
            .unwrap();
        assert_eq!(
            versions_after,
            versions_before + 1,
            "each accept must add exactly one method_versions row"
        );
        // 2. The latest version in the DB is the edited text.
        assert_eq!(
            image
                .method_source("Point", image_store::Side::Instance, "y")
                .unwrap()
                .as_deref(),
            Some("y [ ^x ]"),
            "the image's latest version must be the edit"
        );
        // 3. The running VM now runs the redefined method (ivars preserved).
        assert_eq!(
            vm.eval("(Point x: 7 y: 2) y.").unwrap(),
            "7",
            "the edit must be live in the VM, with the x ivar preserved"
        );

        std::fs::remove_file(&tmp).ok();
    }

    /// A `ClassOutliner` rendered through `handle` gets its method-source
    /// `<pre>` blocks filled from image_store (the VM keeps no source) —
    /// exercises the full worker seam incl. `inject_method_sources`.
    #[test]
    fn class_outliner_source_is_filled_from_the_image() {
        let world_dir = test_world_dir();
        let tmp = std::env::temp_dir()
            .join(format!("macvm_co_src_{}.sqlite3", std::process::id()));
        std::fs::remove_file(&tmp).ok();
        let image = open_or_seed_image(&world_dir, &tmp).expect("seed");

        let mut vm = VmHandle::boot_without_world(macvm::runtime::VmOptions {
            heap_mib: 64,
            jit: macvm::runtime::JitMode::Off,
            ..Default::default()
        });
        crate::world_boot::load_world_from_image(&mut vm, &image, WORLD_DOITS).expect("db load");

        let mut world = MockWorld::seed();
        let mut selection = BrowserSelection::default();
        let responses = handle(
            VmRequest::SmapplRender {
                id: "s0".to_string(),
                code: "ClassOutliner for: (ClassMirror on: Point)".to_string(),
            },
            &mut world,
            &mut selection,
            Some(&image),
            &mut vm,
        );
        let html = match responses.as_slice() {
            [VmResponse::SmapplFragment { html, .. }] => html.clone(),
            other => panic!("expected one SmapplFragment, got {other:?}"),
        };
        // Point>>printOn:'s source body (from the image) must be spliced into
        // its selector node's <pre> — a snippet unique to the source, not the
        // selector list.
        assert!(
            html.contains("nextPutAll:") && html.contains("st-smappl-src"),
            "the class outliner must carry Point's method source, got a {}-char fragment",
            html.len()
        );

        std::fs::remove_file(&tmp).ok();
    }

    /// The real GUI boots from a DB image (S22), not `world/*.mst` directly.
    /// `33_smappl.mst` must survive that import/export round trip — S22 found
    /// several image_store fidelity bugs, so a widget rendering through the
    /// `.mst` boot is no guarantee it renders through the DB boot the shell
    /// actually uses. Seed → DB-boot → render the same button.
    #[test]
    fn smappl_renders_through_a_db_booted_vm() {
        let world_dir = test_world_dir();
        let tmp =
            std::env::temp_dir().join(format!("macvm_smappl_db_{}.sqlite3", std::process::id()));
        std::fs::remove_file(&tmp).ok();
        let image = open_or_seed_image(&world_dir, &tmp).expect("seed");

        let mut vm = VmHandle::boot_without_world(macvm::runtime::VmOptions {
            heap_mib: 64,
            jit: macvm::runtime::JitMode::Off,
            ..Default::default()
        });
        crate::world_boot::load_world_from_image(&mut vm, &image, WORLD_DOITS).expect("db load");

        let html = vm
            .render_fragment("Button labeled: 'DB' action: [ :b | b ]")
            .expect("the smappl classes must render through a DB-booted VM");
        assert!(
            html.contains("smappl-button") && html.contains("DB"),
            "the DB-booted world must render a live button, got {html:?}"
        );

        // The Phase-W outliner (allClasses primitive + ClassMirror +
        // HtmlWriter, world/34_tools.mst) must also survive the DB round trip
        // — this is the start page's own smappl.
        let tree = vm
            .render_fragment("ClassHierarchyOutliner imbeddedVisualForClass: Object")
            .expect("the hierarchy outliner must render through a DB-booted VM");
        assert!(
            tree.contains("st-outliner") && tree.contains("Object") && tree.contains("Behavior"),
            "the DB-booted world must render the class-hierarchy tree, got {tree:?}"
        );

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn doit_evaluates_through_the_real_vm_and_echoes_the_source() {
        let mut world = MockWorld::seed();
        let mut selection = BrowserSelection::default();
        let mut vm = test_vm_handle(macvm::runtime::JitMode::Off);
        let responses = handle(
            VmRequest::Doit {
                code: "3 + 4.".to_string(),
            },
            &mut world,
            &mut selection,
            None,
            &mut vm,
        );
        // "Do it" echoes what ran (the result value itself is "Print it"'s
        // job, not shown here) — but it went through the real evaluator, so
        // a well-formed doit produces exactly the echo and nothing else.
        assert!(matches!(responses.as_slice(), [VmResponse::Transcript(t)] if t.contains("3 + 4")));
    }

    #[test]
    fn workspace_print_it_returns_the_real_printstring() {
        let mut world = MockWorld::seed();
        let mut selection = BrowserSelection::default();
        let mut vm = test_vm_handle(macvm::runtime::JitMode::Off);
        let responses = handle(
            VmRequest::WorkspacePrintIt {
                code: "3 + 4.".to_string(),
            },
            &mut world,
            &mut selection,
            None,
            &mut vm,
        );
        let [VmResponse::WorkspacePrintResult { text }] = responses.as_slice() else {
            panic!("expected exactly one WorkspacePrintResult, got {responses:?}");
        };
        // The real printString of 7 — no longer the old "(no VM yet: ...)"
        // stub. Leading space is the inline-insertion convention.
        assert_eq!(text, " 7");
    }

    /// Regression for "the workspace evaluates nothing": the default
    /// placeholder buffer must be a leading comment FOLLOWED BY a runnable
    /// statement, so Do it / Print it on the untouched buffer (nothing
    /// selected → the whole buffer is the eval target) produce a real
    /// result. The old placeholder was pure comment, which parses to no
    /// statement and answers empty — indistinguishable from "nothing
    /// happened".
    #[test]
    fn the_default_workspace_placeholder_prints_a_real_result() {
        let mut world = MockWorld::seed();
        let mut selection = BrowserSelection::default();
        let mut vm = test_vm_handle(macvm::runtime::JitMode::Off);
        let responses = handle(
            VmRequest::WorkspacePrintIt {
                code: crate::workspace_render::initial_text().to_string(),
            },
            &mut world,
            &mut selection,
            None,
            &mut vm,
        );
        let [VmResponse::WorkspacePrintResult { text }] = responses.as_slice() else {
            panic!("expected exactly one WorkspacePrintResult, got {responses:?}");
        };
        assert_eq!(
            text, " 7",
            "the untouched workspace buffer must Print it to a real 7, not empty"
        );
    }

    /// "the JIT MUST be supported" (the S21 directive) — end-to-end through
    /// the GUI's own request path, not just the embedding layer's unit test.
    /// `Threshold(1)` compiles on the first call.
    #[test]
    fn workspace_print_it_works_with_the_jit_enabled() {
        let mut world = MockWorld::seed();
        let mut selection = BrowserSelection::default();
        let mut vm = test_vm_handle(macvm::runtime::JitMode::Threshold(1));
        let responses = handle(
            VmRequest::WorkspacePrintIt {
                code: "6 * 7.".to_string(),
            },
            &mut world,
            &mut selection,
            None,
            &mut vm,
        );
        let [VmResponse::WorkspacePrintResult { text }] = responses.as_slice() else {
            panic!("expected exactly one WorkspacePrintResult, got {responses:?}");
        };
        assert_eq!(text, " 42");
    }

    /// A malformed doit is a `GuestError::Compile`, surfaced as a transcript
    /// line — NOT a thread death. The same `vm` keeps serving afterward.
    #[test]
    fn doit_compile_error_is_reported_and_the_vm_survives() {
        let mut world = MockWorld::seed();
        let mut selection = BrowserSelection::default();
        let mut vm = test_vm_handle(macvm::runtime::JitMode::Off);
        let responses = handle(
            VmRequest::Doit {
                code: "3 + + 4.".to_string(),
            },
            &mut world,
            &mut selection,
            None,
            &mut vm,
        );
        assert!(transcript_containing(&responses, "error"), "{responses:?}");
        // Same handle still works — the compile error didn't kill anything.
        let after = handle(
            VmRequest::WorkspacePrintIt {
                code: "1 + 1.".to_string(),
            },
            &mut world,
            &mut selection,
            None,
            &mut vm,
        );
        assert!(
            matches!(after.as_slice(), [VmResponse::WorkspacePrintResult { text }] if text == " 2")
        );
    }

    /// Polls `drain_responses` (accumulating across calls) until `pred`
    /// holds or a generous deadline passes — booting the real world twice
    /// (original + respawned worker) genuinely takes a moment.
    fn poll_until(host: &VmHost, pred: impl Fn(&[VmResponse]) -> bool) -> Vec<VmResponse> {
        let start = Instant::now();
        let mut collected = Vec::new();
        while start.elapsed() < Duration::from_secs(20) {
            collected.extend(host.drain_responses());
            if pred(&collected) {
                return collected;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        collected
    }

    /// THE end-to-end proof of the whole S21 sprint: a doit that triggers a
    /// fatal guest condition (unhandled DNU -> `error:` -> `fatal_exit` ->
    /// `pthread_exit`) kills ONLY its worker thread; the supervisor detects
    /// the silence (no `WorkerIdle` before the timeout), respawns a fresh
    /// worker + VM, and that fresh worker serves the very next request. The
    /// test process surviving to assert all this IS the "GUI must not die"
    /// guarantee, made concrete against a real crash rather than a mock.
    ///
    /// Wake target is NIL — `CrossThreadObjcRef::notify` short-circuits, so
    /// this needs no Objective-C runtime / NSApplication (headless-safe).
    #[test]
    fn supervisor_respawns_the_worker_after_a_fatal_doit_and_serves_the_next_request() {
        // Isolate the image to a temp dir, pre-seeded once, so the two worker
        // generations boot from the DB (S22) without seeding or touching the
        // repo's own world/image.sqlite3. The worker derives its image path
        // as `<world_dir>/image.sqlite3`, so passing this temp dir as the
        // world_dir points it here.
        let tmp_dir = std::env::temp_dir().join(format!("macvm_respawn_{}", std::process::id()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        {
            let img = image_store::Image::open(&tmp_dir.join("image.sqlite3")).unwrap();
            image_store::import::import_world_dir(&img, &test_world_dir()).unwrap();
        }

        // A NIL SEL is fine: notify() never dereferences it (NIL target).
        let host = spawn_with_world_and_timeout(
            objc::NIL,
            objc::NIL,
            tmp_dir.clone(),
            Duration::from_millis(150),
        );

        // 1. Fatal doit. The worker emits its "Error: does not understand …"
        //    transcript (via ChannelTranscript) and only THEN pthread_exits —
        //    so it never sends WorkerIdle, leaving pending_since armed.
        host.submit(VmRequest::Doit {
            code: "3 thisSelectorDoesNotExistAnywhereInTheBaseWorld.".to_string(),
        });
        let seen = poll_until(&host, |rs| transcript_containing(rs, "does not understand"));
        assert!(
            transcript_containing(&seen, "does not understand"),
            "the worker must have emitted its DNU error before dying, got {seen:?}"
        );

        // 2. By now pending_since is far older than the 150ms timeout, so the
        //    next submit presumes the worker dead, respawns a fresh worker +
        //    VM, and routes this request to it. (If the worker were somehow
        //    still alive, respawn is still harmless — the old one was already
        //    committed to pthread_exit.)
        host.submit(VmRequest::Doit {
            code: "6 * 7.".to_string(),
        });
        let seen = poll_until(&host, |rs| transcript_containing(rs, "6 * 7"));
        assert!(
            transcript_containing(&seen, "fresh one has been started"),
            "the respawn notice must have been delivered, got {seen:?}"
        );
        assert!(
            transcript_containing(&seen, "6 * 7"),
            "the fresh worker must have served the follow-up doit, got {seen:?}"
        );

        std::fs::remove_dir_all(&tmp_dir).ok();
    }

    /// S22-E: editing a method through the browser's accept path makes the
    /// new behaviour LIVE in the running VM — a subsequent eval sees it, not
    /// just the mock/image. The class exists in both the mock and the VM (as
    /// it would after a DB boot); a class-side method keeps the test free of
    /// instantiation.
    #[test]
    fn browser_method_edit_is_live_in_the_running_vm() {
        let mut world = MockWorld::empty();
        world.add_class("LiveFoo", Some("Object"), "Test", "", "", "");
        world.add_method(
            "LiveFoo",
            Side::Class,
            "ping",
            "accessing",
            "LiveFoo class >> ping [ ^1 ]",
        );
        let mut vm = test_vm_handle(macvm::runtime::JitMode::Off);
        vm.exec("Object subclass: LiveFoo [ LiveFoo class >> ping [ ^1 ] ]")
            .unwrap();
        assert_eq!(
            vm.eval("LiveFoo ping.").unwrap(),
            "1",
            "sanity: starts at 1"
        );

        let mut selection = BrowserSelection {
            class: Some("LiveFoo".to_string()),
            category: Some("accessing".to_string()),
            method: Some("ping".to_string()),
            side: Side::Class,
            edit_target: SourceEditTarget::Method,
            ..Default::default()
        };
        let responses = handle(
            VmRequest::BrowserSaveSource {
                text: "LiveFoo class >> ping [ ^42 ]".to_string(),
                saved_package: String::new(),
                saved_class: "LiveFoo".to_string(),
                saved_side: "class".to_string(),
                saved_category: "accessing".to_string(),
                saved_method: "ping".to_string(),
                saved_target: "method".to_string(),
            },
            &mut world,
            &mut selection,
            None,
            &mut vm,
        );

        assert!(
            transcript_containing(&responses, "live in the VM"),
            "the save must report it went live, got {responses:?}"
        );
        // The real proof: the running VM now runs the edited method.
        assert_eq!(
            vm.eval("LiveFoo ping.").unwrap(),
            "42",
            "the browser edit must be live in the VM"
        );
    }

    /// S22-E part A: a fresh image path is auto-seeded from `world/*.mst`
    /// (zero setup); re-opening the same file does NOT re-seed.
    #[test]
    fn open_or_seed_image_seeds_then_reuses() {
        let world_dir = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../world"));
        let tmp =
            std::env::temp_dir().join(format!("macvm_seed_test_{}.sqlite3", std::process::id()));
        std::fs::remove_file(&tmp).ok();

        let img = open_or_seed_image(&world_dir, &tmp).expect("seed fresh");
        let n = img.all_classes().unwrap().len();
        drop(img);
        assert!(
            n >= 40,
            "a fresh path must be seeded with the whole world, got {n}"
        );

        // Re-open the same file: it already has classes, so it is NOT
        // re-seeded (same count, no duplicate-class error).
        let img2 = open_or_seed_image(&world_dir, &tmp).expect("reopen seeded");
        assert_eq!(img2.all_classes().unwrap().len(), n);
        drop(img2);
        std::fs::remove_file(&tmp).ok();
    }

    /// The "Restart VM Thread" menu action (`VmHost::restart`) swaps in a
    /// fresh worker + VM that serves the next request, and posts a
    /// "restarted" notice. A generous timeout so the timeout-respawn path
    /// can't fire — this tests the EXPLICIT restart, not death detection.
    #[test]
    fn restart_swaps_in_a_fresh_worker_that_still_serves() {
        let tmp_dir = std::env::temp_dir().join(format!("macvm_restart_{}", std::process::id()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        {
            let img = image_store::Image::open(&tmp_dir.join("image.sqlite3")).unwrap();
            image_store::import::import_world_dir(&img, &test_world_dir()).unwrap();
        }
        let host = spawn_with_world_and_timeout(
            objc::NIL,
            objc::NIL,
            tmp_dir.clone(),
            Duration::from_secs(30),
        );

        // The original worker serves a request.
        host.submit(VmRequest::Doit {
            code: "3 + 4.".to_string(),
        });
        let seen = poll_until(&host, |rs| transcript_containing(rs, "3 + 4"));
        assert!(
            transcript_containing(&seen, "3 + 4"),
            "original served: {seen:?}"
        );

        // Explicit restart → fresh worker + "restarted" notice serving next.
        host.restart();
        host.submit(VmRequest::Doit {
            code: "6 * 7.".to_string(),
        });
        let seen = poll_until(&host, |rs| transcript_containing(rs, "6 * 7"));
        assert!(
            transcript_containing(&seen, "VM thread restarted"),
            "the restart notice must appear: {seen:?}"
        );
        assert!(
            transcript_containing(&seen, "6 * 7"),
            "the fresh worker must serve the next request: {seen:?}"
        );

        std::fs::remove_dir_all(&tmp_dir).ok();
    }

    /// FFI (S20) and the DB-boot world loader (S22) had never been exercised
    /// TOGETHER before — `world/tests/30_ffi_alien_tests.mst` proves FFI
    /// works, but it isn't in `world.list`, so it's never part of the seeded
    /// image; the actual GUI boots via `load_world_from_image`
    /// (`boot_without_world` + two-phase replay), not the `.mst` file tree a
    /// plain `VmHandle::boot` would use. This defines the exact same
    /// `<primitive: FFI function: #getpid ret: #g args: #()>` class the S20
    /// test uses, live, against a DB-booted VM (the real Workspace/browser
    /// path — `live_compile`), and checks a real OS pid comes back.
    #[test]
    fn ffi_works_through_a_db_booted_vm() {
        let world_dir = test_world_dir();
        let tmp =
            std::env::temp_dir().join(format!("macvm_ffi_probe_{}.sqlite3", std::process::id()));
        std::fs::remove_file(&tmp).ok();
        let image = open_or_seed_image(&world_dir, &tmp).expect("seed");

        let mut vm = VmHandle::boot_without_world(macvm::runtime::VmOptions {
            heap_mib: 64,
            jit: macvm::runtime::JitMode::Off,
            ..Default::default()
        });
        crate::world_boot::load_world_from_image(&mut vm, &image, WORLD_DOITS).expect("db load");

        vm.exec(
            "Object subclass: FFIProbe [\n\
             FFIProbe class >> getpid [\n\
             <primitive: FFI function: #getpid ret: #g args: #()>\n\
             ]\n\
             ]",
        )
        .expect("FFI class must compile against a DB-booted VM");
        let pid: i64 = vm
            .eval("FFIProbe getpid.")
            .expect("getpid must run")
            .parse()
            .expect("must answer a plain integer");
        assert!(pid > 0, "expected a real positive pid, got {pid}");

        std::fs::remove_file(&tmp).ok();
    }

    /// `Time class>>millisecondClockValue` (`world/30_date_time.mst`, S22) —
    /// a real wall-clock FFI primitive (`clock_gettime`) combined with a
    /// `<classVars:>`-cached mmap scratch buffer and Alien struct reads, all
    /// through the actual DB-boot path the real GUI uses. Checks two calls a
    /// moment apart return distinct, increasing, real epoch-millisecond
    /// values — not just that it runs without erroring.
    #[test]
    fn time_millisecond_clock_value_works_through_a_db_booted_vm() {
        let world_dir = test_world_dir();
        let tmp =
            std::env::temp_dir().join(format!("macvm_clock_probe_{}.sqlite3", std::process::id()));
        std::fs::remove_file(&tmp).ok();
        let image = open_or_seed_image(&world_dir, &tmp).expect("seed");

        let mut vm = VmHandle::boot_without_world(macvm::runtime::VmOptions {
            heap_mib: 64,
            jit: macvm::runtime::JitMode::Off,
            ..Default::default()
        });
        crate::world_boot::load_world_from_image(&mut vm, &image, WORLD_DOITS).expect("db load");

        let parse_ms = |vm: &mut VmHandle| -> i64 {
            vm.eval("Time millisecondClockValue.")
                .expect("millisecondClockValue must run")
                .parse()
                .expect("must answer a plain integer")
        };
        let first = parse_ms(&mut vm);
        let second = parse_ms(&mut vm);

        // Sane epoch range (after 2023-01-01) — catches "answered garbage/
        // uninitialized memory" without pinning an exact value.
        assert!(
            first > 1_672_531_200_000,
            "not a plausible epoch ms: {first}"
        );
        assert!(
            second >= first,
            "the clock must not go backwards between two calls: {first} then {second}"
        );

        std::fs::remove_file(&tmp).ok();
    }
}
