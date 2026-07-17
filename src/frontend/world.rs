//! `world/world.list` loader (SPEC §3.2 step 2, `sprint_s05_detail.md`
//! §Algorithms "world.list loader"). One path per line, relative to
//! `world.list`'s own directory; blank lines and `#`-comments skipped;
//! duplicate entries are an error. First error aborts the whole load.

use std::collections::HashSet;
use std::path::Path;

use crate::runtime::vm_state::VmState;

use super::lexer::Span;
use super::CompileError;

/// Parses and executes every top-level item of one `.mst` file, in order.
pub fn load_file(vm: &mut VmState, path: &Path) -> Result<(), CompileError> {
    let src = std::fs::read_to_string(path).map_err(|e| CompileError {
        path: Some(path.to_path_buf()),
        span: Span { line: 1, col: 1 },
        msg: format!("cannot read '{}': {e}", path.display()),
        eof: false,
    })?;
    let items = super::parser::parse_file(&src).map_err(|e| e.with_path(path.to_path_buf()))?;
    for item in items {
        super::classdef::execute_top_item(vm, item).map_err(|e| e.with_path(path.to_path_buf()))?;
        // `quit`/`quit:` (S6, SPEC §10 system group) — stop loading further
        // items/files immediately rather than continuing past a guest's
        // explicit exit request (e.g. `TestRunner report`'s final `quit:`).
        if vm.exit_requested {
            break;
        }
    }
    Ok(())
}

/// Loads `dir/world.list` (SPEC §3.2 step 2), then sends `Smalltalk
/// startUp` (step 3) if a `Smalltalk` global happens to be bound and
/// understands it — silently skipped otherwise (an early/incomplete world
/// is expected in v1; A5's real `SystemDictionary` removes this allowance).
/// Returns `false` (no error) if `world.list` itself does not exist — the
/// caller (`main.rs`) decides whether/how to warn about that.
pub fn load_world(vm: &mut VmState, dir: &Path) -> Result<bool, CompileError> {
    let list_path = dir.join("world.list");
    let list_src = match std::fs::read_to_string(&list_path) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };

    let mut seen: HashSet<String> = HashSet::new();
    for (i, raw_line) in list_src.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !seen.insert(line.to_string()) {
            return Err(CompileError {
                path: Some(list_path.clone()),
                span: Span {
                    line: (i + 1) as u32,
                    col: 1,
                },
                msg: format!("duplicate world.list entry '{line}'"),
                eof: false,
            });
        }
        load_file(vm, &dir.join(line))?;
        if vm.exit_requested {
            return Ok(true);
        }
    }

    vm.universe.world_loaded = true;
    let smalltalk_sym = vm.universe.intern(b"Smalltalk");
    if let Some(assoc) = crate::runtime::globals::global_lookup(vm, smalltalk_sym) {
        let receiver = crate::oops::wrappers::MemOop::try_from(assoc)
            .expect("global association is a mem oop")
            .body_oop(1);
        let startup_sel = vm.universe.intern(b"startUp");
        let _ = crate::runtime::send_unary_if_understood(vm, receiver, startup_sel);
    }
    Ok(true)
}

/// Loads an EXTRA world list (same format as `world.list`: one `.mst` path
/// per line, relative to *this list file's own directory*; blank lines and
/// `#`-comments skipped; duplicate entries within the list are an error) on
/// top of an already-booted base world — the conditional-layer mechanism
/// (`docs/cocoa_gui_design.md` §12.3, reserving files 63+). Unlike
/// [`load_world`] it takes the list path directly (not `dir/world.list`), and
/// it neither sets `world_loaded` nor sends `Smalltalk startUp`: the base
/// world already did both, and this only appends more classes/methods in
/// order. The first load error aborts the whole extra layer.
///
/// It is NOT wired into `world/world.list`, so the CLI, the WKWebView GUI, and
/// the base test suite carry none of it — only a caller that explicitly opts
/// in (the `macvm-cocoa` UI worker, for `world/cocoaui.list`) loads it.
pub fn load_list(vm: &mut VmState, path: &Path) -> Result<(), CompileError> {
    let list_src = std::fs::read_to_string(path).map_err(|e| CompileError {
        path: Some(path.to_path_buf()),
        span: Span { line: 1, col: 1 },
        msg: format!("cannot read '{}': {e}", path.display()),
        eof: false,
    })?;
    // Paths are relative to the list file's own directory (as `world.list`'s
    // entries are relative to `world/`), so an absolute or a cwd-relative list
    // path both resolve their entries the same way.
    let dir = path.parent().unwrap_or_else(|| Path::new("."));

    let mut seen: HashSet<String> = HashSet::new();
    for (i, raw_line) in list_src.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !seen.insert(line.to_string()) {
            return Err(CompileError {
                path: Some(path.to_path_buf()),
                span: Span {
                    line: (i + 1) as u32,
                    col: 1,
                },
                msg: format!("duplicate list entry '{line}'"),
                eof: false,
            });
        }
        load_file(vm, &dir.join(line))?;
        if vm.exit_requested {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::vm_state::VmOptions;
    use std::io::Write;

    fn test_vm() -> VmState {
        VmState::with_options(VmOptions {
            heap_mib: 64,
            trace: Default::default(),
            gc_stress: false,
            gc_stress_full_period: None,
            eden_kb: None,
            jit: crate::runtime::JitMode::Off,
        })
    }

    fn write_file(dir: &Path, name: &str, contents: &str) {
        let mut f = std::fs::File::create(dir.join(name)).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn worldlist_parse() {
        let tmp =
            std::env::temp_dir().join(format!("macvm_worldlist_parse_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        write_file(&tmp, "a.mst", "Object subclass: A [ foo [ ^1 ] ]\n");
        write_file(&tmp, "b.mst", "Object subclass: B [ bar [ ^2 ] ]\n");
        write_file(&tmp, "world.list", "# a comment\n\na.mst\n  \nb.mst\n");
        let mut vm = test_vm();
        let loaded = load_world(&mut vm, &tmp).expect("load_world");
        assert!(loaded);
        assert!(vm.universe.world_loaded);
        let a_sym = vm.universe.intern(b"A");
        assert!(crate::runtime::globals::global_lookup(&vm, a_sym).is_some());
        let b_sym = vm.universe.intern(b"B");
        assert!(crate::runtime::globals::global_lookup(&vm, b_sym).is_some());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn worldlist_duplicate_entry_errors() {
        let tmp = std::env::temp_dir().join(format!("macvm_worldlist_dup_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        write_file(&tmp, "a.mst", "Object subclass: A [ ]\n");
        write_file(&tmp, "world.list", "a.mst\na.mst\n");
        let mut vm = test_vm();
        let err = load_world(&mut vm, &tmp).unwrap_err();
        assert!(err.msg.contains("duplicate"));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn worldlist_missing_is_not_an_error() {
        let tmp =
            std::env::temp_dir().join(format!("macvm_worldlist_missing_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut vm = test_vm();
        let loaded = load_world(&mut vm, &tmp).expect("missing world.list must not error");
        assert!(!loaded);
        assert!(!vm.universe.world_loaded);

        std::fs::remove_dir_all(&tmp).ok();
    }
}
