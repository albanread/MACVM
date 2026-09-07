//! GAME INPUT — a process service (`docs/process_services.md` S6).
//!
//! The census in that record listed "game command queue + input atomics" among
//! the process-level state that already behaved like a service — global,
//! main-written, VM-independent — and marked it *annexed later*. This is the
//! annexation, and it is what lets a demo run in a VM of its own.
//!
//! WHY IT HAD TO MOVE. The atomics lived in `cocoa_gui`, which the VM cannot
//! see, so guest code could not ask what the keyboard was doing. Input reached
//! a demo exactly one way: the main-thread frame timer stored a mask, the
//! PRIMARY's supervisor loop read it, formatted `GamePane stepWithKeys: 4
//! mouseX: 100 y: 200 buttons: 1` as a STRING, and exec'd that string on the
//! primary. The numbers were baked into source. That string-exec was the whole
//! coupling between a game and the primary's thread: not the frame clock (the
//! timer service replaced that), not the pixels (the command queue was always
//! global) — the input.
//!
//! Here it is state the process owns and any VM may read, so a demo's own tick
//! can ASK instead of being TOLD, and the primary stops being a frame pump.
//!
//! THE SNAPSHOT IS ONE READ, deliberately. All four values are taken together
//! under one lock, because a frame that mixes this tick's keys with last
//! tick's mouse is a bug nobody would ever find — and reading them together is
//! exactly what the string-formatting code did, by accident of being one
//! `format!`. A mutex rather than four atomics for the same reason: the
//! coherence IS the contract.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

/// One instant of input. Position is in PANE PIXELS (the frame tick converts
/// from view points once, since it alone knows this session's pane size);
/// `-1` means "no pane, nothing sensible to report". Buttons is a bitmask,
/// bit 0 = left, bit 1 = right; keys is the held-key mask, bit 0 = Left … 5 = B.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputState {
    pub keys: i64,
    pub mouse_x: i64,
    pub mouse_y: i64,
    pub buttons: i64,
    /// THE USER ASKED A DEMO TO STOP — Escape, or the pane's close button. It
    /// rides with the input because that is what it IS: a key the user
    /// pressed, read by the demo's own tick like every other key. A demo in a
    /// VM of its own then ENDS ITSELF (`DemoVmClient`) instead of being killed
    /// from outside — the difference between a program that quits and one that
    /// is shot.
    ///
    /// A BOOLEAN WAS THE WRONG SHAPE, and shipped broken for exactly one
    /// commit: the only thing that cleared it was the IN-PROCESS launch path,
    /// so after a single Escape every demo launched into its own VM read a
    /// stale `true` on its first beat and shut down instantly. Demos "refused
    /// to open" — the author saw it before any test did.
    ///
    /// So the truth is [`exit_generation`], which only ever counts UP: a demo
    /// records it at birth and reacts when it has moved. A request raised
    /// before this demo existed cannot be its request — which is what "the
    /// user asked THIS demo to stop" always meant. The flag below is kept for
    /// a reader that just wants "was a stop asked during this session".
    pub exit_requested: bool,
    /// How many stop requests this process has ever seen. Monotonic and NEVER
    /// reset — a comparison that can go backwards is a bug waiting for a
    /// second demo.
    pub exit_generation: u64,
}

/// WHICH PANE OWNS THE KEYBOARD (`docs/multi_pane_design.md` §2c).
///
/// Input is a process service: there is one keyboard and one pointer, so the
/// snapshot below is the truth for the machine. What was missing was a
/// SUBJECT — `primInputState` took no key, so every VM that asked got the same
/// answer, and two demos would both have answered to the same keystrokes.
///
/// The rule adopted is the one every window system already implements: **the
/// focused window owns the keyboard, and the pointer belongs to the window
/// under it.** An unfocused demo reads no keys and a pointer outside its pane,
/// which is exactly right — a background game must not respond to typing aimed
/// at the Browser.
///
/// `0` means "no pane has claimed focus", and every asker is answered in full.
/// That is the headless and single-window case, and it is why this can land
/// without changing anything that works today.
static FOCUSED_PANE: AtomicU32 = AtomicU32::new(0);

/// The host tells the service who has focus — on pane creation now, and on
/// `windowDidBecomeKey:` once panes can coexist.
pub fn set_focus(pane: u32) {
    FOCUSED_PANE.store(pane, Ordering::Release);
}

pub fn focused_pane() -> u32 {
    FOCUSED_PANE.load(Ordering::Acquire)
}

