# MACVM: A High-Level Overview

## What this is

MACVM is a virtual machine — a program that runs other programs — built to
run Smalltalk, one of the oldest and most influential programming languages,
on a modern Apple Silicon Mac. This document explains how it actually works,
from the moment you type Smalltalk source code to the moment your computer's
processor is running your program at full speed. It's written for someone
who wants to understand the system, not to write one — no prior knowledge
of virtual machines or compilers is assumed.

The short version: MACVM reads your Smalltalk code, turns it into a simple
internal form it can run right away, runs it in a straightforward
step-by-step way at first, and then — automatically, invisibly, without you
doing anything — rewrites the parts of your program that run a lot into
real machine code the processor can execute directly, so your program keeps
getting faster the more it runs. This document is about how all of that
actually happens.

## The Smalltalk way of thinking

Before getting into the machine, it helps to know what kind of language
MACVM is running. Smalltalk has one big idea, applied with total
consistency: **everything is an object, and the only thing objects do is
send each other messages.**

A number is an object. `3 + 4` isn't the processor doing addition directly —
it's the object `3` being sent the message `+` with the argument `4`. A
list is an object. Adding something to it is a message send. Even things
that look like control flow in other languages — an `if`, a loop — are
really just message sends to objects that happen to be `true`, `false`, or
a block of code.

Objects are grouped into **classes**, which describe what kind of object
something is and what messages it knows how to respond to (its
**methods**). When an object receives a message, the system has to figure
out — by walking up from the object's own class through its ancestors —
which method actually answers that message. This "figure out which method
to run" step, called **method lookup**, happens constantly, for almost
every single thing a Smalltalk program does. Making that step fast, without
giving up the flexibility that makes Smalltalk what it is, is most of what
the rest of this document is about.

## The big picture

MACVM runs your program in two different ways, switching between them
automatically:

1. A simple, always-correct way of running code, called the
   **interpreter**. It's not especially fast, but it's simple, and it can
   run absolutely anything you throw at it, including code that's changing
   or being redefined while it runs (Smalltalk allows this).
2. A fast way of running code, where the parts of your program that run
   over and over get turned into genuine machine instructions your Mac's
   processor runs directly, with no VM step-by-step interpretation in the
   way at all. This is called **compiled code**, and producing it is the
   job of MACVM's **optimizing compiler**.

Every method starts out running the first way. MACVM watches how often each
method actually gets called, and once one is called enough times, it gets
compiled into the second, faster form — automatically, in the background,
with no visible pause or change in behavior from the program's point of
view. From then on, calling that method takes the fast path instead.

This is the same overall strategy used by the JavaScript engines in every
web browser and by Java's HotSpot virtual machine: start simple and
correct, then specialize the hot parts once you've actually observed them
being hot. The rest of this document walks through each stage of that
journey in order.

## Stage one: turning source text into something runnable

You write Smalltalk as ordinary text — method definitions, class
definitions, expressions. The first thing that has to happen to any of that
is **compiling** it: turning readable source text into a compact internal
form the virtual machine actually knows how to run.

This happens in two steps:

- **Parsing.** The compiler reads your source text and figures out its
  actual structure — which parts are message sends, which are variable
  references, where each expression begins and ends. Smalltalk's grammar is
  small and regular (it's one of the simplest real programming languages to
  parse), so this step is fast and simple.
- **Code generation.** Once the compiler understands the structure of what
  you wrote, it walks through it and emits **bytecode**: a sequence of
  small, simple instructions in MACVM's own internal instruction set — not
  machine instructions your processor understands yet, just a compact
  recipe the virtual machine's interpreter knows how to follow one step at
  a time. Things like "push the receiver," "push the literal constant in
  slot 3," "send this message," "jump back to the top of this loop" are all
  individual bytecode instructions. (The full list of these instructions,
  and exactly what each one does, is documented separately in
  `docs/ISA.md` — this overview only needs the idea that they exist.)

The result of compiling a method is a small package: the bytecode itself,
a table of constants the bytecode refers to (numbers, strings, other
methods), and a little scratch space (described in the next section) for
remembering what's been learned about how this method actually gets called
at runtime. This package is what actually gets stored as "the method" from
this point on — the original source text is kept around too (so you can
still read and edit it), but it's the bytecode that runs.

## Stage two: running the bytecode — the interpreter

The **interpreter** is the part of MACVM that runs bytecode directly. It
works the way you'd naturally imagine a simple bytecode machine to work: it
keeps a stack of values (think of it as a stack of plates — you can only
add to or remove from the top), and it walks through the bytecode one
instruction at a time, in a loop, doing exactly what each instruction says:
push this value, pop these two values and send a message with them, jump
back to an earlier point, and so on. Every active method call gets its own
region of this stack holding its arguments, its temporary variables, and
whatever values it's currently working with.

The single most important — and most frequent — thing the interpreter does
is **send a message**. Every `+`, every method call, every loop condition
check is a message send under the hood. And every single message send
requires figuring out which method actually answers it, by looking at the
receiver's class and walking up its ancestry until a matching method is
found.

