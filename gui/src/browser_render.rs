//! Renders the class browser's five panes (packages → classes → categories
//! → methods → source) as HTML fragments from a [`MockWorld`] plus the
//! current [`BrowserSelection`] — the "`HtmlWriter`" role from
//! `../docs/APPS.md` §6, standing in for the real thing until Phase W.
//!
//! Layout follows Helios's composable multi-pane browser (`../docs/APPS.md`
//! §11), adapted to plain CSS flex panes rather than draggable splitters —
//! visually still Strongtalk-styled (bevels/pixel icons under Classic, flat
//! cards under Hi-Def), just a different *shape* of page than the
//! single-scrolling-outliner pages `preprocess.rs` renders.
//!
//! Every function here is pure (`&MockWorld`, `&BrowserSelection` in,
//! `String` out) — deliberately no threading, no ObjC, no I/O — so the
//! rendering logic is unit-testable on its own, independent of
//! `vm_host.rs`'s threading around it.

use macvm_mock_vm::{MockWorld, Side};

/// What the source pane is currently showing, and what `BrowserSaveSource`
/// (Cmd+S) applies the accepted text to — kept as its own field rather than
/// inferred from `method: Option<String>` being `Some`/`None`, because
/// "new method"/"new class" modes need "no method is the active list
/// selection" (`method` stays `None`) to coexist with "the source pane is
/// showing an editable template, not the class comment" — two questions
/// that used to collapse into one `Option` before creation modes existed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SourceEditTarget {
    /// The selected class's comment (the original, and still the default,
    /// behavior whenever nothing more specific is going on).
    #[default]
    ClassComment,
    /// The selected class's header (superclass + instance variables),
    /// rendered and parsed as real `.mst` syntax — the "definition" tab
    /// alongside "comment".
    ClassDefinition,
    /// The selected method's source (`method` names which one).
    Method,
    /// A blank method template; accepting parses the selector out of the
    /// typed message pattern and creates/redefines/reopens it on
    /// `class`+`side`+`category`.
    NewMethod,
    /// A blank class-definition template; accepting parses the whole
    /// header (and any pasted-along methods) as real `.mst` syntax and
    /// creates/reopens it in `package`.
    NewClass,
}

/// `PartialEq` isn't just a convenience derive: `vm_host.rs`'s
/// `BrowserSaveSource` handler compares a snapshot of this (captured at
/// render time, round-tripped through the DOM and back — see
/// `render_source_pane`'s `data-save-*` attributes) against the worker's
/// *current* selection before applying a save, to catch a save meant for
/// one target silently landing on another if a selection-change request
/// and a Cmd+S race each other.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BrowserSelection {
    pub package: Option<String>,
    pub class: Option<String>,
    pub side: Side,
    pub category: Option<String>,
    pub method: Option<String>,
    pub edit_target: SourceEditTarget,
}

/// `pub(crate)` rather than private: `workspace_render.rs` needs the exact
/// same HTML-escaping for its own textarea content, and this one-liner
/// isn't worth its own shared-utils module just to avoid a second `use`.
pub(crate) fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn pane_item(kind: &str, value: &str, label: &str, active: bool, extra_style: &str) -> String {
    let active_class = if active { " active" } else { "" };
    format!(
        "<div class=\"st-browser-item{active_class}\" data-browser-{kind}=\"{}\" style=\"{extra_style}\">{}</div>",
        escape(value),
        escape(label)
    )
}

pub fn render_packages_pane(world: &MockWorld, sel: &BrowserSelection) -> String {
    let packages = world.packages();
    let items: String = packages
        .iter()
        .map(|pkg| {
            let active = sel.package.as_deref() == Some(pkg.as_str());
            pane_item("package", pkg, pkg, active, "")
        })
        .collect();
    // Needs *some* package to attach the new class to (`vm_host.rs`'s
    // `BrowserNewClass` handler defaults to the first one, same fallback
    // `render_classes_pane` already applies below) — hidden on a totally
    // empty world instead of producing a button that can't work.
    let new_class_row = if packages.is_empty() {
        String::new()
    } else {
        action_button_row("new-class", "+ New Class")
    };
    format!(
        "<div class=\"st-browser-pane\" id=\"macvm-browser-packages\">{items}{new_class_row}</div>"
    )
}

fn action_button_row(action: &str, label: &str) -> String {
    format!("<div class=\"st-browser-action-row\"><button type=\"button\" class=\"st-browser-new-button\" data-browser-action=\"{action}\">{label}</button></div>")
}

