//! Seeding an image from a `world/*.mst` file tree — the library half of
//! the `import_world` binary (`bin/import_world.rs`), extracted so tests and
//! other tooling (the GUI's DB-boot path) can seed a temporary image
//! in-process without shelling out. `.mst` stays the checked-in source of
//! truth; this is the "one-time, repeatable" import step (`docs/IMAGE.md`
//! §6).

use std::path::Path;

use crate::mst::parse_mst_source;
use crate::{Image, Side};

/// What one [`import_world_dir`] call did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportStats {
    pub classes: usize,
    pub methods: usize,
}

/// Imports every file named in `world_dir/world.list` (in that order) into
/// `image`. A class already present (a legitimate `.mst` reopen — see
/// `bin/import_world.rs`'s own note) keeps its original definition and just
/// gains the new methods. Returns per-run counts. `Err` carries a
/// human-readable message; individual per-method failures are collected into
/// `errors` rather than aborting the whole import (so one malformed method
/// doesn't lose the rest of the world).
pub fn import_world_dir(image: &Image, world_dir: &Path) -> Result<ImportStats, String> {
    let list_path = world_dir.join("world.list");
    let list_text = std::fs::read_to_string(&list_path)
        .map_err(|e| format!("failed to read {}: {e}", list_path.display()))?;

    // world.list format: one filename per line; blank and `#`-comment lines
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
                // incremental reseed re-imports it): re-apply its shell so a
                // changed superclass/ivars/classVars isn't silently dropped —
                // methods alone are updated below. Without this, adding a
                // `<classVars: …>` pragma and reseeding left the vars empty and
                // broke image boot.
                image
                    .reimport_class_shell(
                        &parsed.name,
                        parsed.superclass.as_deref(),
                        &category,
                        &parsed.comment,
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
