//! The D-G4 HTML translator (`../PLAN.md` §3): rewrites the two Strongtalk
//! HTML extensions in memory, at load time, so the original `.html` files
//! under `reference/` stay byte-identical as a test corpus. Also injects the
//! in-page toolbar/status-bar chrome (D-G1: those are HTML inside the web
//! view, not native controls) and the `strongtalk.css`/`smtk.js` links.
//!
//! Scope note: ordinary internal cross-page links (`<a href="progenv2.html">`)
//! are deliberately *not* rewritten here — `smtk.js` intercepts those
//! generically at click time (any relative `.html` href), so this module
//! only has to handle the two attributes that don't already mean anything
//! to a browser: `doit=` and the unclosed `<smappl>` tag.

use std::path::Path;

/// The visual themes the shell's native Theme menu can switch between
/// (`main.rs::build_theme_menu`). Every theme reuses the exact same class
/// names (`.st-toolbar`, `.st-raised`, `.st-lowered`, `.st-transcript`,
/// `.st-statusbar`, `.st-field`, `.smappl`, `.st-outliner`, …) — see
/// `assets/hidef.css`'s own doc comment — so switching is just "load a
/// different stylesheet and a different icon set," never a template change.
/// `Classic` is the one genuinely different structural design (its own
/// PNG icon set, Win95 bevels); every other non-`HiDef` theme is a color/
/// font reskin of `hidef.css`'s exact structure, reusing its SVG icon set
/// (`assets/icons-hidef/`) — for the CRT/monochrome themes, recolored via
/// a CSS `filter` in that theme's own stylesheet rather than needing
/// dedicated icon assets (see e.g. `assets/crt-amber.css`'s own comment).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Theme {
    /// The period-accurate Strongtalk (1996) look — `../PLAN.md` §2.
    Classic,
    /// A modern flat theme: system fonts, vector icons, no bevels — added
    /// because the pixel-art/Win95 chrome is true to the original but reads
    /// as dated for extended use; same layout and semantics, just restyled.
    HiDef,
    /// Modern dark mode — `assets/dark.css`.
    Dark,
    /// 1970s-80s amber-phosphor terminal — `assets/crt-amber.css`.
    CrtAmber,
    /// 1970s-80s green-phosphor terminal — `assets/crt-green.css`.
    CrtGreen,
    /// Squeak's playful, colorful 1996+ Morphic look — `assets/squeak.css`.
    Squeak,
    /// Xerox Alto/Star, the monochrome bitmap-display era Smalltalk itself
    /// was born on (1970s-early 80s) — `assets/alto-mono.css`.
    AltoMono,
}

impl Theme {
    /// Every variant, in native Theme-menu display order — the single
    /// source of truth `main.rs::build_theme_menu` walks to build the menu
    /// and the checkmark list, so adding a theme never means updating two
    /// separate lists by hand.
    pub const ALL: [Theme; 7] = [
        Theme::Classic,
        Theme::HiDef,
        Theme::Dark,
        Theme::CrtAmber,
        Theme::CrtGreen,
        Theme::Squeak,
        Theme::AltoMono,
    ];

    /// Parse a CLI theme name (case-insensitive, dashes optional) — used by
    /// the headless `macvm-gui render` "eyes" command.
    pub fn from_cli_name(name: &str) -> Option<Theme> {
        let key: String = name.chars().filter(|c| *c != '-' && *c != '_').flat_map(|c| c.to_lowercase()).collect();
        Theme::ALL.into_iter().find(|t| {
            let label: String = t.menu_label().chars().filter(|c| *c != '-' && *c != ' ').flat_map(|c| c.to_lowercase()).collect();
            label == key
        })
    }

