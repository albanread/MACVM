# MACVM Apps Design — Mirrors, Tools, Live Pages

The design of MACVM's Smalltalk-side application layer: the reflection
(mirror) library, the programming tools (browsers/outliners, inspector,
workspace, find tools, debugger), and the HTML-fragment rendering that
connects them to the Phase G GUI shell. Grounded in a file-level survey of
`../strongtalk-repo/StrongtalkSource/` (citations are bare filenames there).
Companion: [`WORLD.md`](WORLD.md) (library); GUI shell: [`../gui/PLAN.md`](../gui/PLAN.md).

**Headline finding: Strongtalk's entire tool suite is image-side Smalltalk
over a small, dumb VM-primitive surface.** Browsers hold mirrors; the
debugger presents activation mirrors; senders/implementors are image-side
sweeps; even doit compilation is orchestrated image-side. MACVM adopts this
architecture — which **revises SPEC §16.3** (amendment A19): mirrors are
Smalltalk objects backed by VM primitives, not Rust-side queries. Rust keeps
only the transport (eval channel + fragments) and the compile service.

---

## 1. The layering (pinned)

```
MACVM VM (Rust)            reflection primitives + compile service (§3, §4)
   ↑ primitives
Smalltalk Mirrors          ObjectMirror, ClassMirror, MethodMirror,
                           ActivationMirror, ProcessMirror            (§2)
   ↑
Smalltalk Tools            ToolNode framework; class/hierarchy/category/
                           method outliners; Inspector; Workspace;
                           StackTraceInspector; find tools             (§5)
   ↑ render
HtmlWriter                 fragment builder: escaping, css-class,
                           doit:/toggle: helpers                       (§6)
   ↓ (nodeId → html fragment)
GuiHost bridge (Rust)      D-G3 JSON ↔ evaluateJavaScript, WKWebView
```

Nothing tool-*logic*-shaped lives in the Rust gui crate: it keeps the window/
menu shell, the D-G4 preprocessor, `smtk.js`, fragment insertion, and the G1
stub host.

## 2. The mirror library (Smalltalk, Phase W2)

