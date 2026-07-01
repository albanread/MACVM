# MACVM Sprint Implementation Docs

Per-sprint design advice + test plans for the implementing agent. **Read in
this order for any sprint SNN:**

1. [`../SPEC.md`](../SPEC.md) — the engineering contract (SPEC always wins;
   §15 is the amendment log from the review that produced these docs).
2. [`CONVENTIONS.md`](CONVENTIONS.md) — binding names, crate layout, env
   flags, templates.
3. `sprint_sNN_detail.md` — the sprint's design: data structures, algorithms,
   implementation order, pitfalls, interfaces for later sprints.
4. `tests_sNN.md` — the acceptance gate and full test inventory. A sprint is
   done only when its gate AND all previously-established stress modes pass
   ([`../SPRINTS.md`](../SPRINTS.md) standing rules).

| Sprint | Detail | Tests | Scope |
|---|---|---|---|
| S0 | [detail](sprint_s00_detail.md) | [tests](tests_s00.md) | Tags, mark word, typed wrappers, smi arithmetic |
| S1 | [detail](sprint_s01_detail.md) | [tests](tests_s01.md) | Reservation, eden allocator, formats, genesis, symbols |
| S2 | [detail](sprint_s02_detail.md) | [tests](tests_s02.md) | Bytecode, builder, disassembler, interpreter core (no sends) |
| S3 | [detail](sprint_s03_detail.md) | [tests](tests_s03.md) | Sends, lookup, IC lattice, DNU, primitives |
| S4 | [detail](sprint_s04_detail.md) | [tests](tests_s04.md) | Closures, Contexts, NLR, ensure:/ifCurtailed: |
| S5 | [detail](sprint_s05_detail.md) | [tests](tests_s05.md) | Source compiler (lexer/parser/codegen), world loader, REPL |
| S6 | [detail](sprint_s06_detail.md) | [tests](tests_s06.md) | Core library, SUnit-lite, in-language suite, benchmarks |
| S7 | [detail](sprint_s07_detail.md) | [tests](tests_s07.md) | Scavenger, card barrier, Handles, GC stress (MacNCL lessons) |
| S8 | [detail](sprint_s08_detail.md) | [tests](tests_s08.md) | Full GC: mark + slide compact, heap growth, soak |
| S9 | [detail](sprint_s09_detail.md) | [tests](tests_s09.md) | Vendor JASM, Assembler trait, code cache, MacJit |
| S10 | [detail](sprint_s10_detail.md) | [tests](tests_s10.md) | Straight-line JIT: IR, linear scan, nmethods, dispatch |
| S11 | [detail](sprint_s11_detail.md) | [tests](tests_s11.md) | Compiled sends, PICs, patching, adapters, stubs |
| S12 | [detail](sprint_s12_detail.md) | [tests](tests_s12.md) | Oop maps, moving GC under compiled frames (flagship gate) |
| S13 | [detail](sprint_s13_detail.md) | [tests](tests_s13.md) | Deopt: scope descs, brk traps, frame materialization |
| S14 | [detail](sprint_s14_detail.md) | [tests](tests_s14.md) | Type feedback, inlining, customization, Context elision |
| S15 | [detail](sprint_s15_detail.md) | [tests](tests_s15.md) | OSR, Richards/DeltaBlue, PERF.md |

Provenance: drafted 2026-07 by parallel review agents against SPEC.md +
`../reference-vm-analysis.md` (Self, Strongtalk, JASM, MacNCL-GC); every
`SPEC-QUESTION` raised was adjudicated in SPEC.md §15. The S7/S8 docs embed
the 15 MacNCL GC lessons (analysis §5).
