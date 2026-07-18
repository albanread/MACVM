//! The RUSTTCL control channel — `macvm-cocoa` made drivable from a script.
//!
//! Opt-in (`MACVM_COCOA_CTL=<port>`), loopback-only: a listener thread accepts
//! one line-of-command connection at a time from `macvm rusttcl`'s `gui` verb
//! and forwards each request to the MAIN thread through the existing
//! default-mode drain (the same wake the supervisor uses), where it runs
//! against the UI worker VM exactly like any other main-thread work. The
//! listener never touches AppKit or the VM itself — it only queues, wakes,
//! and relays the reply.
//!
//! Protocol (both directions): `<len>\n<len bytes>`, one request in flight per
//! connection. Requests:
//!   `eval <smalltalk>` → `OK <printString>` / `ERR <error>`
//!   `doit <smalltalk>` → `OK` / `ERR <error>`
//!   `snap <path>`      → `OK` / `ERR no window` (client-area PNG, main-thread)
//!   `sleep <ms>`       → `OK` (listener-side pause — lets scripts wait out an
//!                         async reply without a Tcl sleep verb)
//!   `ping`             → `OK pong`
//!
//! This exists because "only a human can look at it" is false the moment the
//! screen is scriptable: `gui view browser`, `gui snap out.png`, read the PNG.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::time::Duration;

/// One queued request: the command line and the channel the drain answers on.
pub struct CtlReq {
    pub cmd: String,
    pub reply: SyncSender<String>,
}

/// Read one `<len>\n<bytes>` frame.
fn read_frame(s: &mut TcpStream) -> std::io::Result<Option<String>> {
    let mut len_line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match s.read(&mut byte)? {
            0 => return Ok(None), // clean disconnect
            _ => {
                if byte[0] == b'\n' {
                    break;
                }
                len_line.push(byte[0]);
            }
        }
    }
    let len: usize = String::from_utf8_lossy(&len_line)
        .trim()
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad frame length"))?;
    let mut buf = vec![0u8; len];
    s.read_exact(&mut buf)?;
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

fn write_frame(s: &mut TcpStream, msg: &str) -> std::io::Result<()> {
    s.write_all(format!("{}\n", msg.len()).as_bytes())?;
    s.write_all(msg.as_bytes())?;
    s.flush()
}

/// Start the listener if `MACVM_COCOA_CTL` is set; answer the receiver the
/// drain serves. `wake` is the run-loop poke (the drain's own wake fn).
pub fn start(wake: std::sync::Arc<dyn Fn() + Send + Sync>) -> Option<Receiver<CtlReq>> {
    let port: u16 = match std::env::var("MACVM_COCOA_CTL") {
        Ok(s) if !s.is_empty() => match s.trim().parse() {
            Ok(p) => p,
            Err(_) => {
                eprintln!("macvm-cocoa: MACVM_COCOA_CTL={s} is not a port — control channel off");
                return None;
            }
        },
        _ => return None,
    };
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("macvm-cocoa: control bind 127.0.0.1:{port} failed: {e}");
            return None;
        }
    };
    eprintln!("macvm-cocoa: control channel on 127.0.0.1:{port} (rusttcl `gui connect {port}`)");
    let (tx, rx) = sync_channel::<CtlReq>(16);
    let _ = std::thread::Builder::new()
        .name("macvm-cocoa-ctl".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                loop {
                    let cmd = match read_frame(&mut conn) {
                        Ok(Some(c)) => c,
                        _ => break, // disconnect / bad frame — next connection
                    };
                    // `sleep` is served HERE (a script pacing aid): the main
                    // thread must stay free to run the drain being waited on.
                    if let Some(ms) = cmd.strip_prefix("sleep ") {
                        let ms: u64 = ms.trim().parse().unwrap_or(0);
                        std::thread::sleep(Duration::from_millis(ms));
                        let _ = write_frame(&mut conn, "OK");
                        continue;
                    }
                    let (rtx, rrx) = sync_channel::<String>(1);
                    if tx.send(CtlReq { cmd, reply: rtx }).is_err() {
                        let _ = write_frame(&mut conn, "ERR control queue gone");
                        return;
                    }
                    wake();
                    let reply = rrx
                        .recv_timeout(Duration::from_secs(30))
                        .unwrap_or_else(|_| "ERR timeout (main thread busy?)".to_string());
                    if write_frame(&mut conn, &reply).is_err() {
                        break;
                    }
                }
            }
        });
    Some(rx)
}

/// Serve every queued request against the UI worker — called from the
/// default-mode drain on the MAIN thread.
pub fn serve(rx: &Receiver<CtlReq>, ui: &mut macvm::embed::VmHandle) {
    while let Ok(req) = rx.try_recv() {
        let reply = if let Some(src) = req.cmd.strip_prefix("eval ") {
            match ui.eval(src) {
                Ok(v) => format!("OK {v}"),
                Err(e) => format!("ERR {e}"),
            }
        } else if let Some(src) = req.cmd.strip_prefix("doit ") {
            match ui.exec(src) {
                Ok(()) => "OK".to_string(),
                Err(e) => format!("ERR {e}"),
            }
        } else if let Some(path) = req.cmd.strip_prefix("snap ") {
            if crate::objc::snapshot_client_area(path.trim()) {
                "OK".to_string()
            } else {
                "ERR no window".to_string()
            }
        } else if req.cmd == "ping" {
            "OK pong".to_string()
        } else {
            format!("ERR unknown command: {}", req.cmd)
        };
        let _ = req.reply.send(reply);
    }
}