Strongtalk has two mirror families: low-level per-VM-format `VMMirror`s
(VMMirror.dlt dispatches on `{{primitiveBehaviorVMType:}}` into ~34 classes)
and source-level definition `Mirror`s (Mirror.dlt, explicitly modeled on
Self's mirrors: tools never send reflective messages to objects directly).
The split exists largely because of mixins and the type system. **MACVM is
class-based and mixin-free, so the two families collapse into five classes:**

| MACVM class | Wraps | Strongtalk ancestry |
|---|---|---|
| `ObjectMirror` | any oop: class, instVar enumeration (visitor), safe printString | OopsVMMirror + Inspector's ObjectIterator |
| `ClassMirror` | a class: name, superclass/subclasses, instVar names, selectors, methodAt:, addMethod:/removeMethod: | ClassVMMirror + ClassMirror + (MixinDeclMirror collapsed — no mixins) |
| `MethodMirror` | a CompiledMethod: selector, argc, source (A16 registry), referenced selectors/globals/instVars, disassembly | MethodVMMirror + Method.dlt protocol |
| `ActivationMirror` | one frame of a (suspended) process: receiver, method, args/temps, bci, source-range highlight | Activation.dlt (all-primitive protocol) |
| `ProcessMirror` | a process: activation stack, resume/step/terminate | ProcessVMMirror + Process.dlt |

Conventions adopted from Strongtalk:
- **Encapsulation rule** (Mirror.dlt:9-48): tools go through mirrors only —
  no `thing class`, `thing instVarAt:` in tool code. Keeps reflection
  swappable and keeps tools honest.
- **Subclass computation is image-side** (ClassVMMirror.dlt:155-242 walks
  `Smalltalk classesDo:` with a cache) — the VM does not maintain a subclass
  index. `ClassMirror subclasses` does the same walk over the SystemDictionary
  with a simple invalidation counter (bumped on class definition).
- **`Reflection` static entry points** (Reflection.dlt): `classOf:`
  (primitive — immune to `#class` overrides), `identityHashOf:`, plus later
  heap queries. A tiny Smalltalk facade over primitives.

## 3. The reflection primitive surface (VM obligation, pinned)

Priority-grouped; this table extends SPEC §10 and replaces the §16.3 sketch.

| Group | Primitives | Needed by |
|---|---|---|
| **R1 structure** (read) | `classOf:`, `superclassOf:`, `identityHashOf:`, `behaviorFormat:`, `instVarNamesOf:`, `instVarOf:at:` / `instVarOf:at:put:`, `oopSizeOf:`, SystemDictionary enumeration (ordinary heap objects — no primitive needed beyond `Smalltalk` access) | G3 browsing, Inspector |
| **R2 methods** (read) | `selectorsOf:`, `methodOf:at:`, per-method: selector, argc, flags, **`methodReferencedSelectors:`**, **`methodReferencedGlobals:`**, **`methodReferencedInstVarIndices:`** (bytecode/literal-frame scan), `methodDisassembly:`; source via the A16 registry (no primitive) | G3, find tools (§5.4) |
| **R3 compile service** (write) | `compileMethod: src class: k → CompiledMethod \| (msg, pos)`; `compileDoit: src receiver: r → closure \| (msg, pos)`; `installMethod:in:` (= S3 install path) | G2 doits, G4 accept |
| **R4 heap queries** (deferrable) | `allObjectsLimit:`, `instancesOf:limit:`, `referencesTo:limit:` | Inspector's references/instances menus |
| **R5 debugger** (deferrable) | `processStack: limit:` → activations; per-activation receiver/method/args/temps/bci; `activationSourceRange:`; single-step/step-next/step-return; process suspend/resume/terminate | StackTraceInspector (W-debugger) |

Notes:
- **R2's referenced-selectors scan is the whole find-tools story**: Strongtalk
  computes senders by sweeping all methods and asking each
  `{{primitiveMethodSenders}}` (Method.dlt:82-84; Smalltalk.dlt:995-1075).
  The VM indexes nothing. MACVM does the same — a literal-frame + IC-table
  walk per method (our IC side tables make this trivial: selectors are
  right there in the `ics` array).
- R3 is the Rust source compiler re-exposed (SPEC §16.2's `eval` is the
  doit path; accept needs the method path with class context). **Error
  returns carry (message, character position)** — Strongtalk's tools display
  in-editor parse errors (`showParseError:at:in:`); pin the same contract.
- R5 requires interpreter support for suspended-frame inspection and
  single-step (a dispatch-loop mode, not new architecture) — schedule with a
  dedicated debugger wave, not before.

## 4. Eval & accept protocol (Smalltalk-facing, pinned)

Tools depend on exactly this surface (from Smalltalk.dlt:181-209,
Workspace/CodeView, MessageDeclarationOutliner.dlt:243-294):

```smalltalk
Smalltalk evaluate: src ifError: [:msg :pos | …]
Smalltalk evaluate: src receiver: oop ifError: [:msg :pos | …]
Smalltalk blockToEvaluateFor: src receiver: oop ifError: […]   "deferred form —
    Workspace wants a closure it can time / wrap in ensure:"
aClassMirror addMethod: src category: cat ifFail: [:msg :pos | …]
```

Implementation: thin Smalltalk wrappers over R3. The deferred form matches
Strongtalk's doit compilation (compile as a block's method, then
`convertToClosure: receiver` — Method.dlt:24-31); MACVM's Rust compiler can
compile the doit to a zero-arg method on the receiver's class and answer a
closure-like thunk. `VmHandle::eval` (SPEC §16.2) becomes a caller of the
same machinery.

## 5. The tool suite (Smalltalk, Phases W3–W4)

### 5.1 `ToolNode` — the outliner framework (replaces OutlinerApp)
Strongtalk's `OutlinerApp` (OutlinerApp.dlt) is a self-describing lazy tree
node: required `buildClosedHeader` + `buildBody`, optional `buildOpenHeader`,
toggle glyphs (`closedItem.bmp`/`openItem.bmp`), indent 20, parent/children
management, cached body/headers, per-node update hooks. MACVM recreates the
protocol with HTML output:

