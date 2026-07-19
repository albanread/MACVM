//! Booting the VM's world from the versioned image database instead of the
//! `world/*.mst` file tree (S22, "the concept is to retain source in the
//! database, and to switch to loading the world from the database").
//!
//! The database is the source of truth; this module replays it into a
//! genesis-only [`VmHandle`] (`macvm::embed::VmHandle::boot_without_world`)
//! by reconstructing each stored class's `.mst` definition and feeding it
//! through the VM's own compiler via `exec` (the run-for-effect twin of
//! `eval`, no printString) — the exact same path a `.mst`-file load uses
//! (`execute_top_item` → `install_class_def`), so a DB-booted world is the
//! same VM state as a file-booted one.
//!
//! Lives in `image_store` (not the `macvm` crate) on purpose: the VM stays
//! free of a SQLite dependency — the embedder wires the database to the VM
//! (the same separation `docs/SPEC.md` §16 draws for the whole GUI↔VM
//! boundary) — while `image_store` may depend on `macvm`'s `VmHandle`
//! freely; the constraint only ever ran one direction. Moved here from
//! `gui/src/world_boot.rs` (`docs/package_aware_editing_design.md` M2): that
//! crate is `[[bin]]`-only with no `[lib]` target, so nothing else could
//! ever depend on it — `cocoa_gui` needs this same replay logic to move its
//! own primary onto DB-boot (a later milestone), and `gui` keeps using it
//! via a one-line re-export (`crate::world_boot` in `gui/src/main.rs`) so
//! none of its own call sites needed to change.
//!
//! Correctness rests on three facts established when this was built (S22):
//! `all_classes()` returns classes in dependency-safe `load_order`
//! (superclass before subclass); the stored method `source` is now the FULL
//! method text (header + body), which `image_store`'s importer was fixed to
//! capture (without the parameter names in the header, a body-only record
//! could not be recompiled); and — critically — the compiler resolves a
//! global/class name at COMPILE time and does NOT auto-declare a forward
//! reference from inside a method body (`src/frontend/codegen.rs`
//! `resolve_var`: auto-declare fires only for a top-level *assignment* to an
//! uppercase name, never a read inside a method). The `.mst` load order
//! avoids forward references by construction; but the DB flattens each
//! class's reopens (e.g. `Object`'s `printString`, added in a much later
//! file, referencing `WriteStream`) onto the class's ORIGINAL `load_order`,
//! reintroducing them. So the load runs in TWO phases: first bind every
//! class global (its shell — superclass + ivars, no methods) in
//! `load_order`, then install every class's methods. By the time any method
//! compiles, every class name it could reference is already a bound global.

use crate::{FullClass, FullMethod, Image};
use macvm::embed::VmHandle;

/// Phase-1 shell source for a class: `<super> subclass: <name> [ |ivars| ]`
/// — binds the class global and fixes its shape (superclass + instance
/// vars), with no methods yet.
///
/// `superclass` is `None` only for `Object` (whose true superclass is
/// `nil`). Instance vars are emitted only when non-empty, which is also
/// exactly when the class is genuinely NEW rather than a reopen of a genesis
/// class (the `.mst`, and thus the DB, only ever declares vars on the
/// original definition), so this naturally satisfies `install_class_def`'s
/// rule that a reopen must declare no vars.
pub fn class_shell_mst(class: &FullClass) -> String {
    let superclass = class.superclass.as_deref().unwrap_or("nil");
    let mut body = String::new();
    let ivars = class.instance_vars.trim();
    if !ivars.is_empty() {
        body.push_str(&format!("    | {ivars} |\n"));
    }
    let cvars = class.class_vars.trim();
    if !cvars.is_empty() {
        body.push_str(&format!("    <classVars: {cvars}>\n"));
    }
    format!("{superclass} subclass: {} [\n{body}]\n", class.name)
}

