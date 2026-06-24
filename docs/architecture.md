# Architecture

The design reference for Tableizer — the *what and why* behind the engine. The binding rules agents
must not break (byte fidelity, ReDoS-safe regex, the bespoke sort, read-only, state-dir artifacts,
tier discipline, hardware-adaptive budgets) live as **Invariants** in [`AGENTS.md`](../AGENTS.md);
this document explains the structure those rules protect. Per-format detail is in
[`formats.md`](formats.md).

## The shape of the problem

Tableizer views data files — including **very large (multi-TB)** ones — as browsable tables, and it
is **read-only** (export is the only write path, always to a new file). A multi-TB file far exceeds
desktop RAM, so the engine is **out-of-core**: it streams/`mmap`s the source, builds a *sparse*
persisted index, and never holds the whole file in memory.

"Open multi-TB files without performance degradation" is physically unsatisfiable as written — any
operation that must touch every byte is bounded by storage bandwidth (a full read is minutes on NVMe,
hours on HDD). The honest replacement is a **per-operation performance contract**: every operation
belongs to one of three tiers, and the tier is surfaced in the UI.

## Performance contract (the tiered SLA)

- **Tier A — Instant (< ~100 ms, independent of file size).** Constant-latency once the index
  exists; never scans the whole file. Open + first-screen render (stream the head forward),
  scroll / jump-to-row (one offset lookup + seek + parse of that window), column show/hide/reorder
  (projection + metadata only), highlight-matches within the rendered page.
- **Tier B — Bounded background build (linear, one-time, persisted, cancellable).** Paid once per
  file and amortised across opens via the persisted index. The row-offset index build (one pass,
  progress + cancel); the user may browse the head and stream-search *while* it builds. Exact total
  row count and arbitrary-jump light up when it completes.
- **Tier C — Streaming global operation (linear, async, cancellable, incremental).** Allowed to be
  slow but must show progress within ~100 ms, report what's processed, stream partial results, and
  cancel cleanly — never blocking the UI thread, never silently windowed. Global sort, full-file
  text/regex search and hide-non-matching, export of the current view.

**Acceptance criteria** (what "no degradation" operationalises to): time-to-first-page < ~2 s
regardless of size; scroll/page-turn sustains 60 fps *including while a Tier B/C job runs*; Tier B/C
scans run near measured storage bandwidth with progress/ETA shown within ~100 ms and clean
cancellation; a configurable memory ceiling is honoured regardless of file size (spill, never OOM).

Budgets are **hardware-relative**: the engine adapts to detected RAM (memory ceiling), core count
(parallelism), and *measured* throughput (ETAs) — there are no fixed MB/s or memory assumptions.

## The `ViewportSource` seam

The load-bearing boundary between engine and GUI is the `ViewportSource` trait
(`crates/tableizer-core/src/viewport.rs`). The UI only ever asks for a small, **already-materialised**
slice of a logical table — `schema()`, `row_count()`, and `fetch(row_range, columns)` — plus an
async `set_view(filter/sort)`. The engine decides how to satisfy a fetch (offset-index seek, sort
permutation, filtered result list, streaming fallback). Keeping the UI confined to this trait is what
makes the grid a thin, swappable layer; it is also the **format-reader seam** (below). A row-yielding
`Iterator<Row>` was deliberately rejected — it leaks streaming assumptions and strangles columnar
formats.

## The sparse offset index — the foundational artifact

Random access to row *N* of a delimited file cannot use `offset = N × width`: quoted embedded
newlines make that silently wrong. Instead a background pass records one **anchor** byte-offset every
*N* records (currently 1024); a lookup seeks the nearest preceding anchor and re-parses forward at
most *N* records. The index is therefore `O(rows / N)` — small even for terabytes — and is built
quote-aware so an anchor always lands outside a quoted field. It carries the row count and a
data-quality signal (ragged-row count), and for delimited files is persisted (see *Derived-artifact
lifecycle*) keyed by `{path, size, mtime, dialect}` with strict stale-detection.

The same idea generalises to other line/record formats: JSON uses a record-offset index whose
boundary finder is newlines (NDJSON) or depth-1 array elements; Parquet needs no offset index at all
(its footer already maps row groups). See [`formats.md`](formats.md).

The byte-window + stored-quote-parity variant (anchors per fixed byte window, parity stored so resync
is *decidable*) additionally bounds *lookup latency* against pathological very-long rows; it is a
documented refinement on top of the record-interval form.

## The view pipeline

`source bytes → encoding decode → format parse (rows/cells) → filter (search-as-hide) → sort →
window (visible viewport) → render | export`. Filter and sort are materialised together as one async
**"view"** (an ordered list of data-rows) before a filtered/sorted window can be served. "Export the
current view" is the output of this pipeline; "export source" bypasses the view.

## Sort — bespoke, not delegated

Global sort builds a permutation of `(key, rownum)` pairs (never full rows, to keep the working set
small). It is **ours on purpose**: multi-TB sort is *not* delegated to DataFusion/Polars, whose
documented spill pathologies (400 GB+ temp blow-ups, ~1 TB freezes) make the hardest requirement too
risky to outsource — though their parsing / Arrow interchange is reused (Parquet decode). Keys compare
numerically when both parse as numbers, else byte-lexicographically.

*Current state:* the view (filter + sort) is built in memory. The spill-to-disk **external merge
sort** (parallel run generation → k-way merge of disk-spilled runs → persisted permutation, all in the
state dir) is the documented refinement for key+rownum sets that exceed RAM.

## I/O

Delimited and JSON sources are memory-mapped; large UTF-16 is transcoded to UTF-8 at open (small
files in practice — huge UTF-16 is not streamed). The known caveat: an `mmap`ed file truncated by
another process can `SIGBUS`; a positioned-`pread` fallback + SIGBUS guard is the planned hardening
for adversarial / network / removable media.

