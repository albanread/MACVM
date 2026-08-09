# The Monitor tab — every running VM, live

MACVM is a multi-VM application: a primary Smalltalk VM on its own thread,
the UI worker VM on the main thread, and up to sixteen spawned compute
workers. Until this tab, only the primary had any live surface (the toolbar
metrics), and the Cocoa side of the app — the main-thread machinery all of
those VMs hang off — had none at all. The Monitor shows both: a table with
one row per running VM, and a UI BRIDGE band for the bridge's own counters.

## The roster (`macvm::embed::monitor_*`)

`VmHandle::metrics()` is plain field reads of `&self.vm`, so only the thread
that owns a handle may sample it. The registry generalizes the supervisor's
existing owner-samples/reader-reads split to N VMs:

- `monitor_register(label, kind)` → `Arc<VmMonitorSlot>`; a dead slot with
  the same label is revived (a respawned primary keeps ONE row).
- The owner thread calls `slot.publish(handle.metrics())` at its natural
  quiescent points:
  - **primary** — every supervisor beat (`primary_generation_main`);
  - **ui** — every `refresh_metrics` pass on main, BEFORE the metrics doit
    (the Monitor's tick runs inside that doit and must see a fresh heartbeat);
  - **workers** — after boot and after every `dispatchPending`
    (`worker_main`), plus an explicit `set_busy` around the exec.
- `monitor_snapshot()` copies every row for any reader.

**Busy is two different truths.** A worker's exec IS its work, so workers
flag busy explicitly. The primary's pump blocks inside `exec` even when idle
(`pumpInbox:` waits there), so for heartbeating VMs busy is derived instead:
alive and not published for >900 ms (≈3 missed beats) means "stuck inside
guest code" — a long doit, measured, with no cooperation needed from the VM.
The numbers in a busy row are the last quiescent snapshot, which is exactly
what was true when the VM was last idle.

## The bridge band (`cocoa_gui/src/bridge_stats.rs`)

Static atomics at the seams nothing else counts: `drain_perform` passes plus
a beat-gap EWMA (the measured drain cadence — watch it stretch when a busy
primary starves the beat wake), callback dispatches (bumped in
`embed::dispatch_callback`, the one door every delegate/action/timer entry
passes through), primary→UI envelopes, control-channel requests. One line,
formatted host-side.

## The verbs (host_service)

- `monitorRows` — the whole table in one blast: one `\n`-line per VM,
  `US`-separated fields (label · kind · state · mem used/cap · heap% ·
  gc s·f · allocs-raw · nmethods · compiles · deopts · ic-misses). Only
  `allocs` is raw: the Smalltalk side diffs successive reads into ALLOC/S
  because only it knows the refresh period the user picked.
- `bridgeStatsLine` — the bridge band's line.

## The view (`world/85_cocoamonitor.mst`)

Debugger-idiom layout (header bands, zebra rows): the VM table on top, the
UI BRIDGE band and an Update popup (Paused / 1 s / 2 s / 5 s, scriptable via
`CocoaMonitor setPeriodIndex:`) below. The tick rides the ~4 Hz metrics beat
(`updateMetricsMem:` — the one periodic callback that reaches the world),
but wall-clock gates it: beats starve when the primary is busy, and counting
them would stretch "every 1 s" exactly when the user is watching. ALLOC/S
anchors its baseline at the counter's last CHANGE, so a busy burst averages
over its true window instead of being crammed into one tick.

Two reentrancy laws this view obeys (both learned the hard way here):

1. **Defer the datasource ATTACH, not just the reload.** `setDataSource:`
   makes AppKit query the source synchronously; inside a scripted
   `switchToView:` that is an exec context, and the nested dispatch re-enters
   the VM and faults (pc=0 storm). The attach is parked on the run loop via
   `macvmAction:` + `performSelector:afterDelay: 0` and lands top-level.
2. Table reloads stay deferred (the debugger's stack-pane lesson).

## Toward a profiling pane

`rowClicked` is the reserved hook. The natural extension: selecting a VM row
opens a lower split of its hottest compiled methods — the `nmethods` surface
(id, state, version, Klass>>selector, trap counts) already exists as a
rusttcl verb over `code_table.iter_all()`, and `VmLiveStats.compiled_depth`
is the lock-free interpreter/compiled ratio sampler. For the primary that is
one new host verb away; workers would need a stats-request envelope. Not
built; the layout and selection hook anticipate it.

## Screenshot sessions (operational note)

Scripted `snapshotWindowTo:` captures are REAL screen captures — if the
display sleeps mid-session (this fanless Air sleeps it fast), every capture
comes back blank white while the app runs on happily. `caffeinate -u -t 2`
immediately before each snap (and a long-lived `caffeinate -di` for the
session) is part of the drill.
