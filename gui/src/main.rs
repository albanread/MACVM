//! MACVM GUI test shell — `../PLAN.md` phase G0 (faithful static shell) plus
//! a G1-shaped stub host (doit clicks echo to the in-page transcript,
//! satisfying that phase's "full JS↔Rust round trip" gate) so this is
//! usable as a real, clickable foundation now, not just a static render.
//!
//! No VM bridge yet (`GuiHost`/`VmHandle`, `../PLAN.md` G2, needs core eval —
//! S5/S6) — every doit/toolbar action just echoes into the Launcher-style
//! transcript pane via `evaluateJavaScript`, standing in for the real
//! `Transcript show:` round trip until G2.

mod browser_render;
mod canvas_render;
mod objc;
mod preprocess;
mod vm_host;
mod workspace_render;
mod world_boot;

use objc::{sel, Id, Sel, NIL};
use preprocess::Theme;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

/// `Id`/`Sel` are raw pointers (not `Send`/`Sync`) — sound to store in a
/// `static` here because AppKit only ever calls into this process on the
/// main thread, and nothing in this crate spawns another one.
struct MainThreadPtr(Id);
unsafe impl Send for MainThreadPtr {}
unsafe impl Sync for MainThreadPtr {}

static WEBVIEW: OnceLock<MainThreadPtr> = OnceLock::new();
static NAV: OnceLock<Mutex<NavState>> = OnceLock::new();

/// The GUI's handle onto the VM worker thread (`vm_host.rs`, SPEC §16.1 /
/// decision A18). Submitting a request from here just does a channel
/// send — the actual work, and the wake-up back to this thread, happen off
/// of it.
static VM: OnceLock<vm_host::VmHost> = OnceLock::new();

/// Current theme (`../PLAN.md` Theme menu), read on every `navigate_to` and
/// flipped by the native Theme menu's target/action handlers below. A plain
/// atomic INDEX into `Theme::ALL` rather than the `Mutex<NavState>` pattern
/// above: `Theme` is `Copy`, and AppKit only ever calls into this process on
/// the main thread (see `MainThreadPtr`'s doc comment), so there's no real
/// concurrency to protect against — `Ordering::Relaxed` is enough. Defaults
/// to `Theme::ALL`'s Hi-Def index rather than 0 (Classic) — the Theme menu
/// (and its checkmarks, computed from `current_theme()` at menu-build time)
/// still let you switch to any theme including Classic.
static THEME: AtomicU8 = AtomicU8::new(1); // Theme::ALL[1] == Theme::HiDef

fn current_theme() -> Theme {
    let idx = THEME.load(Ordering::Relaxed) as usize;
    Theme::ALL.get(idx).copied().unwrap_or(Theme::HiDef)
}

fn set_theme(theme: Theme) {
    if let Some(idx) = Theme::ALL.iter().position(|&t| t == theme) {
        THEME.store(idx as u8, Ordering::Relaxed);
    }
}

/// One `(Theme, menu-item-pointer)` pair per `Theme::ALL` entry, so their
/// checkmarks can be updated when the active theme changes
/// (`update_theme_menu_checkmarks`).
static THEME_MENU_ITEMS: OnceLock<Vec<(Theme, MainThreadPtr)>> = OnceLock::new();

/// Page zoom percentage (G5 `biggerText`/`smallerText`, `../PLAN.md` §4),
/// read on every `navigate_to` same as `THEME`. Stored directly as the
/// percentage (not steps) — `u32` would be the natural type, but a `Copy`
/// atomic needs a fixed-width integer type with atomic support, and `AtomicU32`
/// is fine here since the value only ever moves in small increments from a
/// single thread.
static FONT_SCALE_PERCENT: AtomicU32 = AtomicU32::new(100);
const FONT_SCALE_STEP: u32 = 10;
const FONT_SCALE_MIN: u32 = 60;
const FONT_SCALE_MAX: u32 = 200;

fn current_font_scale_percent() -> u32 {
    FONT_SCALE_PERCENT.load(Ordering::Relaxed)
}

/// The Workspace's last-known text, kept for the lifetime of the app run
/// only (not written to disk anywhere) — `None` means "untouched this
/// run, show `workspace_render::initial_text()`'s placeholder comment";
/// `Some(s)` (even `Some(String::new())`, if the user deliberately
/// cleared it) means "restore exactly this." Updated from `smtk.js`'s
/// `input` listener on every keystroke in the workspace's textarea (a
/// `postMessage` per keystroke is cheap enough at this scale — see
/// `on_script_message`'s `"workspaceTextChanged"` arm), since this bridge
/// has no way to *pull* the current DOM value back out of a WKWebView
/// (`objc.rs`'s module doc: `evaluateJavaScript:completionHandler:` is
/// always called with a nil handler, so there's no return-value path).
static WORKSPACE_TEXT: Mutex<Option<String>> = Mutex::new(None);

/// The transcript's full history for this app run (not written to disk) —
/// appended to by `append_transcript`, the single function that already
/// pushes every new line into the *live* page via `evaluateJavaScript`, so
/// the persisted copy can never drift from what a running page shows.
/// Threaded into every freshly-built page (`preprocess::render_generated_page`/
/// `load_and_preprocess`'s new `transcript` parameter) so navigating,
/// switching themes, or changing font scale — all of which throw away and
/// rebuild the page — no longer silently erases transcript history the way
/// `statusbar_html`'s old hardcoded "Ready." greeting did.
static TRANSCRIPT: Mutex<String> = Mutex::new(String::new());

/// Empty only means "nothing's been printed yet this run" — same "show a
/// friendly placeholder instead of literally nothing" idea as
/// `workspace_render::initial_text()`.
fn current_transcript() -> String {
    let buf = TRANSCRIPT.lock().unwrap();
    if buf.is_empty() {
        "MACVM Transcript\nReady.".to_string()
    } else {
        buf.clone()
    }
}

fn bump_font_scale(delta: i32) {
    let current = current_font_scale_percent() as i32;
    let next = (current + delta).clamp(FONT_SCALE_MIN as i32, FONT_SCALE_MAX as i32);
    FONT_SCALE_PERCENT.store(next as u32, Ordering::Relaxed);
}

struct NavState {
    history: Vec<PathBuf>,
    index: usize,
}

impl NavState {
    fn new(start: PathBuf) -> Self {
        Self {
            history: vec![start],
            index: 0,
        }
    }
    fn current(&self) -> PathBuf {
        self.history[self.index].clone()
    }
    fn go(&mut self, path: PathBuf) {
        self.history.truncate(self.index + 1);
        self.history.push(path);
        self.index = self.history.len() - 1;
    }
    fn back(&mut self) -> Option<PathBuf> {
        if self.index > 0 {
            self.index -= 1;
            Some(self.current())
        } else {
            None
        }
    }
    fn forward(&mut self) -> Option<PathBuf> {
        if self.index + 1 < self.history.len() {
            self.index += 1;
            Some(self.current())
        } else {
            None
        }
    }
}

fn gui_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A sentinel `NavState` entry for the class browser view — not a real
/// file (the browser is Rust-generated, `browser_render.rs`), but pushing
/// it onto `NavState` like any other page lets back/forward navigate to
/// and from it uniformly. `navigate_to` special-cases this exact path.
fn browser_view_marker() -> PathBuf {
    gui_root().join(".browser-view")
}

