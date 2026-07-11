# The `smappl` tag

Grounding document for Strongtalk's `<smappl visual="...">` HTML extension:
what it is in the real Strongtalk source, exactly how MACVM's GUI shell
handles it today (G0), and what a real implementation needs to support
(G2+, per [`PLAN.md`](PLAN.md) D-G4/D-G5 and [`../docs/APPS.md`](../docs/APPS.md)
§5–6). All citations are to `../../strongtalk-repo/StrongtalkSource/`,
checked directly against that source, same convention as `PLAN.md`.

This document doesn't change any decision already on record in `PLAN.md` or
`APPS.md` — it supplies the exact source citations and the full vocabulary
catalog neither of those docs spells out yet, and slots in underneath them.

**Related but distinct: the Canvas widget** (`../docs/CANVAS.md`). Both are
ultimately "Smalltalk controls something embedded in the page," but the
shapes are opposite. `smappl` evaluates `visual=` **once**, at render
time, embedding a static result — cached and reused across relayouts (§2
below), but never re-evaluated on its own after that. Canvas is a
long-lived widget that keeps receiving **new** drawing commands over
time — an animation, a live diagram, incremental redraws — pushed over
the same batched VM↔GUI channel `smappl`'s own G2+ evaluator will
eventually use (§6). Treat this document's vocabulary (§3) as the
"one-shot widget" side of Smalltalk-embedded content; Canvas is the
"continuous/animated" side — not a `smappl` variant, and not reachable
through any of §3's six shapes.

## 1. What "smappl" means

No `.dlt` comment anywhere expands the name — it's assumed knowledge in the
original codebase. The strongest evidence reads it as **S(malltalk)
APPL(ication)**:

- The class implementing the tag is named **`ElementSMAPPL`**
  (`ElementSMAPPL.dlt`), following the same `Element<TAGNAME>` convention as
  every other HTML element class (`ElementA`, `ElementIMG`, `ElementH1`, …)
  — so `SMAPPL` simply *is* the tag name, uppercased, in Strongtalk's own
  HTML grammar.
- It's registered as a production named `#smappl` in
  `HTMLParser.dlt:113` (`HTMLParser class>>initialize`), alongside `#br` and
  `#hr` (lines 110–115) — ordinary tag-name productions, nothing special.
- Strongtalk has a real `Application` class (`Application.dlt`) — the
  superclass of tools/widgets that answer a `Visual` via `imbeddedVisual` —
  so "embed a Smalltalk **application**" is a literal, not just cute, read
  of the name. ("Smalltalk **applet**" is a plausible cousin reading given
  the tag's 1996 vintage, contemporaneous with Java applets, but has no
  direct class-name support the way "application" does.)

## 2. Exact parsing and semantics

**It is a void element — no closing tag, ever, by design, not by
omission.** `HTMLParser.dlt:113` registers it via `singletonNamed:`, the
*exact same* mechanism used for `#img`, `#br`, `#hr`. The instance-variable
comment on `HTMLProduction.dlt` spells out what that means: *"if this is an
element, then if it is a singleton, there is no end tag, just a start
tag."* The parser honors this at `HTMLParser.dlt:410–411`
(`parseElementWithStartTag:`):

```smalltalk
prod isSingleton ifTrue: [ ^HTMLElement fromStartTag: startTag parts: parts ]
```

— it returns immediately, without ever scanning for a closing tag. Grepping
the whole `strongtalk-repo` tree (not just the HTML corpus) for `</smappl`
or `/smappl>` turns up **zero matches anywhere**. MACVM's
`rewrite_smappl_placeholders` (`src/preprocess.rs`) already assumes exactly
this (no closing tag, tag ends at its own `>`), and that assumption is now
backed by a hard citation rather than just corpus observation.

