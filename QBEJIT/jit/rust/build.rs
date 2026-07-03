use std::env;
use std::fs;
use std::path::{Path, PathBuf};

// `qbe_bridge.c`/`jit_collect.c` build a fixed 5-entry target table
// (`extern Target T_amd64_sysv / T_amd64_apple / T_arm64 / T_arm64_apple /
// T_rv64`) for `qbe_compile_il_jit`'s `target_name` argument, so all three
// backends must be compiled in even though this crate only tests/supports
// arm64_apple (MACVM's only target — see README.md).
const CORE_SOURCES: &[&str] = &[
    "abi.c",
    "alias.c",
    "cfg.c",
    "copy.c",
    "emit.c",
    "fold.c",
    "gcm.c",
    "gvn.c",
    "ifopt.c",
    "jit_collect.c",
    "live.c",
    "load.c",
    "mem.c",
    "parse.c",
    "qbe_bridge.c",
    "rega.c",
    "simpl.c",
    "spill.c",
    "ssa.c",
    "util.c",
];

const ARM64_SOURCES: &[&str] = &["arm64/abi.c", "arm64/emit.c", "arm64/isel.c", "arm64/targ.c"];

const AMD64_SOURCES: &[&str] = &["amd64/emit.c", "amd64/isel.c", "amd64/sysv.c", "amd64/targ.c"];

const RV64_SOURCES: &[&str] = &["rv64/abi.c", "rv64/emit.c", "rv64/isel.c", "rv64/targ.c"];

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let qbe_c_dir = manifest_dir.join("../c");
    let csrc_dir = manifest_dir.join("csrc");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    if !qbe_c_dir.join("all.h").exists() {
        panic!(
            "expected vendored QBE-fork sources at {} (see QBEJIT/jit/README.md) — \
             did the ../c directory move?",
            qbe_c_dir.display()
        );
    }

    write_config_h(&out_dir);

    // Vendored upstream/FBCQBE sources: warnings off, not ours to lint.
    let mut vendored = cc::Build::new();
    vendored
        .include(&qbe_c_dir)
        .include(&out_dir) // config.h
        .flag_if_supported("-std=c99")
        .warnings(false);

    for src in CORE_SOURCES
        .iter()
        .chain(ARM64_SOURCES.iter())
        .chain(AMD64_SOURCES.iter())
        .chain(RV64_SOURCES.iter())
    {
        let path = qbe_c_dir.join(src);
        println!("cargo:rerun-if-changed={}", path.display());
        vendored.file(path);
    }
    vendored.compile("qbejit_c");

    // Our own C, on top of the vendored core: rust_bridge.c (the
    // setjmp/longjmp safety shim) and ir_builder.c (the text-free IR
    // construction API — see jit/c/ir_builder.h). Warnings left on
    // (cc::Build's default) since this is code we're responsible for.
    let mut ours = cc::Build::new();
    ours.include(&qbe_c_dir).include(&out_dir).flag_if_supported("-std=c99");

    let bridge_c = csrc_dir.join("rust_bridge.c");
    println!("cargo:rerun-if-changed={}", bridge_c.display());
    ours.file(bridge_c);

    let ir_builder_c = qbe_c_dir.join("ir_builder.c");
    let ir_builder_h = qbe_c_dir.join("ir_builder.h");
    println!("cargo:rerun-if-changed={}", ir_builder_c.display());
    println!("cargo:rerun-if-changed={}", ir_builder_h.display());
    ours.file(ir_builder_c);

    ours.compile("qbejit_ours");

    println!("cargo:rerun-if-changed=build.rs");
}

/// Mirrors `jit/c` upstream's `build_qbe.sh`: QBE's `all.h` expects a
/// generated `config.h` defining `VERSION` and the default `Target`
/// (`Deftgt`). Hardcoded to arm64_apple — see CORE_SOURCES comment.
fn write_config_h(out_dir: &Path) {
    let contents = "#define VERSION \"qbe+macvm-rust-jit\"\n#define Deftgt T_arm64_apple\n";
    fs::write(out_dir.join("config.h"), contents).expect("failed to write generated config.h");
}
