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
}

static STATE: Mutex<InputState> = Mutex::new(InputState {
    keys: 0,
    mouse_x: -1,
    mouse_y: -1,
    buttons: 0,
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

/// No pane, no pointer — what a closed session reports.
pub fn clear_mouse() {
    set_mouse(-1, -1, 0);
}

/// Everything back to rest. A new session must not inherit the last one's held
/// keys: a demo that ended with Left down would otherwise start walking.
pub fn reset() {
    with(|s| *s = InputState {
        keys: 0,
        mouse_x: -1,
        mouse_y: -1,
        buttons: 0,
    });
}

/// This instant, coherently — the read a demo's tick makes.
pub fn snapshot() -> InputState {
    with(|s| *s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract in one test: what was written is what a reader sees, all
    /// four fields from the same instant, and a reset really rests.
    #[test]
    fn a_snapshot_is_one_coherent_instant() {
        reset();
        set_keys(0b101);
        set_mouse(120, 64, 1);
        let s = snapshot();
        assert_eq!(
            s,
            InputState {
                keys: 0b101,
                mouse_x: 120,
                mouse_y: 64,
                buttons: 1
            }
        );
        reset();
        let s = snapshot();
        assert_eq!(s.keys, 0, "a new session inherits no held keys");
        assert_eq!(s.mouse_x, -1, "no pane means no pointer");
    }
}
