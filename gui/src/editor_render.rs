//! Pure page-body rendering for the Smalltalk text editor
//! (`docs/editor_design.md`, the toolbar's `texteditor` icon).
//!
//! M0: a class picker (a `<datalist>` combobox over `Image::class_names`) plus
//! a read-only `<textarea>` holding the class rendered to `.mst` text by
//! `Image::class_source`. This module is deliberately just the HTML *body* — no
//! Objective-C, no image I/O — so it is unit-testable; `main.rs::display_editor`
//! does the image read and wraps the result with `render_generated_page`.

use crate::preprocess::html_escape_text;

/// The editor body. `class_names` fills the picker; `(current, source)` is the
/// loaded class and its text — `("", hint)` when nothing is loaded yet.
pub fn render_editor(class_names: &[String], current: &str, source: &str) -> String {
    let options: String = class_names
        .iter()
        .map(|n| format!("<option value=\"{}\">", html_escape_text(n)))
        .collect();
    format!(
        "<h1>Text Editor</h1>\
         <p>Edit a class as text. Pick a class, then Enter to load it. \
         Save syntax-checks and writes to the database + world files \
         (takes effect on the next launch).</p>\
         <div class=\"st-editor-toolbar\">\
           <input class=\"st-editor-class\" type=\"text\" list=\"st-editor-classes\" \
            value=\"{current}\" placeholder=\"class name, e.g. Mandelbrot\" \
            autocomplete=\"off\" spellcheck=\"false\">\
           <button type=\"button\" class=\"st-editor-save\" data-editor-action=\"save\">Save</button>\
         </div>\
         <datalist id=\"st-editor-classes\">{options}</datalist>\
         <textarea class=\"st-editor-buffer\" spellcheck=\"false\" \
          autocomplete=\"off\" autocapitalize=\"off\" wrap=\"off\" \
          data-class=\"{current}\">{source}</textarea>",
        current = html_escape_text(current),
        source = html_escape_text(source),
        options = options,
    )
}

/// The buffer text for a fetch outcome: the class source, or a quoted hint (a
/// Smalltalk comment, so it reads naturally in a code buffer) when the class is
/// absent or nothing is selected yet.
pub fn buffer_for(class_name: Option<&str>, source: Option<String>) -> (String, String) {
    match (class_name, source) {
        (Some(name), Some(text)) => (name.to_string(), text),
        (Some(name), None) => (
            String::new(),
            format!("\"No class named {name} in the image.\""),
        ),
        (None, _) => (
            String::new(),
            "\"Pick a class above to load its source.\"".to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_for_covers_the_three_outcomes() {
        assert_eq!(
            buffer_for(Some("Mandelbrot"), Some("Object subclass: Mandelbrot [ ]".into())),
            ("Mandelbrot".to_string(), "Object subclass: Mandelbrot [ ]".to_string())
        );
        let (cur, src) = buffer_for(Some("Nope"), None);
        assert_eq!(cur, "");
        assert!(src.contains("No class named Nope"));
        let (cur, src) = buffer_for(None, None);
        assert_eq!(cur, "");
        assert!(src.contains("Pick a class"));
    }

    #[test]
    fn render_has_picker_options_and_buffer() {
        let names = vec!["Object".to_string(), "Mandelbrot".to_string()];
        let html = render_editor(&names, "Mandelbrot", "Object subclass: Mandelbrot [ ]");
        assert!(html.contains("<option value=\"Object\">"));
        assert!(html.contains("<option value=\"Mandelbrot\">"));
        assert!(html.contains("class=\"st-editor-class\""));
        assert!(html.contains("value=\"Mandelbrot\""));
        // M3c: the buffer is now a live editing surface (VM-authoritative), no
        // longer readonly — smtk.js's editor terminal drives it.
        assert!(html.contains("class=\"st-editor-buffer\""));
        assert!(!html.contains("readonly"));
        assert!(html.contains("Object subclass: Mandelbrot"));
    }

    #[test]
    fn render_escapes_source_and_names() {
        // A method body with < and & must not break out of the textarea.
        let html = render_editor(&["A<B".to_string()], "A<B", "foo\n\t^1 < 2 & true");
        assert!(html.contains("A&lt;B"));
        assert!(html.contains("1 &lt; 2 &amp; true"));
        assert!(!html.contains("1 < 2 & true"));
    }
}
