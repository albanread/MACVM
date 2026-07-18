//! Shared browser WRITE flows — the exact call sequences `gui/src/vm_host.rs`'s
//! handler arms perform against an [`Image`], extracted so the Cocoa GUI's
//! browser drives the SAME implementation instead of a second one (dual
//! placement covers renderers, never the write logic). Each function is thin
//! orchestration over the existing `Image` methods + `mst` parsers; all real
//! work (versioning, SQL, parsing) stays where it already lives.
//!
//! LIVE-compile is deliberately NOT here: each GUI owns its path to its
//! running VM (`vm_host::live_compile` = `vm.exec`; the Cocoa browser ships
//! the same source text as a `#doit` to its primary). The `reopen_*` builders
//! below produce that source text, shared so both compile the same thing.

use crate::mst::{parse_method_selector, parse_mst_source};
use crate::{Image, Side};

/// `vm_host`'s `is_valid_var_name`, verbatim: a Smalltalk-style identifier
/// (a letter or `_`, then letters/digits/`_`).
pub fn is_valid_var_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `vm_host`'s `reopen_one_method`, superclass supplied by the caller (read
/// from the image, exactly as the web reads it from its mirror): the .mst
/// reopen that live-compiles ONE method into an existing class.
pub fn reopen_one_method(superclass: &str, class_name: &str, method_text: &str) -> String {
    format!("{superclass} subclass: {class_name} [\n{}\n]\n", method_text.trim())
}

/// The class-variable reopen `vm_host`'s `SmapplAddVar` arm live-compiles
/// (a class var is an association, not instance shape — reopening appends it
/// cleanly and takes effect immediately).
pub fn class_var_reopen(superclass: &str, class_name: &str, var_name: &str) -> String {
    format!("{superclass} subclass: {class_name} [\n    <classVars: {var_name}>\n]")
}

/// New class from accepted `.mst` text — the `SmapplNewClass` /
/// `BrowserSaveSource::NewClass` image sequence: parse, refuse a duplicate,
/// `create_or_reopen_class`, then each parsed method via
/// `create_or_reopen_method`. Answers the new class's name.
pub fn new_class_from_source(img: &Image, text: &str) -> Result<String, String> {
    let Some(pc) = parse_mst_source(text).into_iter().next() else {
        return Err("Could not parse the class definition — check the syntax.".to_string());
    };
    if matches!(img.class_named(&pc.name), Ok(Some(_))) {
        return Err(format!("A class named {} already exists.", pc.name));
    }
    img.create_or_reopen_class(&pc.name, pc.superclass.as_deref(), "", "", &pc.instance_vars)
        .map_err(|e| e.to_string())?;
    let mut failed = 0usize;
    for m in &pc.methods {
        let side = if m.is_class_side { Side::Class } else { Side::Instance };
        if img
            .create_or_reopen_method(&pc.name, side, &m.selector, "as yet unclassified", &m.source)
            .is_err()
        {
            failed += 1;
        }
    }
    if failed > 0 {
        return Err(format!(
            "Created {}, but {failed} method(s) failed to persist.",
            pc.name
        ));
    }
    Ok(pc.name)
}

/// New-or-edited method from accepted text — the `BrowserSaveSource::NewMethod`
/// image sequence: parse the message pattern off the text, then
/// `create_or_reopen_method` (idempotent create-or-update, versioned). The
/// text may carry the in-body class-side form (`Cls class >> sel [ … ]`) —
/// the stored form class-side sources already use — so the pattern is parsed
/// from the text with any `Cls class >>` prefix stripped first. Answers the
/// selector.
pub fn save_method(
    img: &Image,
    class_name: &str,
    side: Side,
    text: &str,
) -> Result<String, String> {
    let pattern_text = text
        .trim_start()
        .strip_prefix(&format!("{class_name} class >>"))
        .map(str::trim_start)
        .unwrap_or(text);
    let Some(selector) = parse_method_selector(pattern_text) else {
        return Err("Could not parse a message pattern from the method source.".to_string());
    };
    match img.create_or_reopen_method(class_name, side, &selector, "as yet unclassified", text) {
        Ok(Some(_)) => Ok(selector),
        Ok(None) => Err(format!("No class named {class_name} in the image.")),
        Err(e) => Err(e.to_string()),
    }
}

