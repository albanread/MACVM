//! A profiler amplifier, not a test: boots the whole world 50 times in one
//! process so a 1 ms sampler (macOS `sample`) sees ~1.5 s of frontend work
//! instead of one 30 ms blip. `#[ignore]`d so it never runs in a normal
//! suite; drive it by hand:
//!
//!     cargo test --release --test boot_profile -- --ignored --nocapture &
//!     sample <pid> 2 -file /tmp/boot.prof
//!
//! Used for the frontend-speed review (2026-08-12). Kept because any future
//! "where does boot time go?" question needs the same amplifier.

use std::path::Path;

#[test]
#[ignore]
fn boot_fifty_times() {
    let t0 = std::time::Instant::now();
    for i in 0..50 {
        let mut vm = macvm::runtime::VmState::new();
        let loaded = macvm::frontend::world::load_world(&mut vm, Path::new("world"))
            .expect("world must load");
        assert!(loaded, "world.list must exist");
        if i == 0 {
            println!("first boot done");
        }
    }
    println!("50 boots in {:.2}s", t0.elapsed().as_secs_f64());
}