    /// Absolute path to this theme's stylesheet file (for inlining it into a
    /// self-contained headless render).
    pub fn stylesheet_path(self) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(self.stylesheet_relative_path())
    }

    /// Native Theme-menu label.
    pub fn menu_label(self) -> &'static str {
        match self {
            Theme::Classic => "Classic",
            Theme::HiDef => "Hi-Def",
            Theme::Dark => "Dark",
            Theme::CrtAmber => "CRT Amber",
            Theme::CrtGreen => "CRT Green",
            Theme::Squeak => "Squeak Morphic",
            Theme::AltoMono => "Alto Mono",
        }
    }

    fn stylesheet_relative_path(self) -> &'static str {
        match self {
            Theme::Classic => "assets/strongtalk.css",
            Theme::HiDef => "assets/hidef.css",
            Theme::Dark => "assets/dark.css",
            Theme::CrtAmber => "assets/crt-amber.css",
            Theme::CrtGreen => "assets/crt-green.css",
            Theme::Squeak => "assets/squeak.css",
            Theme::AltoMono => "assets/alto-mono.css",
        }
    }

    fn icon_url(self, icon: &str) -> String {
        match self {
            Theme::Classic => gui_file_url(&format!("reference/icons-png/{icon}.png")),
            _ => gui_file_url(&format!("assets/icons-hidef/{icon}.svg")),
        }
    }
}

/// Resolve the `data-icon="resources/NAME.bmp"` attributes a smappl `Button
/// withImage:` fragment carries (`world/33_smappl.mst`) into a themed icon:
/// inserts a `src` pointing at the active theme's asset for NAME. Keyed by
/// *logical name*, not the literal path (`gui/smappl.md` §5), so the corpus
/// text stays byte-identical (D-G4) while what actually loads follows the
/// theme. A path that doesn't reduce to a known cataloged name is left
/// as-is (no `src`) — the fall-through the doc describes.
pub fn rewrite_smappl_icons(html: &str, theme: Theme) -> String {
    const NEEDLE: &str = "data-icon=\"";
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    loop {
        let Some(pos) = rest.find(NEEDLE) else {
            out.push_str(rest);
            break;
        };
        let val_start = pos + NEEDLE.len();
        let Some(q) = rest[val_start..].find('"') else {
            out.push_str(rest);
            break;
        };
        let path = &rest[val_start..val_start + q];
        let stripped = path.strip_prefix("resources/").unwrap_or(path);
        let name = stripped.strip_suffix(".bmp").unwrap_or(stripped);
        out.push_str(&rest[..pos]);
        if !name.is_empty() {
            out.push_str(&format!("src=\"{}\" ", theme.icon_url(name)));
        }
        out.push_str(&rest[pos..val_start + q + 1]); // keep the data-icon="..." attr
        rest = &rest[val_start + q + 1..];
    }
    out
}

