//! Standalone preview/validator for `<asm: '...'>` method bodies
//! (`docs/ASM.md` §7, S23). Extracts the pragma's assembly text — or accepts
//! a raw `.s`-style file as-is — and runs it through the real, already-
//! tested `wfasm` assembler (`src/vendor/wfasm`). Proves the mechanism
//! without installing anything into a running VM or touching any part of
//! the real frontend/compiler pipeline: this file and its one `[[bin]]`
//! `Cargo.toml` entry are the only things it adds.

use std::env;
use std::fs;
use std::process::ExitCode;

use macvm::vendor::wfasm::a64;

/// Pull the quoted text out of a `<asm: '...'>` pragma, un-escaping `''` to
/// `'` (Smalltalk's own string-literal escape — the same convention
/// `image_store::mst`'s scanner uses). Returns `None` if no pragma is found
/// (including an unterminated literal), so callers can fall back to
/// treating the whole input as raw assembly text.
fn extract_asm_pragma(source: &str) -> Option<String> {
    let start = source.find("<asm:")?;
    let after_tag = &source[start + "<asm:".len()..];
    let quote_start = after_tag.find('\'')?;
    let body = &after_tag[quote_start + 1..];

    let mut out = String::new();
    let mut rest = body;
    loop {
        let i = rest.find('\'')?;
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        if rest.starts_with('\'') {
            // `''` is an escaped quote inside the literal.
            out.push('\'');
            rest = &rest[1..];
        } else {
            return Some(out);
        }
    }
}

/// Registers `docs/ASM.md` §4 reserves for a v1 ASM method: IP scratch, the
/// platform register, and every VM/callee-saved register a leaf body must
/// not touch. `x0`-`x15` (receiver/args/scratch) are deliberately absent.
///
/// Best-effort — a whole-word text scan over the (comment-stripped) source,
/// not a real operand parser, so it can't tell "touches" from "merely
/// mentions" the way a full decoder could. Cheap, and catches the obvious
/// mistake; not a formal verifier.
const RESERVED_REGISTERS: &[&str] = &[
    "x16", "w16", "x17", "w17", // IP0/IP1 veneer scratch
    "x18", "w18", // platform-reserved
    "x19", "w19", "x20", "w20", "x21", "w21", "x22", "w22", "x23", "w23", "x24", "w24", "x25",
    "w25", "x26", "w26", "x27", "w27", "x28", "w28", "x29", "w29", "fp", "x30", "w30", "lr", "sp",
    "wsp",
];

fn lint_reserved_registers(asm_text: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for (lineno, raw) in asm_text.lines().enumerate() {
        let clean = a64::parse::strip_comment(raw);
        for token in clean.split(|c: char| !c.is_ascii_alphanumeric()) {
            if token.is_empty() {
                continue;
            }
            let lower = token.to_ascii_lowercase();
            if RESERVED_REGISTERS.contains(&lower.as_str()) {
                hits.push(format!(
                    "line {}: `{}` touches reserved register `{token}` (docs/ASM.md §4)",
                    lineno + 1,
                    raw.trim(),
                ));
            }
        }
    }
    hits
}

fn main() -> ExitCode {
    let path = match env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: asm_preview <file.mst-or-.s>");
            eprintln!(
                "  reads a `<asm: '...'>` method (extracts the pragma text) or a raw \
                 assembly file (used as-is), assembles it via src/vendor/wfasm, and \
                 reports the result. See docs/ASM.md §7."
            );
            return ExitCode::FAILURE;
        }
    };
    let source = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("asm_preview: reading {path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let (asm_text, source_kind) = match extract_asm_pragma(&source) {
        Some(text) => (text, "<asm: '...'> pragma"),
        None => (source.clone(), "raw assembly (no <asm: ...> pragma found)"),
    };
    eprintln!("asm_preview: assembling {source_kind} from {path}");

    let warnings = lint_reserved_registers(&asm_text);
    for w in &warnings {
        eprintln!("warning: {w}");
    }

    match a64::assemble(&asm_text) {
        Ok(module) => {
            println!("OK: {} bytes encoded", module.code.len());
            println!(
                "hex: {}",
                module
                    .code
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            if module.symbols.is_empty() {
                println!("symbols: (none)");
            } else {
                println!("symbols:");
                for (name, off) in &module.symbols {
                    println!("  {name} @ {off:#x}");
                }
            }
            if module.relocs.is_empty() {
                println!("relocs: (none)");
            } else {
                println!("relocs:");
                for r in &module.relocs {
                    println!(
                        "  {:?} at {:#x} -> {} (+{})",
                        r.kind, r.at, r.target, r.addend
                    );
                }
            }
            if module.externs.is_empty() {
                println!("externs: (none)");
            } else {
                println!("externs: {}", module.externs.join(", "));
            }
            if !warnings.is_empty() {
                eprintln!(
                    "asm_preview: {} reserved-register warning(s) — see docs/ASM.md §4",
                    warnings.len()
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_simple_pragma() {
        let src = "bitAnd: aSmallInteger [\n    <asm: 'and x0, x0, x1\nret'>\n]";
        let text = extract_asm_pragma(src).unwrap();
        assert_eq!(text, "and x0, x0, x1\nret");
    }

    #[test]
    fn extracts_pragma_with_escaped_quote() {
        let src = "<asm: 'mov x0, #0 // it''s fine\nret'>";
        let text = extract_asm_pragma(src).unwrap();
        assert_eq!(text, "mov x0, #0 // it's fine\nret");
    }

    #[test]
    fn falls_back_to_raw_text_when_no_pragma() {
        assert_eq!(extract_asm_pragma("and x0, x0, x1\nret"), None);
    }

    #[test]
    fn unterminated_literal_yields_none() {
        assert_eq!(extract_asm_pragma("<asm: 'and x0, x0, x1"), None);
    }

    #[test]
    fn bitand_assembles_to_two_instructions() {
        let text = extract_asm_pragma("<asm: 'and x0, x0, x1\nret'>").unwrap();
        let module = a64::assemble(&text).expect("bitAnd: must assemble");
        assert_eq!(module.code.len(), 8); // `and` + `ret`, 4 bytes each
    }

    #[test]
    fn bitor_and_bitxor_also_assemble() {
        for op in ["orr", "eor"] {
            let text = format!("{op} x0, x0, x1\nret");
            assert!(a64::assemble(&text).is_ok(), "{op} should assemble");
        }
    }

    #[test]
    fn lint_flags_reserved_register() {
        let hits = lint_reserved_registers("mov x28, #0\nret");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].contains("x28"));
    }

    #[test]
    fn lint_allows_general_registers() {
        let hits = lint_reserved_registers("and x0, x0, x1\nmov x5, x10\nret");
        assert!(hits.is_empty());
    }

    #[test]
    fn lint_ignores_reserved_names_inside_comments() {
        let hits = lint_reserved_registers("mov x0, x1 // uses x28 elsewhere\nret");
        assert!(hits.is_empty());
    }

    #[test]
    fn malformed_asm_reports_a_line_numbered_parse_error() {
        let err = a64::assemble("bogus_mnemonic x0, x1\nret").unwrap_err();
        assert!(format!("{err:#}").contains("line 1"));
    }
}