**Gzip** (`crates/tableizer-core/src/gzip.rs`) is *streaming* — no random access — so it is handled
the same way as a remote object: a gzipped file (detected by magic bytes, not extension) is
**decompressed once to a cache file** in the OS *state* dir under `tableizer/decompressed` (streamed,
progress + cancel, keyed by source `{path, size, mtime}`), and the seekable decompressed file is what
the engine opens. Byte fidelity holds on the decompressed content (decode is lossless/deterministic;
the wrapper is transport). It composes with remote: an `s3://…/data.csv.gz` is downloaded, then
decompressed, then opened. The three caches (index, downloaded objects, decompressed files) share the
state dir and are surfaced together with one **Clear cache** control (Settings ▸ Cache).

**Remote / cloud sources** (`crates/tableizer-core/src/remote.rs`) are reached through the
`object_store` crate (multi-cloud: S3 / GCS / Azure / HTTP behind one ranged-read API). It is *pure
I/O* — a byte source, not a query or sort engine — so reusing it leaves the "the external sort is
ours" invariant untouched. The current strategy is **download-to-cache**: a remote object is fetched
once (streamed, with progress + cancel) into the OS *state* dir under `tableizer/remote-cache`, keyed
by the object's ETag (else size + last-modified), then opened exactly like a local file — so the whole
engine (index, view, sort, search, export, index persistence) works unchanged. The object store is
async; that is confined to `remote.rs` behind a per-call current-thread runtime so the engine stays
synchronous. **Credentials:** `object_store::parse_url` builds a *blank* backend (it does not read the
environment), so auth is passed explicitly as options merged over the process environment. For S3 the
options come from one of two sources, chosen in Settings ▸ Cloud storage: (1) the **AWS provider chain**
(default) — `remote::aws_credentials` runs `aws-config` to resolve environment, `~/.aws` profiles,
**SSO** (the `aws sso login` token cache → temporary credentials), assume-role, and EC2/ECS roles;
`object_store` has no SSO of its own, so we resolve and pass static temp credentials in. Or (2) a
**static-keys** form for pasted credentials / S3-compatible stores (MinIO, R2: endpoint + allow-HTTP).
S3 buckets are region-specific, but one credential set (e.g. an SSO role) can span regions, so before
each bucket operation the engine **resolves the bucket's region** (S3 `HeadBucket`, cached per bucket)
and passes it to `object_store` — the in-app equivalent of the CLI's `--region`, so a bucket in a
non-default region just works.
A **cloud file browser** lets the user pick a file instead of typing a URL, as a lazy **tree**: it
opens to buckets discovered from the credentials (`remote::list_s3_buckets`, via `aws-sdk-s3`'s
`ListBuckets` — needed because `object_store` is bucket-scoped and can't enumerate buckets), and each
folder is listed on expand with `remote::list_dir` (`object_store`'s `list_with_delimiter`). Expanded
subtrees are **cached** in the in-memory `BrowseNode` tree and kept across re-opens, so revisiting a
branch never re-lists. All on background threads with the same credential resolution as opening.

The documented next step is a **streaming `ReadAt` source** — first screen from a head fetch,
random access by ranged GET, no full download — which generalises the same `pread`/SIGBUS seam above
and reuses this `object_store` backend and cache directory; download-to-cache is a strict subset of
it (eager instead of lazy).

## Concurrency

Index builds and view builds run on background threads; each long job carries a `CancellationToken`
checked on a coarse row/block cadence, so a new view cancels the in-flight one and a dropped table
stops its builders promptly. The UI thread only ever receives small, already-materialised viewport
slices.

## Derived-artifact lifecycle

**Nothing is ever written beside the source file** — it must work on read-only / network / removable
media. Derived artifacts (the offset index, and future sort permutations / search accelerators) go to
the OS **state** dir (`~/.local/state/tableizer` on Linux, the `data_local_dir` equivalent on
macOS/Windows), resolved via the `directories` crate. Each carries a versioned, magic-numbered header
recording source `{size, mtime}` + dialect; on open it is validated and, on any mismatch, discarded
and rebuilt (a stale index against a changed file = silently-wrong rows, the worst bug class). The app
surfaces total cache size and a **Clear cache** control. App **config** (preferences, recent files,
window state, saved per-file views) is separate, in the OS **config** dir.

## Desktop delivery

One Rust binary per OS, no webview, no bundled runtime: **`winit`** (window + input), **`wgpu`** (GPU
abstraction — Metal/D3D12/Vulkan), **`egui`/`eframe`** with **`egui_table`** for the virtualised grid.
In-process means the viewport is a reference into engine-owned memory with zero IPC — the point of a
single-language stack. The grid stays swappable behind `ViewportSource`: if `egui_table` ever fails a
60-fps-under-load bar, the escalation ladder (custom `wgpu` grid → native grid over thin FFI to the
same engine) is a viewport swap, not an engine rewrite.

## Security & testing posture

Untrusted multi-GB/TB files and user-supplied regex are the attack surface. Defences: field/record
size caps (resource-exhaustion), ReDoS-safe regex (enforced — `regex` crate only), SIGBUS safety, and
no path traversal on export targets; no file content is ever executed. Correctness is held by a
checked-in golden corpus of pathological CSVs, differential testing against a trusted oracle, `proptest`
round-trip fidelity, `cargo-mutants`, and `cargo-fuzz` on the byte/encoding/index/mmap layers, with a
synthetic large-file generator so CI needs no multi-TB fixtures. See [`AGENTS.md`](../AGENTS.md) §
*Testing layers* for the working rules.
