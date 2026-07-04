//! Offline FFI binding generator (`../docs/FFI.md`, S20) — queries a sibling
//! repo's `cocoa_data/cocoa.sqlite` (never the SDK directly; never writes to
//! the database) for a curated manifest of Cocoa methods, POSIX functions,
//! and structs, and emits real `.mst` Smalltalk text carrying the AAPCS64
//! ABI shape tokens (`g`/`f`/`h1-4`/`i1-2`/`s`/`v`) `derive_method_abi.py`/
//! `derive_posix_abi.py` already computed — no `@encode` or C-declarator
//! parsing happens in this crate; that's `cocoa_data`'s job, done once,
//! shared across "every data-driven compiler in the portfolio" per its own
//! README.
//!
//! Forward-declared, not yet callable: the emitted `<primitive: FFI ...>`
//! pragma names a primitive `src/runtime/primitives.rs` doesn't register
//! yet (`docs/FFI.md` §6.3) — the real S20 sprint builds that, and the VM
//! trampoline behind it, later. This crate's job stops at "produce the
//! right declaration," same contract `world/*.mst` already keeps for a few
//! primitives earlier sprints haven't reached.

use std::fmt;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

// ─────────────────────────────────────────────────────────── DB access

/// Resolve `cocoa.sqlite`'s path — explicit override, then `COCOA_DATA_DB`,
/// then a well-known checkout location. Mirrors `cocoa-core::resolve_db_path`
/// exactly (`cocoa_data/rust/cocoa-core/src/lib.rs`) so both projects'
/// tooling finds the same database the same way, without this crate taking
/// a path dependency on that one (`docs/FFI.md` §6.2: `cocoa_data` is an
/// independently-versioned shared resource for a whole portfolio of
/// compilers, not something to couple a build to).
pub fn resolve_db_path(explicit: Option<&str>) -> Result<PathBuf, GenError> {
    if let Some(p) = explicit {
        return Ok(PathBuf::from(p));
    }
    if let Ok(p) = std::env::var("COCOA_DATA_DB") {
        return Ok(PathBuf::from(p));
    }
    let cwd_candidate = Path::new("cocoa.sqlite");
    if cwd_candidate.exists() {
        return Ok(cwd_candidate.to_path_buf());
    }
    let fallback = PathBuf::from("/Users/oberon/claudeprojects/cocoa_data/cocoa.sqlite");
    if fallback.exists() {
        return Ok(fallback);
    }
    Err(GenError::DbNotFound)
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(explicit: Option<&str>) -> Result<Self, GenError> {
        let path = resolve_db_path(explicit)?;
        let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(GenError::Sql)?;
        Ok(Self { conn })
    }

    #[cfg(test)]
    fn open_at(path: &Path) -> Result<Self, GenError> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(GenError::Sql)?;
        Ok(Self { conn })
    }

    /// `method_abi`'s `ret_class`/`arg_classes` for one `(class, selector,
    /// is_class)` — the AAPCS64 shape a compiled/trampoline call needs, not
    /// the raw `@encode` string (`docs/FFI.md` §1).
    pub fn method_abi(&self, class: &str, selector: &str, is_class: bool) -> Result<Option<Abi>, GenError> {
        self.conn
            .query_row(
                "SELECT ret_class, arg_classes FROM method_abi WHERE class = ?1 AND selector = ?2 AND is_class = ?3",
                rusqlite::params![class, selector, is_class as i64],
                |r| Ok(Abi::from_row(r.get(0)?, r.get(1)?)),
            )
            .map(Some)
            .or_else(none_if_no_rows)
    }

    pub fn posix_function_abi(&self, name: &str) -> Result<Option<Abi>, GenError> {
        self.conn
            .query_row(
                "SELECT ret_class, arg_classes FROM posix_function_abi WHERE name = ?1",
                [name],
                |r| Ok(Abi::from_row(r.get(0)?, r.get(1)?)),
            )
            .map(Some)
            .or_else(none_if_no_rows)
    }

    pub fn struct_layout(&self, name: &str) -> Result<Option<StructLayout>, GenError> {
        let head = self
            .conn
            .query_row("SELECT name, size FROM structs WHERE name = ?1", [name], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map(Some)
            .or_else(none_if_no_rows)?;
        let Some((name, size)) = head else { return Ok(None) };

        let mut stmt = self
            .conn
            .prepare("SELECT name, ty, offset FROM struct_fields WHERE struct = ?1 ORDER BY idx")
            .map_err(GenError::Sql)?;
        let fields = stmt
            .query_map([&name], |r| {
                Ok(StructField {
                    name: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    ty: r.get(1)?,
                    offset: r.get(2)?,
                })
            })
            .map_err(GenError::Sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(GenError::Sql)?;
        Ok(Some(StructLayout { name, size, fields }))
    }
}

fn none_if_no_rows<T>(e: rusqlite::Error) -> Result<Option<T>, GenError> {
    if e == rusqlite::Error::QueryReturnedNoRows {
        Ok(None)
    } else {
        Err(GenError::Sql(e))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Abi {
    pub ret_class: String,
    /// Empty for a zero-arg call, never a single empty-string element —
    /// `method_abi.arg_classes`/`posix_function_abi.arg_classes` are stored
    /// comma-joined with `''` meaning "no args" (`derive_method_abi.py`'s own
    /// doc comment), so splitting `""` needs this special case or it yields
    /// a spurious one-element `[""]`.
    pub arg_classes: Vec<String>,
}

impl Abi {
    fn from_row(ret_class: String, arg_classes: String) -> Self {
        let arg_classes = if arg_classes.is_empty() { Vec::new() } else { arg_classes.split(',').map(str::to_string).collect() };
        Self { ret_class, arg_classes }
    }

    /// `#(g f h4 ...)` — an array-of-symbols literal for the pragma.
    fn as_smalltalk_array_literal(&self) -> String {
        format!("#({})", self.arg_classes.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" "))
    }
}

#[derive(Clone, Debug)]
pub struct StructField {
    pub name: String,
    pub ty: String,
    pub offset: i64,
}

#[derive(Clone, Debug)]
pub struct StructLayout {
    pub name: String,
    pub size: i64,
    pub fields: Vec<StructField>,
}

#[derive(Debug)]
pub enum GenError {
    DbNotFound,
    Sql(rusqlite::Error),
    NotFound(String),
    UnsupportedFieldType { struct_name: String, field: String, ty: String },
}

impl fmt::Display for GenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GenError::DbNotFound => write!(f, "could not locate cocoa.sqlite (pass --db, set COCOA_DATA_DB, or run from a checkout next to it)"),
            GenError::Sql(e) => write!(f, "sqlite error: {e}"),
            GenError::NotFound(what) => write!(f, "not found in cocoa.sqlite: {what}"),
            GenError::UnsupportedFieldType { struct_name, field, ty } => {
                write!(f, "{struct_name}.{field}: no Alien accessor generated for field type {ty:?} (nested struct/array — out of scope for this worked example, see docs/FFI.md §6.2)")
            }
        }
    }
}

