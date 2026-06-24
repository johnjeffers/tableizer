# Formats

How each input format satisfies the `ViewportSource` seam (see [`architecture.md`](architecture.md)).
Every reader is a `Box<dyn ViewportSource>`; the app is format-agnostic once a file is open.

## Detection

Format is detected from **content first**, because extensions lie (a `.json` holding NDJSON, a
mis-named Parquet). `detect_format` (`crates/tableizer/src/model.rs`) reads a head sample and decides:

1. `PAR1` magic prefix → **Parquet**.
2. JSON shape sniff (`tableizer_core::json::sniff`) → **JSON** — first non-whitespace `{` whose first
   *line* parses as a complete value (NDJSON), or `[` that begins a real JSON value (an array). The
   same sniff the reader uses to pick its mode, so routing and reading never disagree.
3. Extension fallback for content that didn't classify (e.g. an empty file): `.parquet`/`.pqt`,
   `.ndjson`/`.jsonl`.
4. Otherwise **delimited** text.

The Open dialog is unfiltered (rfd flattens filters and ignores an all-files wildcard on macOS, so a
filter would silently grey out valid files); detection runs on whatever the user picks.

## Cell rendering and byte fidelity

For delimited text the canonical cell is the **exact source bytes** — the byte-fidelity invariant
holds end to end. Typed/binary formats have no source byte for a decoded value, so JSON and Parquet
render each value to faithful UTF-8 **text** at the reader boundary (the `Cell` then carries those
rendered bytes unchanged through search/sort/export). This is a deliberate v1 choice: it keeps the
whole downstream engine (byte cells, regex search, numeric-or-lexical sort) unchanged. A typed-value
path is a future option if typed sorting/formatting is needed.

## Delimited — CSV / TSV / arbitrary separator (`CsvTable`)

The reference reader. `Dialect::sniff` auto-detects the delimiter (comma/tab/semicolon/pipe, with
custom + header overrides) as a visible, editable default. Parsing is byte-faithful via the `csv`
crate's `ByteRecord` (never hand-rolled RFC 4180): quoting, doubled-quote escaping, embedded
newlines/delimiters, ragged rows tolerated and counted (a "⚠ N ragged rows" badge). Encodings:
BOM-aware UTF-8 / UTF-16 LE+BE (transcoded to UTF-8 at open), with user override to Latin-1 /
Windows-1252 for display. Random access is served by the persisted sparse offset index
(`crates/tableizer-core/src/index.rs`). This is the only format with the Parsing tab (delimiter /
header / encoding) and the only one whose index is cached to the state dir.

## JSON — NDJSON and top-level arrays (`JsonTable`)

One reader, two record-boundary strategies (`crates/tableizer-core/src/json.rs`):

- **Lines** (NDJSON / JSON Lines): a raw `\n` is always a boundary — newlines inside strings are
  escaped.
- **Array** (a single top-level `[ … ]`): records are the depth-1 elements, located by a
  quote/escape/brace-depth-aware scan. The whole file is **never** parsed at once — the same sparse
  record-offset index, streaming search, and view build as NDJSON, so arrays read **out-of-core** and
  scroll instantly at any size.

Columns are the union of top-level object keys in first-seen order (`serde_json` with `preserve_order`
so columns follow the document, not alphabetical sort). Values render as: strings unquoted, numbers /
booleans as their JSON text, `null` as empty, nested arrays/objects as compact JSON. When records
aren't objects (e.g. an array of scalars) a single synthetic `value` column holds the whole record.

**Limitations.** A single non-array JSON *object* (a config blob) isn't tabular and isn't read as a
table. Sort/filter build the view in memory (like CSV today). The record index isn't persisted to the
state dir yet, so a large JSON file re-indexes on each open. A record larger than the detection head
sample may defeat the content sniff.

## Parquet (`ParquetTable`)

Columnar, so there is no offset index to build: the footer metadata gives the exact row count, schema,
and row-group layout, so `open` is metadata-only and the row count is exact from the first frame
(`crates/tableizer-core/src/parquet.rs`). A viewport fetch reads **only the visible rows** via a
Parquet `RowSelection` (with the page index loaded so it skips at page granularity, not whole row
groups), decodes them through `arrow`, and renders via `arrow`'s `ArrayFormatter` (nulls → empty). A
one-entry fetch cache keeps a static frame from re-decoding. `InferredType` comes from the Arrow
schema (for right-alignment / sort hints).

Filter and sort do a full decode scan — a filter reads every column (it matches any field), a pure
sort reads only the sort column — the same Tier-C cost as the CSV in-memory view build, and likewise
in memory for now. Per the bespoke-sort invariant, `arrow`/`parquet` are reused for **decode** only;
the sort stays ours.

## Export

Export runs through the seam, so it works from any source: **current view** (filter + sort + visible
columns, in display order) or **source** (every row/column, view-bypassing), in any of four formats —
**CSV**, **TSV**, **NDJSON**, or **Parquet**. By the time export runs, cells are already rendered text,
so values are written as their displayed text with no type coercion, preserving the exact bytes: CSV/TSV
go through the `csv` writer with correct re-quoting (a round-trip self-test asserts byte/field
equivalence on the source path); NDJSON writes one object per row with every value as a JSON string (a
numeric-looking id keeps its leading zeros); Parquet writes one UTF-8 string column per column (a
round-trip self-test re-reads it through the Parquet reader). **Typed** NDJSON/Parquet export (numbers,
booleans, nulls — with documented coercion rules) and a user-chosen delimiter are tracked in
[`todo.md`](todo.md).