/// Same idea as `browser_view_marker`, for the Workspace tool
/// (`workspace_render.rs`).
fn workspace_view_marker() -> PathBuf {
    gui_root().join(".workspace-view")
}

/// Same idea as `browser_view_marker`, for the Canvas view
/// (`canvas_render.rs`, `../docs/CANVAS.md`).
fn canvas_view_marker() -> PathBuf {
    gui_root().join(".canvas-view")
}

fn start_page() -> PathBuf {
    gui_root().join("reference/pages/startPage.html")
}

/// MACVM's own authored help viewer (`reference/pages/macvm-help/`) — the
/// MACVM documentation and tour. This is what the toolbar's Documentation
/// button, the start page's "Browse Documentation" link, and the native
/// Help menu all open. The original Strongtalk documentation corpus
/// (`reference/pages/documentation/`, copied byte-identical from
/// `../../strongtalk-repo`) is kept as reference material and reached via a
/// link from this index, not as the primary documentation.
fn macvm_help_index() -> PathBuf {
    gui_root().join("reference/pages/macvm-help/index.html")
}

/// Load, D-G4-preprocess, and display `path` in the web view.
///
/// Writes the transformed HTML to a scratch file under `gui/.rendered/`
/// and loads *that* via `loadFileURL:allowingReadAccessToURL:<gui root>`,
/// rather than `loadHTMLString:baseURL:` — WKWebView does not grant local
/// `file://` subresource access outside `baseURL`'s own directory for the
/// latter (confirmed by actually running the shell: `strongtalk.css`,
/// `smtk.js`, and every toolbar icon — all outside the page's own
/// directory — silently failed to load, leaving only the browser's
/// default stylesheet, easy to mistake for "it worked" at a glance since
/// default HTML rendering is passably similar in the wrong way). Granting
/// read access to the whole `gui/` root covers the rendered file, the
/// original page's own directory (so *its* relative links/images keep
/// resolving), and `assets/`/`reference/icons-png/` in one grant.
/// `macvm-gui render <page.html> [--theme NAME] [--world DIR] [-o OUT]` — the
/// headless "eyes" command. Runs the full page pipeline (preprocess + real-VM
/// smappl resolution + icon theming) with NO Cocoa window, inlines the theme
/// CSS so the output is self-contained, and writes it (default:
/// `gui/.rendered/headless.html`). View it in a browser to see exactly what a
/// corpus page renders as — the offline substitute for driving the native
/// WKWebView (which this objc bridge can't snapshot: no block ABI for
/// `takeSnapshotWithConfiguration:`).
fn cmd_render(args: &[String]) {
    let mut page: Option<PathBuf> = None;
    let mut theme = preprocess::Theme::Classic;
    let mut world_dir = PathBuf::from("world");
    let mut out: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--theme" => {
                i += 1;
                theme = args
                    .get(i)
                    .and_then(|n| preprocess::Theme::from_cli_name(n))
                    .unwrap_or(preprocess::Theme::Classic);
            }
            "--world" => {
                i += 1;
                if let Some(d) = args.get(i) {
                    world_dir = PathBuf::from(d);
                }
            }
            "-o" | "--out" => {
                i += 1;
                out = args.get(i).map(PathBuf::from);
            }
            other if page.is_none() => page = Some(PathBuf::from(other)),
            _ => {}
        }
        i += 1;
    }
    let Some(page) = page else {
        eprintln!("usage: macvm-gui render <page.html> [--theme NAME] [--world DIR] [-o OUT]");
        std::process::exit(2);
    };

    let chromed = match preprocess::load_and_preprocess(&page, theme, 100, "Ready") {
        Ok(h) => h,
        Err(e) => {
            eprintln!("render: cannot read {}: {e}", page.display());
            std::process::exit(1);
        }
    };

    let mut vm = match macvm::embed::VmHandle::boot(macvm::runtime::VmOptions::default(), &world_dir)
    {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("render: VM boot failed: {e:?}");
            std::process::exit(1);
        }
    };
    // One-shot CLI on the main thread: a genuinely-fatal render should exit
    // the process cleanly rather than pthread_exit the main thread.
    macvm::embed::set_fatal_mode(macvm::embed::FatalMode::ExitProcess);

    let resolved =
        preprocess::resolve_smappl_spans(&chromed, theme, |code| vm.render_fragment(code).ok());

    // Fill ClassOutliner method-source blocks from an image_store DB beside the
    // world, if one exists (the source of method text — the VM keeps none).
    let image_path = world_dir.join("image.sqlite3");
    let resolved = if image_path.exists() {
        match image_store::Image::open(&image_path) {
            Ok(img) => preprocess::inject_method_sources(&resolved, |c, s, sel| {
                let side = if s == "class" {
                    image_store::Side::Class
                } else {
                    image_store::Side::Instance
                };
                img.method_source(c, side, sel).ok().flatten()
            }),
            Err(_) => resolved,
        }
    } else {
        resolved
    };

    // Inline the theme CSS and smtk.js so the file renders self-contained and
    // interactive (the chrome's own `file://` stylesheet/script links won't
    // resolve when served over http). smtk.js's `post()` no-ops without a
    // native host, so client-side behavior (outliner expand/collapse) still
    // works in a plain browser.
    let gui_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let css = std::fs::read_to_string(theme.stylesheet_path()).unwrap_or_default();
    let js = std::fs::read_to_string(gui_root.join("assets/smtk.js")).unwrap_or_default();
    let head = format!("<style>{css}</style><script>{js}</script>");
    let self_contained = if resolved.contains("</head>") {
        resolved.replacen("</head>", &format!("{head}</head>"), 1)
    } else {
        format!("{head}{resolved}")
    };

    let out = out.unwrap_or_else(|| {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push(".rendered");
        std::fs::create_dir_all(&p).ok();
        p.push("headless.html");
        p
    });
    if let Err(e) = std::fs::write(&out, self_contained) {
        eprintln!("render: cannot write {}: {e}", out.display());
        std::process::exit(1);
    }
    println!("{}", out.display());
}