impl std::error::Error for GenError {}

// ───────────────────────────────────────────────────── curated manifest

/// One Cocoa method the generator should produce a named, browsable
/// Smalltalk binding for. `smalltalk_class` is the *generated* class's own
/// name — deliberately suffixed `Alien` (real Strongtalk naming, `docs/FFI.md`
/// §3) rather than colliding with a same-named real Smalltalk class.
#[derive(Clone, Debug)]
pub struct CocoaMethodSpec {
    pub objc_class: &'static str,
    pub selector: &'static str,
    pub is_class_method: bool,
    pub smalltalk_class: &'static str,
}

/// One POSIX/libc function. `smalltalk_selector` is hand-curated here (the
/// manifest layer, not derived from anything in cocoa_data) because
/// `posix_function_args` carries only positional C types, no parameter
/// names to build a keyword selector from (`docs/FFI.md` §6.1) — must have
/// exactly one keyword part per argument, checked by `generate_posix_function`.
#[derive(Clone, Debug)]
pub struct PosixFunctionSpec {
    pub c_name: &'static str,
    pub smalltalk_selector: &'static str,
}

/// A small, real, worked example — not an attempt at "the entire macOS"
/// (`docs/FFI.md` §6.1: curated is the point, Tier 2's dynamic dispatch is
/// what reaches the rest). Exercises `f`/`h4`/`g` Cocoa shapes and the
/// file-I/O POSIX surface `WORLD.md` §9 already wanted.
pub fn seed_manifest() -> (Vec<CocoaMethodSpec>, Vec<PosixFunctionSpec>, &'static [&'static str]) {
    let cocoa = vec![
        CocoaMethodSpec { objc_class: "NSColor", selector: "colorWithRed:green:blue:alpha:", is_class_method: true, smalltalk_class: "NSColorAlien" },
        CocoaMethodSpec { objc_class: "NSView", selector: "frame", is_class_method: false, smalltalk_class: "NSViewAlien" },
        CocoaMethodSpec { objc_class: "NSView", selector: "initWithFrame:", is_class_method: false, smalltalk_class: "NSViewAlien" },
    ];
    let posix = vec![
        // `open`'s real declaration is variadic (`open(path, flags, ...)`) —
        // the optional `mode_t mode` third argument only applies when
        // `O_CREAT` is in `flags`, and `posix_function_args` (like clang's
        // own AST) only records the two FIXED parameters, never the
        // variadic tail (`docs/FFI.md` §7: no variadic calling-convention
        // story here or in cocoa_data itself) — so the manifest's arity
        // must match the fixed-arg count, not the full C prototype's.
        PosixFunctionSpec { c_name: "open", smalltalk_selector: "openPath:flags:" },
        PosixFunctionSpec { c_name: "read", smalltalk_selector: "readFd:buffer:count:" },
        PosixFunctionSpec { c_name: "write", smalltalk_selector: "writeFd:buffer:count:" },
        PosixFunctionSpec { c_name: "close", smalltalk_selector: "closeFd:" },
        PosixFunctionSpec {
            c_name: "mmap",
            smalltalk_selector: "mmapAddr:length:prot:flags:fd:offset:",
        },
    ];
    let structs: &[&str] = &["CGPoint"];
    (cocoa, posix, structs)
}