```smalltalk
Object subclass: ToolNode [
  | id parent children open |
  closedHeaderOn: html   "required — one line, an HtmlWriter"
  openHeaderOn: html     "default: closedHeaderOn:"
  buildChildren          "required — lazy; answers Array of ToolNode"
  actionsFor: node       "menu items as data (§6.3)"
  toggle  update  renderOn: html   "framework-provided"
]
```

Framework behavior (mirrors OutlinerApp): body built on first open; children
cached; `update` re-renders header/body fragments and ships them over the
bridge keyed by `id` (the D-G3 `{kind:"toggle", nodeId}` message lands in
`ToolNode>>toggle`). The **node id ↔ DOM id registry** plays the role of
Strongtalk's `visualRegistry` (HTMLBuilder.dlt:402-415) — stable identity for
embedded live parts across re-renders.

### 5.2 The browsers (Strongtalk's node classes, mixin layer collapsed)
- `ClassHierarchyNode` (← ClassHierarchyOutliner.dlt): recursive body = one
  child per subclass mirror; filter blocks ("Filter: %HTML with subclasses",
  superclasses in lighter face).
- `ClassNode` (← ClassOutliner/DefWithMsgOutliner): header = class name;
  body = definition item, comment, instance side, class side.
- `SideNode` → `CategoryNode` → `MethodNode` (← Side/Category/Method
  outliners): categories from a per-class category map (MACVM keeps method
  categories in a world-side registry alongside A16 sources — Strongtalk
  kept them in the external SourceHandler database); `CategoryNode`'s header
  is rename-in-place editable (CategoryOutliner.dlt:171-193); `MethodNode`
  header = selector + badge glyphs, menu = Senders/Implementors/Bytecodes.
- `MethodNode` expanded body = **embedded editor**: get-model = source from
  A16 registry; accept = `mirror addMethod:category:ifFail:` with in-editor
  (msg, pos) error display (← MessageDeclarationOutliner.dlt:243-294).

### 5.3 Inspector (← Inspector.dlt)
`Inspector on: oop` wraps an `ObjectMirror` and drives a **visitor**
(`beginObject/iterateHeader/iterateInstanceVariables/iterateIndexables/
endObject` — VMMirror.dlt:98-123) to build rows; indexables paged; every
value cell's action opens `Inspector on: value`; `safePrintString` guards
broken `printOn:`s. Direct recreation with HtmlWriter rows.

### 5.4 Find tools (← Smalltalk.dlt:995-1075)
`Smalltalk implementorsOf: sel` = walk class mirrors asking `includesSelector:`;
`sendersOf:` = walk all methods asking `referencedSelectors includes:` (R2);
`referencesToGlobal:/toInstVar:for:` likewise. Results = list-of-(mirror,
selector) nodes (← MirrorListOutliner). Wildcards supported at the tool
level (`sendersMatching:`). No VM index; sweep cost is fine at MACVM scale.

### 5.5 Workspace & Transcript (← Workspace.dlt, CodeView.dlt:309-330)
Workspace = editor pane + doIt/printIt/inspectIt actions over §4's protocol;
**Transcript is just a global bound to a Workspace-like sink**
(Launcher.dlt:32-37 does `Transcript := Workspace new`) — MACVM: the world's
`Transcript` global forwards to the TranscriptSink (SPEC §16.2), and the GUI
presents it in the Launcher pane; a `TempTranscript`-style console fallback
(TempTranscript.dlt) covers pre-GUI boot, which is exactly S6's design.

### 5.6 Launcher (← Launcher.dlt)
Toolbar actions (the nine buttons, gui/PLAN.md §1) map to: home page, find
definition / implementors / senders dialogs → §5.4 sweeps, hierarchy
browsers → §5.2 nodes, workspace/editor → §5.5, documentation → static page.
Adopt the **class-side menu registry** (`Launcher registerMenu:for:`,
Launcher.dlt:92-108) so any tool class can contribute menus — it becomes the
data source for the native NSMenu population (D-G1).

