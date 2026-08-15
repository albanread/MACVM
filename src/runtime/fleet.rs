//! THE FLEET — process services S4 (`docs/process_services.md` §5).
//!
//! The fleet registry — who exists, their links, the boot recipe, which peer
//! is the display — used to live INSIDE `WorkerState::Primary`, entangling
//! every fleet operation with one VM's health and one thread's attention:
//! only the primary's thread could spawn, a death was a letter in an unpumped
//! inbox, and the UI launching an app VM meant a relayed doit through the
//! language thread. This module is those fields moved to where they always
//! belonged: the process layer.
//!
//! WHAT THE PRIMARY KEEPS: its own inbox (a VM's mailbox is personal) and its
//! epoch (its identity). It REGISTERS the boot recipe and its parent inbox
//! here at birth, and its clean death REMOVES the whole epoch's entry — the
//! registry cleanup that previously never happened.
//!
//! WHAT MOVES: the link table (slot/generation allocation, send routing,
//! terminate, the poison broadcast), hosted-peer registration, the display
//! registration, and spawn itself — which any AUTHORIZED caller may now
//! invoke by epoch. Authorization is policy, not plumbing: a primary spawns
//! its own epoch; the registered display is granted the same (S4b); spawned
//! workers still cannot (`docs/process_services.md`: policy stays policy).
//!
//! Locking: one mutex over the whole fleet, held only for registry mutation
//! — never across a boot (a spawn inserts its link, then the worker thread
//! boots outside the lock), never while sending anything but an inbox push.

use std::sync::{Mutex, OnceLock};

use crate::runtime::workers::{
    handle_generation, handle_slot, make_handle, spawn_worker_thread, terminate_envelope,
    Envelope, HostedInbox, InboxSender, InboxWakeFn, WorkerBootFn, MAX_WORKERS,
};

pub(crate) struct FleetLink {
    pub inbox: InboxSender,
    pub alive: bool,
    pub gen: u32,
}

struct EpochFleet {
    links: Vec<FleetLink>,
    boot: WorkerBootFn,
    /// The primary's own inbox — every spawned worker's `to_primary`, and
    /// the destination for death notices, whoever asked for the spawn.
    parent_inbox: InboxSender,
    display: Option<u32>,
}

static FLEET: OnceLock<Mutex<Vec<(u64, EpochFleet)>>> = OnceLock::new();

fn fleet() -> &'static Mutex<Vec<(u64, EpochFleet)>> {
    FLEET.get_or_init(|| Mutex::new(Vec::new()))
}

/// A primary is born: its epoch's fleet exists from here.
pub(crate) fn register_epoch(epoch: u64, boot: WorkerBootFn, parent_inbox: InboxSender) {
    let mut f = fleet().lock().unwrap_or_else(|e| e.into_inner());
    f.retain(|(e, _)| *e != epoch);
    f.push((
        epoch,
        EpochFleet {
            links: Vec::new(),
            boot,
            parent_inbox,
            display: None,
        },
    ));
}

/// A primary died cleanly: its whole registry entry goes with it — the
/// cleanup that never used to happen. (A FATAL primary runs no Drop; its
/// entry is retired by the next `register_epoch` of a respawned generation
/// or lingers harmlessly — its workers self-reap by epoch as before.)
pub(crate) fn retire_epoch(epoch: u64) {
    let mut f = fleet().lock().unwrap_or_else(|e| e.into_inner());
    f.retain(|(e, _)| *e != epoch);
}

fn with_epoch<R>(epoch: u64, f: impl FnOnce(&mut EpochFleet) -> R) -> Option<R> {
    let mut g = fleet().lock().unwrap_or_else(|e| e.into_inner());
    g.iter_mut().find(|(e, _)| *e == epoch).map(|(_, ef)| f(ef))
}

/// Spawn a worker VM into `epoch`'s fleet — the kernel operation. The caller
/// no longer matters: the primary's own thread, the display's, a test's.
/// Slot reclaim + generation bump + the display introduction all happen
/// here, exactly as they did inside the primary.
pub fn spawn(epoch: u64, init: Option<String>, grant_spawn: bool) -> Option<u32> {
    // Registry mutation under the lock; thread creation after.
    let (id, rx, self_inbox, to_primary, boot, ui_link, intro_inbox) = {
        let mut g = fleet().lock().unwrap_or_else(|e| e.into_inner());
        let (_, ef) = g.iter_mut().find(|(e, _)| *e == epoch)?;
        let reuse_idx = ef.links.iter().position(|l| !l.alive);
        let (slot, gen) = match reuse_idx {
            Some(idx) => ((idx + 1) as u32, ef.links[idx].gen.wrapping_add(1)),
            None => {
                if ef.links.len() >= MAX_WORKERS {
                    return None;
                }
                (ef.links.len() as u32 + 1, 1)
            }
        };
        let id = make_handle(slot, gen);
        let (tx, rx) = std::sync::mpsc::channel::<Envelope>();
        let self_inbox = InboxSender::detached(tx.clone());
        let ui_link = ef.display.and_then(|h| {
            ef.links
                .get(handle_slot(h) as usize - 1)
                .filter(|l| l.gen == handle_generation(h) && l.alive)
                .map(|l| (h, l.inbox.clone()))
        });
        // The newborn's own inbox, for the display's introduction envelope —
        // which is SENT AFTER this lock is released (see below): it is a send,
        // and a send wakes somebody.
        let intro_inbox = InboxSender::detached(tx.clone());
        let link = FleetLink {
            inbox: InboxSender::detached(tx),
            alive: true,
            gen,
        };
        match reuse_idx {
            Some(idx) => ef.links[idx] = link,
            None => ef.links.push(link),
        }
        (
            id,
            rx,
            self_inbox,
            ef.parent_inbox.clone(),
            ef.boot.clone(),
            ui_link,
            intro_inbox,
        )
    };
    // The display learns the newcomer by the ordinary rule: an introduction
    // envelope carrying the newborn's inbox — sent with the registry lock
    // RELEASED, because the display's wake hook runs on this thread.
    if let Some((_, ui_inbox)) = &ui_link {
        let _ = ui_inbox.send(Envelope {
            from: id,
            corr: 0,
            bytes: Vec::new(),
            reply_to: Some(intro_inbox),
        });
    }
    spawn_worker_thread(
        id, epoch, rx, self_inbox, to_primary, boot, ui_link, init, grant_spawn,
    );
    Some(id)
}