**It is inline content, not a block.** `HTMLParser.dlt:56–62`'s
`#charMarkup` production groups `smappl` with `b i tt img` and the emphasis
tags (`dfn em cite code kbd samp strong var`) — i.e. character-level/phrase
markup. `ElementSMAPPL.dlt`'s `buildFor:` calls `builder addVisual:for:`,
and `HTMLBuilder.dlt:402–415`'s `addVisual:for:` is built on the same
primitive `add:` used for ordinary characters during line flow
(`HTMLBuilder.dlt`'s `addChar:`) — so a smappl-embedded Visual participates
in line breaking exactly like an inline image would.

**The `visual=` attribute is a Smalltalk expression, evaluated and coerced
to `Visual`.** The entire implementation, `ElementSMAPPL.dlt:16–29`:

```smalltalk
buildFor: builder <HTMLBuilder>
    builder
        addVisual:
            [ (Visual coerce:
                    (Smalltalk evaluate: (self attributeAt: #VISUAL)
                        ifError: [ :err <Str> :spot <Int> |
                            Transcript show: 'Error in HTML doit: ''',
                                ((self attributeAt: #VISUAL) stringCopyReplaceFrom: spot to: spot with: '<<',err,'>>'), ''''; cr.
                            ^self ]
                    )
                )
            ]
            for: self!
```

- `Smalltalk evaluate:ifError:` is the same doit-evaluation entry point
  `<a doit="...">` uses (`docs/APPS.md` §4).
- `Visual coerce:` is not smappl-specific — it's the generic dynamic-cast
  idiom (`Object.dlt`'s `Object class>>coerce:`/`coerce:else:`), used
  throughout the codebase to assert an arbitrary object down to a
  statically-declared type. So `visual=` must evaluate to something that
  **is-a `Visual`** — not a bare `Glyph` (Visual's shareable, context-free
  cousin per `Glyph.dlt`; nothing in the corpus wraps a bare Glyph for
  smappl use).
- On evaluation error, it prints the bad source into the Transcript with
  the failure column spliced in as `<<err>>`, then aborts (`^self`) —
  **errors are swallowed into the transcript, not surfaced as broken page
  content.** Worth carrying over: MACVM's real evaluator should behave the
  same way (transcript error, not a page-breaking exception).

**Embedded Visuals are cached per HTML element across relayouts**, not
rebuilt from scratch every time. `HTMLBuilder.dlt:402–415`:

```smalltalk
addVisual: visualBlock <[^Visual]> for: element <HTMLElement>
    "This should be used rather than add: to insert imbedded visuals
        that have state, or that might change their preferences, so
        that they can be associated with their element and reused
        across relayouts"
    self add:
        (self visualRegistry
            at: element
            ifPresent: [ :e <Visual> | e noParent ]
            ifAbsentPut: [ visualBlock value ]
        ).
    self lineHasNothingSubstantial: false.!
```

`HTMLVisual.dlt`'s comment on the registry: *"A map of the imbedded visuals
with state, which will be reused if possible if the same page is layed out
multiple times."* Keyed by the HTML element node itself — on relayout, a
previously-built Visual for the same element is detached (`noParent`) and
reattached rather than re-evaluated, which is what lets a button keep its
pressed state, or an outliner keep its expand/collapse state, across
reflows. `docs/APPS.md` §5.1 has already independently landed on the same
idea for MACVM's `ToolNode` (a node-id ↔ DOM-id registry, explicitly
compared there to this exact mechanism) — no conflict, just the source
citation that comparison was missing.

## 3. The real vocabulary — what a `visual=` evaluator must support

Every `smappl visual="..."` in `strongtalk-repo`'s own HTML corpus (and,
byte-identically, in MACVM's copied `reference/pages/` — D-G4 keeps these
untouched) falls into one of six shapes:

1. **Icon button** — `Button withImage: (Image fromFile: (FilePath for:
   '...bmp')) action: [ :b | ... ]`. By far the most common — every
   Launcher toolbar button in `progenv.html`, plus `startPage.html`'s home
   and edit buttons.
2. **Labeled button** — `Button labeled: '...' action: [ :b | ... ]`, e.g.
   `differences2.html`: `Button labeled: 'Press Me!' action: [ :b |
   b promptOk: '...' title: '...' type: #info action: [] ]`.
3. **Filtered class-hierarchy outliner** —
   `(ClassHierarchyOutliner for: (ClassMirror on: X)) filterOn...`, then
   `(h topVisualWithHRule: false) withBorder: (Border standard3DRaised:
   true)`. `progenv2.html` filters `filterOnCommentsContaining: '%HTML'`;
   `ui.html` filters `filterOnUIUserClasses`. The shorthand seen in
   `startPage.html`, `ClassHierarchyOutliner imbeddedVisualForClass: X`, is
   the same tool via a convenience constructor.