// ────────────────────────────────────────────────────────── generation

/// Positional Smalltalk parameter names — deliberately generic (`a1`, `a2`,
/// …), not derived from ObjC keyword text: real Objective-C keyword parts
/// often repeat or don't make valid/distinct Smalltalk identifiers on their
/// own, and generated code doesn't need to look hand-written (the header
/// comment already says so).
fn param_names(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("a{i}")).collect()
}

/// Splits a keyword selector into its parts (`"colorWithRed:green:blue:alpha:"`
/// → `["colorWithRed:", "green:", "blue:", "alpha:"]`); a unary selector
/// (no `:`) is returned as its one whole self-contained part.
fn selector_parts(selector: &str) -> Vec<String> {
    if !selector.contains(':') {
        return vec![selector.to_string()];
    }
    selector.split(':').filter(|s| !s.is_empty()).map(|s| format!("{s}:")).collect()
}

/// One method's `.mst` body — the message pattern line plus a
/// forward-declared FFI pragma (`docs/FFI.md` §6.3). `receiver_arg_offset`
/// skips `self`/`_cmd` when consulting `arg_classes` for an instance-side
/// Cocoa send (both already implicit in an ordinary Smalltalk send), 0 for
/// POSIX (no receiver at all).
fn method_text(pattern: &str, primitive_pragma: &str) -> String {
    format!("    {pattern} [\n        {primitive_pragma}\n    ]\n")
}

pub fn generate_cocoa_method(db: &Db, spec: &CocoaMethodSpec) -> Result<String, GenError> {
    let abi = db
        .method_abi(spec.objc_class, spec.selector, spec.is_class_method)?
        .ok_or_else(|| GenError::NotFound(format!("{}{}{}", spec.objc_class, if spec.is_class_method { " class>>" } else { ">>" }, spec.selector)))?;

    let is_keyword = spec.selector.contains(':');
    let parts = selector_parts(spec.selector);
    let pattern = if is_keyword {
        let names = param_names(parts.len());
        parts.iter().zip(&names).map(|(kw, name)| format!("{kw} {name}")).collect::<Vec<_>>().join(" ")
    } else {
        spec.selector.to_string()
    };
    let full_pattern = if spec.is_class_method { format!("{} class >> {pattern}", spec.smalltalk_class) } else { pattern };

    let pragma = format!(
        "<primitive: FFI selector: #{} class: #{} classSide: {} ret: #{} args: {}>",
        spec.selector,
        spec.objc_class,
        spec.is_class_method,
        abi.ret_class,
        abi.as_smalltalk_array_literal(),
    );
    Ok(method_text(&full_pattern, &pragma))
}

