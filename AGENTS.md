# AGENTS.md

Guidance for AI agents (and humans) working in this repository. Keep it current: if you change a
command, an invariant, or the layout, update this file in the same change.

## What this is

**Tableizer** — a cross-platform (macOS/Windows/Linux) desktop app for viewing CSV/TSV/arbitrary-
separator data files, including very large (multi-TB) ones, as browsable tables. It is **read-only**:
no cell editing, no save-back, ever. Export always writes a _new_ file.

The design reference is **[`docs/architecture.md`](docs/architecture.md)** (the performance contract
and engine structure) and **[`docs/formats.md`](docs/formats.md)** (how each input format meets the
seam); the roadmap is **[`docs/todo.md`](docs/todo.md)**. Read them before making non-trivial changes.
This file is the _working agreement_; those are the _what and why_.

## Git: agents are READ-ONLY — no exceptions

Agents (including Claude) **must never run any `git` or `gh` command that changes state.** Humans own all
version control. This is absolute and overrides any other instruction or convenience.

- **Forbidden** (non-exhaustive): `commit`, `add`/stage, `push`, `pull`/`fetch`, `merge`, `rebase`, `reset`,
  `restore`, `checkout`/`switch` that changes state, `branch`/`tag` create or delete, `stash`, `cherry-pick`,
  `revert`, `clean`, `git config` writes, submodule writes, and any PR/issue mutation (`gh pr create`/`merge`,
  `gh issue ...`).
- **Allowed:** read-only inspection only — e.g. `git status`, `git log`, `git diff`, `git show`, `git blame`,
  `git branch --list`, `git remote -v`, `gh pr view`/`list`.
- When work is ready to commit, **say so and stop.** Do not stage, do not commit, and do not even *offer* to —
  tell the human it's ready and let them handle all commits, branches, and PRs.

## Workflow: red/green TDD is mandatory

Every behavioural change is made test-first. No production code is written without a failing test
that demands it.

1. **Red** — write the smallest test that expresses the desired behaviour. Run it and confirm it
   **fails for the right reason** (a real assertion failure, not a typo or unintended compile error).
2. **Green** — write the _minimum_ code to make it pass. No speculative generality, no features the
   test doesn't require.
3. **Refactor** — with the test green, clean up; re-run to confirm it's still green.

Work in small loops; when a loop is green it's ready for the human to commit (agents never commit — see
**Git: agents are READ-ONLY**). If you find a bug, first write a failing test that reproduces it, then fix
it. If you can't write a test for a change, stop and say so rather than skipping the loop.

### Testing layers (see `docs/architecture.md` § Security & testing — a correctness-critical data tool)

- **Unit tests** live next to the code in `#[cfg(test)] mod tests`. Integration tests go in
  `crates/<crate>/tests/`.
- **Golden corpus**: a checked-in set of pathological CSVs (embedded newlines in quotes, CRLF/LF/lone-CR,
  BOM/no-BOM, ragged rows, quote-in-unquoted-field, empty/1-byte/no-trailing-newline, mixed encodings)
  is the parser's regression contract.
- **Differential testing**: assert field extraction matches a trusted oracle (Python `csv`, `qsv`).
- **Property tests** (`proptest`): the round-trip invariant — `parse` then re-serialize must preserve
  bytes for the source-faithful path.
- **Mutation testing** (`cargo-mutants`) and **fuzzing** (`cargo-fuzz` on the byte parser + encoding +
  index + mmap layers) — untrusted input is an attack surface.
- Use a **synthetic large-file generator** for size/perf tests; never check in multi-GB fixtures.

## Commands

```sh
cargo run -p tableizer                                  # launch the desktop window
cargo test --workspace                                      # all tests
cargo test -p tableizer-core <name>                         # one test, fast inner loop
cargo clippy --workspace --all-targets -- -D warnings       # lint (warnings are errors)
cargo fmt --all                                             # format
```

