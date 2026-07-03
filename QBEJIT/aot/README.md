# aot/ — QBE as an ahead-of-time backend (FBCQBE)

Source: [github.com/albanread/FBCQBE](https://github.com/albanread/FBCQBE),
an earlier BASIC compiler that used QBE as a native-code backend by shelling
out to `as`/`cc`. Cloned 2026-07-03, shallow clone of the default branch.

**This is not a JIT.** See [`../jit/`](../jit/) for the actual in-process
JIT extraction, and [`../REVIEW.md`](../REVIEW.md) for why this AOT path was
the wrong first answer to "extract the QBE JIT components."

## Layout

- **`core/`** — the vendored QBE compiler proper: SSA-form IL parser, a
  target-independent optimization pipeline, and three backends (`amd64/`,
  `arm64/`, `rv64/`). `main.c` has been restored to QBE's original CLI driver
  (`qbe -t <target> file.ssa -o out.s`) — the upstream FBCQBE source had
  spliced a BASIC-file front-end and a shell-out-to-`cc` linker driver
  directly into `main()` (see `integration_reference/`). Everything else,
  including the `arm64/emit.c` MADD/FMADD fusion peephole FBCQBE added on top
  of stock QBE, is unmodified.

  Verified to build and run standalone on this machine (Darwin arm64):
  `cd core && ./build_qbe.sh` produces `core/qbe`, confirmed against a
  hand-written hello-world `.ssa` file compiled through to a running arm64
  binary via `cc`.

- **`integration_reference/`** — *not compiled, kept for reference only*. Shows
  how FBCQBE embedded QBE as an in-process library rather than shelling out to
  a `qbe` binary: `qbe_lib.h` (the intended C API), `fasterbasic_wrapper.cpp` /
  `basic_frontend.cpp` (their C++ frontend calling into QBE's `parse()`/`func()`
  callbacks via a `fmemopen`-backed `FILE*`), and the patched `main.c` behavior
  captured in `build_qbe_basic.sh`. Useful as a worked example of embedding
  QBE's *frontend* side; the backend side of every code path here still shells
  out to `cc` to turn assembly text into an executable — see REVIEW.md.

- **`known_issues/`** — `BUG_REPORT.md`, a register-allocation correctness bug
  the FBCQBE authors found in QBE's arm64 backend (interacting with their MADD
  fusion peephole): array writes after a `WHILE` containing a nested `IF` land
  in the wrong physical register.

## License

Upstream QBE: `../LICENSE-QBE`. FBCQBE's own additions: `../LICENSE-FBCQBE`.
Both MIT.