/// This instant's input AS SEEN BY `pane` — the read behind primitive 279.
///
/// The stop request is deliberately NOT gated here. `exit_generation` is
/// monotonic by design ("a comparison that can go backwards is a bug waiting
/// for a second demo"), and blanking it for an unfocused pane would hand a demo
/// a number that had gone backwards. Escape is still session-wide, and becomes
/// per-pane when sessions do — the last step of the design, where the host
/// stops asking "the" demo to quit and asks the focused one.
pub fn snapshot_for(pane: u32) -> InputState {
    let live = snapshot();
    let focus = focused_pane();
    // THE STOP IS THIS PANE'S OWN. Once several demos can be open, a stop
    // aimed at one of them must not end all of them, so a pane reports the
    // count at which IT was asked — 0 until it ever is. Pane 0 (headless, or a
    // VM with no sink) keeps the process-wide answer it always had.
    let mine = if pane == 0 {
        live
    } else {
        let gen = pane_exit_generation(pane);
        InputState {
            exit_requested: gen > 0,
            exit_generation: gen,
            ..live
        }
    };
    if pane == 0 || focus == 0 || pane == focus {
        return mine;
    }
    InputState {
        keys: 0,
        mouse_x: -1,
        mouse_y: -1,
        buttons: 0,
        ..mine
    }
}

static STATE: Mutex<InputState> = Mutex::new(InputState {
    keys: 0,
    mouse_x: -1,
    mouse_y: -1,
    buttons: 0,
    exit_requested: false,
    exit_generation: 0,
});

fn with<R>(f: impl FnOnce(&mut InputState) -> R) -> R {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut g)
}

/// The frame tick's held-key mask for this instant.
pub fn set_keys(keys: i64) {
    with(|s| s.keys = keys);
}

/// The pointer, in pane pixels, with its button mask.
pub fn set_mouse(x: i64, y: i64, buttons: i64) {
    with(|s| {
        s.mouse_x = x;
        s.mouse_y = y;
        s.buttons = buttons;
    });
}

/// The user asked the running demo to stop (Escape, the close button). Read
/// by the demo itself; cleared when the next session resets.
pub fn request_exit() {
    with(|s| {
        s.exit_requested = true;
        s.exit_generation += 1;
    });
}

/// WHICH PANE HAS BEEN ASKED TO STOP, and at which count.
///
/// Escape belongs to the focused window like every other key, so with several
/// demos open a stop must reach ONE of them. The global count above still only
/// ever rises (it is the process's tally of stop requests); this records the
/// value at which a particular pane was asked, and `snapshot_for` reports that
/// pane's own number.
///
/// A demo reads its birth value — 0, "never asked" — and reacts when it moves,
/// exactly as before. Per pane the sequence is still monotonic, which is the
/// property the whole design rests on.
static PANE_EXIT: Mutex<BTreeMap<u32, u64>> = Mutex::new(BTreeMap::new());

/// Ask ONE pane to stop. `0` means "whatever is running" and behaves as the
/// process-wide request always did — the headless and single-window case.
pub fn request_exit_for(pane: u32) {
    request_exit();
    if pane != 0 {
        let gen = snapshot().exit_generation;
        let mut guard = PANE_EXIT.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(pane, gen);
    }
}

fn pane_exit_generation(pane: u32) -> u64 {
    let guard = PANE_EXIT.lock().unwrap_or_else(|e| e.into_inner());
    guard.get(&pane).copied().unwrap_or(0)
}

/// Forget a pane's stop record — it has gone, and a later pane must not
/// inherit an answer meant for it. (Ids are never reused, so this is hygiene
/// rather than correctness, but an unbounded map is its own defect.)
pub fn forget_pane(pane: u32) {
    let mut guard = PANE_EXIT.lock().unwrap_or_else(|e| e.into_inner());
    guard.remove(&pane);
}

/// No pane, no pointer — what a closed session reports.
pub fn clear_mouse() {
    set_mouse(-1, -1, 0);
}

/// Everything back to rest. A new session must not inherit the last one's held
/// keys: a demo that ended with Left down would otherwise start walking.
pub fn reset() {
    with(|s| {
        // The generation SURVIVES a reset: it is the process's count of stop
        // requests, not this session's state, and a demo compares against a
        // value it captured earlier.
        let gen = s.exit_generation;
        *s = InputState {
            keys: 0,
            mouse_x: -1,
            mouse_y: -1,
            buttons: 0,
            exit_requested: false,
            exit_generation: gen,
        };
    });
}

