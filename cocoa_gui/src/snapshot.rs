//! In-app client-area screenshot sequence — a development aid so on-screen work
//! is inspectable without attaching to a display. Enabled by the env var
//! `MACVM_COCOA_SNAP=<count>[:<secs>]` (default 16 shots, 2s apart); the output
//! directory is `MACVM_COCOA_SNAP_DIR` (default `/tmp`). Each capture hops onto
//! the main thread ([`crate::objc::snapshot_client_area`]) because AppKit view
//! rendering is main-only; the thread here only sleeps and requests.

use std::time::Duration;

/// Spawn the snapshot thread if `MACVM_COCOA_SNAP` is set. Call once, just before
/// `[NSApp run]` — the first shot waits one interval, by when the window is up.
pub fn start() {
    let spec = match std::env::var("MACVM_COCOA_SNAP") {
        Ok(s) if !s.is_empty() => s,
        _ => return,
    };
    let mut parts = spec.split(':');
    let count: u32 = parts
        .next()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(16);
    let interval: f64 = parts
        .next()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(2.0);
    let dir = std::env::var("MACVM_COCOA_SNAP_DIR").unwrap_or_else(|_| "/tmp".to_string());
    eprintln!("macvm-cocoa: snapshot sequence — {count} shots, {interval}s apart, into {dir}/");
    let _ = std::thread::Builder::new()
        .name("macvm-cocoa-snap".into())
        .spawn(move || {
            for i in 0..count {
                std::thread::sleep(Duration::from_secs_f64(interval));
                let path = format!("{dir}/macvm-cocoa-snap-{i:02}.png");
                let ok = crate::objc::snapshot_client_area(&path);
                eprintln!(
                    "macvm-cocoa: snap {i:02} -> {path} ({})",
                    if ok { "ok" } else { "no window yet" }
                );
            }
        });
}