/// `macvm-gui seed [--world DIR]` — import `world/*.mst` into
/// `<world_dir>/image.sqlite3` without launching the GUI. Headless complement
/// to `render`: the class browser and the ClassOutliner's method-source
/// blocks read their text from this DB (the running VM keeps no source).
fn cmd_seed(args: &[String]) {
    let mut world_dir = PathBuf::from("world");
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--world" {
            i += 1;
            if let Some(d) = args.get(i) {
                world_dir = PathBuf::from(d);
            }
        }
        i += 1;
    }
    let image_path = world_dir.join("image.sqlite3");
    let image = match image_store::Image::open(&image_path) {
        Ok(img) => img,
        Err(e) => {
            eprintln!("seed: cannot open {}: {e}", image_path.display());
            std::process::exit(1);
        }
    };
    match image_store::import::import_world_dir(&image, &world_dir) {
        Ok(stats) => println!("seeded {} ({stats:?})", image_path.display()),
        Err(e) => {
            eprintln!("seed: import failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Displays `path` (a view marker or a corpus file). Returns `false` if a file
/// load failed — the caller must NOT commit a failed target to history, or a
/// broken relative link's bad path becomes `current()` and the next relative
/// click resolves against it, compounding (the documentation-tour
/// `tour/tour/tour/…` bug). Marker views always succeed.
fn navigate_to(path: &Path) -> bool {
    if path == browser_view_marker() {
        // Back/forward/refresh landing back on the browser marker: NAV
        // already points here (unlike `open_class_browser`, which pushes
        // it), so just re-request current state — same "never show a page
        // before its data arrives" rule applies on the way back in, too.
        request_browser_open();
        return true;
    }
    if path == workspace_view_marker() {
        // Unlike the browser marker, this rebuilds and displays the page
        // directly — see `open_workspace`'s doc comment for why there's no
        // VM round trip (or content persistence) involved here.
        display_workspace();
        return true;
    }
    if path == canvas_view_marker() {
        // Same shape as the workspace marker: rebuild and display directly.
        // Whatever was drawn is lost on navigating away, same accepted
        // limitation `open_workspace`'s doc comment already notes for its
        // own content — see `open_canvas`'s doc comment.
        display_canvas();
        return true;
    }
    if path == class_outliner_view_marker() {
        // Rebuild the smappl class-outliner page; its `<smappl>` resolves
        // via the worker on load, same as any corpus page.
        display_class_outliner();
        return true;
    }
    if let Some(tool) = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix(".find-"))
    {
        display_find(tool);
        return true;
    }
    let html = match preprocess::load_and_preprocess(
        path,
        current_theme(),
        current_font_scale_percent(),
        &current_transcript(),
    ) {
        Ok(html) => html,
        Err(e) => {
            eprintln!("macvm-gui: failed to load {}: {e}", path.display());
            return false;
        }
    };
    display_html(&html);
    true
}

/// Render the class browser's initial state and display it — shared by
/// `open_class_browser` (first open) and `navigate_to`/`reload_current_page`
/// (back/forward/refresh landing back on the browser view). Synchronous,
/// local `MockWorld`/`BrowserSelection` render, not a round trip through
/// `vm_host` — see `open_class_browser`'s doc comment for why.
/// Opens the class browser view (the "hierarchy" toolbar button — real
/// Strongtalk's own "Full hierarchy" action, `../PLAN.md` §1) — and
/// likewise what `navigate_to` calls when back/forward/refresh lands back
/// on the browser marker. Deliberately does **not** render or load
/// anything itself: it only pushes the NAV marker and asks the worker
/// thread for the current state. The actual page only ever gets built and
/// loaded once, from real data, when the matching `VmResponse::BrowserOpened`
/// arrives (`vm_bridge_drain`) — see `browser_render::assemble_panes`'s doc
/// comment for the race this avoids (an earlier version rendered a
/// hardcoded mock page here immediately, then tried to asynchronously
/// patch in real data — which could lose the race against the page still
/// loading and silently no-op).
fn open_class_browser() {
    let marker = browser_view_marker();
    if let Some(nav) = NAV.get() {
        nav.lock().unwrap().go(marker);
    }
    request_browser_open();
}

fn request_browser_open() {
    if let Some(vm) = VM.get() {
        vm.submit(vm_host::VmRequest::BrowserOpen);
    }
}

/// Opens the Workspace tool (`workspace_render.rs`, the toolbar's
/// long-unwired "Workspace" button — `../PLAN.md`'s nine, `blankSheet`
/// icon). Unlike `open_class_browser`, this builds and displays the page
/// synchronously, with no VM round trip at all: the browser's initial
/// content depends on worker-thread state (`MockWorld`), but the
/// Workspace's content doesn't depend on anything the worker thread
/// knows — it's just a starting placeholder comment, so there's nothing
/// to race and nothing to wait for.
///
/// Known limitation, accepted rather than solved here: whatever's typed
/// into the workspace lives only in the page's own DOM, which a fresh
/// `loadFileURL:` throws away — navigating away and back (or
/// quitting) loses it, same as any unsaved scratch buffer. Worth
/// revisiting (e.g. persisting the latest text to a small in-memory
/// static, mirroring `FONT_SCALE_PERCENT`/`THEME`) if that turns out to
/// be annoying in practice; not built now since it's not needed for the
/// hooks themselves to be real.
fn open_workspace() {
    if let Some(nav) = NAV.get() {
        nav.lock().unwrap().go(workspace_view_marker());
    }
    display_workspace();
}

/// The "User hierarchy" toolbar button (`preprocess::TOOLBAR_BUTTONS`) — a
/// full-page class browser built on the smappl `ClassHierarchyOutliner`
/// (click a class name to drill into its editable method browser). Distinct
/// from the "Full hierarchy" button, which opens the older MockWorld browser.
fn class_outliner_view_marker() -> PathBuf {
    gui_root().join(".class-outliner-view")
}

fn open_class_outliner() {
    if let Some(nav) = NAV.get() {
        nav.lock().unwrap().go(class_outliner_view_marker());
    }
    display_class_outliner();
}

/// The find-tool pages (Implementors / Senders / Find definition) — a search
/// input over the reflection layer (docs/APPS.md §5.4). Distinct marker per
/// tool so back/forward/refresh rebuild the right one.
fn find_view_marker(tool: &str) -> PathBuf {
    gui_root().join(format!(".find-{tool}"))
}

fn open_find(tool: &str) {
    if let Some(nav) = NAV.get() {
        nav.lock().unwrap().go(find_view_marker(tool));
    }
    display_find(tool);
}

fn display_find(tool: &str) {
    let (title, placeholder) = match tool {
        "senders" => ("Senders", "selector, e.g. printOn:"),
        "definition" => ("Find Definition", "class name, e.g. Collection"),
        _ => ("Implementors", "selector, e.g. printOn:"),
    };
    // The results div carries a widget id + hierarchy root so clicking a
    // result (a .st-class-link) drills into that class via the same
    // smapplOpenClass path the outliner uses.
    let body = format!(
        "<h1>{title}</h1>\
         <p>Type a selector and press Enter.</p>\
         <input class=\"st-find-input\" type=\"text\" data-find-tool=\"{tool}\" \
          placeholder=\"{placeholder}\" autocomplete=\"off\" spellcheck=\"false\">\
         <div id=\"find-results\" data-widget-id=\"find\" data-hierarchy-root=\"Object\"></div>"
    );
    let html = preprocess::render_generated_page(
        title,
        &body,
        &gui_root(),
        current_theme(),
        current_font_scale_percent(),
        &current_transcript(),
    );
    display_html(&html);
}

fn display_class_outliner() {
    // A `<smappl>` tag: `render_generated_page` rewrites it to a placeholder
    // span (like any corpus page), and smtk.js resolves it via the worker on
    // load — no smappl evaluation happens here on the main thread.
    let body = "<h1>Class Browser</h1>\
        <p>Click a class name to open its methods; the &#9656; toggles subclasses.</p>\
        <smappl visual=\"ClassHierarchyOutliner imbeddedVisualForClass: Object\">";
    let html = preprocess::render_generated_page(
        "Class Browser",
        body,
        &gui_root(),
        current_theme(),
        current_font_scale_percent(),
        &current_transcript(),
    );
    display_html(&html);
}

fn display_workspace() {
    let text = WORKSPACE_TEXT
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| workspace_render::initial_text().to_string());
    let body = workspace_render::render_workspace(&text);
    let html = preprocess::render_generated_page(
        "Workspace",
        &body,
        &gui_root(),
        current_theme(),
        current_font_scale_percent(),
        &current_transcript(),
    );
    display_html(&html);
}

/// Opens the Canvas view (`canvas_render.rs`, `../docs/CANVAS.md`, the
/// toolbar's "abstract" icon). Displays synchronously at the default
/// size, exactly like `open_workspace`, then *also* submits a real
/// `CanvasCreate` request — "allocate a widget of a specific size" is a
/// genuine round trip through `vm_host`, not just a static initial
/// render, so opening the view exercises it for real every time, not
/// only when "Run Demo" is clicked.
///
/// Same accepted limitation as Workspace: whatever's drawn lives only in
/// the page's own `<canvas>` pixel buffer, which a fresh `loadFileURL:`
/// throws away on navigating elsewhere.
fn open_canvas() {
    if let Some(nav) = NAV.get() {
        nav.lock().unwrap().go(canvas_view_marker());
    }
    display_canvas();
    if let Some(vm) = VM.get() {
        vm.submit(vm_host::VmRequest::CanvasCreate {
            width: canvas_render::DEFAULT_WIDTH,
            height: canvas_render::DEFAULT_HEIGHT,
        });
    }
}

fn display_canvas() {
    let body =
        canvas_render::render_canvas(canvas_render::DEFAULT_WIDTH, canvas_render::DEFAULT_HEIGHT);
    let html = preprocess::render_generated_page(
        "Canvas",
        &body,
        &gui_root(),
        current_theme(),
        current_font_scale_percent(),
        &current_transcript(),
    );
    display_html(&html);
}

/// The "write to `.rendered/current.html`, then `loadFileURL:` it" half of
/// `navigate_to`, factored out so Rust-generated pages (the class browser
/// view, `open_class_browser`) can reuse the exact same load mechanism —
/// and its doc comment's reasoning — without going through a corpus file
/// on disk first.
fn display_html(html: &str) {
    let rendered_dir = gui_root().join(".rendered");
    if let Err(e) = std::fs::create_dir_all(&rendered_dir) {
        eprintln!(
            "macvm-gui: failed to create {}: {e}",
            rendered_dir.display()
        );
        return;
    }
    // A single reusable filename: navigation is synchronous from the main
    // thread and one-at-a-time, so there's no concurrent writer to race.
    let rendered_path = rendered_dir.join("current.html");
    if let Err(e) = std::fs::write(&rendered_path, html) {
        eprintln!(
            "macvm-gui: failed to write {}: {e}",
            rendered_path.display()
        );
        return;
    }

    let Some(webview) = WEBVIEW.get() else { return };
    let target_url = make_file_url(&rendered_path);
    let access_root_url = make_file_url(&gui_root());
    objc::send2_id(
        webview.0,
        sel("loadFileURL:allowingReadAccessToURL:"),
        target_url,
        access_root_url,
    );
}

fn make_file_url(dir: &Path) -> Id {
    let cls = objc::get_class("NSURL");
    let path_str = objc::nsstring(&dir.to_string_lossy());
    objc::send1_id(cls, sel("fileURLWithPath:"), path_str)
}

/// `[webview evaluateJavaScript:completionHandler:]` with a nil completion
/// handler — Cocoa accepts nil there when the caller doesn't need the
/// result (this shell never does), which sidesteps building an
/// Objective-C block literal entirely (see `objc.rs`'s module doc: this
/// bridge deliberately doesn't implement the block ABI).
fn eval_js(js: &str) {
    let Some(webview) = WEBVIEW.get() else { return };
    objc::send2_id(
        webview.0,
        sel("evaluateJavaScript:completionHandler:"),
        objc::nsstring(js),
        NIL,
    );
}

fn js_string_literal(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "")
    )
}

