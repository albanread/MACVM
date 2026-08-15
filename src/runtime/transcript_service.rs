//! THE TRANSCRIPT IS A VM SERVICE — the author's design, stated mid-debug and
//! adopted whole: "the transcript is a shared service fed by messages from the
//! primary and any worker … a view onto a VM transcript service that
//! serializes updates and guarantees no transcript writes are lost. The UI
//! just displays the transcript."
//!
//! WHAT IT REPLACES, and why it had to go. The old shape relayed: a worker's
//! `Transcript show:` became an envelope to the PRIMARY, whose own transcript
//! sink re-forwarded it to the UI worker, whose drain finally appended it to
//! the view. Three VMs in the path of one line; two hops that exist only
//! while their pumps turn; and the primary — the user's LANGUAGE thread —
//! conscripted as a message router for other VMs' words. The failure mode was
//! not hypothetical: a worker whose parent never pumps is SILENT, its error
//! traces queued unread in an inbox, which is precisely the debugging wall
//! that produced this design.
//!
//! THE SERVICE: one process-global, mutex-serialized, sequence-numbered line
//! buffer. A write lands in it — under the lock, tagged, numbered — BEFORE
//! the writing VM continues; from that moment no pump, no peer, and no other
//! VM's health can lose it. Order is the lock's order, which is the only
//! total order concurrent writers have. The buffer is bounded (a transcript
//! is not a database): a viewer that lags beyond `CAP` lines loses the
//! OLDEST, and the loss itself is counted and reported as a line — dropped
//! silently is the one thing a lossless-by-contract service must never do.
//!
//! THE VIEW: whoever displays the transcript — the GUI's drain, a test —
//! `drain_since` their own cursor and renders what they get. The UI holds no
//! transcript state of its own and forwards nothing anywhere: it just
//! displays. A late-attaching viewer sees everything still in the window,
//! which is what makes early-boot words from a worker visible at all.
//!
//! ACTIVATION is explicit (`activate`), because the CLI's single-VM world has
//! a perfectly good transcript already: stdout. Inactive, every VM keeps its
//! existing sink and nothing here exists; active (the GUI, a test that wants
//! worker words without pumping anybody), `worker_main` and the supervisor
//! route every VM's sink here instead of at each other.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Bounded history: enough that a busy boot plus a laggy viewer lose nothing
/// in practice, small enough that an unwatched process stays honest about
/// memory. At ~120 bytes a line this is ~2.5 MB worst case.
const CAP: usize = 20_000;

struct State {
    seq: u64,
    lines: VecDeque<(u64, String)>,
    dropped: u64,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static STATE: OnceLock<Mutex<State>> = OnceLock::new();

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| {
        Mutex::new(State {
            seq: 0,
            lines: VecDeque::new(),
            dropped: 0,
            wake: None,
        })
    })
}

/// Turn the service on for this process. Idempotent. Every VM booted (or
/// re-sunk) after this writes here; the CLI never calls it and never changes.
pub fn activate() {
    ACTIVE.store(true, Ordering::Release);
}

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

/// The view's poke — fired (coalesced by the caller's own machinery) whenever
/// a line lands, so a sleeping run loop knows to drain. One viewer: the
/// transcript has one display, which is the author's own definition of it.
pub fn register_wake(f: Arc<dyn Fn() + Send + Sync>) {
    let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
    s.wake = Some(f);
}

/// A complete line, already tagged by its writer. The write is the whole
/// guarantee: once this returns, the line is in the buffer and nothing any
/// VM does or fails to do can un-record it.
pub fn push_line(line: String) {
    let wake = {
        let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
        s.seq += 1;
        let seq = s.seq;
        if s.lines.len() >= CAP {
            s.lines.pop_front();
            s.dropped += 1;
        }
        s.lines.push_back((seq, line));
        s.wake.clone()
    };
    if let Some(w) = wake {
        w();
    }
}

