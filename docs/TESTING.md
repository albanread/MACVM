# Testing — SUnit in MACVM

*How to write a test, how to run it, and where tests live. For the design and
the reasoning behind it, see [`sunit_design.md`](sunit_design.md); for the
framework source, `world/86_sunit.mst`.*

## 1. What this is

**SUnit** — the original. Kent Beck wrote it for Smalltalk-80, and when he and
Erich Gamma ported the idea to Java in 1997 it became JUnit; every *Unit since
descends from that Smalltalk ancestor. Pharo, Squeak, VisualWorks, GNU
Smalltalk and Dolphin all ship it, so if you have written a Smalltalk test
before, you already know this framework.

MACVM's is a faithful implementation of the core protocol, not a look-alike.
The selectors below are SUnit's own names, and a test written for Pharo will
usually run here unchanged.

**As shipped: 572 tests across 65 suites, green.**

## 2. Writing a test

A test class is a subclass of `TestCase`. Every method whose name starts with
`test` is a test, found by reflection — there is no list to maintain.

```smalltalk
TestCase subclass: PointTest [
    | origin |

    setUp    [ origin := 0 @ 0 ]
    tearDown [ origin := nil ]

    testAddingMovesBothAxes [
        self assert: (origin + (3 @ 4)) equals: 3 @ 4
    ]

    testDistanceIsPythagorean [
        self assert: (origin dist: 3 @ 4) closeTo: 5.0
    ]

    testOutOfRangeRaises [
        self should: [ (Array new: 3) at: 99 ] raise: Error
    ]
]
```

**Each test gets its own fresh instance.** `setUp` runs before every test and
`tearDown` after every one, including after a test that blows up. Nothing
leaks from one test to the next — that is core SUnit semantics, and it is what
makes a suite trustworthy. Do not stash state in class variables expecting the
next test to see it; if two tests need to share expensive setup, they are
probably one test.

### The assertion protocol

| | |
|---|---|
| `assert: aBoolean` | fails unless true |
| `assert: aBoolean description: aString` | …and says why |
| `deny: aBoolean` / `deny:description:` | the negations |
| `assert: actual equals: expected` | **prefer this** — reports `expected 7 but was 8` |
| `assert:equals:description:` | …with your own note as well |
| `assert: actual closeTo: expected` | floats, default precision |
| `assert: actual closeTo: expected precision: eps` | floats, your epsilon |
| `should: aBlock raise: anExceptionClass` | the block must raise |
| `shouldnt: aBlock raise: anExceptionClass` | the block must not |

Reach for `assert:equals:` over `assert: a = b` every time. The first tells you
what it expected and what it got; the second only tells you that a test broke,
and you then go hunting.

### Failure is not error

This distinction is the one the whole framework turns on, and it is worth
internalising:

- A **FAILURE** is an assertion that did not hold — the code ran fine and
  produced the wrong answer.
- An **ERROR** is anything else: a `doesNotUnderstand`, a divide by zero, an
  index out of range. The test never got as far as judging anything.

They are counted apart and reported apart, because they mean different things.
A failure says "your logic is wrong"; an error usually says "your test is
wrong, or something upstream of it broke."

### Helper classes

`allTestClasses` sweeps the image by reflection, so a `TestCase` subclass that
exists only to be run *by another test* — a deliberately-broken fixture, say —
must opt out, or it will be counted as a real suite full of real failures:

```smalltalk
TestCase subclass: BrokenOnPurpose [
    BrokenOnPurpose class >> isHelper [ ^true ]
    testThatFails [ self assert: 1 equals: 2 ]
]
```

`isHelper` is MACVM's own addition (standard SUnit uses abstract classes for
this). SUnit's own suite, `world/t10_sunit_tests.mst`, is the main user: it
runs deliberately-broken cases through a nested runner and asserts on the
result, so that a framework which mis-reports a failure cannot pass its own
tests.

## 3. Running them

### From the command line

```bash
./target/release/macvm-gui test
```

Prints one line per test and a summary, and exits **0 if green, 1 if not** —
so it drops straight into a build script or a git hook.

```
PASS PointTest testAddingMovesBothAxes 0
FAIL PointTest testDistanceIsPythagorean 1 expected 5.0 but was 4.9
572 run, 0 failures, 0 errors, 258 ms
```

That last line is the wire format. The command line, the Tests tab and
`tests/it_world.rs` all parse it, so it is pinned by a test of its own.

