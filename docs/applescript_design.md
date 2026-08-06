# Scripting macVM — an Apple Event vocabulary

**Status: design (not implemented).** Designs the `.sdef` that makes
`macVM.app` scriptable from AppleScript, JavaScript for Automation,
`osascript`, Automator, and Shortcuts (through its *Run AppleScript*
action). It replaces `MACVM_COCOA_CTL` — a localhost TCP socket that
evaluates arbitrary Smalltalk with no authentication — as the *supported*
automation surface, and leaves that socket where it belongs: a development
affordance, off unless you set the variable.

```applescript
tell application "macVM"
    evaluate "(1 to: 20) inject: 0 into: [:a :b | a + (b * b)]"  --> "2870"
    set current view to browser
    browse "Integer"
    get transcript
end tell
```

The verb set *is* the API. It is awkward to change once scripts depend on
it, which is why this document exists before any code.

## 1. What the vocabulary has to be

Four constraints shape every decision below.

**A closed vocabulary, not a shell.** The point of moving off the socket is
that `set current view to browser` is automation and `gui eval {...}` is
arbitrary code execution. `evaluate` survives — this is a Smalltalk IDE,
that verb is the product — but as one declared capability among many,
behind macOS's per-client automation consent, rather than an ambient open
port that any local process can drive.

**Evaluation is asynchronous.** `CocoaUI>>doIt` posts to the primary VM and
the reply lands later on the beat loop; the UI worker does not block. An
Apple Event that answers a value therefore cannot compute it inline. Cocoa
Scripting has the exact primitive for this — `suspendExecution` /
`resumeExecutionWithResult:` — and §5 is built on it.

**Errors are ordinary, not exceptional.** A script that sends `evaluate
"3 + "` must get a script error, not a dead app. This is the sharpest
requirement in the document: today a guest error inside a callback is
recovered but its *message is discarded* (§6.3), which is fine for a
delegate answering a row count and useless for a scripting interface whose
whole job is to report what went wrong.

**One window, one document-less app.** macVM has no documents, so the
Standard Suite's `open`/`close`/`save`/`print` mostly do not apply. The
object model hangs off `application` (§4), and the interesting nouns are
Smalltalk classes and methods (§7), not files.

## 2. Suite and codes

One suite, `macVM Suite`. Codes are four characters, mixed-case (Apple
reserves the all-lowercase space); every macVM code starts `Mv`. One sdef
mechanic the table elides: a *command's* `code` attribute is eight
characters — event class, then event ID — and the suite code doubles as
the event class, so `evaluate` appears in the file as `MvSuMvEv`. The
table lists the ID half.

| Kind | Name | Code |
|---|---|---|
| suite | macVM Suite | `MvSu` |
| command | evaluate | `MvEv` |
| command | browse | `MvBr` |
| command | snapshot | `MvSn` |
| command | clear transcript | `MvCT` |
| property | current view | `MvCv` |
| property | appearance | `MvAp` |
| property | transcript | `MvTr` |
| property | workspace text | `MvWt` |
| property | transcript collapsed | `MvTc` |
| property | busy | `MvBs` |
| enumeration | view | `MvVw` |
| enumeration | appearance | `MvAe` |
| class | Smalltalk class | `MvCl` |
| class | Smalltalk method | `MvMe` |

Command *parameters* carry codes too, assigned when the sdef was written:

| Command | Parameter | Code |
|---|---|---|
| evaluate | `time limit` | `MvTl` |
| browse | `selector` | `MvSl` |
| snapshot | `in` | `MvIn` |

Enumerators carry codes of their own, and those codes are what a compiled
script *stores* — Script Editor decompiles `MvV1` back to `browser`
through the dictionary. That makes them append-only: renumbering breaks
every saved script, silently. A view added later appends the next code;
one removed retires its code forever.

| Enumeration | Enumerators |
|---|---|
| view `MvVw` | `MvV0` workspace · `MvV1` browser · `MvV2` browser2 · `MvV3` find · `MvV4` editor · `MvV5` outliner · `MvV6` canvas · `MvV7` help · `MvV8` debugger |
| appearance `MvAe` | `MvA0` system · `MvA1` light · `MvA2` dark |

The Standard Suite comes in by reference — the `<dictionary>` element
needs `xmlns:xi="http://www.w3.org/2003/XInclude"` — which is the
ordinary idiom and gives `quit`, `get`, `set`, `count`, `exists`, and the
`window` class, so `bounds of window 1` works against AppKit's own
scripting support with zero macVM code:

```xml
<xi:include href="file:///System/Library/ScriptingDefinitions/CocoaStandard.sdef"
            xpointer="xpointer(/dictionary/suite)"/>
```

**Correction from implementation (2026-08-06):** an earlier draft of this
section gave the namespace as `.../2003/XInclude`. It is **2001** —
`/System/Library/DTDs/sdef.dtd` declares `xmlns:xi CDATA #FIXED
'http://www.w3.org/2001/XInclude'`, so 2003 is not DTD-valid. The 2003
spelling does appear in shipping Apple sdefs (Mail's, for one) and works,
because the runtime processor does not validate — but it forfeits
`xmllint --valid` as a gate for no benefit.

The cost is dictionary noise: Script Editor will also show document verbs
(`open`, `save`, `print`) a document-less app cannot honour. Nearly every
scriptable app accepts that trade, and macVM does too.

## 3. Commands

### `evaluate`

The core verb. Takes Smalltalk source, answers its `printString`.

```applescript
evaluate "3 + 4"                              --> "7"
evaluate "SuiteAll run" time limit 300        -- a long doit: raise the guest-side limit
evaluate "1/0" --> error "ZeroDivide" number -10000
```

| Parameter | Type | Optional | Meaning |
|---|---|---|---|
| *direct* | text | no | Smalltalk source |
| `time limit` | integer | yes | seconds before the command fails with a script error (default 60) |

An earlier draft had a third parameter, `as target`, choosing "which
surface's context". Cut on review: the implementation has exactly one
evaluation context — `Worker uiDoit:` posts to the primary VM — and a
parameter that changes nothing is worse than no parameter.

Answers `text`. It answers the *printString*, not a typed value: the guest
is a Smalltalk image whose objects have no Apple Event representation, and
inventing one for arbitrary results is exactly the kind of leaky mapping
that makes scriptable apps unpleasant. Scripts that want structure ask for
it — `evaluate "someCollection asArray printString"` — or use §7.

`time limit` deserves two notes. The name: not `timeout`, because
`timeout` is an AppleScript *reserved word* (`with timeout of N seconds`),
and a parameter so named does not compile in Script Editor. The semantics:
AppleScript's own reply timeout — two minutes by default, adjustable with
`with timeout` — is the *client* giving up, and it reports a useless
error. `time limit` is macVM giving up: the command resumes with a script
error when the primary VM has not answered (§5), and it should always be
the one that fires. A runaway doit must produce a script error, never a
hung Script Editor.

### `browse`

```applescript
browse "Integer"
browse "Integer" selector "printOn:"
```

Switches to the Browser and selects the class (and optionally the method).
Answers nothing. This is deliberately a *command*, not `set current view to
browser` plus property pokes: "show me this" is one user intention and
should be one script line.

### `snapshot`

```applescript
snapshot in POSIX file "/tmp/macvm.png"
```

Writes a PNG of the window — `in`, not `to`, because `save in file` is
the Standard Suite's own spelling of a write target and matching it is
free. Wraps `CocoaUI>>snapshotTo:`, and inherits its
documented limitation — under a forced Light or Dark appearance the
titlebar chrome does not re-render faithfully offscreen. The `.sdef`
comment must say so, because a script author cannot be expected to read
[64_cocoaui.mst](world/64_cocoaui.mst).

### `clear transcript`

```applescript
clear transcript
```

Exists because the alternative is `set transcript to ""`, and a read-only
transcript with a separate clearing verb models what actually happens
better than a settable text property would.

## 4. The application object

Properties, all on `application`:

| Property | Type | Access | Notes |
|---|---|---|---|
| `current view` | view enum | r/w | routes through `switchToView:`, so the toolbar segment tracks |
| `appearance` | appearance enum | r/w | `system`/`light`/`dark`; persists, same as the menu |
| `transcript` | text | r/o | the whole log, newest first |
| `workspace text` | text | r/w | the Workspace buffer; setting it recolourises |
| `transcript collapsed` | boolean | r/w | the dock |
| `busy` | boolean | r/o | true while the primary VM is evaluating |

`busy` earns its place: with `evaluate` suspending, a script that wants to
poll rather than block needs to see the state. It is also the honest answer
to "why did nothing happen" when a previous long doit is still running.

Enumerations:

```
view:        workspace | browser | browser2 | find | editor
             | outliner | canvas | help | debugger
appearance:  system | light | dark
```

The `view` enumerators are the registry's own symbols, in registration
order — find before editor. (This document's first draft had them
swapped; the live registry disagreed — `switchToView: #canvas` lights
segment 6, which only the find-first order predicts.) That is exactly the
drift a static resource invites, so the names are *parity-tested* against
`Views`: a cocoa_gui gate boots the world headless, reads the registry,
parses the sdef out of Resources, and fails when names or codes drift.
One transport fact shapes the accessors: over KVC an enumeration-typed
property travels as its four-char code in an `NSNumber`, so
`current view`'s getter answers `MvV6`, not `"canvas"` — the world-side
code table is the single mapping in both directions.

## 5. Asynchrony: suspend and resume

`evaluate` is the only command that needs this, and it is the reason Apple
Events fit macVM better than a hand-rolled protocol would.

```
osascript ──AE──▶ MacvmEvaluateCommand — performDefaultImplementation IMP
                      dispatch → CocoaScript evaluate: src command: cmd
                        ├─ cmd suspendExecution        (bridge send, inside the dispatch)
                        ├─ Worker uiDoit: src onReply: [:r | cmd resumeExecutionWithResult: r]
                        └─ IMP returns nil             (AppKit keeps pumping)
                  ...primary VM works; the window stays live...
                  reply drains on main → the block resumes the command
osascript ◀──AE── the result
```

**The world owns the suspended command.** The IMP hands the command object
itself into the dispatch as an ordinary argument, so it arrives in
Smalltalk as an `ObjcRef` — and everything after that is world-side.
`suspendExecution` goes through the bridge *inside* the synchronous
dispatch (it must precede the IMP's return); the reply block closes over
the ref and later sends `resumeExecutionWithResult:`; the error
properties (`setScriptErrorNumber:` / `setScriptErrorString:`) are plain
ObjC setters the world can call before resuming. An earlier draft of this
document specified a Rust-side ticket→command table and a world→Rust
resume primitive; the review deleted both. Holding AppKit objects the
framework will not retain for us is already this GUI's ownership
discipline — the toolbar items — and the command is just one more.

Details that are easy to get wrong:

**Exactly one resume.** The reply and the time limit race, and resuming a
command twice is not survivable. A one-shot guard per suspended command
makes whichever fires second a no-op.

**The time limit has to be ours.** If the primary never replies — it
died, or the doit is an infinite loop — nothing resumes the command, and
the script hangs until AppleScript's own two-minute reply timeout
delivers a useless error. The ~4Hz beat that already refreshes the
metrics checks a world-side deadline list and resumes an expired command
with a script error. No `NSTimer`, no new machinery.

**Resumption must happen on the main thread.** True by construction — the
reply is drained on main by the same flag/wake mechanism every UI refresh
uses (`cocoa_gui_flag_and_drain.md`) — but it is a constraint, not an
accident, so it is written down.

**Re-entrancy.** `dispatch_callback` fails closed if a callback is already
active on the thread ([embed.rs](src/embed.rs)), which is correct and must
stay. Suspension is compatible: the IMP *returns* before the wait, so the
callback is over by the time the run loop turns, and the resume is a
fresh top-level entry. Concurrent `evaluate`s compose the same way — each
suspends, and they queue on the primary's serial doit queue. This is the
same reason the design works at all — the AppKit run loop is entered with
the VM quiescent (`cocoa_gui_design.md` §1).

**The reply must distinguish error from value.** `uiDoit:onReplyTimed:`
answers text either way — the workspace prints both the same, so it never
had to care. A script does: `evaluate "1/0"` must raise
`error number -10000`, not deliver the *text* "ZeroDivide" as a success,
and string-matching a result is not a protocol. The reply needs an
explicit discriminator — `uiDoit:onReply:onError:`, following the
precedent `DnsService` already sets
(`resolve:timeoutMs:onReply:onError:`, [75_dns.mst](world/75_dns.mst)) —
a change to the worker protocol, not to scripting.

## 6. How it lands on the existing bridge

### 6.1 What is reused

Nearly everything. Registering an ObjC class at runtime with typed IMPs
that dispatch into the world is exactly the machinery behind the toolbar
delegate: `objc_allocateClassPair`, `imp_ptr!`, `RetShape`, the callback
door. `performDefaultImplementation` returns `id`, which `RetShape::Id`
already covers.

### 6.2 What is new

| Piece | Where | Size |
|---|---|---|
| non-`NSObject` superclass in `register_class` | [objc_delegate.rs:535](src/runtime/objc_delegate.rs:535) hardcodes `NSObject` | one line + a parameter |
| `MacvmEvaluateCommand` etc. — `NSScriptCommand` subclasses whose IMPs hand the command itself into the dispatch | `objc_delegate.rs` | one IMP each |
| a `#app` role: `application:delegateHandlesKey:` + a getter/setter per property, absorbing the terminate IMP it displaces | `objc_delegate.rs`, replacing the minimal delegate at [objc.rs:336](cocoa_gui/src/objc.rs:336) | one IMP per property |
| the guest-error message reaching the script (§6.3) | `embed.rs` | small |
| `macVM.sdef` | `Contents/Resources/` | the document |
| `NSAppleScriptEnabled`, `OSAScriptingDefinition` | generated Info.plist | two keys |
| world-side `CocoaScript` class: verbs, suspended-command ownership, the deadline list, the enum-code table | `world/` | the feature |

The property route is worth calling out. Scripting properties on
`application` are resolved by KVC against `NSApp`, and the sanctioned way
to answer them from your own object — rather than subclassing
`NSApplication` or injecting methods into a framework class — is
`application:delegateHandlesKey:` on the app delegate. macVM installs a
delegate today, but on the wrong side of the boundary for this job: it is
a cocoa_gui-crate class whose one IMP
(`applicationShouldTerminateAfterLastWindowClosed:`) answers a constant,
with no door into the world — `dispatch`/`lookup_entry` are private to
objc_delegate.rs. So the plan is not "extend it" but "replace it": a
`#app` role registered by the same machinery as every other delegate,
carrying the terminate IMP it displaces.

### 6.3 Errors must carry their message

`dispatch_callback` already recovers a guest `error:`/DNU through
`sigsetjmp`, restores the clean idle baseline, and answers the shape
default — the run loop keeps pumping, which is why a bad delegate does not
kill the app. But it then does this:

```rust
let _ = deopt_trap::take_last_guest_fatal_message();
```

The message is *available and discarded*. For a delegate answering a row
count that is right. For a scripting interface it is the whole point: the
script needs `error "MessageNotUnderstood: Integer>>foo" number -10000`,
not silence.

So the scripting path needs a variant that returns the message rather than
dropping it, and maps it onto `setScriptErrorString:` /
`setScriptErrorNumber:`. Nothing else about the recovery changes.

Scope it precisely: this is for errors raised *inside a synchronous
handler*, so it ships with stage 2 — a `browse` whose handler itself
fails must become a script error, not a silent success. `evaluate`'s
guest errors never come through here at all; they travel the reply path,
which is why §5 requires the reply discriminator instead. And *expected*
failures (an unknown class name the handler checks for) need neither
mechanism: the world holds the command and sets the script error
properties directly through the bridge.

One thing this does *not* fix, and the document should not pretend
otherwise: a Rust **panic** inside an IMP is `panic_cannot_unwind` →
`SIGABRT`, not a recoverable guest error. That is what took the app down on
2026-08-06 when a half-built world left a delegate method missing. Guest
errors are handled; panics are a separate hardening question.

## 7. Phase 2: the image as an object model

This is the part worth building macVM's scripting *for*, and it is
deliberately not in v1.

```applescript
tell application "macVM"
    name of every Smalltalk class whose name starts with "Ordered"
    source of Smalltalk method "printOn:" of Smalltalk class "Integer"
    count Smalltalk methods of Smalltalk class "Dictionary"
end tell
```

| Class | Properties | Elements |
|---|---|---|
| `Smalltalk class` | `name`, `comment`, `superclass name`, `package`, `instance variable names` | `Smalltalk method` |
| `Smalltalk method` | `selector`, `source`, `class side` (boolean) | — |

Named `Smalltalk class`, never `class`: `class` is a reserved property on
every AppleScript object and an element of that name is unusable.

Two reasons to defer it. It needs `NSScriptObjectSpecifier` support and
KVC-compliant container accessors, which is a materially larger surface
than four commands and six properties. And the data is already in the
SQLite image with a send-edge index behind it
(`image_store`/`host_service`), so the interesting question is how to
project that as an object model without re-querying per element — a design
problem in its own right, not a bolt-on.

Writing is a further step again. `set source of Smalltalk method ... to
"..."` is `Accept` in the browser, with everything that implies about
live-compile and the IC epoch invariant. v1 stays read-only; `evaluate`
remains the escape hatch for anyone who wants to compile from a script.

## 8. What is deliberately excluded

**`do script`.** Terminal's verb, and script authors know it — but it reads
as *run this and discard*, whereas the primitive here answers a value.
`evaluate` says what it does.

**A `print` command.** The Standard Suite's `print` means printing to
paper. Overloading it with Smalltalk's Print It would be a genuine
collision. Scripts wanting Print It's behaviour set `workspace text` from
`evaluate`'s answer.

**Typed results.** See §3. `printString` is the contract.

**App Intents / Shortcuts actions.** The modern Apple answer, and out of
reach: App Intents is a Swift-only framework needing a new build target.
AppleScript support reaches Shortcuts anyway through *Run AppleScript*, at
a fraction of the cost. Revisit if Shortcuts becomes a real use case.

**Scripting the dev binary.** Scripting keys live in Info.plist, so this
works from `macVM.app` and not from `target/release/macvm-cocoa`. That is
fine, and it is why `MACVM_COCOA_CTL` stays: development drives the bare
binary, automation drives the app.

## 9. Security posture

The socket this replaces binds a TCP port and evaluates whatever arrives.
Any local process can drive the VM, and there is no consent step,
no identification of the caller, and no audit trail.

Apple Events are better on every axis that matters: macOS mediates them
with per-client-application consent (the *"X wants to control macVM"*
prompt, recorded and revocable in System Settings), the caller is
identified by code signature, and the vocabulary bounds what a caller can
ask for. `evaluate` is still arbitrary code execution by design — but it is
*consented* arbitrary code execution, from a named application, which is a
different thing from an open port.

Recommended follow-ups, out of scope here but noted so they are not lost:

- Move `MACVM_COCOA_CTL` from TCP to a Unix domain socket. Same
  capability, no network surface. `tools/macvm.entitlements` already flags
  the TCP form as a prerequisite for review.
- A Services menu entry (*"Evaluate in macVM"* on selected text anywhere)
  — small, and the most Mac-feeling thing on this list.
- Accessibility labels on the toolbar group and panes: independently
  worth doing, and what makes UI-level automation possible at all.

## 10. Staging

1. `.sdef` + Info.plist keys + the `#app` role carrying the
   `application` properties (§4, §6.2). An earlier draft claimed this
   stage needed "no new Rust class registration"; not quite — the
   existing delegate has no door into the world, so the role comes
   first. Provable with `osascript -e 'tell application "macVM" to get
   transcript'`, and `bounds of window 1` arrives free with the Standard
   Suite.
2. `browse`, `snapshot`, `clear transcript` — synchronous commands: the
   `NSScriptCommand` subclasses, the `register_class` superclass
   generalisation, and §6.3 (a handler failure must be a script error,
   not a silent success).
3. `evaluate` — suspension with world-owned commands, the one-shot
   resume guard, the beat-loop deadline list, and the
   `uiDoit:onReply:onError:` reply discriminator (§5).
4. Phase 2 object model (§7), if it earns its way.

**Landed 2026-08-06 (stage 1, first half):** [`tools/macVM.sdef`](tools/macVM.sdef)
— the whole vocabulary above, `sdp`-clean; [`tools/check-sdef-parity.py`](tools/check-sdef-parity.py)
— the §4 parity gate; and `make-macapp.sh` installs the sdef into
`Contents/Resources/` and sets both Info.plist keys, for the cocoa app only
(the web app gets neither). The `#app` role is **not** implemented yet, so
the Standard Suite works (`quit`, `bounds of window 1`, `name`, `version`)
while every macVM-Suite verb and property answers *"doesn't understand"*
until it lands. Two notes from building it:

- The parity gate reads the *registration sequence* out of `CocoaUI class >>
  startup` statically rather than booting the world headless as this document
  first proposed — `Views` is populated only under Cocoa, so there is no
  headless registry to read. The order it recovers (workspace, browser,
  browser2, find, editor, outliner, canvas, help, debugger) independently
  confirms §2's table, and swapping two enumerators does make the gate fail.
- **`sdp -fh` does not merge `class-extension` members into the extended
  class**, so the generated header shows the four commands but none of the
  six properties. That is an `sdp` limitation, not a defect in the file:
  Calendar's own sdef loses its `class-extension` element the same way. Read
  the round-trip gate below as a *parse* check; the dictionary viewer (or
  `sdef` on the built app) is what shows the properties.

Each stage is independently shippable, and each has a mechanical gate:
Script Editor's dictionary viewer renders the terminology; `sdef
"dist/macVM.app" | sdp -fh --basename macVM` round-trips the file;
`defaults write com.macvm.cocoa NSScriptingDebugLogLevel 1` traces
command dispatch while developing; and an `osascript` smoke line per
verb sits beside the existing boot gates. Scripting keys live in
Info.plist, so every gate runs against `dist/macVM.app`, never the bare
dev binary (§8).