Doing that full search on every single send would be far too slow — most
sends, in practice, go to the exact same method every single time they're
reached, because the same piece of code usually gets called with the same
kind of object over and over. So MACVM remembers. Each place in the
bytecode where a message gets sent has a small memory of what it found last
time — this is called an **inline cache**. The next time that exact send
runs, the interpreter first checks: "is the receiver the same kind of thing
as last time?" If so, it reuses the answer immediately, skipping the
search entirely. If the receiver turns out to be a different kind of thing
than before, the cache remembers a short list of the different kinds it's
seen (this is where the terms "monomorphic," "one shape seen," and
"polymorphic," "a few shapes seen," come from) before eventually giving up
on remembering and just doing the full search every time for message sends
that are simply too varied to cache usefully.

This turns out to matter for more than just speed. Those same inline
caches are also where MACVM learns what your program actually does at
runtime — which becomes the raw material the next stage uses to generate
genuinely fast code.

Two other things worth knowing about the interpreter:

- It counts. Every time a method is called, and every time a loop goes
  around again, a counter goes up. Those counters are what eventually
  trigger the next stage.
- It's precise about memory. Every value on its stack is either a real
  reference to an object or a plain number — nothing is ambiguous — which
  matters enormously for how garbage collection works, covered later in
  this document.

## Stage three: getting faster automatically — the optimizing compiler

Once a method's call counter (or a loop's iteration counter) crosses a
threshold, MACVM decides that method is worth making fast, and hands it to
the **optimizing compiler**. This is the part of the system that turns
bytecode into genuine ARM64 machine code — the same kind of instructions
any natively-compiled program's processor executes — which then gets
stored in a region of memory called the **code cache** and used directly
for every future call to that method, completely bypassing the
interpreter's step-by-step loop.