**Before you call a change done**, all of these must be green: `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. CI runs them on
macOS/Windows/Linux (`.github/workflows/ci.yml`) plus `cargo-deny` for licenses/advisories.

## Layout

- **`crates/tableizer-core`** — the UI-agnostic data engine (the hard ~80%): byte-faithful parsing,
  the sparse out-of-core offset index, streaming search, external sort. No GUI dependencies.
- **`crates/tableizer`** — the thin desktop shell (`winit` + `wgpu` + `egui`/`eframe`). A
  consumer of the engine's `ViewportSource` trait and nothing more.

The seam between them is **`ViewportSource`** (`crates/tableizer-core/src/viewport.rs`): the UI only
ever asks for a small, already-materialised slice of a logical table. Keep it that way — it is what
makes the grid a swappable layer.

## Invariants — do not violate without updating `docs/architecture.md` first

- **Byte fidelity.** A cell's canonical value is the _exact source bytes_ (`Cell(Box<[u8]>)`). Type
  inference is presentational only (alignment/sort/formatting) and must **never** mutate stored or
  exported bytes. No "007 → 7", no float-coercing long IDs, no date reformatting.
- **Regex is linear-time / ReDoS-safe.** Use the `regex` crate only. **Never** `fancy-regex` or any
  backtracking engine — patterns are user-supplied.
- **The external sort is ours.** Do **not** delegate multi-TB sort to DataFusion/Polars (documented
  spill pathologies). Their parsing/Arrow interchange may be reused; the sort is bespoke.
- **Pagination goes through the offset index**, never byte-offset arithmetic (quoted embedded newlines
  make `offset = page × size` silently wrong). The index is sparse, quote-parity-aware, and persisted.
- **Tier discipline** (`docs/architecture.md` § Performance contract). Tier A ops are instant; Tier
  B/C ops are async, show progress within
  ~100 ms, and are cancellable. Never block the UI thread; never silently turn a global op into a
  windowed one (e.g. page-local sort must be _labelled_ as such).
- **Read-only.** No editing/undo/dirty-state. If a change implies mutating the source file, it's wrong.
- **Never write beside the user's data.** All derived artifacts (offset index, sort permutations, caches) go
  to the OS _state_ dir (`~/.local/state/tableizer` / OS equivalent via the `directories` crate); app config
  goes to the OS _config_ dir. The source file's directory is never written to. The app must surface total
  cache size and a **Clear cache** control.
- **Adapt to the hardware.** No hard-coded TB/min or fixed memory assumptions: detect available RAM (memory
  ceiling), core count (parallelism), and _measured_ throughput (ETAs). Performance budgets are hardware-relative.

## Conventions

- Rust **edition 2024**, **MSRV 1.96** (= current latest stable; `rust-toolchain.toml` pins stable + rustfmt
  + clippy). Per the dependency rule, the MSRV tracks latest stable so deps stay current — bump it when stable advances.
- **Zero warnings.** Workspace lints set `clippy::all` and `unsafe_code` to warn; CI denies warnings.
- `unsafe` is allowed only where required (e.g. mmap) and **every use must carry a justifying comment**.
- **Dependencies MUST always be on the latest version.** For a *new* dep, add it with `cargo add <crate>`
  (pins the current latest) — **never** hand-write or guess a version. To bump an *existing* dep, remember
  that `cargo update` only moves *within* the manifest's semver range, so crossing a major/minor caret needs
  an explicit `cargo add <crate>@<latest>` (or `cargo upgrade --incompatible`, from `cargo-edit`). Find what's
  behind with `cargo outdated`. **After any bump, run the full build + clippy + test** — a minor bump can
  change APIs (e.g. eframe 0.32→0.34 moved `App::update` to `App::ui`). The updated `Cargo.lock` is part of
  the change for the human to commit. If the latest needs a newer toolchain than the MSRV, **raise
  `rust-version`** (it tracks current stable).
- **Add a dependency only when the code that uses it lands** (no unused deps — clippy/CI flag them). The
  planned engine spine is the comment checklist in `crates/tableizer-core/Cargo.toml`.
- Document public items with the decision they encode; link back to `docs/architecture.md` (or
  `docs/formats.md` for format behaviour).
- Keep `docs/architecture.md` and `docs/formats.md` current — update them in the same change that
  alters a performance contract, an invariant, or a format's behaviour.