/// Phase-2 reopen source for a class: `<super> subclass: <name> [ <methods> ]`
/// — the class already exists (phase 1 created it), so this declares NO vars
/// (a reopen must not) and just adds the methods. Each method's stored
/// `source` is the full `selector params [ body ]` text, including the
/// `<name> class >>` prefix for class-side methods, so they concatenate
/// verbatim into the class body. The comment and category are omitted:
/// neither affects compiled behaviour (comments are lexer-skipped; category
/// is browser metadata the browser reads straight from the DB).
pub fn class_methods_mst(class: &FullClass, methods: &[FullMethod]) -> String {
    let superclass = class.superclass.as_deref().unwrap_or("nil");
    let mut src = format!("{superclass} subclass: {} [\n", class.name);
    for m in methods {
        src.push_str(m.source.trim_end());
        src.push('\n');
    }
    src.push_str("]\n");
    src
}

/// What a [`load_world_from_image`] call installed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadStats {
    pub classes: usize,
    pub methods: usize,
    pub doits: usize,
}

/// If `doit` is a top-level assignment to an uppercase (global) name —
/// `Transcript := TranscriptStream new.` — returns that name (`"Transcript"`).
/// These globals must be DECLARED before phase-2 methods that reference them
/// compile, even though their real VALUE isn't assigned until the doIt runs
/// after phase 2 (a method holds the global's Association and reads its value
/// at runtime, by which point the doIt has filled it in). A doIt that is a
/// plain message send (`Character initTable.`) declares no global and yields
/// `None`.
fn doit_assigned_global(doit: &str) -> Option<&str> {
    let (lhs, _) = doit.split_once(":=")?;
    let name = lhs.trim();
    if !name.is_empty()
        && name.starts_with(|c: char| c.is_uppercase())
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
    {
        Some(name)
    } else {
        None
    }
}

