# SUnit for MACVM — a test suite Smalltalkers can live in

*Design, 2026-08-10. Status: S1–S4 planned, nothing built.*

## 0. Why, and for whom

The VM is guarded by 845 Rust tests. The world — ~3,400 Smalltalk methods, the
part users actually touch — is guarded by nothing they can run. Worse, the
existing guards for world code are written backwards: the Minesweeper and Life
regression tests live in `src/embed.rs`, asserting Smalltalk facts ("a flagged
square must not reveal") in Rust, through `vm.eval` strings, runnable only by
someone with a toolchain who rebuilds the compiler. The app *invites* editing —
Accept live-compiles into a running image; `ic_epoch` exists precisely so
redefinition takes effect immediately — and offers no way to check an edit.

Three audiences, in priority order:

1. **Users who live in the GUI.** They edit classes in the Browser/Editor and
   never touch Rust. Add a test, change a test, run a suite, see red/green,
   click a failure, end in the debugger — all inside the app.
2. **Smalltalkers.** SUnit was invented in Smalltalk; anyone arriving from
   Pharo/Squeak looks for `TestCase` in the first ten minutes. The protocol
   should be the one their fingers know.
3. **The project.** The world's own suite should gate `cargo test` and be
   runnable headless from the shipped binary, so world regressions fail CI the
   same way VM regressions do.

## 1. Grounding — what exists (verified, not assumed)

Every design decision below leans on machinery that is already built and
tested. File:line citations are to the state at commit `11aaa84`.

- **Exceptions are complete enough.** `Exception` → `Error` → {`ZeroDivide`,
  `MessageNotUnderstood`, `KeyNotFound`, `IndexOutOfBounds`,
  `InvalidArgument`}, plus `Warning` (world/78_exceptions.mst:41,226–264).
  `on:do:`, `signal`, `retry`, and — the ones a test runner cannot live
  without — `ensure:` / `ifCurtailed:` (world/04a_blockclosure.mst:33,36).
- **VM failures are catchable.** E4 (world/82_vm_errors.mst) turned `1/0`,
  DNU and out-of-range into ordinary signals. A runner can catch a test's
  errors and keep going — impossible before E4, trivial now.
- **Halt-on-error fires on UNHANDLED signals only** (82's header: primitive 95
  runs only when nothing handles). This is the hinge of the whole debugger
  story: a runner whose handlers catch everything never trips the debugger;
  re-running one test *bare* parks the primary in the DBG4 debugger at the
  raise point with zero new debugger work.
- **The authoring verbs already exist.** The host service exposes
  `newClassFrom:` (cocoa_gui/src/host_service.rs:266), `saveMethod` (:279,
  the outliner's parse-gated single-method accept), `removeMethod` (:297),
  `acceptEditorClass:` (:636), all writing versioned rows to the image DB and
  splicing the world `.mst` tree.
- **Discovery belongs to the DB.** `classNames` (:139), `classShellFor:`
  (:243, includes superclass), `packageTree` (:813). The Browser's hierarchy
  is already answered by Rust+SQLite, not VM reflection — the Tests tab
  follows the same precedent. No `allSubclasses` primitive is needed anywhere.
- **UI ↔ primary RPC exists in Smalltalk.** The UI worker ships a doit to the
  supervised primary and receives the reply in a callback block:
  `Worker uiDoit: 'src' onReply: [:r | …]` (cocoa_gui/src/supervisor.rs:549
  uses exactly this idiom). Async — the GUI stays live during a run; the reply
  is a string.
- **A hung or fatal doit is already survivable.** The S21 supervisor respawns
  a dead primary; restart-in-place is a menu item. A test that loops forever
  stalls the primary, not the GUI, and the existing recovery applies.
- **Packages are real.** `world.list` + `cocoaui.list` are separate package
  lists in one directory; the seed imports every `*.list`; `load_list`
  (src/embed.rs:1449) stacks an extra list onto a booted world; selective
  DB-boot (package-aware M1–M7) chooses what a VM loads.
- **Views self-register.** `registerViewNamed:title:icon:container:onShow:`
  (world/64_cocoaui.mst:684); the Monitor tab (world/85_cocoamonitor.mst) is
  the worked example of a late-added, self-registering view.

## 2. The framework — `world/86_sunit.mst` (package: world)

Four classes, in `world.list` proper — the framework must exist even when no
test package is loaded, because user tests subclass it.

### TestCase

The class Smalltalkers expect, scoped to what this world can honour:

- **Fixtures:** `setUp`, `tearDown`. `tearDown` runs through `ensure:`, so it
  runs whether the test passes, fails, or errors.
- **Assertions:** `assert:`, `assert:description:`, `deny:`,
  `assert:equals:` (failure message auto-built from both `printString`s —
  "expected 7 but was 8" is the difference between a suite new users read and
  one they ignore), `assert:closeTo:` (Doubles; the game world is full of
  them), `fail`, `fail:`.
- **Exception assertions:** `should:raise:`, `shouldnt:raise:`.
- **Discovery:** `testSelectors` — unary selectors beginning `test`, sorted.
- **Running:** `runCase` (one selector through setUp/test/tearDown),
  `run` (class-side: whole class → prints the summary on the Transcript, so
  the zero-GUI Workspace path is one message: `LifeTest run`).

### TestFailure — an `Error` subclass, and the discrimination rule

A failed assertion signals `TestFailure`; anything else that unwinds is an
*error*. The runner discriminates with nested handlers, **inner handler
first**:

```smalltalk
[[ aCase runCase. result pass: aCase ]
    on: TestFailure do: [:e | result fail: aCase message: e messageText ]]
    on: Error do: [:e | result error: aCase message: e messageText ]
```

`TestFailure` sits under `Error` (not beside it) so that code which blindly
catches `Error` — including the runner's own outer handler — never lets a
failure escape as an unhandled signal. The inner handler claims failures
before the outer sees them; the outer catches genuine errors, including E4's
VM-raised ones. Failure vs error is kept distinct from day one; retrofitting
the distinction is the classic SUnit-port mistake.

### TestResult

`runCount`, and per-case records for passed / failures / errors, each with a
duration in ms (`Time millisecondClockValue` deltas). Its `printString` is
**the wire format** — one machine-parseable line per case plus a summary:

```
PASS LifeTest testGliderWalks 3
FAIL MinesweeperTest testFirstClickSafe 1 expected 0 but was 1
ERROR FooTest testBroken 0 doesNotUnderstand: #frobnicate
5 run, 1 failure, 1 error, 41 ms
```

One format, three consumers: the Transcript, the Tests tab (parses it out of
the `uiDoit:onReply:` reply string), and the CLI (prints it verbatim, exit
code from the summary). No second serialization is ever invented.

### TestSuite / TestRunner

`TestSuite forClasses:` composes; `TestRunner runClasses: #(LifeTest …)`
takes an **explicit class-name list** — in-VM code never discovers, it is
told. Discovery is the caller's job (the DB, §4/§5). This is a deliberate
divergence from classic SUnit and the reason no reflection primitive is
needed.

### Deliberate omissions (recorded so they are not re-proposed piecemeal)

No `TestResource`, no `expectedFailures`, no resumable failures, no async
tests in v1. Each is real scope with a real design cost; none is needed to
test this world's code. Revisit only on demand (§7).

## 3. Where tests live — the `tests` package

- **`world/tests.list`**, third package beside `world` and `cocoaui`; member
  files carry a `t` prefix (`t10_library_tests.mst`, `t20_life_tests.mst`,
  `t30_minesweeper_tests.mst`) so the shared directory reads at a glance.
- Seeded into the image like every package; the **primary boots it** via the
  selective DB-boot set (dev default ON). `cocoaui` stays UI-worker-only.
- **Ships in the .app payload.** A fresh install can open the Tests tab, press
  Run All, and watch the library's own suite go green on their Mac — a trust
  feature no About box matches, and the reason the first suites are ports of
  the nine Minesweeper + four Life embed tests (the Smalltalk-side assertions;
  the game primitives silently no-op headless, and `stepWithKeys:…` is an
  ordinary message) plus exception/collection smoke tests.

## 4. The headless runner — `macvm-gui test`

For CI and for users who live in a terminal:

```
macvm-gui test [--world <dir>] [--filter <substring>]
```

Opens (seeding if absent) the image, queries the DB for the `TestCase`
subtree and its `test*` selectors, boots the VM from the image, execs
`TestRunner runClasses:` with the discovered list, prints the result block,
exits 0/1. Same discovery, same runner, same wire format as the GUI.

**The cargo bridge** — `gui/tests/sunit_bridge.rs` — drives that same path in
a `#[test]` and asserts `0 failures, 0 errors`. From S1 on, the world's
Smalltalk suite gates `cargo test` exactly as the VM's Rust suite does. This
is the highest-value single deliverable in the plan.

## 5. The Tests tab — `world/86_coctests.mst` (package: cocoaui)

Registers as `#tests` through `registerViewNamed:…`; Monitor is the
template. All UI code is Smalltalk on the UI worker, per the house rule.

```
┌ toolbar ─────────────────────────────────────────────────────────────┐
│ [Run All] [Run Selected] [New Test Class] [New Method]  ☑ Run after  │
│                                    Accept      ● 41 run 2 red 320ms  │
├────────────────┬─────────────────────────────────────────────────────┤
│ ▾ LifeTest   ● │  status │ test                        │  ms │ note  │
│    testGlider ●│  PASS   │ testGliderWalks             │   3 │       │
│    testPause ○ │  FAIL   │ testFirstClickSafe          │   1 │ expec…│
│ ▾ Minesweep… ●│  …                                                   │
├────────────────┴─────────────────────────────────────────────────────┤
│ detail: full failure text of the selected row            [Debug]     │
│         — or the selected method's source, with [Accept]             │
└──────────────────────────────────────────────────────────────────────┘
```

- **Tree** (left): `TestCase` subclasses → their `test*` selectors, from the
  DB (one new host verb, `testClassRecords`, a small SQL closure query in the
  Rust+SQLite tradition — or composed from `classNames`+`classShellFor:` if
  the query stays trivial). Status dots carry the last run's per-node state,
  held UI-side.
- **Run** (all / selected class / one method): the tab sends
  `Worker uiDoit: 'TestRunner runClasses: …' onReply: [:r | self showResults: r]`.
  The GUI never blocks; per-test progress appears live on the Transcript
  (the runner prints each line as it goes — TranscriptSink already streams);
  the table fills when the reply lands. Suites here are sub-second on the
  JIT; streaming table updates are deliberately NOT built (§7).
- **Authoring, the critical loop:**
  - *New Test Class* → name prompt → template through `newClassFrom:`:
    ```smalltalk
    TestCase subclass: FooTest [
        testExample [ self assert: 3 + 4 equals: 7 ]
    ]
    ```
  - *New Method / edit* → the detail pane holds the selected method's source;
    Accept goes through `saveMethod` (parse-gated; a red is the same compile
    error the next boot would hit).
  - *Run after Accept* (default ON): a green Accept immediately re-runs that
    class — the tight loop GUI dwellers asked for.
  - **Open question, resolved in S3:** whether `newClassFrom:` classes are
    live on the running primary or DB-only. If DB-only, the tab nudges
    restart-in-place (`requestPrimaryRestart` exists, :539; restart is
    seconds). Either way the flow works on day one.
- **Debug** (S4): a red row's [Debug] re-runs that ONE case **bare** — no
  handlers — via the same `uiDoit:`. The signal goes unhandled, primitive 95
  fires, and the existing DBG4 debugger parks the primary at the raise with
  the test's frames on the stack. Zero new debugger machinery; this falls out
  of E4's unhandled-only rule.
- **Empty state** (new users): one short paragraph on what a test is, and
  [Create my first test] → the `ExampleTest` template above, pre-selected,
  ready to Run.

## 6. Sprints

Each sprint lands committed, verified per house rules (release-profile tests;
GUI sprints driven end-to-end over `MACVM_COCOA_CTL` with screenshots).

- **S1 — the framework, the first suites, the headless runner.**
  `86_sunit.mst`; `tests.list` + t-files porting the Life/Minesweeper embed
  assertions and library smoke; `macvm-gui test`; `gui/tests/sunit_bridge.rs`
  gating cargo. *Verify:* bridge red-then-green against a deliberately broken
  assertion; CLI transcript in the commit message. **This sprint alone pays
  for the feature.**
- **S2 — the Tests tab, read-and-run.** View registration, DB-backed tree,
  Run All/Selected over `uiDoit:onReply:`, results table + detail, summary
  chip, Transcript progress. *Verify:* ctl-driven run of the shipped suite,
  screenshot of green; force a red, screenshot the failure detail.
- **S3 — the authoring loop.** New Test Class template, in-tab method Accept,
  run-after-accept, jump-to-Browser; resolve the new-class liveness question
  (restart nudge if needed). *Verify:* create a failing test entirely through
  the tab, watch it red, fix it in-tab, watch it green — scripted.
- **S4 — debug the red.** [Debug] bare re-run → DBG4 at the raise;
  `assert:equals:` rich diff in the detail pane. *Verify:* scripted Debug of
  a known-red test lands in the debugger at the failing assertion's frame.
- **S5 — deferred, on demand only:** worker-VM isolation + timeouts for
  hostile tests; `expectedFailures`; per-run history; `should:raise:`
  message-text matching.

## 7. Rejected alternatives (do not re-propose without new facts)

- **In-VM reflection discovery** (`allSubclasses`): the DB already owns class
  facts and answers them in SQL; the browser tools moved there deliberately.
- **Running suites on the UI worker:** blocks the GUI's own event servicing;
  the primary is where user code lives and where the debugger parks.
- **Worker-VM isolation in v1:** workers boot from the image, so they cannot
  see un-Accepted state, and results cross as pickles; isolation buys hang
  immunity the supervisor already approximates. S5, on demand.
- **Streaming per-test table updates:** machinery for suites that finish in
  under a second; the Transcript already streams progress.
- **A synchronous run verb on the host service:** beachballs the main thread;
  `uiDoit:onReply:` is async and already exists.
- **A second results format for the GUI:** one wire format (§2 TestResult),
  parsed everywhere.
