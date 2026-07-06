# Library-Porting Journal

A running log of primitives, VM capabilities, and library methods noticed
as **missing or deferred** while porting Strongtalk's standard library
into `world/*.mst` (the W1 "library wave 1" effort — see `docs/WORLD.md`
and `docs/SPRINTS.md`'s W1 entry). Not a design doc — a worklist for
whoever picks these up once the compiler sprints (S12/S13/...) land.
Updated as porting continues; entries move to "Resolved" once built.

## How to read this

Each entry says: what's missing, what it blocks, how it was discovered
(a real gap hit during porting, not speculation), and — where relevant —
the most direct path to filling it given what already exists.

## Missing VM primitives / capabilities

### Wall-clock (calendar) time
**Blocks**: `Date class>>today`, `Time class>>now`, `Time class>>dateAndTimeNow`
(all skipped in `world/30_date_time.mst`).
**Not** a gap in `millisecondClock` — that primitive is intentionally
elapsed-time-since-VM-start (`vm.start_instant.elapsed()`,
`src/runtime/primitives.rs::prim_millisecond_clock`), matching Strongtalk's
own `Time class>>clockValue` doc comment ("elapsed time since Smalltalk
started") — not a design oversight, just a different clock than the one
Date/Time need.
**Path**: likely does NOT need a new bespoke VM primitive at all. POSIX
`time()` is already catalogued in the sibling `cocoa_data` repo
(`posix_functions`: header `time.h`, `ret_type time_t`; `posix_function_abi`:
`ret_class=g arg_classes=g` — the simplest possible signature, a single
GPR in, a single GPR out, no float/struct marshaling). Once S20's FFI
Tier-1 POSIX path is actually built (`docs/FFI.md` — designed and the
offline generator (`ffi_gen`) works, but the VM-side primitive/installer
is still to come), `Time class>>now` could plausibly be a direct
`{{<libc time: NULL>}}`-style call rather than a new primitive. Revisit
once S20 lands before assuming a dedicated primitive is needed.

### FFI is designed but not callable yet (confirmed, not assumed)
Checked directly rather than trusting the earlier design-phase memory:
`grep -rn "FFI\|Alien" src/runtime/primitives.rs` and `world/*.mst` both
return **zero matches** — no `<primitive: FFI ...>` is registered, and the
`Alien` base representation class (`ffi_gen`'s own generated output
subclasses it — `Alien subclass: NSColorAlien [...]`) doesn't exist in
`world/` either. Running `cargo run -p ffi_gen --bin generate_ffi` prints
its own header confirming this: *"Forward-declared: no `<primitive: FFI
...>` exists in `src/runtime/primitives.rs` yet."* So the real S20 sprint
needs (at minimum) both an `Alien` class (`IndexableBytes`-backed, typed
accessors per `docs/FFI.md` §4) and the primitive itself before any
generated binding — or anything built on top, like the wall-clock item
above — actually runs.

### `Object>>instVarAt:put:` (confirmed missing, not just awkward)
**Blocks**: a generic `Object>>copy`. Confirmed by actually trying to
build it (`world/32_object_protocol_gaps.mst`'s first draft) and hitting
`doesNotUnderstand: instVarAt:put:` at runtime — not a guess. `instVarAt:`
(read, primitive 25) exists; there is no write-side equivalent anywhere in
`src/runtime/primitives.rs`. Named-ivar assignment (`x := val`) only works
today because the *compiler* resolves the ivar name to a fixed slot
number at compile time and emits a direct store-to-slot-N bytecode —
writing an arbitrary, **runtime-computed** slot index (which a generic
copy fundamentally needs, since instance-variable count/layout varies per
class) has no compile-time name to hook into. This needs a real
primitive; no pure-Smalltalk workaround exists.
**Path, once the primitive exists**: `Object>>copy` becomes straightforward
— `Behavior>>instVarNames` (`03_behavior.mst`) already reports a class's
own declared ivars (confirmed directly: `SortedCollection instVarNames`
is `#(#sortBlock)`, and `instVarAt: 4` is indeed where `sortBlock` lives,
slots 1-3 being `OrderedCollection`'s own inherited
`array`/`firstIndex`/`lastIndex`) — walk the superclass chain summing
`instVarNames size` (treating `nil` as 0) for the total slot count, then
`1 to: total do: [:i | result instVarAt: i put: (self instVarAt: i)]`,
finished with a `postCopy` hook (Strongtalk's own convention — several
ported classes' real source defines `copy ^self` for immutable values, or
override `postCopy` for a deeper copy, e.g. `Bag`'s own `contents := contents copy`).
Scope note for whenever this lands: a generic `Object>>copy` this way is
shallow at the NAMED-ivar level only — `Dictionary`/`Set`/`Bag`/
`OrderedCollection`/`SortedCollection` would still share their internal
array(s) with the original unless each also gets its own `postCopy`
override to deep-copy that ivar specifically.

## Missing Smalltalk-level library methods (no new primitive needed)

### `Symbol`/`String>>asString`
**Status: unverified, not confirmed missing** — `world/30_date_time.mst`'s
`Date>>printOn:` needed to print a Symbol (`self month`, e.g. `#July`) as
plain text and just used `nextPutAll: self month` directly (works because
`Symbol` IS-A `String` IS-A `ArrayedCollection`, so `nextPutAll:`'s generic
`do:`-based character iteration applies unchanged) — sidestepping the
question rather than answering it. Worth an explicit check next time
something actually needs `aSymbol asString` as its own message.

## Genuinely unimplemented in Strongtalk's own source (not a MACVM gap)

These would be **new feature work**, not porting — Strongtalk itself never
finished them (`Date.dlt`'s own methods literally say `self unimplemented`).
Listed here so nobody mistakes "not in world/" for "forgot to port it."

- **Date's Julian-day calendar arithmetic**: `julian`, `Date class>>julian:`,
  `leapYear:`, `daysInMonth:forYear:`, `daysInYear:`, `asSeconds`, and by
  extension `addDays:`/`subtractDays:`/`subtractDate:` (all three call the
  unimplemented `julian`). A real, correct implementation would need a
  proleptic-Gregorian day-number algorithm (e.g. the standard
  civil-from-days / days-from-civil conversion) — buildable, just not a
  port of anything that exists.

## Deliberately out of scope (non-goals, not gaps)

- **`Rectangle`** and `Point`'s `expandRect:`/`insetRect:`/`corner:`/`extent:`
  (`world/28_point.mst`) — Strongtalk's own UI-toolkit geometry class.
  MACVM's GUI is HTML/JS-rendered (`gui/src/objc.rs`'s WKWebView shell),
  not Smalltalk-`Rectangle`-based. Matches `docs/WORLD.md`'s Layer C ("UI,
  replaced by WKWebView").
- **`LinkedList`** — Strongtalk's own class comment: *"This class is bogus
  and should become obsolete, but it is in the BlueBook, so it is here."*
  Its own authors disowned it. It also requires an intrusive `Link`
  protocol (elements must implement `nextLink:`/`linksDo:`/`linkAt:`/
  `noNextLink` themselves — very different from every other collection
  ported here, which hold arbitrary objects) for a use case
  (position-free O(1) insert/remove given a link reference)
  `OrderedCollection` already covers for virtually every real purpose.
  Not ported; not worth the intrusive-protocol cost for something the
  source itself calls bogus.
- Strongtalk's generic-type/mixin/protocol machinery generally — a
  language-level non-goal already covered by `docs/WORLD.md`'s Layer C,
  not specific to any one class.
- **`AssocSet`/`KeyedSet`** — the class's own doc comment (signed by a
  real Strongtalk author, Dave Griswold): *"KeyedSets and AssocSets are
  only marginally useful in a Smalltalk system because there are already
  Set and Dictionary... I have put these classes in as examples."*
  Already superseded by `Set`/`Dictionary`, which MACVM has.

## Coverage status (as of this pass)

**Ported and tested** (`world/21`-`32_*.mst`, 4602 assertions, 0
failures): the full Collection enumeration protocol (`select:`/`reject:`/
`collect:`/`occurrencesOf:`/`allSatisfy:`/`anySatisfy:`), `Set`,
`Fraction` (+ `Integer>>/`), `ReadStream`, `SortedCollection`, `Bag`,
`Random` (Park-Miller LCG, verified against Strongtalk's own published
conformance vector), `Point`, `IdentityDictionary`/`IdentitySet`,
`Date`/`Time`, `ReadWriteStream`, `Collection>>=`/`hasSameElementsAs:`
and `Bag>>=` (content-based equality, unblocked once `Bag` existed).

**Swept the remaining ~1000 Strongtalk source files and found nothing
else cleanly portable right now** — what's left all belongs to
categories that are *already* separately scoped, not overlooked:
- **Exceptions** (ANSI hierarchy, SUnit 3.1 convergence) — W5, needs
  `S4` solid + `W1` (this effort) as prerequisites; deliberately not
  pulled forward into W1.
- **Mirrors/reflection** — W2, needs R1/R2 primitives that don't exist.
- **File/external I/O, the real `Alien`/Cocoa bridge** — S20, blocked on
  the FFI primitive gap above.
- **`WeakArray`/`WeakSet`** — S22, needs real weak-reference GC support,
  not expressible as pure `.mst` library text.
- Strongtalk's own compiler/type-system/mirror/UI/benchmark-harness
  internals (~880 files) — `docs/WORLD.md`'s Layer C, never meant to be
  ported (different type system, different UI model, its own benchmark
  scaffolding).

## Resolved

- **`Collection>>allSatisfy:`/`anySatisfy:`** — `world/32_object_protocol_gaps.mst`.
- **`Collection>>=`/`hasSameElementsAs:`, `Bag>>=`** — `world/32_object_protocol_gaps.mst`,
  unblocked once `Bag` existed (`world/26_bag.mst`).
