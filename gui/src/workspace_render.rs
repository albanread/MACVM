//! The Workspace tool (`../docs/APPS.md` §5.5: "Workspace = editor pane +
//! doIt/printIt/inspectIt actions over §4's protocol") — a free-text
//! scratch buffer for evaluating arbitrary Smalltalk, deliberately *not*
//! tied to any class/method identity the way `browser_render.rs`'s source
//! pane is. That's the key difference from the class browser's editor:
//! there's no Cmd+S "accept" here, because there's no class/method to
//! save the text *as* — only Do it (§4's `evaluate:ifError:`, reusing the
//! existing `vm_host::VmRequest::Doit`) and Print it (the same eval, but
//! answering a printString to insert inline rather than echoing to the
//! transcript — `VmRequest::WorkspacePrintIt`).
//!
//! `inspectIt` isn't wired up: it needs a whole separate Inspector tool
//! (`docs/APPS.md` §5.7-adjacent) that doesn't exist yet, and isn't part
//! of "the hooks needed ready for the VM to run" — just doIt/printIt.
//!
//! Reuses the same transparent-textarea-over-highlighted-`<pre>` overlay
//! `browser_render.rs`'s source pane uses (`smtk.js`'s
//! `macvmHighlightCodeEditors()` fills the `<pre>` in, same as there) —
//! same visual language, same one-tokenizer-in-JS rule.

use crate::browser_render::escape;

/// A short, honest starting comment rather than a truly blank buffer —
/// "obviously we can't actually run it yet" (this is a stub-VM tool), so
/// says so up front instead of leaving a first-time user wondering why
/// Do it/Print it don't do anything interesting.
const PLACEHOLDER_TEXT: &str = "\"Type Smalltalk here.\n\
Do it runs the selection (or everything, if nothing's selected).\n\
Print it does the same and inserts the result right after it.\n\n\
There's no VM yet, so both are stubs for now — see vm_host::VmRequest::Doit/WorkspacePrintIt.\"";

/// `initial_text` lets `main.rs` restore whatever was last typed within
/// the same app run (currently: it doesn't — every open starts fresh; see
/// `main.rs::open_workspace`'s doc comment for why that's an accepted
/// limitation for now, not an oversight) without this function needing to
/// know or care where the text came from.
pub fn render_workspace(initial_text: &str) -> String {
    format!(
        "<div class=\"st-workspace\" id=\"macvm-workspace\">\
         <div class=\"st-browser-action-row st-workspace-actions\">\
         <button type=\"button\" class=\"st-browser-new-button\" data-workspace-action=\"do-it\">Do it</button>\
         <button type=\"button\" class=\"st-browser-new-button\" data-workspace-action=\"print-it\">Print it</button>\
         </div>\
         <div class=\"st-code-editor st-workspace-editor\">\
         <pre class=\"st-code-highlight\" aria-hidden=\"true\"></pre>\
         <textarea class=\"st-code-input\" id=\"macvm-workspace-input\" spellcheck=\"false\">{}</textarea>\
         </div></div>",
        escape(initial_text)
    )
}

/// The text a brand-new Workspace opens with.
pub fn initial_text() -> &'static str {
    PLACEHOLDER_TEXT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_workspace_includes_editor_and_both_action_buttons() {
        let html = render_workspace("foo bar baz");
        assert!(html.contains("id=\"macvm-workspace\""), "{html}");
        assert!(html.contains("id=\"macvm-workspace-input\""), "{html}");
        assert!(html.contains("foo bar baz"), "{html}");
        assert!(html.contains("data-workspace-action=\"do-it\""), "{html}");
        assert!(html.contains("data-workspace-action=\"print-it\""), "{html}");
    }

    #[test]
    fn render_workspace_escapes_its_content() {
        let html = render_workspace("1 < 2 & 3 > 1");
        assert!(!html.contains("1 < 2"), "{html}");
        assert!(html.contains("1 &lt; 2"), "{html}");
    }

    #[test]
    fn no_accept_affordance_unlike_the_browser_source_editor() {
        // Workspace text isn't source belonging to any class/method — no
        // st-browser-source / st-browser-pane wrapper, which is what
        // `render_source_pane` uses and what smtk.js's Cmd+S handler is
        // scoped to.
        let html = render_workspace("");
        assert!(!html.contains("st-browser-source"), "{html}");
    }
}