4. **Single-class outliner** — `ClassOutliner for: (ClassMirror on: X)` →
   `topVisualWithHRule: false` → `withBorder: (Border standard3DRaised:
   true)` — a one-class browser embed (`progenv3.html` on `HTMLView`,
   `virtualmachine2.html` on `Test`, the latter additionally calling
   `openSide: true selector: #simpleTest:` to pre-open one method).
5. **Embedded code view** — `((CodeView forString) doneBlock: [...];
   model: '...source text...') imbeddedVisual with3DBorder withBorder:
   (Border standard3DRaised: true)` — read-only/editable source viewers
   (`progenv4.html`, `progenv6.html`).
6. **Side-effecting non-widget** — `progenv.html`'s "dummy smappl" forks a
   background comment-preload sweep and returns `Glue xRigid: 0` (an
   invisible zero-width spacer). `visual=` only has to evaluate to *some*
   `Visual` — Strongtalk exploits that to run background work as a page
   loads, not just to build widgets.

`doit=` vocabulary alongside these: `TextEditor new launch`, `Mandelbrot new
launch`, `SystemMonitor new launch`, `VM collectGarbage`,
`(TextEditor on: (FilePath for: '.strongtalkrc')) launch`,
`(CodeEditor on: (FilePath for: '.strongtalkrc')) launch`,
`Test benchmark: [ Test simpleTest: 10000000 ]`.

**Implementation scope this implies** — a real `visual=` evaluator needs, at
minimum: `Button withImage:action:` / `Button labeled:action:`;
`ClassHierarchyOutliner imbeddedVisualForClass:` and its longer filtered
form (`for:`, `filterOnCommentsContaining:`, `filterOnUIUserClasses`,
`topVisualWithHRule:`, `withBorder:`); `ClassOutliner for:` (+
`openSide:selector:`); `CodeView forString` with `doneBlock:`/`model:` plus
`imbeddedVisual`/`with3DBorder`/`withBorder:`; `Border standard3DRaised:`;
`Glue xRigid:`. This matches `PLAN.md` §5's existing risk note ("Button
withImage:action:, Glue, outliner embeds, CodeView") almost exactly — good
corroboration that scope was already right, but that note should be
expanded to name `Button labeled:action:` and the `ClassOutliner`/filter-chain
shapes explicitly, since they're distinct call shapes, not covered by
"Button withImage:action:" alone.

## 4. From Visual tree to HTML fragment (D-G5)

Quick map of the Visual-side classes `PLAN.md` §1 already cites, with what
each one is *for*:

- **`Glyph`** — the base abstraction: a possibly-*shareable* graphical
  object (e.g. a character glyph reused at many positions).
- **`Visual`** (subclass of `Region`) — the unshareable specialization:
  exactly one `parent`, one `position`, one `allocation`; tagged
  `UIEventHandler; Glyph; RelayoutTarget`. This is what `visual=` must
  evaluate to.
- **`Glue`** (subclass of `Visual`) — invisible, geometry-preferences only;
  used for spacing/filling. Direct analog of a CSS flex spacer or margin.
- **`Row` / `Column`** (via `RowOrColumn`, a `ComplexCompositeVisual[SUB]`)
  — line up child Visuals in a row or column. Direct analog of
  `flex-direction: row` / `column`; children are an ordered, `id`-indexed
  sequence.
- **`Border`** — not a Visual itself; a decorator descriptor
  (`naturalBlock`/`minBlock`/`maxBlock`/`layoutBlock`/`displayBlock`) that
  `Frame` (subclass of `VisualWrapper`) uses to add edge decoration and
  backdrop paint. `Border standard3DRaised:` is exactly the bevel `PLAN.md`
  §2.3's CSS recipe already reproduces pixel-for-pixel.

This shape maps onto HTML/CSS almost directly: `Row`/`Column` → flex
containers; `Glue` → spacer `<div>`s or `margin`/`flex-grow`; `Frame` +
`Border` → a wrapping `<div>` using `PLAN.md` §2.3's bevel `box-shadow`
recipe; leaf Visuals (a button, a `CodeView`, an outliner header row) → each
its own fragment.

### 4.1 How "thin" are the leaf tool classes, really?

