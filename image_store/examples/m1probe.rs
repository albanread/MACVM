//! M1 fidelity gate for the text editor (`docs/editor_design.md` §5, milestone
//! M1): every class in the real world image must render via `class_source` and
//! parse BACK to byte-identical stored method sources. If this drifts, the
//! editor's accept path would rewrite every method on every accept — a deopt
//! storm and a version-history flood. Run against the checked-in image:
//!
//!     cargo run -p image_store --example m1probe
//!
//! Exits non-zero on any drift. Complements the `class_source_round_trips_*`
//! unit test (which pins the property on a synthetic image with no dependency
//! on `world/image.sqlite3` being present).
fn main() {
    let path = std::path::Path::new("world/image.sqlite3");
    if !path.exists() {
        eprintln!("m1probe: {} not found (run from the repo root)", path.display());
        std::process::exit(2);
    }
    let img = image_store::Image::open(path).unwrap();
    let names = img.class_names().unwrap();
    let (mut ok, mut bad, mut methods) = (0usize, 0usize, 0usize);
    for name in &names {
        let text = match img.class_source(name).unwrap() {
            Some(t) => t,
            None => continue,
        };
        let parsed = image_store::mst::parse_mst_source(&text);
        let stored = img.all_methods_of(name).unwrap();
        if parsed.len() != 1 {
            println!("BAD {name}: parsed {} blocks", parsed.len());
            bad += 1;
            continue;
        }
        let pc = &parsed[0];
        let mut class_ok = true;
        for s in &stored {
            let want_cls = s.side == image_store::Side::Class;
            match pc.methods.iter().find(|m| m.selector == s.selector && m.is_class_side == want_cls) {
                None => {
                    println!("BAD {name}>>{}: missing after re-parse", s.selector);
                    class_ok = false;
                }
                Some(g) if g.source != s.source => {
                    println!("BAD {name}>>{}: source drifted", s.selector);
                    class_ok = false;
                }
                _ => {}
            }
            methods += 1;
        }
        if pc.methods.len() != stored.len() {
            println!("BAD {name}: {} parsed vs {} stored methods", pc.methods.len(), stored.len());
            class_ok = false;
        }
        if class_ok { ok += 1 } else { bad += 1 }
    }
    println!("M1: {ok}/{} classes round-trip clean ({methods} methods checked), {bad} bad", names.len());
    std::process::exit(if bad == 0 { 0 } else { 1 });
}