/// Replays the whole world into `vm` (a fresh genesis-only handle): every
/// class in the image, then the top-level `doits`. Classes load in two
/// phases (module doc): phase 1 binds every class shell in `load_order`;
/// phase 2 installs every class's methods. Between them, each global a doIt
/// assigns is pre-declared (as nil) so phase-2 methods referencing it
/// compile; the doIts themselves run last, once every class and method
/// exists to satisfy them. The first failure aborts with an `Err` naming
/// what broke — a corrupt/edited record shouldn't silently yield a
/// half-built world.
///
/// `doits` is passed explicitly for now (the `.mst` world runs two:
/// `Transcript := TranscriptStream new.` and `Character initTable.`); S22-B
/// moves them into the image (a doits table + `Image::all_doits`), after
/// which this reads them from `image` and drops the parameter.
///
/// `list_names` selects which package lists to load
/// (`docs/package_aware_editing_design.md` §4.5, M4) — `Image::
/// classes_for_lists`, not `all_classes()`, so a VM boots exactly the world
/// plus the packages it needs, no more. Pass `&["world"]` for a VM with no
/// Cocoa-GUI role; a UI worker also wanting `cocoaui`'s classes passes
/// `&["world", "cocoaui"]`. `classes_for_lists` already sorts by the same
/// global `load_order` `all_classes()` used, so this is a pure narrowing —
/// dependency-safety is unaffected by which subset is requested.
pub fn load_world_from_image(
    vm: &mut VmHandle,
    image: &Image,
    list_names: &[&str],
    doits: &[&str],
) -> Result<LoadStats, String> {
    let classes = image
        .classes_for_lists(list_names)
        .map_err(|e| format!("reading classes from image: {e}"))?;

    // Phase 1: bind every class global (shells only) in load_order.
    for class in &classes {
        vm.exec(&class_shell_mst(class))
            .map_err(|e| format!("defining class {}: {e}", class.name))?;
    }

    // Pre-declare every global a doIt assigns, so phase-2 methods that
    // reference it (e.g. `Transcript`) resolve at compile time.
    for &doit in doits {
        if let Some(name) = doit_assigned_global(doit) {
            vm.exec(&format!("{name} := nil."))
                .map_err(|e| format!("pre-declaring global {name}: {e}"))?;
        }
    }

    // Phase 2: install every class's methods (reopen — all class globals and
    // doIt globals are now bound, so any forward reference resolves).
    let mut stats = LoadStats {
        classes: classes.len(),
        methods: 0,
        doits: 0,
    };
    for class in &classes {
        let methods = image
            .all_methods_of(&class.name)
            .map_err(|e| format!("reading methods of {}: {e}", class.name))?;
        if methods.is_empty() {
            continue;
        }
        vm.exec(&class_methods_mst(class, &methods))
            .map_err(|e| format!("installing methods of {}: {e}", class.name))?;
        stats.methods += methods.len();
    }

    // Finally the doIts, with every class and method in place.
    for &doit in doits {
        vm.exec(doit)
            .map_err(|e| format!("running doit `{doit}`: {e}"))?;
        stats.doits += 1;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Side;
    use macvm::runtime::{JitMode, VmOptions};
    use std::path::PathBuf;

    fn world_dir() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../world"))
    }

    fn opts() -> VmOptions {
        VmOptions {
            heap_mib: 256,
            jit: JitMode::Off,
            ..Default::default()
        }
    }

    /// A fresh temp image seeded from `world/` via the real importer.
    fn seed_image() -> (Image, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "macvm_world_boot_{}_{}.sqlite3",
            std::process::id(),
            // a per-call discriminator without Instant/rand (unavailable):
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let image = Image::open(&path).expect("open temp image");
        crate::import::import_world_dir(&image, &world_dir()).expect("import world");
        (image, path)
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[test]
    fn reconstructs_shell_and_methods() {
        // Object: superclass nil, no ivars — shell is a bare reopen (no
        // `| |`), which is what `install_class_def` requires of a reopen.
        let object = FullClass {
            name: "Object".into(),
            superclass: None,
            category: "object".into(),
            comment: String::new(),
            instance_vars: String::new(),
            class_vars: String::new(),
            load_order: 1000,
        };
        let shell = class_shell_mst(&object);
        assert!(shell.starts_with("nil subclass: Object [\n"), "{shell}");
        assert!(
            !shell.contains('|'),
            "reopen shell declares no ivars: {shell}"
        );

        // A new class with ivars: shell carries the ivars, no methods.
        let point = FullClass {
            name: "Point".into(),
            superclass: Some("Object".into()),
            category: "point".into(),
            comment: String::new(),
            instance_vars: "x y".into(),
            class_vars: String::new(),
            load_order: 5000,
        };
        let shell = class_shell_mst(&point);
        assert!(shell.contains("Object subclass: Point ["), "{shell}");
        assert!(shell.contains("| x y |"), "{shell}");

        // Phase-2 methods reopen: no ivars, the method body verbatim.
        let m = FullMethod {
            selector: "x".into(),
            side: Side::Instance,
            category: "acc".into(),
            source: "x [ ^x ]".into(),
        };
        let methods = class_methods_mst(&point, std::slice::from_ref(&m));
        assert!(methods.contains("Object subclass: Point ["), "{methods}");
        assert!(
            !methods.contains('|'),
            "reopen must not redeclare ivars: {methods}"
        );
        assert!(methods.contains("x [ ^x ]"), "{methods}");
    }

    /// THE proof: a world booted purely from the database behaves
    /// identically to one booted from `world/*.mst`, across a battery of
    /// expressions exercising many classes. If reconstruction drops or
    /// mangles anything, a printString diverges.
    #[test]
    fn db_booted_world_matches_mst_booted_world() {
        // The two load-bearing doIts the `.mst` files run at top level,
        // not yet captured in the DB (S22-B) — replayed so the DB world is
        // complete for the comparison.
        let doits = [
            "Transcript := TranscriptStream new.",
            "Character initTable.",
        ];

        // Every expression here must be understood by the REAL world — an
        // unhandled runtime DNU goes through `error:` → `fatal_exit`, which
        // under `FatalMode::ExitThread` would silently kill this test thread
        // and hang the harness. Both VMs run on this one thread and share the
        // thread-local fatal mode, so switching to `ExitProcess` (below,
        // after both boot) turns any such mistake into a clean crash-failure
        // instead of a hang. The comparison itself needs no hardcoded
        // expected values — it asserts the two worlds agree.
        let battery = [
            "3 + 4.",
            "6 * 7.",
            "100 - 1.",
            "(1000 * 1000).",
            "'hello' size.",
            "('foo' , 'bar').",
            "(2 < 3).",
            "(3 = 3).",
            "4 even.",
            "5 even.",
            "$A asInteger.",
            "42 printString.",
            "| o | o := OrderedCollection new. o add: 10. o add: 20. o size.",
            "(Array new: 3) size.",
        ];

        let mut mst_vm = VmHandle::boot(opts(), &world_dir()).expect("mst boot");
        let (image, path) = seed_image();
        let mut db_vm = VmHandle::boot_without_world(opts());

        // Both VMs are booted (each set ExitThread); from here on run under
        // ExitProcess so any stray runtime DNU — in the doits or the battery
        // — is a clean crash-failure, never a hung test thread. Both VMs
        // share this thread's fatal mode. Class installation (in the load
        // just below) is compile-only and can't DNU, so ordering is fine.
        macvm::embed::set_fatal_mode(macvm::embed::FatalMode::ExitProcess);

        let stats = load_world_from_image(&mut db_vm, &image, &["world"], &doits).expect("db load");

        let expected: Vec<Result<String, String>> = battery
            .iter()
            .map(|e| mst_vm.eval(e).map_err(|err| err.to_string()))
            .collect();
        let got: Vec<Result<String, String>> = battery
            .iter()
            .map(|e| db_vm.eval(e).map_err(|err| err.to_string()))
            .collect();

        std::fs::remove_file(&path).ok();

        assert!(
            stats.classes >= 40,
            "expected the whole world, got {stats:?}"
        );
        for (i, (exp, act)) in expected.iter().zip(got.iter()).enumerate() {
            assert_eq!(
                exp, act,
                "expr `{}` diverged: mst={exp:?} db={act:?}",
                battery[i]
            );
        }
    }

    #[test]
    fn world_image_installs_all_classes_without_error() {
        // Seed a fresh image from the real world/ tree and boot it. This is
        // where a class-var pragma that didn't round-trip through the importer
        // shows up: a class reopened WITHOUT restating `<classVars: …>` (e.g.
        // Character's second definition) must keep its vars, or method
        // installation fails with "undeclared variable". Regression guard for
        // the blank-editors bug.
        let (image, path) = seed_image();
        let mut vm = VmHandle::boot_without_world(opts());
        let doits = ["Transcript := TranscriptStream new.", "Character initTable."];
        let result = load_world_from_image(&mut vm, &image, &["world"], &doits);
        std::fs::remove_file(&path).ok();
        match result {
            Ok(stats) => assert!(stats.classes >= 40, "expected the whole world, got {stats:?}"),
            Err(e) => panic!("world image failed to install: {e}"),
        }
    }

    #[test]
    fn db_boot_leaves_transcript_usable() {
        // `Transcript := TranscriptStream new.` is one of the doIts; without
        // it, `Transcript show:` would fail — the GUI's own output depends on
        // this, so it gets its own check.
        let (image, path) = seed_image();
        let mut vm = VmHandle::boot_without_world(opts());
        // Fail fast rather than hang if TranscriptStream reconstruction is
        // broken and `show:`/`new` DNUs.
        macvm::embed::set_fatal_mode(macvm::embed::FatalMode::ExitProcess);
        load_world_from_image(
            &mut vm,
            &image,
            &["world"],
            &[
                "Transcript := TranscriptStream new.",
                "Character initTable.",
            ],
        )
        .expect("db load");
        // A single statement per eval (eval runs ONE top-level item): a
        // clean Ok means `show:` was understood and didn't fatal, i.e.
        // Transcript is bound to a usable TranscriptStream.
        vm.eval("Transcript show: 'hi'.")
            .expect("Transcript show: must work");
        let out = vm.eval("3 + 4.").expect("vm still usable after show:");
        std::fs::remove_file(&path).ok();
        assert_eq!(out, "7");
    }
}
