//! Seeding an image from a `world/*.mst` file tree — the library half of
//! the `import_world` binary (`bin/import_world.rs`), extracted so tests and
//! other tooling (the GUI's DB-boot path) can seed a temporary image
//! in-process without shelling out. `.mst` stays the checked-in source of
//! truth; this is the "one-time, repeatable" import step (`docs/IMAGE.md`
//! §6).
//!
//! Package lists (`docs/package_aware_editing_design.md` §4.5): every
//! `world/*.list` file names a set of `.mst` files, one per line — `.list`
//! files themselves stay the checked-in source of truth for a package
//! list's *initial* membership, exactly as `.mst` files are for class/method
//! source, and [`import_list_file`] records that membership into the
//! database (`Image::ensure_package_list_member`) the same way it records
//! everything else it reads. [`import_all_lists`] imports every `.list` file
//! in a world directory, `world.list` first (later lists' classes may
//! reference base-world globals).

use std::path::Path;

use crate::mst::parse_mst_source;
use crate::{Image, Side};

/// What one import call did — a single list file ([`import_list_file`]) or
/// several summed together ([`import_all_lists`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportStats {
    pub classes: usize,
    pub methods: usize,
}

/// Imports every file named in `world_dir/world.list`. Thin wrapper over
/// [`import_list_file`] — kept as its own name because it's the one every
/// existing caller (tests, `bin/import_world.rs`, `cmd_seed`'s predecessor)
/// already spells; behavior is unchanged, just routed through the
/// now-parameterized function.
pub fn import_world_dir(image: &Image, world_dir: &Path) -> Result<ImportStats, String> {
    import_list_file(image, world_dir, "world.list")
}

/// Imports `world_dir/world.list` first, then every other `world_dir/*.list`
/// file found on disk (sorted, so the order is deterministic and a future
/// third list needs no code change here), summing stats across all of them.
/// A world directory with only `world.list` (every existing caller's world
/// fixture, e.g. `gui`'s `test_world_dir()`) behaves identically to
/// [`import_world_dir`] — there's nothing else to find.
pub fn import_all_lists(image: &Image, world_dir: &Path) -> Result<ImportStats, String> {
    let mut stats = import_list_file(image, world_dir, "world.list")?;
    let mut others: Vec<String> = std::fs::read_dir(world_dir)
        .map_err(|e| format!("failed to read {}: {e}", world_dir.display()))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".list") && name != "world.list")
        .collect();
    others.sort();
    for list_file in others {
        let more = import_list_file(image, world_dir, &list_file)?;
        stats.classes += more.classes;
        stats.methods += more.methods;
    }
    Ok(stats)
}

/// The list name a `.list` filename derives to — `"world.list"` -> `"world"`,
/// `"cocoaui.list"` -> `"cocoaui"` — the same "strip the mechanical suffix"
/// idea [`package_name_from_filename`] already applies to `.mst` files' `NN_`
/// prefix, just at the other end of the filename.
fn list_name_from_filename(list_file: &str) -> &str {
    list_file.strip_suffix(".list").unwrap_or(list_file)
}

