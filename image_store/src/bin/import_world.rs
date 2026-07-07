//! Imports `world/*.mst` (in `world/world.list` order) into a fresh
//! `image_store` SQLite image — `../../../docs/IMAGE.md` §6's "one-time,
//! repeatable" import step. `.mst` stays the checked-in source of truth;
//! this just seeds the versioned database the GUI class browser can point
//! at (`gui/src/vm_host.rs`'s `MACVM_IMAGE_PATH`).
//!
//! Usage: `import_world <world-dir> <output-image-path>` — defaults to
//! `world` and `world/image.sqlite3` (relative to the current directory,
//! i.e. run from the repo root) if not given.

use image_store::import::import_world_dir;
use image_store::Image;
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let world_dir = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("world"));
    let image_path = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| world_dir.join("image.sqlite3"));

    if image_path.exists() {
        eprintln!("import_world: refusing to overwrite existing {} — remove it first if you want a clean re-import.", image_path.display());
        std::process::exit(1);
    }

    let image = Image::open(&image_path).expect("failed to create image");
    match import_world_dir(&image, &world_dir) {
        Ok(stats) => println!(
            "import_world: imported {} classes, {} methods from {} into {}",
            stats.classes,
            stats.methods,
            world_dir.display(),
            image_path.display()
        ),
        Err(e) => {
            eprintln!("import_world: {e}");
            std::process::exit(1);
        }
    }
}
