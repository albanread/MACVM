//! CLI entry point for the offline FFI generator (`../lib.rs`, `docs/FFI.md`
//! S20). Mirrors `image_store/src/bin/import_world.rs`'s shape: a small
//! binary over the library crate, run by hand or from a future build step,
//! never by the interpreter/compiler.

use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    let db_arg = env::args().nth(1);
    let db = match ffi_gen::Db::open(db_arg.as_deref()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("generate_ffi: {e}");
            return ExitCode::FAILURE;
        }
    };

    let (cocoa, posix, structs) = ffi_gen::seed_manifest();

    let cocoa_text = match ffi_gen::generate_cocoa_bindings(&db, &cocoa) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("generate_ffi: generating Cocoa bindings: {e}");
            return ExitCode::FAILURE;
        }
    };
    let posix_text = match ffi_gen::generate_posix_bindings(&db, &posix) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("generate_ffi: generating POSIX bindings: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut struct_text = String::new();
    for name in structs {
        match ffi_gen::generate_struct_accessor_class(&db, name) {
            Ok(t) => struct_text.push_str(&t),
            Err(e) => {
                eprintln!("generate_ffi: generating struct accessor for {name}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    print!(
        "{}{struct_text}\n{cocoa_text}\n{posix_text}",
        ffi_gen::GENERATED_HEADER
    );
    eprintln!(
        "generate_ffi: {} Cocoa method(s), {} POSIX function(s), {} struct(s) — forward-declared, not yet callable (docs/FFI.md §6.3)",
        cocoa.len(),
        posix.len(),
        structs.len()
    );
    ExitCode::SUCCESS
}