### 5.7 Debugger (later wave, needs R5)
`StackTraceInspector` (StackTraceInspector.dlt) over `ProcessMirror`:
body = one `ActivationNode` per frame (first open, block-value frames
elided); `ActivationNode` header = `receiver >> selector` (each an
inspect link), body = step into/over/out buttons + source with **current
range highlighted** + temporaries column. Strongtalk highlights via
VM pretty-print with 0x1B markers (ActivationOutliner.dlt:112-129); MACVM
can do better: bci → source range via the A16 source + a bci↔source map the
compiler can retain (design at the debugger wave). Requires R5 + suspended
processes — schedule after W4, likely alongside Phase-E processes.

## 6. HtmlWriter — the one genuinely new piece (Phase W2)

Strongtalk pages are Visual trees built by HTMLBuilder; MACVM pages are real
HTML in WKWebView. Tools therefore render through a small Smalltalk
fragment builder (nothing like it exists upstream — Visual vocabulary maps
to CSS):

```smalltalk
html := HtmlWriter new.
html span: 'Point' class: #className.
html doit: 'Point browse' text: 'browse'.        "→ <a class=doit data-code=…>"
html toggleFor: node.                            "→ the open/close glyph img"
html div: [ … ] class: #categoryHeader.
html escaped: aString.                           "text with HTML escaping"
node fragment  "→ the accumulated string, shipped keyed by node id"
```

- **CSS-class mapping of painter roles** (ProgrammingEnvironment.dlt):
  `codePainter/selectorPainter/classPainter/sectionPainter` →
  `.code/.selector/.class/.section` in `strongtalk.css` (already the G0
  deliverable).
- **Menus as data** (Menu/MenuAction's `name/active/checked/action` — a
  clean declarative model): tools answer menu structures; the shell renders
  them (NSMenu or beveled HTML popup per G3's call).
- doit/smappl attributes follow the D-G4 rewriting convention so tool-made
  fragments and preprocessed legacy pages share one JS runtime.

## 7. Update protocol

Strongtalk outliners register as dependents of the source database and react
to `changed:` events (MirrorOutliner.dlt:66-69; fired by the accept path,
DefWithMsgSourceHandler.dlt:159-168). MACVM: a world-side `ToolRegistry`
(open nodes by id); mirror mutations (`addMethod:` etc.) notify it; affected
nodes re-render and push fragments. Same shape, one page-refresh-free loop.

## 8. Redefinition semantics

Strongtalk's accept path clones the *mixin*, mutates the copy, and commits
atomically with `{{primitiveApplyChange:}}` (MixinDeclMirror.dlt:357-391) —
machinery MACVM does not need: **per-method install + IC self-heal (S3) +
compiled-code dependency invalidation (S13)** covers the class-based case.
Class-*shape* changes (adding instVars to a class with live instances) stay
out of scope for v1 tools (reopen-with-migration is a stretch topic; the
tools grey out shape edits on classes with instances).

## 9. What stays in Rust

The gui crate: window/menus/WKWebView shell, D-G4 preprocessor, smtk.js,
fragment transport, stub host. The VM core: the R1–R5 primitives, the
compile service, A16 source registry, TranscriptSink. Everything else in
this document is MACVM Smalltalk in `world/tools/`.

## 10. Phasing (see SPRINTS.md Phase W)

- **W2**: mirror library + R1/R2 primitives + HtmlWriter + ToolNode core.
- **W3** (pairs with G2/G3): Inspector, Workspace/eval wiring, find tools.
- **W4** (pairs with G4): browser node suite + accept path + update protocol.
- **W-debugger** (later, with R5): StackTraceInspector + ActivationMirror.

Each wave lands with in-language tests (mirror facts against known classes;
HtmlWriter escaping goldens; find-tool sweeps over the seed world) plus a GUI
gate shared with the corresponding G-phase.