fn append_transcript(text: &str) {
    {
        let mut buf = TRANSCRIPT.lock().unwrap();
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(text);
    }
    eval_js(&format!(
        "window.macvmAppendTranscript({})",
        js_string_literal(text)
    ));
}

/// Cut/Copy/Paste/Select All from `smtk.js`'s custom context menu
/// (`../PLAN.md`'s native-context-menu takeover — see that file's own
/// comment on why it can't just be a `WKUIDelegate` override: this bridge
/// has no Objective-C block ABI, and macOS's context-menu-customization
/// delegate methods are all completion-handler/block-based). Routed
/// through `NSApp sendAction:to:from:` with a nil target so AppKit's
/// standard responder-chain dispatch finds whatever's actually
/// focused — the exact mechanism a *native* Edit-menu Cut/Copy/Paste/
/// Select All item already uses when clicked, so this reaches WKWebView's
/// own internal text handling exactly as reliably, rather than
/// reimplementing clipboard access in JavaScript (fragile/sandboxed in a
/// WKWebView, particularly for paste).
fn send_edit_action(action: &str) {
    let sel_name = match action {
        "cut" => "cut:",
        "copy" => "copy:",
        "paste" => "paste:",
        "selectAll" => "selectAll:",
        _ => return,
    };
    let app = objc::send0(objc::get_class("NSApplication"), sel("sharedApplication"));
    objc::send3_id(app, sel("sendAction:to:from:"), sel(sel_name), NIL, NIL);
}

fn dict_get_string(dict: Id, key: &str) -> String {
    if dict.is_null() {
        return String::new();
    }
    let value = objc::send1_id(dict, sel("objectForKey:"), objc::nsstring(key));
    objc::string_from_nsstring(value)
}

// ── WKScriptMessageHandler: userContentController:didReceiveScriptMessage: ─