pub fn generate_posix_function(db: &Db, spec: &PosixFunctionSpec) -> Result<String, GenError> {
    let abi = db.posix_function_abi(spec.c_name)?.ok_or_else(|| GenError::NotFound(spec.c_name.to_string()))?;

    // A manifest selector with no colon (e.g. a hypothetical zero-arg
    // `getpid`) means arity 0, same convention `generate_cocoa_method` uses
    // for a unary Smalltalk selector — `selector_parts` alone can't
    // distinguish "unary, 0 args" from "one keyword part, 1 arg" (both
    // produce a 1-element Vec), so that check has to happen here first.
    let is_keyword = spec.smalltalk_selector.contains(':');
    let parts = selector_parts(spec.smalltalk_selector);
    let arity = if is_keyword { parts.len() } else { 0 };
    if arity != abi.arg_classes.len() {
        return Err(GenError::NotFound(format!(
            "{}: manifest selector {:?} implies {arity} arg(s) but the real function takes {} — fix the manifest",
            spec.c_name,
            spec.smalltalk_selector,
            abi.arg_classes.len()
        )));
    }
    let pattern = if is_keyword {
        let names = param_names(parts.len());
        format!("FFIPosix class >> {}", parts.iter().zip(&names).map(|(kw, name)| format!("{kw} {name}")).collect::<Vec<_>>().join(" "))
    } else {
        format!("FFIPosix class >> {}", spec.smalltalk_selector)
    };
    let pragma = format!("<primitive: FFI function: #{} ret: #{} args: {}>", spec.c_name, abi.ret_class, abi.as_smalltalk_array_literal());
    Ok(method_text(&pattern, &pragma))
}

/// One Alien-backed accessor pair per flat scalar field (`docs/FFI.md` §4:
/// struct field access is generated Smalltalk over the existing typed byte
/// accessors, not a new primitive per struct). `struct_fields.offset` is a
/// 0-based byte offset (`encoding.py`'s `_lay`); Alien's own typed accessors
/// are 1-based byte offsets (confirmed against `Alien.dlt`'s
/// `asExternalProxy`: `unsignedShortAt: 3`/`unsignedShortAt: 1` for a 4-byte
/// value's two halves) — hence `+ 1` below.
fn field_accessor(struct_name: &str, field: &StructField) -> Result<String, GenError> {
    let idx = field.offset + 1;
    // Real Alien's setters are the *same* first keyword part as the getter,
    // with a second `put:` part added (`self doubleAt: 1 put: aNumber`,
    // `Alien.dlt`'s own `unsignedLongAt:put:` etc.) — one two-keyword send,
    // not a single combined selector token. An earlier version of this
    // function stored a bogus combined `"doubleAt:put:"` string and reused
    // it as if it were one keyword before the index arg, which produced
    // `self doubleAt:put: 1 put: aNumber` — malformed, caught by actually
    // running the generator against the real database (`cargo run --bin
    // generate_ffi`), not by the unit tests alone.
    let getter = match field.ty.as_str() {
        "d" => "doubleAt:",
        "f" => "floatAt:",
        "q" | "Q" => "signedLongAt:",
        "^" => "unsignedLongAt:",
        other => return Err(GenError::UnsupportedFieldType { struct_name: struct_name.to_string(), field: field.name.clone(), ty: other.to_string() }),
    };
    Ok(format!(
        "    {name} [ ^self {getter} {idx} ]\n    {name}: aNumber [ ^self {getter} {idx} put: aNumber ]\n",
        name = field.name
    ))
}