/// A "Remove X" button that reveals an inline confirm/cancel strip on
/// click (`smtk.js`) rather than relying on `window.confirm()` — this
/// WKWebView shell has no `WKUIDelegate` installed, so native JS confirm/
/// alert dialogs wouldn't show anything at all. `warning` is pre-escaped
/// HTML (from [`subclass_warning`]) to splice straight in, not a raw
/// string to escape again.
fn render_remove_row(action_kind: &str, label: &str, name: &str, warning: &str) -> String {
    format!(
        "<div class=\"st-browser-action-row st-remove-row\">\
         <button type=\"button\" class=\"st-browser-danger-button\" data-browser-action=\"remove-{action_kind}\">Remove {label}</button>\
         <span class=\"st-confirm-strip\" hidden>Remove <strong>{}</strong>?{warning} \
         <button type=\"button\" class=\"st-browser-danger-button\" data-browser-action=\"confirm-remove-{action_kind}\">Yes, remove</button> \
         <button type=\"button\" data-browser-action=\"cancel-remove\">Cancel</button></span>\
         </div>",
        escape(name)
    )
}

/// The confirmation warning for removing a class with subclasses —
/// `docs/IMAGE.md`'s "allow, GUI warns first" rule: `image_store`/
/// `MockWorld` remove a class unconditionally, so this text is the *only*
/// place that decision surfaces to the user before they commit to it.
fn subclass_warning(world: &MockWorld, class_name: &str) -> String {
    let subs = world.subclasses_of(class_name);
    if subs.is_empty() {
        return String::new();
    }
    let names: Vec<String> = subs.iter().map(|c| escape(&c.name)).collect();
    let plural = if subs.len() == 1 { "" } else { "es" };
    format!(
        " It has {} subclass{plural}: {}.",
        subs.len(),
        names.join(", ")
    )
}

/// Recursive hierarchy tree for whichever package is selected (falls back
/// to the first package if none is selected yet) — depth-indented, same
/// visual idiom as `strongtalk.css`'s `.st-outliner .st-node` (15px/level).
pub fn render_classes_pane(world: &MockWorld, sel: &BrowserSelection) -> String {
    let package = sel
        .package
        .clone()
        .or_else(|| world.packages().first().cloned());
    let mut out = String::new();
    if let Some(package) = &package {
        for root in world.package_roots(package) {
            render_class_subtree(world, sel, &root.name, 0, &mut out);
        }
    }
    if let Some(class_name) = &sel.class {
        out.push_str(&render_remove_row(
            "class",
            "Class",
            class_name,
            &subclass_warning(world, class_name),
        ));
    }
    format!("<div class=\"st-browser-pane\" id=\"macvm-browser-classes\">{out}</div>")
}

fn render_class_subtree(
    world: &MockWorld,
    sel: &BrowserSelection,
    class_name: &str,
    depth: u32,
    out: &mut String,
) {
    let active = sel.class.as_deref() == Some(class_name);
    let style = format!("padding-left: {}px", 8 + depth * 14);
    out.push_str(&pane_item("class", class_name, class_name, active, &style));
    for child in world.subclasses_of(class_name) {
        render_class_subtree(world, sel, &child.name, depth + 1, out);
    }
}

/// Instance/class side toggle plus the selected class's categories for
/// that side.
pub fn render_categories_pane(world: &MockWorld, sel: &BrowserSelection) -> String {
    let Some(class_name) = &sel.class else {
        return "<div class=\"st-browser-pane\" id=\"macvm-browser-categories\"></div>".to_string();
    };
    let side_tabs = format!(
        "<div class=\"st-browser-side-tabs\">\
         <span class=\"st-browser-side-tab{}\" data-browser-side=\"instance\">instance</span>\
         <span class=\"st-browser-side-tab{}\" data-browser-side=\"class\">class</span>\
         </div>",
        if sel.side == Side::Instance {
            " active"
        } else {
            ""
        },
        if sel.side == Side::Class {
            " active"
        } else {
            ""
        },
    );
    // A leading "(all)" pseudo-category, active whenever `sel.category` is
    // `None` — which the methods pane now reads as "show every method on
    // this side, not one specific category's" (`render_methods_pane`),
    // rather than "show nothing until a category is explicitly picked."
    // `data-browser-category=""` reuses the exact same click plumbing
    // (`smtk.js`) real categories use; `BrowserSelectCategory`'s handler
    // (`vm_host.rs`) treats an empty name as clearing back to `None`.
    // The selected side's variables (instance vars on the instance side, class
    // vars on the class side), read from the mock (image mirror), with a "＋ add"
    // field. This is the image-based half of the dual placement — a variable
    // added here shows IMMEDIATELY (unlike the outliner, which reflects the live
    // VM and so waits for a restart on an instance var). The field reuses the
    // outliner's `.st-smappl-new-var-src` handler (smtk.js) → smapplAddVar.
    let vars_section = world
        .class_named(class_name)
        .map(|c| {
            let (label, vars, kind) = if sel.side == Side::Class {
                ("class variables", c.class_vars.as_str(), "class")
            } else {
                ("instance variables", c.instance_vars.as_str(), "instance")
            };
            let items: String = vars
                .split_whitespace()
                .map(|v| format!("<div class=\"st-browser-var-item\">{}</div>", escape(v)))
                .collect();
            format!(
                "<div class=\"st-browser-vars\"><div class=\"st-browser-vars-label\">{label}</div>\
                 {items}<input type=\"text\" class=\"st-smappl-new-var-src st-browser-var-add\" \
                 spellcheck=\"false\" data-src-class=\"{}\" data-var-kind=\"{kind}\" \
                 placeholder=\"＋ add {kind} variable\"></div>",
                escape(class_name),
            )
        })
        .unwrap_or_default();
    let all_item = pane_item("category", "", "(all)", sel.category.is_none(), "");
    let categories = world.categories(class_name, sel.side);
    let items: String = categories
        .iter()
        .map(|cat| {
            let active = sel.category.as_deref() == Some(cat.as_str());
            pane_item("category", cat, cat, active, "")
        })
        .collect();
    format!("<div class=\"st-browser-pane\" id=\"macvm-browser-categories\">{side_tabs}{vars_section}{all_item}{items}</div>")
}