| flag | |
|---|---|
| `--repeat N` | run the whole suite N times in one image |
| `--list PATH` | a different package list (absolute paths work) |
| `--world DIR` | a different world directory |

**`--repeat 2` is worth the two seconds.** The second run is the first whose
methods are JIT-compiled, so it is the cheapest possible check that your code
survives its own warm-up. This is not hypothetical: it is how a silent
compiler bug was caught in August 2026, where run 1 was green and run 2
reported `nil does not understand add:` because a constructor had started
returning uninitialised objects once it crossed the compile threshold.

### From a Workspace

```smalltalk
PointTest run.              "one suite, printed on the Transcript"
TestRunner runAllAndShow.   "everything in the image"
```

### From the GUI — the Tests tab

Pick **Tests** in the toolbar (the ✓ icon). The tab is three panes:

- **SUITES** (left) — every runnable suite, expandable to its tests. The dot
  carries the last run's verdict: `●` passed, `✕` did not, `·` not yet run.
- **RESULTS** (right) — one row per test: status, name, milliseconds, and the
  failure message.
- **DETAIL** (below) — the full text of whatever row or test you selected.

**Run All** runs everything. **Run Selected** runs the selected suite, or the
single selected test. Tests run on the *primary* VM, not the UI one, so the
interface stays responsive while they go.

## 4. Where tests live

One package, `world/tests.list`, with three consumers: the command line, the
Tests tab, and cargo.

```
world/tests.list          the package — this is the list to add to
world/t10_sunit_tests.mst SUnit testing itself
world/t11_*.mst           VM semantics a Smalltalker's code rests on
world/tests/NN_*.mst      the library corpus (60 suites, 549 tests)
```

To add a suite: write the `.mst` file, add one line to `world/tests.list`.
Nothing else — suites are found by reflection, so there is no driver to update.

> **After changing anything under `world/`, reseed before looking at the GUI:**
>
> ```bash
> ./target/release/macvm-gui seed --world world
> ```
>
> The GUI boots from the SQLite image, not from the `.mst` files. The command
> line reads the `.mst` files directly, so the two can disagree — and the
> command line is the one that will look right. If the Tests tab is missing a
> suite you just added, this is why.

## 5. How it is gated

Three layers, so a regression cannot slip through whichever way you run:

| gate | what it covers |
|---|---|
| `cargo test` → `tests/it_world.rs` | loads the package and runs all 572 in-process |
| `just test-gui` → `gui/tests/sunit_bridge.rs` | drives the real `macvm-gui test` binary: green exits 0, red exits 1 and names the test, an error is counted as an error, and `--repeat 2` stays green |
| `just ci` | both of the above, plus lint |

`gui` is a non-default workspace member, so a bare `cargo test` does **not**
reach it — `just test-gui` is what does.

There is also a differential check worth running after touching the compiler,
since a JIT that miscompiles will usually still pass a single run:

```bash
MACVM_JIT=off      ./target/release/macvm run /tmp/all_tests.mst --world world
MACVM_JIT=threshold=20 ./target/release/macvm run /tmp/all_tests.mst --world world
```

Identical output either way is the property you want. (`just gate-s10` does
this for you.)

## 6. What MACVM's SUnit does not have

Recorded so they are not re-proposed piecemeal, and so you know what to reach
for instead:

- **`TestResource`** — SUnit's shared, expensive, set-up-once-per-suite
  fixture. Use `setUp` per test; if that is genuinely too slow, say so.
- **`expectedFailures`** — marking a test known-red so it does not break the
  build. Deliberately absent: a red test should be fixed or deleted.
- **`TestSuite` as a composable object.** Standard SUnit builds a `TestSuite`
  and hands *that* to a runner; here `TestRunner run:` takes the classes
  directly. This is the most visible structural divergence, and the first
  thing to revisit if suite composition is ever wanted.
- **Resumable failures and async tests.**

## 7. Where the code is

| | |
|---|---|
| `world/86_sunit.mst` | the framework: `TestCase`, `TestFailure`, `TestResult`, `TestRunner` |
| `world/t10_sunit_tests.mst` | SUnit's own suite |
| `world/86_coctests.mst` | the Tests tab (`CocoaTests`) |
| `gui/src/main.rs` | `macvm-gui test` |
| `gui/tests/sunit_bridge.rs` | the cargo gate on the shipped package |
| `tests/it_world.rs` | the cargo gate on the corpus |
| `docs/sunit_design.md` | the design, the sprints, and the rejected alternatives |