/// Add a variable — the `SmapplAddVar` image sequence: validate the name,
/// then `reimport_class_shell` (the idempotent union into the stored shell).
/// An INSTANCE variable changes shape and so takes effect on the next boot
/// (no `become:`); a CLASS variable can additionally be made live by
/// compiling [`class_var_reopen`] — the caller's half.
pub fn add_variable(
    img: &Image,
    class_name: &str,
    is_class_var: bool,
    name: &str,
) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Enter a variable name.".to_string());
    }
    if !is_valid_var_name(name) {
        return Err(format!(
            "'{name}' is not a valid variable name (letters, digits, _)."
        ));
    }
    if !matches!(img.class_named(class_name), Ok(Some(_))) {
        return Err(format!("No class named {class_name}."));
    }
    let (iv, cv) = if is_class_var { ("", name) } else { (name, "") };
    match img.reimport_class_shell(class_name, iv, cv) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!("No class named {class_name}.")),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img() -> Image {
        let img = Image::open_in_memory().expect("in-memory image");
        img.create_or_reopen_class("Object", None, "", "", "")
            .expect("seed Object");
        img
    }

    #[test]
    fn new_class_parses_creates_and_refuses_duplicates() {
        let img = img();
        let name =
            new_class_from_source(&img, "Object subclass: Painter [\n    |brush|\n    stroke [ ^1 ]\n]")
                .expect("create");
        assert_eq!(name, "Painter");
        let cls = img.class_named("Painter").unwrap().expect("stored");
        assert_eq!(cls.instance_vars, "brush");
        assert_eq!(
            img.method_source("Painter", Side::Instance, "stroke").unwrap(),
            Some("stroke [ ^1 ]".to_string())
        );
        assert!(new_class_from_source(&img, "Object subclass: Painter [\n]").is_err());
        assert!(new_class_from_source(&img, "not a class at all").is_err());
    }

    #[test]
    fn save_method_parses_pattern_including_class_side_prefix() {
        let img = img();
        new_class_from_source(&img, "Object subclass: Painter [\n]").expect("create");
        assert_eq!(
            save_method(&img, "Painter", Side::Instance, "dab: x [ ^x ]").expect("save"),
            "dab:"
        );
        assert_eq!(
            save_method(&img, "Painter", Side::Class, "Painter class >> make [ ^self new ]")
                .expect("save class side"),
            "make"
        );
        assert!(save_method(&img, "Painter", Side::Instance, "").is_err());
        assert!(save_method(&img, "NoSuch", Side::Instance, "x [ ^1 ]").is_err());
    }

    #[test]
    fn add_variable_validates_and_unions() {
        let img = img();
        new_class_from_source(&img, "Object subclass: Painter [\n    |brush|\n]").expect("create");
        add_variable(&img, "Painter", false, "canvas").expect("add ivar");
        add_variable(&img, "Painter", true, "Palette").expect("add classvar");
        let cls = img.class_named("Painter").unwrap().expect("stored");
        assert!(cls.instance_vars.split_whitespace().any(|v| v == "canvas"));
        assert!(cls.instance_vars.split_whitespace().any(|v| v == "brush"));
        assert!(cls.class_vars.split_whitespace().any(|v| v == "Palette"));
        assert!(add_variable(&img, "Painter", false, "9bad").is_err());
        assert!(add_variable(&img, "Painter", false, "").is_err());
        assert!(add_variable(&img, "NoSuch", false, "x").is_err());
    }

    #[test]
    fn reopen_builders_match_vm_host_shapes() {
        assert_eq!(
            reopen_one_method("Object", "Painter", "dab: x [ ^x ]\n"),
            "Object subclass: Painter [\ndab: x [ ^x ]\n]\n"
        );
        assert_eq!(
            class_var_reopen("Object", "Painter", "Palette"),
            "Object subclass: Painter [\n    <classVars: Palette>\n]"
        );
    }
}