extern "C" fn on_script_message(_this: Id, _cmd: Sel, _controller: Id, message: Id) {
    let body = objc::send0(message, sel("body"));
    let kind = dict_get_string(body, "kind");
    match kind.as_str() {
        "doit" => {
            let code = dict_get_string(body, "code");
            // SPEC §16.1: doits cross the GUI→VM channel to the worker
            // thread rather than running inline here. The worker (still a
            // stub — no real VM yet, ../PLAN.md G2) answers by echoing the
            // code back over the response channel; `vm_bridge_drain` picks
            // it up on this thread and appends it to the transcript. The
            // round trip is real even though the "VM" on the other end
            // isn't yet.
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::Doit { code });
            }
        }
        "toolbar" => {
            let button = dict_get_string(body, "button");
            match button.as_str() {
                "goBack" => {
                    if let Some(path) = NAV.get().and_then(|n| n.lock().unwrap().back()) {
                        navigate_to(&path);
                    }
                }
                "goForward" => {
                    if let Some(path) = NAV.get().and_then(|n| n.lock().unwrap().forward()) {
                        navigate_to(&path);
                    }
                }
                "home" => {
                    let home = start_page();
                    if let Some(nav) = NAV.get() {
                        nav.lock().unwrap().go(home.clone());
                    }
                    navigate_to(&home);
                }
                "documentation" => {
                    // The toolbar Documentation button opens MACVM's own help
                    // (which hosts the MACVM tour). The Strongtalk reference
                    // corpus is still reachable as a link from that index.
                    let doc_index = macvm_help_index();
                    if let Some(nav) = NAV.get() {
                        nav.lock().unwrap().go(doc_index.clone());
                    }
                    navigate_to(&doc_index);
                }
                "userHierarchy" => open_class_outliner(),
                "find" => open_find("definition"),
                "implementors" => open_find("implementors"),
                "senders" => open_find("senders"),
                "hierarchy" => open_class_browser(),
                "workspace" => open_workspace(),
                "canvas" => open_canvas(),
                "refresh" => reload_current_page(),
                "biggerText" => {
                    bump_font_scale(FONT_SCALE_STEP as i32);
                    reload_current_page();
                }
                "smallerText" => {
                    bump_font_scale(-(FONT_SCALE_STEP as i32));
                    reload_current_page();
                }
                other => append_transcript(&format!("(toolbar: {other} — not wired yet)")),
            }
        }
        "clearTranscript" => {
            // Empty the persisted history so the clear survives navigation (the
            // next page's `statusbar_html` renders from this buffer), then clear
            // the live pane to match — same native-drives-the-DOM shape as
            // `append_transcript`.
            TRANSCRIPT.lock().unwrap().clear();
            eval_js("window.macvmClearTranscript && window.macvmClearTranscript()");
        }
        "navigate" => {
            let href = dict_get_string(body, "href");
            let Some(nav) = NAV.get() else { return };
            let current_dir = {
                let n = nav.lock().unwrap();
                n.current()
                    .parent()
                    .unwrap_or_else(|| Path::new("/"))
                    .to_path_buf()
            };
            let target = normalize_path(&current_dir.join(&href));
            // Commit to history only if the page actually loads. A broken link
            // otherwise makes its non-existent path `current()`, so the next
            // relative click resolves against it and the path compounds
            // (documentation-tour `tour/tour/…`). On failure the displayed page
            // and `current()` are both unchanged.
            if navigate_to(&target) {
                nav.lock().unwrap().go(target);
            } else {
                eval_js(&format!(
                    "if(window.macvmSetStatus)window.macvmSetStatus({})",
                    js_string_literal(&format!("Not found: {href}"))
                ));
            }
        }
        // Class browser view (`browser_render.rs`, `../PLAN.md`'s "hierarchy"
        // toolbar action) — each of these just submits a request to the VM
        // worker thread; the resulting pane HTML comes back asynchronously
        // via `vm_bridge_drain`/`replace_pane`, same round trip as `doit`.
        "browserSelectPackage" => {
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::BrowserSelectPackage {
                    name: dict_get_string(body, "name"),
                });
            }
        }
        "browserSelectClass" => {
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::BrowserSelectClass {
                    name: dict_get_string(body, "name"),
                });
            }
        }
        "browserSelectSide" => {
            let side = if dict_get_string(body, "side") == "class" {
                vm_host::Side::Class
            } else {
                vm_host::Side::Instance
            };
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::BrowserSelectSide { side });
            }
        }
        "browserSelectCategory" => {
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::BrowserSelectCategory {
                    name: dict_get_string(body, "name"),
                });
            }
        }
        "browserSelectMethod" => {
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::BrowserSelectMethod {
                    selector: dict_get_string(body, "name"),
                });
            }
        }
        "browserSaveSource" => {
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::BrowserSaveSource {
                    text: dict_get_string(body, "text"),
                    saved_package: dict_get_string(body, "savedPackage"),
                    saved_class: dict_get_string(body, "savedClass"),
                    saved_side: dict_get_string(body, "savedSide"),
                    saved_category: dict_get_string(body, "savedCategory"),
                    saved_method: dict_get_string(body, "savedMethod"),
                    saved_target: dict_get_string(body, "savedTarget"),
                });
            }
        }
        "browserNewMethod" => {
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::BrowserNewMethod);
            }
        }
        "browserNewClass" => {
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::BrowserNewClass);
            }
        }
        "browserEditDefinition" => {
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::BrowserEditDefinition);
            }
        }
        "browserEditComment" => {
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::BrowserEditComment);
            }
        }
        "browserRemoveClass" => {
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::BrowserRemoveClass);
            }
        }
        "browserRemoveMethod" => {
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::BrowserRemoveMethod);
            }
        }
        "workspacePrintIt" => {
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::WorkspacePrintIt {
                    code: dict_get_string(body, "code"),
                });
            }
        }
        "workspaceTextChanged" => {
            *WORKSPACE_TEXT.lock().unwrap() = Some(dict_get_string(body, "text"));
        }
        "canvasRunDemo" => {
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::CanvasRunDemo);
            }
        }
        "canvasEval" => {
            // Generic: whatever Smalltalk the clicked control carried in its
            // `data-canvas-eval` attribute is evaluated and its command-batch
            // answer drawn. The GUI holds no per-drawing knowledge.
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::CanvasEval {
                    code: dict_get_string(body, "code"),
                });
            }
        }
        "canvasEvalPixels" => {
            // Generic pixel path: the clicked control's `data-canvas-eval`
            // answers a width*height*4 RGBA buffer, blitted via putImageData.
            // width/height arrive as strings (only dict_get_string exists).
            if let Some(vm) = VM.get() {
                let width = dict_get_string(body, "width").parse().unwrap_or(0);
                let height = dict_get_string(body, "height").parse().unwrap_or(0);
                vm.submit(vm_host::VmRequest::CanvasEvalPixels {
                    code: dict_get_string(body, "code"),
                    width,
                    height,
                });
            }
        }
        "canvasClear" => {
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::CanvasClear);
            }
        }
        "editAction" => {
            send_edit_action(&dict_get_string(body, "action"));
        }
        "smappl" => {
            // A `<smappl>` placeholder on the loaded page asks to be rendered
            // (smtk.js posts one per span on load). The worker evaluates the
            // `visual=` code and answers a fragment (D-G5, ../smappl.md).
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::SmapplRender {
                    id: dict_get_string(body, "id"),
                    code: dict_get_string(body, "code"),
                });
            }
        }
        "smapplAction" => {
            // A rendered smappl widget was clicked — fire its action closure.
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::SmapplAction {
                    action_id: dict_get_string(body, "actionId"),
                });
            }
        }
        "smapplAccept" => {
            // Cmd+S in a ClassOutliner source editor — version the edit into
            // the image and live-compile it.
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::SmapplAccept {
                    cls: dict_get_string(body, "cls"),
                    side: dict_get_string(body, "side"),
                    sel: dict_get_string(body, "sel"),
                    text: dict_get_string(body, "text"),
                    widget_id: dict_get_string(body, "widgetId"),
                });
            }
        }
        "smapplOpenClass" => {
            // Drill from a hierarchy outliner into a class's method browser.
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::SmapplOpenClass {
                    cls: dict_get_string(body, "cls"),
                    root: dict_get_string(body, "root"),
                    widget_id: dict_get_string(body, "widgetId"),
                });
            }
        }
        "smapplOpenHierarchy" => {
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::SmapplOpenHierarchy {
                    root: dict_get_string(body, "root"),
                    widget_id: dict_get_string(body, "widgetId"),
                });
            }
        }
        "find" => {
            if let Some(vm) = VM.get() {
                vm.submit(vm_host::VmRequest::Find {
                    tool: dict_get_string(body, "tool"),
                    query: dict_get_string(body, "query"),
                });
            }
        }
        _ => {}
    }
}

/// Resolve `..`/`.` components without touching the filesystem (the target
/// may not exist yet if `href` is wrong — better to report a clean load
/// error than to silently fail path resolution).
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Reload whatever page `NAV` currently points at — used after a theme
/// switch, so the change is visible immediately rather than on the next
/// navigation.
fn reload_current_page() {
    if let Some(nav) = NAV.get() {
        let current = nav.lock().unwrap().current();
        navigate_to(&current);
    }
}