This compilation happens automatically and, from the Smalltalk program's
point of view, invisibly — you don't ask for it, you don't control it, you
just notice (if you're watching closely) that code which runs a lot ends up
running much faster than code that doesn't. This is what "adaptive" means
in an adaptive virtual machine: the system adapts its own strategy, per
method, based on what it's actually observed that method doing.

A few things make the compiled code fast, beyond simply "not being
interpreted one bytecode at a time":

- **It trusts what it's already seen.** Remember those inline caches from
  the interpreter? The compiler reads them as **type feedback** — a
  reliable record of what kinds of objects have actually shown up at each
  send site so far. If a send has only ever seen one kind of receiver, the
  compiler generates code that assumes that will keep being true, checks it
  cheaply, and skips the general message-lookup machinery entirely on the
  common path.
- **It inlines.** If a message send almost always goes to one specific,
  small method, the compiler can paste that method's own logic directly
  into the caller, rather than generating a real call to it at all. This is
  especially powerful for Smalltalk's control-flow idioms — things like
  `ifTrue:ifFalse:`, `whileTrue:`, and collection-iteration methods like
  `do:` — which are ordinary message sends to ordinary blocks of code under
  the hood, but after inlining collapse down into the same tight loops and
  branches a hand-written low-level program would have used from the
  start.
- **It specializes per receiver.** Rather than compiling one generic
  version of a method that has to handle every possible kind of receiver,
  MACVM can compile separate, specialized versions for different receiver
  classes, each one free to assume its own specific class throughout.

None of this is guesswork treated as certain fact, though — everything the
compiler assumes is checked, cheaply, at the point where the assumption
matters, and if a check ever fails, the program doesn't get a wrong answer.
It falls back — which is exactly the subject of the next section.

## Stage four: when fast assumptions stop holding — deoptimization

Compiled code is built on optimistic bets about what's been true so far:
"this variable has always held a small whole number," "this particular
send has only ever gone to one method," "this method has never been
redefined." Smalltalk, though, is a deeply dynamic language — you're
allowed to redefine a method while the system is running, change a class's
shape, or simply pass a value of a kind that's never shown up at some
call site before. When one of these bets stops paying off, the compiled
code that depended on it can no longer be trusted.

MACVM's answer to this is **deoptimization**: a graceful, precise way to
abandon a piece of compiled code — potentially in the middle of a currently
running method call — and drop back to the always-correct interpreter,
picking up exactly where execution would have been if the fast path had
never been taken at all. This happens without losing any state: local
variables, the operand stack, everything gets faithfully reconstructed into
an ordinary interpreter frame, and execution simply continues from there,
just more slowly than the compiled path would have been.

Deoptimization happens for two different kinds of reasons:

- **An assumption is violated while running.** For example, compiled code
  optimistically assumed two small numbers being added would never overflow
  the range a machine word can hold directly, and this one time, they did.
  Compiled code has a cheap trap for exactly this case: it notices the
  overflow, bails out immediately, and lets the interpreter redo that one
  operation the general, always-correct way (which knows how to grow into
  an arbitrarily large number, something the fast path deliberately didn't
  bother handling itself).
- **Something changed the program itself.** If a method gets redefined, or
  a class hierarchy changes underneath already-compiled code that assumed
  the old shape, MACVM finds every compiled method whose assumptions are
  now stale, marks it as no longer valid, and makes sure any calls to it —
  including calls that are already in progress on the stack right now, not
  just future ones — get redirected back to correct, up-to-date code.

This is what actually lets MACVM be both aggressive about optimizing and
fully faithful to Smalltalk's live, dynamic nature at the same time. The
compiler is free to make bold, speculative bets, because there's always a
safe, correct, and precise way back if a bet doesn't pan out — rather than
either refusing to optimize anything that could possibly change (slow), or
optimizing anyway and risking a wrong answer (broken).

## Where objects live: memory management

Every Smalltalk object — every number too large to fit directly in a
pointer, every string, every collection, every custom object your program
creates — needs somewhere to live in memory, and needs that memory reclaimed
once nothing refers to it anymore. This is the job of the **garbage
collector**, and MACVM's is a **generational, moving** collector, which is
worth unpacking a little.

**Generational** means it's built around one very reliable observation:
most objects die young. A huge fraction of everything a program allocates
is temporary — intermediate results, short-lived helper objects — used
briefly and then never referenced again almost immediately. So MACVM keeps
new objects in a small, dedicated area of memory (nicknamed **eden**, after
the same idea in other well-known collectors) and cleans just that small
area out very frequently, very cheaply. Whatever survives multiple cleanups
in a row — evidently a longer-lived object — gets promoted into a larger,
separate area meant for things expected to stick around, which gets cleaned
out much less often, using a more thorough process that also compacts
memory back into a tidy, contiguous shape.

**Moving** means that when the collector cleans up, it doesn't just cross
out dead objects in place — it actually relocates surviving objects to a
new spot in memory, so that live memory stays compact and there's no
long-term fragmentation. This is more efficient than the alternative, but
it means every single reference to a moved object, anywhere in the entire
system, has to be found and corrected to point at its new location. That
includes references sitting in ordinary object fields, but also references
currently held in the interpreter's stack, and — trickiest of all —
references sitting in registers or spill locations inside currently
executing compiled machine code. MACVM's compiler solves this by recording,
for every point where a collection could possibly happen, an exact map of
where every live object reference is at that moment (in a register, in a
stack slot, wherever) — so the collector always knows precisely what to
update, even inside optimized machine code, with nothing left to guesswork.

The practical result of all this is that allocation is extremely cheap most
of the time (handing out a new object is normally just "bump a pointer
forward"), long-lived data structures don't slowly fragment memory over
time, and none of this requires the Smalltalk programmer to think about
memory management at all — it happens continuously, automatically, safely,
underneath everything described in the earlier sections.

## The live system

One more thing worth knowing, because it's central to why Smalltalk (and
therefore MACVM) is built the way it is: a Smalltalk system isn't really a
program you compile once, run, and throw away. It's a **live, running
environment** — sometimes called an *image* — that holds all of your
classes, all of your methods, and all of your live objects at once, and
that you interact with, inspect, and modify *while it keeps running*.

You can open a running instance of any class and look at its actual current
values. You can browse a class's methods, edit one, and have that new
version take effect immediately, in the running system, the very next time
it's sent — no restart, no separate build step. This is the whole reason
the earlier sections spend so much attention on staying correct even when
methods get redefined out from under already-compiled code: in a system
built to be edited live, "the code might change while it's running" isn't
an edge case to tolerate, it's an everyday, expected way of working.

## Putting it all together

Here's what actually happens, start to finish, when you write and use a
new piece of Smalltalk code:

1. You write a method as ordinary Smalltalk source text.
2. The compiler parses it and generates bytecode — the compact internal
   recipe the VM knows how to run.
3. The first several times your method gets called, the interpreter runs
   that bytecode directly, one instruction at a time, using inline caches
   to keep repeated message sends cheap and, along the way, quietly
   recording what kinds of objects are actually showing up.
4. Once the method (or a loop inside it) has run often enough, the
   optimizing compiler takes over: it reads what the interpreter observed,
   generates real, specialized ARM64 machine code — inlining small,
   frequently-called methods directly in, trusting the type patterns it's
   already seen — and installs it in the code cache.
5. From then on, calling that method runs the compiled version directly,
   at full native speed, with the interpreter and all its step-by-step
   overhead completely out of the way.
6. If you (or anything else in the running system) ever does something
   that breaks one of the compiled code's assumptions — redefines the
   method, hands it a kind of object it's never seen before, overflows an
   arithmetic operation it assumed would stay small — MACVM deoptimizes
   gracefully: it reconstructs an ordinary interpreter frame with all the
   right values in it and carries on exactly as if the fast path had never
   been taken, with nothing lost and nothing gone wrong.
7. All the while, objects are being allocated cheaply, cleaned up
   automatically in the background, and kept track of precisely enough
   that none of the speed described above ever comes at the cost of a
   dangling or corrupted reference — even while objects are actively
   moving around in memory underneath a running, compiled program.

None of this is visible to you as a Smalltalk programmer. You just write
normal Smalltalk, in a live system you can inspect and change while it
runs, and the system underneath makes sure that whatever you write ends up
running as fast as it reasonably can — automatically, safely, and without
ever asking you to think about any of the machinery described in this
document.