/// This instant, coherently — the read a demo's tick makes.
pub fn snapshot() -> InputState {
    with(|s| *s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE FOCUS RULE, pinned. The keyboard belongs to the focused pane and
    /// the pointer to the window under it, so an unfocused demo must read no
    /// keys and a pointer outside — otherwise two demos would both answer to
    /// typing aimed at one of them (or at the Browser).
    #[test]
    fn only_the_focused_pane_reads_the_keyboard() {
        let _guard = FOCUS_TEST_LOCK.lock().unwrap();
        reset();
        set_focus(0);
        set_keys(0b1011);
        set_mouse(120, 64, 1);

        // Nobody has claimed focus: every asker reads the keyboard and the
        // pointer in full. This is the headless and single-window case, and
        // why the rule can land without disturbing anything that worked.
        // (The exit half is compared separately below — it became PER PANE
        // when Escape learned to target one demo.)
        assert_eq!(snapshot_for(0), snapshot());
        let unfocused = snapshot_for(9);
        assert_eq!(unfocused.keys, snapshot().keys);
        assert_eq!(unfocused.mouse_x, snapshot().mouse_x);
        assert_eq!(unfocused.buttons, snapshot().buttons);

        set_focus(7);
        let mine = snapshot_for(7);
        assert_eq!(mine.keys, 0b1011, "the focused pane reads the keyboard");
        assert_eq!((mine.mouse_x, mine.mouse_y), (120, 64));
        assert_eq!(mine.buttons, 1);

        let theirs = snapshot_for(8);
        assert_eq!(theirs.keys, 0, "an unfocused pane reads no keys");
        assert_eq!(
            (theirs.mouse_x, theirs.mouse_y),
            (-1, -1),
            "and a pointer that is outside it"
        );
        assert_eq!(theirs.buttons, 0);

        // The pane asking as 0 — a VM with no sink, or one that never claimed
        // a pane — is still answered in full rather than silenced.
        assert_eq!(snapshot_for(0).keys, 0b1011);
        set_focus(0);
    }

    /// A STOP REACHES ONE DEMO. Escape belongs to the focused window like any
    /// other key, so with several panes open the demo that was asked must end
    /// and the others must not notice.
    ///
    /// This replaces an earlier test that pinned the opposite — that every
    /// pane saw the process-wide count — which was the honest contract while
    /// only one demo could exist, and said so. Per-pane sessions changed it.
    #[test]
    fn a_stop_reaches_only_the_pane_it_was_asked_of() {
        let _guard = FOCUS_TEST_LOCK.lock().unwrap();
        reset();
        forget_pane(7);
        forget_pane(8);
        set_focus(7);

        // Nobody has been asked yet: every pane's own count is at rest, which
        // is what a demo records at birth.
        assert_eq!(snapshot_for(7).exit_generation, 0);
        assert_eq!(snapshot_for(8).exit_generation, 0);
        assert!(!snapshot_for(7).exit_requested);

        request_exit_for(7);

        let asked = snapshot_for(7);
        assert!(asked.exit_requested, "the pane asked to stop knows it");
        assert!(
            asked.exit_generation > 0,
            "and its own count moved, which is what a demo watches"
        );

        let bystander = snapshot_for(8);
        assert!(
            !bystander.exit_requested,
            "a demo nobody asked must keep running"
        );
        assert_eq!(
            bystander.exit_generation, 0,
            "its count has not moved — and per pane it only ever rises"
        );

        // Pane 0 — headless, or a VM with no sink — keeps the process-wide
        // answer it always had.
        assert!(snapshot_for(0).exit_requested);
        forget_pane(7);
        set_focus(0);
    }

    /// The two focus tests share one process-wide service, so they must not
    /// interleave — the same reason the audio tests serialize.
    static FOCUS_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// The contract in one test: what was written is what a reader sees, all
    /// four fields from the same instant, and a reset really rests.
    #[test]
    fn a_snapshot_is_one_coherent_instant() {
        // Same lock as the focus tests: one process-wide service, so a test
        // that writes to it cannot run beside another that reads it. (This
        // test predates the lock and raced the moment a second writer existed.)
        let _guard = FOCUS_TEST_LOCK.lock().unwrap();
        reset();
        // The generation is the PROCESS's tally and `reset` deliberately keeps
        // it (see the assertion at the end of this test). It is therefore
        // whatever earlier stops left behind — not zero — so the coherent
        // instant below is compared against the count as it stands. Hard-coding
        // 0 here only ever held while this test happened to run first, which is
        // an order dependency rather than a contract.
        let carried = snapshot().exit_generation;
        set_keys(0b101);
        set_mouse(120, 64, 1);
        let s = snapshot();
        assert_eq!(
            s,
            InputState {
                keys: 0b101,
                mouse_x: 120,
                mouse_y: 64,
                buttons: 1,
                exit_requested: false,
                exit_generation: carried
            }
        );
        reset();
        let s = snapshot();
        assert_eq!(s.keys, 0, "a new session inherits no held keys");
        assert_eq!(s.mouse_x, -1, "no pane means no pointer");
        let before = snapshot().exit_generation;
        request_exit();
        assert!(snapshot().exit_requested, "the stop key is readable");
        assert_eq!(snapshot().exit_generation, before + 1, "the count moved");
        reset();
        assert!(!snapshot().exit_requested, "a new session starts unasked");
        assert_eq!(
            snapshot().exit_generation,
            before + 1,
            "the generation must SURVIVE a reset — a demo born after this \
             request compares against it, and a count that goes backwards \
             makes every later demo exit on its first beat"
        );
    }
}