Traced the `CodeView` case in full, since §3's vocabulary treats
`CodeView forString; doneBlock:; model:` as one opaque unit. It isn't —
**every method in that expression is inherited, not defined on `CodeView`
itself:**

| Call | Actually defined on | Citation |
|---|---|---|
| `forString` | `TextView` (generic factory, not CodeView's own) | `TextView.dlt:107-124` |
| `doneBlock:` | `View` (plain setter) | `View.dlt:43-45` |
| `model:` | `View` (store) + `TextView` override (rebuild glyphs) | `View.dlt:51-54`, `TextView.dlt:595-600` |
| `imbeddedVisual` | `Application` (`buildVisualTop: false`, no window backdrop) | `Application.dlt:70-77` |
| `with3DBorder`/`withBorder:` | `Visual` (wraps receiver in a `Frame`) | `Visual.dlt:1670-1681` |

`CodeView.dlt` itself is 393 lines and contributes **none of this** —
its own content (do-it/show-it/inspect-it evaluation, a handful of
Ctrl-key command bindings, source-index↔glyph-index translation) only
matters once a user is actively typing inside the widget, not at
construction time. The generic chain it rides on — `TextView` (1518
lines: cursor movement, selection, undo/redo, scrolling, keyboard
handling, model⇄glyph conversion), `View` (190 lines: accept/cancel/
model/doneBlock protocol), `Application` (337 lines: visual-lifecycle
skeleton) — is doing essentially all of the actual work, and is shared
by every text-editing tool in the system, not written for this one.

**A real discrepancy worth flagging:** CodeView's own class comment says
*"One may create a CodeView using `CodeView forText`"* (`CodeView.dlt:16`,
with `forText` defined at `CodeView.dlt:32`, building a `CharGlyphs`-typed
view) — but the actual corpus HTML (`progenv4.html`, `progenv6.html`) uses
`CodeView forString` instead, which only resolves because it's inherited
from `TextView`'s *generic* factory (`TextView.dlt:107-124`, building a
plain `TextView[Str]`-shaped instance with its own separate
glyphBuilder/modelBuilder pair). The corpus doesn't use CodeView's own
documented constructor. Worth keeping in mind for G2: a page can legally
invoke a superclass's generic factory on a subclass receiver and get a
subtly different configuration than the subclass's own "intended" one —
MACVM's evaluator needs to resolve method lookup the same way (walk to
whichever class actually defines the selector), not assume a tool class's
own doc comment describes everything reachable through it.

**The outliner family shows the same pattern isn't universal.**
`ClassOutliner` (57 lines, over `AbstractClassOutliner` →
`DefWithMsgOutliner` → … → `Outliner`, collectively 1000+ lines) is even
thinner than CodeView relative to its framework — its entire content is
one typecheck command, one menu item, and three tiny private overrides.
But `ClassHierarchyOutliner` (896 lines, over the same `MirrorOutliner`
base) is *not* thin — it carries substantial original logic (tree layout,
incremental search/filter state, bold-class painting). So "leaf tool class
= thin wrapper" is the common case, not a guarantee — some tools
(hierarchy browsers with real per-tool state) are large on their own
terms.

**Implication for MACVM's G2 evaluator:** this is good news for scope, not
bad. It means the Rust/VM side doesn't need one bespoke implementation per
smappl-buildable widget — it needs a small number of generic, reusable
substrates (a `TextView`-equivalent, an `Outliner`-equivalent) that the
actual tool classes mostly just *configure* rather than reimplement.
`HtmlWriter` (`docs/APPS.md` §6) should be designed around that shape:
one HTML-rendering path per generic substrate, not one per named tool.

**Did Strongtalk ever render a Visual to HTML itself? No.** Grepping the
entire `strongtalk-repo` tree for `asHTML`/`toHTML`/`renderHTML`/
`htmlOn:`/`visualToHtml`-shaped method names turns up only
`DeltaPrim.dlt`/`DeltaPrimitiveGenerator.dlt`'s `fileOutHTMLOn:`/
`fileOutDocHTMLOn:` — which generate **static VM-primitive reference
documentation pages**, unrelated to the live UI Visual hierarchy.
Strongtalk's HTML pages only ever flow *into* the Visual world
(`HTMLBuilder`/`HTMLVisual` turn parsed HTML into a `Row`/`Column`/glyph
tree for on-screen rendering) — never the reverse.