/// Reverse of [`html_escape_attr`] — turn an escaped `data-code` attribute
/// value back into the raw Smalltalk source. `&amp;` is undone last so a
/// literal `&lt;` in the source round-trips correctly.
fn unescape_attr(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Resolve every smappl placeholder span (from [`rewrite_smappl_placeholders`])
/// in `html` to its rendered widget fragment, the way the live GUI does
/// asynchronously — but synchronously, for the headless renderer (`macvm-gui
/// render`, the offline "eyes" on a page). `render(code)` evaluates one
/// `visual=` expression (`VmHandle::render_fragment`); `None` (an unbuildable
/// shape) leaves the placeholder box in place, exactly the live fallback.
/// Icon `data-icon` names are themed via [`rewrite_smappl_icons`].
pub fn resolve_smappl_spans(
    html: &str,
    theme: Theme,
    mut render: impl FnMut(&str) -> Option<String>,
) -> String {
    const OPEN: &str = "<span class=\"smappl\" data-widget-id=\"";
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    loop {
        let Some(pos) = rest.find(OPEN) else {
            out.push_str(rest);
            break;
        };
        let Some(tag_end_rel) = rest[pos..].find('>') else {
            out.push_str(rest);
            break;
        };
        let after_open = pos + tag_end_rel + 1;
        let Some(close_rel) = rest[after_open..].find("</span>") else {
            out.push_str(rest);
            break;
        };
        let span_end = after_open + close_rel + "</span>".len();
        let opening_tag = &rest[pos..after_open];

        out.push_str(&rest[..pos]);
        let code = attr_value(opening_tag, "data-code").as_deref().map(unescape_attr);
        let replaced = match code.as_deref().and_then(&mut render) {
            Some(frag) => rewrite_smappl_icons(&frag, theme),
            None => rest[pos..span_end].to_string(), // leave the placeholder box
        };
        out.push_str(&replaced);
        rest = &rest[span_end..];
    }
    out
}

/// Fill the empty `<pre class="st-code" data-src-class data-src-side
/// data-src-sel></pre>` placeholders a `ClassOutliner` fragment emits
/// (`world/34_tools.mst`) with method source. The running VM keeps no source
/// — it lives in image_store — so the worker (and the headless renderer)
/// enrich the VM-rendered fragment here. `source_of(class, side, selector)`
/// returns the text (or `None`, leaving the block blank). Reused by both call
/// sites so they can't drift.
pub fn inject_method_sources(
    html: &str,
    mut source_of: impl FnMut(&str, &str, &str) -> Option<String>,
) -> String {
    const OPEN: &str = "<pre class=\"st-code\"";
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    loop {
        let Some(pos) = rest.find(OPEN) else {
            out.push_str(rest);
            break;
        };
        let Some(gt) = rest[pos..].find('>') else {
            out.push_str(rest);
            break;
        };
        let tag_end = pos + gt + 1;
        let tag = rest[pos..tag_end].to_string();
        if rest[tag_end..].find("</pre>").is_none() {
            out.push_str(rest);
            break;
        }
        // Emit up to and including the opening tag, then the resolved source
        // as its content; the original (empty) body and the `</pre>` are left
        // for the next iteration / final push.
        out.push_str(&rest[..tag_end]);
        if let (Some(c), Some(s), Some(sel)) = (
            attr_value(&tag, "data-src-class"),
            attr_value(&tag, "data-src-side"),
            attr_value(&tag, "data-src-sel"),
        ) {
            if let Some(src) = source_of(&unescape_attr(&c), &s, &unescape_attr(&sel)) {
                out.push_str(&html_escape_text(src.trim_end()));
            }
        }
        rest = &rest[tag_end..];
    }
    out
}

/// Value of `name="..."` within a single tag, or `None` if absent.
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let len = tag[start..].find('"')?;
    Some(tag[start..start + len].to_string())
}

/// Absolute `file://` URL for a path under the `gui/` crate root
/// (`CARGO_MANIFEST_DIR`, baked in at build time — fine for a dev/test
/// shell run via `cargo run`; revisit if this ever ships as an app bundle).
fn gui_file_url(relative: &str) -> String {
    let root = env!("CARGO_MANIFEST_DIR");
    format!("file://{root}/{relative}")
}

/// Rewrite `<a doit="CODE">` → `<a class="doit" href="javascript:void(0)"
/// data-code="ESCAPED">`, preserving the link's own text content and
/// everything else about the tag (in the corpus, `doit=` is always the only
/// attribute, so there's nothing else to carry over). Handles multi-line
/// `doit="..."` values (none observed in the corpus, but the scan is the
/// same either way).
fn rewrite_doit_links(html: &str) -> String {
    let needle = "<a doit=\"";
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    loop {
        let Some(tag_start) = rest.find(needle) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..tag_start]);
        let code_start = tag_start + needle.len();
        let Some(code_end_rel) = rest[code_start..].find('"') else {
            out.push_str(&rest[tag_start..]);
            break;
        };
        let code = &rest[code_start..code_start + code_end_rel];
        out.push_str(&format!(
            "<a class=\"doit\" href=\"javascript:void(0)\" data-code=\"{}\"",
            html_escape_attr(code)
        ));
        // Leave the tag's closing `>` (and everything after) for the next
        // iteration's plain copy — mirrors the closing-`>` handling in
        // rewrite_smappl_placeholders below.
        rest = &rest[code_start + code_end_rel + 1..];
    }
    out
}