/// Imports every file named in `world_dir/<list_file>` (in that order) into
/// `image`, and records each file's derived package as a member of
/// `<list_file>`'s own package list ([`list_name_from_filename`]). A class
/// already present (a legitimate `.mst` reopen — see `bin/import_world.rs`'s
/// own note) keeps its original definition and just gains the new methods.
/// Returns per-run counts. `Err` carries a human-readable message;
/// individual per-method failures are collected into `errors` rather than
/// aborting the whole import (so one malformed method doesn't lose the rest
/// of the world).
pub fn import_list_file(image: &Image, world_dir: &Path, list_file: &str) -> Result<ImportStats, String> {
    let list_path = world_dir.join(list_file);
    let list_text = std::fs::read_to_string(&list_path)
        .map_err(|e| format!("failed to read {}: {e}", list_path.display()))?;
    let list_name = list_name_from_filename(list_file);

    // list format: one filename per line; blank and `#`-comment lines
    // skipped (matches `bin/import_world.rs`).
    let filenames: Vec<&str> = list_text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    let mut stats = ImportStats::default();
    for filename in filenames {
        let path = world_dir.join(filename);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let category = package_name_from_filename(filename);
        image
            .ensure_package_list_member(list_name, &category)
            .map_err(|e| format!("{filename}: ensure_package_list_member: {e}"))?;

        for parsed in parse_mst_source(&text) {
            let already_existed = image
                .class_exists(&parsed.name)
                .map_err(|e| format!("{filename}: class_exists({}): {e}", parsed.name))?;
            if !already_existed {
                let load_order = image
                    .next_load_order()
                    .map_err(|e| format!("{filename}: next_load_order: {e}"))?;
                image
                    .add_class(
                        &parsed.name,
                        parsed.superclass.as_deref(),
                        &category,
                        &parsed.comment,
                        &parsed.instance_vars,
                        &parsed.class_vars,
                        load_order,
                    )
                    .map_err(|e| format!("{filename}: add_class({}): {e}", parsed.name))?;
                stats.classes += 1;
            } else {
                // The class already exists (a later file re-opens it, or an
                // incremental reseed re-imports it): MERGE its declared vars so
                // adding a `<classVars: …>` pragma is picked up — but a
                // method-only reopen (no pragma) does not wipe existing vars.
                image
                    .reimport_class_shell(
                        &parsed.name,
                        &parsed.instance_vars,
                        &parsed.class_vars,
                    )
                    .map_err(|e| {
                        format!("{filename}: reimport_class_shell({}): {e}", parsed.name)
                    })?;
            }
            for method in &parsed.methods {
                let side = if method.is_class_side {
                    Side::Class
                } else {
                    Side::Instance
                };
                // A later file legitimately REDEFINES an already-imported
                // method (e.g. `TranscriptStream>>show:` in 04 then 19) — the
                // `.mst` load replaces it (`install_method`), latest wins. So
                // if the method already exists, add a new version (latest
                // wins) rather than failing the UNIQUE constraint; only a
                // genuinely-new method gets `add_method`.
                let redefined = image
                    .set_method_source(&parsed.name, side, &method.selector, &method.source)
                    .map_err(|e| {
                        format!(
                            "{filename}: set_method_source({}>>{}): {e}",
                            parsed.name, method.selector
                        )
                    })?;
                if !redefined {
                    match image.add_method(
                        &parsed.name,
                        side,
                        &method.selector,
                        "as yet unclassified",
                        &method.source,
                    ) {
                        Ok(Some(_)) => stats.methods += 1,
                        Ok(None) => {
                            return Err(format!(
                                "{filename}: add_method found no class {} (bug)",
                                parsed.name
                            ))
                        }
                        Err(e) => {
                            return Err(format!(
                                "{filename}: add_method({}>>{}): {e}",
                                parsed.name, method.selector
                            ))
                        }
                    }
                }
                // Record this method's home file (last file wins for a method
                // reopened across files) so `export` writes edits/deletions back
                // into the right `.mst`.
                image
                    .set_method_home_file(&parsed.name, side, &method.selector, filename)
                    .map_err(|e| {
                        format!(
                            "{filename}: set_method_home_file({}>>{}): {e}",
                            parsed.name, method.selector
                        )
                    })?;
            }
        }
    }
    Ok(stats)
}

