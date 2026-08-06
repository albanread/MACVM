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

One suite, `macVM Suite`. Four-character codes avoid the all-lowercase
space, which Apple reserves; every macVM code starts `Mv`.

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

The Standard Suite comes in by reference, which is the ordinary idiom and
gives `quit`, `get`, `set`, `count`, `exists` for free:

```xml
<xi:include href="file:///System/Library/ScriptingDefinitions/CocoaStandard.sdef"
            xpointer="xpointer(/dictionary/suite)"/>
```

## 3. Commands

### `evaluate`

The core verb. Takes Smalltalk source, answers its `printString`.

```applescript
evaluate "3 + 4"                              --> "7"
evaluate "Integer" as target workspace        -- runs it as if typed in the Workspace
evaluate "1/0" --> error "ZeroDivide" number -10000
```

| Parameter | Type | Optional | Meaning |
|---|---|---|---|
| *direct* | text | no | Smalltalk source |
| `as target` | view enum | yes | which surface's context (default: workspace) |
| `timeout` | integer | yes | seconds before the command errors out (default 60) |

Answers `text`. It answers the *printString*, not a typed value: the guest
is a Smalltalk image whose objects have no Apple Event representation, and
inventing one for arbitrary results is exactly the kind of leaky mapping
that makes scriptable apps unpleasant. Scripts that want structure ask for
it — `evaluate "someCollection asArray printString"` — or use §7.

`timeout` deserves a note. AppleScript's own send timeout defaults to 60
seconds and a script can raise it with `with timeout of N seconds`. The
command's own `timeout` is the *guest-side* limit — how long macVM waits
for the primary VM before giving up and answering an error. A runaway doit
must produce a script error, never a hung Script Editor.

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
snapshot to POSIX file "/tmp/macvm.png"
```

Writes a PNG of the window. Wraps `CocoaUI>>snapshotTo:`, and inherits its
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
view:        workspace | browser | browser2 | editor | find
             | outliner | canvas | help | debugger
appearance:  system | light | dark
```

The `view` enumerators are the registry's own symbols, so the enumeration
is generated from `Views` rather than hand-maintained — the same discipline
`installViewMenuOn:` already follows. A view added later gets a scripting
name for free; one removed cannot leave a dangling enumerator.

## 5. Asynchrony: suspend and resume

`evaluate` is the only command that needs this, and it is the reason Apple
Events fit macVM better than a hand-rolled protocol would.

```
osascript ──AE──▶ MacvmEvaluateCommand
                      performDefaultImplementation
                        ├─ CocoaUI evaluateForScript: src ticket: n
                        │     └─ Worker uiDoit: src onReplyTimed: [...]   (posts, returns)
                        ├─ [self suspendExecution]
                        └─ return nil                    ← AppKit keeps pumping
                  ...primary VM works...
                  beat loop delivers the reply
                      CocoaUI scriptReply: text ticket: n
                        └─ [command resumeExecutionWithResult: text]
osascript ◀──AE── the result
```

Three details that are easy to get wrong:

**The command object must be retained across the suspension.** It is held
in a ticket→command table on the Rust side, keyed the same way
`MacvmDelegate`'s registry keys receivers. The world side never holds the
`NSScriptCommand`; it holds the ticket.

**The timeout has to fire on our side.** If the primary VM never replies —
it died, or the doit is an infinite loop — nothing resumes the command and
the script hangs until AppleScript's own timeout, with a useless error. A
timer armed at suspension resumes with a script error instead.

**Re-entrancy.** `dispatch_callback` fails closed if a callback is already
active on the thread ([embed.rs](src/embed.rs)), which is correct and must
stay. Suspension is compatible with it: `performDefaultImplementation`
*returns* before the wait, so the callback is over by the time the run loop
turns. The resume arrives as a fresh top-level entry. This is the same
reason the design works at all — the AppKit run loop is entered with the VM
quiescent (`cocoa_gui_design.md` §1).

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
| `MacvmEvaluateCommand` etc. — `NSScriptCommand` subclasses | `objc_delegate.rs` | one IMP each |
| ticket→command table, retained across suspension | `objc_delegate.rs` | small |
| `application:delegateHandlesKey:` + property getters/setters | the app delegate that already exists at [objc.rs:336](cocoa_gui/src/objc.rs:336) | one IMP per property |
| `macVM.sdef` | `Contents/Resources/` | the document |
| `NSAppleScriptEnabled`, `OSAScriptingDefinition` | generated Info.plist | two keys |
| world-side `CocoaScript` class | `world/` | the verbs |

The property route is worth calling out. Scripting properties on
`application` are resolved by KVC against `NSApp`, and the sanctioned way
to answer them from your own object — rather than subclassing
`NSApplication` or injecting methods into a framework class — is
`application:delegateHandlesKey:` on the app delegate. macVM already
installs a delegate for
`applicationShouldTerminateAfterLastWindowClosed:`, so the properties hang
off an object we own.

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

1. `.sdef` + Info.plist keys + `application` properties (§4). No new
   Rust class registration — properties go through the existing app
   delegate. Provable with `osascript -e 'tell application "macVM" to get
   transcript'`.
2. `browse`, `snapshot`, `clear transcript` — synchronous commands, which
   need the `NSScriptCommand` subclass and the `register_class` superclass
   generalisation but not the suspend/resume machinery.
3. `evaluate` — suspension, the ticket table, the timeout, and the
   error-message path of §6.3.
4. Phase 2 object model (§7), if it earns its way.

Each stage is independently shippable, and stage 1 alone makes the app
scriptable enough to be useful.
