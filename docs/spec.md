# Tableizer — Specification

Tableizer is a cross-platform desktop app for loading and parsing data files that are
not easily human-readable into browsable tables, including **very large (multi-TB)** files.

This document supersedes the original goals list (preserved verbatim in the appendix).
It exists because the original headline goal ("parse multi-TB files without performance
degradation") is *physically unsatisfiable as written*: any operation that must touch
every byte of a multi-TB file is bounded by storage bandwidth (a single full read is
minutes on NVMe, hours on HDD). The fix is not a faster algorithm; it is an honest,
**per-operation performance contract**. That contract is the centre of this spec.

---

## 1. Decisions

### Confirmed by you

| Decision | Choice | Rationale |
|---|---|---|
| **License** | **MIT OR Apache-2.0** (dual, permissive — the Rust norm) | Replaces the initial GPL-3.0. Also unblocks permissively-licensed grid options (Avalonia's TreeDataGrid went AGPL/commercial; not relevant now, but the principle holds). |
| **Size envelope** | **Literal multi-TB is a committed v1 target** | A multi-TB file far exceeds desktop RAM, so the engine must operate **out-of-core for large inputs** (streaming/mmap, sparse persisted index, external spill-to-disk sort) — a derived consequence of the target, mandatory not deferred. Smaller files that fit comfortably may still be loaded in memory where that's simpler. |
| **Language / stack** | **Single-language Rust**, engine + GUI in one process (no FFI/IPC boundary) | Won a weighted judge-panel comparison 89.6/100 (perfect on the engine tier, 18+ pts clear): the only stack scoring top marks on all three dominant axes — memory-safe adversarial parsing, no-GC 60 fps-under-scan latency, exact-fit out-of-core primitives. |
| **Mutability** | **Read-only. No editing, ever.** | No cell editing, save-back, or in-place mutation — therefore no undo/redo/dirty-state. Export is the only write path and always writes a *new* file. A deliberate, permanent scope decision, not just a v1 cut. |
| **Hardware** | **Adapts to the machine at runtime** | No fixed target. Detect available RAM (→ memory budget), core count (→ parallelism), and *measured* disk throughput (→ ETAs/progress). Performance budgets are **hardware-relative**, not absolute. The app runs anywhere; it tunes itself to what it finds. |
| **Derived-artifact location** | **OS *state* dir only — never beside your data** | Index and sort-permutation files go to the OS state directory (`~/.local/state/tableizer` on Linux; `~/Library/Application Support/tableizer` on macOS; `%LOCALAPPDATA%\tableizer` on Windows). Nothing is ever written next to the source file. Persistent across sessions, with an in-app cache-size display + **Clear cache** control (§3.7, §4.7). |
| **File-change handling** | **Assume static; detect drift; prompt to reload** | Files are treated as static. The index is fingerprinted against `{path, size, mtime, hash}`; if the source changes on disk, the user is prompted to reload/re-index. No incremental append-indexing or live-tailing in v1. |
| **Counts & global ops** | **Progressive** | Show "≥ N rows (still counting)" and let sorted/filtered views fill in as the background scan advances; exact totals + jump-to-arbitrary-page light up when indexing completes. |

### Recommended — pending your confirmation

My proposals from the analysis, **not yet ratified**. Flag any you disagree with.

| Decision | Proposal | Rationale |
|---|---|---|
| **GUI framework** | **`egui` + `eframe`** (on `winit` + `wgpu`), with **`egui_table`** for the virtualized grid, behind a swappable `ViewportSource` trait | One native binary per OS — no webview, no bundled runtime. In-process, so the viewport shares engine memory with zero IPC (the whole point of single-language). egui_table (Rerun) is purpose-built for millions of virtualized rows. The immediate-mode concern is bounded: only the visible viewport is laid out per frame and the engine runs off-thread. See §4.8. |
| **Grid validation** | Go/no-go **spike** on egui_table (drag-reorder + pin + hide + highlight holding 60 fps under a synthetic scan, on a 4K Windows display) | The grid is the schedule's long pole and the one axis where Rust trails. Escalation ladder if it fails: gpui-component → custom `wgpu` grid → (last resort) Rust `cdylib` engine + native **Qt** grid over thin FFI. The `ViewportSource` seam makes any of these a viewport swap, not an engine rewrite. |

---

## 2. Performance contract (the tiered SLA)

Replace "without performance degradation" with three operation classes. Every operation
belongs to exactly one class, and the class is **surfaced in the UI**.

### Tier A — Instant (target < 100 ms, independent of file size)
Constant per-operation latency *after* the index exists. These never scan the whole file.
- Open + first-screen render (stream the head forward; no global index needed for page 1).
- Scroll / paginate / jump-to-row (one offset lookup + one seek + parse of that page).
- Change page size; column show / hide / reorder (projection + metadata only — no data movement).
- "Highlight matches" within the currently-rendered page.

### Tier B — Bounded background build (linear, one-time, persisted, cancellable)
Paid once per file, amortised across the session and future opens via the persisted index in the OS state dir (§4.7).
- **Row-offset index build** (a single quote-aware pass; see §4.1). Progress bar + cancel.
  The user may browse the head and stream-search *while* it builds; jump-to-arbitrary-page
  and exact total row count light up when it completes.
- Per-column sort-permutation build; optional full-text/literal search accelerator build.

### Tier C — Streaming global operation (linear, async, cancellable, incremental results)
Allowed to be slow; **must** show progress within ~100 ms, report what's processed, stream
partial results, and cancel cleanly. Never blocks the UI thread. Never silently windowed.
- Global column sort (external merge sort — see §4.3).
- Full-file text / regex search; "hide non-matching rows"; invert search.
- Export of the current view.

**Adapts to the hardware it runs on.** Budgets are *relative to the machine*, not absolute: the engine
detects available RAM (memory ceiling), core count (parallelism), and measured disk throughput (ETAs),
and tunes itself. The app runs anywhere; it promises behaviour relative to what the hardware can do.

**Acceptance criteria** (operationalises "no degradation" into something testable):
- Time-to-first-page **< 2 s** regardless of file size (head streamed before the index completes).
- Scroll / page-turn p99 **< 16 ms/frame** (sustained 60 fps) **including while a Tier B/C job runs**.
- Tier B/C scans sustain **near the measured storage bandwidth** (not a fixed MB/s), with progress + ETA
  shown within **100 ms** and clean cancellation.
- A **configurable memory ceiling** (defaulting to a fraction of detected RAM) is honoured regardless of
  file size — the engine spills to disk, never OOMs.

---

## 3. Functional requirements

### 3.1 Parsing (CSV / TSV / arbitrary separator) — "safely and correctly"
- Use a battle-tested parser core (`csv` / `csv-core`); **do not hand-roll RFC 4180**. Quoting,
  doubled-quote escaping, embedded newlines/delimiters in quotes, configurable
  delimiter/quote/escape/terminator, ragged rows, comment lines.
- **Byte fidelity is non-negotiable.** Parse via `ByteRecord` (not `StringRecord`); the canonical
  cell value is the *exact source bytes*. Type inference is a **presentational overlay only**
  (alignment, sort key, formatting) and must NEVER mutate stored/exported bytes. Preserve leading
  zeros, `+` signs, over-long/over-precise numeric IDs (treat as text), and ambiguous dates.
- **Encodings**: BOM-aware sniffing (UTF-8 / UTF-16 LE+BE), default UTF-8 with user override for
  UTF-16 / Latin-1 / Windows-1252 via `encoding_rs`. Undecodable bytes → visible U+FFFD + a per-file
  "N decode errors" indicator. Never panic, never silently drop. Keep raw bytes for re-decode without reload.
- **Dialect auto-detection** is a *visible, editable default*, never a silent authority. Sample
  multiple regions (not just the head); show the detected delimiter/quote/header; allow override + re-parse.
- **Malformed-row resilience**: ragged/short/long rows, unclosed quotes, mixed line endings, multi-GB
  single fields — *show and flag, never crash*. Enforce a max-field/max-record guard (DoS defence).
- Extensible to JSON / Parquet later behind a format-reader seam (see §4.5) — designed on paper against
  one nested + one columnar format before the trait is frozen, even though only CSV/TSV ship in v1.

### 3.2 Pagination
- Custom page size. Pages are served from the **row-offset index** (§4.1) — *never* by byte-offset
  arithmetic (variable-length quoted records make `offset = page × size` silently wrong).
- Pagination is defined over the **active view** (post-filter, post-sort). The view pipeline order is
  explicit (§4.4); a filtered/sorted page requires that filter/sort to be materialised first (Tier B/C).

### 3.3 Search ("robust")
- One **streaming-scan engine** by default — serves literal, substring, AND regex, emits matches
  incrementally with progress + cancel + early-out.
- **Regex must be linear-time / ReDoS-safe**: use the `regex` crate (NEVER `fancy-regex` or any
  backtracking engine). Linear-time is an *enforced invariant* given user-supplied patterns.
- **Invert search** = complement of the match predicate over the scan.
- **Highlight-only** (Tier A, in-place over the paginated view) vs **hide non-matching** (Tier C,
  a virtualised result list backed by a growing match-rownum list) are distinct, both surfaced.
- A literal-term inverted-index accelerator is **deferred** (one streaming-scan engine covers v1: literal +
  regex + invert). Revisit only if usage shows heavy *repeated* exact-term searches on the same large file;
  never the path for regex.

### 3.4 Sort
- **Default = page-local sort** (Tier A, instant), explicitly *labelled* as sorting the current page.
- **Global sort** (Tier C) is a distinct, named, async action that builds + persists a sort-permutation
  of `(key, rownum)` pairs via external merge sort (§4.3). Surfaces estimated time + disk up front.
  Never silently global-sort; never ship page-local sort as a silent default (it is *misleading*, not
  just slower — the on-screen top row is rarely the global minimum).

### 3.5 Columns
- Show / hide / reorder — all **view-only** (projection + metadata, no data movement). Tier A.
- Column resize, drag-to-reorder, pin/freeze, sticky header, horizontal virtualisation for wide tables.

### 3.6 Export
- Two **explicit, labelled** modes: **(A) Export current view** (applies filter + sort + reorder + hide —
  *not* a source round-trip) and **(B) Export source faithfully** (ignores view transforms).
- Tier C: routed through the same async / progress / cancel framework; pre-flight destination free space; stream output.
- Correct write-side quoting (quote any field containing delimiter / quote / CR / LF; double embedded quotes;
  record line terminator; emit/omit BOM per choice). A round-trip self-test asserts byte/field equivalence
  on the source-export path.
- v1 export is **same-family only** (CSV → CSV/TSV). Cross-format export (→ JSON/Parquet) is Phase 3 with
  *documented coercion rules* — "export in any format we import" is NOT a lossless N×N promise.

### 3.7 Cache management
- Because the engine writes potentially large index/permutation files to the OS state dir (§4.7), the app
  **exposes that cost**: a storage view showing **total cache size** and a **Clear cache** action.
- Optional niceties: a per-file breakdown (size, last-used, individual evict) and an optional configurable
  **size cap with LRU eviction** as a safety net so the cache cannot silently fill the disk. Manual clear is
  the headline control.

---

## 4. Architecture

The **data engine** (multi-TB indexing, byte-faithful safe parsing, out-of-core external sort,
streaming cancellable search) is the hard, risky, valuable ~80%. The **grid is a thin, swappable
viewport**. The language decision was driven by engine fit; the engine lives in `tableizer-core`
(UI-agnostic, independently testable) and the UI is a thin consumer of one trait.

### 4.1 Sparse, persisted, quote-aware row-offset index — the foundational artifact
- Built by a single **quote-aware** streaming pass (`csv-core` record boundaries), in the background,
  cancellable + resumable, with progress.
- **Sparse anchor design** (dissolves the size objection): one anchor per fixed byte window
  (~64 KB–1 MB) storing `{ byte_offset, quote_parity_at_block_start, cumulative_record_count }`.
  ~400 MB for 3 TB regardless of row length, vs tens-to-hundreds of GB for a dense per-row index.
  Because quote parity is *stored*, resync at any anchor is **decidable** — never the silently-wrong
  seek-then-resync heuristic.
  - *Phase 0 implements a simpler variant:* anchors at **record boundaries** every N records (no stored
    quote parity needed — a record boundary is always outside a quoted field). Correct and sparse; lookup
    re-parses up to N records. The byte-window form above is the refinement that also bounds *lookup
    latency* regardless of row length (pathological very-long rows), via `csv-core` resumable parsing.
- Serve page N: binary-search the anchor with `cumulative_count ≤ N×page_size`, seek, parse forward
  with known quote state. Per-page work = a few blocks, not a full scan.
- Persist in the **OS state dir** (§4.7), keyed by `{ path, size, mtime, content hash, dialect }`; validate on
  open and, on mismatch, **prompt the user to reload/re-index** (static-file assumption — no incremental
  append-indexing or live-tailing in v1). The pass does double duty: row count, header, per-column type
  hints, optional per-block min/max.

### 4.2 I/O
- Prefer bounded `pread`-based streaming with a buffer pool for adversarial files. If mmap is used,
  do all faulting on worker threads, advise sequential vs random, enforce a memory budget, and guard
  against **SIGBUS** (truncation / network drop = recoverable error, not a crash). Fall back to
  positioned reads on Windows / network filesystems.

### 4.3 Bespoke external merge sort (do NOT delegate)
- Run-generation in parallel (rayon) → k-way merge of disk-spilled runs → persisted permutation file (in the
  OS state dir, §4.7). Spill runs use the same state-dir area, never the source file's directory.
- Sort `(key, rownum)` pairs, never full rows, to minimise the working set.
- **Do not** delegate the multi-TB sort to DataFusion/Polars: documented spill pathologies (400 GB+ temp
  blow-ups, ~1 TB freezes) make the hardest requirement too risky to outsource. (Their *parsing* and
  Arrow interchange may still be reused; the *sort* is ours.)

### 4.4 The view pipeline (resolves composition-order ambiguity)
`source bytes → encoding decode → format parse (rows/cells) → filter (search-as-hide) → sort →
paginate/window → render | export`. Each stage is tagged **Tier A/B/C** and the tier is surfaced to the user.
"Export the current view" = export the output of this pipeline.

### 4.5 Format-reader seam (page/query level, not row level)
- The seam is roughly `schema()` + `read_page(projection, row_range, filters)` — a logical slice of a
  logical table. CSV satisfies it via the offset index; Parquet (later) via row-group metadata + pushdown;
  JSON (later) via path projection. A row-yielding `Iterator<Row>` trait is rejected — it leaks streaming
  assumptions and strangles columnar formats.

### 4.6 Concurrency
- Engine on a rayon worker pool; bounded back-pressured channels (crossbeam/flume) for UI↔engine; one
  `CancellationToken` per job (checked on a row/block cadence); engine coalesces/drops stale viewport
  requests so fast scrolling never queues already-scrolled-past work. UI thread only ever receives small,
  already-materialised viewport slabs. Global memory budget enforced via bounded buffers, not per-subsystem guesses.

### 4.7 Derived-artifact lifecycle
- **All derived artifacts live in the OS *state* dir — never beside the source file.** Resolve the location
  with the `directories` crate: `state_dir` on Linux (`~/.local/state/tableizer`), falling back to
  `data_local_dir` on macOS (`~/Library/Application Support/tableizer`) and Windows
  (`%LOCALAPPDATA%\tableizer`), since those platforms have no XDG state dir. The source file's directory is
  **never** written to — works cleanly on read-only / network / removable media.
- Each artifact (offset index, sort permutation, future search index) carries a **versioned, magic-numbered**
  manifest recording source `{size, mtime, content hash}`, dialect, schema, and a format version. On open,
  validate; on mismatch, treat as stale and prompt to re-index (a stale index against a changed file =
  silently-wrong rows, the worst bug class).
- **User-visible + user-controlled** (§3.7): the app shows total cache size and offers **Clear cache**. A
  safety-net **size cap with LRU eviction** is optional/configurable; low-disk pre-flight before a build.
- **App config is separate** from the cache: lightweight preferences, recent-files, window state, and
  optional saved "views" live in the OS **config** dir (`config_dir`), not mixed with the index cache.

### 4.8 Cross-platform desktop delivery — how it ships on macOS / Windows / Linux
- **One Rust binary per OS, no webview, no bundled runtime.** "Cross-platform" is delivered by three
  layers beneath the widgets, all in-process:
  - **`winit`** — cross-platform window + input event loop (Cocoa on macOS, Win32 on Windows, Wayland/X11 on Linux).
  - **`wgpu`** — cross-platform GPU abstraction (Metal on macOS, D3D12/Vulkan on Windows, Vulkan on Linux); the grid is GPU-rendered.
  - **`egui` / `eframe`** — the immediate-mode widget layer, with **`egui_table`** for the virtualized grid.
  The same code compiles to a `.app` (macOS), `.exe` (Windows), and ELF binary (Linux).
- **Why this shape:** in-process (the viewport is a reference into engine-owned memory — zero IPC, the
  entire point of single-language); a single small binary; mature and genuinely cross-platform today. It
  is the lowest-risk path to a working app on all three OSes.
- **Packaging / signing / updates:** `cargo-packager` (or `cargo-dist`) produces a notarized **universal**
  `.app` (arm64 + x86_64) on macOS, an Authenticode-signed `.msi`/`.exe` on Windows, and an AppImage
  (+ optional `.deb`/Flatpak) on Linux, plus a signed auto-updater — all from CI.
- **The grid remains spike-gated** (the one place this stack trails): see §7 Phase 0. Escalation ladder if
  egui_table fails the under-load reorder/pin spike: gpui-component → a custom `wgpu` grid → (last resort)
  a native **Qt** grid over a thin FFI to the same Rust engine. The `ViewportSource` seam (§4.5) makes any
  of these a viewport swap, not an engine rewrite.

---

## 5. Cross-cutting / non-functional

- **Untrusted input is an attack surface.** Arbitrary multi-GB/TB files from unknown sources + user-supplied
  regex. `cargo-fuzz` on the byte parser + encoding + index + mmap layers from early, run in CI. Decompression/
  resource-exhaustion defences (field/record caps). ReDoS-safe regex (enforced, §3.3). SIGBUS safety (§4.2).
  A short threat-model note: what "malicious file" means for a read-only viewer; no code paths execute file
  content; validate export target paths (no traversal).
- **Test strategy** (correctness-critical tool): a checked-in **golden corpus** of pathological CSVs (embedded
  newlines in quotes, CRLF/LF/lone-CR, BOM/no-BOM, ragged rows, trailing-delimiter, quote-in-unquoted-field,
  empty/1-byte files, no-trailing-newline, mixed encodings); **differential testing** vs a trusted oracle
  (Python `csv` / qsv); **property tests** (`proptest`) for round-trip `serialize(parse(bytes))` fidelity;
  `cargo-mutants` (already anticipated in `.gitignore`); a synthetic large-file generator so CI needs no multi-TB fixtures.
- **Performance harness**: `criterion` micro-benchmarks for the hot parse/scan loop + an end-to-end harness
  measuring the §2 budgets across a storage-tier matrix; CI perf-regression + memory-ceiling gates.
- **Observability / recovery**: `tracing` structured logs; partial-failure semantics (corrupt row mid-scan →
  skip-and-flag, surfaced in a "data quality" indicator, never abort silently); resumable index build; panic/crash reporting.
- **App lifecycle**: lightweight config in the OS *config* dir (separate from the index cache in the *state*
  dir, §4.7) — prefs, recent files, window state, default delimiter/encoding/header overrides, and **optional**
  saved "views" (column order/sort/filters/page size as a named session). Index/sort artifacts are *not* config.
- **Distribution / CI from day one**: Cargo workspace, MSRV policy, `rustfmt`/`clippy`, `cargo-deny` license+advisory
  gate, three-OS build matrix (Apple-silicon + Intel, Windows signing cert, Linux AppImage/Flatpak), signing +
  notarization + auto-update (`cargo-dist`/`cargo-bundle`).
- **i18n correctness (not just UI)**: locale-aware number/date *display* and Unicode collation for sort
  (changes sort results + tie-breaking); CJK / RTL / combining-character rendering in a fixed grid.

---

## 6. v1 scope

**In:** CSV/TSV/arbitrary-separator parsing (byte-faithful, encoding-aware, malformed-resilient; UTF-8 +
UTF-16 LE/BE + Latin-1/Windows-1252 + BOM); sparse offset index persisted to the OS state dir with
progressive availability + change-detection-and-reload; **cache-management UI (size + clear)**; instant
pagination + custom page size; column show/hide/reorder; page-local sort + global async sort; streaming
text+regex search with invert + highlight/hide; same-family export (current-view + source) with round-trip
self-test; read-only; multi-TB hardened (mmap/stream + spill); hardware-adaptive budgets; lightweight config
+ recent-files (saved-views optional).

**Out (deferred):** any editing / undo / save-back; JSON + Parquet readers; cross-format export; CJK legacy
encodings (Shift-JIS/GBK/EUC-KR); a literal-term search-index accelerator; live-tailing growing files;
incremental/append re-indexing; multiple tabs/files; full accessibility (screen-reader grid semantics);
localization; telemetry.

---

## 7. Phased roadmap (ordered by descending technical risk)

**This section is the live work tracker.** Completed items are marked ✅; pending items ☐. Update them in
the same change that completes the work.

### Phase 0 — tracer bullet + grid go/no-go spike
- ✅ Cargo workspace (`tableizer` app + `tableizer-core` engine), CI, lints, dual license, deps at latest
- ✅ `tableizer-core` `ViewportSource` seam + module stubs (`index`/`parse`/`search`/`sort`/`cancel`/`error`)
- ✅ Cross-platform desktop shell (eframe window) builds and runs
- ✅ **Quote-aware row-offset index** — `build`/`row_count`/`offset_of_row`, **sparse anchors** (every 1024 records),
  **mmap source** (`CsvTable`), **background build with progress + cancel** (`build_with`); quote-aware +
  byte-faithful. (Byte-window + quote-parity *lookup-latency* refinement is noted in §4.1, deferred.)
- ✅ **Virtualised scroll + progressive load** — `CsvTable::open` returns **instantly** (mmap + head parse); the index
  builds on a background thread with an honest growing count (`AtLeast` → `Exact`); `fetch` streams from the head until
  the index lands, then O(1) random access. The app renders via egui `ScrollArea::show_rows`, fetching only the
  visible rows. **Time-to-first-page is sub-ms regardless of file size** (measured by `examples/bench_load.rs`;
  the index-build O(n) scan runs off the UI thread). Synthetic-file generator: `examples/gen_csv.rs`. 11 engine tests.
- ☐ **Grid spike — needs a human run:** wire `egui_table` and measure 60 fps under a synthetic scan on a 4K display
  (ideally Windows) → go/no-go vs Plan B. The virtualized data path + large-file generator are ready; the fps
  measurement requires eyes on a screen and cannot be done headless.

### Phase 1 — MVP
- ☐ Real CSV/TSV/custom-delimiter parsing behind the format seam; header detection
- ☐ Encoding handling (UTF-8/16, Latin-1/Windows-1252, BOM); malformed-row resilience
- ☐ Pagination + custom page size; column hide/show/reorder
- ☐ Substring-search highlight; clipboard copy; prefs/recent-files; cache-management UI (size + clear)

### Phase 2 — v1
- ☐ Global async sort + global filter (hide non-matching) with progress/cancel
- ☐ Regex + invert search; same-family export (both modes) + round-trip self-test
- ☐ Saved views; keyboard nav; wide-table horizontal virtualisation + frozen columns
- ☐ Type detection/formatting; null/empty handling

### Phase 3 — later
- ☐ JSON + Parquet readers (proving the seam); cross-format export with coercion rules
- ☐ Deeper multi-TB streaming/spill hardening; live-tailing; multiple tabs
- ☐ Accessibility; localization; telemetry

---

## 8. Resolved decisions

All of the original open questions are now answered (see also §1):

- **Hardware target** → *adapt at runtime*; no fixed spec. Budgets are hardware-relative (§2).
- **Source media / artifact location** → derived files go to the OS **state dir** only, never beside the
  source; works on read-only / network / removable media (§4.7).
- **File-change pattern** → assume **static**; fingerprint and **prompt to reload** on drift; no
  incremental/append re-indexing in v1 (§4.1).
- **Exact vs progressive** → **progressive** counts and global ops (§2, §1).
- **Search mix** → defer the literal-index accelerator; one streaming-scan engine for v1 (§3.3).
- **Encoding scope (v1)** → UTF-8 + UTF-16 (LE/BE) + Latin-1/Windows-1252 + BOM; CJK legacy deferred (§3.1).
- **Saved views / sessions** → lightweight config + recent-files in the OS config dir; saved "views" optional (§4.7, §5).

No open questions remain blocking Phase 0.

---

## Appendix — Original goals (preserved verbatim)

> Tableizer is a desktop app for loading and parsing data files that are not easily human-readable into tables.
>
> - Open and parse huge (multi-TB) files into tables, without performance degradation
> - Parse CSV, TSV, or arbitrary separators safely and correctly
>   - We may add JSON, parquet, or other formats later, so build with extensibility in mind
> - Cross-platform Desktop app (macOS, Windows, Linux)
> - Pagination, with customizable page size
> - Robust search
>   - Support text and regex searches
>   - Invert search (show data without the search text)
>   - Highlight text only, or hide rows that do not match the search
> - Column sort
> - Column ordering
> - Column hide/show
> - Export data in any format that we support for import