const NS_CONTROL_STATE_VALUE_ON: i64 = 1;
const NS_CONTROL_STATE_VALUE_OFF: i64 = 0;

/// Reflect the active theme as a checkmark on every Theme-menu item.
fn update_theme_menu_checkmarks() {
    let Some(items) = THEME_MENU_ITEMS.get() else {
        return;
    };
    let current = current_theme();
    for (theme, item) in items {
        objc::send1_i64(
            item.0,
            sel("setState:"),
            if *theme == current {
                NS_CONTROL_STATE_VALUE_ON
            } else {
                NS_CONTROL_STATE_VALUE_OFF
            },
        );
    }
}

extern "C" fn theme_menu_select_classic(_this: Id, _cmd: Sel, _sender: Id) {
    set_theme(Theme::Classic);
    update_theme_menu_checkmarks();
    reload_current_page();
}

extern "C" fn theme_menu_select_hidef(_this: Id, _cmd: Sel, _sender: Id) {
    set_theme(Theme::HiDef);
    update_theme_menu_checkmarks();
    reload_current_page();
}

extern "C" fn theme_menu_select_dark(_this: Id, _cmd: Sel, _sender: Id) {
    set_theme(Theme::Dark);
    update_theme_menu_checkmarks();
    reload_current_page();
}

extern "C" fn theme_menu_select_crt_amber(_this: Id, _cmd: Sel, _sender: Id) {
    set_theme(Theme::CrtAmber);
    update_theme_menu_checkmarks();
    reload_current_page();
}

extern "C" fn theme_menu_select_crt_green(_this: Id, _cmd: Sel, _sender: Id) {
    set_theme(Theme::CrtGreen);
    update_theme_menu_checkmarks();
    reload_current_page();
}

extern "C" fn theme_menu_select_squeak(_this: Id, _cmd: Sel, _sender: Id) {
    set_theme(Theme::Squeak);
    update_theme_menu_checkmarks();
    reload_current_page();
}

extern "C" fn theme_menu_select_alto_mono(_this: Id, _cmd: Sel, _sender: Id) {
    set_theme(Theme::AltoMono);
    update_theme_menu_checkmarks();
    reload_current_page();
}

/// A small target object for the Theme menu's two actions. Not stored
/// anywhere long-term, same as `build_quit_on_last_window_delegate` and
/// `build_message_handler` below: this bridge never sends `release`, so an
/// object's `alloc` retain count of 1 never drops to zero and it simply
/// lives for the process's lifetime — the same reasoning that already
/// makes those two delegates work without an explicit `static` holding
/// them.
fn build_theme_delegate() -> Id {
    let cls = objc::allocate_class(objc::get_class("NSObject"), "MacvmThemeDelegate");
    objc::add_method(
        cls,
        sel("selectClassicTheme:"),
        theme_menu_select_classic as *const _,
        "v@:@",
    );
    objc::add_method(
        cls,
        sel("selectHiDefTheme:"),
        theme_menu_select_hidef as *const _,
        "v@:@",
    );
    objc::add_method(
        cls,
        sel("selectDarkTheme:"),
        theme_menu_select_dark as *const _,
        "v@:@",
    );
    objc::add_method(
        cls,
        sel("selectCrtAmberTheme:"),
        theme_menu_select_crt_amber as *const _,
        "v@:@",
    );
    objc::add_method(
        cls,
        sel("selectCrtGreenTheme:"),
        theme_menu_select_crt_green as *const _,
        "v@:@",
    );
    objc::add_method(
        cls,
        sel("selectSqueakTheme:"),
        theme_menu_select_squeak as *const _,
        "v@:@",
    );
    objc::add_method(
        cls,
        sel("selectAltoMonoTheme:"),
        theme_menu_select_alto_mono as *const _,
        "v@:@",
    );
    objc::register_class(cls);
    objc::alloc_init("MacvmThemeDelegate")
}

extern "C" fn open_macvm_help(_this: Id, _cmd: Sel, _sender: Id) {
    let help = macvm_help_index();
    if let Some(nav) = NAV.get() {
        nav.lock().unwrap().go(help.clone());
    }
    navigate_to(&help);
}

/// Target object for the Help menu's item — same not-stored-long-term
/// reasoning as `build_theme_delegate`.
fn build_help_delegate() -> Id {
    let cls = objc::allocate_class(objc::get_class("NSObject"), "MacvmHelpDelegate");
    objc::add_method(
        cls,
        sel("openMacvmHelp:"),
        open_macvm_help as *const _,
        "v@:@",
    );
    objc::register_class(cls);
    objc::alloc_init("MacvmHelpDelegate")
}

/// The VM menu's "Restart VM Thread" action — kills the current language
/// thread and boots a fresh VM (`vm_host::VmHost::restart`). Runs on the main
/// thread, which is independent of the (possibly wedged) worker thread, so
/// this stays clickable even when a runaway doit has hung the VM — the whole
/// reason they're on separate threads.
extern "C" fn restart_vm_thread(_this: Id, _cmd: Sel, _sender: Id) {
    if let Some(vm) = VM.get() {
        vm.restart();
    }
}

/// Target object for the VM menu's item — same not-stored-long-term
/// reasoning as `build_theme_delegate`.
fn build_vm_delegate() -> Id {
    let cls = objc::allocate_class(objc::get_class("NSObject"), "MacvmVmDelegate");
    objc::add_method(
        cls,
        sel("restartVmThread:"),
        restart_vm_thread as *const _,
        "v@:@",
    );
    objc::register_class(cls);
    objc::alloc_init("MacvmVmDelegate")
}

extern "C" fn app_should_terminate_after_last_window_closed(
    _this: Id,
    _cmd: Sel,
    _sender: Id,
) -> bool {
    true
}

fn build_quit_on_last_window_delegate() -> Id {
    let cls = objc::allocate_class(objc::get_class("NSObject"), "MacvmAppDelegate");
    objc::add_method(
        cls,
        sel("applicationShouldTerminateAfterLastWindowClosed:"),
        app_should_terminate_after_last_window_closed as *const _,
        "B@:@",
    );
    objc::register_class(cls);
    objc::alloc_init("MacvmAppDelegate")
}

fn build_message_handler() -> Id {
    let cls = objc::allocate_class(objc::get_class("NSObject"), "MacvmScriptMessageHandler");
    objc::add_method(
        cls,
        sel("userContentController:didReceiveScriptMessage:"),
        on_script_message as *const _,
        "v@:@@",
    );
    objc::register_class(cls);
    objc::alloc_init("MacvmScriptMessageHandler")
}

// ── VM bridge: the main-thread side of vm_host's worker thread ────────────