/// Register an externally-hosted peer (the UI) in `epoch`'s fleet.
pub(crate) fn register_hosted(
    epoch: u64,
    wake: InboxWakeFn,
) -> Option<(u32, HostedInbox, InboxSender)> {
    with_epoch(epoch, |ef| {
        if ef.links.len() >= MAX_WORKERS {
            return None;
        }
        let slot = ef.links.len() as u32 + 1;
        let gen = 1;
        let id = make_handle(slot, gen);
        let (inbox, hosted) = InboxSender::hosted_pair(wake);
        ef.links.push(FleetLink {
            inbox,
            alive: true,
            gen,
        });
        Some((id, hosted, ef.parent_inbox.clone()))
    })
    .flatten()
}

/// Send from the parent's side: resolve `id` in `epoch`'s links.
/// NOTHING IS SENT UNDER THE FLEET LOCK. `InboxSender::send` fires the
/// receiver's wake hook — arbitrary code — and this mutex guards every fleet
/// operation there is, so a hook that spawned, terminated or even asked who
/// was alive would deadlock the whole registry. Every function below takes the
/// same shape: resolve under the lock, clone what is needed, release, then
/// send; re-acquire only to record what the send told us.
pub(crate) fn send_to(epoch: u64, id: u32, corr: u64, bytes: Vec<u8>) -> bool {
    let inbox = with_epoch(epoch, |ef| {
        let idx = handle_slot(id) as usize;
        if idx == 0 {
            return None;
        }
        let link = ef.links.get(idx - 1)?;
        if link.gen != handle_generation(id) || !link.alive {
            return None;
        }
        Some(link.inbox.clone())
    })
    .flatten();
    let Some(inbox) = inbox else {
        return false;
    };
    if inbox
        .send(Envelope {
            from: 0,
            corr,
            bytes,
            reply_to: None,
        })
        .is_ok()
    {
        return true;
    }
    // The far end is gone: mark it dead and tell the parent — the notice is
    // itself a send, so it too happens outside the lock.
    let parent = with_epoch(epoch, |ef| {
        let idx = handle_slot(id) as usize;
        if let Some(link) = ef.links.get_mut(idx - 1) {
            if link.gen == handle_generation(id) {
                link.alive = false;
            }
        }
        ef.parent_inbox.clone()
    });
    if let Some(p) = parent {
        let _ = p.send(crate::runtime::workers::died_envelope(id));
    }
    false
}

pub(crate) fn terminate(epoch: u64, id: u32) -> bool {
    let inbox = with_epoch(epoch, |ef| {
        let idx = handle_slot(id) as usize;
        if idx == 0 {
            return None;
        }
        let link = ef.links.get_mut(idx - 1)?;
        if link.gen != handle_generation(id) {
            return None;
        }
        link.alive = false;
        Some(link.inbox.clone())
    })
    .flatten();
    match inbox {
        Some(i) => {
            let _ = i.send(terminate_envelope());
            true
        }
        None => false,
    }
}

pub(crate) fn terminate_all(epoch: u64) -> usize {
    let doomed = with_epoch(epoch, |ef| {
        let mut out = Vec::new();
        for link in ef.links.iter_mut() {
            if link.alive {
                link.alive = false;
                out.push(link.inbox.clone());
            }
        }
        out
    })
    .unwrap_or_default();
    for inbox in &doomed {
        let _ = inbox.send(terminate_envelope());
    }
    doomed.len()
}

pub(crate) fn link_alive(epoch: u64, id: u32) -> bool {
    with_epoch(epoch, |ef| {
        let idx = handle_slot(id) as usize;
        idx > 0
            && ef
                .links
                .get(idx - 1)
                .map(|l| l.gen == handle_generation(id) && l.alive)
                .unwrap_or(false)
    })
    .unwrap_or(false)
}

pub(crate) fn inbox_of(epoch: u64, id: u32) -> Option<InboxSender> {
    with_epoch(epoch, |ef| {
        let idx = handle_slot(id) as usize;
        if idx == 0 {
            return None;
        }
        ef.links
            .get(idx - 1)
            .filter(|l| l.gen == handle_generation(id) && l.alive)
            .map(|l| l.inbox.clone())
    })
    .flatten()
}

pub(crate) fn set_display(epoch: u64, id: u32) -> bool {
    with_epoch(epoch, |ef| {
        let idx = handle_slot(id) as usize;
        let ok = idx > 0
            && ef
                .links
                .get(idx - 1)
                .map(|l| l.gen == handle_generation(id) && l.alive)
                .unwrap_or(false);
        if ok {
            ef.display = Some(id);
        }
        ok
    })
    .unwrap_or(false)
}

pub(crate) fn display_of(epoch: u64) -> Option<u32> {
    with_epoch(epoch, |ef| ef.display).flatten()
}