/// Rewrite `<smappl visual="CODE">` (no closing tag in the corpus — see
/// this module's doc comment) → a placeholder `<span>` carrying the code and
/// a stable `data-widget-id`. The span starts as the G0 lowered-bevel box
/// naming its code; on page load `smtk.js` posts a `{kind:"smappl"}` message
/// per span, the VM worker renders the `visual=` expression to an HTML
/// fragment (`VmHandle::render_fragment`, D-G5), and `main.rs` swaps the
/// span's `outerHTML` for it — so the box is only what the user sees for the
/// instant before the live widget arrives, or permanently if the shape isn't
/// buildable yet (the render fails and the box stays).
fn rewrite_smappl_placeholders(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    let mut widget_seq = 0usize;
    loop {
        let Some(tag_start) = rest.find("<smappl") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..tag_start]);
        let after_tag_name = &rest[tag_start..];
        let Some(attr_start) = after_tag_name.find("visual=\"") else {
            // Malformed/unexpected shape — leave it alone rather than guess.
            out.push_str("<smappl");
            rest = &after_tag_name[7..];
            continue;
        };
        let code_start = attr_start + "visual=\"".len();
        let Some(code_end_rel) = after_tag_name[code_start..].find('"') else {
            out.push_str(&after_tag_name[..code_start]);
            rest = &after_tag_name[code_start..];
            continue;
        };
        let code = &after_tag_name[code_start..code_start + code_end_rel];
        let after_close_quote = &after_tag_name[code_start + code_end_rel + 1..];
        let Some(tag_end_rel) = after_close_quote.find('>') else {
            // No closing `>` found — bail out on the rest untouched.
            out.push_str(&after_tag_name[..code_start + code_end_rel + 1]);
            rest = after_close_quote;
            continue;
        };

        // Collapsed for both the attribute and the visible text: this is a
        // G0 placeholder only (D-G5's real server-rendered fragment lands
        // in G2), so raw source whitespace fidelity doesn't matter, and a
        // single-line attribute value is simpler to read/debug either way.
        let collapsed = code.split_whitespace().collect::<Vec<_>>().join(" ");
        out.push_str(&format!(
            "<span class=\"smappl\" data-widget-id=\"s{}\" data-code=\"{}\">{}</span>",
            widget_seq,
            html_escape_attr(&collapsed),
            html_escape_text(&collapsed)
        ));
        widget_seq += 1;
        rest = &after_close_quote[tag_end_rel + 1..];
    }
    out
}

fn html_escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn html_escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The nine Launcher toolbar buttons (`../PLAN.md` §1) plus back/forward/home
/// navigation (§G0: "back/forward/home navigation works" — the three icons
/// exist in the set but aren't part of the documented nine, so they're
/// placed first as a judgment call, not a source-anchored fact; see
/// `../PLAN.md` §5 if this needs revisiting).
const TOOLBAR_BUTTONS: &[(&str, &str, &str)] = &[
    ("goBack", "goBack", "Back"),
    ("goForward", "goForward", "Forward"),
    ("home", "home", "Start page"),
    ("open", "find", "Find definition"),
    ("implementors", "implementors", "Implementors"),
    ("senders", "senders", "Senders"),
    ("userHierarchy", "userHierarchy", "User hierarchy"),
    ("hierarchy", "hierarchy", "Full hierarchy"),
    ("blankSheet", "workspace", "Workspace"),
    ("texteditor", "editor", "Text editor"),
    ("documentation", "documentation", "Documentation"),
    ("abstract", "canvas", "Canvas"),
    // G5 polish items (`../PLAN.md` §4 G5) — not part of the documented
    // nine either, same judgment call as goBack/goForward/home above.
    ("refresh", "refresh", "Refresh"),
    ("smallerText", "smallerText", "Smaller text"),
    ("biggerText", "biggerText", "Bigger text"),
];

