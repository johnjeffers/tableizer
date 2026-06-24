- Per column filters/functions (min, max, unique)
- CI/CD
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
- Export refinements
  - Typed NDJSON/Parquet export (numbers/booleans/nulls) with documented coercion rules — current
    export writes every value as text to preserve byte fidelity (e.g. a `"007"` id keeps its zeros)
  - User-chosen delimiter for CSV/TSV export
- Multiple tabs / files
- Distinguish "NDJSON" vs "JSON (array)" in the status-bar format label (`JsonTable::mode()` exists)
- Validate the grid holds 60 fps under load (4K, ideally Windows) — the original grid go/no-go bar