**`PLAN.md`'s D-G5 ("outliners are server-rendered HTML fragments") is
therefore a genuine MACVM-only design, not a rediscovery of an existing
Strongtalk mechanism.** Worth stating plainly, since every other visual
decision in this project is explicitly grounded in a real Strongtalk
precedent — this is the one exception, and it's exactly the piece
`docs/APPS.md` §6 already calls out as "the one genuinely new piece."
`docs/APPS.md` §5.1's `ToolNode`/DOM-id-registry design and §6's
`HtmlWriter` are the concrete carriers of this idea; this document doesn't
add anything new there, just confirms the "no upstream precedent" premise
they're built on.

## 5. Theming `smappl`-embedded images (`Image fromFile:`)

Every `Button withImage:` shape in §3's vocabulary hard-codes a bitmap path:
`Image fromFile: (FilePath for: 'resources/edit.bmp')`. Taken literally,
that ties every smappl-built button to one fixed pixel-art image — in
tension with the Theme menu (`main.rs`'s `Theme::Classic`/`Theme::HiDef`,
same document set this project already ships): Classic should keep the
period bitmap, Hi-Def should get the matching vector icon, and the corpus
HTML asking for `resources/edit.bmp` shouldn't need to change to get either
(D-G4 keeps corpus files byte-identical — rewriting the string is not an
option).

**The fix is the same resolver already built for the native toolbar,
reused at a different call site.** `src/preprocess.rs`'s `Theme::icon_url`
already maps a *logical icon name* (`"edit"`, `"home"`, …) to a
theme-appropriate asset URL — PNG under Classic, SVG under Hi-Def — for the
toolbar chrome this crate injects. The **names are not a coincidence**:
every bitmap the corpus's own `smappl` code references under `resources/`
(`edit`, `smallHome`, `open`, `implementors`, `senders`, `userHierarchy`,
`hierarchy`, `blankSheet`, `texteditor`, `documentation`, `goForward`, plus
`abstract`, `clone`, `closedItem`, `fullInterface`, `incontext`, `openAll`,
`openItem`, `publicInterface`, `superclass`) already exists as a bitmap
under `reference/icons-bmp/` with the exact same name — both trace back to
the same original Strongtalk icon set. `assets/icons-hidef/` now has an SVG
for every one of those 20 names (not just the 11 the native toolbar uses),
so the vector art is ready and waiting for the day a real evaluator needs
it.

**What G2's `Image fromFile:` hook needs to do**, once a real `FilePath`/
`Image` primitive exists: parse `FilePath for: 'resources/NAME.bmp'` down
to the logical name (strip the `resources/` prefix and `.bmp` suffix), then
call the *same* `Theme::icon_url(NAME)` the toolbar already calls — not
literally load whatever file the string names. This makes the resolver
name-keyed, not path-keyed: the corpus text can go on saying
`'resources/edit.bmp'` forever (byte-identical, per D-G4), while what
actually loads depends entirely on the active theme. A bitmap path that
doesn't map to a known logical name (some page's own custom, uncataloged
image) should fall through to loading the literal path relative to the
page's directory, same as today's behavior — theming only intercepts the
*known, cataloged* icon vocabulary, not arbitrary page-supplied images.

This is scoped as design only for now — there's no `Image`/`FilePath`
primitive to hook until G2's VM eval lands (`PLAN.md` §6) — but the assets
are in place and the interception point is exact, so this is a small,
concrete addition to G2's task list rather than an open question.

## 6. Current MACVM status and what's next

**G0 (parsing half, done earlier):** `gui/src/preprocess.rs`'s
`rewrite_smappl_placeholders` rewrites every `<smappl visual="CODE">` (no
closing tag, multiline values collapsed to a single line) into a
`<span class="smappl" data-code="CODE">` — a correct, cited-accurate
implementation of the *parsing* half of `smappl` (§2 above).