pub fn render_methods_pane(world: &MockWorld, sel: &BrowserSelection) -> String {
    let Some(class_name) = &sel.class else {
        return "<div class=\"st-browser-pane\" id=\"macvm-browser-methods\"></div>".to_string();
    };
    // `None` means "(all)" (see `render_categories_pane`) — a freshly
    // created, still-methodless class has zero categories to pick from at
    // all, so gating this pane (and its "+ New Method" button below) on a
    // category being selected meant there was no way to add its first
    // method. Defaulting to "every method on this side" instead of "no
    // method" fixes that and removes a click for the common case too.
    let methods = match &sel.category {
        Some(category) => world.methods_in(class_name, sel.side, category),
        None => world.methods_of(class_name, sel.side),
    };
    let items: String = methods
        .iter()
        .map(|m| {
            let active = sel.method.as_deref() == Some(m.selector.as_str());
            pane_item("method", &m.selector, &m.selector, active, "")
        })
        .collect();
    let new_method_row = action_button_row("new-method", "+ New Method");
    let remove_row = match &sel.method {
        Some(selector) => render_remove_row("method", "Method", selector, ""),
        None => String::new(),
    };
    format!("<div class=\"st-browser-pane\" id=\"macvm-browser-methods\">{items}{new_method_row}{remove_row}</div>")
}

/// Real `.mst` class-header syntax (`image_store::mst`'s doc comment) for
/// a class's superclass + instance variables — `nil` spelled out literally
/// when there's no superclass (matching how the corpus itself writes
/// `nil subclass: Object [...]`), and the `|ivars|` line omitted entirely
/// when there are none, rather than an empty `||` that reads like a
/// mistake. Shared by the "definition" tab (an existing class's current
/// header) and the "New Class" template (a blank one) so both are, and
/// stay, real parseable `.mst` text — `vm_host.rs`'s `BrowserSaveSource`
/// handler for both feeds the accepted text straight through
/// `image_store::mst::parse_mst_source`.
fn render_class_header_template(
    superclass: Option<&str>,
    class_name: &str,
    instance_vars: &str,
    class_vars: &str,
) -> String {
    let superclass = superclass.unwrap_or("nil");
    let ivars_line = if instance_vars.is_empty() {
        String::new()
    } else {
        format!("    |{instance_vars}|\n")
    };
    // Class vars are a `<classVars: A B>` body pragma (the image half of the
    // dual placement — the Smalltalk outliner shows the same names via live
    // reflection). Emitted only when there are some, and as real parseable
    // `.mst` so the definition tab round-trips through `parse_mst_source`.
    let cvars_line = if class_vars.trim().is_empty() {
        String::new()
    } else {
        format!("    <classVars: {}>\n", class_vars.trim())
    };
    format!("{superclass} subclass: {class_name} [\n{ivars_line}{cvars_line}\n]")
}

/// Blank "New Class" template — prefills the superclass with whichever
/// class (if any) is currently selected, since "make a subclass of what
/// I'm looking at" is the common case; falls back to `Object` otherwise.
fn render_new_class_template(sel: &BrowserSelection) -> String {
    let superclass = sel.class.as_deref().unwrap_or("Object");
    render_class_header_template(Some(superclass), "NewClass", "", "")
}