/// Runs on the main thread, invoked via `performSelector:withObject:
/// waitUntilDone:` from the VM worker thread (`vm_host::spawn`) whenever a
/// response is ready. Drains every response queued since the last call
/// (not just one) — `performSelector:` calls can coalesce in principle, and
/// this is cheap and correct either way.
extern "C" fn vm_bridge_drain(_this: Id, _cmd: Sel, _arg: Id) {
    let Some(vm) = VM.get() else { return };
    for response in vm.drain_responses() {
        match response {
            vm_host::VmResponse::Transcript(text) => append_transcript(&text),
            vm_host::VmResponse::BrowserPanes {
                packages_html,
                classes_html,
                categories_html,
                methods_html,
                source_html,
            } => {
                replace_pane("macvm-browser-packages", &packages_html);
                replace_pane("macvm-browser-classes", &classes_html);
                replace_pane("macvm-browser-categories", &categories_html);
                replace_pane("macvm-browser-methods", &methods_html);
                replace_pane("macvm-browser-source", &source_html);
            }
            vm_host::VmResponse::BrowserOpened {
                packages_html,
                classes_html,
                categories_html,
                methods_html,
                source_html,
            } => {
                // The only place a fresh page gets built from real data
                // rather than patched into an existing one — see
                // `browser_render::assemble_panes`'s doc comment for why.
                let body = browser_render::assemble_panes(
                    &packages_html,
                    &classes_html,
                    &categories_html,
                    &methods_html,
                    &source_html,
                );
                let html = preprocess::render_generated_page(
                    "Class Browser",
                    &body,
                    &gui_root(),
                    current_theme(),
                    current_font_scale_percent(),
                    &current_transcript(),
                );
                display_html(&html);
            }
            vm_host::VmResponse::WorkspacePrintResult { text } => {
                eval_js(&format!(
                    "window.macvmInsertPrintResult({})",
                    js_string_literal(&text)
                ));
            }
            vm_host::VmResponse::CanvasCreated { id, width, height } => {
                // The response is the authority on size, not the page's
                // initial static guess (`canvas_render::render_canvas`) —
                // keeps the `<canvas>` pixel buffer in sync if a future
                // `Canvas extent:` ever requests a size other than the
                // default this view opens with.
                eval_js(&format!(
                    "window.macvmCanvasCreated({id}, {width}, {height})"
                ));
            }
            vm_host::VmResponse::CanvasDraw { id, commands_json } => {
                eval_js(&format!(
                    "window.macvmCanvasDraw({id}, {})",
                    js_string_literal(&commands_json)
                ));
            }
            vm_host::VmResponse::CanvasPixels {
                id,
                width,
                height,
                base64,
            } => {
                // Blit the RGBA buffer in one putImageData (docs/CANVAS.md pixel
                // path). smtk.js decodes the base64 into an ImageData.
                eval_js(&format!(
                    "window.macvmCanvasPutPixels({id}, {width}, {height}, {})",
                    js_string_literal(&base64)
                ));
            }
            vm_host::VmResponse::CanvasCleared { .. } => {
                // No DOM action of its own — the paired `CanvasDraw`
                // response (a `clearRect` batch, see `vm_host::handle`'s
                // `CanvasClear` arm) already does the actual pixel clear.
                // This variant exists for a future GUI-side content
                // cache/replay-on-reopen (`../docs/CANVAS.md` §7) to
                // invalidate against, not for anything the DOM needs today.
            }
            vm_host::VmResponse::FindResults { html } => {
                // Fill the find page's results container (keeping its
                // data-widget-id so a result click can drill via smapplOpenClass).
                eval_js(&format!(
                    "window.macvmSetFindResults({})",
                    js_string_literal(&html)
                ));
            }
            vm_host::VmResponse::SmapplFragment { id, html } => {
                // Swap the placeholder span with `data-widget-id == id` for
                // the rendered widget (D-G5, ../smappl.md). smtk.js locates it
                // by the widget id rather than a DOM `id`, so this can't
                // collide with the page's own element ids. Icon buttons carry
                // a `data-icon` logical name resolved to the active theme's
                // asset here (theme is a main-thread concept; the worker that
                // built the fragment doesn't know it — ../smappl.md §5).
                let html = preprocess::rewrite_smappl_icons(&html, current_theme());
                eval_js(&format!(
                    "window.macvmRenderSmappl({}, {})",
                    js_string_literal(&id),
                    js_string_literal(&html)
                ));
            }
            vm_host::VmResponse::SmapplOverlay { html } => {
                // A live widget action popped a modal dialog (Visual>>promptOk:,
                // the "Press Me!" demo). Float it over the current page; smtk.js
                // owns the overlay lifecycle (backdrop click / OK / Esc close).
                eval_js(&format!(
                    "if(window.macvmShowOverlay)window.macvmShowOverlay({})",
                    js_string_literal(&html)
                ));
            }
            vm_host::VmResponse::WorkerIdle => {
                // Never actually delivered here — `VmHost::drain_responses`
                // consumes the worker-liveness marker internally and never
                // returns it (see that variant's doc). This arm exists only
                // to keep the match exhaustive.
            }
        }
    }
}

/// Replace a browser pane's entire element (its `outerHTML`, not just its
/// contents) with freshly-rendered markup carrying the same `dom_id` —
/// `browser_render.rs`'s pane functions always return a complete,
/// self-contained `<div id="...">`, so this works uniformly for every pane
/// on every selection change without the renderer needing two different
/// shapes (wrapped vs. bare) for initial render vs. later patching.
fn replace_pane(dom_id: &str, html: &str) {
    // The trailing `macvmHighlightCodeEditors()` call re-syncs the source
    // pane's highlighted `<pre>` after a fresh `<textarea>` value lands in
    // it (see `browser_render::render_source_pane`) — harmless/no-op for
    // every other pane, which has no `.st-code-editor` to find.
    eval_js(&format!(
        "{{ const el = document.getElementById({}); if (el) el.outerHTML = {}; \
         if (window.macvmHighlightCodeEditors) window.macvmHighlightCodeEditors(); }}",
        js_string_literal(dom_id),
        js_string_literal(html),
    ));
}

fn build_vm_bridge() -> Id {
    let cls = objc::allocate_class(objc::get_class("NSObject"), "MacvmVmBridge");
    objc::add_method(
        cls,
        sel("drainVmResponses:"),
        vm_bridge_drain as *const _,
        "v@:@",
    );
    objc::register_class(cls);
    objc::alloc_init("MacvmVmBridge")
}

// ── Menu bar: File / Edit / Help (../PLAN.md G0) ───────────────────────────

fn menu_item(title: &str, action: Option<&str>, key_equivalent: &str) -> Id {
    let item = objc::send0(objc::get_class("NSMenuItem"), sel("alloc"));
    objc::send3_id(
        item,
        sel("initWithTitle:action:keyEquivalent:"),
        objc::nsstring(title),
        action.map(objc::sel).unwrap_or(NIL),
        objc::nsstring(key_equivalent),
    )
}

fn separator_item() -> Id {
    objc::send0(objc::get_class("NSMenuItem"), sel("separatorItem"))
}

fn submenu(title: &str, items: &[Id]) -> Id {
    let menu_item = objc::send0(objc::get_class("NSMenuItem"), sel("alloc"));
    let menu_item = objc::send3_id(
        menu_item,
        sel("initWithTitle:action:keyEquivalent:"),
        objc::nsstring(title),
        NIL,
        objc::nsstring(""),
    );
    let menu = objc::alloc_init("NSMenu");
    for &item in items {
        objc::send1_id(menu, sel("addItem:"), item);
    }
    objc::send1_id(menu_item, sel("setSubmenu:"), menu);
    menu_item
}

/// `NSApplicationActivationPolicyRegular` — a normal foreground app (Dock
/// icon, menu bar, window activation). Without this, a bare executable
/// (no `.app` bundle / `Info.plist`) defaults to behaving like a
/// background agent: `run()` still returns control correctly at
/// termination, but no window ever becomes visible or key. This is the
/// exact detail MacModula2's `Cocoa.mod` `InitApp` calls out
/// (`library/macrtmod/Cocoa.mod:42-43`) — missing it here was a real bug,
/// caught by actually running the shell rather than just reading the code.
const NS_APPLICATION_ACTIVATION_POLICY_REGULAR: i64 = 0;

