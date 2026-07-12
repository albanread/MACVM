// smtk.js — the Strongtalk-page runtime, per ../PLAN.md D-G3/D-G4.
//
// Wires the two Strongtalk HTML extensions (once the Rust-side D-G4
// translator has normalized them — see ../src/preprocess.rs) plus ordinary
// internal navigation and the in-page toolbar, all to
// `window.webkit.messageHandlers.macvm.postMessage(...)`, JSON per D-G3:
//   {kind:"doit", code}
//   {kind:"navigate", href}
//   {kind:"toolbar", button}
// The Rust host (../src/main.rs) answers by calling back into this file's
// macvmAppendTranscript/macvmSetStatus functions via evaluateJavaScript.

(function () {
  "use strict";

  function post(message) {
    if (window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.macvm) {
      window.webkit.messageHandlers.macvm.postMessage(message);
    } else {
      // No native host attached (e.g. previewing this page in a plain
      // browser) — fail quietly rather than throwing on every click.
      console.log("[smtk] (no macvm host)", message);
    }
  }

  // A same-directory-or-below relative link to another local page — the
  // ones smtk.js should intercept and hand to the Rust host instead of
  // letting WKWebView navigate directly (which would bypass the D-G4
  // translator on the next page). External links, mailto:, and in-page
  // "#" anchors are left alone.
  function isInternalPageLink(href) {
    if (!href) return false;
    if (/^(https?:|mailto:|javascript:|#)/i.test(href)) return false;
    return /\.html?($|[?#])/i.test(href);
  }

  // Class browser create/remove actions (browser_render.rs's
  // `data-browser-action` buttons) that just post one fixed message kind
  // with no other client-side logic — the remove/cancel-remove actions
  // below aren't in this table because they toggle an inline confirm strip
  // instead of posting anything (this WKWebView shell has no
  // `WKUIDelegate` installed, so `window.confirm()` wouldn't show
  // anything — see `../src/objc.rs`).
  const SIMPLE_BROWSER_ACTIONS = {
    "new-class": "browserNewClass",
    "new-method": "browserNewMethod",
    "edit-comment": "browserEditComment",
    "edit-definition": "browserEditDefinition",
    "confirm-remove-class": "browserRemoveClass",
    "confirm-remove-method": "browserRemoveMethod",
  };

  document.addEventListener(
    "click",
    function (ev) {
      const doit = ev.target.closest(".doit");
      if (doit) {
        ev.preventDefault();
        post({ kind: "doit", code: doit.getAttribute("data-code") || "" });
        return;
      }

      // Drill down: a class NAME in a hierarchy outliner opens that class's
      // method browser (ClassOutliner, with editors); the back link re-opens
      // the hierarchy. Checked BEFORE the header toggle below so clicking the
      // name drills rather than expanding subclasses. Both replace the same
      // widget (by data-widget-id).
      const openClass = ev.target.closest(".st-class-link[data-open-class]");
      if (openClass) {
        ev.preventDefault();
        const host = openClass.closest("[data-widget-id]");
        const root = openClass.closest("[data-hierarchy-root]");
        post({
          kind: "smapplOpenClass",
          cls: openClass.getAttribute("data-open-class") || "",
          widgetId: host ? host.getAttribute("data-widget-id") || "" : "",
          root: root ? root.getAttribute("data-hierarchy-root") || "" : "",
        });
        return;
      }
      const backLink = ev.target.closest(".st-class-link[data-open-hierarchy]");
      if (backLink) {
        ev.preventDefault();
        const host = backLink.closest("[data-widget-id]");
        post({
          kind: "smapplOpenHierarchy",
          root: backLink.getAttribute("data-open-hierarchy") || "",
          widgetId: host ? host.getAttribute("data-widget-id") || "" : "",
        });
        return;
      }

      // Outliner expand/collapse (world/34_tools.mst). The WHOLE header row is
      // the toggle target — clicking anywhere on "▸ instance side (3)" or a
      // selector/class row toggles it, not just the tiny glyph. The subtree is
      // shown/hidden entirely client-side (no VM round trip). A leaf header
      // (no ▾/▸ glyph) does nothing; the editor textarea lives in .st-children,
      // a sibling of the header, so editing never triggers a toggle.
      const header = ev.target.closest(".st-header");
      if (header) {
        const tw = header.querySelector(":scope > .st-tw[data-tw]");
        const node = header.closest(".st-node");
        const children = node && node.querySelector(":scope > .st-children");
        if (tw && children) {
          ev.preventDefault();
          const collapsed = children.style.display === "none";
          children.style.display = collapsed ? "" : "none";
          tw.innerHTML = collapsed ? "▾ " : "▸ "; // ▾ open / ▸ closed
          return;
        }
      }

      // Modal dialog overlay (Visual>>promptOk:, the differences2.html
      // "Press Me!" demo): the OK button or a click on the backdrop dismisses
      // it. Handled BEFORE the widget-action branch because the OK button is a
      // .smappl-button but deliberately carries no data-widget-action (the
      // corpus continuation is always []), so it must close, not post.
      const modalOk = ev.target.closest(".st-modal-ok");
      const onBackdrop =
        ev.target.classList && ev.target.classList.contains("st-modal");
      if (modalOk || onBackdrop) {
        ev.preventDefault();
        const overlay = ev.target.closest(".st-modal");
        if (overlay) overlay.remove();
        return;
      }

      // A rendered smappl widget (Button etc., ../smappl.md §3) — the
      // fragment carries the opaque action id the VM worker stored in
      // SmapplRegistry; clicking posts it back so `SmapplRegistry fire:`
      // runs the widget's action closure (../PLAN.md G1: route by id
      // through the worker-owned handler table).
      const widgetAction = ev.target.closest("[data-widget-action]");
      if (widgetAction) {
        ev.preventDefault();
        post({ kind: "smapplAction", actionId: widgetAction.getAttribute("data-widget-action") || "" });
        return;
      }

      const toolbtn = ev.target.closest(".st-toolbtn");
      if (toolbtn) {
        ev.preventDefault();
        post({ kind: "toolbar", button: toolbtn.getAttribute("data-action") || "" });
        return;
      }

      // Class browser view (browser_render.rs) — each pane item posts its
      // own selection kind; the VM worker thread re-renders the affected
      // panes and the Rust host patches them back in (main.rs::replace_pane).
      const browserPackage = ev.target.closest("[data-browser-package]");
      if (browserPackage) {
        ev.preventDefault();
        post({ kind: "browserSelectPackage", name: browserPackage.getAttribute("data-browser-package") || "" });
        return;
      }
      const browserClass = ev.target.closest("[data-browser-class]");
      if (browserClass) {
        ev.preventDefault();
        post({ kind: "browserSelectClass", name: browserClass.getAttribute("data-browser-class") || "" });
        return;
      }
      const browserSide = ev.target.closest("[data-browser-side]");
      if (browserSide) {
        ev.preventDefault();
        post({ kind: "browserSelectSide", side: browserSide.getAttribute("data-browser-side") || "" });
        return;
      }
      const browserCategory = ev.target.closest("[data-browser-category]");
      if (browserCategory) {
        ev.preventDefault();
        post({ kind: "browserSelectCategory", name: browserCategory.getAttribute("data-browser-category") || "" });
        return;
      }
      const browserMethod = ev.target.closest("[data-browser-method]");
      if (browserMethod) {
        ev.preventDefault();
        post({ kind: "browserSelectMethod", name: browserMethod.getAttribute("data-browser-method") || "" });
        return;
      }

      const browserAction = ev.target.closest("[data-browser-action]");
      if (browserAction) {
        const action = browserAction.getAttribute("data-browser-action") || "";
        const kind = SIMPLE_BROWSER_ACTIONS[action];
        if (kind) {
          ev.preventDefault();
          post({ kind: kind });
          return;
        }
        // "Remove Class"/"Remove Method": reveal the inline confirm strip
        // rather than removing anything yet — the actual request only
        // goes out from "Yes, remove" (SIMPLE_BROWSER_ACTIONS above).
        if (action === "remove-class" || action === "remove-method") {
          ev.preventDefault();
          browserAction.hidden = true;
          const strip = browserAction.parentElement && browserAction.parentElement.querySelector(".st-confirm-strip");
          if (strip) strip.hidden = false;
          return;
        }
        if (action === "cancel-remove") {
          ev.preventDefault();
          const strip = browserAction.closest(".st-confirm-strip");
          const row = strip && strip.parentElement;
          const button = row && row.querySelector('[data-browser-action^="remove-"]');
          if (strip) strip.hidden = true;
          if (button) button.hidden = false;
          return;
        }
      }

      const link = ev.target.closest("a[href]");
      if (link && isInternalPageLink(link.getAttribute("href"))) {
        ev.preventDefault();
        post({ kind: "navigate", href: link.getAttribute("href") });
      }
    },
    true
  );

  // Status bar: show the hovered link's target, "Ready" otherwise —
  // ../PLAN.md G0's "status bar live" gate.
  document.addEventListener(
    "mouseover",
    function (ev) {
      const link = ev.target.closest("a[href], .doit, .st-toolbtn");
      if (!link) return;
      const label =
        link.getAttribute("href") ||
        link.getAttribute("data-code") ||
        link.getAttribute("title") ||
        "";
      window.macvmSetStatus(label);
    },
    true
  );
  document.addEventListener(
    "mouseout",
    function (ev) {
      const link = ev.target.closest("a[href], .doit, .st-toolbtn");
      if (link) window.macvmSetStatus("Ready");
    },
    true
  );

  // ── VM → page entry points (called via evaluateJavaScript) ────────────

  window.macvmSetStatus = function (text) {
    const el = document.getElementById("macvm-status");
    if (el) el.textContent = text;
  };

  window.macvmAppendTranscript = function (text) {
    const el = document.getElementById("macvm-transcript");
    if (el) el.textContent += "\n" + text;
  };

  // ── Class browser source editor (browser_render.rs) ────────────────────
  //
  // The bottom pane is a transparent, editable <textarea class="st-code-input">
  // stacked exactly on top of a <pre class="st-code-highlight"> — the
  // textarea is the real input surface (native cursor/selection/undo/IME),
  // the <pre> underneath is what's actually visible. This is the one and
  // only tokenizer for that highlighting — the Rust side deliberately
  // leaves the <pre> empty and lets this file fill it in, rather than
  // duplicating the same logic in two languages that could drift apart.
  // Not a real Smalltalk parser: just enough regex-based token
  // recognition (comments, strings, symbols, keyword-message parts,
  // self/super/true/false/nil, numbers) to make source readable, matching
  // the "G0 placeholder, not the real thing" spirit of this whole mock —
  // upgrade to real parser-driven highlighting once there's a real one to
  // ask (../smappl.md's `docs/APPS.md` R2 note).

  function escapeHtml(s) {
    return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  }

  const SMALLTALK_TOKEN_RE =
    /("[^"]*")|('[^']*')|(#[A-Za-z_][A-Za-z0-9_]*)|(\b[a-zA-Z_][A-Za-z0-9_]*:)|(\b(?:self|super|true|false|nil|thisContext)\b)|(\b\d+(?:\.\d+)?\b)/g;

  function highlightSmalltalk(source) {
    let out = "";
    let last = 0;
    let m;
    SMALLTALK_TOKEN_RE.lastIndex = 0;
    while ((m = SMALLTALK_TOKEN_RE.exec(source)) !== null) {
      out += escapeHtml(source.slice(last, m.index));
      let cls;
      if (m[1]) cls = "st-tok-comment";
      else if (m[2]) cls = "st-tok-string";
      else if (m[3]) cls = "st-tok-symbol";
      else if (m[4]) cls = "st-tok-keyword";
      else if (m[5]) cls = "st-tok-pseudo";
      else cls = "st-tok-number";
      out += '<span class="' + cls + '">' + escapeHtml(m[0]) + "</span>";
      last = SMALLTALK_TOKEN_RE.lastIndex;
    }
    out += escapeHtml(source.slice(last));
    return out;
  }

  function highlightEditor(editor) {
    const input = editor.querySelector(".st-code-input");
    const pre = editor.querySelector(".st-code-highlight");
    if (input && pre) pre.innerHTML = highlightSmalltalk(input.value);
  }

  // ── Editor hardening ────────────────────────────────────────────────────
  //
  // The transparent <textarea> and the highlighted <pre> beneath it are a
  // stack that must behave as ONE surface. As shipped they didn't: nothing
  // synced the <pre>'s scroll to the textarea's, both were `overflow:auto`,
  // and nothing measured the text — so scrolling drifted the highlight away
  // from the caret, two scrollbars fought, and the box never sized to its
  // content. These two functions fix all three, THEME-AGNOSTICALLY (inline
  // styles only — the 7 theme CSS files are left untouched): the <pre> becomes
  // a pure backdrop that mirrors the textarea's scroll, only the textarea
  // scrolls, both reserve a scrollbar gutter so they wrap identically (the
  // caret lands on the right glyph), and the editor autosizes to the text up
  // to the room its pane affords, then scrolls.

  // Size the editor box to its text: measure the true content height from the
  // textarea's own layout (scrollHeight already accounts for font/wrap/tabs
  // and, with border-box, padding), clamp to the room between the editor's top
  // and the bottom of its pane, and keep the backdrop's scroll in step.
  function measureCodeEditor(editor) {
    const input = editor.querySelector(".st-code-input");
    const pre = editor.querySelector(".st-code-highlight");
    if (!input) return;
    // The ClassOutliner's inline per-method editor (.st-smappl-editor) is a
    // deliberate fixed-height box (CSS `height: 8em`) sitting inside a
    // shrink-to-fit tree node. That node hugs the editor, so a parent-relative
    // autosize cap is circular — `parent.bottom - editor.top` is just the
    // editor's own height, which collapses the box to a sliver and (once small)
    // keeps it small. Leave its CSS height untouched; only the backdrop's scroll
    // still needs to track the textarea. (Autosizing every method to its full
    // source would also make a browsed class's tree absurdly tall.)
    if (editor.classList.contains("st-smappl-editor")) {
      if (pre) {
        pre.scrollTop = input.scrollTop;
        pre.scrollLeft = input.scrollLeft;
      }
      return;
    }
    // cap = the room from the editor's top to the bottom of its pane (measured
    // before we touch the height, so the editor's top is its settled position).
    let cap = Infinity;
    const parent = editor.parentElement;
    if (parent) {
      const pr = parent.getBoundingClientRect();
      const er = editor.getBoundingClientRect();
      const padB = parseFloat(getComputedStyle(parent).paddingBottom) || 0;
      cap = Math.max(0, pr.bottom - er.top - padB);
    }
    // scrollHeight never reports LESS than the visible box, so to size DOWN to
    // a short method we must collapse the box first, then read the true content
    // height (one synchronous reflow), then size to it — clamped to the pane.
    editor.style.height = "0px";
    const contentH = input.scrollHeight;
    editor.style.height = Math.min(contentH, cap) + "px";
    if (pre) {
      pre.scrollTop = input.scrollTop;
      pre.scrollLeft = input.scrollLeft;
    }
  }

  function hardenCodeEditor(editor) {
    const input = editor.querySelector(".st-code-input");
    const pre = editor.querySelector(".st-code-highlight");
    if (!input || !pre) return;
    highlightEditor(editor);
    if (!editor.__macvmHardened) {
      editor.__macvmHardened = true;
      // The <pre> is a backdrop only: never its own scrollbar, only moved to
      // mirror the textarea (which is the single scroller).
      pre.style.overflow = "hidden";
      // Reserve a stable gutter on both so the textarea's scrollbar can't
      // narrow its text and drift its wrap away from the <pre>'s.
      input.style.scrollbarGutter = "stable";
      pre.style.scrollbarGutter = "stable";
      // Autosize hugs the text: our explicit height must drive the box, so
      // take it out of the `flex: 1` regime (whose `flex-basis: 0%` would
      // otherwise collapse it and ignore `height`) with `flex: 0 0 auto`.
      // The fixed-height inline outliner editor doesn't autosize (see
      // measureCodeEditor), so leave its flex alone.
      if (!editor.classList.contains("st-smappl-editor")) {
        editor.style.flex = "0 0 auto";
      }
      input.addEventListener(
        "scroll",
        function () {
          pre.scrollTop = input.scrollTop;
          pre.scrollLeft = input.scrollLeft;
        },
        { passive: true }
      );
    }
    measureCodeEditor(editor);
  }

  function remeasureAll() {
    document.querySelectorAll(".st-code-editor").forEach(measureCodeEditor);
  }

  // Called after every pane patch (main.rs::replace_pane) and once on
  // initial page load (below) — idempotent (per-editor `__macvmHardened`
  // guard), cheap at mock scale. A second measure on the NEXT frame catches
  // the case where this fires before the pane's flex layout has settled (an
  // early measure collapses the box); the font-load pass below covers the
  // metrics changing when the code font finishes loading.
  window.macvmHighlightCodeEditors = function () {
    document.querySelectorAll(".st-code-editor").forEach(hardenCodeEditor);
    requestAnimationFrame(remeasureAll);
  };

  // Re-measure when the window resizes (the pane's available height, hence the
  // autosize cap, changes) and once the code font has actually loaded (its
  // metrics decide the content height — measuring against a fallback font
  // mis-sizes the box).
  window.addEventListener("resize", remeasureAll);
  if (document.fonts && document.fonts.ready) {
    document.fonts.ready.then(remeasureAll);
  }

  // Live re-highlight as the user types; the workspace's own textarea
  // (id-scoped, not every `.st-code-input`) also pushes its latest value
  // back to Rust on every keystroke so navigating away and back doesn't
  // silently lose it — main.rs's `WORKSPACE_TEXT`, read by
  // `display_workspace`. `macvmInsertPrintResult` dispatches a synthetic
  // "input" event after inserting a Print-it result so both effects fire
  // from this one listener rather than needing to duplicate either.
  document.addEventListener(
    "input",
    function (ev) {
      const input = ev.target.closest(".st-code-input");
      if (!input) return;
      const editor = input.closest(".st-code-editor");
      if (editor) {
        highlightEditor(editor);
        // Re-autosize + re-sync the backdrop as the text changes.
        measureCodeEditor(editor);
      }
      if (input.id === "macvm-workspace-input") {
        post({ kind: "workspaceTextChanged", text: input.value });
      }
    },
    true
  );

  // Cmd+S (or Ctrl+S) inside the class browser's source editor accepts the
  // edit — see vm_host::VmRequest::BrowserSaveSource. Scoped to
  // `.st-browser-source .st-code-input` specifically (not just any
  // `.st-code-input`) so it doesn't also fire inside the Workspace's own
  // editor below, which has no class/method to "accept" the text as
  // source *of* — Workspace has its own Do it/Print it actions instead.
  //
  // The `data-save-*` attributes on `.st-browser-source` are a snapshot of
  // exactly what this pane was rendered against (browser_render.rs); sent
  // back alongside the text so the worker can verify the selection hasn't
  // moved on since — see BrowserSelection's own doc comment for the race
  // this closes.
  document.addEventListener(
    "keydown",
    function (ev) {
      const input = ev.target.closest(".st-browser-source .st-code-input");
      if (!input) return;
      if ((ev.metaKey || ev.ctrlKey) && (ev.key === "s" || ev.key === "S")) {
        ev.preventDefault();
        const pane = input.closest(".st-browser-source");
        post({
          kind: "browserSaveSource",
          text: input.value,
          savedPackage: pane ? pane.getAttribute("data-save-package") || "" : "",
          savedClass: pane ? pane.getAttribute("data-save-class") || "" : "",
          savedSide: pane ? pane.getAttribute("data-save-side") || "" : "",
          savedCategory: pane ? pane.getAttribute("data-save-category") || "" : "",
          savedMethod: pane ? pane.getAttribute("data-save-method") || "" : "",
          savedTarget: pane ? pane.getAttribute("data-save-target") || "" : "",
        });
      }
    },
    true
  );

  // Cmd+S (or Ctrl+S) inside a ClassOutliner smappl source editor accepts the
  // edit — vm_host versions it back into image_store (never an overwrite) and
  // live-compiles it into the running VM. Scoped to `.st-smappl-src` so it
  // can't collide with the class browser's own `.st-browser-source` Cmd+S.
  document.addEventListener(
    "keydown",
    function (ev) {
      const input = ev.target.closest(".st-smappl-src");
      if (!input) return;
      if ((ev.metaKey || ev.ctrlKey) && (ev.key === "s" || ev.key === "S")) {
        ev.preventDefault();
        const host = input.closest("[data-widget-id]");
        post({
          kind: "smapplAccept",
          cls: input.getAttribute("data-src-class") || "",
          side: input.getAttribute("data-src-side") || "",
          sel: input.getAttribute("data-src-sel") || "",
          text: input.value,
          widgetId: host ? host.getAttribute("data-widget-id") || "" : "",
        });
      }
    },
    true
  );

  // Find tools (Implementors/Senders): Enter in the search box runs the query
  // (vm_host VmRequest::Find); results land via macvmSetFindResults.
  document.addEventListener(
    "keydown",
    function (ev) {
      const input = ev.target.closest(".st-find-input");
      if (!input) return;
      if (ev.key === "Enter") {
        ev.preventDefault();
        const q = input.value.trim();
        if (q) post({ kind: "find", tool: input.getAttribute("data-find-tool") || "", query: q });
      }
    },
    true
  );

  // vm_host::VmResponse::FindResults arrives here (main.rs). Fill the results
  // container (keeping its data-widget-id so a result click can drill).
  window.macvmSetFindResults = function (html) {
    const el = document.getElementById("find-results");
    if (el) el.innerHTML = html;
  };

  // ── Workspace (workspace_render.rs) ────────────────────────────────────
  //
  // Do it/Print it act on the current selection, or the whole buffer if
  // nothing's selected — classic Smalltalk workspace convention. "Do it"
  // reuses the plain "doit" message (vm_host::VmRequest::Doit) unchanged;
  // "Print it" is its own round trip because its result has to land
  // *inline* in the textarea, not in the transcript.

  function workspaceEvalTarget(input) {
    const start = input.selectionStart;
    const end = input.selectionEnd;
    if (start !== end) return { code: input.value.slice(start, end), insertAt: end };
    return { code: input.value, insertAt: input.value.length };
  }

  // Where to insert the "Print it" result once its (asynchronous)
  // response arrives — captured at click/keypress time rather than
  // re-read from the current selection then, in case the user has since
  // clicked or typed elsewhere in the textarea.
  let pendingPrintInsertAt = null;

  function doIt(input) {
    post({ kind: "doit", code: workspaceEvalTarget(input).code });
  }

  function printIt(input) {
    const target = workspaceEvalTarget(input);
    pendingPrintInsertAt = target.insertAt;
    post({ kind: "workspacePrintIt", code: target.code });
  }

  document.addEventListener(
    "click",
    function (ev) {
      const doItBtn = ev.target.closest('[data-workspace-action="do-it"]');
      if (doItBtn) {
        ev.preventDefault();
        const input = document.getElementById("macvm-workspace-input");
        if (input) doIt(input);
        return;
      }
      const printItBtn = ev.target.closest('[data-workspace-action="print-it"]');
      if (printItBtn) {
        ev.preventDefault();
        const input = document.getElementById("macvm-workspace-input");
        if (input) printIt(input);
      }
    },
    true
  );

  // Cmd+D / Cmd+P, scoped to the Workspace's own textarea by id (not the
  // shared `.st-code-input` class every code editor has) so these can't
  // collide with the class browser's Cmd+S, and vice versa.
  document.addEventListener(
    "keydown",
    function (ev) {
      const input = ev.target.closest("#macvm-workspace-input");
      if (!input) return;
      if ((ev.metaKey || ev.ctrlKey) && (ev.key === "d" || ev.key === "D")) {
        ev.preventDefault();
        doIt(input);
      } else if ((ev.metaKey || ev.ctrlKey) && (ev.key === "p" || ev.key === "P")) {
        ev.preventDefault();
        printIt(input);
      }
    },
    true
  );

  // vm_host::VmResponse::WorkspacePrintResult arrives here (main.rs).
  window.macvmInsertPrintResult = function (text) {
    const input = document.getElementById("macvm-workspace-input");
    if (!input) {
      pendingPrintInsertAt = null;
      return;
    }
    const at = pendingPrintInsertAt === null ? input.value.length : Math.min(pendingPrintInsertAt, input.value.length);
    input.value = input.value.slice(0, at) + text + input.value.slice(at);
    const newCursor = at + text.length;
    input.selectionStart = input.selectionEnd = newCursor;
    pendingPrintInsertAt = null;
    input.focus();
    // Setting .value directly doesn't fire a real "input" event — dispatch
    // one so the shared listener above re-highlights *and* persists this
    // change, instead of duplicating both here.
    input.dispatchEvent(new Event("input", { bubbles: true }));
  };

  // ── Canvas (canvas_render.rs, ../docs/CANVAS.md) ───────────────────────
  //
  // "Run Demo"/"Clear" stand in for a real Smalltalk `Canvas` send the same
  // way Workspace's Do it/Print it stand in for real evaluation above —
  // they round-trip through vm_host's mock world (VmRequest::CanvasRunDemo/
  // CanvasClear) rather than drawing directly in JS, so the whole pipeline
  // gets exercised for real every time, not just the rendering half of it.

  document.addEventListener(
    "click",
    function (ev) {
      const runDemoBtn = ev.target.closest('[data-canvas-action="run-demo"]');
      if (runDemoBtn) {
        ev.preventDefault();
        post({ kind: "canvasRunDemo" });
        return;
      }
      // Generic "Smalltalk draws to the canvas": any control carrying
      // data-canvas-eval posts its expression through the same canvasEval
      // path; the VM evaluates it and its command-batch answer is drawn. The
      // Mandelbrot button is just one such control — no per-drawing JS. The
      // compute runs on the worker thread, so the UI stays responsive.
      const evalBtn = ev.target.closest('[data-canvas-action="eval"]');
      if (evalBtn) {
        ev.preventDefault();
        post({ kind: "canvasEval", code: evalBtn.getAttribute("data-canvas-eval") || "" });
        return;
      }
      // Generic pixel path (docs/CANVAS.md): the control's data-canvas-eval
      // answers a width*height*4 RGBA buffer; blit it via putImageData. The
      // canvas element itself is the authority on width/height (its pixel
      // buffer size), so the Smalltalk buffer and the blit always agree.
      const pixBtn = ev.target.closest('[data-canvas-action="eval-pixels"]');
      if (pixBtn) {
        ev.preventDefault();
        const cv = document.getElementById("macvm-canvas-0");
        post({
          kind: "canvasEvalPixels",
          code: pixBtn.getAttribute("data-canvas-eval") || "",
          width: String(cv ? cv.width : 0),
          height: String(cv ? cv.height : 0),
        });
        return;
      }
      const clearBtn = ev.target.closest('[data-canvas-action="clear"]');
      if (clearBtn) {
        ev.preventDefault();
        post({ kind: "canvasClear" });
      }
    },
    true
  );

  // The wire format (docs/CANVAS.md §5.2): a JSON array of
  // `[opName, ...args]` entries. Two explicit allowlists, checked before
  // touching `ctx` at all — an unrecognized op name is a clean, logged
  // no-op rather than a thrown exception or (worse) blindly indexing into
  // the canvas context with a bug/attacker-controlled string.
  const CANVAS_METHODS = new Set([
    "beginPath", "closePath", "moveTo", "lineTo", "rect", "arc", "arcTo",
    "quadraticCurveTo", "bezierCurveTo", "fill", "stroke", "clip",
    "fillRect", "strokeRect", "clearRect", "fillText", "strokeText",
    "save", "restore", "translate", "rotate", "scale", "resetTransform",
  ]);
  const CANVAS_PROPERTIES = new Set([
    "fillStyle", "strokeStyle", "lineWidth", "lineCap", "lineJoin", "font",
    "textAlign", "textBaseline", "globalAlpha",
  ]);

  // vm_host::VmResponse::CanvasDraw arrives here (main.rs).
  window.macvmCanvasDraw = function (id, commandsJson) {
    const canvas = document.getElementById("macvm-canvas-" + id);
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    let commands;
    try {
      commands = JSON.parse(commandsJson);
    } catch (e) {
      console.warn("macvm canvas: malformed commands JSON", e);
      return;
    }
    for (const cmd of commands) {
      const name = cmd[0];
      const args = cmd.slice(1);
      if (CANVAS_METHODS.has(name)) ctx[name](...args);
      else if (CANVAS_PROPERTIES.has(name)) ctx[name] = args[0];
      else console.warn("macvm canvas: unknown op", name);
    }
  };

  // vm_host::VmResponse::CanvasPixels arrives here (main.rs): a width*height*4
  // RGBA buffer, base64-encoded (docs/CANVAS.md pixel path). Decode it into an
  // ImageData and blit in one putImageData — the right primitive for a
  // per-pixel image (the Mandelbrot), vs thousands of fillRect commands. atob
  // gives a binary string; copy its char codes straight into the ImageData's
  // Uint8ClampedArray (RGBA layout matches the Pixmap byte order).
  window.macvmCanvasPutPixels = function (id, width, height, base64) {
    const canvas = document.getElementById("macvm-canvas-" + id);
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    let bin;
    try {
      bin = atob(base64);
    } catch (e) {
      console.warn("macvm canvas: malformed base64 pixels", e);
      return;
    }
    const need = width * height * 4;
    if (bin.length !== need) {
      console.warn("macvm canvas: pixel buffer size mismatch", bin.length, need);
      return;
    }
    const img = ctx.createImageData(width, height);
    const data = img.data;
    for (let i = 0; i < need; i++) data[i] = bin.charCodeAt(i);
    ctx.putImageData(img, 0, 0);
  };

  // vm_host::VmResponse::CanvasCreated arrives here (main.rs). The response
  // is the authority on size, not this page's own initial static guess
  // (canvas_render::render_canvas) — keeps the <canvas> element's actual
  // pixel-buffer dimensions (its width/height attributes, not CSS size) in
  // sync if a future request ever asks for a size other than the default.
  window.macvmCanvasCreated = function (id, width, height) {
    const canvas = document.getElementById("macvm-canvas-" + id);
    if (!canvas) return;
    canvas.width = width;
    canvas.height = height;
  };

  // ── Context menu takeover ───────────────────────────────────────────────
  //
  // WKWebView's own default right-click menu is generic web-browser chrome
  // (Reload, Look Up, Search the web, Share, spelling suggestions…) — none
  // of it belongs in a native Smalltalk IDE recreation. There's no
  // WKUIDelegate installed to customize it natively (`../src/objc.rs`), and
  // there couldn't easily be one anyway: macOS's context-menu-customization
  // delegate methods are all completion-handler/block-based, and this
  // bridge doesn't implement the Objective-C block ABI (see that file's
  // module doc). So this is handled entirely in JS instead: suppress the
  // native menu outright, replace it with a small custom one.
  //
  // For now the replacement only offers Cut/Copy/Paste/Select All, and
  // only inside an editable field — real actions, not stubs: clicking one
  // posts `{kind:"editAction", action}`, which `main.rs`'s `send_edit_action`
  // fires via `NSApp sendAction:to:from:` with a nil target, the same
  // responder-chain dispatch a *native* Edit-menu item uses, so it reaches
  // WKWebView's own internal text handling reliably rather than
  // reimplementing clipboard access in JS (fragile/sandboxed, especially
  // for paste). Right-clicking anywhere else shows nothing yet — the
  // obvious extension point for later, once Smalltalk exists to ask "what
  // menu belongs here?" per the classic yellow-button-menu idea, is right
  // where `EDITABLE_FIELD_MENU_ITEMS` is built below, keyed on
  // `ev.target`/whatever widget was actually clicked.

  const EDITABLE_FIELD_MENU_ITEMS = [
    ["Cut", "cut"],
    ["Copy", "copy"],
    ["Paste", "paste"],
    ["Select All", "selectAll"],
  ];
  // Outside an editable field, only Copy/Select All make sense — offered
  // whenever there's an actual text selection (e.g. documentation prose),
  // so right-clicking selected page text isn't a dead end now that the
  // native menu (which used to cover this) is suppressed everywhere.
  const SELECTION_MENU_ITEMS = [
    ["Copy", "copy"],
    ["Select All", "selectAll"],
  ];

  let activeContextMenu = null;

  function closeContextMenu() {
    if (activeContextMenu) {
      activeContextMenu.remove();
      activeContextMenu = null;
    }
  }

  function openContextMenu(x, y, items) {
    closeContextMenu();
    const menu = document.createElement("div");
    menu.className = "st-context-menu";
    menu.style.left = x + "px";
    menu.style.top = y + "px";
    items.forEach(function ([label, action]) {
      const item = document.createElement("div");
      item.className = "st-context-menu-item";
      item.textContent = label;
      item.addEventListener("click", function (ev) {
        ev.preventDefault();
        closeContextMenu();
        post({ kind: "editAction", action: action });
      });
      menu.appendChild(item);
    });
    document.body.appendChild(menu);
    activeContextMenu = menu;
  }

  document.addEventListener(
    "contextmenu",
    function (ev) {
      ev.preventDefault();
      if (ev.target.closest("textarea, input")) {
        openContextMenu(ev.clientX, ev.clientY, EDITABLE_FIELD_MENU_ITEMS);
      } else if (window.getSelection && window.getSelection().toString().length > 0) {
        openContextMenu(ev.clientX, ev.clientY, SELECTION_MENU_ITEMS);
      } else {
        closeContextMenu();
      }
    },
    true
  );

  document.addEventListener(
    "click",
    function (ev) {
      if (activeContextMenu && !activeContextMenu.contains(ev.target)) closeContextMenu();
    },
    true
  );

  document.addEventListener("keydown", function (ev) {
    if (ev.key === "Escape") {
      closeContextMenu();
      // Esc also dismisses an open modal dialog overlay (Visual>>promptOk:).
      document.querySelectorAll(".st-modal").forEach(function (m) {
        m.remove();
      });
    }
  });

  // ── smappl widgets (../smappl.md, ../src/preprocess.rs) ────────────────
  //
  // Each `<span class="smappl" data-widget-id data-code>` is an inert G0
  // placeholder until the VM renders its `visual=` expression. On load we
  // post one `{kind:"smappl"}` per span; the worker answers with an HTML
  // fragment that `main.rs` hands to `macvmRenderSmappl`, which swaps the
  // span for the live widget (D-G5). A shape the VM can't build yet just
  // never gets swapped, so the placeholder box remains — no broken page.

  function requestSmapplRenders() {
    document.querySelectorAll("span.smappl[data-widget-id]").forEach(function (span) {
      post({
        kind: "smappl",
        id: span.getAttribute("data-widget-id") || "",
        code: span.getAttribute("data-code") || "",
      });
    });
  }

  // The open/expanded nodes of an outliner, keyed by their header text — so a
  // live refresh (re-rendered fragment) can restore the tree to how the user
  // had it rather than snapping everything shut.
  // A node's stable key = its header text minus the toggle glyph (which flips
  // ▾/▸ with open state, so it must not be part of the key).
  function nodeKey(header) {
    const tw = header.querySelector(":scope > .st-tw");
    let t = header.textContent;
    if (tw) t = t.replace(tw.textContent, "");
    return t.trim();
  }
  function captureOpenNodes(root) {
    const open = new Set();
    root.querySelectorAll(".st-node > .st-children").forEach(function (kids) {
      if (kids.style.display !== "none") {
        const header = kids.parentElement.querySelector(":scope > .st-header");
        if (header) open.add(nodeKey(header));
      }
    });
    return open;
  }
  function restoreOpenNodes(root, open) {
    root.querySelectorAll(".st-node > .st-header").forEach(function (header) {
      const kids = header.parentElement.querySelector(":scope > .st-children");
      const tw = header.querySelector(":scope > .st-tw[data-tw]");
      if (kids && tw && open.has(nodeKey(header))) {
        kids.style.display = "";
        tw.innerHTML = "▾ ";
      }
    });
  }

  // vm_host::VmResponse::SmapplFragment arrives here (main.rs). Matches the
  // placeholder span on first render OR an already-rendered fragment's root on
  // a live refresh (both carry data-widget-id).
  window.macvmRenderSmappl = function (widgetId, html) {
    const el = document.querySelector('[data-widget-id="' + widgetId + '"]');
    if (!el) return;
    const open = el.classList && el.classList.contains("smappl") ? null : captureOpenNodes(el);
    el.outerHTML = html;
    if (open) {
      const fresh = document.querySelector('[data-widget-id="' + widgetId + '"]');
      if (fresh) restoreOpenNodes(fresh, open);
    }
    // A ClassOutliner fragment carries `.st-code-editor` source editors —
    // paint their syntax highlighting now that they're in the DOM.
    if (window.macvmHighlightCodeEditors) window.macvmHighlightCodeEditors();
  };

  // vm_host::VmResponse::SmapplOverlay arrives here (main.rs): a live widget
  // action (Visual>>promptOk:, the "Press Me!" demo) answered a modal dialog
  // fragment. Float it over the page; clicking OK / the backdrop / Esc removes
  // it (the document click + keydown handlers above own the close). Only one
  // dialog at a time — a fresh one replaces any still-open overlay.
  window.macvmShowOverlay = function (html) {
    document.querySelectorAll(".st-modal").forEach(function (m) {
      m.remove();
    });
    const host = document.createElement("div");
    host.innerHTML = html;
    const overlay = host.firstElementChild;
    if (!overlay) return;
    document.body.appendChild(overlay);
    // Focus the OK button so Enter/Space also dismisses (native-dialog feel).
    const ok = overlay.querySelector(".st-modal-ok");
    if (ok) ok.focus();
  };

  // The script tag lives in <head> (chrome_head_extra), so <body> — and
  // any .st-code-editor in it — doesn't exist yet when this file first
  // runs; the initial highlight has to wait for the DOM to actually load.
  document.addEventListener("DOMContentLoaded", function () {
    window.macvmHighlightCodeEditors();
    requestSmapplRenders();
  });
})();