/// Blank "New Method" template. The placeholder word is itself a valid
/// (if meaningless) unary selector — `vm_host.rs`'s `BrowserSaveSource`
/// handler parses whatever message pattern is actually on the first line
/// at accept time (`image_store::mst::parse_method_selector`), unary,
/// binary, or keyword, so replacing this line is what actually names the
/// method; nothing else about the request needs to know the selector in
/// advance.
fn render_new_method_template() -> String {
    "messageSelector\n\t\"Say what this method does.\"".to_string()
}

/// The "comment"/"definition" tab pair above the source pane, shown only
/// while a class (not a method, and not one of the New* creation modes)
/// is what the pane's showing — `Method`/`NewMethod`/`NewClass` all render
/// no tabs, since "which tab" isn't a meaningful question for any of them.
fn definition_tabs(sel: &BrowserSelection) -> String {
    if sel.class.is_none() {
        return String::new();
    }
    let (comment_active, definition_active) = match sel.edit_target {
        SourceEditTarget::ClassComment => (" active", ""),
        SourceEditTarget::ClassDefinition => ("", " active"),
        _ => return String::new(),
    };
    format!(
        "<div class=\"st-browser-side-tabs st-definition-tabs\">\
         <span class=\"st-browser-side-tab{comment_active}\" data-browser-action=\"edit-comment\">comment</span>\
         <span class=\"st-browser-side-tab{definition_active}\" data-browser-action=\"edit-definition\">definition</span>\
         </div>"
    )
}

fn side_to_wire(side: Side) -> &'static str {
    match side {
        Side::Instance => "instance",
        Side::Class => "class",
    }
}

fn side_from_wire(s: &str) -> Side {
    if s == "class" {
        Side::Class
    } else {
        Side::Instance
    }
}

fn edit_target_to_wire(target: &SourceEditTarget) -> &'static str {
    match target {
        SourceEditTarget::ClassComment => "comment",
        SourceEditTarget::ClassDefinition => "definition",
        SourceEditTarget::Method => "method",
        SourceEditTarget::NewMethod => "new_method",
        SourceEditTarget::NewClass => "new_class",
    }
}

fn edit_target_from_wire(s: &str) -> SourceEditTarget {
    match s {
        "definition" => SourceEditTarget::ClassDefinition,
        "method" => SourceEditTarget::Method,
        "new_method" => SourceEditTarget::NewMethod,
        "new_class" => SourceEditTarget::NewClass,
        _ => SourceEditTarget::ClassComment,
    }
}

