//! Imports `world/*.mst` (in `world/world.list` order) into a fresh
//! `image_store` SQLite image — `../../../docs/IMAGE.md` §6's "one-time,
//! repeatable" import step. `.mst` stays the checked-in source of truth;
//! this just seeds the versioned database the GUI class browser can point
//! at (`gui/src/vm_host.rs`'s `MACVM_IMAGE_PATH`).
//!
//! Usage: `import_world <world-dir> <output-image-path>` — defaults to
//! `world` and `world/image.sqlite3` (relative to the current directory,
//! i.e. run from the repo root) if not given.

use image_store::mst::parse_mst_source;
use image_store::{Image, Side};
use std::path::{Path, PathBuf};

fn main() {
    let mut args = std::env::args().skip(1);
    let world_dir = args.next().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("world"));
    let image_path = args.next().map(PathBuf::from).unwrap_or_else(|| world_dir.join("image.sqlite3"));

    if image_path.exists() {
        eprintln!("import_world: refusing to overwrite existing {} — remove it first if you want a clean re-import.", image_path.display());
        std::process::exit(1);
    }

    let list_path = world_dir.join("world.list");
    let list_text = match std::fs::read_to_string(&list_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("import_world: failed to read {}: {e}", list_path.display());
            std::process::exit(1);
        }
    };
    // world.list's own format (see the file's header comment): one
    // filename per line, `#`-prefixed lines and blank lines are comments.
    let filenames: Vec<&str> = list_text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    let image = Image::open(&image_path).expect("failed to create image");
    let mut classes_imported = 0usize;
    let mut methods_imported = 0usize;

    for filename in filenames {
        let path = world_dir.join(filename);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("import_world: skipping {}: {e}", path.display());
                continue;
            }
        };
        // The file's own basename-without-extension, minus a leading
        // `NN_`/`NNa_` load-order prefix, becomes the package/category —
        // e.g. "15_ordered.mst" -> "ordered". Good enough for a first
        // import; renaming a package is just a browser edit away.
        let category = package_name_from_filename(filename);

        for parsed in parse_mst_source(&text) {
            // Real `.mst` files legitimately *reopen* an already-defined
            // class to add more methods once dependencies exist elsewhere
            // (`01_object.mst`'s own header comment names this exact
            // pattern for Object's Boolean- and printing-dependent
            // methods, landing in 02/19) — so an already-existing name
            // here isn't an error, just "add these methods to the class
            // that's already there," leaving its own definition
            // (superclass/category/comment/ivars) untouched.
            let already_existed = image.class_exists(&parsed.name).expect("class_exists failed");
            if !already_existed {
                let load_order = image.next_load_order().expect("next_load_order failed");
                image
                    .add_class(&parsed.name, parsed.superclass.as_deref(), &category, &parsed.comment, &parsed.instance_vars, "", load_order)
                    .unwrap_or_else(|e| panic!("{filename}: failed to create class {}: {e}", parsed.name));
                classes_imported += 1;
            }
            for method in &parsed.methods {
                let side = if method.is_class_side { Side::Class } else { Side::Instance };
                match image.add_method(&parsed.name, side, &method.selector, "as yet unclassified", &method.source) {
                    Ok(Some(_)) => methods_imported += 1,
                    Ok(None) => eprintln!("import_world: {}: add_method found no class {} (shouldn't happen)", filename, parsed.name),
                    Err(e) => eprintln!("import_world: {}: failed to add {}>>{}: {e}", filename, parsed.name, method.selector),
                }
            }
        }
    }

    println!(
        "import_world: imported {classes_imported} classes, {methods_imported} methods from {} into {}",
        world_dir.display(),
        image_path.display()
    );
}

fn package_name_from_filename(filename: &str) -> String {
    let stem = Path::new(filename).file_stem().and_then(|s| s.to_str()).unwrap_or(filename);
    // Strip a leading "NN_" or "NNa_" load-order prefix (matches the
    // world/*.mst naming convention throughout world.list).
    match stem.find('_') {
        Some(idx) if stem[..idx].chars().all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()) => stem[idx + 1..].to_string(),
        _ => stem.to_string(),
    }
}
