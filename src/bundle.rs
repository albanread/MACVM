//! Bundle self-bootstrap — what the `launcher` shell script used to do.
//!
//! A packaged `.app` carries its whole runtime payload (world `.mst` sources,
//! GUI assets, docs) read-only under `Contents/Resources/payload`, but the VM
//! needs a WRITABLE home (the SQLite image is edited in place). So on launch
//! the payload is copied to `~/Library/Application Support/MACVM/<mode>` and
//! the process runs from there.
//!
//! That used to be a bash script installed as `CFBundleExecutable`, which made
//! the bundle awkward to sign: hardened runtime and entitlements attach to a
//! Mach-O, and a script entry point runs under Apple-signed `/bin/bash` before
//! `exec`ing the real binary — an entitlement chain that is hard to reason
//! about and that notarization flags. Doing it here makes the binary itself the
//! bundle's entry point.
//!
//! Deliberately no subprocess: the old script shelled out to `/usr/bin/ditto`.
//! The recursive copy below keeps the app from spawning anything at startup,
//! which is one less thing to explain to a reviewer and one less binary whose
//! signature has to be trusted.
//!
//! **No-op outside a bundle.** Running `target/release/macvm-cocoa` from the
//! source tree finds no `Contents/Resources/payload` and returns immediately,
//! so the development workflow is untouched.

use std::path::{Path, PathBuf};

/// Copy `src` into `dst` recursively, creating directories as needed.
/// Symlinks are followed (the payload has none; this keeps it simple).
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// If this executable lives inside a `.app`, materialise its payload into the
/// user's Application Support directory and point the VM's env vars at it.
/// Answers `true` when a bundle was detected (whether or not a copy was
/// needed), so the caller can log it.
///
/// `mode` names the per-app subdirectory (`cocoa` / `web`), matching what the
/// launcher script used and so preserving existing users' runtime homes.
/// `#[allow(unsafe_code)]` (CONVENTIONS §1): `std::env::set_var` is `unsafe` in
/// the 2024 edition because a concurrent reader is UB. This runs as the FIRST
/// statement of `main`, before any thread is spawned and before AppKit is
/// touched, so there is no other thread that could observe the write.
#[allow(unsafe_code)]
pub fn bootstrap_payload(mode: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    // .../Foo.app/Contents/MacOS/<binary> -> .../Foo.app/Contents
    let Some(contents) = exe.parent().and_then(Path::parent) else {
        return false;
    };
    let payload = contents.join("Resources").join("payload");
    if !payload.is_dir() {
        return false; // running from the source tree — nothing to do
    }

    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return false;
    };
    let sup = home
        .join("Library")
        .join("Application Support")
        .join("MACVM")
        .join(mode);

    // Refresh on first run or whenever the bundled version differs. The version
    // file is the git short-hash `make-macapp.sh` stamps in.
    let bundled = std::fs::read(payload.join(".version")).ok();
    let installed = std::fs::read(sup.join(".version")).ok();
    if bundled.is_some() && bundled != installed {
        if let Err(e) = copy_tree(&payload, &sup) {
            eprintln!("macvm: could not install payload into {}: {e}", sup.display());
            return true;
        }
        // Force a fresh reseed for the new world — the old image predates it.
        let _ = std::fs::remove_file(sup.join("world").join("image.sqlite3"));
    }

    // Run from the writable home, exactly as the launcher's `cd` did, so every
    // relative default (`world/image.sqlite3`) resolves there.
    let _ = std::env::set_current_dir(&sup);
    // SAFETY: single-threaded startup — this runs as the first statement of
    // `main`, before any thread is spawned and before AppKit is touched.
    unsafe {
        std::env::set_var("MACVM_GUI_ROOT", sup.join("gui"));
        std::env::set_var("MACVM_WORLD_PATH", sup.join("world"));
        std::env::set_var(
            "MACVM_IMAGE_PATH",
            sup.join("world").join("image.sqlite3"),
        );
    }
    true
}