fn opt_from_wire(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

impl BrowserSelection {
    /// Reconstructs a `BrowserSelection` from the six `data-save-*`
    /// attributes `render_source_pane` stamps on the source pane's
    /// wrapper element — round-tripped through the DOM by `smtk.js`'s
    /// Cmd+S handler into `vm_host::VmRequest::BrowserSaveSource`'s six
    /// `saved_*` fields, then back into a `BrowserSelection` here for
    /// comparison against the worker's live selection (see this struct's
    /// own doc comment). Empty strings mean `None` — never a real
    /// package/class/category/method name in this app.
    pub fn from_wire(
        package: &str,
        class: &str,
        side: &str,
        category: &str,
        method: &str,
        edit_target: &str,
    ) -> Self {
        Self {
            package: opt_from_wire(package),
            class: opt_from_wire(class),
            side: side_from_wire(side),
            category: opt_from_wire(category),
            method: opt_from_wire(method),
            edit_target: edit_target_from_wire(edit_target),
        }
    }
}

/// The bottom pane: an editable `<textarea>` overlaid exactly on top of a
/// syntax-highlighted `<pre>` (the standard "transparent textarea over a
/// highlighted underlay" technique: the textarea stays the actual input
/// surface, giving native cursor/selection/undo/IME for free, and its
/// value is still exact 1:1 text — per the text-editing design
/// discussion, deliberately not `contentEditable` — while the `<pre>`
/// underneath, kept in sync by `smtk.js` on every keystroke, is what's
/// actually visible). The `<pre>` starts empty; `smtk.js`'s
/// `macvmHighlightCodeEditors()` fills it in once the pane lands in the
/// DOM (see `main.rs::replace_pane` and this module's own initial in-page
/// trigger), so there's exactly one tokenizer (JS), not a second one here
/// that could drift out of sync. Cmd+S in the textarea posts
/// `{kind:"browserSaveSource", text}`, which
/// `vm_host::VmRequest::BrowserSaveSource` "accepts" against
/// `sel.edit_target` — a method, the class comment, the class definition,
/// or (for the two `New*` targets) parses an identity out of `text` itself
/// to create/reopen/redefine.
pub fn render_source_pane(world: &MockWorld, sel: &BrowserSelection) -> String {
    let text = match &sel.edit_target {
        SourceEditTarget::ClassComment => sel
            .class
            .as_deref()
            .and_then(|n| world.class_comment(n))
            .unwrap_or_default(),
        SourceEditTarget::ClassDefinition => sel
            .class
            .as_deref()
            .and_then(|n| world.class_named(n))
            .map(|c| {
                render_class_header_template(
                    c.superclass.as_deref(),
                    &c.name,
                    &c.instance_vars,
                    &c.class_vars,
                )
            })
            .unwrap_or_default(),
        SourceEditTarget::Method => match (&sel.class, &sel.method) {
            (Some(class_name), Some(selector)) => world
                .method_source(class_name, sel.side, selector)
                .unwrap_or_else(|| String::from("(source not found)")),
            _ => String::new(),
        },
        SourceEditTarget::NewMethod => render_new_method_template(),
        SourceEditTarget::NewClass => render_new_class_template(sel),
    };
    let tabs = definition_tabs(sel);
    // The six `data-save-*` attributes are a snapshot of exactly what this
    // pane was rendered against — `smtk.js`'s Cmd+S handler reads them back
    // and sends them alongside the edited text, so `vm_host.rs` can verify
    // the save still targets what's on screen (see `BrowserSelection`'s own
    // doc comment for the race this closes: a selection-change request
    // queued between this render and the save landing).
    format!(
        "<div class=\"st-browser-pane st-browser-source\" id=\"macvm-browser-source\" \
         data-save-package=\"{}\" data-save-class=\"{}\" data-save-side=\"{}\" \
         data-save-category=\"{}\" data-save-method=\"{}\" data-save-target=\"{}\">\
         {tabs}\
         <div class=\"st-code-editor\">\
         <pre class=\"st-code-highlight\" aria-hidden=\"true\"></pre>\
         <textarea class=\"st-code-input\" spellcheck=\"false\">{}</textarea>\
         </div></div>",
        escape(sel.package.as_deref().unwrap_or("")),
        escape(sel.class.as_deref().unwrap_or("")),
        side_to_wire(sel.side),
        escape(sel.category.as_deref().unwrap_or("")),
        escape(sel.method.as_deref().unwrap_or("")),
        edit_target_to_wire(&sel.edit_target),
        escape(&text)
    )
}

/// Wraps five already-rendered pane HTML strings in the browser's outer
/// shell. Factored out from [`render_full_browser`] so `main.rs` can build
/// the *initial* page from real `VmResponse::BrowserOpened` data (the
/// worker thread's actual `world`/`selection` — mock or a real
/// `image_store::Image` mirror, see `vm_host.rs`) using the exact same
/// wrapper shape, without needing its own copy of this HTML structure.
/// This is also what fixed a real race: an earlier version synchronously
/// displayed `render_full_browser(&MockWorld::seed(), ...)` immediately on
/// open, then asynchronously patched in real data once `BrowserOpen`
/// returned — but that patch could arrive before (or while) the page was
/// still loading, and silently no-op (`if (el) ...` finding nothing yet).
/// Never showing a page before its real data is in hand removes the race
/// by construction instead of trying to win it.
pub fn assemble_panes(
    packages_html: &str,
    classes_html: &str,
    categories_html: &str,
    methods_html: &str,
    source_html: &str,
) -> String {
    format!(
        "<div class=\"st-browser\" id=\"macvm-browser\">\
         <div class=\"st-browser-lists\">{packages_html}{classes_html}{categories_html}{methods_html}</div>\
         {source_html}\
         </div>"
    )
}

/// All five panes together, for tests and for any future caller that
/// already has a `&MockWorld` in hand and just wants one full render — no
/// production call site right now (`main.rs` builds from real
/// `VmResponse::BrowserOpened` data via `assemble_panes` directly instead).
#[allow(dead_code)]
pub fn render_full_browser(world: &MockWorld, sel: &BrowserSelection) -> String {
    assemble_panes(
        &render_packages_pane(world, sel),
        &render_classes_pane(world, sel),
        &render_categories_pane(world, sel),
        &render_methods_pane(world, sel),
        &render_source_pane(world, sel),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packages_pane_lists_all_packages_none_active_by_default() {
        let world = MockWorld::seed();
        let sel = BrowserSelection::default();
        let html = render_packages_pane(&world, &sel);
        assert!(html.contains("data-browser-package=\"Kernel\""), "{html}");
        assert!(
            html.contains("data-browser-package=\"Collections\""),
            "{html}"
        );
        assert!(!html.contains("active"), "{html}");
    }

    #[test]
    fn selected_package_gets_active_class() {
        let world = MockWorld::seed();
        let sel = BrowserSelection {
            package: Some("Kernel".into()),
            ..Default::default()
        };
        let html = render_packages_pane(&world, &sel);
        assert!(
            html.contains("st-browser-item active\" data-browser-package=\"Kernel\""),
            "{html}"
        );
    }

    #[test]
    fn classes_pane_indents_by_hierarchy_depth() {
        let world = MockWorld::seed();
        let sel = BrowserSelection {
            package: Some("Collections".into()),
            ..Default::default()
        };
        let html = render_classes_pane(&world, &sel);
        // Collection is a root within "Collections" (Object lives in Kernel);
        // OrderedCollection and Dictionary are one level deeper.
        assert!(html.contains("data-browser-class=\"Collection\""), "{html}");
        let collection_depth_0 = html.contains("padding-left: 8px\">Collection<");
        let ordered_depth_1 = html.contains("padding-left: 22px\">OrderedCollection<");
        assert!(collection_depth_0, "{html}");
        assert!(ordered_depth_1, "{html}");
    }

    #[test]
    fn categories_pane_empty_until_class_selected() {
        let world = MockWorld::seed();
        let sel = BrowserSelection::default();
        let html = render_categories_pane(&world, &sel);
        assert!(!html.contains("data-browser-category"), "{html}");
    }

    #[test]
    fn categories_pane_shows_instance_side_categories_by_default() {
        let world = MockWorld::seed();
        let sel = BrowserSelection {
            class: Some("Object".into()),
            ..Default::default()
        };
        let html = render_categories_pane(&world, &sel);
        assert!(
            html.contains("data-browser-category=\"printing\""),
            "{html}"
        );
        assert!(
            !html.contains("data-browser-category=\"instance creation\""),
            "{html}"
        );
    }

    #[test]
    fn methods_pane_filters_by_selected_category() {
        let world = MockWorld::seed();
        let sel = BrowserSelection {
            class: Some("Object".into()),
            category: Some("comparing".into()),
            ..Default::default()
        };
        let html = render_methods_pane(&world, &sel);
        assert!(html.contains("data-browser-method=\"=\""), "{html}");
        assert!(html.contains("data-browser-method=\"hash\""), "{html}");
        assert!(
            !html.contains("data-browser-method=\"printString\""),
            "{html}"
        );
    }

    #[test]
    fn source_pane_shows_class_comment_with_no_method_selected() {
        let world = MockWorld::seed();
        let sel = BrowserSelection {
            class: Some("Object".into()),
            ..Default::default()
        };
        let html = render_source_pane(&world, &sel);
        assert!(html.contains("root of the class hierarchy"), "{html}");
    }

    #[test]
    fn source_pane_shows_method_source_when_method_selected() {
        let world = MockWorld::seed();
        let sel = BrowserSelection {
            class: Some("Object".into()),
            category: Some("printing".into()),
            method: Some("printString".into()),
            edit_target: SourceEditTarget::Method,
            ..Default::default()
        };
        let html = render_source_pane(&world, &sel);
        assert!(
            html.contains("printString\n\t^String streamContents"),
            "{html}"
        );
    }

    #[test]
    fn source_pane_stamps_a_save_snapshot_matching_the_selection() {
        let world = MockWorld::seed();
        let sel = BrowserSelection {
            package: Some("Kernel".into()),
            class: Some("Object".into()),
            side: Side::Instance,
            category: Some("printing".into()),
            method: Some("printString".into()),
            edit_target: SourceEditTarget::Method,
        };
        let html = render_source_pane(&world, &sel);
        assert!(html.contains("data-save-package=\"Kernel\""), "{html}");
        assert!(html.contains("data-save-class=\"Object\""), "{html}");
        assert!(html.contains("data-save-side=\"instance\""), "{html}");
        assert!(html.contains("data-save-category=\"printing\""), "{html}");
        assert!(html.contains("data-save-method=\"printString\""), "{html}");
        assert!(html.contains("data-save-target=\"method\""), "{html}");
    }

    #[test]
    fn save_snapshot_round_trips_through_from_wire() {
        let sel = BrowserSelection {
            package: Some("Kernel".into()),
            class: Some("Object".into()),
            side: Side::Class,
            category: Some("instance creation".into()),
            method: Some("new".into()),
            edit_target: SourceEditTarget::Method,
        };
        let html = render_source_pane(&MockWorld::seed(), &sel);
        // Pull each attribute back out the same way smtk.js reads them,
        // rather than re-deriving the wire strings by hand — a genuine
        // round trip through what render_source_pane actually emits.
        let get = |attr: &str| -> String {
            let needle = format!("data-save-{attr}=\"");
            let start = html.find(&needle).unwrap() + needle.len();
            html[start..].split('"').next().unwrap().to_string()
        };
        let rebuilt = BrowserSelection::from_wire(
            &get("package"),
            &get("class"),
            &get("side"),
            &get("category"),
            &get("method"),
            &get("target"),
        );
        assert_eq!(rebuilt, sel);
    }

    #[test]
    fn from_wire_treats_empty_strings_as_none() {
        let sel = BrowserSelection::from_wire("", "", "instance", "", "", "comment");
        assert_eq!(sel, BrowserSelection::default());
    }

    #[test]
    fn full_browser_render_includes_every_pane_id() {
        let world = MockWorld::seed();
        let sel = BrowserSelection::default();
        let html = render_full_browser(&world, &sel);
        for id in [
            "macvm-browser-packages",
            "macvm-browser-classes",
            "macvm-browser-categories",
            "macvm-browser-methods",
            "macvm-browser-source",
        ] {
            assert!(html.contains(id), "missing {id} in {html}");
        }
    }

    #[test]
    fn packages_pane_offers_new_class_button() {
        let world = MockWorld::seed();
        let html = render_packages_pane(&world, &BrowserSelection::default());
        assert!(html.contains("data-browser-action=\"new-class\""), "{html}");
    }

    #[test]
    fn classes_pane_offers_remove_only_when_a_class_is_selected() {
        let world = MockWorld::seed();
        let none = render_classes_pane(&world, &BrowserSelection::default());
        assert!(
            !none.contains("data-browser-action=\"remove-class\""),
            "{none}"
        );

        let sel = BrowserSelection {
            package: Some("Kernel".into()),
            class: Some("Object".into()),
            ..Default::default()
        };
        let html = render_classes_pane(&world, &sel);
        assert!(
            html.contains("data-browser-action=\"remove-class\""),
            "{html}"
        );
        // Object has subclasses in the seed data — the confirm strip should
        // warn about them before the store unconditionally removes it.
        assert!(html.contains("subclasses"), "{html}");
        assert!(html.contains("Collection"), "{html}");
    }

    #[test]
    fn methods_pane_offers_new_always_and_remove_only_when_a_method_is_selected() {
        let world = MockWorld::seed();
        let sel = BrowserSelection {
            class: Some("Object".into()),
            category: Some("printing".into()),
            ..Default::default()
        };
        let html = render_methods_pane(&world, &sel);
        assert!(
            html.contains("data-browser-action=\"new-method\""),
            "{html}"
        );
        assert!(
            !html.contains("data-browser-action=\"remove-method\""),
            "{html}"
        );

        let sel_with_method = BrowserSelection {
            method: Some("printString".into()),
            ..sel
        };
        let html = render_methods_pane(&world, &sel_with_method);
        assert!(
            html.contains("data-browser-action=\"remove-method\""),
            "{html}"
        );
    }

    #[test]
    fn methods_pane_shows_every_category_when_none_is_selected() {
        let world = MockWorld::seed();
        // Object's instance side spans "printing" and "comparing" —
        // selecting no category ("(all)") must show methods from both,
        // not just one.
        let sel = BrowserSelection {
            class: Some("Object".into()),
            ..Default::default()
        };
        let html = render_methods_pane(&world, &sel);
        assert!(
            html.contains("data-browser-method=\"printString\""),
            "{html}"
        );
        assert!(html.contains("data-browser-method=\"hash\""), "{html}");
    }

    #[test]
    fn methods_pane_offers_new_method_button_for_a_fresh_methodless_class() {
        let mut world = MockWorld::empty();
        world.add_class("Empty", None, "Test", "", "", "");
        // No methods at all yet means no categories exist to pick from —
        // the pane must still render (not the early-return empty div) and
        // still offer "+ New Method", or there'd be no way to add the
        // class's first method.
        let sel = BrowserSelection {
            class: Some("Empty".into()),
            ..Default::default()
        };
        let html = render_methods_pane(&world, &sel);
        assert!(
            html.contains("data-browser-action=\"new-method\""),
            "{html}"
        );
    }

    #[test]
    fn class_definition_shows_instance_and_class_variables() {
        // The image half of the dual placement: the definition view must show
        // BOTH the |instance vars| line and the <classVars: …> pragma (the
        // latter was previously omitted), as round-trippable .mst.
        let def = render_class_header_template(Some("Object"), "Widget", "x y", "Registry Count");
        assert!(def.contains("|x y|"), "instance vars: {def}");
        assert!(
            def.contains("<classVars: Registry Count>"),
            "class vars: {def}"
        );
        // No class vars → no empty pragma (would read as a mistake).
        let bare = render_class_header_template(Some("Object"), "Bare", "", "");
        assert!(!bare.contains("classVars"), "{bare}");
    }

    #[test]
    fn categories_pane_shows_variables_with_a_side_aware_add_field() {
        let mut world = MockWorld::empty();
        world.add_class("Widget", Some("Object"), "Test", "", "x y", "Registry");

        // Instance side: instance vars + an instance-kind add field.
        let sel = BrowserSelection {
            class: Some("Widget".into()),
            side: Side::Instance,
            ..Default::default()
        };
        let html = render_categories_pane(&world, &sel);
        assert!(html.contains("instance variables"), "label: {html}");
        assert!(html.contains(">x</div>") && html.contains(">y</div>"), "ivars shown: {html}");
        assert!(
            html.contains("st-smappl-new-var-src") && html.contains("data-var-kind=\"instance\""),
            "instance add field: {html}"
        );

        // Class side: class vars + a class-kind add field.
        let sel_c = BrowserSelection {
            class: Some("Widget".into()),
            side: Side::Class,
            ..Default::default()
        };
        let html_c = render_categories_pane(&world, &sel_c);
        assert!(
            html_c.contains("class variables") && html_c.contains(">Registry</div>"),
            "class vars: {html_c}"
        );
        assert!(html_c.contains("data-var-kind=\"class\""), "class add field: {html_c}");

        // Image-based: a mock add shows IMMEDIATELY (no restart).
        world.add_var("Widget", false, "z");
        let html2 = render_categories_pane(&world, &sel);
        assert!(html2.contains(">z</div>"), "added ivar shows at once: {html2}");
    }

    #[test]
    fn all_pseudo_category_is_active_by_default_and_reachable_by_empty_name() {
        let world = MockWorld::seed();
        let sel = BrowserSelection {
            class: Some("Object".into()),
            ..Default::default()
        };
        let html = render_categories_pane(&world, &sel);
        assert!(
            html.contains("st-browser-item active\" data-browser-category=\"\""),
            "{html}"
        );
    }

    #[test]
    fn source_pane_shows_new_method_template() {
        let world = MockWorld::seed();
        let sel = BrowserSelection {
            class: Some("Object".into()),
            category: Some("printing".into()),
            edit_target: SourceEditTarget::NewMethod,
            ..Default::default()
        };
        let html = render_source_pane(&world, &sel);
        assert!(html.contains("messageSelector"), "{html}");
        // Creation modes show no comment/definition tabs.
        assert!(!html.contains("st-definition-tabs"), "{html}");
    }

    #[test]
    fn source_pane_shows_new_class_template_prefilling_selected_class_as_superclass() {
        let world = MockWorld::seed();
        let sel = BrowserSelection {
            class: Some("Collection".into()),
            edit_target: SourceEditTarget::NewClass,
            ..Default::default()
        };
        let html = render_source_pane(&world, &sel);
        assert!(html.contains("Collection subclass: NewClass"), "{html}");
    }

    #[test]
    fn source_pane_shows_class_definition_with_real_mst_syntax() {
        let world = MockWorld::seed();
        let sel = BrowserSelection {
            class: Some("Object".into()),
            edit_target: SourceEditTarget::ClassDefinition,
            ..Default::default()
        };
        let html = render_source_pane(&world, &sel);
        // Object has no superclass — the template must spell that as the
        // real `.mst` grammar does ("nil subclass: ..."), not e.g. omit it.
        assert!(html.contains("nil subclass: Object"), "{html}");
        assert!(html.contains("st-definition-tabs"), "{html}");
        assert!(
            html.contains("st-browser-side-tab active\" data-browser-action=\"edit-definition\""),
            "{html}"
        );
    }

    #[test]
    fn definition_tab_omits_the_ivars_line_when_there_are_none() {
        let world = MockWorld::seed();
        let sel = BrowserSelection {
            class: Some("Collection".into()),
            edit_target: SourceEditTarget::ClassDefinition,
            ..Default::default()
        };
        let html = render_source_pane(&world, &sel);
        // The seed's Collection has no ivars recorded — the |...| line
        // should be omitted entirely rather than rendered as an empty "||".
        assert!(!html.contains('|'), "{html}");
    }

    #[test]
    fn definition_tab_shows_real_ivars_when_present() {
        let mut world = MockWorld::empty();
        world.add_class("Point", Some("Object"), "Kernel", "An x/y pair.", "x y", "");
        let sel = BrowserSelection {
            class: Some("Point".into()),
            edit_target: SourceEditTarget::ClassDefinition,
            ..Default::default()
        };
        let html = render_source_pane(&world, &sel);
        assert!(html.contains("|x y|"), "{html}");
        assert!(html.contains("Object subclass: Point"), "{html}");
    }
}