fn toolbar_html(theme: Theme) -> String {
    let buttons: String = TOOLBAR_BUTTONS
        .iter()
        .map(|(icon, action, title)| {
            let icon_url = theme.icon_url(icon);
            format!(
                "<button class=\"st-toolbtn\" data-action=\"{action}\" title=\"{title}\">\
                 <img src=\"{icon_url}\" alt=\"{title}\"></button>"
            )
        })
        .collect();
    format!("<div class=\"st-toolbar st-raised\" id=\"macvm-toolbar\">{buttons}</div>")
}

/// `transcript` is the persisted history (`main.rs`'s `TRANSCRIPT`, appended
/// to by `append_transcript` on every `VmResponse::Transcript`) — every
/// freshly-built page starts the transcript pane showing real history
/// instead of always resetting to a blank "Ready." greeting, which used to
/// make any navigation (even just a theme switch) silently erase it.
fn statusbar_html(transcript: &str) -> String {
    format!(
        "<div class=\"st-transcript st-lowered\" id=\"macvm-transcript\" style=\"height: 72px\">{}</div>\
         <div class=\"st-statusbar st-raised\"><span class=\"st-field\" id=\"macvm-status\">Ready</span></div>",
        html_escape_text(transcript)
    )
}

/// A `<base href>` for `dir` (with the trailing slash `<base>` needs to be
/// treated as a directory, not a file). Required because the rendered HTML
/// is loaded from `gui/.rendered/current.html` (see `main.rs::navigate_to`
/// for why: `loadFileURL:allowingReadAccessToURL:` needs a real file to
/// load, and a single wide read-access grant is much simpler than one
/// scoped per page directory) — without this, the *page's own* relative
/// links/images would resolve against `.rendered/` instead of wherever the
/// original file actually lives.
fn base_href_tag(dir: &Path) -> String {
    format!("<base href=\"file://{}/\">", dir.display())
}

/// The `zoom` CSS property (non-standard, but WebKit implements it fully —
/// this shell only ever runs inside WKWebView) is the simplest way to scale
/// a whole rendered page uniformly: text, images, and layout together, like
/// a browser's page zoom. That's exactly what the G5 `biggerText`/
/// `smallerText` toolbar buttons are for (`../PLAN.md` §4 G5), so there's no
/// need to touch `strongtalk.css`/`hidef.css`'s own fixed-px sizes at all —
/// one `<style>` override here covers both themes identically.
fn font_scale_style(font_scale_percent: u32) -> String {
    format!("<style>body {{ zoom: {font_scale_percent}%; }}</style>")
}

fn chrome_head_extra(original_dir: &Path, theme: Theme, font_scale_percent: u32) -> String {
    // The `<meta charset>` matters even though the original corpus files
    // don't declare one: none of them use non-ASCII bytes, so the gap was
    // silent there, but this module's own injected/authored HTML (e.g. the
    // documentation pages) uses UTF-8 punctuation (em dashes, arrows), and
    // without an explicit charset WKWebView guessed a Latin-1-ish encoding
    // and mangled it. Declaring UTF-8 here covers every loaded page.
    format!(
        "<meta charset=\"utf-8\">\n{}\n<link rel=\"stylesheet\" href=\"{}\">\n<script src=\"{}\"></script>\n{}",
        base_href_tag(original_dir),
        gui_file_url(theme.stylesheet_relative_path()),
        gui_file_url("assets/smtk.js"),
        font_scale_style(font_scale_percent),
    )
}

/// Insert `extra` right before the first `</head>` (or, failing that,
/// right before `<body`, for a page with no explicit head section).
fn inject_before_head_close(html: &str, extra: &str) -> String {
    if let Some(pos) = html.to_ascii_lowercase().find("</head>") {
        format!("{}{}{}", &html[..pos], extra, &html[pos..])
    } else if let Some(pos) = html.to_ascii_lowercase().find("<body") {
        format!("{}{}{}", &html[..pos], extra, &html[pos..])
    } else {
        format!("{extra}{html}")
    }
}

