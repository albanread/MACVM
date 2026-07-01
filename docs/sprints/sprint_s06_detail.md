# Sprint S06 — Core library + in-language test suite

Objective: write the MACVM Smalltalk core library as `world/*.mst` files
loaded by S5's `load_world`, deep enough that the VM tests itself in
Smalltalk (SUnit-lite, ~200 assertions) and runs fib/sieve. Implements
SPEC §1.3 (core semantics), §10 (primitive bindings), §12.3 (in-language
suite), §3.2 step 2–3.

## Prerequisites

- S5 complete: `.mst` pipeline, class-definition execution, `world.list`
  loader, `macvm run`/`repl`, top-level doIts with declare-on-assign
  globals, `<classVars:>` pragma.
- S3/S4 runtime: all SPEC §10 primitives registered (smi, Double, oops,
  ByteArray/String, BlockClosure, Symbol, control, system groups).
- Primitive **ids**: this sprint needs the concrete numbering. Pinned table
  below (§"Primitive id map") — if S3 chose different ids, update the table
  in this doc AND the `.mst` sources together; the Rust registry is the
  single source of truth.

## Deliverables

- `world/*.mst` per the load-order table below + `world/world.list`.
- `world/tests/*.mst` — SUnit-lite + test classes (~200 assertions),
  `world/tests/tests.list`, runner wiring in `tests/it_world.rs`.
- `world/bench/fib.mst`, `world/bench/sieve.mst`; `docs/PERF.md` started
  with the S6 interpreter baseline.
- Genesis skeleton extension in `src/memory/` (below) if S1 built less.

## Design

### Genesis skeleton (Rust-side prerequisite)

S5's reopen rule ("superclass must match, no shape change") means every
class whose superclass differs from plain `Object`, but which must exist
before `world/` loads (because genesis instances or literals reference it),
has to be created in `Universe::genesis()` with its **final** superclass.
Pinned genesis klass set (empty method dictionaries; formats per SPEC §2.3):

```
Object
├─ UndefinedObject(nil)   Boolean ├─ True(true) ├─ False(false)
├─ Magnitude ─ Character
│            └─ Number ─ Double
│                      └─ Integer ─ SmallInteger
│                                 └─ LargeInteger ─ LargePositiveInteger
│                                                 └─ LargeNegativeInteger
├─ Collection ─ SequenceableCollection ─ ArrayedCollection ─ Array
│             │                        │                   ├─ ByteArray
│             │                        │                   └─ String ─ Symbol
│             │                        ├─ OrderedCollection  └─ Interval
│             └─ HashedCollection ─ Dictionary
├─ Association   ├─ Message      ├─ BlockClosure  ├─ CompiledMethod
├─ Behavior ─ Class              └─ Metaclass (⊂ Behavior)
├─ WriteStream   ├─ TranscriptStream
└─ SystemDictionary
```
(Context/Process formats stay VM-internal, no Smalltalk class in v1.)
All of these are well-known oops (macro list per SPEC §3.1). The
`smalltalk` namespace object is an instance of `SystemDictionary`
(format `IndexableOops`, VM-managed layout — it is NOT a library
`Dictionary`; only `startUp` and friends are sent to it).

> **SPEC-QUESTION:** SPEC §3.1 lists core klasses with an ellipsis and §2.4
> shows no `Behavior`. Proposal (this doc assumes it): amend §3.1 to the
> tree above; metaclass superclass chain ends `… → Object class → Class →
> Behavior → Object`, so class-side methods inherit from Class/Behavior.
> Also: `Smalltalk` is a `SystemDictionary` (VM layout), not a library
> Dictionary — §3.1's "a Dictionary of Association" should say so.

### Primitive id map (pinned for `.mst` sources)

Group/base: smi 1–19 (`+`1 `-`2 `*`3 `//`4 `\\`5 `bitAnd:`6 `bitOr:`7
`bitXor:`8 `bitShift:`9 `<`10 `<=`11 `>`12 `>=`13 `=`14 `~=`15);
Double 20–39 (`+`20 `-`21 `*`22 `/`23 `<`24 `=`25 `sqrt`26 `floor`27
`asDouble`(smi→D)28 `fromSmi:`29 `printDigits`30 — returns a String of the
shortest round-trip decimal); oops 40–59 (`identityHash`40 `class`41 `==`42
`basicNew`43 `basicNew:`44 `instVarAt:`45 `at:`46 `at:put:`47 `size`48);
ByteArray/String 60–69 (`at:`60 `at:put:`61 `size`62
`replaceFrom:to:with:startingAt:`63 `hash`64 `compare:`65 → smi <0/0/>0);
BlockClosure 70–79 (`value`70 `value:`71 `value:value:`72
`value:value:value:`73 `valueWithArguments:`74); Symbol 80 (`intern` on
String → Symbol); control 85 (`ensure:`) 86 (`ifCurtailed:`); system 90+
(`quit:`90 `gcScavenge`91 `gcFull`92 `millisecondClock`93
`printOnStdout:`94 `sourceCompile:`95 `error:`96 — prints message + VM
stack trace, terminates per SPEC §6.3).