fn build_menu_bar() {
    let app = objc::send0(objc::get_class("NSApplication"), sel("sharedApplication"));
    objc::send1_i64(
        app,
        sel("setActivationPolicy:"),
        NS_APPLICATION_ACTIVATION_POLICY_REGULAR,
    );

    let app_menu = submenu("MACVM", &[menu_item("Quit MACVM", Some("terminate:"), "q")]);
    let file_menu = submenu(
        "File",
        &[menu_item("Close Window", Some("performClose:"), "w")],
    );
    let edit_menu = submenu(
        "Edit",
        &[
            menu_item("Cut", Some("cut:"), "x"),
            menu_item("Copy", Some("copy:"), "c"),
            menu_item("Paste", Some("paste:"), "v"),
            separator_item(),
            menu_item("Select All", Some("selectAll:"), "a"),
        ],
    );
    // The VM menu: kill + restart the language thread. Cmd+R is free (no
    // other menu binds it) and this action is a recovery hatch for a wedged
    // VM, so a shortcut earns its keep.
    let vm_delegate = build_vm_delegate();
    let restart_item = menu_item("Restart VM Thread", Some("restartVmThread:"), "r");
    objc::send1_id(restart_item, sel("setTarget:"), vm_delegate);
    let vm_menu = submenu("VM", &[restart_item]);

    let theme_menu = build_theme_menu();
    let help_delegate = build_help_delegate();
    let help_item = menu_item("MACVM GUI Shell Help", Some("openMacvmHelp:"), "");
    objc::send1_id(help_item, sel("setTarget:"), help_delegate);
    let help_menu = submenu("Help", &[help_item]);

    let main_menu = objc::alloc_init("NSMenu");
    for &item in &[
        app_menu, file_menu, edit_menu, vm_menu, theme_menu, help_menu,
    ] {
        objc::send1_id(main_menu, sel("addItem:"), item);
    }
    objc::send1_id(app, sel("setMainMenu:"), main_menu);
}

/// The Theme menu (`../PLAN.md` Theme menu): `preprocess::Theme::ALL`,
/// each paired with its own native action selector below. Every item's
/// target is explicitly set to a small delegate object rather than left
/// `nil`, because `nil` dispatches through the responder chain looking for
/// the selector, and nothing else in this app implements any of these
/// `selectXTheme:` actions.
fn build_theme_menu() -> Id {
    let delegate = build_theme_delegate();

    let actions: [(Theme, &str); 7] = [
        (Theme::Classic, "selectClassicTheme:"),
        (Theme::HiDef, "selectHiDefTheme:"),
        (Theme::Dark, "selectDarkTheme:"),
        (Theme::CrtAmber, "selectCrtAmberTheme:"),
        (Theme::CrtGreen, "selectCrtGreenTheme:"),
        (Theme::Squeak, "selectSqueakTheme:"),
        (Theme::AltoMono, "selectAltoMonoTheme:"),
    ];

    let mut checkmark_entries = Vec::with_capacity(actions.len());
    let mut menu_items = Vec::with_capacity(actions.len());
    for (theme, action) in actions {
        let item = menu_item(theme.menu_label(), Some(action), "");
        objc::send1_id(item, sel("setTarget:"), delegate);
        checkmark_entries.push((theme, MainThreadPtr(item)));
        menu_items.push(item);
    }

    THEME_MENU_ITEMS
        .set(checkmark_entries)
        .ok()
        .expect("build_theme_menu called twice");
    update_theme_menu_checkmarks();

    submenu("Theme", &menu_items)
}

// ── Window + WKWebView ─────────────────────────────────────────────────────

const STYLE_TITLED_CLOSABLE_MINIATURIZABLE_RESIZABLE: u64 = 15;
const BACKING_BUFFERED: u64 = 2;
const AUTORESIZING_WIDTH_HEIGHT_SIZABLE: u64 = 18; // NSViewWidthSizable(2) | NSViewHeightSizable(16)

fn build_window_and_webview() {
    let window = objc::send0(objc::get_class("NSWindow"), sel("alloc"));
    let window = objc::send_window_init(
        window,
        sel("initWithContentRect:styleMask:backing:defer:"),
        0.0,
        0.0,
        980.0,
        760.0,
        STYLE_TITLED_CLOSABLE_MINIATURIZABLE_RESIZABLE,
        BACKING_BUFFERED,
        false,
    );
    objc::send1_id(window, sel("setTitle:"), objc::nsstring("MACVM"));
    objc::send0(window, sel("center"));

    let config = objc::alloc_init("WKWebViewConfiguration");
    let user_content_controller = objc::send0(config, sel("userContentController"));
    objc::send2_id(
        user_content_controller,
        sel("addScriptMessageHandler:name:"),
        build_message_handler(),
        objc::nsstring("macvm"),
    );

    let webview = objc::send0(objc::get_class("WKWebView"), sel("alloc"));
    let webview = objc::send_frame_config_init(
        webview,
        sel("initWithFrame:configuration:"),
        0.0,
        0.0,
        980.0,
        760.0,
        config,
    );
    objc::send1_i64(
        webview,
        sel("setAutoresizingMask:"),
        AUTORESIZING_WIDTH_HEIGHT_SIZABLE as i64,
    );
    objc::send1_id(window, sel("setContentView:"), webview);

    WEBVIEW
        .set(MainThreadPtr(webview))
        .ok()
        .expect("build_window_and_webview called twice");

    objc::send1_id(window, sel("makeKeyAndOrderFront:"), NIL);
}

fn main() {
    // Headless "eyes" command — render a page (with all smappls resolved by a
    // real VM) to self-contained HTML, no Cocoa window. Guarded before
    // `objc::bootstrap()` so it runs anywhere, not just a windowing session.
    let cli: Vec<String> = std::env::args().skip(1).collect();
    match cli.first().map(String::as_str) {
        Some("render") => return cmd_render(&cli[1..]),
        Some("seed") => return cmd_seed(&cli[1..]),
        _ => {}
    }

    objc::bootstrap();

    let start = start_page();
    if !start.exists() {
        eprintln!(
            "macvm-gui: {} not found — is CARGO_MANIFEST_DIR wired up correctly?",
            start.display()
        );
        std::process::exit(1);
    }
    NAV.set(Mutex::new(NavState::new(start.clone()))).ok();

    let vm_bridge = build_vm_bridge();
    VM.set(vm_host::spawn(vm_bridge, sel("drainVmResponses:")))
        .ok()
        .expect("VM.set called twice");

    build_menu_bar();
    build_window_and_webview();
    navigate_to(&start);

    let app = objc::send0(objc::get_class("NSApplication"), sel("sharedApplication"));
    // Quit when the (only) window closes, so a `cargo run` test session
    // exits cleanly instead of lingering window-less — same pattern as
    // MacModula2's demo lifecycle (src/newm2-runtime/src/objc.rs).
    let delegate = build_quit_on_last_window_delegate();
    objc::send1_id(app, sel("setDelegate:"), delegate);
    objc::send1_bool(app, sel("activateIgnoringOtherApps:"), true);
    objc::send0(app, sel("run"));
}
