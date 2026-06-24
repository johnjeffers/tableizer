- Package as a standard app
- Create release artifacts for each OS
- App should be allowed to be set as the default handler for CSV, TSV, etc.
  - macOS: DONE — verified warm + cold (Open With / double-click / set-as-default) on macOS 26.
    Declaration via `CFBundleDocumentTypes` + imported UTIs in `scripts/package-macos.sh`; the
    open-documents receiver is `src/macos_open.rs` (cold launch leans on a run-loop-entry re-assert —
    re-verify if winit/eframe/macOS are upgraded).
  - Windows: per-extension association via a ProgID + `shell\open\command` (installer-written); files
    arrive as argv, so no receiver code needed.
  - Linux: a `.desktop` file with `MimeType=` + `update-desktop-database`, plus a `shared-mime-info`
    XML for Parquet/NDJSON (no standard MIME types); files arrive as argv.
- Export
  - Delimited (default to CSV, but allow user to pick)
  - Parquet
  - NDJSON
  - Cross-format export with documented coercion rules

## Deferred (from the former spec)

Engine / scale:
- External spill-to-disk merge sort — the view (filter + sort) is in-memory for now
- Persist the JSON/Parquet record index to the state dir (JSON re-indexes on each open today)
- Byte-window + stored-quote-parity index variant (bounds lookup latency on pathological long rows)
- SIGBUS guard / positioned-`pread` fallback for mmap on truncation / network / removable media
- Live-tailing growing files; incremental / append re-indexing

UX:
- Multiple tabs / files
- Distinguish "NDJSON" vs "JSON (array)" in the status-bar format label (`JsonTable::mode()` exists)
- Validate the grid holds 60 fps under load (4K, ideally Windows) — the original grid go/no-go bar
- Accessibility (screen-reader grid semantics); localization (i18n display + Unicode collation sort); telemetry

Testing infra:
- Golden corpus of pathological CSVs; differential tests vs a trusted oracle; `proptest` round-trip;
  `cargo-fuzz` on the byte/encoding/index/mmap layers; `cargo-mutants`