> **SPEC-QUESTION:** SPEC §10's table has no `error:`/stack-trace primitive
> and no Double→String support; both are required here (ids 96, 30).
> `instVarAt:` (45) must also accept `Klass`-format receivers, reading the
> 8 named fields (§2.4 order, 1-based) — Class accessors depend on it.

### Load order — `world/world.list`

Ordering law: **name resolution happens at method-compile time**, so a file
may only *reference* globals bound earlier (or in genesis); a class may be
reopened later to add methods that need later classes. Two ordering
consequences are load-bearing: Transcript comes before everything chatty
(it uses only primitive 94, not streams), and `printString` lands late
(needs WriteStream).

```
01_object.mst          10_array.mst            18_writestream.mst
02_nil_boolean.mst     11_bytearray.mst        19_printing.mst   (reopens)
03_behavior.mst        12_string.mst           20_system.mst
04_transcript.mst      13_symbol.mst
05_magnitude.mst       14_association.mst      tests/00_sunit.mst
06_smallinteger.mst    15_ordered.mst          tests/01…13_*_tests.mst
07_largeinteger.mst    16_dictionary.mst       tests/99_run_all.mst
08_double.mst          17_interval.mst         (via tests/tests.list,
09_character.mst                                loaded by it_world.rs only)
```
`world/bench/*.mst` are not in world.list; `macvm run` loads them.

### Per-class specification

Notation: `(p N)` = binds `<primitive: N>`; `*` = has Smalltalk fallback
after the primitive. Instvars listed in declaration order. Every class
below is a **reopen** of a genesis klass (S5 rule: no instvar/shape
declarations on reopen) **except** where marked NEW — but note v1 pins ALL
listed classes into genesis, so instvar layouts below are implemented in
`Universe::genesis()` (Rust), and the `.mst` files declare no instvars.

#### 01 Object (superclass nil)
File 01 defines **only** methods that need no other class's methods:
`class` (p41) · `==` (p42) · `identityHash` (p40) ·
`hash [ ^self identityHash ]` · `yourself` · `basicNew` (p43) ·
`basicNew:` (p44) · `instVarAt:` (p45) · `error:` (p96) ·
`subclassResponsibility [ self error: 'subclass responsibility' ]` ·
`doesNotUnderstand: aMessage [ self error: 'does not understand ' ,
aMessage selector asString ]` (Message instvars pinned: `selector
arguments`; accessors defined here via instVarAt:).
Boolean-dependent methods land in the 02 reopen: `= [^self == x]`,
`~= ~~ isNil notNil`. Printing methods land in the 19 reopen:
`printOn: printString displayString inspect`.

#### 02 UndefinedObject / Boolean / True / False
UndefinedObject: `isNil [^true]` `notNil [^false]` `printOn:` (in 19).
Boolean (abstract): `&`/`|` as non-short-circuit (`& b [ ^self and: [b] ]`
— NO: `and:` with non-literal block is a real send needing True/False
`and:`; define directly). True: `not [^false]` `and: b [^b value]`
`or: b [^true]` `ifTrue: b [^b value]` `ifTrue: b ifFalse: c [^b value]`
`ifFalse: b [^nil]` `ifFalse: b ifTrue: c [^c value]` `& b [^b]`
`| b [^true]` `printOn:`(19). False: mirror images. These are the
**real-send fallbacks** for S5's inlined selectors — semantics must match
the inlined patterns exactly (incl. `ifTrue:` answering nil on the false
leg). Object gains `= ~= ~~ isNil [^false] notNil [^true] isString
[^false] isSymbol isInteger isDouble isCharacter isClass [^false]`
(each overridden true in its class later — poor-man's type tests used by
coercion and `<<`).

#### 03 Behavior / Class / Metaclass
Behavior: `name [^self instVarAt: 5]` `superclass [^self instVarAt: 3]`
`instVarNames [^self instVarAt: 6]` `new [^self basicNew]`
`new: n [^self basicNew: n]` `inheritsFrom: k` (walk superclass chain).
Class/Metaclass: `printOn:`(19; metaclass prints `X class`). Object gains
`isKindOf: k [ ^self class == k or: [self class inheritsFrom: k] ]`
`isMemberOf: k [ ^self class == k ]` `respondsTo:` — **deferred** (needs
MethodDictionary reflection; out of scope v1).