/// Insert `extra` right after the opening `<body...>` tag's `>`.
fn inject_after_body_open(html: &str, extra: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let Some(body_start) = lower.find("<body") else {
        return format!("{extra}{html}");
    };
    let Some(tag_close_rel) = html[body_start..].find('>') else {
        return format!("{extra}{html}");
    };
    let insert_at = body_start + tag_close_rel + 1;
    format!("{}{}{}", &html[..insert_at], extra, &html[insert_at..])
}

/// Insert `extra` right before `</body>`.
fn inject_before_body_close(html: &str, extra: &str) -> String {
    if let Some(pos) = html.to_ascii_lowercase().find("</body>") {
        format!("{}{}{}", &html[..pos], extra, &html[pos..])
    } else {
        format!("{html}{extra}")
    }
}

/// Load `path`, apply the full D-G4 transform plus chrome injection, and
/// return the resulting HTML string ready to be written to a scratch file
/// (`gui/.rendered/current.html`, see `main.rs::navigate_to`) and loaded via
/// `loadFileURL:allowingReadAccessToURL:`. Since the loaded file's own
/// location is then `.rendered/`, not `path`'s directory, an injected
/// `<base href="file://{path's parent}/">` (see `base_href_tag`) is what
/// keeps the page's *own* relative links/images resolving exactly as they
/// do for the original file — the chrome this function injects uses
/// absolute `file://` URLs regardless (see `gui_file_url`), so it isn't
/// affected either way.
pub fn load_and_preprocess(
    path: &Path,
    theme: Theme,
    font_scale_percent: u32,
    transcript: &str,
) -> std::io::Result<String> {
    let raw = std::fs::read_to_string(path)?;
    let original_dir = path.parent().unwrap_or_else(|| Path::new("."));
    Ok(preprocess_html(
        raw,
        original_dir,
        theme,
        font_scale_percent,
        transcript,
    ))
}

/// The chrome-injection half of [`load_and_preprocess`], factored out so
/// Rust-generated pages (not loaded from a corpus file — e.g. the class
/// browser view, `browser_render.rs`) get the exact same toolbar/status
/// bar/theme/font-scale treatment as any corpus page, via
/// [`render_generated_page`].
fn preprocess_html(
    raw: String,
    original_dir: &Path,
    theme: Theme,
    font_scale_percent: u32,
    transcript: &str,
) -> String {
    let mut html = raw;
    html = rewrite_doit_links(&html);
    html = rewrite_smappl_placeholders(&html);
    html = inject_before_head_close(
        &html,
        &chrome_head_extra(original_dir, theme, font_scale_percent),
    );
    html = inject_after_body_open(&html, &toolbar_html(theme));
    html = inject_before_body_close(&html, &statusbar_html(transcript));
    html
}