pub fn generate_struct_accessor_class(db: &Db, struct_name: &str) -> Result<String, GenError> {
    let layout = db.struct_layout(struct_name)?.ok_or_else(|| GenError::NotFound(struct_name.to_string()))?;
    let class_name = format!("{struct_name}Alien");
    let mut body = String::new();
    for field in &layout.fields {
        body.push_str(&field_accessor(struct_name, field)?);
    }
    Ok(format!(
        "Alien subclass: {class_name} [\n\
         \x20   \"{} bytes, {} field(s), byte-offset-addressed via the shared Alien typed accessors — docs/FFI.md §4.\"\n\
         {body}]\n",
        layout.size,
        layout.fields.len(),
    ))
}

/// Groups `specs` by `smalltalk_class` and emits one `Object subclass: ... [
/// ]` block per group, all their methods inside — `image_store::mst`'s
/// grammar (the same parser `import_world` uses) has no block-grouping form
/// for class-side vs. instance-side (confirmed against the real `.mst`
/// corpus while building that parser), so instance and class methods for
/// the same generated class are simply interleaved in one body, exactly as
/// real `.mst` files already do.
pub fn generate_cocoa_bindings(db: &Db, specs: &[CocoaMethodSpec]) -> Result<String, GenError> {
    let mut classes: Vec<&str> = Vec::new();
    for s in specs {
        if !classes.contains(&s.smalltalk_class) {
            classes.push(s.smalltalk_class);
        }
    }
    let mut out = String::new();
    for class in classes {
        out.push_str(&format!("Alien subclass: {class} [\n"));
        for spec in specs.iter().filter(|s| s.smalltalk_class == class) {
            out.push_str(&generate_cocoa_method(db, spec)?);
        }
        out.push_str("]\n");
    }
    Ok(out)
}

pub fn generate_posix_bindings(db: &Db, specs: &[PosixFunctionSpec]) -> Result<String, GenError> {
    let mut out = String::from("Alien subclass: FFIPosix [\n");
    for spec in specs {
        out.push_str(&generate_posix_function(db, spec)?);
    }
    out.push_str("]\n");
    Ok(out)
}