#### 04 TranscriptStream + Transcript global
TranscriptStream (no instvars): `show: aString [ self basicPrint:
aString ]` where `basicPrint:` binds p94 (accepts any bytes-format
object). `cr` needs a newline string but Character/String methods are not
loaded yet — **pin**: the file uses a string literal containing a real
newline byte, i.e. `self show: '` + physical line break + `'` (lexer rule
L8 passes raw bytes, no escape syntax exists). Keep that literal alone on
its lines with a warning comment; file 19 re-defines `cr` via
`Character nl`. `<<` and `tab`/`space` also come in 19 (need
printString). Top-level doIt: `Transcript := TranscriptStream new.`
From file 04 onward, later files may debug-print while loading.

#### 05 Magnitude (abstract)
`< x [ self subclassResponsibility ]` — add `Object>>subclassResponsibility
[ self error: 'subclass responsibility' ]` to 01. Derived: `> x [^x < self]`
`<= x [^(x < self) not]` `>= x [^(self < x) not]` `max: min: between:and:`.
Number (abstract): `abs negated isZero sign squared` in terms of `< - *`;
`to: stop [^Interval from: self to: stop]` (compiles in 17's reopen — so
Number's `to:` actually lands in 17_interval.mst) ; `to: stop do: aBlock`
**in 05** as the real-send twin of the S5 inline:
```smalltalk
to: stop do: aBlock [
  | i | i := self.
  [ i <= stop ] whileTrue: [ aBlock value: i. i := i + 1 ].
  ^self
]
```

#### 06 SmallInteger (+ Integer shared)
Integer: `isInteger [^true]` `timesRepeat:` `even odd` `gcd:` (Euclid) ·
`printOn:` / `printString:` (radix) in 19 · `asInteger [^self]`
`hash [^self]`. SmallInteger — every arithmetic/comparison selector binds
its primitive with a fallback (`*` = shown for `+`; others analogous):
```smalltalk
+ aNumber [
  <primitive: 1>
  aNumber isDouble ifTrue: [ ^self asDouble + aNumber ].
  ^self asLargeInteger addLarge: aNumber asLargeInteger
]
```
`- (p2)* * (p3)* // (p4)* \\ (p5)* bitAnd:(p6) bitOr:(p7) bitXor:(p8)
bitShift:(p9)* < (p10)* <= (p11)* > (p12)* >= (p13)* = (p14)* ~= (p15)*`.
Comparison fallbacks: Double arg → `self asDouble < arg`; Large arg →
`self asLargeInteger compareLarge:`. `//`/`\\` fallback on overflow
(only `SmallInteger minVal // -1`) → Large path. `asDouble` (p28)
`asLargeInteger [ ^LargeInteger fromSmallInteger: self ]` `asCharacter
[^Character value: self]` `hash [^self]` `identityHash [^self]`
(smis: identity = value; p40 must already do this — assert in tests).

#### 07 LargeInteger (the concrete part)
Representation (pinned): `IndexableBytes` body = **base-256 digits,
little-endian, no trailing (most-significant) zero bytes, never empty**.
Sign = the class (`LargePositiveInteger` ≥ 0 — but any value that fits a
smi must normalize *to* a smi, so a live Large is always > `SmallInteger
maxVal` or < minVal). All algorithms are pure Smalltalk on `basicNew:` +
byte `at:`/`at:put:` (p60/61/62 work on any bytes-format object).

Class-side `LargeInteger fromSmallInteger: n` — the naive `n abs` breaks
on `SmallInteger minVal` (abs overflows), so the **pinned algorithm**
never takes an absolute value:
1. If `n >= 0`: peel magnitude digits directly — repeat
   `d := n \\ 256. n := n // 256` until n = 0; answer a
   LargePositiveInteger with those digits.
2. If `n < 0`: peel with the same floored ops — `q := n // 256` (floors
   toward −∞), `d := n - (q * 256)` (always 0..255), collect d, continue
   with `n := q` until q = −1. The collected k digits are the low k bytes
   of the two's-complement of |n|; take the two's-complement of the digit
   string (invert each byte, add 1 with ripple carry, appending a final
   carry byte if it survives) to obtain the magnitude; answer a
   LargeNegativeInteger.
Every operation stays in smi range for all inputs including minVal.
Test edge table: `minVal`, `minVal + 1`, −257, −256, −255, −1 (the last
few only reachable via explicit `fromSmallInteger:`, since in-range
values normalize back to smis).

Instance methods (m = self digits length, n = arg's):
- `normalize` (class-side `LargeInteger reduce: aLarge`): strip
  high zero bytes; if length ≤ 8 assemble a candidate value via
  `v := 0. k downTo… v := (v bitShift: 8) + digit` guarded so it stops
  early if v would exceed smi range (check `v > (SmallInteger maxVal
  bitShift: -8)` before each shift); if it fits (incl. the negative bound)
  answer the smi, else the Large.
- `addLarge: b` (same-sign magnitude add): result `basicNew: (max(m,n)+1)`;
  ripple carry `s := da + db + c. r at: i put: (s \\ 256). c := s // 256`;
  normalize. Mixed signs → `subMagnitude:` (below) with the larger-
  magnitude operand's class. Public `+`/`-` on LargeInteger dispatch on
  arg class (`isInteger` → large path via `asLargeInteger`; `isDouble` →
  `asDouble` path) then on the two signs → add/sub of magnitudes.
- `subMagnitude: b` (|self| ≥ |b| required — caller compares first with
  `compareMagnitude:`): ripple borrow, normalize.
- `compareMagnitude: b`: longer wins; equal lengths compare bytes from the
  high end; answer −1/0/1 (smi). `compareLarge:` folds in signs.
  `<' '<= = >' etc. and Magnitude protocol derive from it. `hash`: (p64)
  on the digit bytes.
- `mulLarge: b`: schoolbook O(m·n): result `basicNew: m+n`; for each i,j:
  `t := (r at: i+j-1) + (da * db) + c` — da·db ≤ 255·255, plus carry ≤
  65535ish: all smi-safe; store `t \\ 256`, carry `t // 256`. Sign = xor
  of classes. Normalize.
- `quoRem10:`-style small division `divMod: d` (d a smi 2..255? pin
  1 < d ≤ 255): high-to-low `part := rem * 256 + digit. q_i := part // d.
  rem := part \\ d.` answers an Association quotient→remainder (quotient
  normalized). This is all `printOn:` needs (repeated ÷10). Large÷Large is
  **not implemented** (v1 limitation; `//` with Large divisor →
  `self error: 'large division unimplemented'`).
- `negated`: same digits, opposite class, normalize (handles the
  fits-in-smi boundary, e.g. `(maxVal + 1) negated` = a smi).
- `printOn: s` (in 19): peel decimal digits via `divMod: 10` into a
  String/OrderedCollection, emit sign then reversed digits.
- `asLargeInteger [^self]` `isInteger [^true]` `asDouble`: fold digits
  high-to-low `v := v * 256.0 + digit asDouble` (precision loss accepted).

#### 08 Double
`isDouble [^true]` · `+`(p20) `-`(p21) `*`(p22) `/`(p23) `<`(p24) `=`(p25)
each with fallback coercing an Integer arg via `asDouble` (Large included);
`sqrt`(p26) `floor`(p27 → Integer) `ceiling truncated rounded abs negated`
derived; `> >= <=` via Magnitude; `hash [^self floor hash]` (documented
weak); `printOn:` (19) via `printDigits` (p30). Division by zero: p23
fails → fallback `self error: 'division by zero'`? — **pin**: p23 never
fails on zero (IEEE inf/nan semantics), only on non-Double arg.

#### 09 Character
Instvar (genesis): `value` (smi code point). Class vars: `Table`.
Class-side: `value: v` — flyweight per SPEC §1.3:
```smalltalk
Character class >> value: v [
  (v >= 0 and: [ v < 256 ]) ifTrue: [ ^Table at: v + 1 ].
  ^self basicNew setValue: v
]
Character class >> initTable [
  Table := Array new: 256.
  0 to: 255 do: [:i | Table at: i + 1 put: (self basicNew setValue: i) ]
]
```
Top-level doIt in this file: `Character initTable.` Instance: `asInteger
value` (getter) `setValue:` `= [^self value = x value]` — but flyweights
make `==` correct for Latin-1; still define `=`+`hash [^value]` for
>U+00FF. `< ` via value (Magnitude works). `asCharacter [^self]`
`isCharacter [^true]` `isVowel isDigit isLetter asUppercase asLowercase`
(ASCII-only arithmetic). `printOn:` (19): `$` + the character;
`Character nl` `cr` `tab` `space` class-side constants (`value: 10` etc.)
— 04's Transcript `cr` is retro-simplified in 19 to use these.

#### 10 Array / (Sequenceable/Arrayed)Collection shared
Collection: `isEmpty notEmpty size [count via do:]` `includes: detect:
detect:ifNone: inject:into: do:separatedBy: addAll:? (only where add:
exists) asOrderedCollection asArray printOn:(19)`.
SequenceableCollection: `do:` via `1 to: self size do:` · `doWithIndex: /
keysAndValuesDo:`(seq version) `first last copyFrom:to: reversed
indexOf:ifAbsent: = (elementwise, same class)` `,` (concat via species
`new:` + replace loop) `collect:` (species-preserving: Array→Array,
String→String, OC→OC — implement per concrete class, NOT generically:
Array/String get their own `collect:`; no `species` machinery in v1).
Array: `at:`(p46) `at:put:`(p47) `size`(p48) `at:ifAbsent: [ (i between: 1
and: self size) ifFalse: [^absentBlock value]. ^self at: i ]`
`copyFrom:to:` (basicNew: + loop; no replace prim for oops arrays — loop
`at:put:`) `with: with:with: with:with:with:` class-side constructors,
`printOn:`(19) as `#(…)`-less `(a b c)`? **pin**: prints `#(1 2 3)` form
recursively.

#### 11 ByteArray
`at:`(p60) `at:put:`(p61) `size`(p62) `at:ifAbsent:` `copyFrom:to:`
(basicNew: + `replaceFrom:to:with:startingAt:` p63) `do: collect:`
(collect: → Array — elements may be any smi result? pin: collect: on
ByteArray answers an Array) `hash`(p64) `= [ compare: == 0 and class
check ]` `printOn:`(19) as `#[1 2 3]`.

#### 12 String
`isString [^true]` `at:` (p60 → **Character**? p60 answers a smi byte —
pin: String>>`at:` wraps: `^Character value: (self basicByteAt: i)` where
`basicByteAt:` binds p60; `at:put:` accepts a Character or smi 0..255)
`size`(p62) `hash`(p64) `compare:`(p65) · `= [ ^(anObject isString) and:
[ (self compare: anObject) = 0 ] ]` — note Symbol *is* a String; `#foo =
'foo'` answers true under this pin (Symbol overrides `=` to identity;
asymmetry documented and tested) · `< <= > >=` defined directly via
`compare:` (String sits under ArrayedCollection, not Magnitude) ·
`, aString` (new String, two p63 replaces) ·
`copyFrom:to:` (p63) `asString [^self]` `asSymbol` (p80) ·
`asUppercase asLowercase reverse` (loops) · `includesChar: indexOf:` ·
`String class >> new: n` (basicNew: — bytes zeroed) `with:` (1-char) ·
`printOn:`(19): quote, double embedded quotes, quote (escaping rule) ·
`displayString` = contents without quotes (19). Mutation of literal
strings is legal in v1 (no read-only objects — documented).

#### 13 Symbol
`asSymbol [^self]` `isSymbol [^true]` `asString [ ^(String new: self size)
replaceFrom: 1 to: self size with: self startingAt: 1 ]` · `= [^self ==
anObject]` `hash [^self identityHash]` (interned ⇒ identity semantics;
faster than byte hash and consistent with `=`) · `at:put: [ self error:
'symbols are immutable' ]` · `printOn:`(19): `#` + name (quoted form
`#'…'` when the name isn't a plain identifier/keyword/binary shape).

#### 14 Association
Instvars (genesis): `key value`. `key value key: value: key:value:`
class-side `key:value:`, `= hash` over key, `printOn:`(19) `key->value`
— no `->` constructor on Object in v1 (binary `->` would shadow nothing;
**include it**: `Object>>-> v [^Association key: self value: v]`).

#### 15 OrderedCollection
Instvars (genesis): `array firstIndex lastIndex` (array: an Array;
elements live at firstIndex..lastIndex; empty ⇔ lastIndex < firstIndex;
initial: `array := Array new: 8. firstIndex := 1. lastIndex := 0`).
`initialize` pattern: class-side `new [ ^self basicNew init ]`.
`size isEmpty` · `add:`/`addLast:` (grow-at-end: if lastIndex = array
size, `makeRoomAtEnd` = new Array 2×, copy, firstIndex reset to 1) ·
`addFirst:` (grow-at-front analog) · `removeFirst removeLast` (nil out
slot, error if empty) · `at: at:put:` (index-check + offset) · `do:`
(`firstIndex to: lastIndex do:` over array) · `detect:ifNone: includes:
collect:` (→ new OC) `asArray addAll: first last` `printOn:`(19)
`OrderedCollection(…)`.

#### 16 Dictionary
Instvars (genesis): `tally keys vals` — two parallel Arrays, power-of-two
capacity (initial 8), nil key slot = empty, linear probing, **no
deletion in v1** (no tombstones needed). Load factor: grow when
`tally * 2 > capacity` (≤ 50%). nil is not a valid key (error).
The probe loop (pinned, verbatim shape):
```smalltalk
scanFor: key [
  | cap i probe |
  cap := keys size.
  i := (key hash bitAnd: cap - 1) + 1.
  [ true ] whileTrue: [
    probe := keys at: i.
    (probe isNil or: [ probe = key ]) ifTrue: [ ^i ].
    i := i = cap ifTrue: [ 1 ] ifFalse: [ i + 1 ] ]
]
```
(`whileTrue:` inlines; the loop is send-free except `hash`/`=`/`at:` —
fine.) `at:put:` (scan; new key → store both, `tally := tally + 1`, then
`self checkGrow`) · `at:` (`at:ifAbsent: [self error: 'key not found']`)
· `at:ifAbsent:` · `at:ifAbsentPut:` · `includesKey:` · `size [^tally]` ·
`keysAndValuesDo: [ 1 to: keys size do: [:i | (keys at: i) notNil
ifTrue: [ aBlock value: (keys at: i) value: (vals at: i) ]]]` ·
`keys values associationsDo: do:`(over values) · `checkGrow`/`grow`
(allocate 2× arrays, re-insert via scanFor: on the new arrays — factor as
`privateAt:put:` used by both paths) · `printOn:`(19). Keys must have
sane `hash`/`=`: smis, Symbols, Strings, Characters all do by this point.

#### 17 Interval
Instvars (genesis): `start stop step` (step fixed +1 usable; general smi
step allowed, non-zero). Class-side `from:to:` `from:to:by:`. `size [ step
> 0 ifTrue: [ ^(stop - start // step) + 1 max: 0 ] … ]` (floored `//`
makes this exact) · `at: do: collect:`(→Array) `includes: first last
isEmpty asArray asOrderedCollection printOn:`(19). `Number>>to:` /
`to:by:` land here (reopen of Number). `Interval do:` uses `whileTrue:`
directly (do NOT call `to:do:` — its inline form can't handle `step`).

#### 18 WriteStream
Instvars (genesis): `collection position` (position = elements written;
write slot = position+1). `WriteStream class >> on: aColl [ ^self basicNew
setOn: aColl ]` (position := 0; on: String and on: Array both work —
element writes go through `at:put:` so bytes/oops dispatch naturally).
`nextPut: e` — if `position = collection size` grow (new `collection
class new:` 2× max 8… `collection class` is Array/String; both respond to
`new:`; copy via loop or p63 for String), then `at:put:`, bump, answer e.
`nextPutAll: aColl [ aColl do: [:e | self nextPut: e]. ^aColl ]` ·
`contents [ ^collection copyFrom: 1 to: position ]` · `<< anObject`:
```smalltalk
<< anObject [
  anObject isString ifTrue: [ self nextPutAll: anObject. ^self ].
  anObject isCharacter ifTrue: [ self nextPut: anObject. ^self ].
  self nextPutAll: anObject printString. ^self
]
```
(defined here but only *fully* usable after 19 gives everything
printString; `<<` on Strings/Characters works immediately) · `cr tab
space nl` (nextPut: the Character constants).

#### 19 Printing (reopens across the world)
Object: `printString [ | s | s := WriteStream on: (String new: 16).
self printOn: s. ^s contents ]` · `printOn: s [ s nextPutAll: self class
name asString ]` (**no article** — deviation from tradition, pinned:
`3 printString` = `'3'`, `Object new printString` = `'Object'`) ·
`displayString [^self printString]` (String overrides) · `inspect`
(Transcript: class name, then each instVarName → `instVarAt:` printString,
indexables first 10 elements — "inspect-lite").
Then per-class `printOn:` as listed above. SmallInteger (pinned):
`self < 0 ifTrue: [ s nextPut: $-. ^self negated printOn: s ]`, then peel
positive digits by repeated `\\ 10` into reversed order — this needs no
minVal special case because `minVal negated` overflows into a Large,
whose printOn: (divMod: 10 loop, file 07) handles it. Also:
Double (p30), Character, String (escaping), Symbol, Array, ByteArray,
OrderedCollection, Dictionary, Association, Interval, Boolean/nil
(`'true' 'false' 'nil'`), Behavior/Metaclass. TranscriptStream gains
`<<` (same body as WriteStream's) and `show:` widens: non-String arg →
`self show: arg printString`.

#### 20 System
SystemDictionary: `startUp [ ^self ]` (hook; Character table etc. already
initialized by their files' doIts) · `quit: code` (p90) `quit [ ^self
quit: 0 ]` · `gcScavenge`(p91) `gcFull`(p92) `millisecondClock`(p93) —
these installed on SystemDictionary so scripts say `Smalltalk gcFull`.

### SUnit-lite (`world/tests/00_sunit.mst`)

**Constraint (SPEC §1.3): no exceptions in v1.** `error:` kills the
process; therefore assertion failures must be *recorded, not raised*, and
a genuinely erroneous test (DNU/error:) aborts the whole run — acceptable:
CI treats a dead run as failure, and the runner prints each test's name
**before** running it so the culprit is identified. `ensure:` (p85) is
used only to keep the result's `completed` count accurate.

```smalltalk
Object subclass: TestResult [        "genesis instvars: runCount failCount failures"
  init [ runCount := 0. failCount := 0. failures := OrderedCollection new ]
  ranOne [ runCount := runCount + 1 ]
  fail: aString [ failCount := failCount + 1. failures add: aString ]
  ...printOn:, isSuccess [^failCount = 0]
]
Object subclass: TestCase [          "genesis instvars: result current"
  result: r [ result := r ]
  assert: aBool [ aBool == true ifFalse:
      [ result fail: current , ': assertion failed' ] ]
  assert: a equals: b [ a = b ifFalse: [ result fail: current ,
      ': expected ' , b printString , ' got ' , a printString ] ]
  deny: aBool [ self assert: aBool == false ]
  runTest: name do: aBlock [
    current := name asString.
    Transcript show: '  ' , current. Transcript cr.
    [ aBlock value. result ranOne ] ensure: [ ] ]
]
```
**No `perform:` exists** (not in SPEC §10) — so test discovery is static:
each TestCase subclass implements `runAll` listing its tests explicitly:
```smalltalk
runAll [
  self runTest: #testAdd do: [ self testAdd ].
  self runTest: #testOverflow do: [ self testOverflow ] ]
```
(the `ensure:` in `runTest:do:` is a placeholder marker for future error
containment (S18); today it documents the boundary). Class-side driver:
`TestCase class >> runWith: aResult [ | c | c := self new. c result:
aResult. c init… c runAll ]`. `TestRunner` (class-side only, class var
`Total`): `run: aClass` prints the class name, calls `runWith:` on a
shared TestResult stored in `Total`; `report` prints
`N run, M failed` + each failure line, then `Smalltalk quit: (Total
isSuccess ifTrue: [0] ifFalse: [1])`. `tests/99_run_all.mst` is doIts:
`TestRunner start. TestRunner run: ObjectTests. … TestRunner report.`
`tests/it_world.rs` runs `macvm run` with the tests list appended and
asserts exit 0 + the `0 failed` line.

### Benchmarks (`world/bench/`)

fib.mst (note the reopen names Integer's TRUE superclass — Pitfall P2):
```smalltalk
Number subclass: Integer [
  fib [ self < 2 ifTrue: [ ^self ].
        ^(self - 1) fib + (self - 2) fib ]
]
Transcript show: 25 fib printString. Transcript cr.
```
sieve.mst: classic flags sieve — `flags := Array new: 8190` all true,
mark multiples, count primes, 10 iterations, print count (expected 1899
for 8190); brackets the run with `Smalltalk millisecondClock`. Exercises
Array at:/at:put:, whileTrue:/to:do:, comparison sends, allocation.

**S6 throughput measurement procedure** (recorded in `docs/PERF.md`):
1. Release build. `MACVM_TRACE=count` (extends CONVENTIONS §3's comma
   list; prints total bytecodes executed at exit — a plain counter in the
   dispatch loop, compiled in always; cost ≈ 1 add/dispatch, acceptable).
2. Run `fib.mst` with 25 **and** 30; `sieve.mst` at 10 iterations; each
   5 times; take median wall time (`millisecondClock` bracketing inside
   the .mst, printed) and median bytecode count (deterministic — assert
   all 5 equal).
3. Report: bytecodes/s per benchmark; check against SPEC §13 row 1
   (≥ 50M bc/s, fib(30) < 2 s). Record pass/fail — **tracking, not
   gating** (SPRINTS standing rule 3).

### Layer boundaries

- Everything in this sprint is `world/*.mst` + genesis skeleton additions
  in `src/memory/` + (if S3 lacked them) primitives 30/45-on-Klass/96 in
  `src/runtime/`. No interpreter or frontend changes.
- `.mst` files may rely only on: earlier files, genesis, and the pinned
  primitive id map.

## Implementation order

1. Genesis skeleton (hierarchy above) + instvar layouts for Character,
   Association, OrderedCollection, Dictionary, Interval, WriteStream,
   TestResult, TestCase, Message; primitives 30/96 + Klass-format
   `instVarAt:`; `MACVM_TRACE=count`.
2. Files 01–04 (Object core, nil/booleans, Behavior, Transcript) —
   smoke: `macvm repl` prints via Transcript.
3. 05–08 numbers: Magnitude, SmallInteger, **LargeInteger** (test
   overflow immediately: `SmallInteger maxVal + 1`), Double.
4. 09–14: Character, collections' shared protocol, Array/ByteArray/
   String/Symbol, Association.
5. 15–18: OrderedCollection, Dictionary, Interval, WriteStream.
6. 19 printing + 20 system; `point_demo.mst` now prints via printString.
7. SUnit-lite + test classes (tests_s06.md inventory); wire
   `tests/it_world.rs`.
8. Benchmarks + PERF.md baseline.

## Pitfalls

- **P1 Bootstrap ordering**: printString needs WriteStream needs String
  needs Character — hence Transcript (04) speaks primitive-only and
  `printOn:` lands last (19). Any file that "just adds a printString
  helper" early will fail name resolution — that's the design working.
- **P2 Reopen superclass lines**: every reopen must name the TRUE genesis
  superclass (`Number subclass: Integer`, `ArrayedCollection subclass:
  String`…). Copy-pasting `Object subclass:` fails with "cannot change
  shape".
- **P3 Large digit peeling for negatives**: floored `//`/`\\` on negative
  smis do NOT yield magnitude digits; use the pinned two's-complement
  peel (07) and test `SmallInteger minVal` explicitly — it is the one
  value whose `negated`/`abs` cannot stay a smi.
- **P4 normalize-to-smi**: every Large op must demote results in smi
  range, or `=`/`hash`/ICs see two representations of one value. The
  reduce guard must not itself overflow (pre-shift bound check, 07).
- **P5 Symbol vs String equality asymmetry**: `'foo' = #foo` true,
  `#foo = 'foo'` false (identity). Pinned + tested; revisit with ANSI.
- **P6 Inlined vs real control flow**: True/False/Number must implement
  `ifTrue:`/`and:`/`to:do:` etc. with semantics IDENTICAL to S5's inlined
  patterns (nil for missing branch, receiver as `to:do:` result). The
  test suite runs each both ways (store the block in a temp to defeat
  inlining).
- **P7 Dictionary hash quality**: smi `hash` = self ⇒ sequential keys
  cluster with linear probing; harmless at ≤50% load. Do NOT "fix" with
  multiplicative hashing in v1 — keep goldens stable.
- **P8 `Transcript cr` newline literal**: the string literal spanning a
  physical newline (04) is fragile under editors that strip trailing
  whitespace — keep it alone on its lines; 19 replaces it with
  `Character nl` anyway.
- **P9 No perform:, no exceptions**: SUnit discovery is the explicit
  `runAll` list; forgetting to list a test silently skips it — the
  runner prints per-class test counts so tests_s06's inventory numbers
  are checkable against the report output.
- **P10 collect: species**: implemented per concrete class (no `species`
  protocol). ByteArray collect: answers an Array (pinned).
- **P11 `String at:` is byte-indexed** (SPEC §1.3): multibyte UTF-8 in
  string literals will index per byte; tests use ASCII only.

## Interfaces for later sprints

- S7/S8 (GC): the whole in-language suite is the stress payload
  (`MACVM_GC_STRESS=1` gate); `Smalltalk gcScavenge/gcFull` are the
  manual triggers tests use.
- S10+ (tier 1): fib/sieve are the §13 speedup benchmarks; `to:do:`'s
  real `#<=`/`#+` IC sites are the type-feedback guinea pigs.
- SUnit-lite grows `should:raise:` in S18 (exceptions) — keep
  TestCase's assert/deny API stable.
- `docs/PERF.md` table schema started here is appended by S10/S14/S15.

## Out of scope

- Large÷Large division, `printString: radix` for Large, Fraction —
  no target sprint (post-v1 library work).
- `perform:`, `respondsTo:`, thisContext-based reflection (needs new
  primitives; earliest S16+).
- removeKey:/tombstones on Dictionary; WeakDictionary (S22).
- ANSI exception classes, `should:raise:` (S18).
- ReadStream, collection sorting, `species`, copy protocols beyond
  `copyFrom:to:` (post-v1 as needed).
- Unicode-correct String operations (SPEC §1.3 documented limitation).

## Addendum — GUI-track obligation (SPEC §16.2, amendment A17)

`Transcript`'s primitive output path (`printOnStdout:`) must route through a
`TranscriptSink` held in `VmState` (default sink = stdout — behavior
identical to this sprint's spec). The GUI (Phase G2) installs a channel sink
via `VmHandle::set_transcript`; nothing else in the world files changes.
Design the primitive with the sink indirection now so G2 is a no-touch swap.