/// Wrap `body_content` (already-built HTML, e.g. `browser_render`'s pane
/// markup) in a minimal page skeleton and run it through the same D-G4
/// preprocessing/chrome pipeline as a corpus page — for views the GUI
/// generates itself rather than loads from `reference/pages/`. `base_dir`
/// only matters if `body_content` has its own relative links/images (the
/// browser view doesn't, today); pass [`gui_root`]-relative paths there if
/// that ever changes. There's no original file whose relative links need
/// preserving, so `base_dir` doubles as the `<base href>` target directly
/// (contrast `load_and_preprocess`, where that's the corpus file's own
/// parent directory).
pub fn render_generated_page(
    title: &str,
    body_content: &str,
    base_dir: &Path,
    theme: Theme,
    font_scale_percent: u32,
    transcript: &str,
) -> String {
    let raw = format!(
        "<html><head><title>{}</title></head><body>{body_content}</body></html>",
        html_escape_text(title)
    );
    preprocess_html(raw, base_dir, theme, font_scale_percent, transcript)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doit_link_gets_class_and_data_code() {
        let input = r#"<a doit="VM collectGarbage">Collect Garbage</a>"#;
        let out = rewrite_doit_links(input);
        assert!(out.contains(r#"class="doit""#), "{out}");
        assert!(out.contains(r#"data-code="VM collectGarbage""#), "{out}");
        assert!(out.ends_with("Collect Garbage</a>"), "{out}");
    }

    #[test]
    fn smappl_multiline_becomes_placeholder_span() {
        let input = "<smappl visual=\" Button\n\t\t\twithImage: (Image fromFile: 'x')\n\t\t\taction: [ :b | ]\t\">";
        let out = rewrite_smappl_placeholders(input);
        assert!(out.starts_with(r#"<span class="smappl""#), "{out}");
        assert!(out.contains("Button withImage:"), "{out}");
        assert!(
            !out.contains('\n'),
            "collapsed whitespace should have no newlines: {out}"
        );
    }

    #[test]
    fn resolve_smappl_spans_swaps_rendered_and_keeps_placeholder_on_none() {
        // Two placeholder spans (as rewrite_smappl_placeholders emits them).
        let html = "before \
            <span class=\"smappl\" data-widget-id=\"s0\" data-code=\"Glue xRigid: 4\">Glue xRigid: 4</span> \
            mid \
            <span class=\"smappl\" data-widget-id=\"s1\" data-code=\"CodeView forString\">CodeView forString</span> \
            after";
        let out = resolve_smappl_spans(&html, Theme::Classic, |code| {
            if code.starts_with("Glue") {
                Some("<span class=\"glue\"></span>".to_string())
            } else {
                None // unbuildable → leave the placeholder
            }
        });
        assert!(out.contains("<span class=\"glue\"></span>"), "rendered span swapped in: {out}");
        assert!(
            out.contains("data-code=\"CodeView forString\""),
            "the None (unbuildable) span keeps its placeholder: {out}"
        );
        assert!(out.starts_with("before ") && out.ends_with(" after"), "surrounding text intact: {out}");
    }

    #[test]
    fn inject_method_sources_fills_empty_pre_blocks() {
        // Two selector nodes; one has source, one (a `<=` selector, escaped in
        // the attr) resolves to None and stays blank.
        let html = concat!(
            r#"<pre class="st-code" data-src-class="Point" data-src-side="instance" data-src-sel="x"></pre>"#,
            r#"<pre class="st-code" data-src-class="Point" data-src-side="instance" data-src-sel="&lt;="></pre>"#,
        );
        let out = inject_method_sources(html, |cls, side, sel| {
            assert_eq!((cls, side), ("Point", "instance"));
            match sel {
                "x" => Some("x [ ^x ]".to_string()),
                "<=" => None, // the escaped attr must reach us unescaped
                other => panic!("unexpected selector {other}"),
            }
        });
        assert!(out.contains("x [ ^x ]"), "source spliced into the block: {out}");
        // The None block stays empty.
        assert!(out.contains(r#"data-src-sel="&lt;="></pre>"#), "blank block preserved: {out}");
    }

    #[test]
    fn resolve_smappl_spans_unescapes_data_code_for_the_renderer() {
        // A code value with an escaped char must reach the renderer raw.
        let html = r#"<span class="smappl" data-widget-id="s0" data-code="a &lt; b">x</span>"#;
        let mut seen = String::new();
        let _ = resolve_smappl_spans(html, Theme::Classic, |code| {
            seen = code.to_string();
            Some(String::new())
        });
        assert_eq!(seen, "a < b", "data-code must be HTML-unescaped for the VM");
    }

    #[test]
    fn theme_from_cli_name_is_case_and_dash_insensitive() {
        assert_eq!(Theme::from_cli_name("classic"), Some(Theme::Classic));
        assert_eq!(Theme::from_cli_name("Hi-Def"), Some(Theme::HiDef));
        assert_eq!(Theme::from_cli_name("crtamber"), Some(Theme::CrtAmber));
        assert_eq!(Theme::from_cli_name("nonsense"), None);
    }

    #[test]
    fn smappl_icon_resolves_to_a_themed_src() {
        let frag = r#"<img class="smappl-icon" width="20" height="20" data-icon="resources/edit.bmp">"#;
        let classic = rewrite_smappl_icons(frag, Theme::Classic);
        assert!(
            classic.contains("src=\"") && classic.contains("reference/icons-png/edit.png"),
            "classic theme resolves the logical name to a png, got {classic}"
        );
        // The original data-icon attribute is preserved (byte-identical corpus).
        assert!(classic.contains(r#"data-icon="resources/edit.bmp""#), "{classic}");
        // A different theme picks a different asset for the same name.
        let dark = rewrite_smappl_icons(frag, Theme::Dark);
        assert!(
            dark.contains("assets/icons-hidef/edit.svg"),
            "dark theme resolves to the svg, got {dark}"
        );
    }

    #[test]
    fn smappl_without_closing_tag_does_not_swallow_following_content() {
        let input = r#"<li><smappl visual="ClassHierarchyOutliner imbeddedVisualForClass: Object"></li><li>next</li>"#;
        let out = rewrite_smappl_placeholders(input);
        assert!(out.contains("</span></li><li>next</li>"), "{out}");
    }

    #[test]
    fn plain_links_are_untouched() {
        let input = r#"<a href="documentation/index.html">Browse Documentation</a>"#;
        let out = rewrite_doit_links(&rewrite_smappl_placeholders(input));
        assert_eq!(out, input);
    }

    #[test]
    fn themes_pick_distinct_stylesheets_and_icon_formats() {
        let classic_head = chrome_head_extra(Path::new("."), Theme::Classic, 100);
        assert!(
            classic_head.contains("assets/strongtalk.css"),
            "{classic_head}"
        );
        let hidef_head = chrome_head_extra(Path::new("."), Theme::HiDef, 100);
        assert!(hidef_head.contains("assets/hidef.css"), "{hidef_head}");

        let classic_toolbar = toolbar_html(Theme::Classic);
        assert!(
            classic_toolbar.contains("reference/icons-png/home.png"),
            "{classic_toolbar}"
        );
        let hidef_toolbar = toolbar_html(Theme::HiDef);
        assert!(
            hidef_toolbar.contains("assets/icons-hidef/home.svg"),
            "{hidef_toolbar}"
        );
    }

    /// The five newer themes: each resolves to its own stylesheet, and —
    /// since only `Classic` has its own icon set — every one of them
    /// reuses HiDef's SVG icons (recolored via CSS `filter` in the CRT/
    /// monochrome themes' own stylesheets, not a different icon set).
    #[test]
    fn newer_themes_pick_their_own_stylesheet_but_share_hidef_icons() {
        let cases: [(Theme, &str); 5] = [
            (Theme::Dark, "assets/dark.css"),
            (Theme::CrtAmber, "assets/crt-amber.css"),
            (Theme::CrtGreen, "assets/crt-green.css"),
            (Theme::Squeak, "assets/squeak.css"),
            (Theme::AltoMono, "assets/alto-mono.css"),
        ];
        for (theme, expected_css) in cases {
            let head = chrome_head_extra(Path::new("."), theme, 100);
            assert!(head.contains(expected_css), "{theme:?}: {head}");
            let toolbar = toolbar_html(theme);
            assert!(
                toolbar.contains("assets/icons-hidef/home.svg"),
                "{theme:?}: {toolbar}"
            );
        }
    }

    #[test]
    fn theme_all_has_no_duplicate_stylesheets_and_matches_its_own_length() {
        assert_eq!(Theme::ALL.len(), 7);
        let mut paths: Vec<&str> = Theme::ALL
            .iter()
            .map(|t| t.stylesheet_relative_path())
            .collect();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(
            paths.len(),
            Theme::ALL.len(),
            "every theme must have a distinct stylesheet"
        );
    }

    #[test]
    fn font_scale_is_injected_as_a_zoom_style() {
        let head = chrome_head_extra(Path::new("."), Theme::Classic, 130);
        assert!(head.contains("zoom: 130%"), "{head}");
    }
}