pub const GENERATED_HEADER: &str = "\"Generated by ffi_gen from cocoa.sqlite (docs/FFI.md, S20) — do not hand-edit; re-run instead.\n Forward-declared: no <primitive: FFI ...> exists in src/runtime/primitives.rs yet.\"\n\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Option<Db> {
        let path = resolve_db_path(None).ok()?;
        Db::open_at(&path).ok()
    }

    macro_rules! require_db {
        ($db:ident) => {
            let Some($db) = test_db() else {
                eprintln!("skipping: cocoa.sqlite not found (set COCOA_DATA_DB)");
                return;
            };
        };
    }

    #[test]
    fn abi_parses_empty_args_as_no_elements_not_one_empty_string() {
        let abi = Abi::from_row("g".to_string(), String::new());
        assert_eq!(abi.arg_classes, Vec::<String>::new());
        assert_eq!(abi.as_smalltalk_array_literal(), "#()");
    }

    #[test]
    fn abi_parses_comma_joined_args() {
        let abi = Abi::from_row("g".to_string(), "f,f,f,f".to_string());
        assert_eq!(abi.arg_classes, vec!["f", "f", "f", "f"]);
        assert_eq!(abi.as_smalltalk_array_literal(), "#(f f f f)");
    }

    #[test]
    fn selector_parts_splits_keyword_selectors() {
        assert_eq!(selector_parts("colorWithRed:green:blue:alpha:"), vec!["colorWithRed:", "green:", "blue:", "alpha:"]);
        assert_eq!(selector_parts("frame"), vec!["frame"]);
    }

    #[test]
    fn generated_cocoa_method_matches_the_real_database_abi() {
        require_db!(db);
        let spec = CocoaMethodSpec {
            objc_class: "NSColor",
            selector: "colorWithRed:green:blue:alpha:",
            is_class_method: true,
            smalltalk_class: "NSColorAlien",
        };
        let text = generate_cocoa_method(&db, &spec).expect("NSColor colorWithRed:green:blue:alpha: must exist in cocoa.sqlite");
        assert!(text.contains("NSColorAlien class >> colorWithRed: a1 green: a2 blue: a3 alpha: a4"), "{text}");
        assert!(text.contains("ret: #g args: #(f f f f)"), "{text}");
    }

    #[test]
    fn generated_frame_method_is_h4_with_no_args() {
        require_db!(db);
        let spec = CocoaMethodSpec { objc_class: "NSView", selector: "frame", is_class_method: false, smalltalk_class: "NSViewAlien" };
        let text = generate_cocoa_method(&db, &spec).unwrap();
        assert!(text.contains("frame ["), "{text}");
        assert!(text.contains("ret: #h4 args: #()"), "{text}");
    }

    #[test]
    fn generated_init_with_frame_takes_an_h4_argument() {
        require_db!(db);
        let spec = CocoaMethodSpec { objc_class: "NSView", selector: "initWithFrame:", is_class_method: false, smalltalk_class: "NSViewAlien" };
        let text = generate_cocoa_method(&db, &spec).unwrap();
        assert!(text.contains("ret: #g args: #(h4)"), "{text}");
    }

    #[test]
    fn generated_mmap_matches_the_real_posix_abi() {
        require_db!(db);
        let spec = PosixFunctionSpec { c_name: "mmap", smalltalk_selector: "mmapAddr:length:prot:flags:fd:offset:" };
        let text = generate_posix_function(&db, &spec).unwrap();
        assert!(text.contains("mmapAddr: a1 length: a2 prot: a3 flags: a4 fd: a5 offset: a6"), "{text}");
        assert!(text.contains("ret: #g args: #(g g g g g g)"), "{text}");
    }

    #[test]
    fn posix_manifest_arity_mismatch_is_a_generation_error_not_a_panic() {
        require_db!(db);
        let spec = PosixFunctionSpec { c_name: "mmap", smalltalk_selector: "mmapWrong:" }; // 1 keyword part, mmap takes 6
        let err = generate_posix_function(&db, &spec).unwrap_err();
        assert!(matches!(err, GenError::NotFound(_)), "{err}");
    }

    #[test]
    fn cgpoint_struct_accessor_uses_real_byte_offsets() {
        require_db!(db);
        let text = generate_struct_accessor_class(&db, "CGPoint").unwrap();
        assert!(text.contains("Alien subclass: CGPointAlien"), "{text}");
        assert!(text.contains("x [ ^self doubleAt: 1 ]"), "{text}");
        assert!(text.contains("y [ ^self doubleAt: 9 ]"), "{text}"); // offset 8 -> Alien index 9
        // The setter is a two-keyword send reusing the getter's own
        // keyword (`self doubleAt: 1 put: aNumber`), not a bogus combined
        // "doubleAt:put:" token before the index arg — a real bug an
        // earlier version of this generator had, caught only by actually
        // running it (`cargo run --bin generate_ffi`), not by a test that
        // only checked the getter.
        assert!(text.contains("x: aNumber [ ^self doubleAt: 1 put: aNumber ]"), "{text}");
        assert!(text.contains("y: aNumber [ ^self doubleAt: 9 put: aNumber ]"), "{text}");
    }

    #[test]
    fn generated_bindings_parse_as_real_mst_syntax() {
        require_db!(db);
        let (cocoa, posix, _structs) = seed_manifest();
        let cocoa_text = generate_cocoa_bindings(&db, &cocoa).unwrap();
        let posix_text = generate_posix_bindings(&db, &posix).unwrap();
        let full = format!("{GENERATED_HEADER}{cocoa_text}\n{posix_text}");

        // The exact parser image_store's own importer uses — generated
        // bindings must be indistinguishable from hand-written .mst text.
        let classes = image_store::mst::parse_mst_source(&full);
        let names: Vec<&str> = classes.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"NSColorAlien"), "{names:?}");
        assert!(names.contains(&"NSViewAlien"), "{names:?}");
        assert!(names.contains(&"FFIPosix"), "{names:?}");

        let nsview = classes.iter().find(|c| c.name == "NSViewAlien").unwrap();
        assert_eq!(nsview.methods.len(), 2); // frame, initWithFrame:
        assert!(nsview.methods.iter().any(|m| m.selector == "frame" && !m.is_class_side));
        assert!(nsview.methods.iter().any(|m| m.selector == "initWithFrame:" && !m.is_class_side));

        let nscolor = classes.iter().find(|c| c.name == "NSColorAlien").unwrap();
        assert!(nscolor.methods.iter().any(|m| m.selector == "colorWithRed:green:blue:alpha:" && m.is_class_side));
    }
}
