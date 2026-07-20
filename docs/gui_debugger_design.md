# GUI Debugger (DBG4) — the HALT loop with a Cocoa face

*The last rung of the `docs/DEBUGGER.md` ladder: DBG1/DBG2 built the whole
engine (halt loop, breakpoints, stepping, frame inspection, print-eval);
DBG4 gives it a native UI in `macvm-cocoa`. Design updated for the current
two-VM architecture, which the original §3.2 sketch predates.*

## 1. The architectural gift

`debug::halt` is a **nested command loop at a bytecode boundary** — the
primary VM parks mid-doit, exactly as paused as it is during GC, and services
commands until resumed. In the Cocoa GUI the UI is a *separate VM on its own
thread*, so while the primary sits in the halt loop **the entire GUI stays
alive** — stack panes, buttons, print-it all responsive, no reentrancy work
needed. (The toolbar metrics freezing during a halt is honest: the primary
really is stopped.)

## 2. The seam: `DebugFrontend`

One trait in `src/runtime/debug.rs`, installed per-VM (the Cocoa supervisor
installs it on the **primary only** — a UI-worker halt would park the main
thread, so the UI worker keeps today's fatal behavior):

```rust
pub trait DebugFrontend: Send + Sync {
    fn publish(&self, report: &str);   // full state, every time
    fn next_command(&self) -> String;  // blocks until the UI sends one
}
```

With a frontend installed, `halt()` runs the same command engine as the CLI
`(halt)` REPL but swaps stdin/stderr for publish/next_command. The report is
the **whole debugger state on every change** (stack + selected frame +
last-command output) — the blast-don't-patch law; the UI renders, never
patches.

Report format (`␟` = 0x1f field separator, sections by marker lines):

```
HALTED <reason>            |  RUNNING
==STACK==
<n> ␟ <Class> ␟ <selector> ␟ <bci>        (one per frame, innermost first)
==FRAME==
<show_frame text for the selected frame — receiver + temps printStrings>
==OUT==
<last command output — print results, errors>
```

Commands (the CLI's own vocabulary): `bt`-implicit, `frame N`, `print <expr>`
(evaluated against the selected frame's receiver — DBG1's own doit path,
reentrancy-guarded by `session_depth`), `step`, `next`, `finish`, `continue`,
and GUI-only `abort` (below). On resume the frontend gets `RUNNING`.

## 3. Transport (flag-and-drain, both directions)

- **Primary → UI**: `publish` stores the report in a host cell and fires the
  UI-worker wake (the same `InboxWakeFn` the pump beats with — thread-safe,
  and the pump itself is parked during a halt, so the frontend must carry its
  own wake). `drain_perform` sees the halt flag and execs
  `CocoaDebugger haltArrived.` top-level.
- **UI → primary**: host verbs `dbgReport` (read the cell) and `dbgCommand:`
  (send one line into the mpsc channel the halt loop blocks on). A workspace
  `uiDoit` arriving mid-halt is *not* serviced (the pump is parked); its
  envelope waits in the inbox and runs on resume — no queue-jumping, no
  nested entry.

## 4. Entry points (v1)

- **Halt on error** (Debug ▸ Halt on Error, default ON): `error:`/DNU on the
  primary route to `halt(GuestError)` *before* the fatal path when armed —
  every red doit becomes an inspectable stop at the fault, the classic
  Smalltalk experience. Toggle off restores today's trace-and-recover.
- **`halt`** — the existing Object primitive (`vm.debug.active` is set when
  the frontend installs, so `... halt` in any doit stops there).
- **Breakpoints** — the engine (`set_breakpoint_by_name`, tier-0 pinning) is
  live; a browser "Break on entry" affordance is a follow-on (a pending-slot
  serviced by the pump, the filein pattern).
- **Step completion** — re-halts via the armed `StepPlan`, unchanged.

## 5. Resumption semantics

- `continue` / `step` / `next` / `finish` — the CLI's own, unchanged.
- **`abort`** — the guest-fatal path (`fatal_exit`): on the primary that is
  `ExitThread` → the supervisor respawns a fresh world. "Recover clean or
  die" — there is no catch-and-continue, consistent with the exceptions
  stance. The halted doit is abandoned with the whole generation.

## 6. The UI (world/73_cocoadebugger.mst)

A `#debugger` tab, auto-fronted when a halt arrives:

```
┌───────────────────────────────────────────────┐
│ [grain] Step Into | Step Over | Finish |      │
│         Continue | Abort   [print field ⏎]    │
├───────────────┬───────────────────────────────┤
│ stack (table) │ source (DB text, selected     │
│ Class>>sel@bci│ frame's method, colorized)    │
├───────────────┴───────────────────────────────┤
│ frame: receiver + temps (text)                │
└───────────────────────────────────────────────┘
```

- Stack rows from `==STACK==`; clicking a row sends `frame N`.
- Source fetched from the image DB by the frame's `Class>>selector` (the
  browser's own host read — the running VM keeps no source).
- The frame pane shows `==FRAME==` verbatim; `==OUT==` appends to it.
- All list/table/click discipline identical to the browsers (deferReload,
  non-editable cells, click actions).

## 7. Honest v1 limits

- **No statement-level highlight**: the §2.2 bci↔source map was designed but
  never built — the stack shows `@bci` and the source pane shows the whole
  method. A compiler-retained map is the natural next rung.
- **Read-only**: no temp editing, no resume-with-value, no restart-frame.
- **One debuggee**: the primary. Worker VMs keep `ErrorPolicy` semantics.
- **Interpreter-grain stepping**: stepping works on interpreter frames
  (breakpointed methods are tier-0-pinned already); compiled frames show in
  the backtrace via DBG2's virtual frames but are not step targets.

## 8. Build ladder

- **D1** — `DebugFrontend` seam + string-core refactor of bt/frame/print +
  GUI halt loop + halt-on-error hook + `VmHandle` setters.
- **D2** — cocoa_gui bridge: report cell, command channel, per-generation
  install in the supervisor, drain servicing, host verbs.
- **D3** — the tab + Debug-menu toggle.
- **D4** — end-to-end gates over the control channel (DNU → halt → report →
  step → continue; `halt` primitive; abort → respawn).