/// The file's basename minus a leading `NN_`/`NNa_` load-order prefix
/// becomes the package/category — e.g. `"15_ordered.mst"` -> `"ordered"`
/// (matches `bin/import_world.rs`).
fn package_name_from_filename(filename: &str) -> String {
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(filename);
    match stem.find('_') {
        Some(idx)
            if stem[..idx]
                .chars()
                .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()) =>
        {
            stem[idx + 1..].to_string()
        }
        _ => stem.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Image;
    use std::fs;

    /// A fresh, empty temp dir this test owns exclusively (pid + tag keyed,
    /// same idiom `gui/src/vm_host.rs`'s own temp-image tests use).
    fn temp_world_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("macvm_import_test_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn import_world_dir_is_unaffected_by_the_generalization() {
        // The pre-existing entry point, now a wrapper: same behavior, same
        // stats, same "no package-list awareness needed to keep using it."
        let dir = temp_world_dir("world_dir_only");
        fs::write(
            dir.join("01_object.mst"),
            "Object subclass: Foo [\n    bar [ ^1 ]\n]\n",
        )
        .unwrap();
        fs::write(dir.join("world.list"), "01_object.mst\n").unwrap();

        let image = Image::open_in_memory().unwrap();
        let stats = import_world_dir(&image, &dir).unwrap();
        assert_eq!(stats.classes, 1);
        assert_eq!(stats.methods, 1);
        assert!(image.class_exists("Foo").unwrap());
        assert_eq!(image.package_lists().unwrap(), vec!["world".to_string()]);
        assert_eq!(
            image.packages_in_list("world").unwrap(),
            vec!["object".to_string()]
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn import_all_lists_imports_world_then_every_other_list_file() {
        let dir = temp_world_dir("all_lists");
        fs::write(
            dir.join("01_object.mst"),
            "Object subclass: Foo [\n    bar [ ^1 ]\n]\n",
        )
        .unwrap();
        fs::write(dir.join("world.list"), "01_object.mst\n").unwrap();
        fs::write(
            dir.join("50_extra.mst"),
            "Object subclass: Extra [\n    baz [ ^2 ]\n]\n",
        )
        .unwrap();
        fs::write(dir.join("extra.list"), "50_extra.mst\n").unwrap();

        let image = Image::open_in_memory().unwrap();
        let stats = import_all_lists(&image, &dir).unwrap();
        assert_eq!(stats.classes, 2, "both classes counted across both lists");
        assert_eq!(stats.methods, 2);
        assert!(image.class_exists("Foo").unwrap());
        assert!(image.class_exists("Extra").unwrap());

        let mut lists = image.package_lists().unwrap();
        lists.sort();
        assert_eq!(lists, vec!["extra".to_string(), "world".to_string()]);
        assert_eq!(
            image.packages_in_list("world").unwrap(),
            vec!["object".to_string()]
        );
        assert_eq!(
            image.packages_in_list("extra").unwrap(),
            vec!["extra".to_string()]
        );

        let world_only = image.classes_for_lists(&["world"]).unwrap();
        assert_eq!(world_only.len(), 1);
        assert_eq!(world_only[0].name, "Foo");

        let both = image.classes_for_lists(&["world", "extra"]).unwrap();
        let mut names: Vec<&str> = both.iter().map(|c| c.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["Extra", "Foo"]);

        assert!(
            image.classes_for_lists(&["nonexistent"]).unwrap().is_empty(),
            "a list name that doesn't exist answers no classes, not an error"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn import_all_lists_with_only_world_list_present_matches_import_world_dir() {
        // A world dir with no OTHER *.list file (every existing fixture,
        // e.g. gui's test_world_dir()) behaves identically either way.
        let dir = temp_world_dir("only_world");
        fs::write(
            dir.join("01_object.mst"),
            "Object subclass: Foo [\n    bar [ ^1 ]\n]\n",
        )
        .unwrap();
        fs::write(dir.join("world.list"), "01_object.mst\n").unwrap();

        let image = Image::open_in_memory().unwrap();
        let stats = import_all_lists(&image, &dir).unwrap();
        assert_eq!(stats.classes, 1);
        assert_eq!(stats.methods, 1);
        assert_eq!(image.package_lists().unwrap(), vec!["world".to_string()]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_package_list_member_is_idempotent() {
        let image = Image::open_in_memory().unwrap();
        image.ensure_package_list_member("world", "object").unwrap();
        image.ensure_package_list_member("world", "object").unwrap();
        image.ensure_package_list_member("world", "ordered").unwrap();
        assert_eq!(
            image.packages_in_list("world").unwrap(),
            vec!["object".to_string(), "ordered".to_string()]
        );
        assert_eq!(image.package_lists().unwrap(), vec!["world".to_string()]);
    }
}