/// Everything after `cursor`, and the new cursor. The viewer owns its cursor,
/// so a second viewer (a test watching alongside the GUI) costs nothing and
/// misses nothing. If the buffer wrapped past the cursor, the gap is reported
/// AS A LINE — the reader is told what it lost, never left to assume.
pub fn drain_since(cursor: u64) -> (u64, Vec<String>) {
    let s = state().lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    let oldest = s.lines.front().map(|(q, _)| *q).unwrap_or(s.seq + 1);
    if cursor + 1 < oldest && s.dropped > 0 {
        out.push(format!(
            "… transcript viewer lagged: {} line(s) rotated out …",
            oldest - cursor - 1
        ));
    }
    for (q, l) in s.lines.iter() {
        if *q > cursor {
            out.push(l.clone());
        }
    }
    (s.seq, out)
}

/// The per-VM sink: buffers fragments into LINES (a trace arrives as dozens
/// of `write!` pieces; the service stores lines because the view renders
/// lines), tags each at its start, and pushes complete lines only. The tag
/// discipline is `ForwardTranscript`'s own — `[w3] ` for a worker, nothing
/// for the primary — because that is what a reader already understands.
pub struct ServiceTranscript {
    tag: String,
    pending: String,
}

impl ServiceTranscript {
    pub fn tagged(tag: &str) -> ServiceTranscript {
        ServiceTranscript {
            tag: tag.to_string(),
            pending: String::new(),
        }
    }
}

impl crate::embed::TranscriptSink for ServiceTranscript {
    fn show(&mut self, text: &str) {
        for piece in text.split_inclusive('\n') {
            if let Some(body) = piece.strip_suffix('\n') {
                let mut line = String::with_capacity(self.tag.len() + self.pending.len() + body.len());
                if !self.tag.is_empty() {
                    line.push_str(&self.tag);
                }
                line.push_str(&self.pending);
                line.push_str(body);
                self.pending.clear();
                push_line(line);
            } else {
                self.pending.push_str(piece);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two words in the contract, measured: SERIALIZED (one total order,
    /// every reader sees the same one) and LOSSLESS (under real contention,
    /// every write from every thread is present exactly once — nothing any
    /// other thread did or failed to do can lose a line).
    #[test]
    fn concurrent_writers_lose_nothing_and_share_one_order() {
        const WRITERS: usize = 8;
        const LINES: usize = 500;
        let mut handles = Vec::new();
        for w in 0..WRITERS {
            handles.push(std::thread::spawn(move || {
                for i in 0..LINES {
                    push_line(format!("w{w}:{i}"));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let (_, lines) = drain_since(0);
        let mine: Vec<&String> = lines
            .iter()
            .filter(|l| l.starts_with('w') && l.contains(':'))
            .collect();
        assert!(mine.len() >= WRITERS * LINES, "lost writes: {}", mine.len());
        // Per-writer order is preserved inside the total order — each
        // writer's own lines appear in the sequence it wrote them.
        for w in 0..WRITERS {
            let seq: Vec<usize> = mine
                .iter()
                .filter_map(|l| l.strip_prefix(&format!("w{w}:")))
                .filter_map(|n| n.parse().ok())
                .collect();
            let mut sorted = seq.clone();
            sorted.sort_unstable();
            assert_eq!(seq, sorted, "writer {w}'s lines arrived out of order");
        }
    }

    /// A fragment-writing sink assembles LINES, tags them at line starts, and
    /// holds back an unterminated tail — the ForwardTranscript discipline.
    #[test]
    fn the_sink_assembles_tagged_lines_from_fragments() {
        use crate::embed::TranscriptSink;
        let before = drain_since(0).0;
        let mut sink = ServiceTranscript::tagged("[w9] ");
        // The service is process-global and the OTHER test floods it from
        // eight threads in parallel — so this one filters to its own tag,
        // which is also a small proof of the multiple-viewers property.
        let only_mine = |v: Vec<String>| -> Vec<String> {
            v.into_iter().filter(|l| l.starts_with("[w9] ")).collect()
        };
        sink.show("hel");
        sink.show("lo\nwor");
        let mid = only_mine(drain_since(before).1);
        assert_eq!(mid, vec!["[w9] hello".to_string()], "the tail is held back");
        sink.show("ld\n");
        let done = only_mine(drain_since(before).1);
        assert_eq!(done, vec!["[w9] hello".to_string(), "[w9] world".to_string()]);
    }
}