**G2 first live slice (done — button/glue/side-effect shapes 1, 2, 6):** the
*evaluation* half now runs for the shapes that don't need reflection. The
placeholder span additionally carries a `data-widget-id`; on page load
`smtk.js` posts `{kind:"smappl", id, code}` per span, the GUI worker calls
`VmHandle::render_fragment` (`src/embed.rs`), and `main.rs` swaps the span's
`outerHTML` for the answer (D-G5). `render_fragment` wraps the source as
`(Visual coerce: (CODE)) htmlFragment.` — exactly §2's `ElementSMAPPL` shape
— and returns the resulting `String`'s **raw** bytes (not a `printString`,
which would re-quote it); a render failure is swallowed to an `Err` so the
GUI keeps the G0 placeholder box, matching §2's `ifError:` discipline.

The Visuals render **themselves** to HTML image-side (`world/33_smappl.mst`,
per `docs/APPS.md` §6 / amendment A19 — Rust only transports the string):
`Visual` (`coerce:`/`htmlFragment`), `Glue xRigid:`, and `Button
labeled:action:` / `withImage:action:`. Live widget actions are closures
kept in `SmapplRegistry` (a `<classVars:>` id→widget dictionary — the
`visualRegistry` analog of §2), keyed by the opaque id the fragment carries
in `data-widget-action`; a click posts `{kind:"smapplAction", id}` and the
worker runs `SmapplRegistry fire: id` (`../PLAN.md` G1's "route by opaque id
through a worker-owned handler table"). CSS: `.smappl-button` renders the
period Win95 raised bevel under the classic `strongtalk.css` theme and a
`currentColor`-keyed flat button under the six alternative themes (the
`--st-*` bevel is invisible on the dark/`--hd-*` themes). Covered by tests in
`src/embed.rs` and `gui/src/vm_host.rs` (including a **DB-boot** render, the
path the real GUI actually uses — S22 fidelity risk).

**G2 hierarchy outliner (done — the start page's own smappl):**
`startPage.html`'s `ClassHierarchyOutliner imbeddedVisualForClass: Object`
now renders a real, live class-hierarchy tree. The first Phase-W layers back
it (`docs/APPS.md` §1's stack): a VM reflection primitive (`allClasses`, R1
— `src/runtime/primitives.rs`, id 98; walks the global namespace, returns
every class object) → an image-side `ClassMirror` (`world/34_tools.mst`:
`name`/`superclass`/`subclassMirrors`, subclasses computed by filtering
`allClasses`, since the VM keeps no subclass index — exactly Strongtalk's
`ClassVMMirror` approach) → a `ClassHierarchyOutliner` `Visual` whose
`htmlFragment` walks the tree → an `HtmlWriter` (§6's fragment builder:
`raw:`/`escaped:`/`contents`). Rendered with the existing `.st-outliner`/
`.st-node`/`.st-header` CSS (a G0/G3 deliverable). Covered by embed +
DB-boot tests; verified visually.

**G2+ remainder (needs later Phase-W waves, `docs/APPS.md` §2/§6):**
- **Lazy expand/collapse** — the tree is currently rendered fully expanded
  in one shot; the D-G5 toggle protocol (`{kind:"toggle", nodeId}` →
  `ToolNode>>toggle`, per-node fragment push) and the node-id/DOM-id
  registry (§2's caching analog) are the next layer.
- **Method/category nodes** inside a class (a class's selectors, categories,
  and method source) — needs a `selectorsOf:` reflection primitive (R2) and
  a `MethodMirror`; not built.
- The **`ClassOutliner` (§3.4) and `CodeView` (§3.5) shapes** — the
  single-class browser embed and the embedded editor. `ClassOutliner` needs
  the `for:`/`topVisualWithHRule:`/`withBorder:`/`Border` chain; `CodeView`
  needs the `TextView` substrate. Until then they fail the render and stay
  the G0 placeholder box (§3's "anything else renders as the placeholder").
- Per-element **caching across relayout** (`visualRegistry` → `ToolNode`'s
  node-id/DOM-id registry) so expand/collapse and button state survive
  reflow — the slice re-renders on each load rather than caching.
- Route `Image fromFile: (FilePath for: 'resources/NAME.bmp')` through
  `Theme::icon_url(NAME)` (§5) instead of carrying the raw path in
  `data-icon`, so icon buttons theme for free. The `Image`/`FilePath` stubs
  in `world/33_smappl.mst` already reduce a `withImage:` argument to its
  logical name; the Rust-side theme resolution is the missing half.
- **HTML-escape** widget label text (the shipped corpus labels are
  ASCII-safe, so the slice omits it).
